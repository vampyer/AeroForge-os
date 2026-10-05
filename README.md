# AeroForge OS

[![boot test](https://github.com/vampyer/AeroForge-os/actions/workflows/boot-test.yml/badge.svg)](https://github.com/vampyer/AeroForge-os/actions/workflows/boot-test.yml)

A from-scratch x86-64 operating system: a Rust kernel core ("AeroKernel") with C++
drivers behind a narrow C ABI (the Driver Host Interface). The full plan lives in
[`docs/AeroForge-OS-Design.md`](docs/AeroForge-OS-Design.md). Every CI run uploads the built ISO as a workflow artifact (Actions tab, "boot test" run, Artifacts).

## What works today (milestone 0.9)

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
link and address and `ping <ip>` sends echo requests. A second C++ driver covers the **igb**
(82576, I350, I210, I211) and **igc** (I225, I226, 2.5 Gb/s) families with advanced descriptors;
on the I225 it turns off Energy Efficient Ethernet, Intel's workaround for link drops on the
early B1/B2 steppings. QEMU emulates the igb family, which tests the shared code; the I225/I226
specifics (device reset, 2.5 Gb/s, EEE) need real hardware.

Since 0.7 USB sticks and USB drives work: the xHCI driver speaks the bulk-only mass storage
protocol with SCSI commands (INQUIRY, TEST UNIT READY, READ CAPACITY, READ(10)/READ(16)),
recovers from stalls, and the disks join the block layer as `usb0`, `usb1`, ... with their
FAT32 volumes mounted at `/usb0p1` and so on. It works on USB 3 ports and behind hubs (tested
in QEMU at 5 Gb/s and at full speed behind two nested hubs). Disk reads and the keyboard/mouse
polling share the controller safely across CPUs. UAS (the faster protocol some USB 3 SSDs
prefer) comes later; those drives also offer bulk-only, so they work now.

Since 0.8 USB Bluetooth adapters come up. The xHCI driver carries HCI over USB (commands on
the control endpoint, events on the interrupt endpoint, ACL data on the bulk endpoints), the
kernel's Bluetooth host (`kernel/src/bt/`) brings each adapter up (reset, version, address,
buffer sizes, event masks) and scans for classic devices (inquiry with extended results) and
Bluetooth LE devices (active scan) side by side, showing each device's name, signal strength
and kind (gamepad, keyboard, mouse, headset, ...). MediaTek MT7921 and MT7922 adapters (the
Bluetooth half of MediaTek Wi-Fi cards) boot with a ROM only, so `drivers/btmtk` downloads
their firmware patch first, a port of Linux's btmtk set-up. The firmware comes from
linux-firmware at build time (`tools/fetch-firmware.sh`, pinned commit and SHA-256 sums) and
is loaded from `/boot/firmware/mediatek/`; MediaTek's licence allows redistribution for use
with MediaTek devices and is shipped next to it. Adapters that need no firmware (most generic
dongles) work directly. QEMU has no Bluetooth device, so `tools/fakebt` simulates an MT7921
adapter over usbredir: it checks every downloaded firmware byte against the file and answers
scans with a classic gamepad, an LE gamepad and a headset. Realtek and Intel adapters need
their own firmware loaders (later).

Since 0.9 classic Bluetooth gamepads work. `bt pair <address>` (with the pad in pairing mode)
connects, pairs with Secure Simple Pairing ("Just Works", or PIN 0000 for older pads), turns on
encryption, reads the pad's HID report descriptor over SDP and opens the HID control and
interrupt channels over L2CAP. A small HID report-descriptor parser finds the sticks and
triggers (X, Y, Z, Rx, Ry, Rz, gas, brake), the d-pad (hat switch) and up to 32 buttons, so any
standard gamepad works without a per-model driver; `gamepad` shows what each pad is pressing.
A paired pad reconnects by itself when switched on (the adapter keeps page scan on and accepts
only paired devices). Pairings live in memory until AeroForge gets a writable disk, so pads have
to be paired again after a restart. Programs cannot read the gamepad yet, and Bluetooth LE
gamepads (HID over GATT) come next. The simulated adapter also plays a classic gamepad that
pairs, sends input, switches off and on and reconnects, which the boot test drives through the
shell.

Since 0.10 Bluetooth headset microphones work. `bt pair <address>` on a headset finds no HID
record over SDP, so it looks up the hands-free (or the older headset) record for the RFCOMM
channel, opens RFCOMM (TS 07.10 with credit flow control) and acts as the audio gateway: it
answers the headset's AT commands as a phone with no call would, until the service level
connection is up. `mic record [seconds]` then opens a voice (SCO) link, 16-bit samples at 8 kHz
over CVSD, which arrive through the adapter's isochronous USB endpoint (the xHCI driver
switches the voice interface's alternate setting and queues isochronous transfers), and reports
the length, peak and RMS level and the pitch; `mic` lists headsets. AeroForge also answers SDP
queries for its own audio gateway record, so a paired headset that is switched on reconnects by
itself. There is no sound output or audio API for programs yet: that comes with the HDA driver.
The simulated headset pairs, holds a 440 Hz tone up to its microphone, switches off and on and
reconnects, and the boot test records it before and after.

Since 0.11 there is sound. A C++ Intel High Definition Audio driver (`drivers/hda`) resets each
HDA controller (the motherboard's audio chip, and the audio function of graphics cards), talks
to its codecs over the CORB/RIRB command rings, walks each codec's widget graph to find the
output jacks and a path from a DAC to each (through mixers and selectors, unmuting amplifiers
at 0 dB, turning on external amplifiers), and plays 48 kHz 16-bit stereo through one output
stream with a cyclic buffer. `sound` lists controllers, codecs and outputs (jack, colour, and
whether something is plugged in), `sound use <card>.<output>` picks one (by default a jack
with something plugged in), and `sound test [hz] [seconds]` plays a tone. HDMI/DisplayPort
outputs of a Radeon card are found but stay silent until the display driver (Phase 7) turns on
the audio of the connected screen. There is no recording from the board's inputs yet. The
boot test plays a tone on QEMU's emulated HDA card and measures it in the WAV file QEMU records.

Since 0.12 programs can play sound. Two system calls, `audio_write` (48 kHz 16-bit stereo
frames, up to 0.1 s per call) and `audio_queued`, give every process its own stream of up to a
quarter of a second; libaero wraps them as `audio::write_all` and `audio::drain`. A kernel
`mixer` thread sums all streams (with clipping), keeps about 100 ms queued on the card, starts
the output when something plays and stops it after half a second of silence, and follows
`sound use`. The `sound test` tone goes through the same mixer, so it plays alongside programs.
`run melody` plays a C major arpeggio, and the boot test checks its four notes in QEMU's WAV
output.

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
| C++ drivers | Behind `drivers/include/dhi.h` (ABI v2: logging, port I/O, DMA buffers, MMIO mapping, delays): PS/2 keyboard, an **NVMe** driver (admin + I/O queue pair, polling, Identify, reads up to 8 KiB per command), an **AHCI** (SATA) driver (one command slot per port, polling, IDENTIFY DEVICE, LBA48 READ DMA EXT) and an **xHCI** (USB 3) driver (command and event rings, device enumeration through hubs (nested up to the USB limit, transaction translators for slow devices behind fast hubs), HID boot keyboard and mouse, bulk-only mass storage with SCSI, Bluetooth HCI transport with isochronous voice endpoints, polled from a kernel thread) and an **Intel Ethernet** driver (e1000/e1000e: one receive and one transmit ring of legacy descriptors, MAC from the receive-address registers or EEPROM, link and speed) and an **igb/igc** driver (I210/I211/I350/82576 and I225/I226 at up to 2.5 Gb/s: advanced descriptors, PHY power-up and auto-negotiation over MDIO, I225 EEE workaround) and a **MediaTek Bluetooth** set-up driver (MT7921/MT7922 firmware download over the WMT vendor protocol) and an **HD Audio** driver (CORB/RIRB, codec widget graph, output routing, one 48 kHz stereo output stream) |
| Networking | smoltcp (IPv4, ARP, ICMP, UDP, TCP) on the first Intel NIC (e1000, e1000e, igb or igc), polled by the `net` kernel thread; DHCP client; ICMP echo |
| Storage | Block device layer, GPT and MBR partitions, read-only **FAT32** with long file names and case-insensitive lookup, first volume at `/` and the others at `/<device>`; `file_read` system call |
| Security | NX, SMEP, SMAP, UMIP and CR0.WP on every CPU that has them; W^X kernel image (code read-only, data, heap, stacks and the direct map non-executable); guard pages under every kernel stack (overflow is reported, not silent); random stack canary (RDRAND) checked by the C++ drivers; every system call copies user memory through checked `copy_from_user` / `copy_to_user` (mapped, user-owned, writable for writes); user code W^X, stacks non-executable; a boot audit re-checks all of it |
| Userland | `libaero` system call library, `aerosmss` (reads its session from disk), `echod`, `client`, `crasher`, `sectest` / `nxtest` / `rotest` (security self-test), `melody` (plays sound) (Rust, `no_std`) |
| Shell | `ps`, `sched`, `run <prog>`, `ports`, `lspci`, `lsusb`, `mouse`, `ifconfig`, `ping <ip>`, `bt`, `bt scan`, `bt pair`, `gamepad`, `mic`, `mic record`, `sound`, `sound test`, `sound use`, `disks`, `ls`, `cat`, `wc`, `mem`, `cpu`, `acpi`, `uptime`, `int3`, `panic` |
| Test | `tools/boot-test.sh` boots headless with an NVMe (GPT) and a SATA (MBR) disk image and a USB stick, and checks every CPU, all three mounts, the USB keyboard and mouse behind a hub, igb and e1000e network cards, a DHCP lease and a ping to the gateway over the igb card, the MediaTek firmware download, a Bluetooth scan, gamepad pairing, input and reconnection and headset pairing, microphone recording and reconnection against the simulated adapter, a 440 Hz test tone and a four-note melody from a user program on the emulated HD Audio card (measured in QEMU's WAV output), the security audit and self-test, the config read and the whole IPC demo |

### System calls (`int 0x80`, number in `rax`, args in `rdi rsi rdx r10`)

`exit`, `write`, `yield`, `getpid`, `sleep_ms`, `cpu_id`, `uptime_ms`, `spawn`, `port_create`,
`port_publish`, `port_lookup`, `port_send`, `port_recv`, `handle_close`, `handle_dup`, `file_read`,
`audio_write`, `audio_queued`.
The numbers are in `kernel/src/syscall.rs` and `userland/src/lib.rs`.

### Still to do in Phase 1

1. Fast `syscall`/`sysret` entry, FPU/SSE state saving (user code is soft-float for now), and thread creation inside a process.
2. Load balancing between CPU run queues (threads are pinned to the CPU they start on), plus priorities and the game/real-time classes.
3. Futexes and event objects, and `wait()` for child processes.
4. An ACPICA port (the current table walker never touches AML), HPET/TSC-deadline timers, and x2APIC mode.
5. `dhi.idl` and a generator for `dhi.h` / `dhi.rs`; NVMe interrupts (MSI-X), one queue pair per CPU and writes; AHCI interrupts, NCQ and writes; USB Attached SCSI (UAS), xHCI MSI-X interrupts and hotplug; virtio-net, network interrupts, and sockets for user programs.
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
  src/bt/                 Bluetooth host: HCI bring-up, scanning, pairing (mod.rs),
                          L2CAP (l2cap.rs), SDP client and server (sdp.rs), HID
                          gamepads (hid.rs), RFCOMM (rfcomm.rs), hands-free audio
                          gateway (hfp.rs)
  src/sound.rs            sound cards (HD Audio): outputs, mixer, test tones
  src/modules.rs          boot modules: user programs and firmware
  src/sync.rs             IrqMutex (interrupt-safe spinlock)
  src/dhi.rs              Rust side of the Driver Host Interface
  src/{console,fb,serial}.rs    output: serial, framebuffer, desktop art
  src/shell.rs            kernel-mode command prompt (a kernel thread)
userland/                 user programs and libaero (Rust, no_std)
  src/lib.rs              system call wrappers, println!, handles
  src/bin/aerosmss.rs     session manager, first process
  src/bin/{echod,client,crasher}.rs   IPC demo service and clients, fault demo
  src/bin/melody.rs       plays four notes through the audio system calls
drivers/include/dhi.h     the DHI contract (C ABI)
drivers/ps2kbd/           PS/2 keyboard driver (C++)
drivers/nvme/             NVMe driver (C++)
drivers/ahci/             AHCI (SATA) driver (C++)
drivers/xhci/             xHCI (USB 3) driver (C++)
drivers/e1000/            Intel Ethernet driver, e1000/e1000e (C++)
drivers/igc/              Intel Ethernet driver, igb/igc: I210/I211, I225/I226 (C++)
drivers/btmtk/            MediaTek Bluetooth firmware loader, MT7921/MT7922 (C++)
drivers/hda/              Intel High Definition Audio driver (C++)
tools/boot-test.sh        headless QEMU boot test
tools/make-disk.sh        builds the NVMe test disk (GPT + FAT32) from tools/disk-files/
tools/make-sata-disk.sh   builds the SATA test disk (MBR + FAT32) from tools/sata-files/
tools/make-usb-disk.sh    builds the USB stick image (MBR + FAT32) from tools/usb-files/
tools/fetch-firmware.sh   downloads the pinned MediaTek Bluetooth firmware into build/firmware/
tools/fakebt/             simulated USB Bluetooth adapter, gamepad and headset for QEMU (usbredir, C)
tools/qemu-type.py        types at the guest's shell through the QEMU monitor (boot test)
tools/wav-tone.py         measures the test tone in QEMU's recorded sound output (boot test)
```

## Building and running

Requirements: Rust stable with the `x86_64-unknown-none` target, `clang++` and `llvm-ar`,
`xorriso`, `git`, `curl`, `gdisk`, `dosfstools`, `mtools`, `libusbredirparser-dev` (for the boot test), QEMU (`qemu-system-x86_64`) and OVMF UEFI firmware.

On Ubuntu/Debian:

```sh
sudo apt install clang llvm lld xorriso curl gdisk dosfstools mtools libusbredirparser-dev qemu-system-x86 ovmf
rustup target add x86_64-unknown-none
```

Then:

```sh
make                    # builds build/aeroforge.iso (fetches Limine and the Bluetooth firmware on first run)
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
