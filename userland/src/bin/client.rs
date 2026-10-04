//! client: looks up the "echo" service and does a few request/reply round
//! trips, passing a send-only handle to its own reply port each time.

#![no_std]
#![no_main]

use aero::{format_buf, port, println, rights};

aero::entry!(main);

const ROUNDS: u64 = 3;

fn main() -> i64 {
    let pid = aero::getpid();
    let echo = loop {
        match port::lookup("echo") {
            Ok(h) => break h,
            Err(_) => aero::sleep_ms(20), // service not published yet
        }
    };
    let reply = port::create().expect("port_create");
    let mut buf = [0u8; 256];

    for i in 1..=ROUNDS {
        let t0 = aero::uptime_ms();
        let ticket = reply.dup(rights::SEND).expect("dup");
        let msg = format_buf!("ping {}", i);
        if let Err(e) = port::send(&echo, msg.as_bytes(), Some(ticket)) {
            println!("[client {}] send failed: {}", pid, e);
            return 1;
        }
        let r = port::recv(&reply, &mut buf).expect("recv");
        let text = core::str::from_utf8(&buf[..r.len]).unwrap_or("?");
        println!("[client {} on cpu{}] ping {} -> {} [{} ms]", pid, aero::cpu_id(), i, text, aero::uptime_ms() - t0);
        aero::sleep_ms(50);
    }
    println!("[client {}] done, exiting", pid);
    0
}
