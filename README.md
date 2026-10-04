# AeroForge OS

A from-scratch x86-64 operating system: a Rust kernel core ("AeroKernel") with C++
drivers behind a narrow C ABI (the Driver Host Interface). The full plan lives in
[`../AeroForge-OS-Design.md`](../AeroForge-OS-Design.md).

## What works today (milestone 0.2)

Boots on UEFI through Limine, brings up every CPU, and starts **aerosmss**, the first user
process. aerosmss starts an `echod` service and three clients; the clients run in ring 3 on
different cores and do request/reply round trips with `echod` over IPC ports.

| Area | Status |
|---|---|
| Boot | UEFI only, Limine 9.x, higher-half kernel at `0xffffffff80000000`, user programs loaded as boot modules |
| Consoles | COM1 serial log + framebuffer console drawn on a procedural Aero-style desktop |
| Memory | Binary **buddy** page allocator (4 KiB to 4 MiB blocks, coalescing), **slab** heap (16 B to 2 KiB size classes, larger requests straight from the buddy), per-process address spaces sharing the kernel half |
| CPU tables | Per-CPU GDT and TSS (`rsp0` updated on every switch), double-fault IST stack, per-CPU data through GS with `swapgs` on ring transitions |
| Interrupts | 256-vector IDT generated at build time, exceptions from ring 3 kill only the faulting process |
| Interrupt controllers | Local APIC (per-CPU periodic timer, calibrated against the PIT) and I/O APIC with MADT overrides; the 8259 PIC is masked |
| Scheduler | Preemptive round robin with one run queue per CPU, idle thread per CPU, sleep, block/wake, reschedule IPIs for cross-core wakeups (sub-millisecond IPC round trips) |
| Processes | ELF64 loader, ring 3, syscall gate (`int 0x80`), exit and cleanup of address space and kernel stack |
| Objects and IPC | Handles with rights (capabilities), IPC ports with 256-byte messages, handle transfer in messages, a name service (`publish` / `lookup`, lookups only grant send rights) |
| C++ drivers | PS/2 keyboard driver in freestanding C++20 behind `drivers/include/dhi.h` |
| Userland | `libaero` system call library, `aerosmss`, `echod`, `client`, `crasher` (Rust, `no_std`) |
| Shell | `ps`, `sched`, `run <prog>`, `ports`, `mem`, `cpu`, `acpi`, `uptime`, `int3`, `panic` |
| Test | `tools/boot-test.sh` boots headless and checks every CPU and the whole IPC demo |

### System calls (`int 0x80`, number in `rax`, args in `rdi rsi rdx r10`)

`exit`, `write`, `yield`, `getpid`, `sleep_ms`, `cpu_id`, `uptime_ms`, `spawn`, `port_create`,
`port_publish`, `port_lookup`, `port_send`, `port_recv`, `handle_close`, `handle_dup`.
The numbers are in `kernel/src/syscall.rs` and `userland/src/lib.rs`.

### Still to do in Phase 1

1. Fast `syscall`/`sysret` entry, FPU/SSE state saving (user code is soft-float for now), and thread creation inside a process.
2. Load balancing between CPU run queues (threads are pinned to the CPU they start on), plus priorities and the game/real-time classes.
3. Futexes and event objects, and `wait()` for child processes.
4. An ACPICA port (the current table walker never touches AML), HPET/TSC-deadline timers, and x2APIC mode.
5. `dhi.idl` and a generator for `dhi.h` / `dhi.rs`, then virtio-blk and NVMe/AHCI drivers in C++ (needed for "reading files from a disk image").
6. KASLR and SMEP/SMAP/UMIP hardening (Zen 2 and newer Ryzen CPUs support all three).

## Layout

```
boot/limine.conf          boot menu
kernel/                   AeroKernel (Rust, no_std, stable toolchain)
  build.rs                compiles the C++ drivers, generates the ISR stubs
  linker.ld               higher-half layout, Limine request sections
  src/main.rs             boot sequence (kmain)
  src/limine.rs           Limine protocol bindings (no external crate)
  src/memory.rs           buddy allocator, slab heap, page tables, address spaces
  src/{gdt,percpu}.rs     per-CPU GDT/TSS and GS-based per-CPU data
  src/interrupts.rs       IDT and dispatch (isr_stubs.s.in is the entry template)
  src/{apic,pic}.rs       LAPIC timer, IPIs, I/O APIC; legacy PIC masking
  src/sched.rs            threads, per-CPU run queues, context switch
  src/{process,elf}.rs    processes, handle tables, ELF loading
  src/{ipc,syscall}.rs    IPC ports, name service, system call table
  src/{acpi,smp}.rs       firmware tables, application processors
  src/sync.rs             IrqMutex (interrupt-safe spinlock)
  src/dhi.rs              Rust side of the Driver Host Interface
  src/{console,fb,serial}.rs    output: serial, framebuffer, desktop art
  src/shell.rs            kernel-mode command prompt (a kernel thread)
userland/                 user programs and libaero (Rust, no_std)
  src/lib.rs              system call wrappers, println!, handles
  src/bin/aerosmss.rs     session manager, first process
  src/bin/{echod,client,crasher}.rs   IPC demo service and clients, fault demo
drivers/include/dhi.h     the DHI contract (C ABI)
drivers/ps2kbd/           first C++ driver
tools/boot-test.sh        headless QEMU boot test
```

## Building and running

Requirements: Rust stable with the `x86_64-unknown-none` target, `clang++` and `llvm-ar`,
`xorriso`, `git`, QEMU (`qemu-system-x86_64`) and OVMF UEFI firmware.

On Ubuntu/Debian:

```sh
sudo apt install clang llvm lld xorriso qemu-system-x86 ovmf
rustup target add x86_64-unknown-none
```

Then:

```sh
make                    # builds build/aeroforge.iso (fetches Limine binaries on first run)
make run                # QEMU window; type into the AeroKernel shell
make run-headless       # serial log on stdout, no window
./tools/boot-test.sh    # CI-style pass/fail boot
```

On Windows, the simplest route is WSL2 (Ubuntu) with the commands above; `make run` opens
the QEMU window through WSLg. OVMF paths can be overridden with
`make run OVMF_CODE=... OVMF_VARS=...`.

The ISO also boots on real UEFI PCs from a USB stick (write it with Rufus in DD mode or
`dd`), with CSM/legacy boot off and Secure Boot off. Expect a PS/2-only keyboard until the
USB (xHCI) driver lands. On a Ryzen desktop with only USB keyboards, typing into the shell
works only if the firmware's USB legacy support emulates PS/2. Booting and the IPC demo
don't need a keyboard.
