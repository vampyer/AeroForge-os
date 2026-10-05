//! Local APIC (per-CPU timer, end-of-interrupt) and I/O APIC (routing device
//! IRQs). Replaces the 8259 PIC and 8254 PIT from the first milestone; the
//! PIT is now only used once, to calibrate the LAPIC timer.
//!
//! The LAPIC timer runs in one-shot mode: the scheduler arms it for
//! whichever comes first, the next 10 ms time-slice tick or the earliest
//! sleeping thread's wake-up time, so sleeps are as precise as the timer
//! (well under a microsecond) instead of rounding to whole ticks.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::arch::{self, inb, outb};
use crate::{acpi, memory};

pub const TIMER_HZ: u64 = 100;
pub const SPURIOUS_VECTOR: u8 = 0xFF;

const REG_TPR: u64 = 0x80;
const REG_EOI: u64 = 0xB0;
const REG_SVR: u64 = 0xF0;
const REG_ICR_LOW: u64 = 0x300;
const REG_ICR_HIGH: u64 = 0x310;
const REG_LVT_TIMER: u64 = 0x320;
const REG_TIMER_INITIAL: u64 = 0x380;
const REG_TIMER_CURRENT: u64 = 0x390;
const REG_TIMER_DIVIDE: u64 = 0x3E0;

static LAPIC_BASE: AtomicU64 = AtomicU64::new(0);
static IOAPIC_BASE: AtomicU64 = AtomicU64::new(0);
/// LAPIC timer counts (divider 16) in 10 ms.
static COUNTS_PER_10MS: AtomicU32 = AtomicU32::new(0);
static TSC_PER_US: AtomicU64 = AtomicU64::new(1000);
/// TSC value at calibration, so `micros` counts from (about) boot.
static TSC_BOOT: AtomicU64 = AtomicU64::new(0);

fn read(reg: u64) -> u32 {
    unsafe { ((LAPIC_BASE.load(Ordering::Relaxed) + reg) as *const u32).read_volatile() }
}

fn write(reg: u64, v: u32) {
    unsafe { ((LAPIC_BASE.load(Ordering::Relaxed) + reg) as *mut u32).write_volatile(v) }
}

pub fn eoi() {
    write(REG_EOI, 0);
}

/// Sends a fixed interrupt to another CPU.
pub fn send_ipi(lapic_id: u32, vector: u8) {
    write(REG_ICR_HIGH, lapic_id << 24);
    write(REG_ICR_LOW, vector as u32); // fixed delivery, physical destination
    while read(REG_ICR_LOW) & (1 << 12) != 0 {
        core::hint::spin_loop(); // delivery pending
    }
}

/// Maps the LAPIC, masks the legacy PIC and calibrates the timer. BSP only.
pub fn init_bsp() -> u32 {
    let phys = unsafe { arch::rdmsr(arch::MSR_APIC_BASE) } & 0xF_FFFF_F000;
    LAPIC_BASE.store(memory::map_mmio(phys, 4096), Ordering::SeqCst);

    // Remap the 8259s away from the exception vectors, then mask them all.
    crate::pic::remap_and_mask();

    enable_local();
    let per_10ms = calibrate();
    COUNTS_PER_10MS.store(per_10ms, Ordering::SeqCst);
    per_10ms * 100 * 16 / 1_000_000 // LAPIC timer input clock in MHz (divider 16)
}

fn enable_local() {
    write(REG_TPR, 0);
    write(REG_SVR, 0x100 | SPURIOUS_VECTOR as u32);
}

/// Counts LAPIC timer ticks across 10 ms measured by PIT channel 2.
fn calibrate() -> u32 {
    unsafe {
        let gate = inb(0x61);
        outb(0x61, (gate & 0xFD) | 0x01); // gate on, speaker off
        outb(0x43, 0xB0); // channel 2, lo/hi, mode 0
        let count: u16 = 11932; // 10 ms at 1.193182 MHz
        outb(0x42, count as u8);
        outb(0x42, (count >> 8) as u8);

        write(REG_TIMER_DIVIDE, 0x3); // divide by 16
        // Restart channel 2 by toggling the gate, then start the LAPIC count.
        let g = inb(0x61) & 0xFE;
        outb(0x61, g);
        outb(0x61, g | 1);
        write(REG_TIMER_INITIAL, u32::MAX);
        let tsc0 = arch::rdtsc();
        while inb(0x61) & 0x20 == 0 {}
        let elapsed = u32::MAX - read(REG_TIMER_CURRENT);
        let tsc1 = arch::rdtsc();
        TSC_PER_US.store(((tsc1 - tsc0) / 10_000).max(1), Ordering::SeqCst);
        TSC_BOOT.store(tsc1, Ordering::SeqCst);
        write(REG_TIMER_INITIAL, 0);
        elapsed
    }
}

/// Enables this CPU's LAPIC and starts its timer (one-shot) on `vector`;
/// the first interrupt comes one tick from now.
pub fn start_timer(vector: u8) {
    enable_local();
    write(REG_TIMER_DIVIDE, 0x3);
    write(REG_LVT_TIMER, vector as u32); // one-shot
    arm_timer(1_000_000 / TIMER_HZ);
}

/// Stops this CPU's timer (a tickless idle CPU with no sleepers).
pub fn stop_timer() {
    write(REG_TIMER_INITIAL, 0);
}

/// Makes this CPU's timer interrupt fire `us` microseconds from now
/// (replacing any earlier setting). A no-op until `start_timer` has run.
pub fn arm_timer(us: u64) {
    let per_10ms = COUNTS_PER_10MS.load(Ordering::Relaxed) as u64;
    let count = (us.saturating_mul(per_10ms) / 10_000).clamp(1, u32::MAX as u64);
    write(REG_TIMER_INITIAL, count as u32);
}

// ----------------------------------------------------------------- I/O APIC

fn ioapic_write(reg: u32, v: u32) {
    let base = IOAPIC_BASE.load(Ordering::Relaxed);
    unsafe {
        (base as *mut u32).write_volatile(reg);
        ((base + 0x10) as *mut u32).write_volatile(v);
    }
}

fn ioapic_read(reg: u32) -> u32 {
    let base = IOAPIC_BASE.load(Ordering::Relaxed);
    unsafe {
        (base as *mut u32).write_volatile(reg);
        ((base + 0x10) as *const u32).read_volatile()
    }
}

/// Returns the number of redirection entries.
pub fn init_ioapic() -> Result<u32, &'static str> {
    let guard = acpi::INFO.lock();
    let info = guard.as_ref().ok_or("ACPI not initialised")?;
    if info.ioapic_address == 0 {
        return Err("no I/O APIC in MADT");
    }
    IOAPIC_BASE.store(memory::map_mmio(info.ioapic_address as u64, 4096), Ordering::SeqCst);
    let entries = ((ioapic_read(1) >> 16) & 0xFF) + 1;
    for i in 0..entries {
        ioapic_write(0x10 + 2 * i, 1 << 16); // masked
    }
    Ok(entries)
}

/// Routes ISA `irq` to `vector` on the CPU with `lapic_id`, honouring the
/// MADT's interrupt source overrides.
pub fn route_isa_irq(irq: u8, vector: u8, lapic_id: u32) {
    let guard = acpi::INFO.lock();
    let info = guard.as_ref().unwrap();
    let (gsi, flags) = info
        .overrides
        .iter()
        .find(|o| o.0 == irq)
        .map(|o| (o.1, o.2))
        .unwrap_or((irq as u32, 0));
    let pin = gsi - info.ioapic_gsi_base;
    let mut low = vector as u32;
    if flags & 0b11 == 0b11 {
        low |= 1 << 13; // active low
    }
    if (flags >> 2) & 0b11 == 0b11 {
        low |= 1 << 15; // level triggered
    }
    ioapic_write(0x10 + 2 * pin + 1, lapic_id << 24);
    ioapic_write(0x10 + 2 * pin, low);
}

/// Microseconds since boot, from the TSC (calibrated against the PIT).
/// This is the clock the scheduler's sleeps and `ticks` run on.
pub fn micros() -> u64 {
    arch::rdtsc().saturating_sub(TSC_BOOT.load(Ordering::Relaxed)) / TSC_PER_US.load(Ordering::Relaxed)
}

/// Busy-waits using the TSC.
pub fn delay_us(us: u64) {
    let end = arch::rdtsc() + us * TSC_PER_US.load(Ordering::Relaxed);
    while arch::rdtsc() < end {
        core::hint::spin_loop();
    }
}
