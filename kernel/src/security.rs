//! The kernel's security layer: CPU protections (NX, SMEP, SMAP, UMIP,
//! write protection), W^X for the kernel image, the stack canary shared with
//! the C++ drivers, guarded copies to and from user memory, and a boot-time
//! audit that checks all of it actually took effect.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::{arch, memory};

static NX: AtomicBool = AtomicBool::new(false);
static SMEP: AtomicBool = AtomicBool::new(false);
static SMAP: AtomicBool = AtomicBool::new(false);
static UMIP: AtomicBool = AtomicBool::new(false);
static CANARY_FROM_RDRAND: AtomicBool = AtomicBool::new(false);
pub static USER_FAULTS_BLOCKED: AtomicU64 = AtomicU64::new(0);

const CR0_WP: u64 = 1 << 16;
const CR4_UMIP: u64 = 1 << 11;
const CR4_SMEP: u64 = 1 << 20;
const CR4_SMAP: u64 = 1 << 21;
const EFER_NXE: u64 = 1 << 11;

/// Turns on every protection this CPU supports. Runs on each CPU before it
/// can run user code; SMEP/SMAP/UMIP are only enabled if every CPU has them,
/// which the boot CPU decides first.
pub fn init_cpu(boot_cpu: bool) {
    let ext = arch::cpuid(0x8000_0001);
    let leaf7 = if arch::cpuid(0).eax >= 7 { arch::cpuid(7) } else { arch::CpuidResult { eax: 0, ebx: 0, ecx: 0, edx: 0 } };
    if boot_cpu {
        NX.store(ext.edx & (1 << 20) != 0, Ordering::SeqCst);
        SMEP.store(leaf7.ebx & (1 << 7) != 0, Ordering::SeqCst);
        SMAP.store(leaf7.ebx & (1 << 20) != 0, Ordering::SeqCst);
        UMIP.store(leaf7.ecx & (1 << 2) != 0, Ordering::SeqCst);
    }
    unsafe {
        if NX.load(Ordering::SeqCst) {
            arch::wrmsr(arch::MSR_EFER, arch::rdmsr(arch::MSR_EFER) | EFER_NXE);
        }
        // Make the kernel honour read-only pages too.
        arch::write_cr0(arch::read_cr0() | CR0_WP);
        let mut cr4 = arch::read_cr4();
        if SMEP.load(Ordering::SeqCst) {
            cr4 |= CR4_SMEP;
        }
        if SMAP.load(Ordering::SeqCst) {
            cr4 |= CR4_SMAP;
        }
        if UMIP.load(Ordering::SeqCst) {
            cr4 |= CR4_UMIP;
        }
        arch::write_cr4(cr4);
    }
}

pub fn nx_enabled() -> bool {
    NX.load(Ordering::Relaxed)
}

// ------------------------------------------------------------ stack canary

/// Read by every C++ driver function that has a stack buffer
/// (`-fstack-protector-strong -mstack-protector-guard=global`).
#[no_mangle]
pub static mut __stack_chk_guard: u64 = 0x595e_9fbd_94fd_a766;

/// Called by C++ code when a function's canary was overwritten.
#[no_mangle]
pub extern "C" fn __stack_chk_fail() -> ! {
    panic!("stack smashing detected in a C++ driver (canary overwritten)");
}

/// Picks a fresh random canary. Must run before the first C++ call.
pub fn init_canary() {
    let (value, hw) = match arch::rdrand() {
        Some(v) => (v, true),
        None => {
            // No RDRAND: mix the TSC, which is at least unpredictable across boots.
            let mut x = arch::rdtsc() ^ 0x9E37_79B9_7F4A_7C15;
            x ^= x >> 33;
            x = x.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
            x ^= x >> 33;
            (x, false)
        }
    };
    // A zero byte first stops string overflows from copying past the canary.
    unsafe { core::ptr::addr_of_mut!(__stack_chk_guard).write_volatile(value & !0xFF) };
    CANARY_FROM_RDRAND.store(hw, Ordering::SeqCst);
}

// ----------------------------------------------------------- kernel image

extern "C" {
    static __kernel_start: u8;
    static __text_start: u8;
    static __text_end: u8;
    static __rodata_start: u8;
    static __rodata_end: u8;
    static __data_start: u8;
    static __data_end: u8;
}

fn sym(s: &u8) -> u64 {
    s as *const u8 as u64
}

pub struct ImageLayout {
    pub start: u64,
    pub text: (u64, u64),
    pub rodata: (u64, u64),
    pub data: (u64, u64),
}

pub fn layout() -> ImageLayout {
    unsafe {
        ImageLayout {
            start: sym(&__kernel_start),
            text: (sym(&__text_start), sym(&__text_end)),
            rodata: (sym(&__rodata_start), sym(&__rodata_end)),
            data: (sym(&__data_start), sym(&__data_end)),
        }
    }
}

/// Enforces W^X on the kernel image: code read-only, everything else
/// non-executable. Returns how many pages were set.
pub fn protect_kernel_image() -> usize {
    let l = layout();
    let nx = nx_enabled();
    let mut pages = 0;
    pages += memory::protect_kernel_range(l.start, l.text.0, true, !nx).0; // Limine requests
    pages += memory::protect_kernel_range(l.text.0, l.text.1, false, true).0;
    pages += memory::protect_kernel_range(l.rodata.0, l.rodata.1, false, !nx).0;
    pages += memory::protect_kernel_range(l.data.0, l.data.1, true, !nx).0;
    pages
}

/// Makes the rest of the kernel half (the direct map of physical memory,
/// the heap, kernel stacks) non-executable. Must run after the other CPUs
/// are up, since Limine's CPU start-up code runs from the direct map, and
/// before the first process copies the kernel half. Returns slots marked.
pub fn lock_kernel_half() -> usize {
    if !nx_enabled() {
        return 0;
    }
    memory::no_execute_kernel_slots(layout().start)
}

// ----------------------------------------------------------- user memory

const USER_TOP: u64 = 0x0000_8000_0000_0000;

/// Checks that every page of [ptr, ptr+len) is mapped, user-accessible and,
/// if `write`, writable, in the current address space.
pub fn check_user(ptr: u64, len: u64, write: bool) -> bool {
    let Some(end) = ptr.checked_add(len) else { return false };
    if end > USER_TOP {
        return false;
    }
    let mut page = ptr & !(memory::PAGE_SIZE - 1);
    while page < end {
        match memory::access(page) {
            Some(a) if a.user && (a.writable || !write) => {}
            _ => {
                USER_FAULTS_BLOCKED.fetch_add(1, Ordering::Relaxed);
                return false;
            }
        }
        page += memory::PAGE_SIZE;
    }
    true
}

/// Runs `f` with SMAP lifted, for a copy the caller already validated.
fn with_user_access<T>(f: impl FnOnce() -> T) -> T {
    let smap = SMAP.load(Ordering::Relaxed);
    if smap {
        unsafe { arch::stac() };
    }
    let r = f();
    if smap {
        unsafe { arch::clac() };
    }
    r
}

/// Copies `len` bytes from user memory into a kernel buffer.
pub fn copy_from_user(ptr: u64, len: u64) -> Option<Vec<u8>> {
    if !check_user(ptr, len, false) {
        return None;
    }
    let mut buf = alloc::vec![0u8; len as usize];
    with_user_access(|| unsafe { core::ptr::copy_nonoverlapping(ptr as *const u8, buf.as_mut_ptr(), buf.len()) });
    Some(buf)
}

/// Copies user bytes straight into kernel memory, for large copies (screen
/// frames) where a temporary Vec would cost too much.
///
/// # Safety
/// The caller has checked [ptr, ptr+len) with `check_user`, and `dst` is
/// valid for `len` bytes.
pub unsafe fn copy_from_user_unchecked(ptr: u64, dst: *mut u8, len: usize) {
    with_user_access(|| core::ptr::copy_nonoverlapping(ptr as *const u8, dst, len));
}

/// Copies kernel bytes into user memory.
pub fn copy_to_user(ptr: u64, data: &[u8]) -> bool {
    if !check_user(ptr, data.len() as u64, true) {
        return false;
    }
    with_user_access(|| unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), ptr as *mut u8, data.len()) });
    true
}

/// Called first thing on every interrupt: user code can set EFLAGS.AC, and
/// that must not carry over into the kernel and switch SMAP off.
#[inline]
pub fn on_kernel_entry() {
    if SMAP.load(Ordering::Relaxed) {
        unsafe { arch::clac() };
    }
}

// ------------------------------------------------------------------ audit

pub struct Audit {
    pub nx: bool,
    pub smep: bool,
    pub smap: bool,
    pub umip: bool,
    pub wp: bool,
    pub canary_hw: bool,
    pub image_pages: usize,
    pub wx_pages: usize,
    pub writable_code: usize,
    pub exec_data: usize,
    pub heap_executable: bool,
    pub stack_guard_unmapped: bool,
}

/// Re-reads the CPU state and walks the page tables to confirm the
/// protections are really in force (not just requested).
pub fn audit(stack_guard: Option<u64>) -> Audit {
    let cr4 = arch::read_cr4();
    let efer = unsafe { arch::rdmsr(arch::MSR_EFER) };
    let l = layout();
    let (mut pages, mut wx, mut wcode, mut xdata) = (0, 0, 0, 0);
    let mut v = l.start & !(memory::PAGE_SIZE - 1);
    while v < l.data.1 {
        if let Some(a) = memory::access(v) {
            pages += 1;
            let in_text = v >= l.text.0 && v < l.text.1;
            if a.writable && a.executable {
                wx += 1;
            }
            if in_text && a.writable {
                wcode += 1;
            }
            if !in_text && a.executable {
                xdata += 1;
            }
        }
        v += memory::PAGE_SIZE;
    }
    let heap_probe = Vec::<u8>::with_capacity(64);
    let heap_executable = memory::access(heap_probe.as_ptr() as u64).map_or(true, |a| a.executable);
    Audit {
        nx: efer & EFER_NXE != 0,
        smep: cr4 & CR4_SMEP != 0,
        smap: cr4 & CR4_SMAP != 0,
        umip: cr4 & CR4_UMIP != 0,
        wp: arch::read_cr0() & CR0_WP != 0,
        canary_hw: CANARY_FROM_RDRAND.load(Ordering::Relaxed),
        image_pages: pages,
        wx_pages: wx,
        writable_code: wcode,
        exec_data: xdata,
        heap_executable,
        stack_guard_unmapped: stack_guard.map_or(false, |g| memory::access(g).is_none()),
    }
}
