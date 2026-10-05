//! balancetest: user threads must move between CPUs when one CPU has
//! threads waiting and another has nothing to do.
//!
//! Six threads do the same floating point work on (in QEMU) four CPUs, so
//! at least two CPUs start with two threads each. When the others finish
//! their single thread they go idle and must take the waiting ones. Each
//! thread notes every CPU it ran on; at least one must have run on more
//! than one, and every result must match the main thread's, which also
//! checks that floating point registers survive a move to another CPU.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;
use core::arch::asm;

use aero::{println, thread};

aero::entry!(main);

const WORKERS: u64 = 6;
const STEPS: u32 = 12_000_000;

/// The work: a long chain of dependent floating point operations, with the
/// CPU number sampled now and then.
fn work(seed: u64) -> (u64, u64) {
    let mut x = 1.0f64 + seed as f64 * 0.0;
    let mut cpus = 0u64;
    for i in 0..STEPS {
        x = x * 1.000_000_1 + 0.5;
        unsafe { asm!("sqrtsd {0}, {0}", inout(xmm_reg) x, options(pure, nomem, nostack)) };
        if i % 50_000 == 0 {
            cpus |= 1 << aero::cpu_id();
        }
    }
    (x.to_bits(), cpus)
}

fn main() -> i64 {
    let start = aero::uptime_ms();
    let mut handles = Vec::new();
    for n in 0..WORKERS {
        match thread::spawn(move || work(n)) {
            Ok(h) => handles.push(h),
            Err(e) => {
                println!("[balancetest] FAILED: thread::spawn: {}", e);
                return 1;
            }
        }
    }
    let results: Vec<(u64, u64)> = handles.into_iter().map(|h| h.join()).collect();
    let ms = aero::uptime_ms() - start;
    let (expected, _) = work(0);
    let moved = results.iter().filter(|r| r.1.count_ones() > 1).count();
    let all_cpus = results.iter().fold(0, |a, r| a | r.1).count_ones();
    if results.iter().any(|r| r.0 != expected) {
        println!("[balancetest] FAILED: a thread computed a different result");
        return 1;
    }
    if moved == 0 {
        println!("[balancetest] FAILED: no thread moved to an idle CPU ({} threads on {} CPUs, {} ms)", WORKERS, all_cpus, ms);
        return 1;
    }
    println!(
        "[balancetest] {} threads on {} CPUs, {} moved to an idle CPU, results agree, {} ms: OK",
        WORKERS, all_cpus, moved, ms
    );
    0
}
