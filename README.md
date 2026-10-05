# AeroForge OS

[![boot test](https://github.com/vampyer/AeroForge-os/actions/workflows/boot-test.yml/badge.svg)](https://github.com/vampyer/AeroForge-os/actions/workflows/boot-test.yml)

A from-scratch x86-64 operating system: a Rust kernel core ("AeroKernel") with C++
drivers behind a narrow C ABI (the Driver Host Interface). The full plan lives in
[`docs/AeroForge-OS-Design.md`](docs/AeroForge-OS-Design.md). Every CI run uploads the built ISO as a workflow artifact (Actions tab, "boot test" run, Artifacts).

## What works today (milestone 0.6)

Boots on UEFI through Limine, brings up every CPU, and starts **aerosmss**, the first user
process. aerosmss starts an `echod` service and three clients; the clients run in ring 3 on
different cores and do request/reply round trips with `echod` over IPC ports.

Since 0.3 the list of programs aerosmss starts comes from `/system/session.cfg` on an NVMe
disk: the kernel finds the NVMe controller on PCIe, brings it up through the C++ NVMe
driver, reads the GPT partition table and mounts the FAT32 partition at `/`.

Since 0.4 SATA drives work too: the C++ **AHCI** driver finds every ATA disk on the
controller (CD/DVD drives are skipped), the kernel reads its MBR or GPT, and every further
FAT32 volume is mounted at `/<device>`, for example `/sata0p1`.

Since 0.5 USB works through the C++ **xHCI** driver: it resets the controller, enumerates
every device on the root ports (USB 2 and USB 3 SuperSpeed), reads its descriptors and drives
HID boot-protocol keyboards and mice. USB keys go to the shell like PS/2 keys; `lsusb` lists
the devices and `mouse` shows the pointer position. Devices behind USB hubs are found too, through
nested hubs (tested with QEMU's USB 1.1 hub; the USB 2 and USB 3 hub paths are written but
need real hardware to test).

Since 0.6 there is networking: a C++ **Intel Ethernet** driver for the e1000 and e1000e family
(8254x, 8257x and the I217/I218/I219 chips built into many Intel boards) feeds the
[smoltcp](https://github.com/smoltcp-rs/smoltcp) TCP/IP stack, which runs in a kernel thread.
At boot the card gets an address over DHCP and pings the gateway; `ifconfig` shows the card,
link and address and `ping <ip>` sends echo requests. The I210/I211 (igb) and I225/I226 (igc)
chips need their own driver, which comes next; until then they are listed with a warning.

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
| PCIe | Enumeration through ECAM (ACPI MCFG), 64-bit BARs, bus mastering |
| C++ drivers | Behind `drivers/include/dhi.h` (ABI v2: logging, port I/O, DMA buffers, MMIO mapping, delays): PS/2 keyboard, an **NVMe** driver (admin + I/O queue pair, polling, Identify, reads up to 8 KiB per command), an **AHCI** (SATA) driver (one command slot per port, polling, IDENTIFY DEVICE, LBA48 READ DMA EXT) and an **xHCI** (USB 3) driver (command and event rings, device enumeration through hubs (nested up to the USB limit, transaction translators for slow devices behind fast hubs), HID boot keyboard and mouse, polled from a kernel thread) and an **Intel Ethernet** driver (e1000/e1000e: one receive and one transmit ring of legacy descriptors, MAC from the receive-address registers or EEPROM, link and speed) |
| Networking | smoltcp (IPv4, ARP, ICMP, UDP, TCP) on the first Intel NIC, polled by the `net` kernel thread; DHCP client; ICMP echo |
| Storage | Block device layer, GPT and MBR partitions, read-only **FAT32** with long file names and case-insensitive lookup, first volume at `/` and the others at `/<device>`; `file_read` system call |
| Security | NX, SMEP, SMAP, UMIP and CR0.WP on every CPU that has them; W^X kernel image (code read-only, data, heap, stacks and the direct map non-executable); guard pages under every kernel stack (overflow is reported, not silent); random stack canary (RDRAND) checked by the C++ drivers; every system call copies user memory through checked `copy_from_user` / `copy_to_user` (mapped, user-owned, writable for writes); user code W^X, stacks non-executable; a boot audit re-checks all of it |
| Userland | `libaero` system call library, `aerosmss` (reads its session from disk), `echod`, `client`, `crasher`, `sectest` / `nxtest` / `rotest` (security self-test) (Rust, `no_std`) |
| Shell | `ps`, `sched`, `run <prog>`, `ports`, `lspci`, `lsusb`, `mouse`, `ifconfig`, `ping <ip>`, `disks`, `ls`, `cat`, `wc`, `mem`, `cpu`, `acpi`, `uptime`, `int3`, `panic` |
| Test | `tools/boot-test.sh` boots headless with an NVMe (GPT) and a SATA (MBR) disk image and checks every CPU, both mounts, the USB keyboard and mouse behind a hub, a DHCP lease and a ping to the gateway over an e1000e card, the security audit and self-test, the config read and the whole IPC demo |

### System calls (`int 0x80`, number in `rax`, args in `rdi rsi rdx r10`)

`exit`, `write`, `yield`, `getpid`, `sleep_ms`, `cpu_id`, `uptime_ms`, `spawn`, `port_create`,
`port_publish`, `port_lookup`, `port_send`, `port_recv`, `handle_close`, `handle_dup`, `file_read`.
The numbers are in `kernel/src/syscall.rs` and `userland/src/lib.rs`.

### Still to do in Phase 1

1. Fast `syscall`/`sysret` entry, FPU/SSE state saving (user code is soft-float for now), and thread creation inside a process.
2. Load balancing between CPU run queues (threads are pinned to the CPU they start on), plus priorities and the game/real-time classes.
3. Futexes and event objects, and `wait()` for child processes.
4. An ACPICA port (the current table walker never touches AML), HPET/TSC-deadline timers, and x2APIC mode.
5. `dhi.idl` and a generator for `dhi.h` / `dhi.rs`; NVMe interrupts (MSI-X), one queue pair per CPU and writes; AHCI interrupts, NCQ and writes; USB mass storage, xHCI MSI-X interrupts and hotplug; Intel igb/igc (I210/I211/I225/I226) and virtio-net, network interrupts, and sockets for user programs.
6. Filesystems move to user-space servers behind IPC, as the design says; FAT32 writes, exFAT and NTFS (read-only on real drives at first).
7. KASLR (needs a position-independent kernel build), Rust stack canaries once they reach stable Rust, and TLB shootdowns so permission changes reach every CPU at once.

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
  src/pci.rs              PCIe enumeration
  src/{block,fat,vfs}.rs  block devices and partitions, FAT32, the mount at /
  src/sync.rs             IrqMutex (interrupt-safe spinlock)
  src/dhi.rs              Rust side of the Driver Host Interface
  src/{console,fb,serial}.rs    output: serial, framebuffer, desktop art
  src/shell.rs            kernel-mode command prompt (a kernel thread)
userland/                 user programs and libaero (Rust, no_std)
  src/lib.rs              system call wrappers, println!, handles
  src/bin/aerosmss.rs     session manager, first process
  src/bin/{echod,client,crasher}.rs   IPC demo service and clients, fault demo
drivers/include/dhi.h     the DHI contract (C ABI)
drivers/ps2kbd/           PS/2 keyboard driver (C++)
drivers/nvme/             NVMe driver (C++)
drivers/ahci/             AHCI (SATA) driver (C++)
drivers/xhci/             xHCI (USB 3) driver (C++)
drivers/e1000/            Intel Ethernet driver, e1000/e1000e (C++)
tools/boot-test.sh        headless QEMU boot test
tools/make-disk.sh        builds the NVMe test disk (GPT + FAT32) from tools/disk-files/
tools/make-sata-disk.sh   builds the SATA test disk (MBR + FAT32) from tools/sata-files/
```

## Building and running

Requirements: Rust stable with the `x86_64-unknown-none` target, `clang++` and `llvm-ar`,
`xorriso`, `git`, `gdisk`, `dosfstools`, `mtools`, QEMU (`qemu-system-x86_64`) and OVMF UEFI firmware.

On Ubuntu/Debian:

```sh
sudo apt install clang llvm lld xorriso gdisk dosfstools mtools qemu-system-x86 ovmf
rustup target add x86_64-unknown-none
```

Then:

```sh
make                    # builds build/aeroforge.iso (fetches Limine binaries on first run)
make disk               # (re)builds build/disk.img (NVMe drive) and build/sata.img (SATA drive)
make run                # QEMU window; type into the AeroKernel shell
make run-headless       # serial log on stdout, no window
./tools/boot-test.sh    # CI-style pass/fail boot
```

On Windows, the simplest route is WSL2 (Ubuntu) with the commands above; `make run` opens
the QEMU window through WSLg. OVMF paths can be overridden with
`make run OVMF_CODE=... OVMF_VARS=...`.

The ISO also boots on real UEFI PCs from a USB stick (write it with Rufus in DD mode or
`dd`), with CSM/legacy boot off and Secure Boot off. USB keyboards work through the xHCI
driver, also behind USB hubs (including hubs built into monitors). Devices plugged in after
boot are not picked up yet. Booting and the IPC demo
don't need a keyboard.
