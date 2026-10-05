//! PCI Express enumeration through ECAM (memory-mapped configuration space,
//! located by the ACPI MCFG table).

use alloc::vec::Vec;
use spin::Once;

use crate::{acpi, memory};

#[derive(Clone, Copy, Debug)]
pub struct Device {
    pub bus: u8,
    pub dev: u8,
    pub func: u8,
    pub vendor: u16,
    pub device: u16,
    pub class: u8,
    pub subclass: u8,
    pub prog_if: u8,
    config: u64, // virtual address of this function's 4 KiB config space
}

static DEVICES: Once<Vec<Device>> = Once::new();

impl Device {
    pub fn read32(&self, off: u64) -> u32 {
        unsafe { ((self.config + off) as *const u32).read_volatile() }
    }

    pub fn write32(&self, off: u64, v: u32) {
        unsafe { ((self.config + off) as *mut u32).write_volatile(v) }
    }

    /// Physical address of a memory BAR (handles 64-bit BARs).
    pub fn bar(&self, index: u64) -> Option<u64> {
        let lo = self.read32(0x10 + index * 4);
        if lo & 1 != 0 {
            return None; // I/O space BAR
        }
        let mut addr = (lo & 0xFFFF_FFF0) as u64;
        if (lo >> 1) & 0b11 == 0b10 {
            addr |= (self.read32(0x14 + index * 4) as u64) << 32;
        }
        (addr != 0).then_some(addr)
    }

    pub fn read16(&self, off: u64) -> u16 {
        (self.read32(off & !3) >> ((off & 2) * 8)) as u16
    }

    pub fn write16(&self, off: u64, v: u16) {
        let shift = (off & 2) * 8;
        let old = self.read32(off & !3) & !(0xFFFF << shift);
        self.write32(off & !3, old | ((v as u32) << shift));
    }

    /// Config-space offset of capability `id` (0x05 MSI, 0x11 MSI-X, ...).
    pub fn capability(&self, id: u8) -> Option<u64> {
        if self.read16(0x06) & (1 << 4) == 0 {
            return None; // no capability list
        }
        let mut off = (self.read32(0x34) & 0xFC) as u64;
        for _ in 0..48 {
            if off < 0x40 {
                return None;
            }
            let header = self.read16(off);
            if header as u8 == id {
                return Some(off);
            }
            off = ((header >> 8) & 0xFC) as u64;
        }
        None
    }

    /// Config-space offsets of every capability `id` (vendor-specific
    /// capabilities, 0x09, can appear several times).
    pub fn capabilities(&self, id: u8) -> alloc::vec::Vec<u64> {
        let mut found = alloc::vec::Vec::new();
        if self.read16(0x06) & (1 << 4) == 0 {
            return found;
        }
        let mut off = (self.read32(0x34) & 0xFC) as u64;
        for _ in 0..48 {
            if off < 0x40 {
                break;
            }
            let header = self.read16(off);
            if header as u8 == id {
                found.push(off);
            }
            off = ((header >> 8) & 0xFC) as u64;
        }
        found
    }

    /// The register regions of a virtio 1.x ("modern") device.
    pub fn virtio_regions(&self) -> Option<crate::dhi::VirtioPci> {
        let mut r = crate::dhi::VirtioPci::default();
        let mut seen = 0u8;
        for cap in self.capabilities(0x09) {
            let kind = (self.read32(cap) >> 24) as u8;
            if !(1..=4).contains(&kind) {
                continue; // PCI config access, shared memory, ...
            }
            let bar = self.read32(cap + 4) as u8 as u64;
            let addr = self.bar(bar)? + self.read32(cap + 8) as u64;
            match kind {
                1 => r.common = addr,
                2 => {
                    r.notify = addr;
                    r.notify_mult = self.read32(cap + 16);
                }
                3 => r.isr = addr,
                _ => r.device = addr,
            }
            seen |= 1 << kind;
        }
        (seen & 0b110 == 0b110).then_some(r)
    }

    /// Lets the device decode memory accesses and master DMA; masks legacy INTx.
    pub fn enable_mmio_and_dma(&self) {
        let cmd = self.read32(0x04);
        self.write32(0x04, (cmd & 0xFFFF_0000) | (cmd & 0xFFFF) | 0b110 | (1 << 10));
    }

    pub fn kind(&self) -> &'static str {
        match (self.class, self.subclass, self.prog_if) {
            (0x01, 0x08, 0x02) => "NVMe controller",
            (0x01, 0x06, 0x01) => "SATA controller (AHCI)",
            (0x01, 0x01, _) => "IDE controller",
            (0x0C, 0x03, 0x30) => "USB 3 controller (xHCI)",
            (0x0C, 0x03, 0x20) => "USB 2 controller (EHCI)",
            (0x0C, 0x03, _) => "USB controller",
            (0x0C, 0x05, _) => "SMBus controller",
            (0x02, 0x00, _) => "Ethernet controller",
            (0x03, 0x00, _) => "VGA display controller",
            (0x03, 0x80, _) if self.vendor == 0x1AF4 => "Display controller (virtio-gpu)",
            (0x03, _, _) => "Display controller",
            (0x04, 0x03, _) => "Audio device (HDA)",
            (0x06, 0x00, _) => "Host bridge",
            (0x06, 0x01, _) => "ISA bridge",
            (0x06, 0x04, _) => "PCI bridge",
            (0x06, _, _) => "Bridge",
            (0x0D, 0x11, _) => "Bluetooth controller",
            _ => "Other device",
        }
    }
}

fn function_config(base: u64, bus: u8, dev: u8, func: u8) -> u64 {
    let phys = base + ((bus as u64) << 20 | (dev as u64) << 15 | (func as u64) << 12);
    memory::map_mmio(phys, 4096)
}

/// Scans every bus in the ECAM window. Returns the number of functions found.
pub fn init() -> Result<usize, &'static str> {
    let (base, first, last) = acpi::INFO.lock().as_ref().and_then(|i| i.ecam).ok_or("no MCFG table")?;
    let list = DEVICES.call_once(|| {
        let mut found = Vec::new();
        for bus in first..=last {
            for dev in 0..32u8 {
                for func in 0..8u8 {
                    let config = function_config(base, bus, dev, func);
                    let id = unsafe { (config as *const u32).read_volatile() };
                    if id & 0xFFFF == 0xFFFF {
                        if func == 0 {
                            break; // no device in this slot
                        }
                        continue;
                    }
                    let class = unsafe { ((config + 8) as *const u32).read_volatile() };
                    found.push(Device {
                        bus, dev, func,
                        vendor: id as u16,
                        device: (id >> 16) as u16,
                        class: (class >> 24) as u8,
                        subclass: (class >> 16) as u8,
                        prog_if: (class >> 8) as u8,
                        config,
                    });
                    let header = unsafe { ((config + 0x0C) as *const u32).read_volatile() } >> 16;
                    if func == 0 && header & 0x80 == 0 {
                        break; // single-function device
                    }
                }
            }
        }
        found
    });
    Ok(list.len())
}

pub fn devices() -> &'static [Device] {
    DEVICES.get().map_or(&[], |v| v.as_slice())
}
