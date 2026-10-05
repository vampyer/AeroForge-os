//! Memory, threads and locks for programs: `mem` (pages from the kernel),
//! the heap behind `alloc` (Box, Vec, String), `futex`, `sync::Mutex`,
//! `thread` and `process::wait`.

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicU32, Ordering};

use crate::{check, sys, syscall};

pub mod mem {
    use super::*;

    pub const PAGE: usize = 4096;

    /// Maps `len` bytes (rounded up to whole pages) of zeroed memory.
    pub fn map(len: usize) -> Result<*mut u8, i64> {
        check(unsafe { syscall(sys::MEM_MAP, len as u64, 0, 0, 0) }).map(|a| a as *mut u8)
    }

    /// Gives back a whole region from `map`.
    pub fn unmap(addr: *mut u8) -> Result<(), i64> {
        check(unsafe { syscall(sys::MEM_UNMAP, addr as u64, 0, 0, 0) }).map(|_| ())
    }
}

pub mod futex {
    use super::*;

    /// Sleeps while `word` holds `expected` (returns at once if it does not).
    /// Wakeups can be spurious: check the word again.
    pub fn wait(word: &AtomicU32, expected: u32) {
        unsafe { syscall(sys::FUTEX_WAIT, word as *const AtomicU32 as u64, expected as u64, 0, 0) };
    }

    /// Wakes up to `count` threads sleeping on `word`; returns how many woke.
    pub fn wake(word: &AtomicU32, count: u32) -> u64 {
        unsafe { syscall(sys::FUTEX_WAKE, word as *const AtomicU32 as u64, count as u64, 0, 0) as u64 }
    }
}

pub mod sync {
    use super::*;

    /// A lock that sleeps in the kernel only when there is a wait.
    /// State: 0 free, 1 held, 2 held with (possible) sleepers.
    pub struct Mutex<T> {
        state: AtomicU32,
        data: UnsafeCell<T>,
    }

    unsafe impl<T: Send> Send for Mutex<T> {}
    unsafe impl<T: Send> Sync for Mutex<T> {}

    impl<T> Mutex<T> {
        pub const fn new(v: T) -> Self {
            Self { state: AtomicU32::new(0), data: UnsafeCell::new(v) }
        }

        pub fn lock(&self) -> MutexGuard<'_, T> {
            if self.state.compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed).is_err() {
                while self.state.swap(2, Ordering::Acquire) != 0 {
                    futex::wait(&self.state, 2);
                }
            }
            MutexGuard { m: self }
        }
    }

    pub struct MutexGuard<'a, T> {
        m: &'a Mutex<T>,
    }

    impl<T> Deref for MutexGuard<'_, T> {
        type Target = T;
        fn deref(&self) -> &T {
            unsafe { &*self.m.data.get() }
        }
    }

    impl<T> DerefMut for MutexGuard<'_, T> {
        fn deref_mut(&mut self) -> &mut T {
            unsafe { &mut *self.m.data.get() }
        }
    }

    impl<T> Drop for MutexGuard<'_, T> {
        fn drop(&mut self) {
            if self.m.state.swap(0, Ordering::Release) == 2 {
                futex::wake(&self.m.state, 1);
            }
        }
    }
}

pub mod thread {
    use super::*;
    use sync::Mutex;

    /// Each thread's stack (a guard page below it comes from the kernel).
    pub const STACK_SIZE: usize = 256 * 1024;

    pub struct JoinHandle<T> {
        tid: u64,
        stack: *mut u8,
        result: Arc<Mutex<Option<T>>>,
    }

    unsafe impl<T: Send> Send for JoinHandle<T> {}

    extern "C" fn start(arg: u64) -> ! {
        let f = unsafe { Box::from_raw(arg as *mut Box<dyn FnOnce()>) };
        f();
        exit()
    }

    /// Starts `f` on a new thread of this program. It may run on another CPU.
    pub fn spawn<F, T>(f: F) -> Result<JoinHandle<T>, i64>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let result = Arc::new(Mutex::new(None));
        let slot = result.clone();
        let main: Box<dyn FnOnce()> = Box::new(move || {
            let r = f();
            *slot.lock() = Some(r);
        });
        let arg = Box::into_raw(Box::new(main));
        let stack = match mem::map(STACK_SIZE) {
            Ok(s) => s,
            Err(e) => {
                drop(unsafe { Box::from_raw(arg) });
                return Err(e);
            }
        };
        let top = stack as u64 + STACK_SIZE as u64;
        match check(unsafe { syscall(sys::THREAD_CREATE, start as *const () as u64, top, arg as u64, 0) }) {
            Ok(tid) => Ok(JoinHandle { tid, stack, result }),
            Err(e) => {
                drop(unsafe { Box::from_raw(arg) });
                let _ = mem::unmap(stack);
                Err(e)
            }
        }
    }

    impl<T> JoinHandle<T> {
        pub fn id(&self) -> u64 {
            self.tid
        }

        /// Waits for the thread to finish and returns what it returned.
        pub fn join(self) -> T {
            unsafe { syscall(sys::THREAD_JOIN, self.tid, 0, 0, 0) };
            let _ = mem::unmap(self.stack);
            let r = self.result.lock().take();
            r.expect("thread ended without a result")
        }
    }

    /// Ends the calling thread only (`aero::exit` ends the whole program).
    pub fn exit() -> ! {
        unsafe { syscall(sys::THREAD_EXIT, 0, 0, 0, 0) };
        loop {}
    }

    pub fn id() -> u64 {
        unsafe { syscall(sys::THREAD_ID, 0, 0, 0, 0) as u64 }
    }
}

pub mod process {
    use super::*;

    /// Waits for child process `pid` (started with `aero::spawn`) to exit and
    /// returns its exit code (killed programs get -1000 minus the CPU
    /// exception number, e.g. -1014 for a page fault).
    pub fn wait(pid: u64) -> Result<i64, i64> {
        let mut code = 0i64;
        check(unsafe { syscall(sys::PROCESS_WAIT, pid, &mut code as *mut i64 as u64, 0, 0) })?;
        Ok(code)
    }
}

/// The heap: blocks of 16 to 2048 bytes come from per-size free lists filled
/// 64 KiB at a time; anything bigger gets its own pages from the kernel and
/// goes straight back when freed.
struct Heap {
    free: sync::Mutex<[usize; CLASSES]>,
}

const CLASSES: usize = 8;
const SMALLEST: usize = 16;
const CHUNK: usize = 64 * 1024;

fn class_of(size: usize) -> Option<usize> {
    let size = size.max(SMALLEST).next_power_of_two();
    let c = size.trailing_zeros() as usize - SMALLEST.trailing_zeros() as usize;
    (c < CLASSES).then_some(c)
}

unsafe impl GlobalAlloc for Heap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let size = layout.size().max(layout.align());
        match class_of(size) {
            Some(c) => {
                let mut free = self.free.lock();
                if free[c] == 0 {
                    // Carve a fresh chunk into blocks, each aligned to its size.
                    let Ok(chunk) = mem::map(CHUNK) else { return core::ptr::null_mut() };
                    let block = SMALLEST << c;
                    for off in (0..CHUNK).step_by(block).rev() {
                        let b = chunk as usize + off;
                        *(b as *mut usize) = free[c];
                        free[c] = b;
                    }
                }
                let b = free[c];
                free[c] = *(b as *const usize);
                b as *mut u8
            }
            None if layout.align() <= mem::PAGE => mem::map(size).unwrap_or(core::ptr::null_mut()),
            None => core::ptr::null_mut(),
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        match class_of(layout.size().max(layout.align())) {
            Some(c) => {
                let mut free = self.free.lock();
                *(ptr as *mut usize) = free[c];
                free[c] = ptr as usize;
            }
            None => {
                let _ = mem::unmap(ptr);
            }
        }
    }
}

#[global_allocator]
static HEAP: Heap = Heap { free: sync::Mutex::new([0; CLASSES]) };
