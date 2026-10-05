//! sectest: tries things a hostile program would try and checks that the
//! kernel refuses each one. System calls with bad pointers must fail with
//! "bad address" instead of reading or writing memory they shouldn't, and the
//! two helper programs must be killed by the CPU's page protections.

#![no_std]
#![no_main]

use aero::sys;

aero::entry!(main);

const E_FAULT: i64 = -2;

fn expect_fault(what: &str, r: i64, passed: &mut u32, failed: &mut u32) {
    if r == E_FAULT {
        *passed += 1;
        aero::println!("[sectest] blocked: {}", what);
    } else {
        *failed += 1;
        aero::println!("[sectest] NOT BLOCKED: {} (returned {})", what, r);
    }
}

fn main() -> i64 {
    let (mut passed, mut failed) = (0u32, 0u32);
    let code_page = main as fn() -> i64 as usize as u64;
    let path = "/README.TXT";
    let r = |n, a0, a1, a2, a3| unsafe { aero::syscall(n, a0, a1, a2, a3) };

    expect_fault("print kernel memory (kernel image address)",
        r(sys::WRITE, 0xFFFF_FFFF_8000_0000, 64, 0, 0), &mut passed, &mut failed);
    expect_fault("print kernel memory (direct map of RAM)",
        r(sys::WRITE, 0xFFFF_8000_0010_0000, 64, 0, 0), &mut passed, &mut failed);
    expect_fault("print from an unmapped user address",
        r(sys::WRITE, 0x1000_0000, 16, 0, 0), &mut passed, &mut failed);
    expect_fault("buffer that runs past the end of user space",
        r(sys::WRITE, 0x0000_7FFF_FFFF_FFF0, 0x100, 0, 0), &mut passed, &mut failed);
    expect_fault("pointer + length that wraps around",
        r(sys::WRITE, u64::MAX - 8, 64, 0, 0), &mut passed, &mut failed);
    expect_fault("read a file into kernel memory",
        r(sys::FILE_READ, path.as_ptr() as u64, path.len() as u64, 0xFFFF_FFFF_8000_0000, 64), &mut passed, &mut failed);
    expect_fault("read a file over this program's own read-only code",
        r(sys::FILE_READ, path.as_ptr() as u64, path.len() as u64, code_page, 64), &mut passed, &mut failed);

    aero::println!("[sectest] {} of {} attacks blocked{}", passed, passed + failed,
        if failed == 0 { ", system call checks OK" } else { "" });

    // These two must be killed by the CPU: running code from the stack (NX)
    // and writing over their own code (read-only pages).
    for program in ["nxtest", "rotest"] {
        if aero::spawn(program).is_err() {
            aero::println!("[sectest] could not start {}", program);
        }
    }
    failed as i64
}
