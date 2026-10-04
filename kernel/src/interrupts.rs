//! IDT, exception handlers and hardware IRQ dispatch.
//!
//! Each vector has a tiny assembly stub that pushes a uniform frame and
//! calls `isr_dispatch` in Rust, so this works on stable Rust without the
//! unstable `x86-interrupt` calling convention.

use core::arch::{asm, global_asm};
use core::mem::size_of;
use core::ptr::{addr_of, addr_of_mut};
use core::sync::atomic::{AtomicU64, AtomicUsize, AtomicU8, Ordering};

use crate::{arch, dhi, gdt, pic};

pub const IRQ_BASE: u8 = 32;
pub const IRQ_TIMER: u8 = IRQ_BASE;
pub const IRQ_KEYBOARD: u8 = IRQ_BASE + 1;
const STUB_COUNT: usize = 48;

global_asm!(
    r#"
.section .text
.macro ISR_NOERR n
isr_stub_\n:
    push 0
    push \n
    jmp isr_common
.endm
.macro ISR_ERR n
isr_stub_\n:
    push \n
    jmp isr_common
.endm

.irp n, 0,1,2,3,4,5,6,7,9,15,16,18,19,20,22,23,24,25,26,27,28,31
ISR_NOERR \n
.endr
.irp n, 8,10,11,12,13,14,17,21,29,30
ISR_ERR \n
.endr
.irp n, 32,33,34,35,36,37,38,39,40,41,42,43,44,45,46,47
ISR_NOERR \n
.endr

isr_common:
    push rax
    push rbx
    push rcx
    push rdx
    push rsi
    push rdi
    push rbp
    push r8
    push r9
    push r10
    push r11
    push r12
    push r13
    push r14
    push r15
    mov rdi, rsp
    cld
    call isr_dispatch
    pop r15
    pop r14
    pop r13
    pop r12
    pop r11
    pop r10
    pop r9
    pop r8
    pop rbp
    pop rdi
    pop rsi
    pop rdx
    pop rcx
    pop rbx
    pop rax
    add rsp, 16
    iretq

.section .rodata
.balign 8
.global isr_stub_table
isr_stub_table:
.irp n, 0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31,32,33,34,35,36,37,38,39,40,41,42,43,44,45,46,47
    .quad isr_stub_\n
.endr
"#
);

extern "C" {
    static isr_stub_table: [u64; STUB_COUNT];
}

/// Register state saved by `isr_common`, lowest address first.
#[repr(C)]
#[derive(Debug)]
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

    fn interrupt_gate(handler: u64, ist: u8) -> Self {
        Self {
            offset_lo: handler as u16,
            selector: gdt::KERNEL_CODE,
            ist,
            attributes: 0x8E, // present, DPL0, 64-bit interrupt gate
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

pub static TICKS: AtomicU64 = AtomicU64::new(0);
pub static BREAKPOINTS: AtomicU64 = AtomicU64::new(0);

/// Single-producer (IRQ1) / single-consumer (main loop) key queue.
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

fn push_key(c: u8) {
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
            idt[vector] = IdtEntry::interrupt_gate(stub, ist);
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
        IRQ_TIMER => {
            TICKS.fetch_add(1, Ordering::Relaxed);
            pic::end_of_interrupt(0);
        }
        IRQ_KEYBOARD => {
            let mut ev = dhi::KeyEvent::default();
            if unsafe { dhi::aero_ps2kbd_on_irq(&mut ev) } == 1 && ev.pressed == 1 && ev.ascii != 0 {
                push_key(ev.ascii);
            }
            pic::end_of_interrupt(1);
        }
        v => pic::end_of_interrupt(v - IRQ_BASE),
    }
}

fn exception(frame: &InterruptFrame) -> ! {
    let name = EXCEPTION_NAMES[frame.vector as usize];
    if frame.vector == 14 {
        panic!(
            "CPU exception #{} {} (error {:#x})\n  faulting address {:#x}\n  rip {:#x}  rsp {:#x}",
            frame.vector, name, frame.error_code, arch::read_cr2(), frame.rip, frame.rsp
        );
    }
    panic!(
        "CPU exception #{} {} (error {:#x})\n  rip {:#x}  rsp {:#x}  rflags {:#x}",
        frame.vector, name, frame.error_code, frame.rip, frame.rsp, frame.rflags
    );
}
