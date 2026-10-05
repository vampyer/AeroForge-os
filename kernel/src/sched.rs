//! Preemptive scheduler: kernel threads and user threads, one run queue per CPU.
//!
//! - Each CPU has its own ready queue, sleep list and idle thread; the LAPIC
//!   timer preempts the running thread every tick (round robin).
//! - A thread is pinned to the CPU it was created on. That keeps the
//!   switch path simple (a thread's saved context is only ever resumed by the
//!   CPU that saved it); load balancing by migration comes later.
//! - Every lock the scheduler touches is an `IrqMutex`, so code holding one
//!   can never be preempted into a deadlock.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::arch::global_asm;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};

use crate::interrupts::InterruptFrame;
use crate::process::Process;
use crate::sync::IrqMutex;
use crate::fpu::FpuArea;
use crate::kstack::KernelStack;
use crate::{arch, gdt, memory, percpu};

const KSTACK_SIZE: usize = 32 * 1024;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum State {
    Ready = 0,
    Running = 1,
    Blocked = 2,
    Sleeping = 3,
    Dead = 4,
}

impl State {
    pub fn name(self) -> &'static str {
        match self {
            State::Ready => "ready",
            State::Running => "running",
            State::Blocked => "blocked",
            State::Sleeping => "sleeping",
            State::Dead => "dead",
        }
    }
}

pub struct Thread {
    pub tid: u64,
    pub name: String,
    pub process: Option<Arc<Process>>,
    pub cpu: usize,
    state: AtomicU8,
    idle: bool,
    kstack: KernelStack,
    saved_rsp: UnsafeCell<u64>,
    pml4: u64,
    wake_at: AtomicU64,
    pub runtime_ticks: AtomicU64,
    /// Saved floating point and vector registers; user threads only.
    fpu: Option<FpuArea>,
    /// Threads waiting in SYS_THREAD_JOIN for this one to exit.
    join: IrqMutex<JoinState>,
}

#[derive(Default)]
struct JoinState {
    exited: bool,
    waiters: Vec<Arc<Thread>>,
}

// saved_rsp is only touched by the owning CPU inside `schedule`.
unsafe impl Sync for Thread {}
unsafe impl Send for Thread {}

impl Thread {
    pub fn state(&self) -> State {
        match self.state.load(Ordering::SeqCst) {
            0 => State::Ready,
            1 => State::Running,
            2 => State::Blocked,
            3 => State::Sleeping,
            _ => State::Dead,
        }
    }

    fn set_state(&self, s: State) {
        self.state.store(s as u8, Ordering::SeqCst);
    }

    fn kstack_top(&self) -> u64 {
        self.kstack.top() & !0xF
    }

    pub fn pid(&self) -> u64 {
        self.process.as_ref().map_or(0, |p| p.pid)
    }
}

pub struct RunQueue {
    ready: VecDeque<Arc<Thread>>,
    current: Option<Arc<Thread>>,
    idle: Option<Arc<Thread>>,
    sleepers: Vec<Arc<Thread>>,
    zombie: Option<Arc<Thread>>,
    pub switches: u64,
}

impl RunQueue {
    pub fn new() -> Self {
        Self { ready: VecDeque::new(), current: None, idle: None, sleepers: Vec::new(), zombie: None, switches: 0 }
    }

    pub fn load(&self) -> usize {
        self.ready.len() + self.current.as_ref().map_or(0, |c| !c.idle as usize)
    }
}

/// Every live (non-exited) thread, for `ps` and lookups.
pub static THREADS: IrqMutex<BTreeMap<u64, Arc<Thread>>> = IrqMutex::new(BTreeMap::new());
static NEXT_TID: AtomicU64 = AtomicU64::new(1);
static NEXT_CPU: AtomicUsize = AtomicUsize::new(0);
pub static TICKS: AtomicU64 = AtomicU64::new(0);

pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

global_asm!(
    r#"
.section .text
// switch_context(save_rsp_to: *mut u64, load_rsp: u64)
.global switch_context
switch_context:
    push rbp
    push rbx
    push r12
    push r13
    push r14
    push r15
    mov [rdi], rsp
    mov rsp, rsi
    pop r15
    pop r14
    pop r13
    pop r12
    pop rbx
    pop rbp
    ret

// First instructions of every new kernel thread: r12 = entry, r13 = argument.
kthread_trampoline:
    mov rdi, r12
    mov rsi, r13
    call kthread_entry
    ud2
"#
);

extern "C" {
    fn switch_context(save_rsp_to: *mut u64, load_rsp: u64);
    fn kthread_trampoline();
    fn isr_exit();
}

#[no_mangle]
extern "C" fn kthread_entry(entry: usize, arg: u64) -> ! {
    arch::enable_interrupts();
    let f: fn(u64) = unsafe { core::mem::transmute(entry) };
    f(arg);
    exit_current();
}

/// Turns the code already running on this CPU (boot stack) into its idle
/// thread, so the scheduler has something to switch away from.
pub fn init_cpu() {
    let cpu = percpu::this();
    let idle = Arc::new(Thread {
        tid: NEXT_TID.fetch_add(1, Ordering::SeqCst),
        name: alloc::format!("idle/{}", cpu.index),
        process: None,
        cpu: cpu.index,
        state: AtomicU8::new(State::Running as u8),
        idle: true,
        kstack: KernelStack::empty(),
        saved_rsp: UnsafeCell::new(0),
        pml4: memory::kernel_pml4(),
        wake_at: AtomicU64::new(0),
        runtime_ticks: AtomicU64::new(0),
        fpu: None,
        join: IrqMutex::new(JoinState::default()),
    });
    let mut rq = cpu.rq.lock();
    rq.idle = Some(idle.clone());
    rq.current = Some(idle);
}

fn pick_cpu() -> usize {
    NEXT_CPU.fetch_add(1, Ordering::SeqCst) % percpu::count().max(1)
}

fn new_thread(name: String, process: Option<Arc<Process>>, cpu: usize) -> Thread {
    let pml4 = process.as_ref().map_or(memory::kernel_pml4(), |p| p.pml4);
    let fpu = process.as_ref().map(|_| FpuArea::new().expect("out of memory for a register save area"));
    Thread {
        tid: NEXT_TID.fetch_add(1, Ordering::SeqCst),
        name,
        process,
        cpu,
        state: AtomicU8::new(State::Ready as u8),
        idle: false,
        kstack: KernelStack::new(KSTACK_SIZE as u64).expect("out of memory for a kernel stack"),
        saved_rsp: UnsafeCell::new(0),
        pml4,
        wake_at: AtomicU64::new(0),
        runtime_ticks: AtomicU64::new(0),
        fpu,
        join: IrqMutex::new(JoinState::default()),
    }
}

fn enqueue_new(t: Thread) -> Arc<Thread> {
    let t = Arc::new(t);
    THREADS.lock().insert(t.tid, t.clone());
    let cpu = percpu::get(t.cpu).expect("thread placed on an offline CPU");
    cpu.rq.lock().ready.push_back(t.clone());
    t
}

/// Writes the frame `switch_context` pops for a thread that has never run.
unsafe fn seed_switch_frame(stack_ptr: u64, ret: u64, r12: u64, r13: u64) -> u64 {
    let frame = stack_ptr as *mut u64;
    // r15, r14, r13, r12, rbx, rbp, return address
    let words = [0, 0, r13, r12, 0, 0, ret];
    for (i, w) in words.iter().enumerate() {
        frame.add(i).write(*w);
    }
    stack_ptr
}

pub fn spawn_kernel(name: &str, entry: fn(u64), arg: u64, cpu: Option<usize>) -> Arc<Thread> {
    let t = new_thread(String::from(name), None, cpu.unwrap_or_else(pick_cpu));
    // After `ret` into the trampoline the stack must be 16-byte aligned.
    let rsp = t.kstack_top() - 72;
    unsafe { *t.saved_rsp.get() = seed_switch_frame(rsp, kthread_trampoline as *const () as u64, entry as *const () as u64, arg) };
    enqueue_new(t)
}

/// Creates a ring-3 thread that starts at `entry` with stack `user_rsp` and
/// `arg` in rdi (the first argument).
pub fn spawn_user(process: Arc<Process>, name: &str, entry: u64, user_rsp: u64, arg: u64) -> Arc<Thread> {
    process.threads.fetch_add(1, Ordering::SeqCst);
    let t = new_thread(String::from(name), Some(process), pick_cpu());
    let frame_addr = t.kstack_top() - core::mem::size_of::<InterruptFrame>() as u64;
    unsafe {
        let frame = &mut *(frame_addr as *mut InterruptFrame);
        *frame = InterruptFrame::zeroed();
        frame.rip = entry;
        frame.cs = gdt::USER_CODE as u64;
        frame.rflags = 0x202; // IF set
        frame.rsp = user_rsp;
        frame.rdi = arg;
        frame.ss = gdt::USER_DATA as u64;
        // First switch "returns" through the interrupt exit path into ring 3.
        *t.saved_rsp.get() = seed_switch_frame(frame_addr - 56, isr_exit as *const () as u64, 0, 0);
    }
    enqueue_new(t)
}

pub fn current() -> Arc<Thread> {
    percpu::this().rq.lock().current.clone().expect("scheduler not initialised")
}

/// Picks the next thread on this CPU and switches to it. Must be called with
/// interrupts disabled; the current thread must already have recorded where
/// it is going (still running, sleeping, blocked or dead).
pub fn schedule() {
    debug_assert!(!arch::interrupts_enabled());
    let cpu = percpu::this();
    let (save_to, load) = {
        let mut rq = cpu.rq.lock();
        let now = ticks();
        let mut i = 0;
        while i < rq.sleepers.len() {
            if rq.sleepers[i].wake_at.load(Ordering::Relaxed) <= now {
                let t = rq.sleepers.swap_remove(i);
                t.set_state(State::Ready);
                rq.ready.push_back(t);
            } else {
                i += 1;
            }
        }

        let prev = rq.current.take().expect("no current thread");
        if prev.state() == State::Running && !prev.idle {
            prev.set_state(State::Ready);
            rq.ready.push_back(prev.clone());
        }
        let next = match rq.ready.pop_front() {
            Some(t) => t,
            None => rq.idle.clone().unwrap(),
        };
        next.set_state(State::Running);
        if Arc::ptr_eq(&prev, &next) {
            rq.current = Some(next);
            return;
        }
        if prev.state() == State::Dead {
            // Keep the dead thread (and its stack, which we are standing on)
            // alive until the next switch; the previous zombie goes now.
            rq.zombie = Some(prev.clone());
        }
        rq.switches += 1;

        // The kernel never uses the floating point registers, so they still
        // hold the outgoing user thread's values here.
        if prev.state() != State::Dead {
            if let Some(f) = prev.fpu.as_ref() {
                f.save();
            }
        }
        if let Some(f) = next.fpu.as_ref() {
            f.restore();
        }

        percpu::set_kernel_stack(next.kstack_top());
        if arch::read_cr3() & !0xFFF != next.pml4 {
            unsafe { arch::write_cr3(next.pml4) };
        }
        let save_to = prev.saved_rsp.get();
        let load = unsafe { *next.saved_rsp.get() };
        rq.current = Some(next);
        (save_to, load)
    };
    unsafe { switch_context(save_to, load) };
}

/// Called from the timer interrupt on every CPU.
pub fn on_tick() {
    let cpu = percpu::this();
    if cpu.index == 0 {
        TICKS.fetch_add(1, Ordering::Relaxed);
    }
    cpu.ticks.fetch_add(1, Ordering::Relaxed);
    if let Some(cur) = cpu.rq.lock().current.as_ref() {
        cur.runtime_ticks.fetch_add(1, Ordering::Relaxed);
    }
    schedule();
}

pub fn sleep_ticks(n: u64) {
    arch::without_interrupts(|| {
        {
            let mut rq = percpu::this().rq.lock();
            let cur = rq.current.clone().unwrap();
            cur.wake_at.store(ticks() + n.max(1), Ordering::Relaxed);
            cur.set_state(State::Sleeping);
            rq.sleepers.push(cur);
        }
        schedule();
    });
}

/// Sleeps up to `n` ticks, unless `flag` is set (checked under the run
/// queue lock, so a `wake_sleeper` after setting the flag is never lost).
fn sleep_ticks_unless(n: u64, flag: &AtomicBool) {
    arch::without_interrupts(|| {
        {
            let mut rq = percpu::this().rq.lock();
            if flag.load(Ordering::SeqCst) {
                return;
            }
            let cur = rq.current.clone().unwrap();
            cur.wake_at.store(ticks() + n.max(1), Ordering::Relaxed);
            cur.set_state(State::Sleeping);
            rq.sleepers.push(cur);
        }
        schedule();
    });
}

/// Ends a thread's sleep early (it runs as soon as its CPU gets to it).
pub fn wake_sleeper(t: &Arc<Thread>) {
    let Some(cpu) = percpu::get(t.cpu) else { return };
    let woke = {
        let mut rq = cpu.rq.lock();
        if t.state() == State::Sleeping {
            rq.sleepers.retain(|s| !Arc::ptr_eq(s, t));
            t.set_state(State::Ready);
            rq.ready.push_back(t.clone());
            true
        } else {
            false
        }
    };
    if woke && cpu.index != percpu::this().index {
        crate::apic::send_ipi(cpu.lapic_id, crate::interrupts::VECTOR_RESCHEDULE);
    }
}

/// Something one kernel thread waits for, such as a device interrupt, with
/// a timeout so a missed signal only costs latency. One waiter at a time.
pub struct Event {
    pending: AtomicBool,
    waiter: IrqMutex<Option<Arc<Thread>>>,
}

impl Event {
    pub const fn new() -> Self {
        Self { pending: AtomicBool::new(false), waiter: IrqMutex::new(None) }
    }

    /// Waits until signalled or `ticks` pass. True if it was signalled.
    pub fn wait(&self, ticks: u64) -> bool {
        *self.waiter.lock() = Some(current());
        sleep_ticks_unless(ticks, &self.pending);
        *self.waiter.lock() = None;
        self.pending.swap(false, Ordering::SeqCst)
    }

    /// Safe from interrupt handlers.
    pub fn signal(&self) {
        self.pending.store(true, Ordering::SeqCst);
        if let Some(t) = self.waiter.lock().clone() {
            wake_sleeper(&t);
        }
    }
}

/// Can the calling code sleep? Not before the scheduler runs on this CPU,
/// and not on an idle thread (the boot code is one).
pub fn can_block() -> bool {
    if percpu::count() == 0 {
        return false;
    }
    let rq = percpu::this().rq.lock();
    rq.current.as_ref().is_some_and(|t| !t.idle)
}

/// Marks the current thread blocked. The caller must have put it on some wait
/// list (holding that list's lock) and must call `schedule` next, with
/// interrupts still disabled.
pub fn mark_current_blocked() -> Arc<Thread> {
    let cur = current();
    cur.set_state(State::Blocked);
    cur
}

/// Makes a blocked thread runnable again (on its own CPU). If that CPU is
/// another one, a reschedule IPI makes it pick the thread up now rather than
/// at its next timer tick.
pub fn wake(t: &Arc<Thread>) {
    let cpu = percpu::get(t.cpu).unwrap();
    let woke = {
        let mut rq = cpu.rq.lock();
        if t.state() == State::Blocked {
            t.set_state(State::Ready);
            rq.ready.push_back(t.clone());
            true
        } else {
            false
        }
    };
    if woke && cpu.index != percpu::this().index {
        crate::apic::send_ipi(cpu.lapic_id, crate::interrupts::VECTOR_RESCHEDULE);
    }
}

pub fn exit_current() -> ! {
    arch::disable_interrupts();
    let cur = current();
    THREADS.lock().remove(&cur.tid);
    {
        let mut j = cur.join.lock();
        j.exited = true;
        for t in j.waiters.drain(..) {
            wake(&t);
        }
    }
    if let Some(p) = cur.process.as_ref() {
        if p.threads.fetch_sub(1, Ordering::SeqCst) == 1 {
            crate::process::on_exit(p);
        }
    }
    cur.set_state(State::Dead);
    drop(cur);
    schedule();
    unreachable!("dead thread was rescheduled");
}

/// Waits until thread `tid` has exited. Returns false if no such thread is
/// running (it may have exited already). Blocking helpers like this one give
/// up when the caller's process is killed and let the system call return
/// path end the thread, so nothing they hold is leaked.
pub fn join(tid: u64) -> bool {
    let Some(t) = THREADS.lock().get(&tid).cloned() else { return false };
    arch::without_interrupts(|| loop {
        {
            let mut j = t.join.lock();
            if j.exited {
                return true;
            }
            j.waiters.push(mark_current_blocked());
        }
        if killed() {
            unblock_current();
            return false;
        }
        schedule();
    })
}

/// Is the current thread's process being torn down? Then the thread must
/// not block (or go back to ring 3) any more, only exit.
pub fn killed() -> bool {
    current().process.as_ref().is_some_and(|p| p.exiting.load(Ordering::SeqCst))
}

/// Undoes `mark_current_blocked` for a thread that changed its mind before
/// calling `schedule` (and may meanwhile have been woken onto the ready queue).
pub fn unblock_current() {
    let cpu = percpu::this();
    let mut rq = cpu.rq.lock();
    let cur = rq.current.clone().unwrap();
    if cur.state() == State::Ready {
        rq.ready.retain(|t| !Arc::ptr_eq(t, &cur));
    }
    cur.set_state(State::Running);
}

/// Gets every thread of process `pid` moving towards its exit: blocked
/// threads are woken (their wait loops see `killed()`), sleepers wake at
/// the next tick, and threads in ring 3 die at their next interrupt.
/// The caller has already set the process's `exiting` flag.
pub fn kill_threads_of(pid: u64) {
    let victims: Vec<Arc<Thread>> = THREADS.lock().values().filter(|t| t.pid() == pid).cloned().collect();
    for t in victims {
        t.wake_at.store(0, Ordering::Relaxed);
        wake(&t);
        if t.cpu != percpu::this().index {
            if let Some(cpu) = percpu::get(t.cpu) {
                crate::apic::send_ipi(cpu.lapic_id, crate::interrupts::VECTOR_RESCHEDULE);
            }
        }
    }
}

/// Idle loop for a CPU once its boot code is done.
pub fn idle_loop() -> ! {
    loop {
        arch::enable_interrupts();
        arch::hlt();
    }
}

pub struct CpuStats {
    pub ready: usize,
    pub switches: u64,
    pub current: String,
}

pub fn cpu_stats(index: usize) -> Option<CpuStats> {
    let cpu = percpu::get(index)?;
    let rq = cpu.rq.lock();
    Some(CpuStats {
        ready: rq.load(),
        switches: rq.switches,
        current: rq.current.as_ref().map_or(String::new(), |t| t.name.clone()),
    })
}

