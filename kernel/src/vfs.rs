//! The (very) early file namespace: the first FAT32 partition found is
//! mounted at "/". Mount points, more filesystems and user-space filesystem
//! servers come with Phase 2.

use alloc::vec::Vec;
use spin::Once;

use crate::block::DEVICES;
use crate::fat::{DirEntry, FatVolume};

static ROOT: Once<FatVolume> = Once::new();

/// Mounts the first FAT32 volume. Returns (device name, volume label).
pub fn mount_root() -> Option<(&'static str, &'static str)> {
    let devices: Vec<_> = DEVICES.lock().iter().cloned().collect();
    for dev in devices {
        if let Ok(vol) = FatVolume::mount(dev) {
            let v = ROOT.call_once(|| vol);
            return Some((v.dev.name(), v.label.as_str()));
        }
    }
    None
}

pub fn list(path: &str) -> Result<Vec<DirEntry>, &'static str> {
    ROOT.get().ok_or("no filesystem mounted")?.list(path)
}

pub fn read(path: &str, limit: usize) -> Result<Vec<u8>, &'static str> {
    ROOT.get().ok_or("no filesystem mounted")?.read_file(path, limit)
}

pub fn root_device() -> Option<&'static str> {
    ROOT.get().map(|v| v.dev.name())
}
