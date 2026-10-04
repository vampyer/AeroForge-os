//! Physical frame allocator, 4-level page-table mapper and the kernel heap.
//!
//! Phase 1 starting point: frames come from the Limine memory map, page
//! tables are the ones Limine built (we extend them), and the heap is a
//! linked-list allocator over a freshly mapped virtual range. The buddy +
//! slab design from the doc replaces the frame allocator later.

use core::sync::atomic::{AtomicU64, Ordering};
use linked_list_allocator::LockedHeap;
use spin::Mutex;

use crate::arch;
use crate::limine::{MemmapResponse, MEMMAP_USABLE};

pub const PAGE_SIZE: u64 = 4096;

/// Kernel heap lives in its own PML4 slot, away from HHDM and the kernel image.
pub const HEAP_START: u64 = 0xFFFF_E000_0000_0000;
pub const HEAP_SIZE: u64 = 8 * 1024 * 1024;

const PRESENT: u64 = 1 << 0;
const WRITABLE: u64 = 1 << 1;
const HUGE: u64 = 1 << 7;
const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;

static HHDM_OFFSET: AtomicU64 = AtomicU64::new(0);

#[global_allocator]
static HEAP: LockedHeap = LockedHeap::empty();

pub fn phys_to_virt(phys: u64) -> u64 {
    phys + HHDM_OFFSET.load(Ordering::Relaxed)
}

pub fn hhdm_offset() -> u64 {
    HHDM_OFFSET.load(Ordering::Relaxed)
}

#[derive(Clone, Copy)]
struct Region {
    next: u64,
    end: u64,
}

/// Hands out 4 KiB frames: recycled frames first (a free list threaded
/// through the frames themselves via the HHDM), then fresh ones from the
/// usable regions in order.
pub struct FrameAllocator {
    regions: [Region; 64],
    region_count: usize,
    current: usize,
    free_list: u64,
    pub total_bytes: u64,
    pub allocated: u64,
}

pub static FRAMES: Mutex<FrameAllocator> = Mutex::new(FrameAllocator {
    regions: [Region { next: 0, end: 0 }; 64],
    region_count: 0,
    current: 0,
    free_list: 0,
    total_bytes: 0,
    allocated: 0,
});

impl FrameAllocator {
    pub fn alloc(&mut self) -> Option<u64> {
        if self.free_list != 0 {
            let frame = self.free_list;
            self.free_list = unsafe { *(phys_to_virt(frame) as *const u64) };
            self.allocated += 1;
            return Some(frame);
        }
        while self.current < self.region_count {
            let r = &mut self.regions[self.current];
            if r.next + PAGE_SIZE <= r.end {
                let frame = r.next;
                r.next += PAGE_SIZE;
                self.allocated += 1;
                return Some(frame);
            }
            self.current += 1;
        }
        None
    }

    pub fn alloc_zeroed(&mut self) -> Option<u64> {
        let f = self.alloc()?;
        unsafe { core::ptr::write_bytes(phys_to_virt(f) as *mut u8, 0, PAGE_SIZE as usize) };
        Some(f)
    }

    #[allow(dead_code)] // first caller arrives with process teardown
    pub fn free(&mut self, frame: u64) {
        unsafe { *(phys_to_virt(frame) as *mut u64) = self.free_list };
        self.free_list = frame;
        self.allocated -= 1;
    }
}

pub struct MemoryReport {
    pub usable_bytes: u64,
    pub regions: usize,
}

pub fn init_frames(hhdm: u64, memmap: &MemmapResponse) -> MemoryReport {
    HHDM_OFFSET.store(hhdm, Ordering::Relaxed);
    let mut fa = FRAMES.lock();
    for e in memmap.entries() {
        if e.kind != MEMMAP_USABLE || fa.region_count == fa.regions.len() {
            continue;
        }
        let start = (e.base + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
        let end = (e.base + e.length) & !(PAGE_SIZE - 1);
        // Leave the first MiB alone (real-mode leftovers, AP trampolines).
        let start = start.max(0x10_0000);
        if end > start {
            let i = fa.region_count;
            fa.regions[i] = Region { next: start, end };
            fa.region_count += 1;
            fa.total_bytes += end - start;
        }
    }
    MemoryReport { usable_bytes: fa.total_bytes, regions: fa.region_count }
}

fn table(phys: u64) -> &'static mut [u64; 512] {
    unsafe { &mut *(phys_to_virt(phys) as *mut [u64; 512]) }
}

fn indices(virt: u64) -> [usize; 4] {
    [
        ((virt >> 39) & 0x1FF) as usize,
        ((virt >> 30) & 0x1FF) as usize,
        ((virt >> 21) & 0x1FF) as usize,
        ((virt >> 12) & 0x1FF) as usize,
    ]
}

/// Maps one 4 KiB page in the active address space, creating any missing
/// intermediate tables.
pub fn map_page(virt: u64, phys: u64, writable: bool) -> Result<(), &'static str> {
    let idx = indices(virt);
    let mut table_phys = arch::read_cr3() & ADDR_MASK;
    for &i in &idx[..3] {
        let t = table(table_phys);
        if t[i] & PRESENT == 0 {
            let new = FRAMES.lock().alloc_zeroed().ok_or("out of physical memory")?;
            t[i] = new | PRESENT | WRITABLE;
        } else if t[i] & HUGE != 0 {
            return Err("address already covered by a huge page");
        }
        table_phys = t[i] & ADDR_MASK;
    }
    let pt = table(table_phys);
    if pt[idx[3]] & PRESENT != 0 {
        return Err("page already mapped");
    }
    pt[idx[3]] = (phys & ADDR_MASK) | PRESENT | if writable { WRITABLE } else { 0 };
    unsafe { arch::invlpg(virt) };
    Ok(())
}

/// Walks the page tables in software: virtual -> physical.
pub fn translate(virt: u64) -> Option<u64> {
    let idx = indices(virt);
    let mut table_phys = arch::read_cr3() & ADDR_MASK;
    for (level, &i) in idx.iter().enumerate() {
        let entry = table(table_phys)[i];
        if entry & PRESENT == 0 {
            return None;
        }
        if level > 0 && level < 3 && entry & HUGE != 0 {
            let page = if level == 1 { 1u64 << 30 } else { 1u64 << 21 };
            return Some((entry & ADDR_MASK & !(page - 1)) | (virt & (page - 1)));
        }
        table_phys = entry & ADDR_MASK;
    }
    Some(table_phys | (virt & 0xFFF))
}

/// Makes sure a physical range is reachable through the HHDM (firmware
/// tables are not always mapped there) and returns its virtual address.
pub fn map_physical(phys: u64, len: u64) -> u64 {
    let start = phys & !(PAGE_SIZE - 1);
    let end = (phys + len + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
    let mut p = start;
    while p < end {
        if translate(phys_to_virt(p)).is_none() {
            let _ = map_page(phys_to_virt(p), p, false);
        }
        p += PAGE_SIZE;
    }
    phys_to_virt(phys)
}

pub fn init_heap() -> Result<(), &'static str> {
    let mut v = HEAP_START;
    while v < HEAP_START + HEAP_SIZE {
        let frame = FRAMES.lock().alloc().ok_or("out of physical memory for heap")?;
        map_page(v, frame, true)?;
        v += PAGE_SIZE;
    }
    unsafe { HEAP.lock().init(HEAP_START as *mut u8, HEAP_SIZE as usize) };
    Ok(())
}

pub fn heap_stats() -> (usize, usize) {
    let h = HEAP.lock();
    (h.used(), h.size())
}
