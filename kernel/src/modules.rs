//! Boot modules handed over by Limine: user programs from /boot/bin and
//! device firmware from /boot/firmware.

use alloc::string::String;
use alloc::vec::Vec;
use spin::Once;

use crate::limine::{cstr, ModuleResponse};

pub struct Module {
    pub name: String,
    /// Path on the boot volume, e.g. /boot/firmware/mediatek/BT_RAM_CODE_MT7961_1_2_hdr.bin
    pub path: String,
    pub data: &'static [u8],
}

static MODULES: Once<Vec<Module>> = Once::new();

pub fn init(resp: Option<&ModuleResponse>) -> usize {
    let list = MODULES.call_once(|| {
        let mut v = Vec::new();
        if let Some(r) = resp {
            for f in r.files() {
                let path = cstr(f.path);
                let name = path.rsplit('/').next().unwrap_or(path);
                let data = unsafe { core::slice::from_raw_parts(f.address, f.size as usize) };
                let path = path.split_once(":").map_or(path, |(_, p)| p);
                v.push(Module { name: String::from(name), path: String::from(path), data });
            }
        }
        v
    });
    list.len()
}

/// A user program by name.
pub fn find(name: &str) -> Option<&'static [u8]> {
    programs().find(|m| m.name == name).map(|m| m.data)
}

/// Firmware by its linux-firmware path, e.g. mediatek/BT_RAM_CODE_MT7961_1_2_hdr.bin.
pub fn firmware(path: &str) -> Option<&'static [u8]> {
    list().iter().find(|m| m.path.strip_prefix("/boot/firmware/") == Some(path)).map(|m| m.data)
}

pub fn programs() -> impl Iterator<Item = &'static Module> {
    list().iter().filter(|m| !m.path.starts_with("/boot/firmware/"))
}

pub fn list() -> &'static [Module] {
    MODULES.get().map_or(&[], |v| v.as_slice())
}
