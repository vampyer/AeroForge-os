//! IPC ports: message queues that processes reach through handles.
//!
//! A message is up to 256 bytes plus, optionally, one handle moved from the
//! sender to the receiver. Moving a handle to your own reply port is how a
//! client gives a server the right to answer it, and nothing else.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::process::Handle;
use crate::sched::{self, Thread};
use crate::sync::IrqMutex;
use crate::arch;

pub const MAX_MESSAGE: usize = 256;
const MAX_QUEUED: usize = 64;

pub struct Message {
    pub data: Vec<u8>,
    pub sender: u64,
    pub handle: Option<Handle>,
}

struct Inner {
    queue: VecDeque<Message>,
    waiters: VecDeque<Arc<Thread>>,
}

pub struct Port {
    pub id: u64,
    inner: IrqMutex<Inner>,
    pub sent: AtomicU64,
}

static NEXT_PORT: AtomicU64 = AtomicU64::new(1);
/// The name service: well-known ports by name ("echo", ...).
pub static NAMES: IrqMutex<BTreeMap<String, Arc<Port>>> = IrqMutex::new(BTreeMap::new());

impl Port {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            id: NEXT_PORT.fetch_add(1, Ordering::SeqCst),
            inner: IrqMutex::new(Inner { queue: VecDeque::new(), waiters: VecDeque::new() }),
            sent: AtomicU64::new(0),
        })
    }

    pub fn send(&self, msg: Message) -> Result<(), Message> {
        let mut g = self.inner.lock();
        if g.queue.len() >= MAX_QUEUED {
            return Err(msg);
        }
        g.queue.push_back(msg);
        self.sent.fetch_add(1, Ordering::Relaxed);
        if let Some(t) = g.waiters.pop_front() {
            sched::wake(&t);
        }
        Ok(())
    }

    /// Blocks until a message arrives. None if the caller's process is being
    /// killed (the system call return path then ends the thread).
    pub fn recv(&self) -> Option<Message> {
        arch::without_interrupts(|| loop {
            {
                let mut g = self.inner.lock();
                if let Some(m) = g.queue.pop_front() {
                    return Some(m);
                }
                let me = sched::mark_current_blocked();
                g.waiters.push_back(me);
            }
            if sched::killed() {
                sched::unblock_current();
                let me = sched::current();
                self.inner.lock().waiters.retain(|t| !Arc::ptr_eq(t, &me));
                return None;
            }
            sched::schedule();
        })
    }

    pub fn queued(&self) -> usize {
        self.inner.lock().queue.len()
    }
}
