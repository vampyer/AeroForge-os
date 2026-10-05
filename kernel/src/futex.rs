//! Futexes: the kernel half of user-space locks. A program keeps its lock
//! word in its own memory and only calls the kernel to sleep while the word
//! holds an expected value, or to wake sleepers after changing it.
//! Queues are keyed by (process, address), so they are private to a process.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::Arc;

use crate::sched::{self, Thread};
use crate::sync::IrqMutex;
use crate::{arch, security};

static QUEUES: IrqMutex<BTreeMap<(u64, u64), VecDeque<Arc<Thread>>>> = IrqMutex::new(BTreeMap::new());

pub enum WaitError {
    /// The word no longer holds the expected value: try again.
    Again,
    /// Not a readable, 4-byte aligned user address.
    Fault,
    /// The deadline passed before a `wake`.
    TimedOut,
}

/// Sleeps until woken, if the u32 at `addr` still equals `expected`. The
/// check and the sleep are one step as far as `wake` can tell. Wakeups can
/// be spurious, so callers re-check their lock word. With a deadline
/// (`apic::micros` time) the wait also ends then, with `TimedOut`.
pub fn wait(pid: u64, addr: u64, expected: u32, deadline: Option<u64>) -> Result<(), WaitError> {
    if addr % 4 != 0 {
        return Err(WaitError::Fault);
    }
    let key = (pid, addr);
    arch::without_interrupts(|| {
        let me = {
            let mut queues = QUEUES.lock();
            let word = security::copy_from_user(addr, 4).ok_or(WaitError::Fault)?;
            if u32::from_le_bytes([word[0], word[1], word[2], word[3]]) != expected {
                return Err(WaitError::Again);
            }
            let me = match deadline {
                Some(d) => sched::mark_current_sleeping(d),
                None => sched::mark_current_blocked(),
            };
            queues.entry(key).or_default().push_back(me.clone());
            me
        };
        if sched::killed() {
            // Our stale queue entry goes with the process (forget_process).
            sched::unblock_current();
            return Ok(());
        }
        sched::schedule();
        if deadline.is_some() {
            // Still queued means no `wake` picked us: the deadline passed.
            let mut queues = QUEUES.lock();
            if let Some(q) = queues.get_mut(&key) {
                if let Some(i) = q.iter().position(|t| Arc::ptr_eq(t, &me)) {
                    q.remove(i);
                    if q.is_empty() {
                        queues.remove(&key);
                    }
                    return Err(WaitError::TimedOut);
                }
            }
        }
        Ok(())
    })
}

/// Wakes up to `count` threads sleeping on `addr`; returns how many.
pub fn wake(pid: u64, addr: u64, count: u64) -> u64 {
    let mut queues = QUEUES.lock();
    let Some(q) = queues.get_mut(&(pid, addr)) else { return 0 };
    let mut woken = 0;
    while woken < count {
        let Some(t) = q.pop_front() else { break };
        sched::wake_waiter(&t);
        woken += 1;
    }
    if q.is_empty() {
        queues.remove(&(pid, addr));
    }
    woken
}

/// Drops the queues of a process that has exited.
pub fn forget_process(pid: u64) {
    QUEUES.lock().retain(|&(p, _), _| p != pid);
}
