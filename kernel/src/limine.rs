//! Minimal, dependency-free bindings for the Limine boot protocol
//! (the subset AeroKernel uses). Field layouts mirror limine.h from Limine 9.x.

use core::cell::UnsafeCell;
use core::ptr;
use core::sync::atomic::AtomicU64;

const COMMON_MAGIC: [u64; 2] = [0xc7b1dd30df4c8b88, 0x0a82e883a194f07b];

const fn id(a: u64, b: u64) -> [u64; 4] {
    [COMMON_MAGIC[0], COMMON_MAGIC[1], a, b]
}

/// Asks Limine for a protocol revision; Limine zeroes word 2 if it supports it.
#[repr(C)]
pub struct BaseRevision(UnsafeCell<[u64; 3]>);
unsafe impl Sync for BaseRevision {}

impl BaseRevision {
    pub const fn new(rev: u64) -> Self {
        Self(UnsafeCell::new([0xf9562b2d5c95a6c8, 0x6a7b384944536bdc, rev]))
    }
    pub fn is_supported(&self) -> bool {
        unsafe { ptr::read_volatile((self.0.get() as *const u64).add(2)) == 0 }
    }
}

/// A Limine request. `E` holds request-specific trailing fields.
#[repr(C)]
pub struct Request<R, E = ()> {
    id: [u64; 4],
    revision: u64,
    response: UnsafeCell<*const R>,
    extra: E,
}
unsafe impl<R, E> Sync for Request<R, E> {}

impl<R> Request<R, ()> {
    pub const fn new(id: [u64; 4]) -> Self {
        Self::with_extra(id, ())
    }
}

impl<R, E> Request<R, E> {
    pub const fn with_extra(id: [u64; 4], extra: E) -> Self {
        Self { id, revision: 0, response: UnsafeCell::new(ptr::null()), extra }
    }
    /// The bootloader fills this in before jumping to the kernel, so read it
    /// volatile: the compiler must not assume it is still null.
    pub fn response(&self) -> Option<&'static R> {
        unsafe { ptr::read_volatile(self.response.get()).as_ref() }
    }
}

pub const BOOTLOADER_INFO: [u64; 4] = id(0xf55038d8e2a1202f, 0x279426fcf5f59740);
pub const HHDM: [u64; 4] = id(0x48dcf1cb8ad2b852, 0x63984e959a98244b);
pub const FRAMEBUFFER: [u64; 4] = id(0x9d5827dcd881dd75, 0xa3148604f6fab11b);
pub const MEMMAP: [u64; 4] = id(0x67cf3d9d378a806f, 0xe304acdfc50c3c62);
pub const MP: [u64; 4] = id(0x95a67b819a1b857e, 0xa0b61b723b6a73e0);
pub const RSDP: [u64; 4] = id(0xc5e77b6b397e7b43, 0x27637845accdcf3c);
pub const EXECUTABLE_ADDRESS: [u64; 4] = id(0x71ba76863cc55f63, 0xb2644a48c516a487);

pub const REQUESTS_START: [u64; 4] =
    [0xf6b8f4b39de7d1ae, 0xfab91a6940fcb9cf, 0x785c6ed015d3e316, 0x181e920a7852b9d9];
pub const REQUESTS_END: [u64; 2] = [0xadc0e0531bb10d03, 0x9572709f31764c62];

#[repr(C)]
pub struct BootloaderInfoResponse {
    pub revision: u64,
    pub name: *const u8,
    pub version: *const u8,
}

#[repr(C)]
pub struct HhdmResponse {
    pub revision: u64,
    pub offset: u64,
}

#[repr(C)]
pub struct Framebuffer {
    pub address: *mut u8,
    pub width: u64,
    pub height: u64,
    pub pitch: u64,
    pub bpp: u16,
    pub memory_model: u8,
    pub red_mask_size: u8,
    pub red_mask_shift: u8,
    pub green_mask_size: u8,
    pub green_mask_shift: u8,
    pub blue_mask_size: u8,
    pub blue_mask_shift: u8,
}

#[repr(C)]
pub struct FramebufferResponse {
    pub revision: u64,
    pub count: u64,
    pub framebuffers: *const *const Framebuffer,
}

pub const MEMMAP_USABLE: u64 = 0;

#[repr(C)]
pub struct MemmapEntry {
    pub base: u64,
    pub length: u64,
    pub kind: u64,
}

#[repr(C)]
pub struct MemmapResponse {
    pub revision: u64,
    pub count: u64,
    pub entries: *const *const MemmapEntry,
}

impl MemmapResponse {
    pub fn entries(&self) -> impl Iterator<Item = &'static MemmapEntry> + '_ {
        (0..self.count as usize).map(move |i| unsafe { &**self.entries.add(i) })
    }
}

#[repr(C)]
pub struct MpInfo {
    pub processor_id: u32,
    pub lapic_id: u32,
    pub reserved: u64,
    /// Writing a function address here releases the parked CPU.
    pub goto_address: AtomicU64,
    pub extra_argument: u64,
}

#[repr(C)]
pub struct MpResponse {
    pub revision: u64,
    pub flags: u32,
    pub bsp_lapic_id: u32,
    pub cpu_count: u64,
    pub cpus: *const *const MpInfo,
}

impl MpResponse {
    pub fn cpus(&self) -> impl Iterator<Item = &'static MpInfo> + '_ {
        (0..self.cpu_count as usize).map(move |i| unsafe { &**self.cpus.add(i) })
    }
}

#[repr(C)]
pub struct RsdpResponse {
    pub revision: u64,
    /// Physical address with base revision 3 (virtual with older revisions).
    pub address: u64,
}

#[repr(C)]
pub struct ExecutableAddressResponse {
    pub revision: u64,
    pub physical_base: u64,
    pub virtual_base: u64,
}

/// Reads a NUL-terminated string handed over by the bootloader.
pub fn cstr(p: *const u8) -> &'static str {
    if p.is_null() {
        return "?";
    }
    let mut len = 0;
    unsafe {
        while *p.add(len) != 0 && len < 256 {
            len += 1;
        }
        core::str::from_utf8(core::slice::from_raw_parts(p, len)).unwrap_or("?")
    }
}

pub const MODULE: [u64; 4] = id(0x3e7e279702be32af, 0xca1c4f3bd1280cee);

#[repr(C)]
pub struct File {
    pub revision: u64,
    pub address: *const u8,
    pub size: u64,
    pub path: *const u8,
    pub string: *const u8,
}

#[repr(C)]
pub struct ModuleResponse {
    pub revision: u64,
    pub count: u64,
    pub modules: *const *const File,
}

impl ModuleResponse {
    pub fn files(&self) -> impl Iterator<Item = &'static File> + '_ {
        (0..self.count as usize).map(move |i| unsafe { &**self.modules.add(i) })
    }
}
