//! Block devices (disks and partitions) and partition table parsing.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use crate::dhi::{self, BlockInfo, DmaBuf};
use crate::sync::IrqMutex;
use crate::{console, pci};

pub trait BlockDevice: Send + Sync {
    fn name(&self) -> &str;
    fn block_size(&self) -> u32;
    /// Reads whole blocks starting at `lba`; `buf.len()` must be a multiple of the block size.
    fn read(&self, lba: u64, buf: &mut [u8]) -> Result<(), &'static str>;
    fn describe(&self) -> String;
}

pub static DEVICES: IrqMutex<Vec<Arc<dyn BlockDevice>>> = IrqMutex::new(Vec::new());

// ------------------------------------------------------ driver-backed disks

/// Which C++ driver (and which of its disks) backs a `DriverDisk`.
#[derive(Clone, Copy)]
enum Backend {
    Nvme(i32),
    Ahci(i32),
    Usb(i32),
}

/// A whole disk served by one of the C++ storage drivers.
pub struct DriverDisk {
    name: String,
    backend: Backend,
    block_size: u32,
    block_count: u64,
    max_transfer: u32,
    model: String,
    serial: String,
    bounce: IrqMutex<DmaBuf>,
}

unsafe impl Send for DriverDisk {}
unsafe impl Sync for DriverDisk {}

impl BlockDevice for DriverDisk {
    fn name(&self) -> &str {
        &self.name
    }
    fn block_size(&self) -> u32 {
        self.block_size
    }
    fn read(&self, mut lba: u64, buf: &mut [u8]) -> Result<(), &'static str> {
        let bs = self.block_size as usize;
        if buf.len() % bs != 0 {
            return Err("read size is not a multiple of the block size");
        }
        // The controller DMAs into a kernel bounce buffer, then we copy out.
        let bounce = self.bounce.lock();
        for chunk in buf.chunks_mut(self.max_transfer as usize) {
            let blocks = (chunk.len() / bs) as u32;
            let rc = unsafe {
                match self.backend {
                    Backend::Nvme(c) => dhi::aero_nvme_read(c, lba, blocks, bounce.phys),
                    Backend::Ahci(d) => dhi::aero_ahci_read(d, lba, blocks, bounce.phys),
                    Backend::Usb(d) => dhi::aero_xhci_read(d, lba, blocks, bounce.phys),
                }
            };
            if rc != 0 {
                return Err("disk read failed");
            }
            unsafe { core::ptr::copy_nonoverlapping(bounce.virt, chunk.as_mut_ptr(), chunk.len()) };
            lba += blocks as u64;
        }
        Ok(())
    }
    fn describe(&self) -> String {
        let bus = match self.backend {
            Backend::Nvme(_) => "NVMe",
            Backend::Ahci(_) => "SATA",
            Backend::Usb(_) => "USB",
        };
        format!("{} sectors x {} B = {} MiB, {} \"{}\" serial {}",
            self.block_count, self.block_size, self.block_count * self.block_size as u64 / (1024 * 1024),
            bus, self.model, self.serial)
    }
}

fn add_disk(name: String, backend: Backend, info: &BlockInfo) -> bool {
    let mut bounce = DmaBuf { phys: 0, virt: core::ptr::null_mut(), size: 0 };
    if (dhi::OPS.dma_alloc)(info.max_transfer as u64, &mut bounce) != 0 {
        return false;
    }
    register_with_partitions(Arc::new(DriverDisk {
        name,
        backend,
        block_size: info.block_size,
        block_count: info.block_count,
        max_transfer: info.max_transfer,
        model: String::from(dhi::c_field(&info.model)),
        serial: String::from(dhi::c_field(&info.serial)),
        bounce: IrqMutex::new(bounce),
    }));
    true
}

/// Finds NVMe controllers on PCIe and brings each one up through the C++ driver.
pub fn probe_nvme() -> usize {
    let mut found = 0;
    for dev in pci::devices().iter().filter(|d| d.class == 0x01 && d.subclass == 0x08 && d.prog_if == 0x02) {
        let Some(bar0) = dev.bar(0) else { continue };
        dev.enable_mmio_and_dma();
        let mut info = BlockInfo::zeroed();
        let ctrl = unsafe { dhi::aero_nvme_init(&dhi::OPS, bar0, &mut info) };
        if ctrl < 0 {
            console::print_colored(console::YELLOW, format_args!("[WARN] NVMe at {:02x}:{:02x}.{}: init failed ({})\n", dev.bus, dev.dev, dev.func, ctrl));
            continue;
        }
        if add_disk(format!("nvme{}", ctrl), Backend::Nvme(ctrl), &info) {
            found += 1;
        }
    }
    found
}

/// Finds AHCI (SATA) controllers and registers every ATA disk attached to them.
pub fn probe_ahci() -> usize {
    const MAX: usize = 8;
    let mut found = 0;
    for dev in pci::devices().iter().filter(|d| d.class == 0x01 && d.subclass == 0x06 && d.prog_if == 0x01) {
        let Some(abar) = dev.bar(5) else { continue };
        dev.enable_mmio_and_dma();
        let mut infos = [BlockInfo::zeroed(); MAX];
        let mut first = 0i32;
        let n = unsafe { dhi::aero_ahci_init(&dhi::OPS, abar, infos.as_mut_ptr(), MAX as i32, &mut first) };
        if n < 0 {
            console::print_colored(console::YELLOW, format_args!("[WARN] AHCI at {:02x}:{:02x}.{}: init failed ({})\n", dev.bus, dev.dev, dev.func, n));
            continue;
        }
        for (i, info) in infos.iter().take(n as usize).enumerate() {
            let id = first + i as i32;
            if add_disk(format!("sata{}", id), Backend::Ahci(id), info) {
                found += 1;
            }
        }
    }
    found
}

/// Registers the USB sticks and drives the xHCI driver found (call after usb::probe).
pub fn probe_usb() -> usize {
    let mut found = 0;
    let ids: Vec<i32> = crate::usb::CONTROLLERS.lock().iter().map(|c| c.id).collect();
    for ctrl in ids {
        for index in 0.. {
            let mut info = BlockInfo::zeroed();
            let disk = unsafe { dhi::aero_xhci_disk(ctrl, index, &mut info) };
            if disk < 0 {
                break;
            }
            if add_disk(format!("usb{}", found), Backend::Usb(disk), &info) {
                found += 1;
            }
        }
    }
    found
}

// ------------------------------------------------------------- partitions

pub struct Partition {
    name: String,
    disk: Arc<dyn BlockDevice>,
    start: u64,
    count: u64,
    label: String,
    scheme: &'static str,
}

impl BlockDevice for Partition {
    fn name(&self) -> &str {
        &self.name
    }
    fn block_size(&self) -> u32 {
        self.disk.block_size()
    }
    fn read(&self, lba: u64, buf: &mut [u8]) -> Result<(), &'static str> {
        let blocks = buf.len() as u64 / self.block_size() as u64;
        if lba + blocks > self.count {
            return Err("read past end of partition");
        }
        self.disk.read(self.start + lba, buf)
    }
    fn describe(&self) -> String {
        format!("{} sectors from LBA {} = {} MiB, {} partition{}",
            self.count, self.start, self.count * self.block_size() as u64 / (1024 * 1024), self.scheme,
            if self.label.is_empty() { String::new() } else { format!(" \"{}\"", self.label) })
    }
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn u64_at(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

fn register_with_partitions(disk: Arc<dyn BlockDevice>) {
    DEVICES.lock().push(disk.clone());
    let parts = scan_gpt(&disk).or_else(|| scan_mbr(&disk)).unwrap_or_default();
    let mut list = DEVICES.lock();
    for p in parts {
        list.push(Arc::new(p));
    }
}

fn read_block(disk: &Arc<dyn BlockDevice>, lba: u64) -> Option<Vec<u8>> {
    let mut b = vec![0u8; disk.block_size() as usize];
    disk.read(lba, &mut b).ok()?;
    Some(b)
}

fn scan_gpt(disk: &Arc<dyn BlockDevice>) -> Option<Vec<Partition>> {
    let hdr = read_block(disk, 1)?;
    if &hdr[..8] != b"EFI PART" {
        return None;
    }
    let entries_lba = u64_at(&hdr, 72);
    let count = u32_at(&hdr, 80).min(128) as usize;
    let size = u32_at(&hdr, 84) as usize;
    let bs = disk.block_size() as usize;
    let mut table = vec![0u8; (count * size).div_ceil(bs) * bs];
    disk.read(entries_lba, &mut table).ok()?;

    let mut parts = Vec::new();
    for i in 0..count {
        let e = &table[i * size..(i + 1) * size];
        if e[..16].iter().all(|&b| b == 0) {
            continue;
        }
        let first = u64_at(e, 32);
        let last = u64_at(e, 40);
        let label: String = char::decode_utf16(
            e[56..128].chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&c| c != 0),
        )
        .map(|c| c.unwrap_or('?'))
        .collect();
        parts.push(Partition {
            name: format!("{}p{}", disk.name(), parts.len() + 1),
            disk: disk.clone(),
            start: first,
            count: last - first + 1,
            label,
            scheme: "GPT",
        });
    }
    Some(parts)
}

fn scan_mbr(disk: &Arc<dyn BlockDevice>) -> Option<Vec<Partition>> {
    let b = read_block(disk, 0)?;
    if b[510] != 0x55 || b[511] != 0xAA {
        return None;
    }
    let mut parts = Vec::new();
    for i in 0..4 {
        let e = &b[0x1BE + i * 16..0x1BE + (i + 1) * 16];
        let kind = e[4];
        let start = u32_at(e, 8) as u64;
        let count = u32_at(e, 12) as u64;
        if kind == 0 || kind == 0xEE || count == 0 {
            continue;
        }
        parts.push(Partition {
            name: format!("{}p{}", disk.name(), i + 1),
            disk: disk.clone(),
            start,
            count,
            label: String::new(),
            scheme: "MBR",
        });
    }
    (!parts.is_empty()).then_some(parts)
}
