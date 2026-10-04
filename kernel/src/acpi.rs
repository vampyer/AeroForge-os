//! Minimal ACPI table discovery: RSDP -> XSDT/RSDT -> MADT.
//! A stopgap so the kernel knows its CPUs and interrupt controllers; the
//! design calls for porting ACPICA for anything involving AML.

use alloc::vec::Vec;
use core::ptr::read_unaligned;
use spin::Mutex;

use crate::memory;

#[repr(C, packed)]
struct Rsdp {
    signature: [u8; 8],
    checksum: u8,
    oem_id: [u8; 6],
    revision: u8,
    rsdt: u32,
    length: u32,
    xsdt: u64,
}

#[repr(C, packed)]
struct SdtHeader {
    signature: [u8; 4],
    length: u32,
    revision: u8,
    checksum: u8,
    oem_id: [u8; 6],
    oem_table_id: [u8; 8],
    oem_revision: u32,
    creator_id: u32,
    creator_revision: u32,
}

pub struct AcpiInfo {
    pub revision: u8,
    pub oem: [u8; 6],
    pub tables: Vec<[u8; 4]>,
    pub lapic_count: usize,
    pub ioapic_count: usize,
    pub lapic_address: u32,
    pub ioapic_address: u32,
    pub ioapic_gsi_base: u32,
    /// ISA IRQ -> (global system interrupt, MPS INTI flags)
    pub overrides: Vec<(u8, u32, u16)>,
}

pub static INFO: Mutex<Option<AcpiInfo>> = Mutex::new(None);

unsafe fn header(phys: u64) -> (u64, SdtHeader) {
    let v = memory::map_physical(phys, core::mem::size_of::<SdtHeader>() as u64);
    let h: SdtHeader = read_unaligned(v as *const SdtHeader);
    let v = memory::map_physical(phys, h.length as u64);
    (v, h)
}

pub fn init(rsdp_addr: u64) -> Result<(), &'static str> {
    // Older Limine revisions hand back an HHDM pointer; newer ones a physical address.
    let rsdp_phys = if rsdp_addr >= memory::hhdm_offset() { rsdp_addr - memory::hhdm_offset() } else { rsdp_addr };
    let rsdp: Rsdp = unsafe { read_unaligned(memory::map_physical(rsdp_phys, 36) as *const Rsdp) };
    if &rsdp.signature != b"RSD PTR " {
        return Err("bad RSDP signature");
    }

    let (root_phys, entry_size) = if rsdp.revision >= 2 && rsdp.xsdt != 0 {
        (rsdp.xsdt, 8)
    } else {
        (rsdp.rsdt as u64, 4)
    };

    let mut info = AcpiInfo {
        revision: rsdp.revision,
        oem: rsdp.oem_id,
        tables: Vec::new(),
        lapic_count: 0,
        ioapic_count: 0,
        lapic_address: 0,
        ioapic_address: 0,
        ioapic_gsi_base: 0,
        overrides: Vec::new(),
    };

    unsafe {
        let (root_v, root) = header(root_phys);
        let count = (root.length as usize - core::mem::size_of::<SdtHeader>()) / entry_size;
        let entries = root_v + core::mem::size_of::<SdtHeader>() as u64;
        for i in 0..count {
            let p = entries + (i * entry_size) as u64;
            let phys = if entry_size == 8 { read_unaligned(p as *const u64) } else { read_unaligned(p as *const u32) as u64 };
            let (tv, t) = header(phys);
            info.tables.push(t.signature);
            if &t.signature == b"APIC" {
                parse_madt(tv, t.length, &mut info);
            }
        }
    }

    *INFO.lock() = Some(info);
    Ok(())
}

unsafe fn parse_madt(v: u64, len: u32, info: &mut AcpiInfo) {
    let base = v + core::mem::size_of::<SdtHeader>() as u64;
    info.lapic_address = read_unaligned(base as *const u32);
    let mut p = base + 8;
    let end = v + len as u64;
    while p + 2 <= end {
        let kind = *(p as *const u8);
        let entry_len = *((p + 1) as *const u8) as u64;
        if entry_len < 2 {
            break;
        }
        match kind {
            0 => {
                let flags = read_unaligned((p + 4) as *const u32);
                if flags & 0b11 != 0 {
                    info.lapic_count += 1; // enabled or online-capable
                }
            }
            1 => {
                if info.ioapic_count == 0 {
                    info.ioapic_address = read_unaligned((p + 4) as *const u32);
                    info.ioapic_gsi_base = read_unaligned((p + 8) as *const u32);
                }
                info.ioapic_count += 1;
            }
            2 => {
                let irq = *((p + 3) as *const u8);
                let gsi = read_unaligned((p + 4) as *const u32);
                let flags = read_unaligned((p + 8) as *const u16);
                info.overrides.push((irq, gsi, flags));
            }
            9 => info.lapic_count += 1, // x2APIC entry
            _ => {}
        }
        p += entry_len;
    }
}
