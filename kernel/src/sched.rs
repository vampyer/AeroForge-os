//! Preemptive scheduler: kernel threads and user threads, one run queue per CPU.
//!
//! - Each CPU has its own ready queue, sleep list and idle thread; the LAPIC
//!   timer preempts the running thread every tick (round robin).
//! - Time comes from the TSC in microseconds (`apic::micros`). The timer is
//!   one-shot and is re-armed for the next tick or the earliest sleeper's
//!   wake-up time, whichever is sooner, so a 2 ms sleep takes 2 ms rather
//!   than a whole 10 ms tick.
//! - Tickless idle: a CPU with nothing to run has no tick. Its timer is armed
//!   only for its earliest sleeper (or not at all), so it stays halted until
//!   an interrupt brings work. Whoever queues work for an idle CPU sends it
//!   a reschedule IPI, and a busy CPU with user threads waiting nudges an
//!   idle one to come and take them, which replaces the idle tick's steal.
//! - User threads move between CPUs: a CPU about to go idle takes a waiting
//!   user thread from the busiest other CPU's ready queue (work stealing),
//!   and every `BALANCE_TICKS` each CPU also pulls one from any CPU with at
//!   least two more threads than itself, so busy CPUs even out too.
//!   A thread is only taken once the CPU that last ran it has finished
//!   saving its context (`on_cpu` is cleared inside `switch_context`).
//!   Kernel threads stay on the CPU they were created on, since some of them
//!   (the USB and network threads) aim their device's interrupts at it.
//! - Four priority levels: low, normal and high for user threads (a game
//!   asks for high, background work for low), and a level above them for
//!   kernel threads, which mostly sleep until a device needs them. A CPU
//!   always runs its highest-priority ready thread; threads of one level
//!   share the CPU round robin. A thread woken at a higher level than the
//!   one running preempts it at once.
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
    /// The CPU whose run queue owns the thread; changes when it migrates.
    cpu: AtomicUsize,
    /// Set while a CPU runs the thread, until `switch_context` has saved
    /// its registers; another CPU must not resume it before that.
    on_cpu: AtomicBool,
    /// May another CPU take it? User threads only.
    migratable: bool,
    priority: AtomicU8,
    state: AtomicU8,
    idle: bool,
    kstack: KernelStack,
    saved_rsp: UnsafeCell<u64>,
    pml4: u64,
    /// When a sleeping thread is due to wake, in `apic::micros` time.
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

    pub fn cpu(&self) -> usize {
        self.cpu.load(Ordering::SeqCst)
    }

    pub fn priority(&self) -> u8 {
        self.priority.load(Ordering::Relaxed)
    }

    fn kstack_top(&self) -> u64 {
        self.kstack.top() & !0xF
    }

    pub fn pid(&self) -> u64 {
        self.process.as_ref().map_or(0, |p| p.pid)
    }
}

/// Priority levels. Programs may pick the first three.
pub const PRIO_LOW: u8 = 0;
pub const PRIO_NORMAL: u8 = 1;
pub const PRIO_HIGH: u8 = 2;
pub const PRIO_KERNEL: u8 = 3;
const LEVELS: usize = 4;

pub fn priority_name(p: u8) -> &'static str {
    ["low", "normal", "high", "kernel"].get(p as usize).copied().unwrap_or("?")
}

/// Ready threads, one round-robin queue per priority level.
pub struct ReadyQueues {
    levels: [VecDeque<Arc<Thread>>; LEVELS],
}

impl ReadyQueues {
    const fn new() -> Self {
        Self { levels: [const { VecDeque::new() }; LEVELS] }
    }

    fn push(&mut self, t: Arc<Thread>) {
        let p = (t.priority() as usize).min(LEVELS - 1);
        self.levels[p].push_back(t);
    }

    /// The first thread of the highest non-empty level.
    fn pop(&mut self) -> Option<Arc<Thread>> {
        self.levels.iter_mut().rev().find_map(|q| q.pop_front())
    }

    fn len(&self) -> usize {
        self.levels.iter().map(|q| q.len()).sum()
    }

    fn is_empty(&self) -> bool {
        self.levels.iter().all(|q| q.is_empty())
    }

    fn remove(&mut self, t: &Arc<Thread>) {
        for q in self.levels.iter_mut() {
            q.retain(|x| !Arc::ptr_eq(x, t));
        }
    }

    /// Is a thread waiting here that another CPU could take?
    fn has_migratable(&self) -> bool {
        self.levels.iter().any(|q| q.iter().any(|t| t.migratable && !t.on_cpu.load(Ordering::SeqCst)))
    }

    /// A thread another CPU may take: the highest level first, and in it
    /// the most recently queued (it has the least cache to lose).
    fn take_migratable(&mut self) -> Option<Arc<Thread>> {
        for q in self.levels.iter_mut().rev() {
            if let Some(i) = q.iter().rposition(|t| t.migratable && !t.on_cpu.load(Ordering::SeqCst)) {
                return q.remove(i);
            }
        }
        None
    }
}

pub struct RunQueue {
    ready: ReadyQueues,
    current: Option<Arc<Thread>>,
    idle: Option<Arc<Thread>>,
    sleepers: Vec<Arc<Thread>>,
    zombie: Option<Arc<Thread>>,
    /// When this CPU's next time-slice tick is due (`apic::micros` time).
    tick_due: u64,
    pub switches: u64,
}

impl RunQueue {
    pub fn new() -> Self {
        Self { ready: ReadyQueues::new(), current: None, idle: None, sleepers: Vec::new(), zombie: None, tick_due: 0, switches: 0 }
    }

    pub fn load(&self) -> usize {
        self.ready.len() + self.current.as_ref().map_or(0, |c| !c.idle as usize)
    }

    /// Moves sleepers whose time has come to the ready queue. True if one
    /// of them should take the CPU from the running thread.
    fn wake_due(&mut self, now: u64) -> bool {
        let mut preempt = false;
        let mut i = 0;
        while i < self.sleepers.len() {
            if self.sleepers[i].wake_at.load(Ordering::Relaxed) <= now {
                let t = self.sleepers.swap_remove(i);
                t.set_state(State::Ready);
                preempt |= outranks(&t, self);
                self.ready.push(t);
            } else {
                i += 1;
            }
        }
        preempt
    }

    /// Sets this CPU's timer for the next tick or the earliest sleeper,
    /// whichever comes first. An idle CPU gets no tick: only its sleepers
    /// can fire the timer, and with none it is stopped.
    fn arm_timer(&self, now: u64, idle: bool) {
        let first = if idle { u64::MAX } else { self.tick_due };
        let next = self.sleepers.iter().map(|t| t.wake_at.load(Ordering::Relaxed)).fold(first, u64::min);
        if next == u64::MAX {
            crate::apic::stop_timer();
        } else {
            crate::apic::arm_timer(next.saturating_sub(now));
        }
    }

    fn current_is_idle(&self) -> bool {
        self.current.as_ref().is_none_or(|c| c.idle)
    }
}

/// Every live (non-exited) thread, for `ps` and lookups.
pub static THREADS: IrqMutex<BTreeMap<u64, Arc<Thread>>> = IrqMutex::new(BTreeMap::new());
static NEXT_TID: AtomicU64 = AtomicU64::new(1);
static NEXT_CPU: AtomicUsize = AtomicUsize::new(0);
/// Threads moved to another CPU by work stealing.
pub static MIGRATIONS: AtomicU64 = AtomicU64::new(0);
/// Of those, the ones periodic balancing moved between busy CPUs.
pub static BALANCE_PULLS: AtomicU64 = AtomicU64::new(0);
/// One bit per CPU that is running its idle thread (tickless, halted).
static IDLE_CPUS: AtomicU64 = AtomicU64::new(0);
/// Reschedule IPIs sent to wake an idle CPU for waiting threads.
pub static IDLE_NUDGES: AtomicU64 = AtomicU64::new(0);

/// Microseconds per scheduler tick (the time slice).
pub const TICK_US: u64 = 1_000_000 / crate::apic::TIMER_HZ;

/// Time since boot in ticks (`apic::TIMER_HZ` per second).
pub fn ticks() -> u64 {
    crate::apic::micros() / TICK_US
}

global_asm!(
    r#"
.section .text
// switch_context(save_rsp_to: *mut u64, load_rsp: u64, prev_on_cpu: *mut u8)
// Clearing prev_on_cpu after the save tells other CPUs the outgoing thread
// may now be resumed elsewhere (x86 keeps the two stores in order).
.global switch_context
switch_context:
    push rbp
    push rbx
    push r12
    push r13
    push r14
    push r15
    mov [rdi], rsp
    mov byte ptr [rdx], 0
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
    fn switch_context(save_rsp_to: *mut u64, load_rsp: u64, prev_on_cpu: *mut u8);
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
        cpu: AtomicUsize::new(cpu.index),
        on_cpu: AtomicBool::new(true),
        migratable: false,
        priority: AtomicU8::new(PRIO_LOW),
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
        migratable: process.is_some(),
        priority: AtomicU8::new(if process.is_some() { PRIO_NORMAL } else { PRIO_KERNEL }),
        process,
        cpu: AtomicUsize::new(cpu),
        on_cpu: AtomicBool::new(false),
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
    let cpu = percpu::get(t.cpu()).expect("thread placed on an offline CPU");
    let preempt = {
        let mut rq = cpu.rq.lock();
        rq.ready.push(t.clone());
        outranks(&t, &rq)
    };
    // The CPU may be idle with its tick stopped: tell it.
    if scheduler_running(cpu) {
        kick(cpu, preempt, t.migratable);
    }
    t
}

/// Has `cpu` started scheduling (so it can take a reschedule IPI)?
fn scheduler_running(cpu: &percpu::PerCpu) -> bool {
    cpu.rq.lock().current.is_some()
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
    let would_idle = {
        let rq = cpu.rq.lock();
        rq.ready.is_empty() && rq.current.as_ref().is_none_or(|c| c.idle || c.state() != State::Running)
    };
    if would_idle {
        steal(cpu);
    }
    let mut nudge = false;
    let switch = {
        let mut rq = cpu.rq.lock();
        let now = crate::apic::micros();
        rq.wake_due(now);

        let prev = rq.current.take().expect("no current thread");
        if prev.state() == State::Running && !prev.idle {
            prev.set_state(State::Ready);
            rq.ready.push(prev.clone());
        }
        let next = match rq.ready.pop() {
            Some(t) => t,
            None => rq.idle.clone().unwrap(),
        };
        next.set_state(State::Running);
        next.on_cpu.store(true, Ordering::SeqCst);
        let bit = 1u64 << (cpu.index % 64);
        if next.idle {
            IDLE_CPUS.fetch_or(bit, Ordering::SeqCst);
        } else {
            IDLE_CPUS.fetch_and(!bit, Ordering::SeqCst);
            if rq.tick_due <= now {
                // Coming out of tickless idle: a full time slice from now.
                rq.tick_due = now + TICK_US;
            }
            nudge = rq.ready.has_migratable();
        }
        rq.arm_timer(now, next.idle);
        if Arc::ptr_eq(&prev, &next) {
            rq.current = Some(next);
            None
        } else {
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
        let prev_on_cpu = prev.on_cpu.as_ptr() as *mut u8;
        rq.current = Some(next);
        Some((save_to, load, prev_on_cpu))
        }
    };
    if nudge {
        nudge_idle(cpu.index);
    }
    if let Some((save_to, load, prev_on_cpu)) = switch {
        unsafe { switch_context(save_to, load, prev_on_cpu) };
    }
}

/// Wakes one idle CPU (other than `me`) so it can take a waiting thread.
fn nudge_idle(me: usize) {
    let idle = IDLE_CPUS.load(Ordering::SeqCst) & !(1u64 << (me % 64));
    if idle == 0 {
        return;
    }
    if let Some(cpu) = percpu::get(idle.trailing_zeros() as usize) {
        IDLE_NUDGES.fetch_add(1, Ordering::Relaxed);
        crate::apic::send_ipi(cpu.lapic_id, crate::interrupts::VECTOR_RESCHEDULE);
    }
}

/// Moves one waiting user thread from the CPU with the longest ready queue
/// to `me`, which has nothing to run. Only one run queue lock is held at a
/// time, so two CPUs stealing from each other cannot deadlock.
fn steal(me: &percpu::PerCpu) {
    let mut victim = None;
    let mut longest = 0;
    for i in 0..percpu::count() {
        if i == me.index {
            continue;
        }
        let Some(c) = percpu::get(i) else { continue };
        let n = c.rq.lock().ready.len();
        if n > longest {
            longest = n;
            victim = Some(c);
        }
    }
    let Some(victim) = victim else { return };
    let taken = {
        let mut rq = victim.rq.lock();
        rq.ready.take_migratable()
    };
    if let Some(t) = taken {
        t.cpu.store(me.index, Ordering::SeqCst);
        MIGRATIONS.fetch_add(1, Ordering::Relaxed);
        me.rq.lock().ready.push(t);
    }
}

/// How often (in ticks) a CPU checks whether another has more threads.
const BALANCE_TICKS: u64 = 4;

/// Periodic balancing: pulls one waiting user thread from the busiest CPU
/// if that CPU has at least two more threads (running or ready) than this
/// one. Moving one then never makes the busiest CPU the lighter one, so
/// threads do not bounce back and forth. Like `steal`, only one run queue
/// lock is held at a time.
fn pull_if_imbalanced(me: &percpu::PerCpu) {
    let mine = me.rq.lock().load();
    let mut victim = None;
    let mut most = mine + 1;
    for i in 0..percpu::count() {
        if i == me.index {
            continue;
        }
        let Some(c) = percpu::get(i) else { continue };
        let n = c.rq.lock().load();
        if n > most {
            most = n;
            victim = Some(c);
        }
    }
    let Some(victim) = victim else { return };
    let taken = {
        let mut rq = victim.rq.lock();
        // Its load may have dropped since we looked.
        if rq.load() < mine + 2 {
            return;
        }
        rq.ready.take_migratable()
    };
    if let Some(t) = taken {
        t.cpu.store(me.index, Ordering::SeqCst);
        MIGRATIONS.fetch_add(1, Ordering::Relaxed);
        BALANCE_PULLS.fetch_add(1, Ordering::Relaxed);
        me.rq.lock().ready.push(t);
    }
}

/// Called from the timer interrupt on every CPU. When a tick is due the
/// running thread's time slice is over; otherwise the interrupt is for a
/// sleeper, which only takes the CPU if it outranks the running thread.
pub fn on_timer() {
    let cpu = percpu::this();
    let now = crate::apic::micros();
    let mut balance = false;
    cpu.timer_irqs.fetch_add(1, Ordering::Relaxed);
    let reschedule = {
        let mut rq = cpu.rq.lock();
        let idle = rq.current_is_idle();
        if !idle && now >= rq.tick_due {
            // Stay on the 10 ms grid unless we fell more than a tick behind.
            rq.tick_due = if rq.tick_due + TICK_US > now { rq.tick_due + TICK_US } else { now + TICK_US };
            if let Some(cur) = rq.current.as_ref() {
                cur.runtime_ticks.fetch_add(1, Ordering::Relaxed);
            }
            balance = cpu.ticks.fetch_add(1, Ordering::Relaxed) % BALANCE_TICKS == 0;
            true
        } else if rq.wake_due(now) {
            true
        } else {
            rq.arm_timer(now, idle);
            false
        }
    };
    if balance {
        pull_if_imbalanced(cpu);
    }
    if reschedule {
        schedule();
    }
}

/// Puts the current thread on this CPU's sleep list until `until_us`
/// (`apic::micros` time) and marks it sleeping. Like
/// `mark_current_blocked`, the caller calls `schedule` next with
/// interrupts still disabled; `wake_sleeper` (or `wake_waiter`) ends
/// the sleep early.
pub fn mark_current_sleeping(until_us: u64) -> Arc<Thread> {
    let mut rq = percpu::this().rq.lock();
    let cur = rq.current.clone().unwrap();
    cur.wake_at.store(until_us, Ordering::Relaxed);
    cur.set_state(State::Sleeping);
    rq.sleepers.push(cur.clone());
    cur
}

/// Sleeps until `until_us` in `apic::micros` time.
pub fn sleep_until(until_us: u64) {
    arch::without_interrupts(|| {
        mark_current_sleeping(until_us);
        schedule();
    });
}

pub fn sleep_us(us: u64) {
    sleep_until(crate::apic::micros().saturating_add(us));
}

pub fn sleep_ticks(n: u64) {
    sleep_us(n.saturating_mul(TICK_US));
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
            cur.wake_at.store(crate::apic::micros() + n.max(1) * TICK_US, Ordering::Relaxed);
            cur.set_state(State::Sleeping);
            rq.sleepers.push(cur);
        }
        schedule();
    });
}

/// Ends a thread's sleep early (it runs as soon as its CPU gets to it).
pub fn wake_sleeper(t: &Arc<Thread>) {
    let Some(cpu) = percpu::get(t.cpu()) else { return };
    let woke = {
        let mut rq = cpu.rq.lock();
        if t.state() == State::Sleeping {
            rq.sleepers.retain(|s| !Arc::ptr_eq(s, t));
            t.set_state(State::Ready);
            rq.ready.push(t.clone());
            Some(outranks(t, &rq))
        } else {
            None
        }
    };
    if let Some(preempt) = woke {
        kick(cpu, preempt, t.migratable);
    }
}

/// Should woken thread `t` take the CPU from the one running there now?
fn outranks(t: &Thread, rq: &RunQueue) -> bool {
    rq.current.as_ref().is_none_or(|c| c.idle || t.priority() > c.priority())
}

/// After a wakeup on `cpu`: another CPU gets a reschedule IPI so it notices
/// at once (it may be idle with no tick); this CPU sends one to itself when
/// the woken thread outranks the running one (it fires as soon as
/// interrupts are on again, e.g. right after the interrupt handler that
/// did the waking). A user thread left waiting behind the running one
/// nudges an idle CPU to come and take it.
fn kick(cpu: &percpu::PerCpu, preempt: bool, migratable: bool) {
    if cpu.index != percpu::this().index || preempt {
        crate::apic::send_ipi(cpu.lapic_id, crate::interrupts::VECTOR_RESCHEDULE);
    }
    if !preempt && migratable {
        // It waits behind the running thread; an idle CPU could take it.
        nudge_idle(cpu.index);
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
    let cpu = percpu::get(t.cpu()).unwrap();
    let woke = {
        let mut rq = cpu.rq.lock();
        if t.state() == State::Blocked {
            t.set_state(State::Ready);
            rq.ready.push(t.clone());
            Some(outranks(t, &rq))
        } else {
            None
        }
    };
    if let Some(preempt) = woke {
        kick(cpu, preempt, t.migratable);
    }
}

/// Wakes a thread waiting with or without a timeout (blocked or sleeping).
pub fn wake_waiter(t: &Arc<Thread>) {
    wake(t);
    wake_sleeper(t);
}

/// Sets the priority of thread `t` (one of the PRIO_ constants). A thread
/// waiting in a ready queue moves to its new level there.
pub fn set_priority(t: &Arc<Thread>, priority: u8) {
    let Some(cpu) = percpu::get(t.cpu()) else { return };
    let mut rq = cpu.rq.lock();
    t.priority.store(priority, Ordering::Relaxed);
    if t.state() == State::Ready && rq.ready.levels.iter().any(|q| q.iter().any(|x| Arc::ptr_eq(x, t))) {
        rq.ready.remove(t);
        rq.ready.push(t.clone());
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

/// Undoes `mark_current_blocked` (or `mark_current_sleeping`) for a thread
/// that changed its mind before calling `schedule` (and may meanwhile have
/// been woken onto the ready queue).
pub fn unblock_current() {
    let cpu = percpu::this();
    let mut rq = cpu.rq.lock();
    let cur = rq.current.clone().unwrap();
    match cur.state() {
        State::Ready => rq.ready.remove(&cur),
        State::Sleeping => rq.sleepers.retain(|s| !Arc::ptr_eq(s, &cur)),
        _ => {}
    }
    cur.set_state(State::Running);
}

/// Gets every thread of process `pid` moving towards its exit: blocked
/// and sleeping threads are woken (their wait loops see `killed()`), and
/// threads in ring 3 die at their next interrupt.
/// The caller has already set the process's `exiting` flag.
pub fn kill_threads_of(pid: u64) {
    let victims: Vec<Arc<Thread>> = THREADS.lock().values().filter(|t| t.pid() == pid).cloned().collect();
    for t in victims {
        wake_waiter(&t);
        if t.cpu() != percpu::this().index {
            if let Some(cpu) = percpu::get(t.cpu()) {
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

