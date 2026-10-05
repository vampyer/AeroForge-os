//! Floating point and vector registers (x87, SSE, AVX, AVX-512) for user programs.
//!
//! The kernel itself is built without SSE (Rust soft-float target, C++ with
//! `-mgeneral-regs-only`), so these registers only ever hold user state.
//! That makes saving them simple: every user thread has a save area, and the
//! scheduler saves the outgoing thread's registers and loads the incoming
//! one's on each switch (XSAVE/XRSTOR, or FXSAVE/FXRSTOR on CPUs without
//! XSAVE). Kernel threads leave the registers alone and have no area.

use alloc::alloc::{alloc_zeroed, dealloc, Layout};
use core::arch::asm;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use crate::arch;

const CR0_MP: u64 = 1 << 1;
const CR0_EM: u64 = 1 << 2;
const CR0_TS: u64 = 1 << 3;
const CR0_NE: u64 = 1 << 5;
const CR4_OSFXSR: u64 = 1 << 9;
const CR4_OSXMMEXCPT: u64 = 1 << 10;
const CR4_OSXSAVE: u64 = 1 << 18;

/// State components handed to programs: x87, SSE, AVX, and the three AVX-512
/// parts (opmask, upper halves of zmm0-15, zmm16-31). Supervisor and
/// protection-key components stay off.
const USER_COMPONENTS: u64 = 0b1110_0111;

static XSAVE: AtomicBool = AtomicBool::new(false);
static MASK: AtomicU64 = AtomicU64::new(0b11);
static SIZE: AtomicUsize = AtomicUsize::new(512);

/// What `init_cpu` turned on, for the boot log.
pub struct Features {
    pub xsave: bool,
    pub avx: bool,
    pub avx512: bool,
    pub area_bytes: usize,
}

unsafe fn xsetbv(xcr: u32, v: u64) {
    asm!("xsetbv", in("ecx") xcr, in("eax") v as u32, in("edx") (v >> 32) as u32, options(nostack, preserves_flags));
}

/// Turns on the floating point units for ring 3 on the calling CPU. Run on
/// every CPU; the first call decides the save-area size for all of them.
pub fn init_cpu() -> Features {
    let xsave = arch::cpuid(1).ecx & (1 << 26) != 0;
    unsafe {
        arch::write_cr0((arch::read_cr0() | CR0_MP | CR0_NE) & !(CR0_EM | CR0_TS));
        let mut cr4 = arch::read_cr4() | CR4_OSFXSR | CR4_OSXMMEXCPT;
        if xsave {
            cr4 |= CR4_OSXSAVE;
        }
        arch::write_cr4(cr4);
    }
    let (mask, size) = if xsave {
        let leaf = core::arch::x86_64::__cpuid_count(0xD, 0);
        let mask = ((leaf.edx as u64) << 32 | leaf.eax as u64) & USER_COMPONENTS;
        unsafe { xsetbv(0, mask) };
        // EBX now gives the area size for exactly the components switched on.
        (mask, core::arch::x86_64::__cpuid_count(0xD, 0).ebx as usize)
    } else {
        (0b11, 512)
    };
    XSAVE.store(xsave, Ordering::Relaxed);
    MASK.store(mask, Ordering::Relaxed);
    SIZE.fetch_max(size, Ordering::Relaxed);
    Features { xsave, avx: mask & 0b100 != 0, avx512: mask & 0b1110_0000 == 0b1110_0000, area_bytes: size }
}

/// One thread's saved registers. 64-byte aligned, as XSAVE requires.
pub struct FpuArea {
    ptr: *mut u8,
    layout: Layout,
}

unsafe impl Send for FpuArea {}
unsafe impl Sync for FpuArea {}

impl FpuArea {
    /// A fresh area: loading it gives a program the power-on register state
    /// (all zero, x87 control word 0x37F, MXCSR 0x1F80 with every exception
    /// masked), so nothing leaks from whoever ran before.
    pub fn new() -> Option<Self> {
        let layout = Layout::from_size_align(SIZE.load(Ordering::Relaxed).max(576), 64).ok()?;
        let ptr = unsafe { alloc_zeroed(layout) };
        if ptr.is_null() {
            return None;
        }
        unsafe {
            (ptr as *mut u16).write(0x037F); // FCW
            (ptr.add(24) as *mut u32).write(0x1F80); // MXCSR
        }
        // The XSAVE header (bytes 512..576) stays zero: XRSTOR then puts every
        // component in its initial state, taking only MXCSR from the area.
        Some(Self { ptr, layout })
    }

    /// Stores the CPU's current user registers here.
    pub fn save(&self) {
        let mask = MASK.load(Ordering::Relaxed);
        unsafe {
            if XSAVE.load(Ordering::Relaxed) {
                asm!("xsave64 [{}]", in(reg) self.ptr, in("eax") mask as u32, in("edx") (mask >> 32) as u32, options(nostack, preserves_flags));
            } else {
                asm!("fxsave64 [{}]", in(reg) self.ptr, options(nostack, preserves_flags));
            }
        }
    }

    /// Loads the registers saved here into the CPU.
    pub fn restore(&self) {
        let mask = MASK.load(Ordering::Relaxed);
        unsafe {
            if XSAVE.load(Ordering::Relaxed) {
                asm!("xrstor64 [{}]", in(reg) self.ptr, in("eax") mask as u32, in("edx") (mask >> 32) as u32, options(nostack, preserves_flags, readonly));
            } else {
                asm!("fxrstor64 [{}]", in(reg) self.ptr, options(nostack, preserves_flags, readonly));
            }
        }
    }
}

impl Drop for FpuArea {
    fn drop(&mut self) {
        unsafe { dealloc(self.ptr, self.layout) };
    }
}
