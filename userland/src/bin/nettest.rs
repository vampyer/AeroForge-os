//! nettest: UDP and TCP sockets against the boot test's echo server
//! (tools/echo-server.py on the host, 10.0.2.2 in QEMU's user network).
//!
//! It waits for a DHCP address, then checks: a UDP datagram comes back
//! unchanged; a UDP receive with nothing to get times out after its 50 ms;
//! 64 KiB sent over TCP comes back in order, followed by the end of the
//! data when the server closes; and a TCP connection to a port nobody
//! listens on is refused.
//!
//! Then names and servers: the network info shows a DNS server; the host's
//! test DNS server (UDP 5553) resolves aeroforge.test through a CNAME and
//! says missing.test does not exist; an `accept` with nobody connecting
//! times out; and a server on port 7070 (QEMU forwards host port 5580 to
//! it) takes a connection from the host and answers its line.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;

use aero::net::{self, Endpoint, Ip, Socket};
use aero::{clock_us, println, E_CLOSED, E_NOTFOUND, E_TIMEDOUT};

aero::entry!(main);

const HOST: [u8; 4] = [10, 0, 2, 2];
const UDP_ECHO: Endpoint = Endpoint::new(HOST, 5555);
const TCP_ECHO: Endpoint = Endpoint::new(HOST, 5556);
const TCP_CLOSED: Endpoint = Endpoint::new(HOST, 5557);
const TCP_TOTAL: usize = 64 * 1024;
const TEST_DNS: Endpoint = Endpoint::new(HOST, 5553);
const SERVER_PORT: u16 = 7070;

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

    if let Err(e) = names() {
        return e;
    }
    if let Err(e) = server() {
        return e;
    }
    println!("[nettest] network info, DNS lookups and a TCP server: OK");
    0
}

/// Network info and DNS lookups.
fn names() -> Result<(), i64> {
    let info = net::info().map_err(|e| fail("no network info", e))?;
    println!("[nettest] address {}/{}, gateway {}, DNS {}", Ip(info.addr), info.prefix_len,
        Ip(info.gateway), Ip(info.dns));
    if info.dns == [0; 4] {
        return Err(fail("DHCP gave no DNS server", 0));
    }

    let t0 = clock_us();
    match net::resolve_with(TEST_DNS, "aeroforge.test", 3_000_000) {
        Ok(a) if a == HOST => println!("[nettest] aeroforge.test is {} ({} us)", Ip(a), clock_us() - t0),
        Ok(a) => return Err(fail("aeroforge.test resolved to the wrong address", u32::from_be_bytes(a) as i64)),
        Err(e) => return Err(fail("aeroforge.test did not resolve", e)),
    }
    match net::resolve_with(TEST_DNS, "missing.test", 3_000_000) {
        Err(E_NOTFOUND) => println!("[nettest] missing.test: no such name"),
        r => return Err(fail("missing.test was not reported missing", r.map_or_else(|e| e, |_| 0))),
    }
    if net::resolve("10.0.2.2", 0) != Ok(HOST) {
        return Err(fail("a dotted address was not taken as is", 0));
    }
    // The real DNS server, through QEMU to the host's resolver. The host may
    // be offline, so this is reported but not required.
    match net::resolve("example.com", 3_000_000) {
        Ok(a) => println!("[nettest] example.com is {} (DHCP's DNS server)", Ip(a)),
        Err(e) => println!("[nettest] example.com did not resolve through DHCP's DNS server ({}), not required", e),
    }
    Ok(())
}

/// A TCP server: accept timeout, then one connection from the host.
fn server() -> Result<(), i64> {
    let idle = Socket::tcp().map_err(|e| fail("no TCP socket", e))?;
    idle.listen(SERVER_PORT + 1).map_err(|e| fail("listen", e))?;
    let t0 = clock_us();
    let r = idle.accept(50_000);
    let took = clock_us() - t0;
    println!("[nettest] accept with a 50 ms timeout and nobody connecting returned {:?} after {} us",
        r.as_ref().map(|_| ()), took);
    if !matches!(r, Err(E_TIMEDOUT)) || took < 50_000 || took > 500_000 {
        return Err(fail("accept timeout", r.err().unwrap_or(0)));
    }
    drop(idle);

    let listener = Socket::tcp().map_err(|e| fail("no TCP socket", e))?;
    listener.listen(SERVER_PORT).map_err(|e| fail("listen", e))?;
    println!("[nettest] listening on port {}", SERVER_PORT);
    let conn = listener.accept(30_000_000).map_err(|e| fail("no connection from the host", e))?;
    let mut line = [0u8; 128];
    let mut n = 0;
    while !line[..n].contains(&b'\n') && n < line.len() {
        match conn.recv(&mut line[n..], 5_000_000) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) => return Err(fail("recv from the host", e)),
        }
    }
    conn.send_all(b"AeroForge heard: ").and_then(|_| conn.send_all(&line[..n])).map_err(|e| fail("send to the host", e))?;
    println!("[nettest] server got {:?} from the host and answered",
        core::str::from_utf8(&line[..n]).unwrap_or("?").trim_end());
    Ok(())
}
