//! echod: a service that answers "ping N" with "pong N" on the reply port the
//! client sends along with each message.

#![no_std]
#![no_main]

use aero::{format_buf, port, println};

aero::entry!(main);

fn main() -> i64 {
    let svc = port::create().expect("port_create");
    port::publish(&svc, "echo").expect("publish");
    println!("[echod] pid {} on cpu{}: serving port \"echo\"", aero::getpid(), aero::cpu_id());

    let mut buf = [0u8; 256];
    let mut served = 0u64;
    loop {
        let Ok(msg) = port::recv(&svc, &mut buf) else { continue };
        let Some(reply_to) = msg.handle else { continue };
        let text = core::str::from_utf8(&buf[..msg.len]).unwrap_or("?");
        let n = text.strip_prefix("ping ").unwrap_or("?");
        served += 1;
        let reply = format_buf!("pong {} from echod (cpu{}, request #{} from pid {})", n, aero::cpu_id(), served, msg.sender);
        let _ = port::send(&reply_to, reply.as_bytes(), None);
        // reply_to is dropped here: the client's reply right is used once and closed.
    }
}
