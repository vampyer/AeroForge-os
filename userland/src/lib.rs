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
    pub const AUDIO_WRITE: u64 = 16;
    pub const AUDIO_QUEUED: u64 = 17;
    pub const GAMEPAD_READ: u64 = 18;
    pub const FILE_WRITE: u64 = 19;
    pub const FILE_DELETE: u64 = 20;
    pub const DIR_CREATE: u64 = 21;
}

pub mod rights {
    pub const SEND: u64 = 1;
    pub const RECV: u64 = 2;
}

pub const NO_HANDLE: u64 = u64::MAX;

/// Error codes the system calls return.
pub const E_BADHANDLE: i64 = -1;
pub const E_FAULT: i64 = -2;
pub const E_NOTFOUND: i64 = -3;
pub const E_RIGHTS: i64 = -4;
pub const E_FULL: i64 = -5;
pub const E_INVAL: i64 = -6;
pub const E_EXISTS: i64 = -7;

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

/// Creates a file, or replaces everything it held, with `data` (at most 8 MiB).
pub fn write_file(path: &str, data: &[u8]) -> Result<usize, i64> {
    check(unsafe {
        syscall(sys::FILE_WRITE, path.as_ptr() as u64, path.len() as u64, data.as_ptr() as u64, data.len() as u64)
    })
    .map(|n| n as usize)
}

/// Deletes a file or an empty directory.
pub fn delete_file(path: &str) -> Result<(), i64> {
    check(unsafe { syscall(sys::FILE_DELETE, path.as_ptr() as u64, path.len() as u64, 0, 0) }).map(|_| ())
}

/// Creates a directory.
pub fn create_dir(path: &str) -> Result<(), i64> {
    check(unsafe { syscall(sys::DIR_CREATE, path.as_ptr() as u64, path.len() as u64, 0, 0) }).map(|_| ())
}

/// Sound: 48 kHz, 16-bit, stereo. Each program gets its own stream; the
/// kernel mixes them into the selected output.
pub mod audio {
    use super::*;

    pub const RATE: u32 = 48_000;

    /// Queues interleaved left/right samples; returns how many frames (pairs)
    /// were taken. Never blocks: when it returns fewer, wait a little and
    /// write the rest. Err if there is no sound output.
    pub fn write(interleaved: &[i16]) -> Result<usize, i64> {
        check(unsafe { syscall(sys::AUDIO_WRITE, interleaved.as_ptr() as u64, (interleaved.len() / 2) as u64, 0, 0) })
            .map(|n| n as usize)
    }

    /// Writes all of `interleaved`, waiting while the queue is full.
    pub fn write_all(mut interleaved: &[i16]) -> Result<(), i64> {
        while interleaved.len() >= 2 {
            let n = write(interleaved)?;
            interleaved = &interleaved[2 * n..];
            if !interleaved.is_empty() {
                sleep_ms(10);
            }
        }
        Ok(())
    }

    /// Frames still waiting to be heard.
    pub fn queued() -> Result<usize, i64> {
        check(unsafe { syscall(sys::AUDIO_QUEUED, 0, 0, 0, 0) }).map(|n| n as usize)
    }

    /// Waits until everything written has been played.
    pub fn drain() {
        while queued().is_ok_and(|n| n > 0) {
            sleep_ms(10);
        }
    }
}

/// Gamepads (Bluetooth and USB). Each read gives the pad's raw state and
/// the same pad in the Xbox 360 layout; `exact` says whether that layout is
/// known (XInput pads) or guessed.
pub mod gamepad {
    use super::*;

    pub const DPAD_UP: u16 = 0x0001;
    pub const DPAD_DOWN: u16 = 0x0002;
    pub const DPAD_LEFT: u16 = 0x0004;
    pub const DPAD_RIGHT: u16 = 0x0008;
    pub const START: u16 = 0x0010;
    pub const BACK: u16 = 0x0020;
    pub const LEFT_THUMB: u16 = 0x0040;
    pub const RIGHT_THUMB: u16 = 0x0080;
    pub const LEFT_SHOULDER: u16 = 0x0100;
    pub const RIGHT_SHOULDER: u16 = 0x0200;
    pub const GUIDE: u16 = 0x0400;
    pub const A: u16 = 0x1000;
    pub const B: u16 = 0x2000;
    pub const X: u16 = 0x4000;
    pub const Y: u16 = 0x8000;

    /// Button bits with their usual names, in display order.
    pub const BUTTON_NAMES: [(u16, &str); 11] = [(A, "A"), (B, "B"), (X, "X"), (Y, "Y"), (LEFT_SHOULDER, "LB"),
        (RIGHT_SHOULDER, "RB"), (BACK, "Back"), (START, "Start"), (LEFT_THUMB, "LS"), (RIGHT_THUMB, "RS"),
        (GUIDE, "Guide")];

    /// Same layout as the kernel's gamepad::State.
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct State {
        pub name: [u8; 32],
        pub connected: u8,
        /// 1 = Bluetooth, 2 = USB.
        pub link: u8,
        /// 1 if the Xbox layout fields are exact, 0 if guessed.
        pub exact: u8,
        /// Raw hat: 0 = up, clockwise to 7 = up-left, 8 = centred.
        pub hat: u8,
        /// Raw buttons: bit n = button n + 1.
        pub buttons: u32,
        pub reports: u32,
        /// Raw axes present (bit per X Y Z Rx Ry Rz Gas Brake).
        pub axis_mask: u8,
        pub _pad: [u8; 3],
        /// Raw axes, -127 to 127.
        pub axes: [i16; 8],
        /// Xbox layout: the constants above.
        pub xbuttons: u16,
        pub left_trigger: u8,
        pub right_trigger: u8,
        /// Left X, left Y, right X, right Y: -32767 to 32767, up positive.
        pub thumbs: [i16; 4],
    }

    impl State {
        pub fn name(&self) -> &str {
            let end = self.name.iter().position(|&b| b == 0).unwrap_or(self.name.len());
            core::str::from_utf8(&self.name[..end]).unwrap_or("?")
        }
    }

    /// Reads gamepad `index` (0, 1, ...). Returns its state and how many
    /// gamepads there are; Err when there is no such gamepad.
    pub fn read(index: usize) -> Result<(State, usize), i64> {
        let mut s = core::mem::MaybeUninit::<State>::zeroed();
        let n = check(unsafe { syscall(sys::GAMEPAD_READ, index as u64, s.as_mut_ptr() as u64, 0, 0) })?;
        Ok((unsafe { s.assume_init() }, n as usize))
    }
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
