//! Global Descriptor Table and Task State Segment for the boot CPU.
//! Ring-3 segments are already in place for the user-mode work to come.

use core::arch::asm;
use core::mem::size_of;
use core::ptr::addr_of;

pub const KERNEL_CODE: u16 = 0x08;
pub const KERNEL_DATA: u16 = 0x10;
#[allow(dead_code)] // used once ring 3 lands
pub const USER_DATA: u16 = 0x18 | 3;
#[allow(dead_code)]
pub const USER_CODE: u16 = 0x20 | 3;
pub const TSS_SEL: u16 = 0x28;

/// IST slot used by the double-fault handler, so a kernel stack overflow
/// still lands on a good stack.
pub const DOUBLE_FAULT_IST: u8 = 1;

#[repr(C, packed)]
struct Tss {
    _reserved0: u32,
    rsp: [u64; 3],
    _reserved1: u64,
    ist: [u64; 7],
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
struct Stack([u8; 16 * 1024]);

static mut DOUBLE_FAULT_STACK: Stack = Stack([0; 16 * 1024]);

static mut TSS: Tss = Tss {
    _reserved0: 0,
    rsp: [0; 3],
    _reserved1: 0,
    ist: [0; 7],
    _reserved2: 0,
    _reserved3: 0,
    iomap_base: size_of::<Tss>() as u16,
};

static mut GDT: [u64; 7] = [
    0,
    0x00af_9a00_0000_ffff, // 0x08 kernel code: 64-bit, DPL0
    0x00cf_9200_0000_ffff, // 0x10 kernel data
    0x00cf_f200_0000_ffff, // 0x18 user data, DPL3
    0x00af_fa00_0000_ffff, // 0x20 user code: 64-bit, DPL3
    0,                     // 0x28 TSS (low)
    0,                     //      TSS (high)
];

pub fn init() {
    unsafe {
        let stack_top = addr_of!(DOUBLE_FAULT_STACK) as u64 + size_of::<Stack>() as u64;
        TSS.ist[(DOUBLE_FAULT_IST - 1) as usize] = stack_top;

        let base = addr_of!(TSS) as u64;
        let limit = (size_of::<Tss>() - 1) as u64;
        GDT[5] = (limit & 0xffff)
            | ((base & 0xff_ffff) << 16)
            | (0x89 << 40) // present, 64-bit available TSS
            | (((limit >> 16) & 0xf) << 48)
            | (((base >> 24) & 0xff) << 56);
        GDT[6] = base >> 32;

        let ptr = DescriptorTablePointer {
            limit: (size_of::<[u64; 7]>() - 1) as u16,
            base: addr_of!(GDT) as u64,
        };
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
