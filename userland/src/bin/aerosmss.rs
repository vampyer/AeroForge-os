//! aerosmss: the AeroForge session manager, first user process.
//! Starts the system services, then the demo clients, and stays resident.

#![no_std]
#![no_main]

use aero::println;

aero::entry!(main);

const SERVICES: &[&str] = &["echod"];
const CLIENTS: usize = 3;

fn main() -> i64 {
    println!("[aerosmss] session manager running as pid {} on cpu{}", aero::getpid(), aero::cpu_id());
    for svc in SERVICES {
        match aero::spawn(svc) {
            Ok(pid) => println!("[aerosmss] started service {} (pid {})", svc, pid),
            Err(e) => println!("[aerosmss] failed to start {}: error {}", svc, e),
        }
    }
    for _ in 0..CLIENTS {
        match aero::spawn("client") {
            Ok(pid) => println!("[aerosmss] started client (pid {})", pid),
            Err(e) => println!("[aerosmss] failed to start client: error {}", e),
        }
    }
    println!("[aerosmss] all services up");
    loop {
        aero::sleep_ms(1000);
    }
}
