//! AeroKernel: the AeroForge OS kernel core.

#![no_std]
#![no_main]

extern crate alloc;

mod acpi;
mod arch;
mod console;
mod dhi;
mod fb;
mod gdt;
mod interrupts;
mod limine;
mod memory;
mod pic;
mod serial;
mod shell;
mod smp;

use alloc::{boxed::Box, collections::BTreeMap, string::String, vec::Vec};
use core::panic::PanicInfo;
use core::sync::atomic::Ordering;

use limine::*;

// ---- Limine requests: the bootloader finds these by their magic numbers. ----

#[used]
#[link_section = ".limine_requests_start"]
static REQUESTS_START_MARKER: [u64; 4] = limine::REQUESTS_START;

#[used]
#[link_section = ".limine_requests"]
static BASE_REVISION: BaseRevision = BaseRevision::new(3);

#[used]
#[link_section = ".limine_requests"]
static BOOTLOADER_INFO_REQ: Request<BootloaderInfoResponse> = Request::new(limine::BOOTLOADER_INFO);

#[used]
#[link_section = ".limine_requests"]
static HHDM_REQ: Request<HhdmResponse> = Request::new(limine::HHDM);

#[used]
#[link_section = ".limine_requests"]
static FRAMEBUFFER_REQ: Request<FramebufferResponse> = Request::new(limine::FRAMEBUFFER);

#[used]
#[link_section = ".limine_requests"]
static MEMMAP_REQ: Request<MemmapResponse> = Request::new(limine::MEMMAP);

#[used]
#[link_section = ".limine_requests"]
static MP_REQ: Request<MpResponse, u64> = Request::with_extra(limine::MP, 0);

#[used]
#[link_section = ".limine_requests"]
static RSDP_REQ: Request<RsdpResponse> = Request::new(limine::RSDP);

#[used]
#[link_section = ".limine_requests"]
static KERNEL_ADDR_REQ: Request<ExecutableAddressResponse> = Request::new(limine::EXECUTABLE_ADDRESS);

#[used]
#[link_section = ".limine_requests_end"]
static REQUESTS_END_MARKER: [u64; 2] = limine::REQUESTS_END;

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[no_mangle]
extern "C" fn kmain() -> ! {
    serial::init();
    if !BASE_REVISION.is_supported() {
        serial::write_str("AeroKernel: bootloader does not support Limine base revision 3\n");
        arch::halt_forever();
    }

    // Screen first, so everything after it is visible.
    if let Some(fbr) = FRAMEBUFFER_REQ.response().filter(|r| r.count > 0) {
        let f = unsafe { &**fbr.framebuffers };
        if f.bpp == 32 {
            let fb = unsafe { fb::Fb::new(f.address, f.width as usize, f.height as usize, f.pitch as usize) };
            console::CONSOLE.lock().attach_framebuffer(fb);
        }
    }

    console::print_colored(console::CYAN, format_args!("AeroForge OS {}", VERSION));
    kprintln!("  |  AeroKernel (Rust core, C++ drivers)");
    if let Some(info) = BOOTLOADER_INFO_REQ.response() {
        console::print_colored(console::DIM, format_args!(
            "Booted by {} {} over UEFI\n\n", cstr(info.name), cstr(info.version)));
    }
    let screen = console::CONSOLE.lock().screen_size();
    if let Some((w, h)) = screen {
        kok!("Framebuffer console {}x{}", w, h);
    }
    kok!("Serial console on COM1 (115200 8N1)");

    gdt::init();
    kok!("GDT loaded, TSS with double-fault IST stack");

    interrupts::init();
    kok!("IDT loaded: 32 exception vectors + 16 IRQ vectors");
    unsafe { core::arch::asm!("int3") };
    if interrupts::BREAKPOINTS.load(Ordering::Relaxed) == 1 {
        kok!("Exception path verified (breakpoint handled, execution resumed)");
    }

    // ---- Memory ----
    let hhdm = HHDM_REQ.response().expect("no HHDM response").offset;
    let memmap = MEMMAP_REQ.response().expect("no memory map");
    let report = memory::init_frames(hhdm, memmap);
    kok!("Physical memory: {} MiB usable in {} regions ({} map entries)",
        report.usable_bytes / (1024 * 1024), report.regions, memmap.count);
    if let Some(k) = KERNEL_ADDR_REQ.response() {
        console::print_colored(console::DIM, format_args!(
            "       kernel image at phys {:#x} -> virt {:#x}\n", k.physical_base, k.virtual_base));
    }

    match memory::init_heap() {
        Ok(()) => kok!("Paging: mapped {} KiB kernel heap at {:#x}", memory::HEAP_SIZE / 1024, memory::HEAP_START),
        Err(e) => panic!("heap setup failed: {}", e),
    }
    heap_self_test();

    // ---- Firmware ----
    match RSDP_REQ.response().map(|r| acpi::init(r.address)) {
        Some(Ok(())) => {
            let guard = acpi::INFO.lock();
            let info = guard.as_ref().unwrap();
            kok!("ACPI {}: {} tables, MADT lists {} CPU(s) and {} I/O APIC(s)",
                core::str::from_utf8(&info.oem).unwrap_or("?").trim(),
                info.tables.len(), info.lapic_count, info.ioapic_count);
        }
        Some(Err(e)) => kprintln!("[WARN] ACPI: {}", e),
        None => kprintln!("[WARN] ACPI: no RSDP from bootloader"),
    }

    // ---- Interrupt sources and the first C++ driver ----
    pic::init(&[0, 1]);
    pic::init_timer();
    kok!("PIC remapped to vectors 32-47, PIT timer at {} Hz", pic::TIMER_HZ);

    let rc = unsafe { dhi::aero_ps2kbd_init(&dhi::OPS) };
    if rc == 0 {
        kok!("C++ PS/2 keyboard driver attached through DHI v{}", dhi::ABI_VERSION);
    } else {
        kprintln!("[WARN] PS/2 keyboard driver init failed ({})", rc);
    }

    arch::enable_interrupts();
    let start = interrupts::TICKS.load(Ordering::Relaxed);
    while interrupts::TICKS.load(Ordering::Relaxed) < start + 10 {
        arch::hlt();
    }
    kok!("Interrupts enabled, timer ticking");

    // ---- SMP ----
    if let Some(mp) = MP_REQ.response() {
        let online = smp::start_aps(mp);
        kok!("SMP: {} of {} CPU(s) online", online, mp.cpu_count);
    }

    kprintln!();
    console::print_colored(console::GREEN, format_args!("AeroKernel is up."));
    kprintln!(" Type 'help' for commands.");

    let mut shell = shell::Shell::new();
    shell.prompt();
    let mut last_second = u64::MAX;
    loop {
        while let Some(c) = interrupts::pop_key() {
            shell.on_key(c);
        }
        let secs = interrupts::TICKS.load(Ordering::Relaxed) / pic::TIMER_HZ as u64;
        if secs != last_second {
            last_second = secs;
            update_tray(secs);
        }
        arch::hlt();
    }
}

fn update_tray(secs: u64) {
    let mut buf = [0u8; 64];
    let mut w = Cursor { buf: &mut buf, len: 0 };
    let _ = core::fmt::write(&mut w, format_args!(
        "{} CPUs   up {:02}:{:02}:{:02}",
        smp::ONLINE.load(Ordering::Relaxed), secs / 3600, (secs / 60) % 60, secs % 60));
    let len = w.len;
    let text = core::str::from_utf8(&buf[..len]).unwrap_or("");
    arch::without_interrupts(|| console::CONSOLE.lock().tray(text));
}

struct Cursor<'a> {
    buf: &'a mut [u8],
    len: usize,
}

impl core::fmt::Write for Cursor<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let n = s.len().min(self.buf.len() - self.len);
        self.buf[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
        self.len += n;
        Ok(())
    }
}

/// Exercises the allocator with the collection types later subsystems use.
fn heap_self_test() {
    let boxed = Box::new(0xAE20_F026u64);
    let v: Vec<u64> = (0..10_000).collect();
    let sum: u64 = v.iter().sum();
    let mut map = BTreeMap::new();
    for i in 0..256u32 {
        map.insert(i, i * i);
    }
    let mut s = String::new();
    s.push_str("AeroForge");
    let ok = *boxed == 0xAE20_F026 && sum == 49_995_000 && map[&255] == 65_025 && s.len() == 9;
    drop(v);
    if ok {
        kok!("Heap self-test passed (Box, Vec of 10k, BTreeMap, String)");
    } else {
        panic!("heap self-test failed");
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    arch::disable_interrupts();
    unsafe { console::force_unlock() };
    console::print_colored(console::RED, format_args!("\n*** AEROKERNEL PANIC ***\n{}\n", info));
    serial::write_str("\nSystem halted.\n");
    arch::halt_forever();
}
