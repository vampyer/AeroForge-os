//! Physical memory (buddy allocator), kernel heap (slab allocator) and
//! 4-level page tables.
//!
//! - Pages: a binary buddy allocator, orders 0 (4 KiB) to 10 (4 MiB), with a
//!   byte of metadata per page frame and free lists threaded through the free
//!   blocks themselves.
//! - Heap: power-of-two slab caches from 16 B to 2 KiB, larger requests go
//!   straight to the buddy allocator. Everything is addressed through the
//!   higher-half direct map (HHDM), so no extra heap mapping is needed.
//! - Address spaces: every process gets its own PML4 whose upper half is
//!   shared with the kernel.

use core::alloc::{GlobalAlloc, Layout};
use core::sync::atomic::{AtomicU64, Ordering};

use crate::arch;
use crate::limine::{MemmapResponse, MEMMAP_USABLE};
use crate::sync::IrqMutex;

pub const PAGE_SIZE: u64 = 4096;
pub const MAX_ORDER: usize = 10;

pub mod flags {
    pub const PRESENT: u64 = 1 << 0;
    pub const WRITABLE: u64 = 1 << 1;
    pub const USER: u64 = 1 << 2;
    pub const WRITE_THROUGH: u64 = 1 << 3;
    pub const NO_CACHE: u64 = 1 << 4;
    pub const HUGE: u64 = 1 << 7;
}
use flags::*;

const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;

static HHDM_OFFSET: AtomicU64 = AtomicU64::new(0);
static KERNEL_PML4: AtomicU64 = AtomicU64::new(0);

pub fn phys_to_virt(phys: u64) -> u64 {
    phys + HHDM_OFFSET.load(Ordering::Relaxed)
}

pub fn virt_to_phys_direct(virt: u64) -> u64 {
    virt - HHDM_OFFSET.load(Ordering::Relaxed)
}

pub fn hhdm_offset() -> u64 {
    HHDM_OFFSET.load(Ordering::Relaxed)
}

pub fn kernel_pml4() -> u64 {
    KERNEL_PML4.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------- buddy ----

const META_FREE: u8 = 0x80;

pub struct Buddy {
    heads: [u64; MAX_ORDER + 1], // physical address of first free block, 0 = empty
    meta: *mut u8,               // one byte per page frame
    frames: u64,
    pub total_pages: u64,
    pub free_pages: u64,
}

unsafe impl Send for Buddy {}

pub static BUDDY: IrqMutex<Buddy> = IrqMutex::new(Buddy {
    heads: [0; MAX_ORDER + 1],
    meta: core::ptr::null_mut(),
    frames: 0,
    total_pages: 0,
    free_pages: 0,
});

/// Free blocks store a doubly linked list node in their first 16 bytes.
fn node(phys: u64) -> *mut [u64; 2] {
    phys_to_virt(phys) as *mut [u64; 2]
}

impl Buddy {
    fn meta(&self, phys: u64) -> u8 {
        unsafe { *self.meta.add((phys / PAGE_SIZE) as usize) }
    }

    fn set_meta(&mut self, phys: u64, v: u8) {
        unsafe { *self.meta.add((phys / PAGE_SIZE) as usize) = v }
    }

    fn push(&mut self, order: usize, p: u64) {
        unsafe {
            let head = self.heads[order];
            *node(p) = [head, 0];
            if head != 0 {
                (*node(head))[1] = p;
            }
        }
        self.heads[order] = p;
        self.set_meta(p, META_FREE | order as u8);
    }

    fn unlink(&mut self, order: usize, p: u64) {
        unsafe {
            let [next, prev] = *node(p);
            if prev != 0 {
                (*node(prev))[0] = next;
            } else {
                self.heads[order] = next;
            }
            if next != 0 {
                (*node(next))[1] = prev;
            }
        }
        self.set_meta(p, order as u8);
    }

    pub fn alloc(&mut self, order: usize) -> Option<u64> {
        let mut o = (order..=MAX_ORDER).find(|&o| self.heads[o] != 0)?;
        let p = self.heads[o];
        self.unlink(o, p);
        while o > order {
            o -= 1;
            self.push(o, p + (PAGE_SIZE << o));
        }
        self.set_meta(p, order as u8);
        self.free_pages -= 1 << order;
        Some(p)
    }

    pub fn free(&mut self, mut p: u64, mut order: usize) {
        self.free_pages += 1 << order;
        while order < MAX_ORDER {
            let buddy = p ^ (PAGE_SIZE << order);
            if buddy / PAGE_SIZE >= self.frames || self.meta(buddy) != META_FREE | order as u8 {
                break;
            }
            self.unlink(order, buddy);
            p = p.min(buddy);
            order += 1;
        }
        self.push(order, p);
    }

    fn add_region(&mut self, mut start: u64, end: u64) {
        while start < end {
            let mut order = MAX_ORDER;
            while order > 0 && (start % (PAGE_SIZE << order) != 0 || start + (PAGE_SIZE << order) > end) {
                order -= 1;
            }
            self.push(order, start);
            self.free_pages += 1 << order;
            self.total_pages += 1 << order;
            start += PAGE_SIZE << order;
        }
    }
}

pub fn order_for(bytes: u64) -> usize {
    let pages = bytes.div_ceil(PAGE_SIZE).max(1);
    pages.next_power_of_two().trailing_zeros() as usize
}

pub fn alloc_frame() -> Option<u64> {
    BUDDY.lock().alloc(0)
}

pub fn alloc_frame_zeroed() -> Option<u64> {
    let f = alloc_frame()?;
    unsafe { core::ptr::write_bytes(phys_to_virt(f) as *mut u8, 0, PAGE_SIZE as usize) };
    Some(f)
}

pub fn free_frame(p: u64) {
    BUDDY.lock().free(p, 0)
}

pub struct MemoryReport {
    pub usable_bytes: u64,
    pub regions: usize,
    pub meta_bytes: u64,
}

pub fn init(hhdm: u64, memmap: &MemmapResponse) -> MemoryReport {
    HHDM_OFFSET.store(hhdm, Ordering::Relaxed);
    KERNEL_PML4.store(arch::read_cr3() & ADDR_MASK, Ordering::Relaxed);

    let usable = || {
        memmap.entries().filter(|e| e.kind == MEMMAP_USABLE).filter_map(|e| {
            // Leave the first MiB alone (real-mode leftovers, AP trampolines).
            let start = ((e.base + PAGE_SIZE - 1) & !(PAGE_SIZE - 1)).max(0x10_0000);
            let end = (e.base + e.length) & !(PAGE_SIZE - 1);
            (end > start).then_some((start, end))
        })
    };

    let top = usable().map(|(_, e)| e).max().unwrap_or(0);
    let frames = top / PAGE_SIZE;
    let meta_bytes = (frames + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
    let (meta_region, _) = usable()
        .filter(|(s, e)| e - s >= meta_bytes)
        .max_by_key(|(s, e)| e - s)
        .expect("no region large enough for page metadata");

    let mut b = BUDDY.lock();
    b.meta = phys_to_virt(meta_region) as *mut u8;
    b.frames = frames;
    unsafe { core::ptr::write_bytes(b.meta, 0, meta_bytes as usize) };

    let mut regions = 0;
    let mut bytes = 0;
    for (mut start, end) in usable() {
        if start == meta_region {
            start += meta_bytes;
        }
        if end > start {
            b.add_region(start, end);
            regions += 1;
            bytes += end - start;
        }
    }
    drop(b);

    prepare_kernel_half();
    MemoryReport { usable_bytes: bytes, regions, meta_bytes }
}

// ----------------------------------------------------------------- slab ----

const CLASSES: [usize; 8] = [16, 32, 64, 128, 256, 512, 1024, 2048];

struct Slab {
    free: [usize; CLASSES.len()], // virtual address of first free object, 0 = empty
    pages: [u64; CLASSES.len()],
    in_use: [u64; CLASSES.len()],
    large_bytes: u64,
}

static SLAB: IrqMutex<Slab> = IrqMutex::new(Slab {
    free: [0; CLASSES.len()],
    pages: [0; CLASSES.len()],
    in_use: [0; CLASSES.len()],
    large_bytes: 0,
});

pub struct KernelHeap;

#[global_allocator]
static HEAP: KernelHeap = KernelHeap;

fn class_of(layout: &Layout) -> Result<usize, usize> {
    let size = layout.size().max(layout.align()).max(1);
    match CLASSES.iter().position(|&c| c >= size) {
        Some(i) => Ok(i),
        None => Err(size),
    }
}

unsafe impl GlobalAlloc for KernelHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        match class_of(&layout) {
            Ok(i) => {
                let mut s = SLAB.lock();
                if s.free[i] == 0 {
                    // Refill: carve a fresh page into objects of this class.
                    let Some(page) = BUDDY.lock().alloc(0) else { return core::ptr::null_mut() };
                    let base = phys_to_virt(page) as usize;
                    let size = CLASSES[i];
                    for off in (0..PAGE_SIZE as usize).step_by(size).rev() {
                        *((base + off) as *mut usize) = s.free[i];
                        s.free[i] = base + off;
                    }
                    s.pages[i] += 1;
                }
                let obj = s.free[i];
                s.free[i] = *(obj as *const usize);
                s.in_use[i] += 1;
                obj as *mut u8
            }
            Err(size) => {
                let Some(p) = BUDDY.lock().alloc(order_for(size as u64)) else { return core::ptr::null_mut() };
                SLAB.lock().large_bytes += PAGE_SIZE << order_for(size as u64);
                phys_to_virt(p) as *mut u8
            }
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        match class_of(&layout) {
            Ok(i) => {
                let mut s = SLAB.lock();
                *(ptr as *mut usize) = s.free[i];
                s.free[i] = ptr as usize;
                s.in_use[i] -= 1;
            }
            Err(size) => {
                let order = order_for(size as u64);
                BUDDY.lock().free(virt_to_phys_direct(ptr as u64), order);
                SLAB.lock().large_bytes -= PAGE_SIZE << order;
            }
        }
    }
}

/// (bytes in use by small objects, bytes in slab pages, bytes in large blocks)
pub fn heap_stats() -> (u64, u64, u64) {
    let s = SLAB.lock();
    let used = (0..CLASSES.len()).map(|i| s.in_use[i] * CLASSES[i] as u64).sum();
    let pages = s.pages.iter().sum::<u64>() * PAGE_SIZE;
    (used, pages, s.large_bytes)
}

// ---------------------------------------------------------- page tables ----

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

/// Gives every upper-half PML4 slot a table now, so address spaces cloned
/// later automatically see kernel mappings added after they were created.
fn prepare_kernel_half() {
    let pml4 = table(kernel_pml4());
    for e in pml4.iter_mut().skip(256) {
        if *e & PRESENT == 0 {
            *e = alloc_frame_zeroed().expect("out of memory") | PRESENT | WRITABLE;
        }
    }
}

/// A new address space: empty lower half, shared kernel upper half.
pub fn new_address_space() -> Option<u64> {
    let pml4 = alloc_frame_zeroed()?;
    table(pml4)[256..].copy_from_slice(&table(kernel_pml4())[256..]);
    Some(pml4)
}

/// Frees the user half of an address space: mapped frames, page tables
/// and finally the PML4 itself. The space must not be active on any CPU.
pub fn destroy_address_space(pml4: u64) {
    // level 3 = PDPT, 2 = PD, 1 = PT
    fn free_table(t: u64, level: usize) {
        for &e in table(t).iter() {
            if e & PRESENT == 0 {
                continue;
            }
            if level == 1 {
                free_frame(e & ADDR_MASK);
            } else if e & HUGE == 0 {
                free_table(e & ADDR_MASK, level - 1);
            }
        }
        free_frame(t);
    }
    for &e in table(pml4)[..256].iter() {
        if e & PRESENT != 0 {
            free_table(e & ADDR_MASK, 3);
        }
    }
    free_frame(pml4);
}

/// Maps one 4 KiB page in the given address space, creating any missing
/// intermediate tables. `flags` are PTE bits besides PRESENT.
pub fn map_page(pml4: u64, virt: u64, phys: u64, flags: u64) -> Result<(), &'static str> {
    let idx = indices(virt);
    let mut table_phys = pml4;
    let parent_flags = PRESENT | WRITABLE | (flags & USER);
    for &i in &idx[..3] {
        let t = table(table_phys);
        if t[i] & PRESENT == 0 {
            t[i] = alloc_frame_zeroed().ok_or("out of physical memory")? | parent_flags;
        } else if t[i] & HUGE != 0 {
            return Err("address already covered by a huge page");
        } else {
            t[i] |= parent_flags;
        }
        table_phys = t[i] & ADDR_MASK;
    }
    let pt = table(table_phys);
    if pt[idx[3]] & PRESENT != 0 {
        return Err("page already mapped");
    }
    pt[idx[3]] = (phys & ADDR_MASK) | PRESENT | flags;
    unsafe { arch::invlpg(virt) };
    Ok(())
}

/// Walks the page tables in software: virtual -> physical.
pub fn translate_in(pml4: u64, virt: u64) -> Option<u64> {
    let idx = indices(virt);
    let mut table_phys = pml4;
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

pub fn translate(virt: u64) -> Option<u64> {
    translate_in(arch::read_cr3() & ADDR_MASK, virt)
}

/// Makes a physical range reachable through the HHDM (firmware tables and
/// MMIO are not always mapped there) and returns its virtual address.
fn map_physical_with(phys: u64, len: u64, flags: u64) -> u64 {
    let start = phys & !(PAGE_SIZE - 1);
    let end = (phys + len + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
    let mut p = start;
    while p < end {
        if translate_in(kernel_pml4(), phys_to_virt(p)).is_none() {
            let _ = map_page(kernel_pml4(), phys_to_virt(p), p, flags);
        }
        p += PAGE_SIZE;
    }
    phys_to_virt(phys)
}

pub fn map_physical(phys: u64, len: u64) -> u64 {
    map_physical_with(phys, len, 0)
}

/// Uncached mapping for device registers.
pub fn map_mmio(phys: u64, len: u64) -> u64 {
    map_physical_with(phys, len, WRITABLE | NO_CACHE | WRITE_THROUGH)
}
