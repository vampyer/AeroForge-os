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
to be paired again after a restart. Programs cannot read the gamepad yet. The simulated adapter also plays a classic gamepad that
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

Since 0.13 Bluetooth LE gamepads work too (`kernel/src/bt/le.rs`). `bt pair <address>` on a pad
the scan saw over LE connects over LE, pairs with the Security Manager Protocol (LE legacy
pairing, "Just Works": the confirm and short-term key functions run on a small AES-128 in
`crypto.rs`, checked against the specification's test values), encrypts the link and receives
the pad's long-term key. HID over GATT then finds the HID service, reads the report map (with
long reads) and each report characteristic's Report Reference, and turns on the input reports'
notifications, which feed the same report parser as classic pads. A paired LE pad reconnects
when it advertises: the adapter waits for any paired LE device through its accept list and
encrypts with the stored key. Adapters with separate LE data buffers (like the MT7921) get their
own flow control. Pads that insist on LE Secure Connections or a passkey are refused for now
with a message saying so. The simulated adapter's LE pad checks the pairing values with its own
AES, guards its HID attributes behind encryption, and switches off and on, which the boot test
drives like the classic one.

Since 0.14 USB gamepads work as well, plugged in before start-up. The xHCI driver sets up Xbox
360 style pads (interface class FF/5D/01, which most wired pads and 2.4 GHz dongles present),
lights their player 1 LED and turns each input report into a small HID report; any other HID
interface whose report descriptor has a joystick or gamepad collection is read as it is. Both
reach the kernel with a report descriptor (`aero_xhci_pad`, `aero_xhci_pad_report`), so the
same parser as for Bluetooth pads decodes them, and `gamepad` lists them next to the Bluetooth
ones. The boot test attaches two simulated pads (`tools/fakepad`), an Xbox style one and a HID
one behind a hub next to a volume-key interface that must be left alone. Xbox One (GIP) pads,
the Xbox 360 wireless receiver's protocol, and plugging in after start-up come later.

Since 0.15 user programs can read the gamepads: system call 18 (`gamepad::read` in libaero)
gives a pad's name, link and raw buttons, hat and axes, and the same pad in the Xbox 360 layout
games expect (XInput button bits, sticks from -32767 to 32767 with up positive, triggers from 0
to 255). For Xbox style USB pads that layout is exact; for other pads it is a guess (buttons 1 to
11 as A B X Y LB RB Back Start LS RS Guide, the hat as the d-pad) until a controller mapping
database like SDL's arrives, and `exact` tells programs which. The `padtest` program prints every
pad that way, and the boot test checks its output for all four simulated pads.

Since 0.16 USB devices can be plugged in and unplugged while AeroForge runs. The xHCI driver
acts on the controller's port status change events for root ports and asks each hub's ports for
changes a few times a second; a new device is set up as at start-up (keyboards, mice, gamepads,
Bluetooth adapters, disks), and an unplugged one (with everything behind it, for a hub) gives
back its slot and memory. The kernel notices through a change counter (`aero_xhci_generation`),
refreshes `lsusb`, and marks unplugged gamepads as not connected; a pad plugged back into the
same port takes its old place in the list. The simulated pads unplug and replug themselves in
the boot test, one on a root port and one behind the hub. An adapter's Bluetooth host does not
restart after a replug yet.

Since 0.17 a USB stick or drive plugged in while AeroForge runs is registered (the first free
`usbN` name), its partitions are read and its FAT32 volumes mounted at `/usbNp1` and so on;
pulling it out unmounts them and removes the disk. The mount list can change at run time now
(lookups hold a reference to their volume, so a read in progress finishes safely). The boot test
plugs a second stick into the hub through the QEMU monitor, reads a file from it and pulls it out
again (`tools/qemu-monitor.py`).

Since 0.18 the disk drivers can write. NVMe uses the Write and Flush commands, SATA uses
WRITE DMA EXT and FLUSH CACHE EXT, and USB disks use SCSI WRITE(10)/WRITE(16) and SYNCHRONIZE
CACHE. The block layer's `write` and `flush` cover whole disks and partitions; a partition
refuses writes past its end. The shell's `diskwrite <disk> <lba> <blocks>` command writes a
test pattern, flushes it and reads it back. It only writes the unused gap between the
partition tables and the first partition. The boot test writes 80 blocks to the NVMe, SATA
and USB disks, enough to take several commands on each driver, and checks that the disk
images hold the pattern after QEMU exits (`tools/check-disk-write.py`).

Since 0.19 files can be saved. FAT32 volumes are writable: files can be created, overwritten
and deleted, and directories created and removed, with long names (and Windows-style `NAME~1.EXT`
aliases), the FSInfo free count kept right, and times from the PC's real-time clock. Updates go
data first, then the FAT, then the directory entry, and end with a cache flush. Programs get
`file_write` (create or replace a whole file, up to 8 MiB), `file_delete` and `dir_create`;
`/system` is off limits to them until files have owners and permissions. The shell has
`write <path> <text>` (quote a path with spaces), `mkdir` and `rm`. The `savetest` program saves
twelve slots with long names in `/saves` (enough to grow the folder), reads them back, overwrites
one and deletes ten. The boot test also saves and deletes files on the SATA and USB disks, then
checks every volume with `fsck.fat` and reads the files back with mtools after QEMU exits
(`tools/check-fat-files.py`).

Since 0.20 exFAT volumes work too, read and write: USB sticks and SD cards over 32 GB come
formatted exFAT. The driver follows Microsoft's exFAT specification: it keeps the allocation
bitmap in memory, reads both one-piece ("no FAT chain") files, as Windows writes them, and
FAT-chained ones, compares names through the volume's up-case table, and writes entry-set
checksums and name hashes so Windows can find what it saved. A folder that outgrows its
cluster gets more, switching to a FAT chain when the new cluster is not next door. Both
filesystems sit behind one `vfs::Volume` interface, so the system calls and shell commands
work the same on either. The boot test's hot-plugged stick is now exFAT (built with
`mkfs.exfat` and `tools/exfat.py`, a separate small exFAT writer and reader). It reads a
many-cluster file, saves five checkpoints into a folder (enough to grow it), overwrites and
deletes a file and creates a folder. After QEMU exits, `fsck.exfat` must find the stick clean
and `tools/exfat.py` must read back what was saved.

Since 0.21 NTFS drives, the ones Windows lives on, can be read. NTFS volumes are mounted
read-only at `/<device>` (never at `/`). The driver reads the Master File Table, including
records split across several MFT entries by an attribute list, walks folder index B-trees,
follows the cluster runs of fragmented files, and reads holes in sparse files as zeros.
Lookups use the volume's own `$UpCase` table. Metadata files and `$Recycle.Bin` are hidden
from the root listing, as in Explorer. Compressed and encrypted files are refused, and so are
writes: NTFS keeps a journal that a writer has to honour, which is a project of its own. A
BitLocker-encrypted volume gets a warning to unlock it in Windows. The test drive is checked
in as `tools/ntfs-test.part.xz` (about 110 KiB), because folders and fragmented, sparse and compressed
files can only be put on NTFS by a real NTFS driver: `tools/make-ntfs-image.sh` rebuilds it with
ntfs-3g, as root. The boot test attaches it as a second SATA drive. It lists a folder of 300
photos (deep enough for an index B-tree), reads a file three folders down, and checks the
fragmented and sparse files against checksums from ntfs-3g. It also checks that a compressed
file and a write are refused.

Since 0.22 programs have hardware floating point and a fast way into the kernel. Every CPU
turns on x87, SSE and AVX (and AVX-512 where the CPU has it) for ring 3, and the scheduler
saves the outgoing program's registers and loads the incoming one's on every switch (XSAVE and
XRSTOR, or FXSAVE on CPUs without XSAVE). New programs start from a clean register state, so
nothing leaks between them. The kernel itself never uses these registers, so kernel threads
cost nothing extra. Programs are now built for Rust's `x86_64-unknown-linux-gnu` target, the
stable way to get hardware floating point (only the instruction set and calling convention
are borrowed, not Linux itself), so `f32`/`f64` code such as `melody`'s sine waves runs on the
FPU instead of in software. System calls go through the `syscall` instruction, with `sysret`
on the way back; `int 0x80` still works. The `fputest` program starts four copies of itself;
all five fill the x87, SSE and AVX registers and MXCSR with their own values and are switched
against each other 30 times, then report over IPC. It also sums the square roots of 1 to
100,000 and times `syscall` against `int 0x80` (1.7 to 3 times faster in QEMU runs so far). With the
register restore left out, the test fails.

Since 0.23 programs can have threads, a heap and locks. `mem_map` hands a program zeroed pages
(each region with an unmapped guard page after it) and `mem_unmap` takes them back; when other
threads of the program may be running on other CPUs, the kernel first makes every CPU drop
its cached translations (a TLB shootdown IPI) so freed memory cannot be reached through stale
entries. `thread_create` starts another thread in the same address space, on any CPU, and
`thread_join` waits for one. `futex_wait` and `futex_wake` let programs build locks that sleep
in the kernel only when there is a wait. `exit` now ends every thread of a program: blocked
and sleeping threads are woken to exit, and threads running in ring 3 stop at their next
interrupt. A program that faults is ended the same way. `process_wait` returns a child
program's exit code. libaero builds on these: a heap (so programs can use `Box`, `Vec` and
`String`), `sync::Mutex`, `thread::spawn` with `join`, and `process::wait`. The
`threadtest` program has four threads, usually on four CPUs, add to one counter under the
mutex. Two threads pass a token back and forth through a futex, and an 8 MiB buffer is freed
while other threads run. It waits for `nxtest` to be killed and checks its exit code, then
exits with three threads still running (spinning, asleep for an hour, blocked forever), and
the boot test checks with `ps` that they are gone.

Since 0.24 devices can interrupt the kernel through message-signalled interrupts. The kernel
walks a PCIe device's capability list and programs MSI-X (its vector table lives in a BAR) or,
failing that, MSI. Each device gets its own vector, aimed at the CPU that services it. The USB
controller is the first user: it used to be polled once per 10 ms timer tick, so a key press or
gamepad report could wait up to 10 ms before the kernel saw it. Now the xHCI driver turns on its
event interrupt, and the interrupt wakes the USB thread (and the Bluetooth thread, whose HCI
events come through the same controller) at once. The tick stays as a backstop. A new
`sched::Event` lets an interrupt handler wake a sleeping kernel thread without missing a signal
that arrives just before it sleeps. The `irq` command lists each device vector, its kind, its
CPU and how often it fired. The boot test checks that the USB controller is on MSI-X and has
raised interrupts.

Since 0.25 NVMe I/O waits for the completion interrupt instead of spinning. Before, a thread
reading or writing the NVMe disk polled the completion queue in a busy loop, holding a spinlock
with interrupts off for the whole command. Now the disk's bounce buffer sits behind a sleeping
lock (a second thread waiting for the same disk is queued and sleeps), the driver polls for
20 µs to catch fast commands, then sleeps until the controller's MSI-X interrupt arrives. The
driver gets its interrupt source through DHI ABI 3: `aero_nvme_init` takes the source and a new
`irq_wait` operation sleeps on it. The kernel sets up MSI-X before the driver creates its I/O
queue, because controllers (QEMU's included) only deliver on vectors that were enabled when the
queue was created. During early boot, before the scheduler runs, the driver still polls. The
boot test checks that the NVMe controller is on MSI-X and has raised interrupts.

Since 0.26 SATA disks do the same. The AHCI controller has a single MSI for all its ports, so
the kernel reserves a group of interrupt sources for it, one per disk, and the controller's
interrupt signals the whole group: each disk's thread sleeps on its own source, and one woken by
another port's completion just checks its command again. The driver enables the completion and
error interrupts on each port and clears the port's status and its bit in the controller's
summary after every command, so the next one can interrupt again. `aero_ahci_init` takes the
first source. The boot test checks that the AHCI controller is on MSI and has raised interrupts.

| Area | Status |
|---|---|
| Boot | UEFI only, Limine 9.x, higher-half kernel at `0xffffffff80000000`, user programs loaded as boot modules |
| Consoles | COM1 serial log + framebuffer console drawn on a procedural Aero-style desktop |
| Memory | Binary **buddy** page allocator (4 KiB to 4 MiB blocks, coalescing), **slab** heap (16 B to 2 KiB size classes, larger requests straight from the buddy), per-process address spaces sharing the kernel half |
| CPU tables | Per-CPU GDT and TSS (`rsp0` updated on every switch), double-fault IST stack, per-CPU data through GS with `swapgs` on ring transitions |
| Interrupts | 256-vector IDT generated at build time, exceptions from ring 3 kill only the faulting process |
| Interrupt controllers | Local APIC (per-CPU periodic timer, calibrated against the PIT) and I/O APIC with MADT overrides; the 8259 PIC is masked; MSI-X and MSI for PCIe devices (the USB, NVMe and SATA controllers so far) |
| Scheduler | Preemptive round robin with one run queue per CPU, idle thread per CPU, sleep, block/wake, reschedule IPIs for cross-core wakeups (sub-millisecond IPC round trips) |
| Processes | ELF64 loader, ring 3, threads (on any CPU), memory mapping with TLB shootdowns, futexes, `exit` that ends every thread, waiting for child exit codes, `syscall`/`sysret` entry (the `int 0x80` gate still works), x87/SSE/AVX/AVX-512 registers saved per thread with XSAVE, exit and cleanup of address space and kernel stack |
| Objects and IPC | Handles with rights (capabilities), IPC ports with 256-byte messages, handle transfer in messages, a name service (`publish` / `lookup`, lookups only grant send rights) |
| PCIe | Enumeration through ECAM (ACPI MCFG), 64-bit BARs, bus mastering |
| C++ drivers | Behind `drivers/include/dhi.h` (ABI v2: logging, port I/O, DMA buffers, MMIO mapping, delays): PS/2 keyboard, an **NVMe** driver (admin + I/O queue pair, polling, Identify, reads and writes up to 8 KiB per command, flush), an **AHCI** (SATA) driver (one command slot per port, polling, IDENTIFY DEVICE, LBA48 READ/WRITE DMA EXT, FLUSH CACHE EXT) and an **xHCI** (USB 3) driver (command and event rings, device enumeration through hubs (nested up to the USB limit, transaction translators for slow devices behind fast hubs), HID boot keyboard and mouse, bulk-only mass storage with SCSI reads, writes and cache sync, Bluetooth HCI transport with isochronous voice endpoints, polled from a kernel thread) and an **Intel Ethernet** driver (e1000/e1000e: one receive and one transmit ring of legacy descriptors, MAC from the receive-address registers or EEPROM, link and speed) and an **igb/igc** driver (I210/I211/I350/82576 and I225/I226 at up to 2.5 Gb/s: advanced descriptors, PHY power-up and auto-negotiation over MDIO, I225 EEE workaround) and a **MediaTek Bluetooth** set-up driver (MT7921/MT7922 firmware download over the WMT vendor protocol) and an **HD Audio** driver (CORB/RIRB, codec widget graph, output routing, one 48 kHz stereo output stream) |
| Networking | smoltcp (IPv4, ARP, ICMP, UDP, TCP) on the first Intel NIC (e1000, e1000e, igb or igc), polled by the `net` kernel thread; DHCP client; ICMP echo |
| Storage | Block device layer, GPT and MBR partitions, read-write **FAT32** with long file names and **exFAT**, read-only **NTFS**, all with case-insensitive lookup, first volume at `/` and the others at `/<device>`; `file_read`, `file_write`, `file_delete` and `dir_create` system calls; CMOS real-time clock for file times |
| Security | NX, SMEP, SMAP, UMIP and CR0.WP on every CPU that has them; W^X kernel image (code read-only, data, heap, stacks and the direct map non-executable); guard pages under every kernel stack (overflow is reported, not silent); random stack canary (RDRAND) checked by the C++ drivers; every system call copies user memory through checked `copy_from_user` / `copy_to_user` (mapped, user-owned, writable for writes); user code W^X, stacks non-executable; a boot audit re-checks all of it |
| Userland | `libaero` system call library, `aerosmss` (reads its session from disk), `echod`, `client`, `crasher`, `sectest` / `nxtest` / `rotest` (security self-test), `melody` (plays sound), `padtest` (reads the gamepads), `savetest` (saves files), `fputest` (floating point and vector registers), `threadtest` (threads, heap, locks) (Rust, `no_std` with `alloc`, hardware floating point) |
| Shell | `ps`, `sched`, `run <prog>`, `ports`, `lspci`, `lsusb`, `mouse`, `ifconfig`, `ping <ip>`, `bt`, `bt scan`, `bt pair`, `gamepad`, `mic`, `mic record`, `sound`, `sound test`, `sound use`, `disks`, `ls`, `cat`, `wc`, `mem`, `irq`, `cpu`, `acpi`, `uptime`, `int3`, `panic` |
| Test | `tools/boot-test.sh` boots headless with an NVMe (GPT) and a SATA (MBR) disk image and a USB stick, and checks every CPU, all three mounts, the USB keyboard and mouse behind a hub, igb and e1000e network cards, a DHCP lease and a ping to the gateway over the igb card, the MediaTek firmware download, a Bluetooth scan, classic and LE gamepad pairing, input and reconnection and headset pairing, microphone recording and reconnection against the simulated adapter, a 440 Hz test tone and a four-note melody from a user program on the emulated HD Audio card (measured in QEMU's WAV output), disk writes on all three disks and saving, overwriting and deleting files on their FAT32 volumes (checked with fsck.fat and mtools afterwards), an exFAT stick plugged in while running, read, written and checked with fsck.exfat, an NTFS drive's folders and fragmented and sparse files, five programs keeping their floating point and vector registers apart, threads sharing a lock and a heap and ending with their program, interrupts from the USB, NVMe and SATA controllers, the security audit and self-test, the config read and the whole IPC demo |

### System calls (`int 0x80`, number in `rax`, args in `rdi rsi rdx r10`)

`exit`, `write`, `yield`, `getpid`, `sleep_ms`, `cpu_id`, `uptime_ms`, `spawn`, `port_create`,
`port_publish`, `port_lookup`, `port_send`, `port_recv`, `handle_close`, `handle_dup`, `file_read`,
`audio_write`, `audio_queued`, `gamepad_read`, `file_write`, `file_delete`, `dir_create`, `mem_map`,
`mem_unmap`, `thread_create`, `thread_exit`, `thread_join`, `futex_wait`, `futex_wake`, `process_wait`,
`thread_id`.
The numbers are in `kernel/src/syscall.rs` and `userland/src/lib.rs`.

### Still to do in Phase 1

1. Saving the vector registers lazily or with XSAVES to make switches cheaper; timeouts for `futex_wait`; reusing freed address ranges.
2. Load balancing between CPU run queues (threads are pinned to the CPU they start on), plus priorities and the game/real-time classes.
3. Event objects, and handles to threads and processes (killing a program from another one).
4. An ACPICA port (the current table walker never touches AML), HPET/TSC-deadline timers, and x2APIC mode.
5. `dhi.idl` and a generator for `dhi.h` / `dhi.rs`; one NVMe queue pair per CPU; AHCI NCQ; USB Attached SCSI (UAS) and xHCI hotplug events; virtio-net, network interrupts, and sockets for user programs.
6. Filesystems move to user-space servers behind IPC, as the design says; NTFS writes and compressed NTFS files.
7. KASLR (needs a position-independent kernel build), Rust stack canaries once they reach stable Rust, and targeted TLB shootdowns (only the CPUs running the program, one page at a time).

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
  src/{block,fat,exfat,ntfs,vfs}.rs  block devices and partitions, FAT32, exFAT, NTFS, the mount table
  src/bt/                 Bluetooth host: HCI bring-up, scanning, pairing (mod.rs),
                          L2CAP (l2cap.rs), SDP client and server (sdp.rs), HID
                          gamepads (hid.rs), RFCOMM (rfcomm.rs), hands-free audio
                          gateway (hfp.rs), LE pairing and HID over GATT (le.rs,
                          crypto.rs)
  src/sound.rs            sound cards (HD Audio): outputs, mixer, test tones
  src/modules.rs          boot modules: user programs and firmware
  src/sync.rs             IrqMutex (interrupt-safe spinlock), SleepMutex (waiters sleep)
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
tools/make-usb-disk.sh    builds the USB stick images (MBR + FAT32 or exFAT) from tools/usb-files/ and tools/usb2-files/
tools/exfat.py            puts files on an exFAT image and reads them back (boot test)
tools/make-ntfs-disk.sh   builds the NTFS test drive from tools/ntfs-test.part.xz
tools/make-ntfs-image.sh  rebuilds tools/ntfs-test.part.xz with ntfs-3g (needs root)
tools/fetch-firmware.sh   downloads the pinned MediaTek Bluetooth firmware into build/firmware/
tools/fakebt/             simulated USB Bluetooth adapter, gamepads and headset for QEMU (usbredir, C)
tools/qemu-type.py        types at the guest's shell through the QEMU monitor (boot test)
tools/wav-tone.py         measures the test tone in QEMU's recorded sound output (boot test)
tools/check-disk-write.py checks the diskwrite test pattern in a disk image (boot test)
tools/check-fat-files.py  fsck.fat / fsck.exfat on every test volume and the saved files read back (boot test)
```

## Building and running

Requirements: Rust stable with the `x86_64-unknown-none` target (kernel) and the `x86_64-unknown-linux-gnu` target (programs; already there on an x86-64 Linux host), a C compiler driver (`cc`) to link the programs, `clang++` and `llvm-ar`,
`xorriso`, `git`, `curl`, `gdisk`, `dosfstools`, `exfatprogs`, `mtools`, `libusbredirparser-dev` (for the boot test), QEMU (`qemu-system-x86_64`) and OVMF UEFI firmware.

On Ubuntu/Debian:

```sh
sudo apt install clang llvm lld xorriso curl gdisk dosfstools exfatprogs mtools libusbredirparser-dev qemu-system-x86 ovmf
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
