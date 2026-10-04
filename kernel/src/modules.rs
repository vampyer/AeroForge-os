//! Boot modules (user programs) handed over by Limine.

use alloc::string::String;
use alloc::vec::Vec;
use spin::Once;

use crate::limine::{cstr, ModuleResponse};

pub struct Module {
    pub name: String,
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
                v.push(Module { name: String::from(name), data });
            }
        }
        v
    });
    list.len()
}

pub fn find(name: &str) -> Option<&'static [u8]> {
    MODULES.get()?.iter().find(|m| m.name == name).map(|m| m.data)
}

pub fn list() -> &'static [Module] {
    MODULES.get().map_or(&[], |v| v.as_slice())
}
