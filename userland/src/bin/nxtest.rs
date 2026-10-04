//! nxtest: copies a `ret` instruction onto its stack and jumps to it. With
//! no-execute stacks the CPU refuses and the kernel kills this program.

#![no_std]
#![no_main]

aero::entry!(main);

fn main() -> i64 {
    let code = [0xC3u8; 16]; // ret
    aero::println!("[nxtest] pid {} running code from its stack (must be stopped)...", aero::getpid());
    let f: extern "C" fn() = unsafe { core::mem::transmute(code.as_ptr()) };
    f();
    aero::println!("[nxtest] NOT BLOCKED: code on the stack ran");
    1
}
