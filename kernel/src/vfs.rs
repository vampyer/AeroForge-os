//! The (very) early file namespace. Every FAT32 volume is mounted, readable
//! and writable: the first one found at boot at "/", each further one at
//! "/<device name>" (for example "/sata0p1"). Volumes on USB disks plugged
//! in later are mounted the same way and go away when the disk is
//! unplugged. Real mount tables, more filesystems and user-space filesystem
//! servers come with Phase 2.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::block::{BlockDevice, DEVICES};
use crate::fat::{DirEntry, FatVolume};
use crate::sync::IrqMutex;

pub struct Mount {
    /// "/" for the root volume, "/<device>" for the others.
    pub path: String,
    pub vol: FatVolume,
}

/// Root first. Lookups clone the Arc and drop the lock before touching the disk.
static MOUNTS: IrqMutex<Vec<Arc<Mount>>> = IrqMutex::new(Vec::new());

/// Mounts every FAT32 volume found at boot. Returns the mounts made, root first.
pub fn mount_all() -> Vec<Arc<Mount>> {
    let devices: Vec<_> = DEVICES.lock().iter().cloned().collect();
    for dev in devices {
        mount(dev);
    }
    MOUNTS.lock().clone()
}

/// Mounts `dev` if it holds a FAT32 volume and is not mounted yet.
pub fn mount(dev: Arc<dyn BlockDevice>) -> Option<Arc<Mount>> {
    if mount_point_of(dev.name()).is_some() {
        return None;
    }
    let vol = FatVolume::mount(dev).ok()?;
    let mut mounts = MOUNTS.lock();
    let path = if mounts.is_empty() { String::from("/") } else { format!("/{}", vol.dev.name()) };
    let m = Arc::new(Mount { path, vol });
    mounts.push(m.clone());
    Some(m)
}

/// Unmounts the volumes on disk `disk` (the disk itself and its partitions).
/// Returns the mount points removed.
pub fn unmount_disk(disk: &str) -> Vec<String> {
    let mut gone = Vec::new();
    MOUNTS.lock().retain(|m| {
        let keep = !crate::block::on_disk(m.vol.dev.name(), disk);
        if !keep {
            gone.push(m.path.clone());
        }
        keep
    });
    gone
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

/// Where a block device is mounted, if anywhere.
pub fn mount_point_of(dev: &str) -> Option<String> {
    MOUNTS.lock().iter().find(|m| m.vol.dev.name() == dev).map(|m| m.path.clone())
}
