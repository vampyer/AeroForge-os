//! Locks that are safe to share with interrupt handlers and the scheduler.
//!
//! `IrqMutex` masks interrupts on the local CPU for as long as it is held, so
//! a holder can never be preempted or interrupted into a handler that wants
//! the same lock (which would spin forever on a single CPU).

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use core::mem::ManuallyDrop;
use core::ops::{Deref, DerefMut};

use crate::sched::{self, Thread};
use crate::arch;

pub struct IrqMutex<T> {
    inner: spin::Mutex<T>,
}

pub struct IrqGuard<'a, T> {
    guard: ManuallyDrop<spin::MutexGuard<'a, T>>,
    restore: bool,
}

impl<T> IrqMutex<T> {
    pub const fn new(v: T) -> Self {
        Self { inner: spin::Mutex::new(v) }
    }

    pub fn lock(&self) -> IrqGuard<'_, T> {
        let restore = arch::interrupts_enabled();
        arch::disable_interrupts();
        IrqGuard { guard: ManuallyDrop::new(self.inner.lock()), restore }
    }
}

impl<T> Deref for IrqGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<T> DerefMut for IrqGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.guard
    }
}

impl<T> Drop for IrqGuard<'_, T> {
    fn drop(&mut self) {
        unsafe { ManuallyDrop::drop(&mut self.guard) };
        if self.restore {
            arch::enable_interrupts();
        }
    }
}

/// A lock for long work, such as a file system update or a disk command
/// that waits for an interrupt. Interrupts stay on while it is held, and a
/// thread that finds it taken sleeps until the holder lets go.
pub struct SleepMutex<T> {
    inner: spin::Mutex<T>,
    waiters: IrqMutex<VecDeque<Arc<Thread>>>,
}

pub struct SleepGuard<'a, T> {
    guard: ManuallyDrop<spin::MutexGuard<'a, T>>,
    mutex: &'a SleepMutex<T>,
}

impl<T> SleepMutex<T> {
    pub const fn new(v: T) -> Self {
        Self { inner: spin::Mutex::new(v), waiters: IrqMutex::new(VecDeque::new()) }
    }

    pub fn lock(&self) -> SleepGuard<'_, T> {
        loop {
            if let Some(g) = self.inner.try_lock() {
                return SleepGuard { guard: ManuallyDrop::new(g), mutex: self };
            }
            if !sched::can_block() {
                core::hint::spin_loop(); // early boot or an idle thread: spin
                continue;
            }
            arch::without_interrupts(|| {
                {
                    let mut w = self.waiters.lock();
                    // The holder unlocks before it looks at the waiters, so
                    // checking again under this lock cannot miss its wakeup.
                    if !self.inner.is_locked() {
                        return;
                    }
                    w.push_back(sched::mark_current_blocked());
                }
                sched::schedule();
                // Woken by the holder (which removed us) or by something
                // else: make sure we are not left in the queue.
                let me = sched::current();
                self.waiters.lock().retain(|t| !Arc::ptr_eq(t, &me));
            });
        }
    }
}

impl<T> Deref for SleepGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<T> DerefMut for SleepGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.guard
    }
}

impl<T> Drop for SleepGuard<'_, T> {
    fn drop(&mut self) {
        unsafe { ManuallyDrop::drop(&mut self.guard) };
        if let Some(t) = self.mutex.waiters.lock().pop_front() {
            sched::wake(&t);
        }
    }
}
