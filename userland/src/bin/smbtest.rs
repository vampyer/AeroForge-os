//! smbtest: the SMB2 client against the Samba server the test runs on the
//! host (tools/smb-server.sh, 10.0.2.2 port 4450 in QEMU's user network,
//! share "aero", signing required).
//!
//! It waits for a DHCP address, then checks that a wrong password is
//! refused, signs in, lists the share, reads a file, writes one (bigger
//! than one 64 KiB piece) and reads it back, makes a folder, renames the
//! file into it, deletes both and lists the share again.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;

use aero::smb::Client;
use aero::{net, println};

aero::entry!(main);

const SERVER: &str = "10.0.2.2:4450";
const SHARE: &str = "aero";
const USER: &str = "aerotest";
const PASSWORD: &str = "Forge-pass-7";

fn fail(what: &str, e: i64) -> i64 {
    println!("[smbtest] FAILED: {} ({})", what, e);
    1
}

fn names(c: &mut Client, path: &str) -> Result<Vec<alloc::string::String>, i64> {
    let mut n: Vec<_> = c.list(path)?.into_iter().map(|e| if e.is_dir { e.name + "/" } else { e.name }).collect();
    n.sort();
    Ok(n)
}

fn main() -> i64 {
    for _ in 0..100 {
        if net::info().is_ok() {
            break;
        }
        aero::sleep_ms(100);
    }
    match Client::connect(SERVER, SHARE, USER, "wrong") {
        Ok(_) => {
            println!("[smbtest] FAILED: a wrong password was let in");
            return 1;
        }
        Err(e) => println!("[smbtest] wrong password refused: {}", e),
    }
    let mut c = match Client::connect(SERVER, SHARE, USER, PASSWORD) {
        Ok(c) => c,
        Err(e) => {
            println!("[smbtest] FAILED: could not connect: {}", e);
            return 1;
        }
    };
    println!("[smbtest] signed in to \\\\{}\\{} as {}", SERVER, SHARE, USER);
    match names(&mut c, "") {
        Ok(n) => println!("[smbtest] share holds: {}", n.join(" | ")),
        Err(e) => return fail("listing the share", e),
    }
    match c.read("hello.txt", 4096) {
        Ok(d) => println!("[smbtest] hello.txt: {}", core::str::from_utf8(&d).unwrap_or("?").trim_end()),
        Err(e) => return fail("reading hello.txt", e),
    }
    let data: Vec<u8> = (0..150_000u32).map(|i| (i * 7 % 251) as u8).collect();
    if let Err(e) = c.write("big.bin", &data) {
        return fail("writing big.bin", e);
    }
    match c.read("big.bin", 1 << 20) {
        Ok(d) if d == data => println!("[smbtest] wrote and read back {} bytes", d.len()),
        Ok(d) => {
            println!("[smbtest] FAILED: big.bin read back {} bytes, not the {} written", d.len(), data.len());
            return 1;
        }
        Err(e) => return fail("reading big.bin back", e),
    }
    if let Err(e) = c.create_dir("Saves") {
        return fail("making Saves", e);
    }
    if let Err(e) = c.rename("big.bin", "Saves/moved.bin") {
        return fail("renaming big.bin", e);
    }
    match names(&mut c, "Saves") {
        Ok(n) => println!("[smbtest] Saves holds: {}", n.join(" | ")),
        Err(e) => return fail("listing Saves", e),
    }
    if let Err(e) = c.delete("Saves/moved.bin", false) {
        return fail("deleting Saves/moved.bin", e);
    }
    if let Err(e) = c.delete("Saves", true) {
        return fail("deleting Saves", e);
    }
    match names(&mut c, "/") {
        Ok(n) => println!("[smbtest] share holds: {}", n.join(" | ")),
        Err(e) => return fail("listing the share again", e),
    }
    println!("[smbtest] sign-in, list, read, write, folders, rename and delete on an SMB share: OK");
    0
}
