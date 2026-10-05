//! Thin wrappers over x86-64 instructions the kernel needs.

use core::arch::asm;

#[inline]
pub unsafe fn inb(port: u16) -> u8 {
    let v: u8;
    asm!("in al, dx", out("al") v, in("dx") port, options(nomem, nostack, preserves_flags));
    v
}

#[inline]
pub unsafe fn outb(port: u16, v: u8) {
    asm!("out dx, al", in("dx") port, in("al") v, options(nomem, nostack, preserves_flags));
}

/// A write to an unused port: roughly 1 µs, for old devices like the PIC.
#[inline]
pub unsafe fn io_wait() {
    outb(0x80, 0);
}

#[inline]
pub fn hlt() {
    unsafe { asm!("hlt", options(nomem, nostack, preserves_flags)) }
}

#[inline]
pub fn enable_interrupts() {
    unsafe { asm!("sti", options(nomem, nostack)) }
}

#[inline]
pub fn disable_interrupts() {
    unsafe { asm!("cli", options(nomem, nostack)) }
}

#[inline]
pub fn interrupts_enabled() -> bool {
    let flags: u64;
    unsafe { asm!("pushfq; pop {}", out(reg) flags, options(nomem, preserves_flags)) };
    flags & (1 << 9) != 0
}

/// Runs `f` with interrupts masked, restoring the previous state after.
pub fn without_interrupts<T>(f: impl FnOnce() -> T) -> T {
    let was_enabled = interrupts_enabled();
    if was_enabled {
        disable_interrupts();
    }
    let r = f();
    if was_enabled {
        enable_interrupts();
    }
    r
}

pub fn halt_forever() -> ! {
    loop {
        disable_interrupts();
        hlt();
    }
}

#[inline]
pub fn read_cr2() -> u64 {
    let v: u64;
    unsafe { asm!("mov {}, cr2", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v
}

#[inline]
pub fn read_cr3() -> u64 {
    let v: u64;
    unsafe { asm!("mov {}, cr3", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v
}

#[inline]
pub unsafe fn invlpg(addr: u64) {
    asm!("invlpg [{}]", in(reg) addr, options(nostack, preserves_flags));
}

#[inline]
pub fn rdtsc() -> u64 {
    let lo: u32;
    let hi: u32;
    unsafe { asm!("rdtsc", out("eax") lo, out("edx") hi, options(nomem, nostack, preserves_flags)) };
    ((hi as u64) << 32) | lo as u64
}

pub struct CpuidResult {
    pub eax: u32,
    pub ebx: u32,
    pub ecx: u32,
    pub edx: u32,
}

pub fn cpuid(leaf: u32) -> CpuidResult {
    let r = core::arch::x86_64::__cpuid_count(leaf, 0);
    CpuidResult { eax: r.eax, ebx: r.ebx, ecx: r.ecx, edx: r.edx }
}

/// The CPU brand string, e.g. "QEMU Virtual CPU version 2.5+".
pub fn cpu_brand(buf: &mut [u8; 48]) -> &str {
    if cpuid(0x8000_0000).eax < 0x8000_0004 {
        return "unknown x86-64 CPU";
    }
    for (i, leaf) in (0x8000_0002u32..=0x8000_0004).enumerate() {
        let r = cpuid(leaf);
        for (j, reg) in [r.eax, r.ebx, r.ecx, r.edx].iter().enumerate() {
            buf[i * 16 + j * 4..i * 16 + j * 4 + 4].copy_from_slice(&reg.to_le_bytes());
        }
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(48);
    core::str::from_utf8(&buf[..end]).unwrap_or("?").trim()
}

pub const MSR_APIC_BASE: u32 = 0x1B;
pub const MSR_GS_BASE: u32 = 0xC000_0101;
pub const MSR_KERNEL_GS_BASE: u32 = 0xC000_0102;

#[inline]
pub unsafe fn rdmsr(msr: u32) -> u64 {
    let (lo, hi): (u32, u32);
    asm!("rdmsr", in("ecx") msr, out("eax") lo, out("edx") hi, options(nomem, nostack, preserves_flags));
    ((hi as u64) << 32) | lo as u64
}

#[inline]
pub unsafe fn wrmsr(msr: u32, v: u64) {
    asm!("wrmsr", in("ecx") msr, in("eax") v as u32, in("edx") (v >> 32) as u32, options(nostack, preserves_flags));
}

#[inline]
pub unsafe fn write_cr3(v: u64) {
    asm!("mov cr3, {}", in(reg) v, options(nostack, preserves_flags));
}

#[inline]
pub fn read_cr0() -> u64 {
    let v: u64;
    unsafe { asm!("mov {}, cr0", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v
}

#[inline]
pub unsafe fn write_cr0(v: u64) {
    asm!("mov cr0, {}", in(reg) v, options(nostack, preserves_flags));
}

#[inline]
pub fn read_cr4() -> u64 {
    let v: u64;
    unsafe { asm!("mov {}, cr4", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v
}

#[inline]
pub unsafe fn write_cr4(v: u64) {
    asm!("mov cr4, {}", in(reg) v, options(nostack, preserves_flags));
}

pub const MSR_EFER: u32 = 0xC000_0080;

/// Sets EFLAGS.AC: lets the kernel touch user pages while SMAP is on.
#[inline]
pub unsafe fn stac() {
    // Not `nomem`: the compiler must not move user memory accesses across it.
    asm!("stac", options(nostack));
}

/// Clears EFLAGS.AC: user pages are off limits to the kernel again.
#[inline]
pub unsafe fn clac() {
    // Not `nomem`: the compiler must not move user memory accesses across it.
    asm!("clac", options(nostack));
}

/// A hardware random number, if the CPU has RDRAND and it delivers.
pub fn rdrand() -> Option<u64> {
    if cpuid(1).ecx & (1 << 30) == 0 {
        return None;
    }
    for _ in 0..10 {
        let v: u64;
        let ok: u8;
        unsafe { asm!("rdrand {}", "setc {}", out(reg) v, out(reg_byte) ok, options(nomem, nostack)) };
        if ok != 0 {
            return Some(v);
        }
    }
    None
}
