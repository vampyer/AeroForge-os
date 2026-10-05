//! Multi-core bring-up. Limine already parked every application processor
//! (AP) in 64-bit mode; we release each one by writing its entry point. Each
//! AP sets up its own GDT/TSS, per-CPU data and LAPIC timer, then joins the
//! scheduler.

use core::sync::atomic::{AtomicU32, Ordering};

use crate::limine::{MpInfo, MpResponse};
use crate::{apic, arch, console, interrupts, percpu, sched};

pub static ONLINE: AtomicU32 = AtomicU32::new(1); // the boot CPU

pub fn start_aps(mp: &MpResponse) -> u32 {
    let mut started = 0;
    let mut index = 1u64;
    for cpu in mp.cpus() {
        if cpu.lapic_id == mp.bsp_lapic_id {
            continue;
        }
        // extra_argument carries our CPU index; it must be written before
        // the goto_address store releases the AP.
        unsafe { core::ptr::addr_of!(cpu.extra_argument).cast_mut().write_volatile(index) };
        cpu.goto_address.store(ap_entry as *const () as u64, Ordering::SeqCst);
        index += 1;
        started += 1;
        // Bring them up one at a time so per-CPU indices stay in order.
        let deadline = arch::rdtsc() + 2_000_000_000;
        while ONLINE.load(Ordering::SeqCst) < 1 + started && arch::rdtsc() < deadline {
            core::hint::spin_loop();
        }
    }
    ONLINE.load(Ordering::SeqCst)
}

extern "C" fn ap_entry(info: &MpInfo) -> ! {
    let index = info.extra_argument as usize;
    crate::security::init_cpu(false);
    percpu::init_this_cpu(index, info.lapic_id);
    interrupts::load();
    crate::syscall::init_cpu();
    crate::fpu::init_cpu();
    sched::init_cpu();
    apic::start_timer(interrupts::VECTOR_TIMER);
    console::print_colored(
        console::DIM,
        format_args!("       cpu{} online (LAPIC id {}), timer running, scheduler joined\n", index, info.lapic_id),
    );
    ONLINE.fetch_add(1, Ordering::SeqCst);
    sched::idle_loop();
}
