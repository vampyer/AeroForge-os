//! Runs the kernel's NTFS driver on the build machine, against an NTFS
//! image file: changes it as told on standard input (one command a line)
//! and checks what it leaves behind. tools/ntfs-write-test.sh drives it,
//! and checks the result with ntfsprogs too. Commands, fields split by |:
//!   w|<path>|<size>|<seed>  write a file of a test pattern
//!   m|<path>                make a folder
//!   d|<path>                delete a file or an empty folder
//!   l|<path>                count what a folder holds
//!   r|<path>                read a file
//!   c|<path>|<size>|<seed>  check a file holds that pattern
//!   v|/                     check every folder's index
//!   x|/                     stop half way through a change (a crash)
#![allow(dead_code)]
extern crate alloc;

mod arch {
    pub unsafe fn outb(_: u16, _: u8) {}
    pub unsafe fn inb(_: u16) -> u8 { 0 }
    pub fn without_interrupts<R>(f: impl FnOnce() -> R) -> R { f() }
}
#[path = "../../../kernel/src/rtc.rs"]
mod rtc;
mod sync {
    pub struct SleepMutex<T>(std::sync::Mutex<T>);
    impl<T> SleepMutex<T> {
        pub const fn new(v: T) -> Self { Self(std::sync::Mutex::new(v)) }
        pub fn lock(&self) -> std::sync::MutexGuard<'_, T> { self.0.lock().unwrap() }
    }
}
mod block {
    use std::io::{Read, Seek, SeekFrom, Write};
    pub trait BlockDevice: Send + Sync {
        fn name(&self) -> &str;
        fn block_size(&self) -> u32;
        fn block_count(&self) -> u64;
        fn read(&self, lba: u64, buf: &mut [u8]) -> Result<(), &'static str>;
        fn write(&self, lba: u64, buf: &[u8]) -> Result<(), &'static str>;
        fn flush(&self) -> Result<(), &'static str>;
    }
    pub struct FileDev(pub std::sync::Mutex<std::fs::File>, pub u64);
    impl BlockDevice for FileDev {
        fn name(&self) -> &str { "img" }
        fn block_size(&self) -> u32 { 512 }
        fn block_count(&self) -> u64 { self.1 / 512 }
        fn read(&self, lba: u64, buf: &mut [u8]) -> Result<(), &'static str> {
            if (lba * 512 + buf.len() as u64) > self.1 { return Err("read past end"); }
            let mut f = self.0.lock().unwrap();
            f.seek(SeekFrom::Start(lba * 512)).unwrap();
            f.read_exact(buf).map_err(|_| "io")
        }
        fn write(&self, lba: u64, buf: &[u8]) -> Result<(), &'static str> {
            assert!(buf.len() % 512 == 0);
            if (lba * 512 + buf.len() as u64) > self.1 { return Err("write past end"); }
            let mut f = self.0.lock().unwrap();
            f.seek(SeekFrom::Start(lba * 512)).unwrap();
            f.write_all(buf).map_err(|_| "io")
        }
        fn flush(&self) -> Result<(), &'static str> { Ok(()) }
    }
}
mod vfs {
    pub struct DirEntry { pub name: String, pub is_dir: bool, pub size: u64, pub modified: u64 }
    pub struct Deleted { pub name: String, pub is_dir: bool, pub size: u64, pub modified: u64, pub slot: u32, pub whole: bool }
    pub trait Volume {
        fn kind(&self) -> &'static str;
        fn label(&self) -> &str;
        fn dev(&self) -> &std::sync::Arc<dyn crate::block::BlockDevice>;
        fn read_only(&self) -> bool { false }
        fn set_writable(&self, _on: bool) -> Result<(), &'static str> { Err("x") }
        fn list(&self, path: &str) -> Result<Vec<DirEntry>, &'static str>;
        fn read_file(&self, path: &str, limit: usize) -> Result<Vec<u8>, &'static str>;
        fn write_file(&self, path: &str, data: &[u8]) -> Result<(), &'static str>;
        fn create_dir(&self, path: &str) -> Result<(), &'static str>;
        fn remove(&self, path: &str) -> Result<(), &'static str>;
    }
}
#[path = "../../../kernel/src/ntfs.rs"]
mod ntfs;

use vfs::Volume;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let img = &args[1];
    let f = std::fs::OpenOptions::new().read(true).write(true).open(img).unwrap();
    let len = f.metadata().unwrap().len();
    let dev: std::sync::Arc<dyn block::BlockDevice> = std::sync::Arc::new(block::FileDev(std::sync::Mutex::new(f), len));
    let v = ntfs::NtfsVolume::mount(dev).unwrap();
    println!("unclean: {:?}", v.unclean());
    println!("writable: {:?}", v.set_writable(true));
    // Commands from stdin: w path size seed | d path | m path | r path | l path
    let mut line = String::new();
    while { line.clear(); std::io::stdin().read_line(&mut line).unwrap() > 0 } {
        let parts: Vec<&str> = line.trim_end().splitn(4, '|').collect();
        let r = match parts[0] {
            "w" => {
                let size: usize = parts[2].parse().unwrap();
                let seed: u8 = parts[3].parse().unwrap();
                let data: Vec<u8> = (0..size).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect();
                v.write_file(parts[1], &data).map(|_| String::new())
            }
            "d" => v.remove(parts[1]).map(|_| String::new()),
            "m" => v.create_dir(parts[1]).map(|_| String::new()),
            "c" => {
                let size: usize = parts[2].parse().unwrap();
                let seed: u8 = parts[3].parse().unwrap();
                v.read_file(parts[1], 1 << 30).map(|d| {
                    let ok = d.len() == size && d.iter().enumerate().all(|(i, &b)| b == (i as u8).wrapping_mul(31).wrapping_add(seed));
                    String::from(if ok { "holds what was written" } else { "DIFFERS" })
                })
            }
            "x" => v.crash().map(|_| String::from("stopped half way")),
            "v" => v.verify().map(|n| format!("{} names, all in order", n)),
            "l" => v.list(parts[1]).map(|l| format!("{} entries", l.len())),
            "r" => v.read_file(parts[1], 1 << 30).map(|d| format!("{} bytes", d.len())),
            _ => Ok(String::from("?")),
        };
        match r {
            Ok(s) if s.is_empty() => {}
            Ok(s) => println!("{} {}: {}", parts[0], parts[1], s),
            Err(e) => println!("{} {}: ERROR {}", parts[0], parts[1], e),
        }
    }
}
