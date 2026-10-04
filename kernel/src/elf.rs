//! ELF64 loader for user programs (static, non-PIE executables).

use crate::memory::{self, flags};

const PT_LOAD: u32 = 1;
const PF_W: u32 = 2;

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn u64_at(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

/// Maps every PT_LOAD segment of `image` into `pml4`; returns the entry point.
pub fn load(image: &[u8], pml4: u64) -> Result<u64, &'static str> {
    if image.len() < 64 || &image[..4] != b"\x7fELF" || image[4] != 2 || image[5] != 1 {
        return Err("not a 64-bit little-endian ELF");
    }
    if u16_at(image, 18) != 0x3E || u16_at(image, 16) != 2 {
        return Err("not an x86-64 executable");
    }
    let entry = u64_at(image, 24);
    let phoff = u64_at(image, 32) as usize;
    let phentsize = u16_at(image, 54) as usize;
    let phnum = u16_at(image, 56) as usize;

    for i in 0..phnum {
        let ph = phoff + i * phentsize;
        if ph + 56 > image.len() || u32_at(image, ph) != PT_LOAD {
            continue;
        }
        let pflags = u32_at(image, ph + 4);
        let offset = u64_at(image, ph + 8) as usize;
        let vaddr = u64_at(image, ph + 16);
        let filesz = u64_at(image, ph + 32) as usize;
        let memsz = u64_at(image, ph + 40);
        if vaddr >= 0x0000_8000_0000_0000 || vaddr + memsz >= 0x0000_8000_0000_0000 {
            return Err("segment outside user space");
        }
        if offset + filesz > image.len() {
            return Err("segment past end of file");
        }

        let pte = flags::USER | if pflags & PF_W != 0 { flags::WRITABLE } else { 0 };
        let start = vaddr & !(memory::PAGE_SIZE - 1);
        let end = (vaddr + memsz + memory::PAGE_SIZE - 1) & !(memory::PAGE_SIZE - 1);
        let mut page = start;
        while page < end {
            let frame = memory::alloc_frame_zeroed().ok_or("out of memory")?;
            memory::map_page(pml4, page, frame, pte)?;
            // Copy the part of the file image that overlaps this page.
            let seg_lo = vaddr.max(page);
            let seg_hi = (vaddr + filesz as u64).min(page + memory::PAGE_SIZE);
            if seg_hi > seg_lo {
                let src = &image[offset + (seg_lo - vaddr) as usize..offset + (seg_hi - vaddr) as usize];
                let dst = memory::phys_to_virt(frame) + (seg_lo - page);
                unsafe { core::ptr::copy_nonoverlapping(src.as_ptr(), dst as *mut u8, src.len()) };
            }
            page += memory::PAGE_SIZE;
        }
    }
    Ok(entry)
}
