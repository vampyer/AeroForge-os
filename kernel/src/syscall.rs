//! System calls from ring 3: the `syscall` instruction (the fast path
//! programs use), or the older `int 0x80` gate, which still works.
//!
//! Convention: rax = number, arguments in rdi, rsi, rdx, r10, r8; result in
//! rax. Negative results are errors. `syscall` also overwrites rcx and r11
//! (the CPU keeps the return address and flags there). Keep in sync with
//! userland/src/lib.rs.

use alloc::string::String;
use alloc::vec::Vec;

use crate::interrupts::InterruptFrame;
use crate::ipc::{Message, Port, MAX_MESSAGE, NAMES};
use crate::process::{self, rights, Handle, Object};
use crate::{apic, arch, console, display, futex, gamepad, gdt, input, net, percpu, sched, security, sound, vfs, wait};

const MSR_STAR: u32 = 0xC000_0081;
const MSR_LSTAR: u32 = 0xC000_0082;
const MSR_FMASK: u32 = 0xC000_0084;
const EFER_SCE: u64 = 1 << 0;
/// Flags `syscall` clears on entry: TF, IF, DF, NT and AC. IF off matches
/// the `int 0x80` interrupt gate; AC off keeps SMAP in force.
const FMASK: u64 = (1 << 8) | (1 << 9) | (1 << 10) | (1 << 14) | (1 << 18);

// `syscall` entry. The CPU has put the user's rip in rcx and rflags in r11
// and switched to kernel CS/SS, but not to a kernel stack, so this swaps in
// the per-CPU data (gs:[8] kernel stack top, gs:[16] scratch), builds the
// same frame `int 0x80` would on this thread's kernel stack and runs the
// same dispatcher. A thread that blocks inside a call sleeps on this frame
// like any other. The way out is `sysret`, which needs no stack.
core::arch::global_asm!(
    r#"
.section .text
.global syscall_entry
syscall_entry:
    swapgs
    mov gs:[16], rsp
    mov rsp, gs:[8]
    push {user_ss}
    push qword ptr gs:[16]
    push r11
    push {user_cs}
    push rcx
    push 0
    push 0x80
    push rax
    push rbx
    push rcx
    push rdx
    push rsi
    push rdi
    push rbp
    push r8
    push r9
    push r10
    push r11
    push r12
    push r13
    push r14
    push r15
    mov rdi, rsp
    call syscall_from_user
    pop r15
    pop r14
    pop r13
    pop r12
    pop r11
    pop r10
    pop r9
    pop r8
    pop rbp
    pop rdi
    pop rsi
    pop rdx
    pop rcx
    pop rbx
    pop rax
    add rsp, 16
    // rip, cs, rflags, rsp, ss left on the stack.
    mov rcx, [rsp]
    mov r11, [rsp + 16]
    mov rsp, [rsp + 24]
    swapgs
    sysretq
"#,
    user_ss = const gdt::USER_DATA as u64,
    user_cs = const gdt::USER_CODE as u64,
);

extern "C" {
    fn syscall_entry();
}

#[no_mangle]
extern "C" fn syscall_from_user(frame: &mut InterruptFrame) {
    security::on_kernel_entry();
    dispatch(frame);
    // `sysret` would fault in ring 0 on a non-canonical return address (one
    // past the top of user space), on the user's stack. Programs cannot map
    // that page, but if one ever gets there it dies instead of the kernel.
    if frame.rip >= 0x0000_8000_0000_0000 {
        crate::kprintln!("[kernel] pid {}: system call returned to a bad address {:#x}, killed",
            sched::current().pid(), frame.rip);
        sched::exit_current();
    }
}

/// Opens the `syscall` instruction to ring 3 on the calling CPU.
pub fn init_cpu() {
    unsafe {
        // Kernel CS = 0x08 (SS 0x10). sysret: SS = base + 8 = user data,
        // CS = base + 16 = user code, the GDT's order. The base carries RPL 3
        // (0x13): Intel CPUs (and QEMU) force RPL 3 on both selectors, but
        // AMD CPUs load them as given. With RPL 0 there, the first interrupt
        // in a program after a system call pushed SS 0x18 and its `iretq`
        // raised #GP(0x18) on real Ryzen hardware.
        const SYSRET_BASE: u64 = gdt::USER_DATA as u64 - 8; // 0x13
        const _: () = assert!(gdt::USER_CODE == gdt::USER_DATA + 8);
        arch::wrmsr(MSR_STAR, (SYSRET_BASE << 48) | ((gdt::KERNEL_CODE as u64) << 32));
        arch::wrmsr(MSR_LSTAR, syscall_entry as *const () as u64);
        arch::wrmsr(MSR_FMASK, FMASK);
        arch::wrmsr(arch::MSR_EFER, arch::rdmsr(arch::MSR_EFER) | EFER_SCE);
    }
}

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
pub const SYS_MEM_MAP: u64 = 22;
pub const SYS_MEM_UNMAP: u64 = 23;
pub const SYS_THREAD_CREATE: u64 = 24;
pub const SYS_THREAD_EXIT: u64 = 25;
pub const SYS_THREAD_JOIN: u64 = 26;
pub const SYS_FUTEX_WAIT: u64 = 27;
pub const SYS_FUTEX_WAKE: u64 = 28;
pub const SYS_PROCESS_WAIT: u64 = 29;
pub const SYS_THREAD_ID: u64 = 30;
pub const SYS_THREAD_PRIORITY: u64 = 31;
pub const SYS_SLEEP_US: u64 = 32;
pub const SYS_CLOCK_US: u64 = 33;
pub const SYS_SOCKET_OPEN: u64 = 34;
pub const SYS_SOCKET_CONNECT: u64 = 35;
pub const SYS_SOCKET_SEND: u64 = 36;
pub const SYS_SOCKET_RECV: u64 = 37;
pub const SYS_SOCKET_LISTEN: u64 = 38;
pub const SYS_SOCKET_ACCEPT: u64 = 39;
pub const SYS_NET_INFO: u64 = 40;
pub const SYS_EVENT_CREATE: u64 = 41;
pub const SYS_EVENT_SET: u64 = 42;
pub const SYS_EVENT_RESET: u64 = 43;
pub const SYS_WAIT_ANY: u64 = 44;
pub const SYS_PROCESS_HANDLE: u64 = 45;
pub const SYS_PROCESS_KILL: u64 = 46;
pub const SYS_DISPLAY_ACQUIRE: u64 = 47;
pub const SYS_DISPLAY_PRESENT: u64 = 48;
pub const SYS_DISPLAY_RELEASE: u64 = 49;
pub const SYS_POINTER: u64 = 50;
pub const SYS_KEYS_READ: u64 = 51;
pub const SYS_TIME: u64 = 52;
pub const SYS_DIR_LIST: u64 = 53;
pub const SYS_MOUSE_SPEED: u64 = 54;
pub const SYS_VOLUMES: u64 = 55;
pub const SYS_DELETED_LIST: u64 = 56;
pub const SYS_UNDELETE: u64 = 57;

/// User addresses end here (the lower half of the address space).
const USER_END: u64 = 0x0000_8000_0000_0000;

/// Largest file one SYS_FILE_WRITE can save.
const MAX_FILE_WRITE: u64 = 8 * 1024 * 1024;

/// Largest buffer one SYS_DIR_LIST fills.
const MAX_DIR_LIST: u64 = 256 * 1024;

fn display_error(e: display::Error) -> i64 {
    match e {
        display::Error::NoDisplay => E_NOTFOUND,
        display::Error::Busy | display::Error::NotOwner => E_RIGHTS,
        display::Error::Fault => E_FAULT,
        display::Error::NoMemory => E_FULL,
    }
}

const E_BADHANDLE: i64 = -1;
const E_FAULT: i64 = -2;
const E_NOTFOUND: i64 = -3;
const E_RIGHTS: i64 = -4;
const E_FULL: i64 = -5;
const E_INVAL: i64 = -6;
const E_EXISTS: i64 = -7;
const E_AGAIN: i64 = -8;
const E_TIMEDOUT: i64 = -9;
/// The connection was refused or reset (or never made).
const E_CLOSED: i64 = -10;

/// Most one SYS_SOCKET_SEND or SYS_SOCKET_RECV moves.
const MAX_SOCKET_IO: u64 = 64 * 1024;

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
        "its data has been written over" | "its data is gone" => E_FULL,
        "NTFS volumes are read-only for now" => E_RIGHTS,
        _ => E_INVAL,
    }
}

fn port_handle(proc_: &process::Process, h: u64, need: u32) -> Result<alloc::sync::Arc<Port>, i64> {
    let table = proc_.handles.lock();
    let handle = table.get(h).ok_or(E_BADHANDLE)?;
    if handle.rights & need != need {
        return Err(E_RIGHTS);
    }
    match &handle.object {
        Object::Port(p) => Ok(p.clone()),
        _ => Err(E_BADHANDLE),
    }
}

fn socket_handle(proc_: &process::Process, h: u64, need: u32) -> Result<alloc::sync::Arc<net::UserSocket>, i64> {
    let table = proc_.handles.lock();
    let handle = table.get(h).ok_or(E_BADHANDLE)?;
    if handle.rights & need != need {
        return Err(E_RIGHTS);
    }
    match &handle.object {
        Object::Socket(s) => Ok(s.clone()),
        _ => Err(E_BADHANDLE),
    }
}

fn event_handle(proc_: &process::Process, h: u64, need: u32) -> Result<alloc::sync::Arc<wait::Event>, i64> {
    let table = proc_.handles.lock();
    let handle = table.get(h).ok_or(E_BADHANDLE)?;
    if handle.rights & need != need {
        return Err(E_RIGHTS);
    }
    match &handle.object {
        Object::Event(e) => Ok(e.clone()),
        _ => Err(E_BADHANDLE),
    }
}

/// Most handles one SYS_WAIT_ANY watches.
const MAX_WAIT: u64 = 64;

/// Is the object ready for a waiter? Takes an auto-reset event's signal.
fn ready(object: &Object) -> bool {
    match object {
        Object::Port(p) => p.queued() > 0,
        Object::Socket(s) => s.readable(),
        Object::Event(e) => e.take(),
        Object::Process(x) => x.exited.load(core::sync::atomic::Ordering::SeqCst),
    }
}

fn sock_error(e: net::SockError) -> i64 {
    match e {
        net::SockError::NoNetwork => E_NOTFOUND,
        net::SockError::Closed => E_CLOSED,
        net::SockError::TimedOut => E_TIMEDOUT,
        net::SockError::Invalid | net::SockError::Killed => E_INVAL,
    }
}

pub fn dispatch(frame: &mut InterruptFrame) {
    let r = handle(frame.rax, frame.rdi, frame.rsi, frame.rdx, frame.r10);
    frame.rax = match r {
        Ok(v) => v,
        Err(e) => e as u64,
    };
    // Its process is exiting (another thread called exit, or faulted): the
    // thread ends here, with nothing of the call still held.
    if sched::killed() {
        sched::exit_current();
    }
}

fn handle(num: u64, a0: u64, a1: u64, a2: u64, a3: u64) -> Result<u64, i64> {
    let thread = sched::current();
    let proc_ = thread.process.clone().ok_or(E_INVAL)?;
    drop(thread);

    match num {
        SYS_EXIT => {
            // The whole process: its other threads stop too.
            process::kill(&proc_, a0 as i64);
            Ok(0)
        }
        SYS_THREAD_EXIT => {
            drop(proc_);
            sched::exit_current();
        }
        SYS_THREAD_ID => Ok(sched::current().tid),
        SYS_THREAD_PRIORITY => {
            // Thread id (0 = the caller), new level: 0 low, 1 normal, 2 high.
            // Only threads of the caller's own process.
            if a1 > sched::PRIO_HIGH as u64 {
                return Err(E_INVAL);
            }
            let t = if a0 == 0 {
                sched::current()
            } else {
                let found = sched::THREADS.lock().get(&a0).cloned();
                match found {
                    Some(t) if t.pid() == proc_.pid => t,
                    _ => return Err(E_NOTFOUND),
                }
            };
            sched::set_priority(&t, a1 as u8);
            Ok(0)
        }
        SYS_THREAD_CREATE => {
            // entry, stack top (16-byte aligned), argument for the entry function.
            if a0 >= USER_END || a1 >= USER_END || a1 % 16 != 0 {
                return Err(E_INVAL);
            }
            if proc_.exiting.load(core::sync::atomic::Ordering::SeqCst) {
                return Err(E_INVAL);
            }
            let name = alloc::format!("{}/thread", proc_.name);
            // Stack as after a call: a return address slot below the top.
            Ok(sched::spawn_user(proc_, &name, a0, a1 - 8, a2).tid)
        }
        SYS_THREAD_JOIN => {
            match sched::THREADS.lock().get(&a0) {
                Some(t) if t.pid() != proc_.pid => return Err(E_INVAL),
                _ => {}
            }
            drop(proc_);
            sched::join(a0);
            Ok(0)
        }
        SYS_MEM_MAP => process::map_memory(&proc_, a0).map_err(|e| if e == "bad length" { E_INVAL } else { E_FULL }),
        SYS_MEM_UNMAP => process::unmap_memory(&proc_, a0).map(|_| 0).map_err(|_| E_INVAL),
        SYS_FUTEX_WAIT => {
            let pid = proc_.pid;
            drop(proc_);
            // a2: timeout in microseconds, 0 for none.
            let deadline = (a2 != 0).then(|| apic::micros().saturating_add(a2));
            futex::wait(pid, a0, a1 as u32, deadline).map(|_| 0).map_err(|e| match e {
                futex::WaitError::Again => E_AGAIN,
                futex::WaitError::Fault => E_FAULT,
                futex::WaitError::TimedOut => E_TIMEDOUT,
            })
        }
        SYS_FUTEX_WAKE => {
            if a0 % 4 != 0 || a0 >= USER_END {
                return Err(E_INVAL);
            }
            Ok(futex::wake(proc_.pid, a0, a1))
        }
        SYS_PROCESS_WAIT => {
            // pid, where to store the exit code (an i64).
            check_writable(a1, 8)?;
            let me = proc_.pid;
            drop(proc_);
            let code = process::wait(me, a0).map_err(|_| E_NOTFOUND)?;
            to_user(a1, &code.to_le_bytes())?;
            Ok(0)
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
            sched::sleep_us(a0.saturating_mul(1000));
            Ok(0)
        }
        SYS_SLEEP_US => {
            drop(proc_);
            sched::sleep_us(a0);
            Ok(0)
        }
        SYS_CLOCK_US => Ok(apic::micros()),
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
            let msg = port.recv().ok_or(E_INVAL)?;
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
        SYS_DIR_LIST => {
            // The entries of a directory into a user buffer, one record each:
            // flags (1 = directory, 2 = a time follows the name), size (8 bytes,
            // little-endian), name length, name, then when it was last written
            // (8 bytes, YYYYMMDDhhmmss as a number) if known. Stops at the last
            // record that fits; returns the bytes used.
            let path = user_str(a0, a1)?;
            let entries = vfs::list(&path).map_err(fs_error)?;
            let cap = a3.min(MAX_DIR_LIST) as usize;
            let mut out = Vec::new();
            for e in entries {
                let name = &e.name.as_bytes()[..e.name.len().min(255)];
                let timed = e.modified != 0;
                if out.len() + 10 + name.len() + if timed { 8 } else { 0 } > cap {
                    break;
                }
                out.push(e.is_dir as u8 | (timed as u8) << 1);
                out.extend_from_slice(&e.size.to_le_bytes());
                out.push(name.len() as u8);
                out.extend_from_slice(name);
                if timed {
                    out.extend_from_slice(&e.modified.to_le_bytes());
                }
            }
            check_writable(a2, out.len() as u64)?;
            to_user(a2, &out)?;
            Ok(out.len() as u64)
        }
        SYS_DELETED_LIST => {
            // Deleted entries still in a directory, one record each: flags
            // (1 = directory, 2 = its data is all still there), size and
            // last-written time (8 bytes each, little-endian), its slot in the
            // directory (4 bytes), name length, name. Returns the bytes used.
            let path = user_str(a0, a1)?;
            let entries = vfs::deleted(&path).map_err(fs_error)?;
            let cap = a3.min(MAX_DIR_LIST) as usize;
            let mut out = Vec::new();
            for e in entries {
                let name = &e.name.as_bytes()[..e.name.len().min(255)];
                if out.len() + 22 + name.len() > cap {
                    break;
                }
                out.push(e.is_dir as u8 | (e.whole as u8) << 1);
                out.extend_from_slice(&e.size.to_le_bytes());
                out.extend_from_slice(&e.modified.to_le_bytes());
                out.extend_from_slice(&e.slot.to_le_bytes());
                out.push(name.len() as u8);
                out.extend_from_slice(name);
            }
            check_writable(a2, out.len() as u64)?;
            to_user(a2, &out)?;
            Ok(out.len() as u64)
        }
        SYS_UNDELETE => {
            // Brings back deleted entry a2 (its slot) of directory a0/a1, and
            // writes its name (up to 255 bytes) to a3; returns the name's length.
            let path = user_str(a0, a1)?;
            writable_by_programs(&path)?;
            let name = vfs::undelete(&path, a2 as u32).map_err(fs_error)?;
            let name = &name.as_bytes()[..name.len().min(255)];
            check_writable(a3, name.len() as u64)?;
            to_user(a3, name)?;
            Ok(name.len() as u64)
        }
        SYS_VOLUMES => {
            // The mounted volumes into a user buffer, root first, one record
            // each: flags (1 = read-only), size and free bytes (8 bytes each,
            // little-endian; free is all ones when unknown), then the mount
            // path, device name, kind and label, each a length byte and text.
            // Stops at the last record that fits; returns the bytes used.
            let cap = a1.min(MAX_DIR_LIST) as usize;
            let mut out = Vec::new();
            for m in vfs::volumes() {
                let (size, free) = m.vol.space();
                let mut rec = Vec::new();
                rec.push(m.vol.read_only() as u8);
                rec.extend_from_slice(&size.to_le_bytes());
                rec.extend_from_slice(&free.unwrap_or(u64::MAX).to_le_bytes());
                for text in [m.path.as_str(), m.vol.dev().name(), m.vol.kind(), m.vol.label()] {
                    let t = &text.as_bytes()[..text.len().min(255)];
                    rec.push(t.len() as u8);
                    rec.extend_from_slice(t);
                }
                if out.len() + rec.len() > cap {
                    break;
                }
                out.extend_from_slice(&rec);
            }
            check_writable(a0, out.len() as u64)?;
            to_user(a0, &out)?;
            Ok(out.len() as u64)
        }
        SYS_DIR_CREATE => {
            let path = user_str(a0, a1)?;
            writable_by_programs(&path)?;
            vfs::create_dir(&path).map_err(fs_error).map(|_| 0)
        }
        SYS_SOCKET_OPEN => {
            // a0: 1 = UDP, 2 = TCP.
            let tcp = match a0 {
                1 => false,
                2 => true,
                _ => return Err(E_INVAL),
            };
            let sock = net::UserSocket::open(tcp).map_err(sock_error)?;
            Ok(proc_.handles.lock().insert(Handle { object: Object::Socket(sock), rights: rights::ALL }))
        }
        SYS_SOCKET_CONNECT => {
            // Handle, IPv4 address (a.b.c.d as a << 24 | ...), port, timeout in µs (0 = none).
            let sock = socket_handle(&proc_, a0, rights::SEND)?;
            drop(proc_);
            let port = u16::try_from(a2).map_err(|_| E_INVAL)?;
            let addr = core::net::Ipv4Addr::from(a1 as u32);
            sock.connect(addr, port, a3).map(|_| 0).map_err(sock_error)
        }
        SYS_SOCKET_SEND => {
            // Returns how many bytes were queued (TCP may take fewer than asked).
            let sock = socket_handle(&proc_, a0, rights::SEND)?;
            drop(proc_);
            let data = user_bytes(a1, a2.min(MAX_SOCKET_IO))?;
            sock.send(&data).map(|n| n as u64).map_err(sock_error)
        }
        SYS_SOCKET_RECV => {
            // Handle, buffer, length, timeout in µs (0 = none). Returns the
            // bytes received; 0 from TCP means the peer closed.
            let sock = socket_handle(&proc_, a0, rights::RECV)?;
            drop(proc_);
            let len = a2.min(MAX_SOCKET_IO);
            check_writable(a1, len)?;
            let data = sock.recv(len as usize, a3).map_err(sock_error)?;
            to_user(a1, &data)?;
            Ok(data.len() as u64)
        }
        SYS_SOCKET_LISTEN => {
            // Handle of an unconnected TCP socket, port.
            let sock = socket_handle(&proc_, a0, rights::RECV)?;
            let port = u16::try_from(a1).map_err(|_| E_INVAL)?;
            sock.listen(port).map(|_| 0).map_err(sock_error)
        }
        SYS_SOCKET_ACCEPT => {
            // Listening handle, timeout in µs (0 = none). Returns the new
            // connection's handle.
            let sock = socket_handle(&proc_, a0, rights::RECV)?;
            let pid = proc_.pid;
            drop(proc_);
            let conn = sock.accept(a1).map_err(sock_error)?;
            // Not held while waiting, like the other blocking calls.
            let proc_ = sched::current().process.clone().filter(|p| p.pid == pid).ok_or(E_INVAL)?;
            let h = proc_.handles.lock().insert(Handle { object: Object::Socket(conn), rights: rights::ALL });
            Ok(h)
        }
        SYS_NET_INFO => {
            // Writes four u32s at a0: address, prefix length, gateway, DNS
            // server (addresses as a << 24 | b << 16 | c << 8 | d, 0 for none).
            let lease = net::info().ok_or(E_NOTFOUND)?;
            let ip = |a: Option<core::net::Ipv4Addr>| a.map_or(0, u32::from);
            let words = [
                u32::from(lease.address.address()),
                lease.address.prefix_len() as u32,
                ip(lease.router),
                ip(lease.dns),
            ];
            let mut bytes = [0u8; 16];
            for (i, w) in words.iter().enumerate() {
                bytes[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
            }
            to_user(a0, &bytes)?;
            Ok(0)
        }
        SYS_EVENT_CREATE => {
            // a0: 1 = manual reset (stays set until reset), 0 = auto reset.
            let event = wait::Event::new(a0 != 0);
            Ok(proc_.handles.lock().insert(Handle { object: Object::Event(event), rights: rights::ALL }))
        }
        SYS_EVENT_SET => {
            event_handle(&proc_, a0, rights::SEND)?.set();
            Ok(0)
        }
        SYS_EVENT_RESET => {
            event_handle(&proc_, a0, rights::SEND)?.reset();
            Ok(0)
        }
        SYS_WAIT_ANY => {
            // Array of u64 handles, count, timeout in µs (0 = none). Returns
            // the index of the first ready handle: a port with a message, a
            // socket with data (or a connection to accept, or closed), a set
            // event, an exited process.
            if a1 == 0 || a1 > MAX_WAIT {
                return Err(E_INVAL);
            }
            let raw = user_bytes(a0, a1 * 8)?;
            let objects = {
                let table = proc_.handles.lock();
                raw.chunks_exact(8)
                    .map(|b| {
                        let h = u64::from_le_bytes(b.try_into().unwrap());
                        let handle = table.get(h).ok_or(E_BADHANDLE)?;
                        if handle.rights & rights::RECV == 0 && !matches!(handle.object, Object::Process(_)) {
                            return Err(E_RIGHTS);
                        }
                        Ok(handle.object.clone())
                    })
                    .collect::<Result<Vec<Object>, i64>>()?
            };
            drop(proc_);
            let deadline = (a2 != 0).then(|| apic::micros().saturating_add(a2));
            wait::wait_any(deadline, || objects.iter().position(ready)).map(|i| i as u64).map_err(|e| match e {
                wait::WaitError::TimedOut => E_TIMEDOUT,
                wait::WaitError::Killed => E_INVAL,
            })
        }
        SYS_PROCESS_HANDLE => {
            // A handle to child process a0 (one this process spawned).
            let child = process::PROCESSES.lock().get(&a0).cloned().ok_or(E_NOTFOUND)?;
            if child.parent != proc_.pid {
                return Err(E_RIGHTS);
            }
            let exit = child.exit.clone();
            drop(child);
            Ok(proc_.handles.lock().insert(Handle { object: Object::Process(exit), rights: rights::ALL }))
        }
        SYS_PROCESS_KILL => {
            // Process handle, exit code. Its threads stop; waiters see it exit.
            let exit = {
                let table = proc_.handles.lock();
                let handle = table.get(a0).ok_or(E_BADHANDLE)?;
                if handle.rights & rights::SEND == 0 {
                    return Err(E_RIGHTS);
                }
                match &handle.object {
                    Object::Process(x) => x.clone(),
                    _ => return Err(E_BADHANDLE),
                }
            };
            let target = process::PROCESSES.lock().get(&exit.pid).cloned();
            if let Some(p) = target {
                process::kill(&p, a1 as i64);
            }
            Ok(0)
        }
        SYS_DISPLAY_ACQUIRE => {
            // The whole screen for this process; returns width << 32 | height.
            display::acquire(proc_.pid).map(|(w, h)| (w as u64) << 32 | h as u64).map_err(display_error)
        }
        SYS_DISPLAY_PRESENT => {
            // Image at a0 with rows of a1 pixels (0x00RRGGBB); a2 packs the
            // rectangle to show: x | y << 16 | w << 32 | h << 48.
            let field = |shift: u64| ((a2 >> shift) & 0xFFFF) as usize;
            display::present(proc_.pid, a0, a1 as usize, (field(0), field(16), field(32), field(48)))
                .map(|_| 0)
                .map_err(display_error)
        }
        SYS_DISPLAY_RELEASE => Ok(display::release(proc_.pid) as u64),
        // The mouse pointer for the screen's owner: x | y << 16 | buttons << 32 | left presses << 40 | double-clicks << 60.
        SYS_POINTER => input::pointer(proc_.pid).ok_or(E_RIGHTS),
        SYS_MOUSE_SPEED => {
            // Pointer speed 1-10: a0 = 0 reads it, 1-10 sets it; returns the speed.
            use core::sync::atomic::Ordering::Relaxed;
            match a0 {
                0 => {}
                1..=10 => {
                    crate::usb::MOUSE_SPEED.store(a0 as u32, Relaxed);
                    crate::kprintln!("mouse: pointer speed {} of 10", a0);
                }
                _ => return Err(E_INVAL),
            }
            Ok(crate::usb::MOUSE_SPEED.load(Relaxed) as u64)
        }
        SYS_TIME => {
            // Wall clock (local time, from the CMOS clock):
            // year << 40 | month << 32 | day << 24 | hour << 16 | minute << 8 | second.
            let t = crate::rtc::now().ok_or(E_NOTFOUND)?;
            Ok((t.year as u64) << 40 | (t.month as u64) << 32 | (t.day as u64) << 24 | (t.hour as u64) << 16
                | (t.minute as u64) << 8 | t.second as u64)
        }
        SYS_KEYS_READ => {
            // Up to a1 typed keys (ASCII) into a0 for the screen's owner; returns
            // how many. With a2 = 1, two bytes a key: the character, then the
            // modifier keys held as it was typed (Shift 1, Ctrl 2, Alt 4, Caps 8).
            let mut keys = [0u16; 64];
            let max = (a1 as usize).min(keys.len());
            let n = input::read_keys(proc_.pid, &mut keys[..max]).ok_or(E_RIGHTS)?;
            let bytes: Vec<u8> = if a2 == 1 {
                keys[..n].iter().flat_map(|k| [*k as u8, (*k >> 8) as u8]).collect()
            } else {
                keys[..n].iter().map(|k| *k as u8).collect()
            };
            if n > 0 && !security::copy_to_user(a0, &bytes) {
                return Err(E_FAULT);
            }
            Ok(n as u64)
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
