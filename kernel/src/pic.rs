//! Legacy 8259 PIC and 8254 PIT. Enough for a timer and the keyboard until
//! the x2APIC / HPET path from the design doc replaces them.

use crate::arch::{inb, io_wait, outb};
use crate::interrupts::IRQ_BASE;

const PIC1_CMD: u16 = 0x20;
const PIC1_DATA: u16 = 0x21;
const PIC2_CMD: u16 = 0xA0;
const PIC2_DATA: u16 = 0xA1;

pub const TIMER_HZ: u32 = 100;

/// Remaps IRQ 0-15 to vectors 32-47 and unmasks only the given IRQs.
pub fn init(enabled_irqs: &[u8]) {
    unsafe {
        outb(PIC1_CMD, 0x11);
        io_wait();
        outb(PIC2_CMD, 0x11);
        io_wait();
        outb(PIC1_DATA, IRQ_BASE);
        io_wait();
        outb(PIC2_DATA, IRQ_BASE + 8);
        io_wait();
        outb(PIC1_DATA, 4); // slave on IRQ2
        io_wait();
        outb(PIC2_DATA, 2);
        io_wait();
        outb(PIC1_DATA, 0x01); // 8086 mode
        io_wait();
        outb(PIC2_DATA, 0x01);
        io_wait();

        let mut mask: u16 = 0xFFFF;
        for &irq in enabled_irqs {
            mask &= !(1 << irq);
        }
        if mask & 0xFF00 != 0xFF00 {
            mask &= !(1 << 2); // cascade line
        }
        outb(PIC1_DATA, mask as u8);
        outb(PIC2_DATA, (mask >> 8) as u8);
    }
}

pub fn end_of_interrupt(irq: u8) {
    unsafe {
        if irq >= 8 {
            outb(PIC2_CMD, 0x20);
        }
        outb(PIC1_CMD, 0x20);
    }
}

/// Programs PIT channel 0 as a periodic tick.
pub fn init_timer() {
    let divisor = (1_193_182 / TIMER_HZ) as u16;
    unsafe {
        outb(0x43, 0x36); // channel 0, lo/hi, mode 3
        outb(0x40, divisor as u8);
        outb(0x40, (divisor >> 8) as u8);
        let _ = inb(0x61);
    }
}
