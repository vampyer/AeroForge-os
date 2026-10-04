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
use crate::{apic, console, memory, percpu, sched};

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

const E_BADHANDLE: i64 = -1;
const E_FAULT: i64 = -2;
const E_NOTFOUND: i64 = -3;
const E_RIGHTS: i64 = -4;
const E_FULL: i64 = -5;
const E_INVAL: i64 = -6;
const E_EXISTS: i64 = -7;

const NO_HANDLE: u64 = u64::MAX;
const USER_TOP: u64 = 0x0000_8000_0000_0000;

/// Written to user memory by SYS_PORT_RECV.
#[repr(C)]
struct RecvInfo {
    len: u64,
    sender_pid: u64,
    handle: u64,
}

/// Checks that [ptr, ptr+len) is mapped user memory in the current address space.
fn user_range(ptr: u64, len: u64) -> Result<(), i64> {
    let end = ptr.checked_add(len).ok_or(E_FAULT)?;
    if end > USER_TOP {
        return Err(E_FAULT);
    }
    let mut page = ptr & !(memory::PAGE_SIZE - 1);
    while page < end {
        memory::translate(page).ok_or(E_FAULT)?;
        page += memory::PAGE_SIZE;
    }
    Ok(())
}

fn user_bytes<'a>(ptr: u64, len: u64) -> Result<&'a [u8], i64> {
    user_range(ptr, len)?;
    Ok(unsafe { core::slice::from_raw_parts(ptr as *const u8, len as usize) })
}

fn user_str<'a>(ptr: u64, len: u64) -> Result<&'a str, i64> {
    if len > 64 {
        return Err(E_INVAL);
    }
    core::str::from_utf8(user_bytes(ptr, len)?).map_err(|_| E_INVAL)
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
            let s = core::str::from_utf8(bytes).unwrap_or("<invalid utf-8>");
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
            process::spawn(name, proc_.pid).map_err(|e| {
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
            if names.contains_key(name) {
                return Err(E_EXISTS);
            }
            names.insert(String::from(name), port);
            Ok(0)
        }
        SYS_PORT_LOOKUP => {
            let name = user_str(a0, a1)?;
            let port = NAMES.lock().get(name).cloned().ok_or(E_NOTFOUND)?;
            // Looking a service up only ever grants the right to send to it.
            Ok(proc_.handles.lock().insert(Handle { object: Object::Port(port), rights: rights::SEND }))
        }
        SYS_PORT_SEND => {
            let port = port_handle(&proc_, a0, rights::SEND)?;
            if a2 as usize > MAX_MESSAGE {
                return Err(E_INVAL);
            }
            let data: Vec<u8> = user_bytes(a1, a2)?.to_vec();
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
            user_range(a1, a2)?;
            user_range(a3, core::mem::size_of::<RecvInfo>() as u64)?;
            let msg = port.recv();
            let n = msg.data.len().min(a2 as usize);
            unsafe { core::ptr::copy_nonoverlapping(msg.data.as_ptr(), a1 as *mut u8, n) };
            let handle = match msg.handle {
                Some(h) => proc_.handles.lock().insert(h),
                None => NO_HANDLE,
            };
            unsafe { (a3 as *mut RecvInfo).write(RecvInfo { len: n as u64, sender_pid: msg.sender, handle }) };
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
        SYS_HANDLE_CLOSE => proc_.handles.lock().take(a0).map(|_| 0).ok_or(E_BADHANDLE),
        _ => Err(E_INVAL),
    }
}
