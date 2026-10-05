//! The system call gate: `int 0x80` from ring 3.
//!
//! Convention: rax = number, arguments in rdi, rsi, rdx, r10, r8; result in
//! rax. Negative results are errors. Keep in sync with userland/src/lib.rs.

use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use crate::interrupts::InterruptFrame;
use crate::ipc::{Message, Port, MAX_MESSAGE, NAMES};
use crate::process::{self, rights, Handle, Object};
use crate::{apic, console, gamepad, percpu, sched, security, sound, vfs};

pub const SYS_EXIT: u64 = 0;
pub const SYS_WRITE: u64 = 1;
pub const SYS_YIELD: u64 = 2;
pub const SYS_GETPID: u64 = 3;
pub const SYS_SLEEP_MS: u64 = 4;
pub const SYS_CPU_ID: u64 = 5;
pub const SYS_UPTIME_MS: u64 = 6;
pub const SYS_SPAWN: u64 = 7;
pub const SYS_PORT_CREATE: u64 = 8;
pub const SYS_PORT_PUBLISH: u64 = 9;
pub const SYS_PORT_LOOKUP: u64 = 10;
pub const SYS_PORT_SEND: u64 = 11;
pub const SYS_PORT_RECV: u64 = 12;
pub const SYS_HANDLE_CLOSE: u64 = 13;
pub const SYS_HANDLE_DUP: u64 = 14;
pub const SYS_FILE_READ: u64 = 15;
pub const SYS_AUDIO_WRITE: u64 = 16;
pub const SYS_AUDIO_QUEUED: u64 = 17;
pub const SYS_GAMEPAD_READ: u64 = 18;
pub const SYS_FILE_WRITE: u64 = 19;
pub const SYS_FILE_DELETE: u64 = 20;
pub const SYS_DIR_CREATE: u64 = 21;

/// Largest file one SYS_FILE_WRITE can save.
const MAX_FILE_WRITE: u64 = 8 * 1024 * 1024;

const E_BADHANDLE: i64 = -1;
const E_FAULT: i64 = -2;
const E_NOTFOUND: i64 = -3;
const E_RIGHTS: i64 = -4;
const E_FULL: i64 = -5;
const E_INVAL: i64 = -6;
const E_EXISTS: i64 = -7;

const NO_HANDLE: u64 = u64::MAX;

/// Written to user memory by SYS_PORT_RECV.
#[repr(C)]
struct RecvInfo {
    len: u64,
    sender_pid: u64,
    handle: u64,
}

// All user memory goes through security::copy_from_user / copy_to_user:
// they check every page is mapped, user-accessible (and writable for
// writes) and lift SMAP only for the copy itself.

fn user_bytes(ptr: u64, len: u64) -> Result<Vec<u8>, i64> {
    security::copy_from_user(ptr, len).ok_or(E_FAULT)
}

fn user_str(ptr: u64, len: u64) -> Result<String, i64> {
    if len > 255 {
        return Err(E_INVAL);
    }
    String::from_utf8(user_bytes(ptr, len)?).map_err(|_| E_INVAL)
}

fn check_writable(ptr: u64, len: u64) -> Result<(), i64> {
    security::check_user(ptr, len, true).then_some(()).ok_or(E_FAULT)
}

fn to_user(ptr: u64, data: &[u8]) -> Result<(), i64> {
    security::copy_to_user(ptr, data).then_some(()).ok_or(E_FAULT)
}

/// Programs may not change /system (the boot configuration) until files
/// have owners and permissions; the kernel shell still can.
fn writable_by_programs(path: &str) -> Result<(), i64> {
    let first = path.split('/').find(|p| !p.is_empty()).unwrap_or("");
    if first.eq_ignore_ascii_case("system") { Err(E_RIGHTS) } else { Ok(()) }
}

/// A file system error as a system call error code.
fn fs_error(e: &'static str) -> i64 {
    match e {
        "file not found" | "not a directory" | "no filesystem mounted" | "no filesystem mounted at /" => E_NOTFOUND,
        "disk full" => E_FULL,
        "already exists" => E_EXISTS,
        _ => E_INVAL,
    }
}

fn port_handle(proc_: &process::Process, h: u64, need: u32) -> Result<alloc::sync::Arc<Port>, i64> {
    let table = proc_.handles.lock();
    let handle = table.get(h).ok_or(E_BADHANDLE)?;
    if handle.rights & need != need {
        return Err(E_RIGHTS);
    }
    let Object::Port(p) = &handle.object;
    Ok(p.clone())
}

pub fn dispatch(frame: &mut InterruptFrame) {
    let r = handle(frame.rax, frame.rdi, frame.rsi, frame.rdx, frame.r10);
    frame.rax = match r {
        Ok(v) => v,
        Err(e) => e as u64,
    };
}

fn handle(num: u64, a0: u64, a1: u64, a2: u64, a3: u64) -> Result<u64, i64> {
    let thread = sched::current();
    let proc_ = thread.process.clone().ok_or(E_INVAL)?;
    drop(thread);

    match num {
        SYS_EXIT => {
            proc_.exit_code.store(a0 as i64, Ordering::SeqCst);
            drop(proc_);
            sched::exit_current();
        }
        SYS_WRITE => {
            let bytes = user_bytes(a0, a1.min(4096))?;
            let s = core::str::from_utf8(&bytes).unwrap_or("<invalid utf-8>");
            crate::kprint!("{}", s);
            Ok(bytes.len() as u64)
        }
        SYS_YIELD => {
            sched::schedule();
            Ok(0)
        }
        SYS_GETPID => Ok(proc_.pid),
        SYS_SLEEP_MS => {
            drop(proc_);
            sched::sleep_ticks(a0 * apic::TIMER_HZ / 1000);
            Ok(0)
        }
        SYS_CPU_ID => Ok(percpu::this().index as u64),
        SYS_UPTIME_MS => Ok(sched::ticks() * 1000 / apic::TIMER_HZ),
        SYS_SPAWN => {
            let name = user_str(a0, a1)?;
            process::spawn(&name, proc_.pid).map_err(|e| {
                console::print_colored(console::YELLOW, format_args!("[kernel] spawn {}: {}\n", name, e));
                E_NOTFOUND
            })
        }
        SYS_PORT_CREATE => {
            let port = Port::new();
            Ok(proc_.handles.lock().insert(Handle { object: Object::Port(port), rights: rights::ALL }))
        }
        SYS_PORT_PUBLISH => {
            let port = port_handle(&proc_, a0, rights::RECV)?;
            let name = user_str(a1, a2)?;
            let mut names = NAMES.lock();
            if names.contains_key(&name) {
                return Err(E_EXISTS);
            }
            names.insert(name, port);
            Ok(0)
        }
        SYS_PORT_LOOKUP => {
            let name = user_str(a0, a1)?;
            let port = NAMES.lock().get(&name).cloned().ok_or(E_NOTFOUND)?;
            // Looking a service up only ever grants the right to send to it.
            Ok(proc_.handles.lock().insert(Handle { object: Object::Port(port), rights: rights::SEND }))
        }
        SYS_PORT_SEND => {
            let port = port_handle(&proc_, a0, rights::SEND)?;
            if a2 as usize > MAX_MESSAGE {
                return Err(E_INVAL);
            }
            let data: Vec<u8> = user_bytes(a1, a2)?;
            let moved = if a3 == NO_HANDLE {
                None
            } else {
                Some(proc_.handles.lock().take(a3).ok_or(E_BADHANDLE)?)
            };
            let msg = Message { data, sender: proc_.pid, handle: moved };
            match port.send(msg) {
                Ok(()) => Ok(0),
                Err(msg) => {
                    if let Some(h) = msg.handle {
                        proc_.handles.lock().insert(h); // give it back
                    }
                    Err(E_FULL)
                }
            }
        }
        SYS_PORT_RECV => {
            let port = port_handle(&proc_, a0, rights::RECV)?;
            // Check both buffers before blocking, so a bad pointer can't lose a message.
            check_writable(a1, a2)?;
            check_writable(a3, core::mem::size_of::<RecvInfo>() as u64)?;
            let msg = port.recv();
            let n = msg.data.len().min(a2 as usize);
            to_user(a1, &msg.data[..n])?;
            let handle = match msg.handle {
                Some(h) => proc_.handles.lock().insert(h),
                None => NO_HANDLE,
            };
            let info = RecvInfo { len: n as u64, sender_pid: msg.sender, handle };
            let raw = unsafe {
                core::slice::from_raw_parts(&info as *const RecvInfo as *const u8, core::mem::size_of::<RecvInfo>())
            };
            to_user(a3, raw)?;
            Ok(n as u64)
        }
        SYS_HANDLE_DUP => {
            // A copy with the same or fewer rights, never more.
            let mut table = proc_.handles.lock();
            let h = table.get(a0).ok_or(E_BADHANDLE)?;
            let rights = a1 as u32;
            if h.rights & rights != rights {
                return Err(E_RIGHTS);
            }
            let dup = Handle { object: h.object.clone(), rights };
            Ok(table.insert(dup))
        }
        SYS_FILE_READ => {
            // Whole-file read into a user buffer; returns the bytes copied.
            let path = user_str(a0, a1)?;
            check_writable(a2, a3)?;
            let data = vfs::read(&path, a3 as usize).map_err(|_| E_NOTFOUND)?;
            to_user(a2, &data)?;
            Ok(data.len() as u64)
        }
        SYS_FILE_WRITE => {
            // Creates or replaces a whole file from a user buffer; returns the bytes written.
            let path = user_str(a0, a1)?;
            writable_by_programs(&path)?;
            if a3 > MAX_FILE_WRITE {
                return Err(E_INVAL);
            }
            let data = user_bytes(a2, a3)?;
            vfs::write(&path, &data).map_err(fs_error)?;
            Ok(a3)
        }
        SYS_FILE_DELETE => {
            let path = user_str(a0, a1)?;
            writable_by_programs(&path)?;
            vfs::remove(&path).map_err(fs_error).map(|_| 0)
        }
        SYS_DIR_CREATE => {
            let path = user_str(a0, a1)?;
            writable_by_programs(&path)?;
            vfs::create_dir(&path).map_err(fs_error).map(|_| 0)
        }
        SYS_HANDLE_CLOSE => proc_.handles.lock().take(a0).map(|_| 0).ok_or(E_BADHANDLE),
        SYS_AUDIO_WRITE => {
            // Interleaved 48 kHz 16-bit stereo frames; returns how many were
            // queued (the rest did not fit: try again a little later).
            let frames = a1.min(sound::RATE as u64 / 10);
            let bytes = user_bytes(a0, frames * 4)?;
            let samples: Vec<i16> = bytes.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
            sound::write(proc_.pid, &samples).map(|n| n as u64).map_err(|_| E_NOTFOUND)
        }
        SYS_AUDIO_QUEUED => {
            if sound::selected().is_none() {
                return Err(E_NOTFOUND);
            }
            Ok(sound::queued(proc_.pid) as u64)
        }
        SYS_GAMEPAD_READ => {
            // Gamepad a0's state into a gamepad::State at a1; returns how many gamepads there are.
            let (state, count) = gamepad::read(a0 as usize).ok_or(E_NOTFOUND)?;
            to_user(a1, state.as_bytes())?;
            Ok(count as u64)
        }
        _ => Err(E_INVAL),
    }
}
