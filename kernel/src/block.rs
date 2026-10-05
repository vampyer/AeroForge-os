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
    fn block_count(&self) -> u64;
    /// For a partition: its first block and block count on the whole disk.
    fn extent(&self) -> Option<(u64, u64)> {
        None
    }
    /// Reads whole blocks starting at `lba`; `buf.len()` must be a multiple of the block size.
    fn read(&self, lba: u64, buf: &mut [u8]) -> Result<(), &'static str>;
    /// Writes whole blocks starting at `lba`; `buf.len()` must be a multiple of the block size.
    /// The data may stay in the drive's cache until `flush`.
    fn write(&self, lba: u64, buf: &[u8]) -> Result<(), &'static str>;
    /// Commits the drive's write cache to media.
    fn flush(&self) -> Result<(), &'static str>;
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

impl Drop for DriverDisk {
    fn drop(&mut self) {
        (dhi::OPS.dma_free)(&*self.bounce.lock());
    }
}

impl BlockDevice for DriverDisk {
    fn name(&self) -> &str {
        &self.name
    }
    fn block_size(&self) -> u32 {
        self.block_size
    }
    fn block_count(&self) -> u64 {
        self.block_count
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
    fn write(&self, mut lba: u64, buf: &[u8]) -> Result<(), &'static str> {
        let bs = self.block_size as usize;
        if buf.len() % bs != 0 {
            return Err("write size is not a multiple of the block size");
        }
        if lba + (buf.len() / bs) as u64 > self.block_count {
            return Err("write past end of disk");
        }
        // Copy into the bounce buffer, then the controller DMAs from it.
        let bounce = self.bounce.lock();
        for chunk in buf.chunks(self.max_transfer as usize) {
            let blocks = (chunk.len() / bs) as u32;
            unsafe { core::ptr::copy_nonoverlapping(chunk.as_ptr(), bounce.virt, chunk.len()) };
            let rc = unsafe {
                match self.backend {
                    Backend::Nvme(c) => dhi::aero_nvme_write(c, lba, blocks, bounce.phys),
                    Backend::Ahci(d) => dhi::aero_ahci_write(d, lba, blocks, bounce.phys),
                    Backend::Usb(d) => dhi::aero_xhci_write(d, lba, blocks, bounce.phys),
                }
            };
            if rc != 0 {
                return Err("disk write failed");
            }
            lba += blocks as u64;
        }
        Ok(())
    }
    fn flush(&self) -> Result<(), &'static str> {
        let _bounce = self.bounce.lock();  // one command at a time per disk
        let rc = unsafe {
            match self.backend {
                Backend::Nvme(c) => dhi::aero_nvme_flush(c),
                Backend::Ahci(d) => dhi::aero_ahci_flush(d),
                Backend::Usb(d) => dhi::aero_xhci_flush(d),
            }
        };
        if rc == 0 { Ok(()) } else { Err("disk flush failed") }
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

/// A USB disk the kernel registered, to tell when it is unplugged.
struct UsbDisk {
    name: String,
    ctrl: i32,
    id: i32,
    block_count: u64,
    serial: String,
}

static USB_DISKS: IrqMutex<Vec<UsbDisk>> = IrqMutex::new(Vec::new());

/// The USB disks the controller has now: (disk id, info).
fn usb_disks_of(ctrl: i32) -> Vec<(i32, BlockInfo)> {
    let mut disks = Vec::new();
    for index in 0.. {
        let mut info = BlockInfo::zeroed();
        let id = unsafe { dhi::aero_xhci_disk(ctrl, index, &mut info) };
        if id < 0 {
            break;
        }
        disks.push((id, info));
    }
    disks
}

fn add_usb(ctrl: i32, id: i32, info: &BlockInfo) -> Option<String> {
    let name = {
        let disks = USB_DISKS.lock();
        let n = (0..).find(|n| !disks.iter().any(|d| d.name == format!("usb{}", n))).unwrap_or(0);
        format!("usb{}", n)
    };
    if !add_disk(name.clone(), Backend::Usb(id), info) {
        return None;
    }
    USB_DISKS.lock().push(UsbDisk { name: name.clone(), ctrl, id, block_count: info.block_count,
        serial: String::from(dhi::c_field(&info.serial)) });
    Some(name)
}

/// Registers the USB sticks and drives the xHCI driver found (call after usb::probe).
pub fn probe_usb() -> usize {
    let mut found = 0;
    let ids: Vec<i32> = crate::usb::CONTROLLERS.lock().iter().map(|c| c.id).collect();
    for ctrl in ids {
        for (id, info) in usb_disks_of(ctrl) {
            if add_usb(ctrl, id, &info).is_some() {
                found += 1;
            }
        }
    }
    found
}

/// True if block device `dev` is disk `disk` or one of its partitions.
pub fn on_disk(dev: &str, disk: &str) -> bool {
    dev == disk || dev.strip_prefix(disk).is_some_and(|rest| rest.starts_with('p'))
}

/// Brings the controller's USB disks up to date after something was plugged
/// in or unplugged: unplugged disks are unmounted and forgotten, new ones
/// registered and their FAT32 and exFAT volumes mounted.
pub fn sync_usb(ctrl: i32) {
    let now = usb_disks_of(ctrl);
    let gone: Vec<String> = USB_DISKS.lock().iter()
        .filter(|d| d.ctrl == ctrl && !now.iter().any(|(id, info)| *id == d.id && info.block_count == d.block_count
            && dhi::c_field(&info.serial) == d.serial))
        .map(|d| d.name.clone())
        .collect();
    for name in gone {
        let unmounted = crate::vfs::unmount_disk(&name);
        DEVICES.lock().retain(|d| !on_disk(d.name(), &name));
        USB_DISKS.lock().retain(|d| d.name != name);
        if unmounted.is_empty() {
            crate::kok!("USB disk {} unplugged", name);
        } else {
            crate::kok!("USB disk {} unplugged, {} unmounted", name, unmounted.join(", "));
        }
    }
    for (id, info) in now {
        if USB_DISKS.lock().iter().any(|d| d.ctrl == ctrl && d.id == id) {
            continue;
        }
        let Some(name) = add_usb(ctrl, id, &info) else { continue };
        let devices: Vec<_> = DEVICES.lock().iter().filter(|d| on_disk(d.name(), &name)).cloned().collect();
        for d in &devices {
            if d.name() == name {
                crate::kok!("USB disk {} plugged in: {}", name, d.describe());
            }
        }
        for d in devices {
            if let Some(m) = crate::vfs::mount(d) {
                crate::kok!("{} volume \"{}\" on {} mounted at {}{}", m.vol.kind(), m.vol.label(), m.vol.dev().name(), m.path,
            if m.vol.read_only() { " (read-only)" } else { "" });
            }
        }
    }
}

/// The test pattern `write_test` puts in block `lba` of `dev`; the boot test
/// checks the disk image for it after QEMU exits (tools/check-disk-write.py).
fn test_block(dev: &str, lba: u64, bs: usize) -> Vec<u8> {
    let mut b: Vec<u8> = (0..bs).map(|i| (i as u64 * 7 + lba) as u8).collect();
    let head = format!("AeroForge write test {} LBA {}\n", dev, lba);
    b[..head.len()].copy_from_slice(head.as_bytes());
    b
}

/// Writes a test pattern to `count` blocks of whole disk `dev` from `lba`,
/// flushes, and reads it back. Only the unused gap between the partition
/// tables and the partitions may be written, so no data can be harmed.
pub fn write_test(dev: &str, lba: u64, count: u64) -> Result<String, &'static str> {
    let devices: Vec<_> = DEVICES.lock().iter().cloned().collect();
    let disk = devices.iter().find(|d| d.name() == dev).ok_or("no such disk")?;
    if disk.extent().is_some() {
        return Err("give a whole disk, not a partition");
    }
    // LBA 0-33 hold the MBR or GPT and its entries; the last 33 the backup GPT.
    if count == 0 || count > 1024 || lba < 34 || lba + count > disk.block_count().saturating_sub(33) {
        return Err("blocks outside the area between the partition tables");
    }
    let overlaps = devices.iter().filter(|p| on_disk(p.name(), dev)).filter_map(|p| p.extent())
        .any(|(start, n)| lba < start + n && start < lba + count);
    if overlaps {
        return Err("blocks overlap a partition");
    }
    let bs = disk.block_size() as usize;
    let mut data = Vec::with_capacity(bs * count as usize);
    for i in 0..count {
        data.extend_from_slice(&test_block(dev, lba + i, bs));
    }
    disk.write(lba, &data)?;
    disk.flush()?;
    let mut back = vec![0u8; data.len()];
    disk.read(lba, &mut back)?;
    if back != data {
        return Err("read back different data");
    }
    Ok(format!("wrote {} blocks to {} at LBA {}, flushed, read back OK", count, dev, lba))
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
    fn block_count(&self) -> u64 {
        self.count
    }
    fn extent(&self) -> Option<(u64, u64)> {
        Some((self.start, self.count))
    }
    fn read(&self, lba: u64, buf: &mut [u8]) -> Result<(), &'static str> {
        let blocks = buf.len() as u64 / self.block_size() as u64;
        if lba + blocks > self.count {
            return Err("read past end of partition");
        }
        self.disk.read(self.start + lba, buf)
    }
    fn write(&self, lba: u64, buf: &[u8]) -> Result<(), &'static str> {
        let blocks = buf.len() as u64 / self.block_size() as u64;
        if lba + blocks > self.count {
            return Err("write past end of partition");
        }
        self.disk.write(self.start + lba, buf)
    }
    fn flush(&self) -> Result<(), &'static str> {
        self.disk.flush()
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
