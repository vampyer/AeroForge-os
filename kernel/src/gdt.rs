//! Per-CPU Global Descriptor Table and Task State Segment.
//!
//! Every CPU needs its own TSS: `rsp0` is the kernel stack the CPU switches to
//! when an interrupt or system call arrives from ring 3, and it changes on
//! every context switch.

use alloc::boxed::Box;
use core::arch::asm;
use core::mem::size_of;

pub const KERNEL_CODE: u16 = 0x08;
pub const KERNEL_DATA: u16 = 0x10;
pub const USER_DATA: u16 = 0x18 | 3;
pub const USER_CODE: u16 = 0x20 | 3;
pub const TSS_SEL: u16 = 0x28;

/// IST slot used by the double-fault handler, so a kernel stack overflow
/// still lands on a good stack.
pub const DOUBLE_FAULT_IST: u8 = 1;
const IST_STACK_SIZE: usize = 16 * 1024;

#[repr(C, packed)]
pub struct Tss {
    _reserved0: u32,
    pub rsp: [u64; 3],
    _reserved1: u64,
    pub ist: [u64; 7],
    _reserved2: u64,
    _reserved3: u16,
    iomap_base: u16,
}

#[repr(C, packed)]
struct DescriptorTablePointer {
    limit: u16,
    base: u64,
}

#[repr(C, align(16))]
pub struct CpuTables {
    gdt: [u64; 7],
    pub tss: Tss,
}

impl CpuTables {
    pub fn new() -> Self {
        let ist_stack = Box::leak(alloc::vec![0u8; IST_STACK_SIZE].into_boxed_slice());
        let mut ist = [0u64; 7];
        ist[(DOUBLE_FAULT_IST - 1) as usize] = ist_stack.as_ptr() as u64 + IST_STACK_SIZE as u64;
        Self {
            gdt: [
                0,
                0x00af_9a00_0000_ffff, // 0x08 kernel code: 64-bit, DPL0
                0x00cf_9200_0000_ffff, // 0x10 kernel data
                0x00cf_f200_0000_ffff, // 0x18 user data, DPL3
                0x00af_fa00_0000_ffff, // 0x20 user code: 64-bit, DPL3
                0,                     // 0x28 TSS (low)
                0,                     //      TSS (high)
            ],
            tss: Tss {
                _reserved0: 0,
                rsp: [0; 3],
                _reserved1: 0,
                ist,
                _reserved2: 0,
                _reserved3: 0,
                iomap_base: size_of::<Tss>() as u16,
            },
        }
    }

    /// Loads this GDT and TSS on the calling CPU. `self` must not move afterwards.
    pub fn load(&mut self) {
        let base = &self.tss as *const Tss as u64;
        let limit = (size_of::<Tss>() - 1) as u64;
        self.gdt[5] = (limit & 0xffff)
            | ((base & 0xff_ffff) << 16)
            | (0x89 << 40) // present, 64-bit available TSS
            | (((limit >> 16) & 0xf) << 48)
            | (((base >> 24) & 0xff) << 56);
        self.gdt[6] = base >> 32;

        let ptr = DescriptorTablePointer {
            limit: (size_of::<[u64; 7]>() - 1) as u16,
            base: self.gdt.as_ptr() as u64,
        };
        unsafe {
            asm!("lgdt [{}]", in(reg) &ptr, options(readonly, nostack, preserves_flags));
            // Reload CS with a far return, then the data segments.
            asm!(
                "push {sel}",
                "lea {tmp}, [rip + 2f]",
                "push {tmp}",
                "retfq",
                "2:",
                sel = const KERNEL_CODE as u64,
                tmp = lateout(reg) _,
            );
            asm!(
                "mov ds, {0:x}",
                "mov es, {0:x}",
                "mov ss, {0:x}",
                in(reg) KERNEL_DATA,
                options(nostack, preserves_flags),
            );
            asm!("ltr {0:x}", in(reg) TSS_SEL, options(nostack, preserves_flags));
        }
    }
}
