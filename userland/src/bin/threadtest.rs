//! threadtest: threads, the heap, locks and waiting for a child program.
//!
//! - Vec and String on the heap, and an 8 MiB buffer straight from the
//!   kernel, freed while other threads are running (a TLB shootdown).
//! - Four threads add to one counter under a futex Mutex and each returns
//!   a result through `join`.
//! - Two threads hand a token back and forth through a futex.
//! - It starts `nxtest` (which the kernel kills for running code on its
//!   stack) and waits for its exit code.
//! - Last, it leaves one thread spinning, one asleep for an hour and one
//!   blocked on a futex forever, and exits: the kernel must end all three.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use aero::sync::Mutex;
use aero::{futex, println, process, thread};

aero::entry!(main);

const WORKERS: u64 = 4;
const ADDS: u64 = 20_000;
const PING_PONGS: u32 = 500;

static COUNTER: Mutex<u64> = Mutex::new(0);
static TURN: AtomicU32 = AtomicU32::new(0);
static STOP: AtomicBool = AtomicBool::new(false);
static FOREVER: AtomicU32 = AtomicU32::new(0);
static CPUS_SEEN: AtomicU64 = AtomicU64::new(0);

fn fail(what: &str) -> i64 {
    println!("[threadtest] FAILED: {}", what);
    1
}

/// Waits until TURN == me, then passes it to the other side.
fn ping_pong(me: u32) -> u32 {
    for _ in 0..PING_PONGS {
        loop {
            let t = TURN.load(Ordering::Acquire);
            if t % 2 == me {
                break;
            }
            futex::wait(&TURN, t);
        }
        TURN.fetch_add(1, Ordering::AcqRel);
        futex::wake(&TURN, 1);
    }
    me
}

fn main() -> i64 {
    // The heap.
    let squares: Vec<u64> = (0..100_000u64).map(|i| i * i).collect();
    let sum: u64 = squares.iter().sum();
    let mut text = String::new();
    for w in ["threads", "share", "one", "heap"] {
        text.push_str(w);
        text.push(' ');
    }
    if sum != 333_328_333_350_000 || text.trim_end() != "threads share one heap" {
        return fail("heap data wrong");
    }
    drop(squares);

    // Counter under a Mutex, from four threads on (probably) several CPUs.
    let mut handles = Vec::new();
    for n in 0..WORKERS {
        let h = thread::spawn(move || {
            CPUS_SEEN.fetch_or(1 << aero::cpu_id(), Ordering::Relaxed);
            let mut local = Vec::new();
            for i in 0..ADDS {
                *COUNTER.lock() += 1;
                if i % 1000 == 0 {
                    local.push(i);
                }
            }
            n * 1000 + local.len() as u64
        });
        match h {
            Ok(h) => handles.push(h),
            Err(e) => return fail(alloc::format!("thread::spawn: {}", e).as_str()),
        }
    }
    // Free a big buffer while the workers run on other CPUs.
    let mut big = alloc::vec![0u8; 8 << 20];
    big[0] = 1;
    big[(8 << 20) - 1] = 2;
    let ends = big[0] as u32 + big[(8 << 20) - 1] as u32;
    drop(big);
    let returned: Vec<u64> = handles.into_iter().map(|h| h.join()).collect();
    let total = *COUNTER.lock();
    let expected: Vec<u64> = (0..WORKERS).map(|n| n * 1000 + ADDS / 1000).collect();
    if total != WORKERS * ADDS || returned != expected || ends != 3 {
        println!("[threadtest] counter {} returned {:?}", total, returned);
        return fail("lost updates or wrong results");
    }
    let cpus = CPUS_SEEN.load(Ordering::Relaxed).count_ones();

    // Futex ping-pong.
    let a = thread::spawn(|| ping_pong(0)).expect("spawn");
    let b = thread::spawn(|| ping_pong(1)).expect("spawn");
    if a.join() + b.join() != 1 || TURN.load(Ordering::Acquire) != 2 * PING_PONGS {
        return fail("ping-pong");
    }

    // A child program and its exit code.
    let child = match aero::spawn("nxtest") {
        Ok(pid) => pid,
        Err(e) => return fail(alloc::format!("spawn nxtest: {}", e).as_str()),
    };
    let code = match process::wait(child) {
        Ok(c) => c,
        Err(e) => return fail(alloc::format!("wait: {}", e).as_str()),
    };
    if code != -1014 {
        return fail(alloc::format!("nxtest exit code {}", code).as_str());
    }
    if process::wait(child).is_ok() {
        return fail("waited for the same child twice");
    }

    println!(
        "[threadtest] {} threads on {} CPUs added {} under one lock, {} futex hand-offs, heap and 8 MiB buffer OK, child nxtest exited with {}: OK",
        WORKERS, cpus, total, 2 * PING_PONGS, code
    );

    // Threads the kernel has to clean up when we exit.
    let _ = thread::spawn(|| {
        while !STOP.load(Ordering::Relaxed) {
            core::hint::spin_loop();
        }
    });
    let _ = thread::spawn(|| aero::sleep_ms(3_600_000));
    let _ = thread::spawn(|| loop {
        futex::wait(&FOREVER, 0);
    });
    aero::sleep_ms(50);
    println!("[threadtest] exiting with 3 threads still running");
    0
}
