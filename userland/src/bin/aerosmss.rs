//! aerosmss: the AeroForge session manager, first user process.
//! Reads the list of programs to start from /system/session.cfg on disk
//! (falling back to a built-in list), starts them, and stays resident.

#![no_std]
#![no_main]

use aero::println;

aero::entry!(main);

const CONFIG: &str = "/system/session.cfg";
const DEFAULT_SESSION: &str = "echod\nclient\nclient\nclient\n";

fn main() -> i64 {
    println!("[aerosmss] session manager running as pid {} on cpu{}", aero::getpid(), aero::cpu_id());

    let mut buf = [0u8; 2048];
    let session = match aero::read_file(CONFIG, &mut buf) {
        Ok(n) => {
            println!("[aerosmss] read {} ({} bytes) from disk", CONFIG, n);
            core::str::from_utf8(&buf[..n]).unwrap_or(DEFAULT_SESSION)
        }
        Err(_) => {
            println!("[aerosmss] {} not found, using the built-in session", CONFIG);
            DEFAULT_SESSION
        }
    };

    // One program per line; '#' starts a comment.
    for line in session.lines() {
        let program = line.split('#').next().unwrap_or("").trim();
        if program.is_empty() {
            continue;
        }
        match aero::spawn(program) {
            Ok(pid) => println!("[aerosmss] started {} (pid {})", program, pid),
            Err(e) => println!("[aerosmss] failed to start {}: error {}", program, e),
        }
    }
    println!("[aerosmss] all services up");
    loop {
        aero::sleep_ms(1000);
    }
}
