//! Large kernel buffers built from separate pages and mapped contiguously,
//! for images bigger than the buddy allocator's largest block (a 1080p
//! screen is 8 MiB). Devices that take scatter lists, such as virtio-gpu,
//! use the page list directly.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::memory::{self, flags, PAGE_SIZE};

/// PML4 slot 507 of the kernel half: 512 GiB for these buffers. Like the
/// stack region, addresses are never reused, so no CPU can hold a stale TLB
/// entry for a live buffer.
const REGION_START: u64 = 0xFFFF_FD80_0000_0000;
const REGION_END: u64 = REGION_START + (1 << 39);

static NEXT: AtomicU64 = AtomicU64::new(REGION_START);

pub struct PageBuffer {
    base: u64,
    /// Physical address of each page, in order.
    pub pages: Vec<u64>,
}

impl PageBuffer {
    /// A zeroed buffer of at least `bytes`.
    pub fn new(bytes: usize) -> Option<Self> {
        let count = (bytes as u64).div_ceil(PAGE_SIZE);
        // One unmapped page after each buffer.
        let base = NEXT.fetch_add((count + 1) * PAGE_SIZE, Ordering::SeqCst);
        if base + (count + 1) * PAGE_SIZE > REGION_END {
            return None;
        }
        let mut buf = PageBuffer { base, pages: Vec::with_capacity(count as usize) };
        for i in 0..count {
            let frame = memory::alloc_frame_zeroed()?;
            if memory::map_page(memory::kernel_pml4(), base + i * PAGE_SIZE, frame, flags::WRITABLE | flags::NO_EXECUTE).is_err() {
                memory::free_frame(frame);
                return None;
            }
            buf.pages.push(frame);
        }
        Some(buf)
    }

    pub fn ptr(&self) -> *mut u8 {
        self.base as *mut u8
    }
}

impl Drop for PageBuffer {
    fn drop(&mut self) {
        for i in 0..self.pages.len() as u64 {
            if let Some(frame) = memory::unmap_page(memory::kernel_pml4(), self.base + i * PAGE_SIZE) {
                memory::free_frame(frame);
            }
        }
    }
}
