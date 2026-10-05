//! AeroKernel: the AeroForge OS kernel core.

#![no_std]
#![no_main]

extern crate alloc;

mod acpi;
mod apic;
mod arch;
mod block;
mod bt;
mod console;
mod dhi;
mod elf;
mod exfat;
mod fat;
mod fb;
mod fpu;
mod futex;
mod gamepad;
mod gdt;
mod kstack;
mod interrupts;
mod ipc;
mod limine;
mod memory;
mod modules;
mod msi;
mod net;
mod ntfs;
mod pci;
mod percpu;
mod pic;
mod process;
mod rtc;
mod sched;
mod security;
mod serial;
mod shell;
mod sound;
mod smp;
mod sync;
mod tlb;
mod syscall;
mod usb;
mod vfs;

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
#[link_section = ".limine_requests"]
static MODULE_REQ: Request<ModuleResponse> = Request::new(limine::MODULE);

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

    // ---- Security: CPU protections first, so later mappings can use NX ----
    security::init_cpu(true);
    security::init_canary();

    // ---- Memory: everything after this may allocate ----
    let hhdm = HHDM_REQ.response().expect("no HHDM response").offset;
    let memmap = MEMMAP_REQ.response().expect("no memory map");
    let report = memory::init(hhdm, memmap);
    kok!("Buddy page allocator: {} MiB usable in {} regions ({} KiB metadata)",
        report.usable_bytes / (1024 * 1024), report.regions, report.meta_bytes / 1024);
    if let Some(k) = KERNEL_ADDR_REQ.response() {
        console::print_colored(console::DIM, format_args!(
            "       kernel image at phys {:#x} -> virt {:#x}\n", k.physical_base, k.virtual_base));
    }
    heap_self_test();
    let pages = security::protect_kernel_image();
    kok!("W^X: kernel code read-only, kernel data non-executable ({} pages)", pages);

    // ---- CPU tables ----
    let bsp_lapic = MP_REQ.response().map_or(0, |m| m.bsp_lapic_id);
    percpu::init_this_cpu(0, bsp_lapic);
    kok!("Per-CPU data via GS, GDT with ring-3 segments, TSS with double-fault IST");

    interrupts::init();
    kok!("IDT loaded: 256 vectors, syscall gate int 0x80 open to ring 3");
    syscall::init_cpu();
    kok!("Fast system calls: syscall/sysret entry open to ring 3");
    let f = fpu::init_cpu();
    kok!("Floating point for programs: x87, SSE{}{} ({}, {}-byte save area per thread)",
        if f.avx { ", AVX" } else { "" }, if f.avx512 { ", AVX-512" } else { "" },
        if f.xsave { "XSAVE" } else { "FXSAVE" }, f.area_bytes);
    unsafe { core::arch::asm!("int3") };
    if interrupts::BREAKPOINTS.load(Ordering::Relaxed) == 1 {
        kok!("Exception path verified (breakpoint handled, execution resumed)");
    }

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

    // ---- Interrupt controllers, timer, first C++ driver ----
    let khz = apic::init_bsp();
    kok!("Local APIC enabled, 8259 PIC masked, timer calibrated against the PIT ({} MHz bus clock)", khz);
    match apic::init_ioapic() {
        Ok(pins) => kok!("I/O APIC: {} redirection entries", pins),
        Err(e) => panic!("I/O APIC: {}", e),
    }
    let rc = unsafe { dhi::aero_ps2kbd_init(&dhi::OPS) };
    if rc == 0 {
        apic::route_isa_irq(1, interrupts::VECTOR_KEYBOARD, bsp_lapic);
        kok!("C++ PS/2 keyboard driver attached through DHI v{}, IRQ1 -> vector {}", dhi::ABI_VERSION, interrupts::VECTOR_KEYBOARD);
    } else {
        kprintln!("[WARN] PS/2 keyboard driver init failed ({})", rc);
    }

    // ---- PCIe and storage ----
    match pci::init() {
        Ok(n) => kok!("PCIe: {} function(s) found through ECAM", n),
        Err(e) => kprintln!("[WARN] PCIe: {}", e),
    }
    let nvme = block::probe_nvme();
    let sata = block::probe_ahci();
    for d in block::DEVICES.lock().iter() {
        console::print_colored(console::DIM, format_args!("       {}: {}\n", d.name(), d.describe()));
    }
    if nvme > 0 {
        kok!("C++ NVMe driver attached through DHI v{}: {} controller(s)", dhi::ABI_VERSION, nvme);
    }
    if sata > 0 {
        kok!("C++ AHCI driver attached through DHI v{}: {} SATA disk(s)", dhi::ABI_VERSION, sata);
    }
    let (xhci, usb_devices) = usb::probe();
    if xhci > 0 {
        kok!("C++ xHCI driver attached through DHI v{}: {} controller(s), {} USB device(s)", dhi::ABI_VERSION, xhci, usb_devices);
    }
    let usb_disks = block::probe_usb();
    for d in block::DEVICES.lock().iter().filter(|d| d.name().starts_with("usb")) {
        console::print_colored(console::DIM, format_args!("       {}: {}\n", d.name(), d.describe()));
    }
    if usb_disks > 0 {
        kok!("USB mass storage: {} disk(s)", usb_disks);
    }
    let bt_adapters = bt::probe();
    for a in bt::ADAPTERS.lock().iter() {
        console::print_colored(console::DIM, format_args!("       {}: USB {:04x}:{:04x} \"{}\"\n",
            a.name, a.usb.vendor, a.usb.product, dhi::c_field(&a.usb.name)));
    }
    if bt_adapters > 0 {
        kok!("Bluetooth: {} USB adapter(s), set-up continues on the bt thread", bt_adapters);
    }
    let nics = net::probe();
    for n in net::NICS.lock().iter() {
        console::print_colored(console::DIM, format_args!("       {}: Intel {:04x}:{:04x} ({} driver) at {}, MAC {}, link {}\n",
            n.name, n.pci_id.0, n.pci_id.1, n.driver_name, n.location, net::mac_string(&n.info.mac),
            if n.info.link_up != 0 { alloc::format!("up at {} Mb/s", n.info.speed_mbps) } else { "down".into() }));
    }
    if nics > 0 {
        kok!("C++ Intel Ethernet drivers (e1000/e1000e, igb/igc) attached through DHI v{}: {} port(s)", dhi::ABI_VERSION, nics);
    }
    let cards = sound::probe();
    for c in sound::CARDS.lock().iter() {
        let codecs: alloc::vec::Vec<alloc::string::String> = c.info.codec_ids[..c.info.codec_count as usize].iter()
            .map(|&id| sound::codec_name(id)).collect();
        let outputs: alloc::vec::Vec<alloc::string::String> = c.outputs().iter().map(sound::output_name).collect();
        console::print_colored(console::DIM, format_args!("       {}: {} {:04x}:{:04x} at {}, codecs: {}, outputs: {}\n",
            c.name, sound::card_name(c.pci_id.0), c.pci_id.0, c.pci_id.1, c.location,
            if codecs.is_empty() { alloc::string::String::from("none") } else { codecs.join(", ") },
            if outputs.is_empty() { alloc::string::String::from("none") } else { outputs.join(", ") }));
    }
    if cards > 0 {
        kok!("C++ HD Audio driver attached through DHI v{}: {} controller(s)", dhi::ABI_VERSION, cards);
    }
    let mounts = vfs::mount_all();
    for m in &mounts {
        kok!("{} volume \"{}\" on {} mounted at {}{}", m.vol.kind(), m.vol.label(), m.vol.dev().name(), m.path,
            if m.vol.read_only() { " (read-only)" } else { "" });
    }
    if mounts.is_empty() {
        kprintln!("[WARN] no FAT32, exFAT or NTFS volume found, running without files");
    }

    // ---- Scheduler ----
    sched::init_cpu();
    apic::start_timer(interrupts::VECTOR_TIMER);
    arch::enable_interrupts();
    let start = sched::ticks();
    while sched::ticks() < start + 5 {
        arch::hlt();
    }
    kok!("Scheduler running on cpu0, LAPIC timer at {} Hz", apic::TIMER_HZ);

    if let Some(mp) = MP_REQ.response() {
        let online = smp::start_aps(mp);
        kok!("SMP: {} of {} CPU(s) online, each with its own run queue", online, mp.cpu_count);
    }
    let slots = security::lock_kernel_half();
    kok!("NX: rest of the kernel half non-executable ({} top-level slots: direct map, heap, stacks)", slots);
    report_security();

    modules::init(MODULE_REQ.response());
    let firmware = modules::list().len() - modules::programs().count();
    kok!("{} user program(s) and {} firmware file(s) loaded by the bootloader", modules::programs().count(), firmware);

    kprintln!();
    console::print_colored(console::GREEN, format_args!("AeroKernel is up."));
    kprintln!(" Starting aerosmss, the session manager. Type 'help' for commands.");
    kprintln!();

    match process::spawn("aerosmss", 0) {
        Ok(pid) => console::print_colored(console::DIM, format_args!("[kernel] aerosmss started as pid {}\n", pid)),
        Err(e) => kprintln!("[WARN] could not start aerosmss: {}", e),
    }
    if xhci > 0 {
        sched::spawn_kernel("usbpoll", usb::poll_thread, 0, Some(1 % smp::ONLINE.load(core::sync::atomic::Ordering::SeqCst) as usize));
    }
    if nics > 0 {
        sched::spawn_kernel("net", net::net_thread, 0, Some(2 % smp::ONLINE.load(core::sync::atomic::Ordering::SeqCst) as usize));
    }
    if bt_adapters > 0 {
        sched::spawn_kernel("bt", bt::bt_thread, 0, Some(3 % smp::ONLINE.load(core::sync::atomic::Ordering::SeqCst) as usize));
    }
    if cards > 0 {
        sched::spawn_kernel("mixer", sound::mixer_thread, 0, Some(1 % smp::ONLINE.load(core::sync::atomic::Ordering::SeqCst) as usize));
    }
    sched::spawn_kernel("shell", shell::run, 0, Some(0));

    // The boot thread is now cpu0's idle thread.
    sched::idle_loop();
}

pub fn update_tray(secs: u64) {
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
    let big: Vec<u8> = alloc::vec![7u8; 256 * 1024]; // > 2 KiB: straight from the buddy allocator
    let ok = *boxed == 0xAE20_F026 && sum == 49_995_000 && map[&255] == 65_025 && s.len() == 9
        && big.iter().all(|&b| b == 7);
    drop(v);
    drop(big);
    if ok {
        kok!("Slab heap self-test passed (Box, Vec of 10k, BTreeMap, String, 256 KiB buffer)");
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

fn report_security() {
    let probe = kstack::KernelStack::new(4096);
    let a = security::audit(probe.as_ref().and_then(|s| s.guard_page()));
    drop(probe);
    let on = |b: bool| if b { "on" } else { "OFF" };
    kok!("Security: NX {}, SMEP {}, SMAP {}, UMIP {}, write-protect {}, stack canary from {}",
        on(a.nx), on(a.smep), on(a.smap), on(a.umip), on(a.wp), if a.canary_hw { "RDRAND" } else { "TSC" });
    if a.wx_pages == 0 && a.writable_code == 0 && a.exec_data == 0 && !a.heap_executable && a.stack_guard_unmapped {
        kok!("Security audit passed: {} kernel pages, 0 writable+executable, heap non-executable, stack guard pages unmapped",
            a.image_pages);
    } else {
        console::print_colored(console::YELLOW, format_args!(
            "[WARN] Security audit: {} W+X page(s), {} writable code page(s), {} executable data page(s), heap {}, stack guard {}\n",
            a.wx_pages, a.writable_code, a.exec_data,
            if a.heap_executable { "EXECUTABLE" } else { "non-executable" },
            if a.stack_guard_unmapped { "unmapped" } else { "MAPPED" }));
    }
}
