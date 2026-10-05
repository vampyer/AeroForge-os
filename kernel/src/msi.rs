//! Message-signalled interrupts: a PCIe device raises an interrupt by
//! writing a vector number to the local APIC's address, so no I/O APIC pin
//! or legacy INTx line is involved. MSI-X is preferred (its table lives in a
//! BAR); plain MSI is the fallback. Each device gets its own vector from
//! 0x40-0xEF, and its handler runs from the interrupt dispatcher.

use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::pci::Device;
use crate::sync::IrqMutex;
use crate::{apic, memory, percpu};

const FIRST: u8 = 0x40;
const LAST: u8 = 0xEF;
const CAP_MSI: u8 = 0x05;
const CAP_MSIX: u8 = 0x11;

/// Handler per vector (a `fn()` as usize, 0 = none) and how often it ran.
static HANDLERS: [AtomicUsize; 256] = [const { AtomicUsize::new(0) }; 256];
static COUNTS: [AtomicU64; 256] = [const { AtomicU64::new(0) }; 256];
static NEXT: AtomicUsize = AtomicUsize::new(FIRST as usize);

pub struct Source {
    pub vector: u8,
    pub name: String,
    pub kind: &'static str,
    pub cpu: usize,
}

static SOURCES: IrqMutex<Vec<Source>> = IrqMutex::new(Vec::new());

/// Called by the interrupt dispatcher for vectors 0x40-0xEF. Returns false if
/// no device owns the vector.
pub fn dispatch(vector: u8) -> bool {
    let h = HANDLERS[vector as usize].load(Ordering::Acquire);
    if h == 0 {
        return false;
    }
    COUNTS[vector as usize].fetch_add(1, Ordering::Relaxed);
    let f: fn() = unsafe { core::mem::transmute(h) };
    f();
    apic::eoi();
    true
}

/// Gives `dev` a vector that runs `handler` on CPU `cpu`, through MSI-X or
/// MSI. Returns the kind used, or None if the device can do neither (it then
/// stays polled).
pub fn attach(dev: &Device, name: &str, handler: fn(), cpu: usize) -> Option<&'static str> {
    let lapic = percpu::get(cpu)?.lapic_id;
    let vector = NEXT.fetch_add(1, Ordering::SeqCst);
    if vector > LAST as usize {
        return None;
    }
    let vector = vector as u8;
    HANDLERS[vector as usize].store(handler as *const () as usize, Ordering::Release);
    // Fixed delivery, physical destination, edge triggered.
    let address = 0xFEE0_0000u64 | ((lapic as u64) << 12);
    let kind = if enable_msix(dev, address, vector) {
        "MSI-X"
    } else if enable_msi(dev, address, vector) {
        "MSI"
    } else {
        HANDLERS[vector as usize].store(0, Ordering::Release);
        return None;
    };
    SOURCES.lock().push(Source { vector, name: String::from(name), kind, cpu });
    Some(kind)
}

/// Uses table entry 0 and masks the rest.
fn enable_msix(dev: &Device, address: u64, vector: u8) -> bool {
    let Some(cap) = dev.capability(CAP_MSIX) else { return false };
    let control = dev.read16(cap + 2);
    let entries = (control & 0x7FF) as u64 + 1;
    let table = dev.read32(cap + 4);
    let Some(bar) = dev.bar((table & 7) as u64) else { return false };
    let base = memory::map_mmio(bar + (table & !7) as u64, entries * 16);
    // Enable with the whole function masked while the table is written.
    dev.write16(cap + 2, control | (1 << 15) | (1 << 14));
    unsafe {
        for i in 0..entries {
            let e = (base + i * 16) as *mut u32;
            e.add(3).write_volatile(1); // masked
        }
        let e = base as *mut u32;
        e.write_volatile(address as u32);
        e.add(1).write_volatile((address >> 32) as u32);
        e.add(2).write_volatile(vector as u32);
        e.add(3).write_volatile(0); // unmasked
    }
    dev.write16(cap + 2, (control | (1 << 15)) & !(1 << 14));
    true
}

fn enable_msi(dev: &Device, address: u64, vector: u8) -> bool {
    let Some(cap) = dev.capability(CAP_MSI) else { return false };
    let control = dev.read16(cap + 2);
    dev.write32(cap + 4, address as u32);
    let data_at = if control & (1 << 7) != 0 {
        dev.write32(cap + 8, (address >> 32) as u32);
        cap + 12
    } else {
        cap + 8
    };
    dev.write16(data_at, vector as u16);
    // One message (bits 4-6 = 0), enabled.
    dev.write16(cap + 2, (control & !(0b111 << 4)) | 1);
    true
}

/// Every attached source with its interrupt count, for the `irq` command.
pub fn sources() -> Vec<(u8, String, &'static str, usize, u64)> {
    SOURCES.lock().iter().map(|s| (s.vector, s.name.clone(), s.kind, s.cpu, COUNTS[s.vector as usize].load(Ordering::Relaxed))).collect()
}
