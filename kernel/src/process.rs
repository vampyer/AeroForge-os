//! Processes: an address space plus a handle table. Handles are the only way
//! a process can reach a kernel object, and each carries rights, so handles
//! are capabilities (design doc section 1.7).

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};

use crate::ipc::Port;
use crate::sync::IrqMutex;
use crate::{arch, console, elf, futex, memory, modules, sched, tlb};

pub mod rights {
    pub const SEND: u32 = 1 << 0;
    pub const RECV: u32 = 1 << 1;
    pub const ALL: u32 = SEND | RECV;
}

#[derive(Clone)]
pub enum Object {
    Port(Arc<Port>),
    Socket(Arc<crate::net::UserSocket>),
    Event(Arc<crate::wait::Event>),
    /// A child process: waitable until it exits, and killable.
    Process(Arc<Exit>),
}

/// What outlives a process for those holding a handle to it (a handle
/// does not keep the process itself, and so its memory, alive).
pub struct Exit {
    pub pid: u64,
    pub exited: AtomicBool,
    pub code: AtomicI64,
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
    /// Set once the process is exiting: its threads may only exit now.
    pub exiting: AtomicBool,
    pub parent: u64,
    pub exit: Arc<Exit>,
    /// Memory handed out by SYS_MEM_MAP.
    vm: IrqMutex<Vm>,
}

/// A process's SYS_MEM_MAP regions: start address -> length in bytes.
/// Addresses are handed out upwards from `HEAP_BASE` and never reused, with
/// an unmapped guard page after each region (so a thread stack that
/// overflows downwards hits the guard of the region below it).
struct Vm {
    next: u64,
    regions: BTreeMap<u64, u64>,
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
/// Where SYS_MEM_MAP regions start (16 TiB, far from the program and stack).
const HEAP_BASE: u64 = 0x0000_1000_0000_0000;
/// Largest single SYS_MEM_MAP.
pub const MAX_MAP: u64 = 1 << 30;

/// Loads a boot module as a new process and starts its main thread.
pub fn spawn(module_name: &str, parent: u64) -> Result<u64, &'static str> {
    let image = modules::find(module_name).ok_or("no such program")?;
    let pml4 = memory::new_address_space().ok_or("out of memory")?;
    let pid = NEXT_PID.fetch_add(1, Ordering::SeqCst);
    let process = Arc::new(Process {
        pid,
        name: String::from(module_name),
        pml4,
        handles: IrqMutex::new(HandleTable::default()),
        threads: AtomicUsize::new(0),
        exit_code: AtomicI64::new(0),
        exiting: AtomicBool::new(false),
        parent,
        exit: Arc::new(Exit { pid, exited: AtomicBool::new(false), code: AtomicI64::new(0) }),
        vm: IrqMutex::new(Vm { next: HEAP_BASE, regions: BTreeMap::new() }),
    });
    // From here on, dropping `process` frees the address space on failure.
    let entry = elf::load(image, pml4)?;
    let mut v = USER_STACK_TOP - USER_STACK_SIZE;
    while v < USER_STACK_TOP {
        let f = memory::alloc_frame_zeroed().ok_or("out of memory")?;
        // The stack is data: never executable.
        let nx = if crate::security::nx_enabled() { memory::flags::NO_EXECUTE } else { 0 };
        memory::map_page(pml4, v, f, memory::flags::USER | memory::flags::WRITABLE | nx)?;
        v += memory::PAGE_SIZE;
    }
    PROCESSES.lock().insert(process.pid, process.clone());
    sched::spawn_user(process, module_name, entry, USER_STACK_TOP - 8, 0);
    Ok(pid)
}

/// Starts process `p` exiting with `code` (the first code wins): every
/// thread stops at its next chance, and the last one out cleans up.
pub fn kill(p: &Process, code: i64) {
    if !p.exiting.swap(true, Ordering::SeqCst) {
        p.exit_code.store(code, Ordering::SeqCst);
    }
    sched::kill_threads_of(p.pid);
}

/// Maps `len` bytes (rounded up to pages) of zeroed, writable, non-executable
/// memory into `p` and returns its address.
pub fn map_memory(p: &Process, len: u64) -> Result<u64, &'static str> {
    if len == 0 || len > MAX_MAP {
        return Err("bad length");
    }
    let len = (len + memory::PAGE_SIZE - 1) & !(memory::PAGE_SIZE - 1);
    let nx = if crate::security::nx_enabled() { memory::flags::NO_EXECUTE } else { 0 };
    let mut vm = p.vm.lock();
    let start = vm.next;
    let mut v = start;
    while v < start + len {
        let mapped = memory::alloc_frame_zeroed()
            .ok_or("out of memory")
            .and_then(|f| memory::map_page(p.pml4, v, f, memory::flags::USER | memory::flags::WRITABLE | nx).map_err(|e| {
                memory::free_frame(f);
                e
            }));
        if let Err(e) = mapped {
            // Undo. Another thread may have touched the pages already, so
            // other CPUs drop them from their TLBs before the frames are reused.
            let frames: Vec<u64> = (start..v).step_by(memory::PAGE_SIZE as usize)
                .filter_map(|a| memory::unmap_page(p.pml4, a)).collect();
            drop(vm);
            tlb::shootdown();
            frames.into_iter().for_each(memory::free_frame);
            return Err(e);
        }
        v += memory::PAGE_SIZE;
    }
    vm.next = start + len + memory::PAGE_SIZE;
    vm.regions.insert(start, len);
    Ok(start)
}

/// Unmaps a whole region returned by `map_memory`.
pub fn unmap_memory(p: &Process, addr: u64) -> Result<(), &'static str> {
    let frames: Vec<u64> = {
        let mut vm = p.vm.lock();
        let len = vm.regions.remove(&addr).ok_or("not a mapped region")?;
        (addr..addr + len).step_by(memory::PAGE_SIZE as usize).filter_map(|a| memory::unmap_page(p.pml4, a)).collect()
    };
    // This CPU's TLB is already clean (unmap_page uses invlpg); the process's
    // other threads may be running elsewhere with the old entries.
    if p.threads.load(Ordering::SeqCst) > 1 {
        tlb::shootdown();
    }
    frames.into_iter().for_each(memory::free_frame);
    Ok(())
}

/// Exit codes of processes nobody has waited for yet, and who is waiting.
struct Children {
    exited: BTreeMap<u64, (u64, i64)>, // pid -> (parent, exit code)
    waiters: BTreeMap<u64, Vec<Arc<sched::Thread>>>,
}

static CHILDREN: IrqMutex<Children> = IrqMutex::new(Children { exited: BTreeMap::new(), waiters: BTreeMap::new() });

/// Waits for child `pid` of process `parent` to exit and returns its exit code.
pub fn wait(parent: u64, pid: u64) -> Result<i64, &'static str> {
    arch::without_interrupts(|| loop {
        {
            let mut c = CHILDREN.lock();
            if let Some(&(par, code)) = c.exited.get(&pid) {
                if par != parent {
                    return Err("not a child");
                }
                c.exited.remove(&pid);
                return Ok(code);
            }
            // on_exit records the code before it leaves PROCESSES, so a child
            // missing from both is no child (or was waited for already).
            match PROCESSES.lock().get(&pid) {
                Some(child) if child.parent == parent => {}
                _ => return Err("not a child"),
            }
            c.waiters.entry(pid).or_default().push(sched::mark_current_blocked());
        }
        if sched::killed() {
            sched::unblock_current();
            return Err("killed");
        }
        sched::schedule();
    })
}

/// The last thread of `p` has exited.
pub fn on_exit(p: &Arc<Process>) {
    let code = p.exit_code.load(Ordering::SeqCst);
    {
        let mut c = CHILDREN.lock();
        // Only a running parent can still wait; programs started by the
        // shell (parent 0) keep no exit code.
        if p.parent != 0 && PROCESSES.lock().contains_key(&p.parent) {
            c.exited.insert(p.pid, (p.parent, code));
        }
        for t in c.waiters.remove(&p.pid).unwrap_or_default() {
            sched::wake(&t);
        }
        // Its own children's codes have no one left to collect them.
        c.exited.retain(|_, (parent, _)| *parent != p.pid);
    }
    PROCESSES.lock().remove(&p.pid);
    futex::forget_process(p.pid);
    crate::display::release(p.pid);
    p.exit.code.store(code, Ordering::SeqCst);
    p.exit.exited.store(true, Ordering::SeqCst);
    crate::wait::notify();
    if code != 0 {
        console::print_colored(console::YELLOW, format_args!("[kernel] {} (pid {}) exited with code {}\n", p.name, p.pid, code));
    }
}
