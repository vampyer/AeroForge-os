//! Multi-core bring-up. Limine already parked every application processor
//! (AP) in 64-bit mode; we release each one by writing its entry point.

use core::sync::atomic::{AtomicU32, Ordering};

use crate::limine::{MpInfo, MpResponse};
use crate::{arch, console, interrupts};

pub static ONLINE: AtomicU32 = AtomicU32::new(1); // the boot CPU

pub fn start_aps(mp: &MpResponse) -> u32 {
    let mut started = 0;
    for cpu in mp.cpus() {
        if cpu.lapic_id == mp.bsp_lapic_id {
            continue;
        }
        cpu.goto_address.store(ap_entry as *const () as u64, Ordering::SeqCst);
        started += 1;
    }

    // Wait for them to check in (TSC-bounded, so a stuck AP can't hang boot).
    let deadline = arch::rdtsc() + 4_000_000_000;
    while ONLINE.load(Ordering::SeqCst) < 1 + started && arch::rdtsc() < deadline {
        core::hint::spin_loop();
    }
    ONLINE.load(Ordering::SeqCst)
}

extern "C" fn ap_entry(info: &MpInfo) -> ! {
    interrupts::load();
    console::print_colored(
        console::DIM,
        format_args!("       cpu{} online (LAPIC id {})\n", info.processor_id, info.lapic_id),
    );
    // Check in only after logging, so the boot CPU's summary prints last.
    ONLINE.fetch_add(1, Ordering::SeqCst);
    // No scheduler yet: APs idle until per-CPU run queues exist.
    arch::halt_forever();
}
