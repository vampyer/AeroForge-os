//! libaero: the AeroForge system call interface for user programs.
//! Numbers and conventions must match kernel/src/syscall.rs.

#![no_std]

use core::arch::asm;
use core::fmt::{self, Write};

pub mod sys {
    pub const EXIT: u64 = 0;
    pub const WRITE: u64 = 1;
    pub const YIELD: u64 = 2;
    pub const GETPID: u64 = 3;
    pub const SLEEP_MS: u64 = 4;
    pub const CPU_ID: u64 = 5;
    pub const UPTIME_MS: u64 = 6;
    pub const SPAWN: u64 = 7;
    pub const PORT_CREATE: u64 = 8;
    pub const PORT_PUBLISH: u64 = 9;
    pub const PORT_LOOKUP: u64 = 10;
    pub const PORT_SEND: u64 = 11;
    pub const PORT_RECV: u64 = 12;
    pub const HANDLE_CLOSE: u64 = 13;
    pub const HANDLE_DUP: u64 = 14;
    pub const FILE_READ: u64 = 15;
}

pub mod rights {
    pub const SEND: u64 = 1;
    pub const RECV: u64 = 2;
}

pub const NO_HANDLE: u64 = u64::MAX;

#[inline(always)]
pub unsafe fn syscall(n: u64, a0: u64, a1: u64, a2: u64, a3: u64) -> i64 {
    let r: u64;
    asm!("int 0x80", inlateout("rax") n => r, in("rdi") a0, in("rsi") a1, in("rdx") a2, in("r10") a3,
         options(nostack));
    r as i64
}

fn check(r: i64) -> Result<u64, i64> {
    if r < 0 { Err(r) } else { Ok(r as u64) }
}

pub fn exit(code: i64) -> ! {
    unsafe { syscall(sys::EXIT, code as u64, 0, 0, 0) };
    loop {}
}

pub fn write(s: &[u8]) {
    unsafe { syscall(sys::WRITE, s.as_ptr() as u64, s.len() as u64, 0, 0) };
}

pub fn yield_now() {
    unsafe { syscall(sys::YIELD, 0, 0, 0, 0) };
}

pub fn getpid() -> u64 {
    unsafe { syscall(sys::GETPID, 0, 0, 0, 0) as u64 }
}

pub fn sleep_ms(ms: u64) {
    unsafe { syscall(sys::SLEEP_MS, ms, 0, 0, 0) };
}

pub fn cpu_id() -> u64 {
    unsafe { syscall(sys::CPU_ID, 0, 0, 0, 0) as u64 }
}

pub fn uptime_ms() -> u64 {
    unsafe { syscall(sys::UPTIME_MS, 0, 0, 0, 0) as u64 }
}

pub fn spawn(program: &str) -> Result<u64, i64> {
    check(unsafe { syscall(sys::SPAWN, program.as_ptr() as u64, program.len() as u64, 0, 0) })
}

/// Reads a whole file (up to `buf.len()` bytes) from the mounted disk.
pub fn read_file(path: &str, buf: &mut [u8]) -> Result<usize, i64> {
    check(unsafe {
        syscall(sys::FILE_READ, path.as_ptr() as u64, path.len() as u64, buf.as_mut_ptr() as u64, buf.len() as u64)
    })
    .map(|n| n as usize)
}

/// A handle to a kernel object, closed when dropped.
pub struct Handle(pub u64);

impl Handle {
    /// A copy carrying the given (equal or smaller) rights.
    pub fn dup(&self, rights: u64) -> Result<Handle, i64> {
        check(unsafe { syscall(sys::HANDLE_DUP, self.0, rights, 0, 0) }).map(Handle)
    }

    /// Gives up ownership without closing (the handle was moved elsewhere).
    pub fn into_raw(self) -> u64 {
        let h = self.0;
        core::mem::forget(self);
        h
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { syscall(sys::HANDLE_CLOSE, self.0, 0, 0, 0) };
    }
}

pub struct Received {
    pub len: usize,
    pub sender: u64,
    pub handle: Option<Handle>,
}

#[repr(C)]
struct RecvInfo {
    len: u64,
    sender_pid: u64,
    handle: u64,
}

pub mod port {
    use super::*;

    pub fn create() -> Result<Handle, i64> {
        check(unsafe { syscall(sys::PORT_CREATE, 0, 0, 0, 0) }).map(Handle)
    }

    pub fn publish(port: &Handle, name: &str) -> Result<(), i64> {
        check(unsafe { syscall(sys::PORT_PUBLISH, port.0, name.as_ptr() as u64, name.len() as u64, 0) }).map(|_| ())
    }

    pub fn lookup(name: &str) -> Result<Handle, i64> {
        check(unsafe { syscall(sys::PORT_LOOKUP, name.as_ptr() as u64, name.len() as u64, 0, 0) }).map(Handle)
    }

    /// Sends `data`, optionally moving `transfer` to the receiver.
    pub fn send(port: &Handle, data: &[u8], transfer: Option<Handle>) -> Result<(), i64> {
        let h = transfer.map_or(NO_HANDLE, Handle::into_raw);
        check(unsafe { syscall(sys::PORT_SEND, port.0, data.as_ptr() as u64, data.len() as u64, h) }).map(|_| ())
    }

    /// Blocks until a message arrives.
    pub fn recv(port: &Handle, buf: &mut [u8]) -> Result<Received, i64> {
        let mut info = RecvInfo { len: 0, sender_pid: 0, handle: NO_HANDLE };
        check(unsafe {
            syscall(sys::PORT_RECV, port.0, buf.as_mut_ptr() as u64, buf.len() as u64, &mut info as *mut _ as u64)
        })?;
        Ok(Received {
            len: info.len as usize,
            sender: info.sender_pid,
            handle: (info.handle != NO_HANDLE).then_some(Handle(info.handle)),
        })
    }
}

/// Formats into a stack buffer, then writes it with one system call so lines
/// from different processes don't interleave mid-line.
pub struct LineWriter {
    buf: [u8; 256],
    len: usize,
}

impl LineWriter {
    pub const fn new() -> Self {
        Self { buf: [0; 256], len: 0 }
    }
    pub fn flush(&mut self) {
        write(&self.buf[..self.len]);
        self.len = 0;
    }
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }
}

impl Write for LineWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for &b in s.as_bytes() {
            if self.len == self.buf.len() {
                self.flush();
            }
            self.buf[self.len] = b;
            self.len += 1;
        }
        Ok(())
    }
}

#[macro_export]
macro_rules! println {
    ($($arg:tt)*) => {{
        use core::fmt::Write;
        let mut w = $crate::LineWriter::new();
        let _ = writeln!(w, $($arg)*);
        w.flush();
    }};
}

/// Formats into a fixed buffer (for building messages).
#[macro_export]
macro_rules! format_buf {
    ($($arg:tt)*) => {{
        use core::fmt::Write;
        let mut w = $crate::LineWriter::new();
        let _ = write!(w, $($arg)*);
        w
    }};
}

/// Declares the program's entry point: `aero::entry!(main);`
#[macro_export]
macro_rules! entry {
    ($main:path) => {
        #[no_mangle]
        pub extern "C" fn _start() -> ! {
            let code: i64 = $main();
            $crate::exit(code)
        }
    };
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("[pid {}] panic: {}", getpid(), info);
    exit(-1)
}
