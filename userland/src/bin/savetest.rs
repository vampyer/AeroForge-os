//! savetest: saves files the way a game saves its progress, through the file
//! system calls: makes a folder, writes save slots with long names (enough
//! of them that the folder has to grow), reads them back, overwrites one with
//! a smaller save, and deletes the rest. The boot test checks what is left
//! on the disk image afterwards with mtools and fsck.fat.

#![no_std]
#![no_main]

use core::ptr::addr_of_mut;

aero::entry!(main);

const BIG: usize = 20_000; // 40 clusters on the test disk
const SMALL: usize = 1_500;
const SLOTS: usize = 12; // 3 directory entries each: the folder grows past two clusters

static mut DATA: [u8; BIG] = [0; BIG];
static mut BACK: [u8; BIG + 1] = [0; BIG + 1];

/// The bytes save `seed` holds (tools/check-fat-files.py makes the same).
fn fill(buf: &mut [u8], seed: u32) {
    for (i, b) in buf.iter_mut().enumerate() {
        *b = (i as u32).wrapping_mul(31).wrapping_add(i as u32 / 251).wrapping_add(seed * 7) as u8;
    }
}

fn slot_path(n: usize) -> aero::LineWriter {
    aero::format_buf!("/saves/Slot {} - Forest Temple.sav", n)
}

fn as_str(w: &aero::LineWriter) -> &str {
    core::str::from_utf8(w.as_bytes()).unwrap_or("")
}

fn fail(what: &str, e: i64) -> i64 {
    aero::println!("[savetest] FAILED: {} ({})", what, e);
    1
}

fn main() -> i64 {
    let data = unsafe { &mut *addr_of_mut!(DATA) };
    let back = unsafe { &mut *addr_of_mut!(BACK) };
    if let Err(e) = aero::create_dir("/saves") {
        if e != aero::E_EXISTS {
            return fail("create /saves", e);
        }
    }
    for n in 1..=SLOTS {
        fill(data, n as u32);
        let name = slot_path(n);
        let path = as_str(&name);
        let len = if n == 1 { BIG } else { SMALL };
        if let Err(e) = aero::write_file(path, &data[..len]) {
            return fail("write a save", e);
        }
        match aero::read_file(path, back) {
            Ok(got) if got == len && back[..len] == data[..len] => {}
            Ok(_) => return fail("read back a save", 0),
            Err(e) => return fail("read back a save", e),
        }
    }
    // Overwrite slot 1 with a smaller save: its old clusters must be freed.
    fill(data, 99);
    let name = slot_path(1);
    let path = as_str(&name);
    if let Err(e) = aero::write_file(path, &data[..SMALL]) {
        return fail("overwrite a save", e);
    }
    match aero::read_file(path, back) {
        Ok(got) if got == SMALL && back[..SMALL] == data[..SMALL] => {}
        Ok(_) => return fail("read back the overwritten save", 0),
        Err(e) => return fail("read back the overwritten save", e),
    }
    // Delete slots 3 and up.
    for n in 3..=SLOTS {
        let name = slot_path(n);
        let path = as_str(&name);
        if let Err(e) = aero::delete_file(path) {
            return fail("delete a save", e);
        }
        if aero::read_file(path, back) != Err(aero::E_NOTFOUND) {
            return fail("a deleted save is still there", 0);
        }
    }
    // The boot configuration is off limits to programs.
    if aero::write_file("/system/session.cfg", b"oops") != Err(aero::E_RIGHTS) {
        return fail("a program could overwrite /system/session.cfg", 0);
    }
    aero::println!("[savetest] saved {} slots, read them back, overwrote slot 1 and deleted slots 3-{}: OK", SLOTS, SLOTS);
    0
}
