//! Per-CPU data, reached through the GS segment base.
//!
//! In kernel mode GS points at this CPU's `PerCpu`; the interrupt entry
//! stubs `swapgs` whenever they come from or return to ring 3, so user code
//! never sees (or can change) the kernel's GS base.

use alloc::boxed::Box;
use core::arch::asm;
use core::sync::atomic::{AtomicPtr, AtomicU64, AtomicUsize, Ordering};

use crate::arch;
use crate::gdt::CpuTables;
use crate::sched::RunQueue;
use crate::sync::IrqMutex;

pub const MAX_CPUS: usize = 64;

#[repr(C)]
pub struct PerCpu {
    self_ptr: u64, // gs:[0]
    pub index: usize,
    pub lapic_id: u32,
    pub tables: CpuTables,
    pub rq: IrqMutex<RunQueue>,
    pub ticks: AtomicU64,
}

static CPUS: [AtomicPtr<PerCpu>; MAX_CPUS] = [const { AtomicPtr::new(core::ptr::null_mut()) }; MAX_CPUS];
static COUNT: AtomicUsize = AtomicUsize::new(0);

/// Creates and activates the per-CPU block for the calling CPU.
pub fn init_this_cpu(index: usize, lapic_id: u32) -> &'static PerCpu {
    let cpu = Box::leak(Box::new(PerCpu {
        self_ptr: 0,
        index,
        lapic_id,
        tables: CpuTables::new(),
        rq: IrqMutex::new(RunQueue::new()),
        ticks: AtomicU64::new(0),
    }));
    cpu.self_ptr = cpu as *mut PerCpu as u64;
    cpu.tables.load();
    unsafe {
        arch::wrmsr(arch::MSR_GS_BASE, cpu.self_ptr);
        arch::wrmsr(arch::MSR_KERNEL_GS_BASE, 0);
    }
    CPUS[index].store(cpu, Ordering::SeqCst);
    COUNT.fetch_max(index + 1, Ordering::SeqCst);
    cpu
}

/// The calling CPU's data.
#[inline]
pub fn this() -> &'static PerCpu {
    let p: u64;
    unsafe { asm!("mov {}, gs:[0]", out(reg) p, options(nostack, preserves_flags, readonly)) };
    unsafe { &*(p as *const PerCpu) }
}

pub fn get(index: usize) -> Option<&'static PerCpu> {
    unsafe { CPUS.get(index)?.load(Ordering::SeqCst).as_ref() }
}

pub fn count() -> usize {
    COUNT.load(Ordering::SeqCst)
}

/// Sets the kernel stack the CPU switches to on entry from ring 3.
pub fn set_kernel_stack(top: u64) {
    let cpu = this() as *const PerCpu as *mut PerCpu;
    unsafe { core::ptr::addr_of_mut!((*cpu).tables.tss.rsp).write_unaligned([top, 0, 0]) };
}
