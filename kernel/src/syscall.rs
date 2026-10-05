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
use crate::{apic, arch, console, futex, gamepad, gdt, percpu, sched, security, sound, vfs};

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
        // Kernel CS = 0x08 (SS 0x10). sysret: SS = 0x10 + 8 = user data,
        // CS = 0x10 + 16 = user code (both with RPL 3), the GDT's order.
        arch::wrmsr(MSR_STAR, (0x10u64 << 48) | ((gdt::KERNEL_CODE as u64) << 32));
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

/// User addresses end here (the lower half of the address space).
const USER_END: u64 = 0x0000_8000_0000_0000;

/// Largest file one SYS_FILE_WRITE can save.
const MAX_FILE_WRITE: u64 = 8 * 1024 * 1024;

const E_BADHANDLE: i64 = -1;
const E_FAULT: i64 = -2;
const E_NOTFOUND: i64 = -3;
const E_RIGHTS: i64 = -4;
const E_FULL: i64 = -5;
const E_INVAL: i64 = -6;
const E_EXISTS: i64 = -7;
const E_AGAIN: i64 = -8;

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
    let Object::Port(p) = &handle.object;
    Ok(p.clone())
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
            futex::wait(pid, a0, a1 as u32).map(|_| 0).map_err(|e| match e {
                futex::WaitError::Again => E_AGAIN,
                futex::WaitError::Fault => E_FAULT,
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
