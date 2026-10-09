//! libaero: the AeroForge system call interface for user programs.
//! Numbers and conventions must match kernel/src/syscall.rs.

#![no_std]

extern crate alloc;

mod rt;
pub use rt::{futex, mem, process, sync, thread};

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
    pub const MEM_MAP: u64 = 22;
    pub const MEM_UNMAP: u64 = 23;
    pub const THREAD_CREATE: u64 = 24;
    pub const THREAD_EXIT: u64 = 25;
    pub const THREAD_JOIN: u64 = 26;
    pub const FUTEX_WAIT: u64 = 27;
    pub const FUTEX_WAKE: u64 = 28;
    pub const PROCESS_WAIT: u64 = 29;
    pub const THREAD_ID: u64 = 30;
    pub const THREAD_PRIORITY: u64 = 31;
    pub const SLEEP_US: u64 = 32;
    pub const CLOCK_US: u64 = 33;
    pub const SOCKET_OPEN: u64 = 34;
    pub const SOCKET_CONNECT: u64 = 35;
    pub const SOCKET_SEND: u64 = 36;
    pub const SOCKET_RECV: u64 = 37;
    pub const SOCKET_LISTEN: u64 = 38;
    pub const SOCKET_ACCEPT: u64 = 39;
    pub const NET_INFO: u64 = 40;
    pub const EVENT_CREATE: u64 = 41;
    pub const EVENT_SET: u64 = 42;
    pub const EVENT_RESET: u64 = 43;
    pub const WAIT_ANY: u64 = 44;
    pub const PROCESS_HANDLE: u64 = 45;
    pub const PROCESS_KILL: u64 = 46;
    pub const DISPLAY_ACQUIRE: u64 = 47;
    pub const DISPLAY_PRESENT: u64 = 48;
    pub const DISPLAY_RELEASE: u64 = 49;
    pub const POINTER: u64 = 50;
    pub const KEYS_READ: u64 = 51;
    pub const TIME: u64 = 52;
    pub const DIR_LIST: u64 = 53;
    pub const MOUSE_SPEED: u64 = 54;
    pub const VOLUMES: u64 = 55;
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
pub const E_AGAIN: i64 = -8;
pub const E_TIMEDOUT: i64 = -9;
/// A connection was refused or reset.
pub const E_CLOSED: i64 = -10;

#[inline(always)]
pub unsafe fn syscall(n: u64, a0: u64, a1: u64, a2: u64, a3: u64) -> i64 {
    let r: u64;
    asm!("syscall", inlateout("rax") n => r, in("rdi") a0, in("rsi") a1, in("rdx") a2, in("r10") a3,
         lateout("rcx") _, lateout("r11") _, options(nostack));
    r as i64
}

fn check(r: i64) -> Result<u64, i64> {
    if r < 0 { Err(r) } else { Ok(r as u64) }
}

/// Ends the whole program, every thread of it.
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

/// Sleeps for `us` microseconds (precise to the timer, not a 10 ms tick).
pub fn sleep_us(us: u64) {
    unsafe { syscall(sys::SLEEP_US, us, 0, 0, 0) };
}

/// Microseconds since boot. For frame pacing: remember a deadline,
/// `sleep_us(deadline - clock_us())`.
pub fn clock_us() -> u64 {
    unsafe { syscall(sys::CLOCK_US, 0, 0, 0, 0) as u64 }
}

/// Wall clock date and time (local time, as the PC's clock keeps it).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DateTime {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

/// The date and time now, or None if the PC's clock can't be read.
pub fn now() -> Option<DateTime> {
    let v = check(unsafe { syscall(sys::TIME, 0, 0, 0, 0) }).ok()?;
    Some(DateTime {
        year: (v >> 40) as u16,
        month: (v >> 32) as u8,
        day: (v >> 24) as u8,
        hour: (v >> 16) as u8,
        minute: (v >> 8) as u8,
        second: v as u8,
    })
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

/// One entry of a directory listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    pub name: alloc::string::String,
    pub is_dir: bool,
    pub size: u64,
}

/// The mouse pointer speed, 1 (slowest) to 10; 5 moves one pixel per
/// mouse count. `set` = Some(n) changes it first.
pub fn mouse_speed(set: Option<u64>) -> Result<u64, i64> {
    check(unsafe { syscall(sys::MOUSE_SPEED, set.unwrap_or(0), 0, 0, 0) })
}

/// The files and directories in `path` ("/" lists the other disks too, as
/// directories). Very large directories are cut off at about 256 KiB of names.
pub fn list_dir(path: &str) -> Result<alloc::vec::Vec<DirEntry>, i64> {
    let mut buf = alloc::vec![0u8; 256 * 1024];
    let n = check(unsafe {
        syscall(sys::DIR_LIST, path.as_ptr() as u64, path.len() as u64, buf.as_mut_ptr() as u64, buf.len() as u64)
    })? as usize;
    let mut out = alloc::vec::Vec::new();
    let mut at = 0;
    while at + 10 <= n {
        let size = u64::from_le_bytes(buf[at + 1..at + 9].try_into().unwrap());
        let len = buf[at + 9] as usize;
        let name = &buf[at + 10..(at + 10 + len).min(n)];
        out.push(DirEntry {
            name: alloc::string::String::from_utf8_lossy(name).into_owned(),
            is_dir: buf[at] & 1 != 0,
            size,
        });
        at += 10 + len;
    }
    Ok(out)
}

/// A mounted volume (a disk partition AeroForge can read).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Volume {
    /// Where its files are: "/" for the first writable volume, "/<device>" for the others.
    pub path: alloc::string::String,
    /// The disk or partition it is on, such as "nvme0n1", "sata0p1" or "usb1p1".
    pub device: alloc::string::String,
    /// "FAT32", "exFAT" or "NTFS".
    pub kind: alloc::string::String,
    pub label: alloc::string::String,
    pub read_only: bool,
    pub size: u64,
    /// Free bytes, when the volume keeps count.
    pub free: Option<u64>,
}

/// The mounted volumes, root first.
pub fn volumes() -> Result<alloc::vec::Vec<Volume>, i64> {
    let mut buf = alloc::vec![0u8; 16 * 1024];
    let n = check(unsafe { syscall(sys::VOLUMES, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0) })? as usize;
    let buf = &buf[..n];
    let mut out = alloc::vec::Vec::new();
    let mut at = 0;
    while at + 17 <= n {
        let read_only = buf[at] & 1 != 0;
        let size = u64::from_le_bytes(buf[at + 1..at + 9].try_into().unwrap());
        let free = u64::from_le_bytes(buf[at + 9..at + 17].try_into().unwrap());
        at += 17;
        let mut texts: [alloc::string::String; 4] = Default::default();
        for t in texts.iter_mut() {
            let len = *buf.get(at).unwrap_or(&0) as usize;
            let end = (at + 1 + len).min(n);
            *t = alloc::string::String::from_utf8_lossy(&buf[(at + 1).min(end)..end]).into_owned();
            at = end;
        }
        let [path, device, kind, label] = texts;
        out.push(Volume { path, device, kind, label, read_only, size, free: if free == u64::MAX { None } else { Some(free) } });
    }
    Ok(out)
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

/// The whole screen for one program at a time. Draw 0x00RRGGBB pixels into
/// your own image and present the parts that changed; the console comes
/// back when the program releases the screen or exits.
pub mod display {
    use super::*;

    /// The screen, while this program owns it.
    pub struct Screen {
        pub width: usize,
        pub height: usize,
    }

    /// Takes the screen. Err if another program has it or there is none.
    pub fn acquire() -> Result<Screen, i64> {
        check(unsafe { syscall(sys::DISPLAY_ACQUIRE, 0, 0, 0, 0) })
            .map(|v| Screen { width: (v >> 32) as usize, height: (v & 0xFFFF_FFFF) as usize })
    }

    impl Screen {
        /// Shows rectangle (x, y, w, h) of `image`, whose rows are `stride`
        /// pixels long and which covers the screen from the top-left.
        pub fn present(&self, image: &[u32], stride: usize, x: usize, y: usize, w: usize, h: usize) -> Result<(), i64> {
            if stride == 0 || image.len() < stride * (y + h).min(self.height) {
                return Err(E_INVAL);
            }
            let rect = x as u64 | (y as u64) << 16 | (w as u64) << 32 | (h as u64) << 48;
            check(unsafe { syscall(sys::DISPLAY_PRESENT, image.as_ptr() as u64, stride as u64, rect, 0) }).map(|_| ())
        }

        /// Shows the whole of `image` (rows of `width` pixels).
        pub fn present_all(&self, image: &[u32]) -> Result<(), i64> {
            self.present(image, self.width, 0, 0, self.width, self.height)
        }
    }

    /// The mouse pointer: position on screen, buttons held (bit 0 = left,
    /// 1 = right, 2 = middle) and how many times the left button has been
    /// pressed (20 bits, wraps), so a quick click between reads is seen.
    /// `doubles` counts the presses that were the second of a double-click
    /// (4 bits, wraps), timed by the system as they happen.
    /// `keys` are the modifier keys held now: MOD_SHIFT, MOD_CTRL, MOD_ALT
    /// and MOD_CAPS (Caps Lock on).
    #[derive(Clone, Copy, Default, PartialEq, Eq)]
    pub struct Pointer {
        pub x: usize,
        pub y: usize,
        pub buttons: u8,
        pub keys: u8,
        pub presses: u32,
        pub doubles: u8,
    }

    pub const MOD_SHIFT: u8 = 1;
    pub const MOD_CTRL: u8 = 2;
    pub const MOD_ALT: u8 = 4;
    pub const MOD_CAPS: u8 = 8;

    impl Screen {
        /// Where the mouse is now.
        pub fn pointer(&self) -> Result<Pointer, i64> {
            check(unsafe { syscall(sys::POINTER, 0, 0, 0, 0) }).map(|v| Pointer {
                x: (v & 0xFFFF) as usize,
                y: ((v >> 16) & 0xFFFF) as usize,
                buttons: ((v >> 32) & 0xF) as u8,
                keys: ((v >> 36) & 0xF) as u8,
                presses: (v >> 40) as u32 & 0xF_FFFF,
                doubles: (v >> 60) as u8 & 0xF,
            })
        }

        /// Keys typed since the last call (ASCII; Enter is 10, Esc 27,
        /// Backspace 8), into `out`. Returns how many.
        pub fn keys(&self, out: &mut [u8]) -> Result<usize, i64> {
            check(unsafe { syscall(sys::KEYS_READ, out.as_mut_ptr() as u64, out.len() as u64, 0, 0) }).map(|n| n as usize)
        }

        /// Like keys(), with the modifier keys (MOD_SHIFT, MOD_CTRL, MOD_ALT,
        /// MOD_CAPS) held as each was typed: (character, modifiers).
        pub fn keys_with_modifiers(&self, out: &mut [(u8, u8)]) -> Result<usize, i64> {
            let mut raw = [0u8; 128];
            let max = out.len().min(raw.len() / 2);
            let n = check(unsafe { syscall(sys::KEYS_READ, raw.as_mut_ptr() as u64, max as u64, 1, 0) })? as usize;
            for (i, slot) in out.iter_mut().take(n).enumerate() {
                *slot = (raw[2 * i], raw[2 * i + 1]);
            }
            Ok(n)
        }
    }

    impl Drop for Screen {
        fn drop(&mut self) {
            unsafe { syscall(sys::DISPLAY_RELEASE, 0, 0, 0, 0) };
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

/// UDP and TCP sockets over the network card the kernel brought up.
/// Errors: `E_NOTFOUND` (no network or no DHCP address yet), `E_CLOSED`
/// (refused or reset), `E_TIMEDOUT`.
pub mod smb;

pub mod net {
    use super::*;

    /// An IPv4 address and port.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub struct Endpoint {
        pub addr: [u8; 4],
        pub port: u16,
    }

    impl Endpoint {
        pub const fn new(addr: [u8; 4], port: u16) -> Self {
            Self { addr, port }
        }
    }

    /// Prints an IPv4 address as a.b.c.d.
    pub struct Ip(pub [u8; 4]);

    impl core::fmt::Display for Ip {
        fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
            let [a, b, c, d] = self.0;
            write!(f, "{}.{}.{}.{}", a, b, c, d)
        }
    }

    impl core::fmt::Display for Endpoint {
        fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
            let [a, b, c, d] = self.addr;
            write!(f, "{}.{}.{}.{}:{}", a, b, c, d, self.port)
        }
    }

    pub struct Socket(Handle);

    /// The network as DHCP set it up.
    #[derive(Clone, Copy, Debug)]
    pub struct Info {
        pub addr: [u8; 4],
        pub prefix_len: u8,
        /// All zero when there is none.
        pub gateway: [u8; 4],
        pub dns: [u8; 4],
    }

    /// `E_NOTFOUND` until DHCP has given an address.
    pub fn info() -> Result<Info, i64> {
        let mut w = [0u32; 4];
        check(unsafe { syscall(sys::NET_INFO, w.as_mut_ptr() as u64, 0, 0, 0) })?;
        Ok(Info { addr: w[0].to_be_bytes(), prefix_len: w[1] as u8, gateway: w[2].to_be_bytes(), dns: w[3].to_be_bytes() })
    }

    /// Looks up `name`'s IPv4 address with the DNS server DHCP gave,
    /// waiting up to `timeout_us` in all. `E_NOTFOUND` if the name does
    /// not exist (or there is no network), `E_TIMEDOUT` if no answer came.
    pub fn resolve(name: &str, timeout_us: u64) -> Result<[u8; 4], i64> {
        let dns = info()?.dns;
        if dns == [0; 4] {
            return Err(E_NOTFOUND);
        }
        resolve_with(Endpoint::new(dns, 53), name, timeout_us)
    }

    /// `resolve` through the DNS server at `server`. A dotted IPv4 address
    /// is returned as is.
    pub fn resolve_with(server: Endpoint, name: &str, timeout_us: u64) -> Result<[u8; 4], i64> {
        if let Some(a) = parse_ipv4(name) {
            return Ok(a);
        }
        let mut query = [0u8; 512];
        let id = (clock_us() as u16) ^ 0xAE40;
        let len = dns::query(&mut query, id, name).ok_or(E_INVAL)?;
        let sock = Socket::udp()?;
        sock.connect(server, 0)?;
        let deadline = clock_us().saturating_add(timeout_us);
        let mut reply = [0u8; 512];
        // Ask again every second: DNS runs over UDP and a packet can be lost.
        loop {
            sock.send(&query[..len])?;
            let resend = clock_us().saturating_add(1_000_000).min(deadline);
            loop {
                let now = clock_us();
                if now >= resend {
                    break;
                }
                match sock.recv(&mut reply, resend - now) {
                    Ok(n) => match dns::answer(&reply[..n], id) {
                        Some(r) => return r,
                        None => continue, // not ours, or garbled
                    },
                    Err(E_TIMEDOUT) => break,
                    Err(e) => return Err(e),
                }
            }
            if clock_us() >= deadline {
                return Err(E_TIMEDOUT);
            }
        }
    }

    pub fn parse_ipv4(s: &str) -> Option<[u8; 4]> {
        let mut out = [0u8; 4];
        let mut parts = s.split('.');
        for b in out.iter_mut() {
            let p = parts.next()?;
            if p.is_empty() || p.len() > 3 || !p.bytes().all(|c| c.is_ascii_digit()) {
                return None;
            }
            *b = p.parse().ok()?;
        }
        parts.next().is_none().then_some(out)
    }

    /// The DNS wire format, just enough for A lookups (RFC 1035).
    pub mod dns {
        use super::super::{E_NOTFOUND};

        const TYPE_A: u16 = 1;
        const CLASS_IN: u16 = 1;

        /// Writes a recursive query for `name`'s A record; returns its length.
        pub fn query(buf: &mut [u8; 512], id: u16, name: &str) -> Option<usize> {
            let name = name.strip_suffix('.').unwrap_or(name);
            if name.is_empty() || name.len() > 253 {
                return None;
            }
            buf[..12].copy_from_slice(&[(id >> 8) as u8, id as u8, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0]);
            let mut n = 12;
            for label in name.split('.') {
                if label.is_empty() || label.len() > 63 {
                    return None;
                }
                buf[n] = label.len() as u8;
                buf[n + 1..n + 1 + label.len()].copy_from_slice(label.as_bytes());
                n += 1 + label.len();
            }
            buf[n] = 0;
            buf[n + 1..n + 5].copy_from_slice(&[0, TYPE_A as u8, 0, CLASS_IN as u8]);
            Some(n + 5)
        }

        /// Skips a (possibly compressed) name at `i`; returns the index after it.
        fn skip_name(m: &[u8], mut i: usize) -> Option<usize> {
            loop {
                let len = *m.get(i)? as usize;
                match len {
                    0 => return Some(i + 1),
                    l if l & 0xC0 == 0xC0 => return Some(i + 2),
                    l => i += 1 + l,
                }
            }
        }

        fn u16_at(m: &[u8], i: usize) -> Option<u16> {
            Some(u16::from_be_bytes([*m.get(i)?, *m.get(i + 1)?]))
        }

        /// The first A record in a reply to query `id`: None if this is not
        /// a reply to it, `E_NOTFOUND` if the name does not exist or has no
        /// IPv4 address.
        pub fn answer(m: &[u8], id: u16) -> Option<Result<[u8; 4], i64>> {
            if m.len() < 12 || u16_at(m, 0)? != id || m[2] & 0x80 == 0 {
                return None;
            }
            let rcode = m[3] & 0x0F;
            if rcode != 0 {
                return Some(Err(E_NOTFOUND));
            }
            let (qd, an) = (u16_at(m, 4)?, u16_at(m, 6)?);
            let mut i = 12;
            for _ in 0..qd {
                i = skip_name(m, i)? + 4;
            }
            for _ in 0..an {
                i = skip_name(m, i)?;
                let (ty, class, rdlen) = (u16_at(m, i)?, u16_at(m, i + 2)?, u16_at(m, i + 8)? as usize);
                let data = m.get(i + 10..i + 10 + rdlen)?;
                if ty == TYPE_A && class == CLASS_IN && rdlen == 4 {
                    return Some(Ok([data[0], data[1], data[2], data[3]]));
                }
                i += 10 + rdlen; // CNAMEs and the like: the A record follows
            }
            Some(Err(E_NOTFOUND))
        }
    }

    fn open(kind: u64) -> Result<Socket, i64> {
        check(unsafe { syscall(sys::SOCKET_OPEN, kind, 0, 0, 0) }).map(|h| Socket(Handle(h)))
    }

    impl Socket {
        /// A UDP socket on a fresh local port. `connect` it before sending.
        pub fn udp() -> Result<Socket, i64> {
            open(1)
        }

        /// A TCP socket; `connect` it before sending or receiving.
        pub fn tcp() -> Result<Socket, i64> {
            open(2)
        }

        /// UDP: sets the peer (sends go there, only its datagrams are
        /// received). TCP: connects, waiting up to `timeout_us` (0 = no
        /// limit).
        pub fn connect(&self, to: Endpoint, timeout_us: u64) -> Result<(), i64> {
            let addr = u32::from_be_bytes(to.addr) as u64;
            check(unsafe { syscall(sys::SOCKET_CONNECT, self.0 .0, addr, to.port as u64, timeout_us) }).map(|_| ())
        }

        /// Sends one UDP datagram, or queues TCP data; returns how much was
        /// taken (TCP may take less than all of it).
        pub fn send(&self, data: &[u8]) -> Result<usize, i64> {
            check(unsafe { syscall(sys::SOCKET_SEND, self.0 .0, data.as_ptr() as u64, data.len() as u64, 0) })
                .map(|n| n as usize)
        }

        /// The handle, for `wait_any`.
        pub fn handle(&self) -> &Handle {
            &self.0
        }

        /// TCP: waits for connections on `port` (take them with `accept`).
        pub fn listen(&self, port: u16) -> Result<(), i64> {
            check(unsafe { syscall(sys::SOCKET_LISTEN, self.0 .0, port as u64, 0, 0) }).map(|_| ())
        }

        /// TCP after `listen`: waits up to `timeout_us` (0 = no limit) for a
        /// connection.
        pub fn accept(&self, timeout_us: u64) -> Result<Socket, i64> {
            check(unsafe { syscall(sys::SOCKET_ACCEPT, self.0 .0, timeout_us, 0, 0) }).map(|h| Socket(Handle(h)))
        }

        /// TCP: sends everything.
        pub fn send_all(&self, mut data: &[u8]) -> Result<(), i64> {
            while !data.is_empty() {
                let n = self.send(data)?;
                data = &data[n..];
            }
            Ok(())
        }

        /// Waits up to `timeout_us` (0 = no limit) for data. UDP: one
        /// datagram. TCP: what has arrived; 0 means the peer closed.
        pub fn recv(&self, buf: &mut [u8], timeout_us: u64) -> Result<usize, i64> {
            check(unsafe {
                syscall(sys::SOCKET_RECV, self.0 .0, buf.as_mut_ptr() as u64, buf.len() as u64, timeout_us)
            })
            .map(|n| n as usize)
        }
    }
}

/// Waits until one of `handles` is ready, up to `timeout_us` (0 = no
/// limit), and returns its index. Ready means: a port has a message, a
/// socket has data (or a connection to accept, or was closed), an event is
/// set (an auto-reset event is reset by this), a process has exited.
pub fn wait_any(handles: &[&Handle], timeout_us: u64) -> Result<usize, i64> {
    let raw: alloc::vec::Vec<u64> = handles.iter().map(|h| h.0).collect();
    check(unsafe { syscall(sys::WAIT_ANY, raw.as_ptr() as u64, raw.len() as u64, timeout_us, 0) }).map(|i| i as usize)
}

/// An event object: `set` wakes a `wait_any` on it. Auto-reset events let one
/// waiter through per `set`; manual-reset ones stay set until `reset`.
pub struct Event(pub Handle);

impl Event {
    pub fn auto_reset() -> Result<Event, i64> {
        check(unsafe { syscall(sys::EVENT_CREATE, 0, 0, 0, 0) }).map(|h| Event(Handle(h)))
    }

    pub fn manual_reset() -> Result<Event, i64> {
        check(unsafe { syscall(sys::EVENT_CREATE, 1, 0, 0, 0) }).map(|h| Event(Handle(h)))
    }

    pub fn set(&self) -> Result<(), i64> {
        check(unsafe { syscall(sys::EVENT_SET, self.0 .0, 0, 0, 0) }).map(|_| ())
    }

    pub fn reset(&self) -> Result<(), i64> {
        check(unsafe { syscall(sys::EVENT_RESET, self.0 .0, 0, 0, 0) }).map(|_| ())
    }
}

/// A handle to a child process (one this program spawned): `wait_any`
/// sees it once the child has exited.
pub struct Child(pub Handle);

impl Child {
    pub fn open(pid: u64) -> Result<Child, i64> {
        check(unsafe { syscall(sys::PROCESS_HANDLE, pid, 0, 0, 0) }).map(|h| Child(Handle(h)))
    }

    /// Ends the child with exit code `code`.
    pub fn kill(&self, code: i64) -> Result<(), i64> {
        check(unsafe { syscall(sys::PROCESS_KILL, self.0 .0, code as u64, 0, 0) }).map(|_| ())
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

// Programs are built for the x86_64 Linux target to get hardware floating
// point (SSE) on stable Rust, so nothing supplies the C memory functions
// compiler-generated code calls. These are the usual `rep` string versions.
core::arch::global_asm!(
    r#"
.section .text.aero_mem,"ax",@progbits
.global memcpy
memcpy:
    mov rax, rdi
    mov rcx, rdx
    rep movsb
    ret
.global memmove
memmove:
    mov rax, rdi
    mov rcx, rdx
    cmp rdi, rsi
    jbe 1f
    lea rsi, [rsi + rdx - 1]
    lea rdi, [rdi + rdx - 1]
    std
    rep movsb
    cld
    ret
1:  rep movsb
    ret
.global memset
memset:
    mov r8, rdi
    mov eax, esi
    mov rcx, rdx
    rep stosb
    mov rax, r8
    ret
.global memcmp
.global bcmp
memcmp:
bcmp:
    xor eax, eax
    test rdx, rdx
    jz 2f
    mov rcx, rdx
    repe cmpsb
    je 2f
    movzx eax, byte ptr [rdi - 1]
    movzx ecx, byte ptr [rsi - 1]
    sub eax, ecx
2:  ret
"#
);

/// The prebuilt `core` for the Linux target refers to this even though
/// programs abort on panic and never unwind.
#[no_mangle]
pub extern "C" fn rust_eh_personality() {}

/// Also referenced (by the prebuilt `alloc`), never reached: nothing unwinds.
#[no_mangle]
pub extern "C" fn _Unwind_Resume() -> ! {
    exit(-1)
}
