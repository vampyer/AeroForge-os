//! rotest: writes over its own code. Code pages are read-only, so the CPU
//! refuses and the kernel kills this program.

#![no_std]
#![no_main]

aero::entry!(main);

fn main() -> i64 {
    aero::println!("[rotest] pid {} overwriting its own code (must be stopped)...", aero::getpid());
    unsafe { core::ptr::write_volatile(main as fn() -> i64 as usize as *mut u8, 0xCC) };
    aero::println!("[rotest] NOT BLOCKED: code was modified");
    1
}
