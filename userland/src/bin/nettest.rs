//! nettest: UDP and TCP sockets against the boot test's echo server
//! (tools/echo-server.py on the host, 10.0.2.2 in QEMU's user network).
//!
//! It waits for a DHCP address, then checks: a UDP datagram comes back
//! unchanged; a UDP receive with nothing to get times out after its 50 ms;
//! 64 KiB sent over TCP comes back in order, followed by the end of the
//! data when the server closes; and a TCP connection to a port nobody
//! listens on is refused.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;

use aero::net::{Endpoint, Socket};
use aero::{clock_us, println, E_CLOSED, E_NOTFOUND, E_TIMEDOUT};

aero::entry!(main);

const HOST: [u8; 4] = [10, 0, 2, 2];
const UDP_ECHO: Endpoint = Endpoint::new(HOST, 5555);
const TCP_ECHO: Endpoint = Endpoint::new(HOST, 5556);
const TCP_CLOSED: Endpoint = Endpoint::new(HOST, 5557);
const TCP_TOTAL: usize = 64 * 1024;

fn fail(what: &str, e: i64) -> i64 {
    println!("[nettest] FAILED: {} ({})", what, e);
    1
}

fn main() -> i64 {
    // 1. A UDP socket; connecting it needs an address from DHCP.
    let udp = match Socket::udp() {
        Ok(s) => s,
        Err(e) => return fail("no UDP socket", e),
    };
    let mut waited = 0;
    loop {
        match udp.connect(UDP_ECHO, 0) {
            Ok(()) => break,
            Err(E_NOTFOUND) if waited < 100 => {
                waited += 1;
                aero::sleep_ms(100);
            }
            Err(e) => return fail("UDP connect (no network?)", e),
        }
    }

    // 2. UDP echo. The first datagram may wait for ARP, so allow retries.
    let mut buf = [0u8; 256];
    let mut echoed = false;
    for attempt in 0..5u32 {
        let msg = aero::format_buf!("AeroForge UDP echo #{}", attempt);
        let t0 = clock_us();
        if let Err(e) = udp.send(msg.as_bytes()) {
            return fail("UDP send", e);
        }
        match udp.recv(&mut buf, 1_000_000) {
            Ok(n) if &buf[..n] == msg.as_bytes() => {
                println!("[nettest] UDP echo from {}: {} bytes back in {} us", UDP_ECHO, n, clock_us() - t0);
                echoed = true;
                break;
            }
            Ok(n) => return fail("UDP echo came back changed", n as i64),
            Err(E_TIMEDOUT) => continue,
            Err(e) => return fail("UDP recv", e),
        }
    }
    if !echoed {
        return fail("no UDP echo in 5 tries", E_TIMEDOUT);
    }

    // 3. Nothing more to receive: the wait must end at its timeout.
    let t0 = clock_us();
    let r = udp.recv(&mut buf, 50_000);
    let took = clock_us() - t0;
    println!("[nettest] UDP recv with a 50 ms timeout and nothing sent returned {:?} after {} us", r, took);
    if r != Err(E_TIMEDOUT) || took < 50_000 || took > 500_000 {
        return fail("UDP recv timeout", r.err().unwrap_or(0));
    }
    drop(udp);

    // 4. TCP: 64 KiB there and back, then the server closes.
    let tcp = match Socket::tcp() {
        Ok(s) => s,
        Err(e) => return fail("no TCP socket", e),
    };
    let t0 = clock_us();
    if let Err(e) = tcp.connect(TCP_ECHO, 5_000_000) {
        return fail("TCP connect", e);
    }
    let data: Vec<u8> = (0..TCP_TOTAL).map(|i| (i * 7 + i / 251) as u8).collect();
    if let Err(e) = tcp.send_all(&data) {
        return fail("TCP send", e);
    }
    let mut back = vec![0u8; TCP_TOTAL];
    let mut got = 0;
    let mut reads = 0;
    let eof = loop {
        match tcp.recv(&mut back[got..], 5_000_000) {
            Ok(0) => break true,
            Ok(n) if got + n <= TCP_TOTAL => {
                got += n;
                reads += 1;
                if got == TCP_TOTAL {
                    // The server closes now: the next read must say so.
                    let mut extra = [0u8; 16];
                    match tcp.recv(&mut extra, 5_000_000) {
                        Ok(0) => break true,
                        Ok(_) => return fail("TCP: more data than was sent", 0),
                        Err(e) => return fail("TCP: no end of data after 64 KiB", e),
                    }
                }
            }
            Ok(_) => return fail("TCP: read past the buffer", 0),
            Err(e) => return fail("TCP recv", e),
        }
    };
    let took = clock_us() - t0;
    if got != TCP_TOTAL || back != data {
        let first_bad = back.iter().zip(&data).position(|(a, b)| a != b).unwrap_or(got);
        println!("[nettest] TCP: got {} of {} bytes, first difference at {}", got, TCP_TOTAL, first_bad);
        return fail("TCP echo came back wrong", got as i64);
    }
    println!("[nettest] TCP echo from {}: {} bytes back unchanged in {} reads, {} us, end of data: {}",
        TCP_ECHO, got, reads, took, eof);
    drop(tcp);

    // 5. Nobody listens here: the connection must be refused, not time out.
    let tcp = match Socket::tcp() {
        Ok(s) => s,
        Err(e) => return fail("no TCP socket", e),
    };
    let t0 = clock_us();
    let r = tcp.connect(TCP_CLOSED, 5_000_000);
    println!("[nettest] TCP connect to {} returned {:?} after {} us", TCP_CLOSED, r, clock_us() - t0);
    if r != Err(E_CLOSED) {
        return fail("connecting to a closed port was not refused", r.err().unwrap_or(0));
    }

    println!("[nettest] UDP echo, receive timeout, 64 KiB TCP echo and a refused connection: OK");
    0
}
