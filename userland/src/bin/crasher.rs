//! crasher: writes through a null pointer, to show that a faulting process
//! is killed while the rest of the system keeps running.

#![no_std]
#![no_main]

aero::entry!(main);

fn main() -> i64 {
    aero::println!("[crasher] pid {} about to dereference null...", aero::getpid());
    unsafe { core::ptr::write_volatile(core::ptr::null_mut::<u64>(), 0xDEAD) };
    0
}
