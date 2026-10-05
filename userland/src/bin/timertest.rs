//! timertest: sleeps and futex timeouts must be precise to well under a
//! 10 ms scheduler tick.
//!
//! It checks four things: twenty 2 ms sleeps each take at least 2 ms and
//! typically not much more; a game-style loop that sleeps until each 144 Hz
//! frame deadline keeps its pace; a futex wait with a 5 ms timeout gives up
//! after about 5 ms; and a futex wait with a long timeout still wakes as
//! soon as another thread wakes it.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

use aero::futex;
use aero::thread;
use aero::{clock_us, println};

aero::entry!(main);

const SLEEP_US: u64 = 2000;
const FRAME_US: u64 = 1_000_000 / 144;
const FRAMES: u64 = 72;

static WORD: AtomicU32 = AtomicU32::new(0);

fn main() -> i64 {
    // 1. Short sleeps.
    let mut took: Vec<u64> = (0..20)
        .map(|_| {
            let t0 = clock_us();
            aero::sleep_us(SLEEP_US);
            clock_us() - t0
        })
        .collect();
    took.sort_unstable();
    let (min, median) = (took[0], took[took.len() / 2]);
    println!("[timertest] 2 ms sleeps took {}..{} us, median {} us", min, took[took.len() - 1], median);
    if min < SLEEP_US || median > 2 * SLEEP_US {
        println!("[timertest] FAILED: sleeps are not precise");
        return 1;
    }

    // 2. Frame pacing: sleep until each frame's deadline.
    let start = clock_us();
    let mut late = 0;
    for f in 1..=FRAMES {
        let deadline = start + f * FRAME_US;
        let now = clock_us();
        if deadline > now {
            aero::sleep_us(deadline - now);
        }
        if clock_us() > deadline + 2000 {
            late += 1;
        }
    }
    let total = clock_us() - start;
    let want = FRAMES * FRAME_US;
    println!("[timertest] {} frames at 144 Hz took {} us (want {}), {} woke over 2 ms late", FRAMES, total, want, late);
    if total < want || total > want + want / 20 || late > FRAMES / 10 {
        println!("[timertest] FAILED: frame pacing is off");
        return 1;
    }

    // 3. A futex wait nobody ends must time out.
    let t0 = clock_us();
    let woken = futex::wait_timeout(&WORD, 0, 5000);
    let waited = clock_us() - t0;
    println!("[timertest] futex wait with a 5 ms timeout returned after {} us", waited);
    if woken || waited < 5000 || waited > 50_000 {
        println!("[timertest] FAILED: futex timeout (woken={})", woken);
        return 1;
    }

    // 4. A futex wait with a long timeout must still end at the wake.
    let waker = thread::spawn(|| {
        aero::sleep_us(3000);
        WORD.store(1, Ordering::SeqCst);
        futex::wake(&WORD, 1);
    })
    .expect("spawn");
    let t0 = clock_us();
    let mut timed_out = false;
    while WORD.load(Ordering::SeqCst) == 0 && !timed_out {
        timed_out = !futex::wait_timeout(&WORD, 0, 1_000_000);
    }
    let waited = clock_us() - t0;
    waker.join();
    println!("[timertest] futex wait with a 1 s timeout was woken after {} us", waited);
    if timed_out || waited > 500_000 {
        println!("[timertest] FAILED: woken wait (timed out={})", timed_out);
        return 1;
    }

    println!("[timertest] sleeps precise, 144 Hz pacing kept, futex timeouts work: OK");
    0
}
