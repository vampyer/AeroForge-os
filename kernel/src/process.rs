//! Processes: an address space plus a handle table. Handles are the only way
//! a process can reach a kernel object, and each carries rights, so handles
//! are capabilities (design doc section 1.7).

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};

use crate::ipc::Port;
use crate::sync::IrqMutex;
use crate::{console, elf, memory, modules, sched};

pub mod rights {
    pub const SEND: u32 = 1 << 0;
    pub const RECV: u32 = 1 << 1;
    pub const ALL: u32 = SEND | RECV;
}

#[derive(Clone)]
pub enum Object {
    Port(Arc<Port>),
}

#[derive(Clone)]
pub struct Handle {
    pub object: Object,
    pub rights: u32,
}

#[derive(Default)]
pub struct HandleTable {
    slots: Vec<Option<Handle>>,
}

impl HandleTable {
    pub fn insert(&mut self, h: Handle) -> u64 {
        if let Some(i) = self.slots.iter().position(|s| s.is_none()) {
            self.slots[i] = Some(h);
            return i as u64;
        }
        self.slots.push(Some(h));
        (self.slots.len() - 1) as u64
    }

    pub fn get(&self, id: u64) -> Option<&Handle> {
        self.slots.get(id as usize)?.as_ref()
    }

    pub fn take(&mut self, id: u64) -> Option<Handle> {
        self.slots.get_mut(id as usize)?.take()
    }

    pub fn count(&self) -> usize {
        self.slots.iter().filter(|s| s.is_some()).count()
    }
}

pub struct Process {
    pub pid: u64,
    pub name: String,
    pub pml4: u64,
    pub handles: IrqMutex<HandleTable>,
    pub threads: AtomicUsize,
    pub exit_code: AtomicI64,
    #[allow(dead_code)] // for wait()/process trees
    pub parent: u64,
}

impl Drop for Process {
    fn drop(&mut self) {
        memory::destroy_address_space(self.pml4);
    }
}

pub static PROCESSES: IrqMutex<BTreeMap<u64, Arc<Process>>> = IrqMutex::new(BTreeMap::new());
static NEXT_PID: AtomicU64 = AtomicU64::new(1);

pub const USER_STACK_TOP: u64 = 0x0000_7FFF_FFFF_0000;
pub const USER_STACK_SIZE: u64 = 64 * 1024;

/// Loads a boot module as a new process and starts its main thread.
pub fn spawn(module_name: &str, parent: u64) -> Result<u64, &'static str> {
    let image = modules::find(module_name).ok_or("no such program")?;
    let pml4 = memory::new_address_space().ok_or("out of memory")?;
    let process = Arc::new(Process {
        pid: NEXT_PID.fetch_add(1, Ordering::SeqCst),
        name: String::from(module_name),
        pml4,
        handles: IrqMutex::new(HandleTable::default()),
        threads: AtomicUsize::new(0),
        exit_code: AtomicI64::new(0),
        parent,
    });
    // From here on, dropping `process` frees the address space on failure.
    let entry = elf::load(image, pml4)?;
    let mut v = USER_STACK_TOP - USER_STACK_SIZE;
    while v < USER_STACK_TOP {
        let f = memory::alloc_frame_zeroed().ok_or("out of memory")?;
        memory::map_page(pml4, v, f, memory::flags::USER | memory::flags::WRITABLE)?;
        v += memory::PAGE_SIZE;
    }
    PROCESSES.lock().insert(process.pid, process.clone());
    let pid = process.pid;
    sched::spawn_user(process, module_name, entry, USER_STACK_TOP - 8);
    Ok(pid)
}

/// The last thread of `p` has exited.
pub fn on_exit(p: &Arc<Process>) {
    PROCESSES.lock().remove(&p.pid);
    let code = p.exit_code.load(Ordering::SeqCst);
    if code != 0 {
        console::print_colored(console::YELLOW, format_args!("[kernel] {} (pid {}) exited with code {}\n", p.name, p.pid, code));
    }
}
