//! Locks that are safe to share with interrupt handlers and the scheduler.
//!
//! `IrqMutex` masks interrupts on the local CPU for as long as it is held, so
//! a holder can never be preempted or interrupted into a handler that wants
//! the same lock (which would spin forever on a single CPU).

use core::mem::ManuallyDrop;
use core::ops::{Deref, DerefMut};

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

/// A lock for long work, such as a file system update that takes many disk
/// commands. Interrupts stay on while it is held, and a thread waiting for
/// it sleeps a tick at a time instead of spinning.
pub struct SleepMutex<T> {
    inner: spin::Mutex<T>,
}

impl<T> SleepMutex<T> {
    pub const fn new(v: T) -> Self {
        Self { inner: spin::Mutex::new(v) }
    }

    pub fn lock(&self) -> spin::MutexGuard<'_, T> {
        loop {
            if let Some(g) = self.inner.try_lock() {
                return g;
            }
            crate::sched::sleep_ticks(1);
        }
    }
}
