//! Legacy 8259 PICs: remapped away from the CPU exception vectors and then
//! masked, since the I/O APIC delivers device interrupts now.

use crate::arch::{io_wait, outb};

const PIC1_CMD: u16 = 0x20;
const PIC1_DATA: u16 = 0x21;
const PIC2_CMD: u16 = 0xA0;
const PIC2_DATA: u16 = 0xA1;

pub fn remap_and_mask() {
    unsafe {
        for (port, v) in [
            (PIC1_CMD, 0x11), (PIC2_CMD, 0x11), // init
            (PIC1_DATA, 0x20), (PIC2_DATA, 0x28), // vector offsets 32 / 40
            (PIC1_DATA, 4), (PIC2_DATA, 2),       // cascade
            (PIC1_DATA, 1), (PIC2_DATA, 1),       // 8086 mode
        ] {
            outb(port, v);
            io_wait();
        }
        outb(PIC1_DATA, 0xFF);
        outb(PIC2_DATA, 0xFF);
    }
}
