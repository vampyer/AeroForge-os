//! Kernel thread stacks with guard pages. Each stack lives in its own slot
//! of a dedicated virtual region, with an unmapped page below it: a stack
//! overflow hits the guard and faults (caught on the double-fault IST stack)
//! instead of silently overwriting whatever memory sits below.
//! Slots are never reused, so no other CPU can hold a stale TLB entry for a
//! live stack.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::memory::{self, flags, PAGE_SIZE};

/// PML4 slot 508 of the kernel half: 512 GiB of room for stacks.
const REGION_START: u64 = 0xFFFF_FE00_0000_0000;
const REGION_END: u64 = REGION_START + (1 << 39);
const SLOT: u64 = 64 * 1024;

static NEXT_SLOT: AtomicU64 = AtomicU64::new(REGION_START);

pub struct KernelStack {
    /// Lowest mapped address (the guard page is the page below it).
    base: u64,
    size: u64,
}

impl KernelStack {
    pub const fn empty() -> Self {
        Self { base: 0, size: 0 }
    }

    pub fn new(size: u64) -> Option<Self> {
        let size = (size + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
        assert!(size + PAGE_SIZE <= SLOT, "kernel stack too large for its slot");
        let slot = NEXT_SLOT.fetch_add(SLOT, Ordering::SeqCst);
        if slot + SLOT > REGION_END {
            return None;
        }
        // The slot's first page stays unmapped: that's the guard.
        let base = slot + SLOT - size;
        let stack = Self { base, size: 0 };
        let mut stack = stack;
        let mut v = base;
        while v < base + size {
            let frame = memory::alloc_frame_zeroed()?;
            if memory::map_page(memory::kernel_pml4(), v, frame, flags::WRITABLE | flags::NO_EXECUTE).is_err() {
                memory::free_frame(frame);
                return None;
            }
            v += PAGE_SIZE;
            stack.size = v - base;
        }
        Some(stack)
    }

    pub fn top(&self) -> u64 {
        self.base + self.size
    }

    /// The unmapped page just below the stack.
    pub fn guard_page(&self) -> Option<u64> {
        (self.size > 0).then(|| self.base - PAGE_SIZE)
    }
}

impl Drop for KernelStack {
    fn drop(&mut self) {
        let mut v = self.base;
        while v < self.base + self.size {
            if let Some(frame) = memory::unmap_page(memory::kernel_pml4(), v) {
                memory::free_frame(frame);
            }
            v += PAGE_SIZE;
        }
    }
}

/// True if `addr` is a guard page of the stack region (for fault reports).
pub fn is_guard(addr: u64) -> bool {
    (REGION_START..REGION_END).contains(&addr) && (addr - REGION_START) % SLOT < PAGE_SIZE
}
