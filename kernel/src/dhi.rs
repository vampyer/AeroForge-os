//! Rust side of the Driver Host Interface. Must match drivers/include/dhi.h.

use core::ffi::{c_char, CStr};

use crate::arch;

pub const ABI_VERSION: u32 = 1;

#[repr(C)]
pub struct DhiOps {
    pub abi_version: u32,
    pub _reserved: u32,
    pub log: extern "C" fn(*const c_char),
    pub port_in8: extern "C" fn(u16) -> u8,
    pub port_out8: extern "C" fn(u16, u8),
}

#[repr(C)]
#[derive(Default, Debug, Clone, Copy)]
pub struct KeyEvent {
    pub scancode: u8,
    pub pressed: u8,
    pub ascii: u8,
    pub modifiers: u8,
}

extern "C" {
    pub fn aero_ps2kbd_init(ops: *const DhiOps) -> i32;
    pub fn aero_ps2kbd_on_irq(out: *mut KeyEvent) -> i32;
}

extern "C" fn dhi_log(msg: *const c_char) {
    if msg.is_null() {
        return;
    }
    let s = unsafe { CStr::from_ptr(msg) }.to_str().unwrap_or("<invalid utf-8>");
    crate::console::print_colored(crate::console::DIM, format_args!("       [driver] {}\n", s));
}

extern "C" fn dhi_in8(port: u16) -> u8 {
    unsafe { arch::inb(port) }
}

extern "C" fn dhi_out8(port: u16, v: u8) {
    unsafe { arch::outb(port, v) }
}

/// Lives for the whole kernel lifetime, as the DHI contract requires.
pub static OPS: DhiOps = DhiOps {
    abi_version: ABI_VERSION,
    _reserved: 0,
    log: dhi_log,
    port_in8: dhi_in8,
    port_out8: dhi_out8,
};
