//! Waiting on several kernel objects at once (SYS_WAIT_ANY), and event
//! objects.
//!
//! Anything a waiter could be waiting for (an event set, a message sent to
//! a port, a network poll, a process exiting) calls `notify`, which wakes
//! every thread in `wait_any`; each checks its own handles again. That is
//! simple and cannot miss a wakeup: a waiter registers before it checks.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::sched::{self, Thread};
use crate::sync::IrqMutex;
use crate::{apic, arch};

static WAITERS: IrqMutex<Vec<Arc<Thread>>> = IrqMutex::new(Vec::new());

/// Something changed that a `wait_any` caller may be waiting for.
pub fn notify() {
    let waiters = core::mem::take(&mut *WAITERS.lock());
    for t in &waiters {
        sched::wake_waiter(t);
    }
}

pub enum WaitError {
    TimedOut,
    /// The caller's process is exiting.
    Killed,
}

/// Waits until `ready` returns Some (it is called again after each
/// `notify`) or `deadline` (`apic::micros` time) passes.
pub fn wait_any<T>(deadline: Option<u64>, mut ready: impl FnMut() -> Option<T>) -> Result<T, WaitError> {
    arch::without_interrupts(|| loop {
        let me = {
            let mut waiters = WAITERS.lock();
            let me = match deadline {
                Some(d) => sched::mark_current_sleeping(d),
                None => sched::mark_current_blocked(),
            };
            waiters.push(me.clone());
            me
        };
        // Registered first: a change after this check wakes us.
        let r = ready();
        let expired = deadline.is_some_and(|d| apic::micros() >= d);
        if r.is_some() || expired || sched::killed() {
            sched::unblock_current();
            WAITERS.lock().retain(|t| !Arc::ptr_eq(t, &me));
            return match r {
                Some(v) => Ok(v),
                None if expired => Err(WaitError::TimedOut),
                None => Err(WaitError::Killed),
            };
        }
        sched::schedule();
        WAITERS.lock().retain(|t| !Arc::ptr_eq(t, &me));
    })
}

/// An event object: set by one thread, waited for by others. An
/// auto-reset event lets one `wait_any` through per `set`; a manual-reset
/// one stays set until `reset`.
pub struct Event {
    signaled: AtomicBool,
    manual: bool,
}

impl Event {
    pub fn new(manual: bool) -> Arc<Event> {
        Arc::new(Event { signaled: AtomicBool::new(false), manual })
    }

    pub fn set(&self) {
        self.signaled.store(true, Ordering::SeqCst);
        notify();
    }

    pub fn reset(&self) {
        self.signaled.store(false, Ordering::SeqCst);
    }

    /// Is it set? An auto-reset event is reset by the caller that sees it.
    pub fn take(&self) -> bool {
        if self.manual {
            self.signaled.load(Ordering::SeqCst)
        } else {
            self.signaled.swap(false, Ordering::SeqCst)
        }
    }
}
