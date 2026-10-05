//! spreadtest: busy CPUs must even out their threads, not only idle ones.
//!
//! Twelve threads start (three per CPU in QEMU). Then, on every second CPU,
//! all but one of them end, which leaves the CPUs at one, three, one and
//! three busy threads: no CPU is ever idle, so taking work only when idle
//! would keep it that way, and the threads on the crowded CPUs would get a
//! third as much CPU time as the lone ones. Periodic balancing must move
//! them until each CPU has two. The remaining threads keep noting which CPU
//! they are on; after letting the balancing act, the main thread looks 40
//! times, 10 ms apart, and at least three looks in four must find the
//! threads evenly spread (no CPU with two more than another).
//!
//! It checks where threads run rather than how much work they get done,
//! because in QEMU one virtual CPU can run much slower than another.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use aero::{println, thread};

aero::entry!(main);

const THREADS: usize = 12;
const MAX_CPUS: usize = 64;
const LOOKS: u32 = 40;

static STARTED: AtomicU32 = AtomicU32::new(0);
static DECIDED: AtomicBool = AtomicBool::new(false);
static STOP: AtomicBool = AtomicBool::new(false);
static PER_CPU: [AtomicU32; MAX_CPUS] = [const { AtomicU32::new(0) }; MAX_CPUS];
/// Where each thread is running now; u32::MAX once it has ended.
static ON_CPU: [AtomicU32; THREADS] = [const { AtomicU32::new(u32::MAX) }; THREADS];

fn worker(n: usize) {
    let cpu = aero::cpu_id() as usize % MAX_CPUS;
    let rank = PER_CPU[cpu].fetch_add(1, Ordering::SeqCst);
    STARTED.fetch_add(1, Ordering::SeqCst);
    while !DECIDED.load(Ordering::SeqCst) {
        core::hint::spin_loop();
    }
    if cpu % 2 == 0 && rank > 0 {
        return;
    }
    while !STOP.load(Ordering::Relaxed) {
        ON_CPU[n].store(aero::cpu_id() as u32, Ordering::Relaxed);
        for _ in 0..1000 {
            core::hint::spin_loop();
        }
    }
    ON_CPU[n].store(u32::MAX, Ordering::Relaxed);
}

fn main() -> i64 {
    let mut handles = Vec::new();
    for n in 0..THREADS {
        match thread::spawn(move || worker(n)) {
            Ok(h) => handles.push(h),
            Err(e) => {
                println!("[spreadtest] FAILED: thread::spawn: {}", e);
                return 1;
            }
        }
    }
    while STARTED.load(Ordering::SeqCst) < THREADS as u32 {
        aero::sleep_ms(1);
    }
    let start: Vec<u32> = PER_CPU.iter().map(|c| c.load(Ordering::SeqCst)).take_while(|&n| n > 0).collect();
    let cpus = start.len();
    DECIDED.store(true, Ordering::SeqCst);
    aero::sleep_ms(200); // the uneven spread forms, and balancing gets to act

    let mut even = 0;
    let mut last = Vec::new();
    for _ in 0..LOOKS {
        aero::sleep_ms(10);
        let mut per = alloc::vec![0u32; cpus];
        for c in ON_CPU.iter() {
            let c = c.load(Ordering::Relaxed) as usize;
            if c < cpus {
                per[c] += 1;
            }
        }
        if per.iter().max().unwrap_or(&0) - per.iter().min().unwrap_or(&0) <= 1 {
            even += 1;
        }
        last = per;
    }
    STOP.store(true, Ordering::SeqCst);
    for h in handles {
        h.join();
    }

    println!(
        "[spreadtest] threads per CPU at start {:?}, then 1 left on every second CPU; evenly spread in {} of {} looks (last {:?})",
        start, even, LOOKS, last
    );
    if cpus < 2 || even * 4 < LOOKS * 3 {
        println!("[spreadtest] FAILED: busy CPUs did not even out");
        return 1;
    }
    println!("[spreadtest] busy CPUs evened out their threads: OK");
    0
}
