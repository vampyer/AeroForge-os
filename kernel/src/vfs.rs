//! The (very) early file namespace. Every FAT32 and exFAT volume is
//! mounted readable and writable, and NTFS volumes read-only: the first
//! writable one found at boot at "/", each further one at
//! "/<device name>" (for example "/sata0p1"). Volumes on USB disks plugged
//! in later are mounted the same way and go away when the disk is
//! unplugged. Real mount tables, more filesystems and user-space filesystem
//! servers come with Phase 2.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::block::{BlockDevice, DEVICES};
use crate::exfat::ExfatVolume;
use crate::fat::FatVolume;
use crate::ntfs::NtfsVolume;
use crate::sync::IrqMutex;

#[derive(Clone, Debug)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    /// When it was last written, as `rtc::DateTime::packed` (0: unknown).
    pub modified: u64,
}

impl DirEntry {
    /// A directory entry standing for another volume's mount point.
    pub fn mount_point(name: &str) -> Self {
        Self { name: String::from(name), is_dir: true, size: 0, modified: 0 }
    }
}

/// A mounted filesystem. Paths are inside the volume, "/"-separated.
pub trait Volume: Send + Sync {
    /// "FAT32", "exFAT".
    fn kind(&self) -> &'static str;
    fn label(&self) -> &str;
    fn dev(&self) -> &Arc<dyn BlockDevice>;
    fn read_only(&self) -> bool {
        false
    }
    /// The volume's size and how much of it is free, in bytes (free is None
    /// when the volume does not keep count).
    fn space(&self) -> (u64, Option<u64>) {
        (self.dev().block_count() * self.dev().block_size() as u64, None)
    }
    fn list(&self, path: &str) -> Result<Vec<DirEntry>, &'static str>;
    /// Reads up to `limit` bytes of a file.
    fn read_file(&self, path: &str, limit: usize) -> Result<Vec<u8>, &'static str>;
    /// Creates the file, or replaces what it holds.
    fn write_file(&self, path: &str, data: &[u8]) -> Result<(), &'static str>;
    fn create_dir(&self, path: &str) -> Result<(), &'static str>;
    /// Deletes a file or an empty directory.
    fn remove(&self, path: &str) -> Result<(), &'static str>;
    /// Turns writing on or off, for volumes written to only when asked
    /// (NTFS); Err says why it can't be.
    fn set_writable(&self, _on: bool) -> Result<(), &'static str> {
        Err("this kind of volume is always writable")
    }
    /// Deleted entries that can still be seen in directory `path`.
    fn deleted(&self, _path: &str) -> Result<Vec<Deleted>, &'static str> {
        Err("not supported on this kind of volume")
    }
    /// Brings back deleted entry `slot` of directory `path`; returns its name.
    fn undelete(&self, _path: &str, _slot: u32) -> Result<String, &'static str> {
        Err("not supported on this kind of volume")
    }
}

/// A deleted file or directory still listed in its directory.
#[derive(Clone, Debug)]
pub struct Deleted {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified: u64,
    /// Where it is in its directory, to ask for it back.
    pub slot: u32,
    /// Whether its data is all still there (not written over).
    pub whole: bool,
}

pub struct Mount {
    /// "/" for the root volume, "/<device>" for the others.
    pub path: String,
    pub vol: alloc::boxed::Box<dyn Volume>,
}

/// Root first. Lookups clone the Arc and drop the lock before touching the disk.
static MOUNTS: IrqMutex<Vec<Arc<Mount>>> = IrqMutex::new(Vec::new());

/// Mounts every FAT32 and exFAT volume found at boot. Returns the mounts made, root first.
pub fn mount_all() -> Vec<Arc<Mount>> {
    let devices: Vec<_> = DEVICES.lock().iter().cloned().collect();
    for dev in devices {
        mount(dev);
    }
    MOUNTS.lock().clone()
}

/// Mounts `dev` if it holds a FAT32, exFAT or NTFS volume and is not mounted yet.
pub fn mount(dev: Arc<dyn BlockDevice>) -> Option<Arc<Mount>> {
    if mount_point_of(dev.name()).is_some() {
        return None;
    }
    let vol: alloc::boxed::Box<dyn Volume> = if let Ok(v) = FatVolume::mount(dev.clone()) {
        alloc::boxed::Box::new(v)
    } else if let Ok(v) = ExfatVolume::mount(dev.clone()) {
        alloc::boxed::Box::new(v)
    } else {
        match NtfsVolume::mount(dev.clone()) {
            Ok(v) => alloc::boxed::Box::new(v),
            Err("BitLocker-encrypted volume") => {
                crate::console::print_colored(crate::console::YELLOW, format_args!(
                    "[WARN] {} is encrypted with BitLocker: unlock it in Windows (or turn BitLocker off) to read it here\n",
                    dev.name()));
                return None;
            }
            Err(_) => return None,
        }
    };
    let mut mounts = MOUNTS.lock();
    // "/" goes to the first writable volume; the others get "/<device>".
    let root_taken = mounts.iter().any(|m| m.path == "/");
    let path = if !root_taken && !vol.read_only() && vol.kind() != "NTFS" { String::from("/") } else { format!("/{}", vol.dev().name()) };
    let m = Arc::new(Mount { path, vol });
    mounts.push(m.clone());
    Some(m)
}

/// Unmounts the volumes on disk `disk` (the disk itself and its partitions).
/// Returns the mount points removed.
pub fn unmount_disk(disk: &str) -> Vec<String> {
    let mut gone = Vec::new();
    MOUNTS.lock().retain(|m| {
        let keep = !crate::block::on_disk(m.vol.dev().name(), disk);
        if !keep {
            gone.push(m.path.clone());
        }
        keep
    });
    gone
}

/// Every mounted volume, root first.
pub fn volumes() -> Vec<Arc<Mount>> {
    MOUNTS.lock().clone()
}

fn mounts() -> Result<Vec<Arc<Mount>>, &'static str> {
    let m = MOUNTS.lock().clone();
    if m.is_empty() { Err("no filesystem mounted") } else { Ok(m) }
}

/// Picks the volume a path lives on and returns the path inside that volume.
fn resolve(path: &str) -> Result<(Arc<Mount>, &str), &'static str> {
    let mounts = mounts()?;
    let trimmed = path.trim_start_matches('/');
    let (first, rest) = trimmed.split_once('/').unwrap_or((trimmed, ""));
    for m in mounts.iter().filter(|m| m.path != "/") {
        if m.path[1..].eq_ignore_ascii_case(first) {
            return Ok((m.clone(), rest));
        }
    }
    match mounts.iter().find(|m| m.path == "/") {
        Some(root) => Ok((root.clone(), path)),
        None => Err("no filesystem mounted at /"),
    }
}

fn is_root(path: &str) -> bool {
    path.trim_matches('/').is_empty()
}

pub fn list(path: &str) -> Result<Vec<DirEntry>, &'static str> {
    let (m, inner) = resolve(path)?;
    let mut entries = m.vol.list(inner)?;
    // Mount points show up as directories in "/".
    if is_root(path) {
        for other in mounts()?.iter().filter(|m| m.path != "/") {
            entries.push(DirEntry::mount_point(&other.path[1..]));
        }
    }
    Ok(entries)
}

pub fn read(path: &str, limit: usize) -> Result<Vec<u8>, &'static str> {
    let (m, inner) = resolve(path)?;
    m.vol.read_file(inner, limit)
}

/// The volume `path` is on and the path inside it, which must name
/// something inside the volume (not a mount point itself).
fn resolve_entry(path: &str) -> Result<(Arc<Mount>, &str), &'static str> {
    let (m, inner) = resolve(path)?;
    if is_root(inner) {
        return Err("bad file name");
    }
    Ok((m, inner))
}

/// Creates the file at `path`, or replaces what it holds.
pub fn write(path: &str, data: &[u8]) -> Result<(), &'static str> {
    let (m, inner) = resolve_entry(path)?;
    m.vol.write_file(inner, data)
}

/// Creates a directory.
pub fn create_dir(path: &str) -> Result<(), &'static str> {
    let (m, inner) = resolve_entry(path)?;
    m.vol.create_dir(inner)
}

/// Deletes a file or an empty directory.
pub fn remove(path: &str) -> Result<(), &'static str> {
    let (m, inner) = resolve_entry(path)?;
    m.vol.remove(inner)
}

/// Deleted entries still in directory `path`.
pub fn deleted(path: &str) -> Result<Vec<Deleted>, &'static str> {
    let (m, inner) = resolve(path)?;
    m.vol.deleted(inner)
}

/// Brings back deleted entry `slot` of directory `path`; returns its name.
pub fn undelete(path: &str, slot: u32) -> Result<String, &'static str> {
    let (m, inner) = resolve(path)?;
    if m.vol.read_only() {
        return Err("read-only volume");
    }
    m.vol.undelete(inner, slot)
}

/// Turns writing to the volume mounted at `path` on or off.
pub fn set_writable(path: &str, on: bool) -> Result<(), &'static str> {
    let (m, inner) = resolve(path)?;
    if !is_root(inner) {
        return Err("not a drive");
    }
    m.vol.set_writable(on)
}

/// Where a block device is mounted, if anywhere.
pub fn mount_point_of(dev: &str) -> Option<String> {
    MOUNTS.lock().iter().find(|m| m.vol.dev().name() == dev).map(|m| m.path.clone())
}
