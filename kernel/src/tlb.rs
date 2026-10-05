//! TLB shootdowns: after a page is unmapped, every other CPU drops its
//! cached translations before the frame can be reused. Needed once threads
//! of one process run on several CPUs at the same time.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::{apic, arch, interrupts, percpu};

static BUSY: AtomicBool = AtomicBool::new(false);
static PENDING: AtomicUsize = AtomicUsize::new(0);
static SHOOTDOWNS: AtomicUsize = AtomicUsize::new(0);

/// Makes every other CPU flush its TLB and waits until all have. Must not be
/// called holding a spinlock: interrupts are on while it waits, so a CPU
/// shooting down at the same time can reach this one.
pub fn shootdown() {
    let me = percpu::this().index;
    let others: usize = (0..percpu::count()).filter(|&i| i != me && percpu::get(i).is_some()).count();
    if others == 0 {
        return;
    }
    let was_enabled = arch::interrupts_enabled();
    arch::enable_interrupts();
    while BUSY.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
        core::hint::spin_loop();
    }
    PENDING.store(others, Ordering::SeqCst);
    // Threads are pinned, so this is still the CPU counted out above.
    let me = percpu::this().index;
    for i in (0..percpu::count()).filter(|&i| i != me) {
        if let Some(cpu) = percpu::get(i) {
            apic::send_ipi(cpu.lapic_id, interrupts::VECTOR_TLB_SHOOTDOWN);
        }
    }
    while PENDING.load(Ordering::SeqCst) != 0 {
        core::hint::spin_loop();
    }
    SHOOTDOWNS.fetch_add(1, Ordering::Relaxed);
    BUSY.store(false, Ordering::Release);
    if !was_enabled {
        arch::disable_interrupts();
    }
}

/// The IPI handler: reloading CR3 flushes every non-global user mapping.
pub fn on_ipi() {
    unsafe { arch::write_cr3(arch::read_cr3()) };
    PENDING.fetch_sub(1, Ordering::SeqCst);
    apic::eoi();
}

/// Shootdowns done since boot (for the `mem` command).
pub fn count() -> usize {
    SHOOTDOWNS.load(Ordering::Relaxed)
}
