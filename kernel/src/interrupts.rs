//! IDT, exception handlers and interrupt dispatch.
//!
//! Every vector has a tiny assembly stub that pushes a uniform frame and
//! calls `isr_dispatch` in Rust, so this works on stable Rust without the
//! unstable `x86-interrupt` calling convention.

use core::arch::{asm, global_asm};
use core::mem::size_of;
use core::ptr::{addr_of, addr_of_mut};
use core::sync::atomic::{AtomicU64, AtomicUsize, AtomicU8, Ordering};

use crate::{apic, console, dhi, gdt, sched, syscall};

pub const VECTOR_TIMER: u8 = 32;
pub const VECTOR_KEYBOARD: u8 = 33;
pub const VECTOR_SYSCALL: u8 = 0x80;
pub const VECTOR_RESCHEDULE: u8 = 0xF0;

global_asm!(include_str!(concat!(env!("OUT_DIR"), "/isr_stubs.s")));

extern "C" {
    static isr_stub_table: [u64; 256];
}

/// Register state saved by `isr_common`, lowest address first.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct InterruptFrame {
    pub r15: u64, pub r14: u64, pub r13: u64, pub r12: u64,
    pub r11: u64, pub r10: u64, pub r9: u64, pub r8: u64,
    pub rbp: u64, pub rdi: u64, pub rsi: u64, pub rdx: u64,
    pub rcx: u64, pub rbx: u64, pub rax: u64,
    pub vector: u64,
    pub error_code: u64,
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

impl InterruptFrame {
    pub const fn zeroed() -> Self {
        Self {
            r15: 0, r14: 0, r13: 0, r12: 0, r11: 0, r10: 0, r9: 0, r8: 0,
            rbp: 0, rdi: 0, rsi: 0, rdx: 0, rcx: 0, rbx: 0, rax: 0,
            vector: 0, error_code: 0, rip: 0, cs: 0, rflags: 0, rsp: 0, ss: 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct IdtEntry {
    offset_lo: u16,
    selector: u16,
    ist: u8,
    attributes: u8,
    offset_mid: u16,
    offset_hi: u32,
    _zero: u32,
}

impl IdtEntry {
    const EMPTY: Self = Self { offset_lo: 0, selector: 0, ist: 0, attributes: 0, offset_mid: 0, offset_hi: 0, _zero: 0 };

    fn gate(handler: u64, ist: u8, dpl: u8) -> Self {
        Self {
            offset_lo: handler as u16,
            selector: gdt::KERNEL_CODE,
            ist,
            attributes: 0x8E | (dpl << 5), // present, 64-bit interrupt gate
            offset_mid: (handler >> 16) as u16,
            offset_hi: (handler >> 32) as u32,
            _zero: 0,
        }
    }
}

static mut IDT: [IdtEntry; 256] = [IdtEntry::EMPTY; 256];

const EXCEPTION_NAMES: [&str; 32] = [
    "Divide Error", "Debug", "NMI", "Breakpoint", "Overflow", "Bound Range",
    "Invalid Opcode", "Device Not Available", "Double Fault", "Coprocessor Overrun",
    "Invalid TSS", "Segment Not Present", "Stack Fault", "General Protection",
    "Page Fault", "Reserved", "x87 FP", "Alignment Check", "Machine Check", "SIMD FP",
    "Virtualization", "Control Protection", "Reserved", "Reserved", "Reserved",
    "Reserved", "Reserved", "Reserved", "Hypervisor Injection", "VMM Communication",
    "Security", "Reserved",
];

pub static BREAKPOINTS: AtomicU64 = AtomicU64::new(0);

/// Single-producer (keyboard IRQ) / single-consumer (shell thread) key queue.
struct KeyQueue {
    buf: [AtomicU8; 256],
    head: AtomicUsize,
    tail: AtomicUsize,
}

static KEYS: KeyQueue = KeyQueue {
    buf: [const { AtomicU8::new(0) }; 256],
    head: AtomicUsize::new(0),
    tail: AtomicUsize::new(0),
};

pub fn push_key(c: u8) {
    let head = KEYS.head.load(Ordering::Relaxed);
    let next = (head + 1) % 256;
    if next != KEYS.tail.load(Ordering::Acquire) {
        KEYS.buf[head].store(c, Ordering::Relaxed);
        KEYS.head.store(next, Ordering::Release);
    }
}

pub fn pop_key() -> Option<u8> {
    let tail = KEYS.tail.load(Ordering::Relaxed);
    if tail == KEYS.head.load(Ordering::Acquire) {
        return None;
    }
    let c = KEYS.buf[tail].load(Ordering::Relaxed);
    KEYS.tail.store((tail + 1) % 256, Ordering::Release);
    Some(c)
}

pub fn init() {
    unsafe {
        let idt = &mut *addr_of_mut!(IDT);
        let stubs = &*addr_of!(isr_stub_table);
        for (vector, &stub) in stubs.iter().enumerate() {
            let ist = if vector == 8 { gdt::DOUBLE_FAULT_IST } else { 0 };
            // Only the syscall gate (and int3, for debuggers) may be raised from ring 3.
            let dpl = if vector == VECTOR_SYSCALL as usize || vector == 3 { 3 } else { 0 };
            idt[vector] = IdtEntry::gate(stub, ist, dpl);
        }
    }
    load();
}

/// Loads the shared IDT on the calling CPU (also used by application processors).
pub fn load() {
    #[repr(C, packed)]
    struct Pointer {
        limit: u16,
        base: u64,
    }
    let ptr = Pointer { limit: (size_of::<[IdtEntry; 256]>() - 1) as u16, base: addr_of!(IDT) as u64 };
    unsafe { asm!("lidt [{}]", in(reg) &ptr, options(readonly, nostack, preserves_flags)) };
}

#[no_mangle]
extern "C" fn isr_dispatch(frame: &mut InterruptFrame) {
    let vector = frame.vector as u8;
    match vector {
        3 => {
            BREAKPOINTS.fetch_add(1, Ordering::Relaxed);
            crate::kprintln!("       #BP breakpoint caught at {:#x}, resuming", frame.rip);
        }
        0..=31 => exception(frame),
        VECTOR_TIMER => {
            apic::eoi();
            sched::on_tick();
        }
        VECTOR_KEYBOARD => {
            let mut ev = dhi::KeyEvent::default();
            if unsafe { dhi::aero_ps2kbd_on_irq(&mut ev) } == 1 && ev.pressed == 1 && ev.ascii != 0 {
                push_key(ev.ascii);
            }
            apic::eoi();
        }
        VECTOR_SYSCALL => syscall::dispatch(frame),
        VECTOR_RESCHEDULE => {
            apic::eoi();
            sched::schedule();
        }
        apic::SPURIOUS_VECTOR => {}
        _ => apic::eoi(),
    }
}

fn exception(frame: &InterruptFrame) {
    let name = EXCEPTION_NAMES[frame.vector as usize];
    if frame.cs & 3 == 3 {
        // A user process faulted: kill it, keep the system running.
        let t = sched::current();
        let (pid, pname) = t.process.as_ref().map_or((0, "?"), |p| (p.pid, p.name.as_str()));
        console::print_colored(console::RED, format_args!(
            "[kernel] {} (pid {}) killed: {} at rip {:#x}{}\n",
            pname, pid, name, frame.rip,
            if frame.vector == 14 { alloc::format!(", address {:#x}", crate::arch::read_cr2()) } else { alloc::string::String::new() }
        ));
        if let Some(p) = t.process.as_ref() {
            p.exit_code.store(-(frame.vector as i64) - 1000, Ordering::SeqCst);
        }
        drop(t);
        sched::exit_current();
    }
    if frame.vector == 14 {
        panic!(
            "CPU exception #{} {} (error {:#x})\n  faulting address {:#x}\n  rip {:#x}  rsp {:#x}",
            frame.vector, name, frame.error_code, crate::arch::read_cr2(), frame.rip, frame.rsp
        );
    }
    panic!(
        "CPU exception #{} {} (error {:#x})\n  rip {:#x}  rsp {:#x}  rflags {:#x}",
        frame.vector, name, frame.error_code, frame.rip, frame.rsp, frame.rflags
    );
}
