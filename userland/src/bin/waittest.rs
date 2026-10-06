//! waittest: one thread waiting on several things at once with `wait_any`.
//!
//! It checks that an auto-reset event set by another thread ends a wait
//! and is used up by it; that a manual-reset event stays set until reset;
//! that a wait on a port, an event and a UDP socket returns the socket when
//! the host's echo comes back (tools/echo-server.py); and that a wait on a
//! child process ends when the child is killed, with the code it was given.

#![no_std]
#![no_main]

use aero::net::{Endpoint, Socket};
use aero::process;
use aero::thread;
use aero::{clock_us, port, println, wait_any, Child, Event, E_TIMEDOUT};

aero::entry!(main);

fn fail(what: &str, r: Result<usize, i64>) -> i64 {
    println!("[waittest] FAILED: {} ({:?})", what, r);
    1
}

fn main() -> i64 {
    // 1. Auto-reset event, set by another thread after 5 ms.
    let ev = Event::auto_reset().expect("event");
    let raw = ev.0 .0;
    let setter = thread::spawn(move || {
        aero::sleep_us(5000);
        let e = Event(aero::Handle(raw));
        e.set().ok();
        core::mem::forget(e); // the main thread owns the handle
    })
    .expect("spawn");
    let t0 = clock_us();
    let r = wait_any(&[&ev.0], 1_000_000);
    let took = clock_us() - t0;
    setter.join();
    if r != Ok(0) || took > 500_000 {
        return fail("auto-reset event did not end the wait", r);
    }
    let again = wait_any(&[&ev.0], 20_000);
    if again != Err(E_TIMEDOUT) {
        return fail("auto-reset event was not used up by the first wait", again);
    }
    println!("[waittest] auto-reset event set by another thread ended the wait after {} us, then was used up", took);

    // 2. Manual-reset event: stays set until reset.
    let manual = Event::manual_reset().expect("event");
    manual.set().ok();
    let (a, b) = (wait_any(&[&manual.0], 20_000), wait_any(&[&manual.0], 20_000));
    manual.reset().ok();
    let c = wait_any(&[&manual.0], 20_000);
    if a != Ok(0) || b != Ok(0) || c != Err(E_TIMEDOUT) {
        println!("[waittest] manual-reset: {:?} {:?} {:?}", a, b, c);
        return fail("manual-reset event", c);
    }
    println!("[waittest] manual-reset event stayed set for two waits and was cleared by reset");

    // 3. A port, an event and a UDP socket: the echo comes back on the socket.
    let p = port::create().expect("port");
    let idle = Event::auto_reset().expect("event");
    let udp = match Socket::udp() {
        Ok(s) => s,
        Err(e) => return fail("no UDP socket", Err(e)),
    };
    if let Err(e) = udp.connect(Endpoint::new([10, 0, 2, 2], 5555), 0) {
        return fail("UDP connect (no network?)", Err(e));
    }
    let mut got = None;
    for _ in 0..5 {
        udp.send(b"waittest").ok();
        let t0 = clock_us();
        match wait_any(&[&p, &idle.0, udp.handle()], 1_000_000) {
            Ok(2) => {
                got = Some(clock_us() - t0);
                break;
            }
            Err(E_TIMEDOUT) => continue,
            r => return fail("wait on port, event and socket returned the wrong one", r),
        }
    }
    let Some(us) = got else { return fail("the UDP echo never made the socket ready", Err(E_TIMEDOUT)) };
    let mut buf = [0u8; 64];
    match udp.recv(&mut buf, 100_000) {
        Ok(8) if &buf[..8] == b"waittest" => {}
        r => return fail("the ready socket had no echo to receive", r),
    }
    println!("[waittest] waiting on a port, an event and a UDP socket: the socket was ready after {} us", us);

    // 4. A child process: still running, then killed.
    let pid = match aero::spawn("sleeper") {
        Ok(pid) => pid,
        Err(e) => return fail("spawn sleeper", Err(e)),
    };
    let child = Child::open(pid).expect("process handle");
    let r = wait_any(&[&child.0], 50_000);
    if r != Err(E_TIMEDOUT) {
        return fail("the sleeping child counted as exited", r);
    }
    child.kill(7).ok();
    let t0 = clock_us();
    let r = wait_any(&[&p, &child.0], 1_000_000);
    let took = clock_us() - t0;
    let code = process::wait(pid);
    if r != Ok(1) || code != Ok(7) {
        println!("[waittest] child: wait {:?}, exit code {:?}", r, code);
        return fail("killing the child did not end the wait with its exit code", r);
    }
    println!("[waittest] child killed with code 7 ended the wait after {} us", took);

    println!("[waittest] events, ports, sockets and child processes in one wait: OK");
    0
}
