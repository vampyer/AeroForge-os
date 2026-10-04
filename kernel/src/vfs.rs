//! The (very) early file namespace. Every FAT32 volume found at boot is
//! mounted read-only: the first one at "/", each further one at
//! "/<device name>" (for example "/sata0p1"). Real mount tables, more
//! filesystems and user-space filesystem servers come with Phase 2.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use spin::Once;

use crate::block::DEVICES;
use crate::fat::{DirEntry, FatVolume};

pub struct Mount {
    /// "/" for the root volume, "/<device>" for the others.
    pub path: String,
    pub vol: FatVolume,
}

static MOUNTS: Once<Vec<Mount>> = Once::new();

/// Mounts every FAT32 volume. Returns the mounts made, root first.
pub fn mount_all() -> &'static [Mount] {
    MOUNTS.call_once(|| {
        let devices: Vec<_> = DEVICES.lock().iter().cloned().collect();
        let mut mounts = Vec::new();
        for dev in devices {
            if let Ok(vol) = FatVolume::mount(dev) {
                let path = if mounts.is_empty() { String::from("/") } else { format!("/{}", vol.dev.name()) };
                mounts.push(Mount { path, vol });
            }
        }
        mounts
    })
}

fn mounts() -> Result<&'static [Mount], &'static str> {
    match MOUNTS.get() {
        Some(m) if !m.is_empty() => Ok(m),
        _ => Err("no filesystem mounted"),
    }
}

/// Picks the volume a path lives on and returns the path inside that volume.
fn resolve(path: &str) -> Result<(&'static Mount, &str), &'static str> {
    let mounts = mounts()?;
    let trimmed = path.trim_start_matches('/');
    let (first, rest) = trimmed.split_once('/').unwrap_or((trimmed, ""));
    for m in &mounts[1..] {
        if m.path[1..].eq_ignore_ascii_case(first) {
            return Ok((m, rest));
        }
    }
    Ok((&mounts[0], path))
}

fn is_root(path: &str) -> bool {
    path.trim_matches('/').is_empty()
}

pub fn list(path: &str) -> Result<Vec<DirEntry>, &'static str> {
    let (m, inner) = resolve(path)?;
    let mut entries = m.vol.list(inner)?;
    // Mount points show up as directories in "/".
    if is_root(path) {
        for other in &mounts()?[1..] {
            entries.push(DirEntry::mount_point(&other.path[1..]));
        }
    }
    Ok(entries)
}

pub fn read(path: &str, limit: usize) -> Result<Vec<u8>, &'static str> {
    let (m, inner) = resolve(path)?;
    m.vol.read_file(inner, limit)
}

/// Where a block device is mounted, if anywhere.
pub fn mount_point_of(dev: &str) -> Option<&'static str> {
    MOUNTS.get()?.iter().find(|m| m.vol.dev.name() == dev).map(|m| m.path.as_str())
}
