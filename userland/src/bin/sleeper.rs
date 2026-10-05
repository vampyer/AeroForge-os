//! sleeper: sleeps for 10 seconds and exits. waittest starts it to have a
//! child process to wait for and kill.

#![no_std]
#![no_main]

aero::entry!(main);

fn main() -> i64 {
    aero::sleep_ms(10_000);
    0
}
