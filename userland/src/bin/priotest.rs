//! priotest: a high-priority thread must run ahead of busy normal ones.
//!
//! It times a fixed piece of floating point work three ways: alone on an
//! otherwise quiet machine; in a normal thread while eight normal threads
//! spin on the four CPUs (it then gets about a third of a CPU); and in a
//! high-priority thread against the same spinners, which should take about
//! as long as alone. It also checks the kernel refuses priorities above
//! `High` and threads of other programs.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;
use core::arch::asm;
use core::sync::atomic::{AtomicBool, Ordering};

use aero::thread::{self, Priority};
use aero::{println, sys};

aero::entry!(main);

const SPINNERS: usize = 8;
const STEPS: u32 = 4_000_000;

static STOP: AtomicBool = AtomicBool::new(false);

fn work() -> u64 {
    let mut x = 1.0f64;
    for _ in 0..STEPS {
        x = x * 1.000_000_1 + 0.5;
        unsafe { asm!("sqrtsd {0}, {0}", inout(xmm_reg) x, options(pure, nomem, nostack)) };
    }
    x.to_bits()
}

/// Milliseconds a thread at `p` takes for `work`, and its result.
fn timed(p: Priority) -> (u64, u64) {
    thread::spawn(move || {
        if thread::set_priority(p).is_err() {
            return (u64::MAX, 0);
        }
        let t0 = aero::uptime_ms();
        let r = work();
        (aero::uptime_ms() - t0, r)
    })
    .expect("spawn")
    .join()
}

fn main() -> i64 {
    // Bad requests first.
    let too_high = unsafe { aero::syscall(sys::THREAD_PRIORITY, 0, 3, 0, 0) };
    let other = unsafe { aero::syscall(sys::THREAD_PRIORITY, 1, 1, 0, 0) }; // tid 1: a kernel idle thread
    if too_high >= 0 || other >= 0 {
        println!("[priotest] FAILED: kernel accepted priority 3 ({}) or another program's thread ({})", too_high, other);
        return 1;
    }

    let (alone, expected) = timed(Priority::Normal);

    let mut spinners = Vec::new();
    for _ in 0..SPINNERS {
        spinners.push(thread::spawn(|| {
            let mut n = 0u64;
            while !STOP.load(Ordering::Relaxed) {
                n = n.wrapping_add(1);
                core::hint::spin_loop();
            }
            n
        }).expect("spawn"));
    }
    aero::sleep_ms(50); // let them spread over the CPUs

    let (normal, r1) = timed(Priority::Normal);
    let (high, r2) = timed(Priority::High);

    STOP.store(true, Ordering::Relaxed);
    for s in spinners {
        s.join();
    }

    if r1 != expected || r2 != expected {
        println!("[priotest] FAILED: wrong result under load");
        return 1;
    }
    println!("[priotest] work alone {} ms, normal thread among {} busy ones {} ms, high-priority thread {} ms", alone, SPINNERS, normal, high);
    if high.saturating_mul(2) > normal {
        println!("[priotest] FAILED: the high-priority thread was not ahead of the busy ones");
        return 1;
    }
    println!("[priotest] high priority ran ahead of {} busy threads, bad requests refused: OK", SPINNERS);
    0
}
