# AeroForge OS

A from-scratch x86-64 operating system: a Rust kernel core ("AeroKernel") with C++
drivers behind a narrow C ABI (the Driver Host Interface). The full plan lives in
[`../AeroForge-OS-Design.md`](../AeroForge-OS-Design.md).

## What works today (milestone 0.1)

Boots on UEFI through Limine, in QEMU, to an Aero-style console with a working shell.

| Area | Status |
|---|---|
| Boot | UEFI only, Limine 9.x, higher-half kernel at `0xffffffff80000000` |
| Consoles | COM1 serial log + framebuffer console drawn on a procedural Aero-style desktop |
| CPU tables | GDT with ring-0 and ring-3 segments, TSS with a dedicated double-fault stack |
| Interrupts | IDT with all 32 exceptions + 16 IRQs, assembly stubs into one Rust dispatcher; exceptions print a report and panic, `#BP` resumes |
| Memory | Frame allocator over the Limine memory map, a 4-level page-table mapper and walker, an 8 MiB kernel heap at `0xffffe00000000000` (so `Box`, `Vec`, `BTreeMap`, `String` work) |
| Firmware | ACPI RSDP, XSDT and MADT parsing (CPU and I/O APIC counts) |
| Timers / IRQs | 8259 PIC remapped to 32-47, 8254 PIT at 100 Hz |
| SMP | All application processors released and checked in (4 of 4 in QEMU); they idle until a scheduler exists |
| C++ drivers | PS/2 keyboard driver in freestanding C++20 (`-fno-exceptions -fno-rtti`) talking to the kernel only through `drivers/include/dhi.h` |
| Shell | `help`, `about`, `mem`, `cpu`, `acpi`, `uptime`, `int3`, `panic` |
| Test | `tools/boot-test.sh` boots headless and fails on panic or missing CPUs |

### Not done yet (rest of roadmap Phase 1)

These are the next steps, in roughly this order:

1. Buddy + slab allocators and per-process address spaces (replacing the bump/free-list frame allocator).
2. LAPIC/x2APIC timer and IOAPIC (replacing PIC/PIT), HPET/TSC calibration.
3. Scheduler with per-CPU run queues, kernel threads, then ring 3 user mode and the `syscall` gate.
4. Object manager, handles as capabilities, IPC ports, futexes, events.
5. ACPICA port (the current table walker is a stopgap and never touches AML).
6. `dhi.idl` and a generator for `dhi.h` / `dhi.rs`; then virtio-blk and NVMe/AHCI drivers in C++.
7. KASLR, SMEP/SMAP/UMIP hardening.

## Layout

```
boot/limine.conf          boot menu
kernel/                   AeroKernel (Rust, no_std, stable toolchain)
  build.rs                compiles the C++ drivers and links them in
  linker.ld               higher-half layout, Limine request sections
  src/main.rs             boot sequence (kmain)
  src/limine.rs           Limine protocol bindings (no external crate)
  src/{gdt,interrupts,pic}.rs   CPU tables, IDT + ISR stubs, PIC/PIT
  src/memory.rs           frames, page tables, heap
  src/{acpi,smp}.rs       firmware tables, application processors
  src/dhi.rs              Rust side of the Driver Host Interface
  src/{console,fb,serial}.rs    output: serial, framebuffer, desktop art
  src/shell.rs            kernel-mode command prompt
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
USB (xHCI) driver lands.
