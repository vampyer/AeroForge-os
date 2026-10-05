//! fputest: checks that programs can use floating point and vector
//! registers, and that the kernel keeps each program's registers apart.
//!
//! Started from the shell it is the leader: it starts four more copies of
//! itself (workers), and all five fill the x87, SSE and (when the CPU has it)
//! AVX registers with their own patterns and their own MXCSR rounding mode,
//! then give up the CPU and spin until the timer takes it away, over and
//! over, and check that nothing changed. With five busy programs on four
//! CPUs, some of them must share a CPU and be switched against each other.
//! Workers report to the leader over IPC; the leader also times the
//! `syscall` instruction against the older `int 0x80` gate.
//!
//! A worker knows it is one because the leader publishes a port named
//! "fputest/<worker pid>" for it right after starting it.

#![no_std]
#![no_main]

use core::arch::asm;

use aero::{format_buf, port, println};

aero::entry!(main);

const WORKERS: usize = 4;
const ROUNDS: u32 = 30;
const SQRT_TERMS: u32 = 100_000;

fn pattern(seed: u64, i: usize) -> u64 {
    seed.wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add((i as u64 + 1).wrapping_mul(0x1234_5678_9ABC_DEF1))
        .rotate_left(i as u32)
}

/// x87, SSE and MXCSR: load them, switch away `ROUNDS` times, read them back.
fn check_sse(seed: u64) -> Result<(), &'static str> {
    let mut src = [0u64; 32];
    for (i, v) in src.iter_mut().enumerate() {
        *v = pattern(seed, i);
    }
    let mut dst = [0u64; 32];
    let x87_in: f64 = seed as f64 * 1.5 + 0.25;
    let mut x87_out: f64 = 0.0;
    // Rounding mode (bits 13-14) differs between programs; exceptions stay masked.
    let mxcsr_in: u32 = 0x1F80 | ((seed as u32 & 3) << 13);
    let mut mxcsr_out: u32 = 0;
    unsafe {
        asm!(
            "ldmxcsr [{mi}]",
            "fld qword ptr [{xi}]",
            "movdqu xmm0, [{s}]", "movdqu xmm1, [{s} + 16]", "movdqu xmm2, [{s} + 32]", "movdqu xmm3, [{s} + 48]",
            "movdqu xmm4, [{s} + 64]", "movdqu xmm5, [{s} + 80]", "movdqu xmm6, [{s} + 96]", "movdqu xmm7, [{s} + 112]",
            "movdqu xmm8, [{s} + 128]", "movdqu xmm9, [{s} + 144]", "movdqu xmm10, [{s} + 160]", "movdqu xmm11, [{s} + 176]",
            "movdqu xmm12, [{s} + 192]", "movdqu xmm13, [{s} + 208]", "movdqu xmm14, [{s} + 224]", "movdqu xmm15, [{s} + 240]",
            "2:",
            "mov eax, {yield_}",
            "syscall",
            "mov ecx, 300000",
            "3:",
            "pause",
            "dec ecx",
            "jnz 3b",
            "dec {n:e}",
            "jnz 2b",
            "movdqu [{d}], xmm0", "movdqu [{d} + 16], xmm1", "movdqu [{d} + 32], xmm2", "movdqu [{d} + 48], xmm3",
            "movdqu [{d} + 64], xmm4", "movdqu [{d} + 80], xmm5", "movdqu [{d} + 96], xmm6", "movdqu [{d} + 112], xmm7",
            "movdqu [{d} + 128], xmm8", "movdqu [{d} + 144], xmm9", "movdqu [{d} + 160], xmm10", "movdqu [{d} + 176], xmm11",
            "movdqu [{d} + 192], xmm12", "movdqu [{d} + 208], xmm13", "movdqu [{d} + 224], xmm14", "movdqu [{d} + 240], xmm15",
            "fstp qword ptr [{xo}]",
            "stmxcsr [{mo}]",
            "ldmxcsr [{md}]",
            s = in(reg) src.as_ptr(),
            d = in(reg) dst.as_mut_ptr(),
            xi = in(reg) &x87_in,
            xo = in(reg) &mut x87_out,
            mi = in(reg) &mxcsr_in,
            mo = in(reg) &mut mxcsr_out,
            md = in(reg) &0x1F80u32,
            n = inout(reg) ROUNDS => _,
            yield_ = const aero::sys::YIELD,
            out("rax") _, out("rcx") _, out("r11") _,
            out("xmm0") _, out("xmm1") _, out("xmm2") _, out("xmm3") _,
            out("xmm4") _, out("xmm5") _, out("xmm6") _, out("xmm7") _,
            out("xmm8") _, out("xmm9") _, out("xmm10") _, out("xmm11") _,
            out("xmm12") _, out("xmm13") _, out("xmm14") _, out("xmm15") _,
        );
    }
    if src != dst {
        return Err("SSE registers changed");
    }
    if x87_out.to_bits() != x87_in.to_bits() {
        return Err("x87 register changed");
    }
    if mxcsr_out != mxcsr_in {
        return Err("MXCSR changed");
    }
    Ok(())
}

fn has_avx() -> bool {
    let leaf1 = core::arch::x86_64::__cpuid(1);
    if leaf1.ecx & (1 << 27) == 0 || leaf1.ecx & (1 << 28) == 0 {
        return false; // no OSXSAVE or no AVX
    }
    let xcr0: u32;
    unsafe { asm!("xgetbv", in("ecx") 0, out("eax") xcr0, out("edx") _, options(nomem, nostack)) };
    xcr0 & 0b110 == 0b110
}

/// The full 256-bit AVX registers, the same way.
fn check_avx(seed: u64) -> Result<(), &'static str> {
    let mut src = [0u64; 64];
    for (i, v) in src.iter_mut().enumerate() {
        *v = pattern(seed ^ 0xA5A5, i);
    }
    let mut dst = [0u64; 64];
    unsafe {
        asm!(
            "vmovdqu ymm0, [{s}]", "vmovdqu ymm1, [{s} + 32]", "vmovdqu ymm2, [{s} + 64]", "vmovdqu ymm3, [{s} + 96]",
            "vmovdqu ymm4, [{s} + 128]", "vmovdqu ymm5, [{s} + 160]", "vmovdqu ymm6, [{s} + 192]", "vmovdqu ymm7, [{s} + 224]",
            "vmovdqu ymm8, [{s} + 256]", "vmovdqu ymm9, [{s} + 288]", "vmovdqu ymm10, [{s} + 320]", "vmovdqu ymm11, [{s} + 352]",
            "vmovdqu ymm12, [{s} + 384]", "vmovdqu ymm13, [{s} + 416]", "vmovdqu ymm14, [{s} + 448]", "vmovdqu ymm15, [{s} + 480]",
            "2:",
            "mov eax, {yield_}",
            "syscall",
            "mov ecx, 300000",
            "3:",
            "pause",
            "dec ecx",
            "jnz 3b",
            "dec {n:e}",
            "jnz 2b",
            "vmovdqu [{d}], ymm0", "vmovdqu [{d} + 32], ymm1", "vmovdqu [{d} + 64], ymm2", "vmovdqu [{d} + 96], ymm3",
            "vmovdqu [{d} + 128], ymm4", "vmovdqu [{d} + 160], ymm5", "vmovdqu [{d} + 192], ymm6", "vmovdqu [{d} + 224], ymm7",
            "vmovdqu [{d} + 256], ymm8", "vmovdqu [{d} + 288], ymm9", "vmovdqu [{d} + 320], ymm10", "vmovdqu [{d} + 352], ymm11",
            "vmovdqu [{d} + 384], ymm12", "vmovdqu [{d} + 416], ymm13", "vmovdqu [{d} + 448], ymm14", "vmovdqu [{d} + 480], ymm15",
            "vzeroupper",
            s = in(reg) src.as_ptr(),
            d = in(reg) dst.as_mut_ptr(),
            n = inout(reg) ROUNDS => _,
            yield_ = const aero::sys::YIELD,
            out("rax") _, out("rcx") _, out("r11") _,
            out("xmm0") _, out("xmm1") _, out("xmm2") _, out("xmm3") _,
            out("xmm4") _, out("xmm5") _, out("xmm6") _, out("xmm7") _,
            out("xmm8") _, out("xmm9") _, out("xmm10") _, out("xmm11") _,
            out("xmm12") _, out("xmm13") _, out("xmm14") _, out("xmm15") _,
        );
    }
    if src != dst { Err("AVX registers changed") } else { Ok(()) }
}

/// Ordinary floating point code: the sum of the square roots of 1..=n.
fn sqrt_sum(n: u32) -> f64 {
    let mut sum = 0.0f64;
    for i in 1..=n {
        let mut x = i as f64;
        unsafe { asm!("sqrtsd {0}, {0}", inout(xmm_reg) x, options(pure, nomem, nostack)) };
        sum += x;
        if i % 10_000 == 0 {
            aero::yield_now();
        }
    }
    sum
}

/// Average cycles per `getpid` call through `syscall` and through `int 0x80`.
fn time_calls() -> (u64, u64) {
    const N: u64 = 2000;
    let t0 = unsafe { core::arch::x86_64::_rdtsc() };
    for _ in 0..N {
        aero::getpid();
    }
    let t1 = unsafe { core::arch::x86_64::_rdtsc() };
    for _ in 0..N {
        unsafe { asm!("int 0x80", inlateout("rax") aero::sys::GETPID => _, options(nostack)) };
    }
    let t2 = unsafe { core::arch::x86_64::_rdtsc() };
    ((t1 - t0) / N, (t2 - t1) / N)
}

fn run_checks(seed: u64, avx: bool) -> Result<(), &'static str> {
    check_sse(seed)?;
    if avx {
        check_avx(seed)?;
    }
    Ok(())
}

fn main() -> i64 {
    let pid = aero::getpid();

    // Worker? The leader names a port after us shortly after we start.
    let own = format_buf!("fputest/{}", pid);
    let own = core::str::from_utf8(own.as_bytes()).unwrap_or("");
    for _ in 0..15 {
        if let Ok(leader) = port::lookup(own) {
            let r = run_checks(pid, has_avx());
            let msg = match r {
                Ok(()) => format_buf!("pid {} on cpu{}: OK", pid, aero::cpu_id()),
                Err(e) => format_buf!("pid {} on cpu{}: {}", pid, aero::cpu_id(), e),
            };
            let _ = port::send(&leader, msg.as_bytes(), None);
            return if r.is_ok() { 0 } else { 1 };
        }
        aero::sleep_ms(20);
    }

    // Leader.
    let avx = has_avx();
    let inbox = port::create().expect("port_create");
    for _ in 0..WORKERS {
        match aero::spawn("fputest") {
            Ok(child) => {
                let name = format_buf!("fputest/{}", child);
                port::publish(&inbox, core::str::from_utf8(name.as_bytes()).unwrap_or("")).expect("publish");
            }
            Err(e) => {
                println!("[fputest] could not start a worker: {}", e);
                return 1;
            }
        }
    }
    let mut failed = 0;
    if let Err(e) = run_checks(pid, avx) {
        println!("[fputest] pid {} on cpu{}: {}", pid, aero::cpu_id(), e);
        failed += 1;
    }
    let mut buf = [0u8; 128];
    for _ in 0..WORKERS {
        let r = port::recv(&inbox, &mut buf).expect("recv");
        let text = core::str::from_utf8(&buf[..r.len]).unwrap_or("?");
        if !text.ends_with(": OK") {
            println!("[fputest] {}", text);
            failed += 1;
        }
    }
    let sum = sqrt_sum(SQRT_TERMS);
    let (fast, slow) = time_calls();
    println!("[fputest] system calls: syscall {} cycles, int 0x80 {} cycles", fast, slow);
    if failed > 0 {
        println!("[fputest] FAILED: {} of {} programs lost register contents", failed, WORKERS + 1);
        return 1;
    }
    println!(
        "[fputest] {} programs kept their x87, SSE{} and MXCSR state across {} rounds of switching; sum of square roots of 1..{} = {:.6}: OK",
        WORKERS + 1, if avx { ", AVX" } else { "" }, ROUNDS, SQRT_TERMS, sum
    );
    0
}
