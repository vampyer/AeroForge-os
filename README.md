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

Since 0.27 the network thread sleeps until the network card interrupts. It used to run the
TCP/IP stack every 10 ms tick, so a frame could wait up to 10 ms, and the thread woke 100 times
a second with nothing to do. On igb and igc cards (the I225-V among them) the thread now points
the card's MSI-X vector at its own CPU, enables the receive, transmit and link interrupts, and
sleeps until one arrives or smoltcp's next timer is due (at most 100 ms). It reads the cause
register before draining the receive ring, so a frame arriving later interrupts again. igb keeps
its interrupt registers at the old e1000 offsets and igc moved them to 0x1500; the driver uses
the right ones for each. Ping times are now measured with the TSC: the gateway answers in about
0.2 ms in QEMU. The e1000 family stays polled. The boot test checks that the igb card is on
MSI-X and has raised interrupts.

Since 0.28 user threads move between CPUs. Until now a thread stayed on the CPU it was created
on, so one CPU could have two busy threads taking turns while another sat idle. Now a CPU that
is about to go idle takes a waiting user thread from the CPU with the longest ready queue (work
stealing, at the latest on its next timer tick). A thread is only taken once the CPU that last
ran it has finished saving its registers: `switch_context` clears the thread's `on_cpu` flag
right after it stores the stack pointer. Only one run queue lock is held at a time, so two CPUs
stealing from each other cannot deadlock. Kernel threads stay where they are, since the USB and
network threads aim their device's interrupts at their own CPU. The new `balancetest` program
runs six threads of floating point work on four CPUs and checks that some of them moved and
that every result matches; in QEMU it finishes in about 380 ms instead of 490 ms. `sched` shows
how many threads have moved.

Since 0.29 threads have priorities. Every run queue keeps one round-robin queue per level, and a
CPU always runs its highest-priority ready thread. Programs choose between `Low` (background
work, which only gets CPUs nobody else wants), `Normal` (the default) and `High` (a game's main
loop or its audio) with `aero::thread::set_priority`, system call 31; they may only change their
own threads, and nothing above `High`. Kernel threads sit on a fourth level above all three,
since they mostly sleep until a device needs them and should then run at once. A thread woken at
a higher level than the one running preempts it straight away: another CPU gets a reschedule
IPI as before, and a CPU that wakes a thread itself (say from a device interrupt while idle)
now sends one to itself instead of leaving the thread until the next tick. Stealing takes the
highest-priority thread it may move. `ps` shows each thread's level. The new `priotest` program
times the same work alone (200 ms in QEMU), in a normal thread among sixteen busy ones (about 1 s) and
in a high-priority one (200 ms), and fails if high priority does not win.

Since 0.30 sleeps are precise. The LAPIC timer used to fire every 10 ms and a sleep ended on the
next tick after it was due, so `sleep_ms(2)` could take anything up to 10 ms and a game had no way
to hold 144 Hz (6.9 ms frames). Now the timer is one-shot: each time the scheduler runs it arms the
timer for the next 10 ms time-slice tick or the earliest sleeping thread's wake-up time, whichever
comes first, and time comes from the TSC in microseconds. Programs get `aero::sleep_us` (system
call 32), `aero::clock_us` (33, microseconds since boot) and `futex::wait_timeout` (a timeout in
microseconds as the third argument of `futex_wait`, which then returns the new `E_TIMEDOUT`). A
thread woken by its timer takes the CPU straight away if it outranks the running thread (or the
CPU was idle), and otherwise at the next tick. The new `timertest` program checks that 2 ms sleeps
take 2 ms (about 2.15 ms in QEMU, where the old timer gave 10 ms), that a loop sleeping until each
144 Hz frame deadline keeps pace (72 frames in 500.1 ms against 499.97), and that futex waits time
out on time but still end at once when woken.

Since 0.31 busy CPUs balance too. Until now a CPU only took threads from others when it was about
to go idle, so four CPUs running one, three, one and three busy threads stayed that way, and the
threads on the crowded CPUs got a third as much time as the others. Now every fourth tick (40 ms)
each CPU also checks the others, and pulls one waiting user thread from the busiest if that CPU
has at least two more threads than itself (counting the running one); moving one thread then
never turns the busiest CPU into the lighter one, so nothing bounces back and forth. `sched` shows
how many moves balancing made. The new `spreadtest` program creates exactly that one, three, one,
three spread and checks, 40 times over 400 ms, that the threads are evenly placed; with the
balancing switched off it found them uneven in all 40 looks.

Since 0.32 idle CPUs are tickless. A CPU with nothing to run no longer takes the 10 ms tick: its
timer is armed only for its earliest sleeping thread, or stopped, and it stays halted until an
interrupt brings work, which saves power and heat. Because an idle CPU no longer wakes up to look
for work, whoever queues a thread for it sends it a reschedule IPI, and a busy CPU that has user
threads waiting nudges an idle one to come and take them (`sched` counts these). Two kernel threads
that woke every tick for nothing now sleep longer: the mixer waits for a program to write sound
(it still runs every tick while playing), and the USB thread, woken by interrupts anyway, only
checks on its own every 100 ms. The new `wakeups` command counts each CPU's timer interrupts over
a second; in QEMU an idle CPU takes 0 to 2 instead of 100. Since 0.32.1 the Bluetooth thread is
quiet too: it services adapters every tick only while something is under way (pairing, a
recording, voice audio, commands or data still to send) and otherwise waits for a USB interrupt or
a job from the shell, checking its deadlines every 100 ms. With gamepads and a headset paired, all
four CPUs together now take about 30 timer interrupts in an idle second instead of about 120. Still
waking every tick: the network thread when a card has no interrupts (the older e1000 family).

Since 0.33 programs can use the network. `aero::net::Socket::udp()` and `Socket::tcp()` give a
socket handle (system call 34); `connect` sets a UDP socket's peer or makes a TCP connection with a
timeout (35), `send` queues data (36) and `recv` waits for it, with a timeout (37); closing the
handle closes the socket. Every socket, the kernel's DHCP and ping ones included, lives in the one
smoltcp stack: a system call locks it to queue data or take what arrived and wakes the network
thread to send, and a call that has to wait sleeps until the network thread's next poll brings
something. TCP data goes through 64 KiB buffers each way; a closed TCP connection gets two seconds
to say goodbye before it is reset. Errors are `E_NOTFOUND` (no network card or no DHCP address
yet), the new `E_CLOSED` (refused or reset) and `E_TIMEDOUT`. `ifconfig` shows how many program
sockets are open. The new `nettest` program checks, against `tools/echo-server.py` on the host
running QEMU, a UDP echo (4 ms), a 50 ms receive timeout, 64 KiB echoed over TCP followed by the
server closing (23 ms), and a refused connection to a port nobody listens on.

Since 0.34 programs can look names up and run servers. `aero::net::info()` (system call 40) gives
the address, gateway and DNS server from DHCP, and `aero::net::resolve("example.com", timeout)` asks
that DNS server for an IPv4 address: the resolver lives in `libaero` and sends its queries over a UDP
socket, asking again every second until the timeout, follows CNAMEs, and reports names that do not
exist as `E_NOTFOUND`. `resolve_with` asks a given server. A TCP socket can `listen` on a port (38);
the kernel then keeps four sockets listening there, so up to four connections can wait, and `accept`
(39, with a timeout) hands one out as a new socket and puts a fresh listener in its place.
`nettest` now also resolves a name through a test DNS server on the host (2 ms), checks that a
missing name is reported, times out an `accept` nobody connects to, and serves a client on the
host that reaches it through QEMU's port forwarding.

Since 0.35 a thread can wait on several things at once. `aero::wait_any(&[...], timeout)` (system
call 44) takes up to 64 handles and returns the index of the first one that is ready: a port with a
message, a socket with data (or a connection to accept, or closed), a set event or an exited
process. Event objects are new (41 to 43): `Event::auto_reset()` lets one waiter through per `set`,
`Event::manual_reset()` stays set until `reset`. A program can also get a handle to a child it
started (45) and kill it with an exit code (46). Inside the kernel, everything a waiter could be
waiting for calls `wait::notify`, which wakes every thread in `wait_any` to check its handles again;
a waiter registers before it checks, so no wakeup is lost. The new `waittest` program checks events
set by another thread (6 ms for a 5 ms delay), a wait on a port, an event and a UDP socket that
returns the socket when the echo arrives, and a wait on a child that ends 0.2 ms after killing it.

Since 0.36 the screen goes through a display layer, the first step toward driving the Radeon. The
console no longer draws on the screen: it draws into an off-screen image in RAM, and the display
copies only the part that changed to the screen. On the firmware framebuffer that is a copy into
video memory; with no firmware framebuffer, a new C++ **virtio-gpu** driver (QEMU's paravirtual
GPU) shows the image through the GPU, and the console replays its boot history once the GPU is up.
A program can take the whole screen (`aero::display::acquire`, system call 47), present all or part
of its own frames (48) and give the screen back (49, also done when it exits); the console then
reappears with everything printed meanwhile. The new `drawtest` program draws four coloured
quarters and a partial update, and the tests check QEMU screenshots of its picture and of the
console afterwards, on the firmware framebuffer (`tools/boot-test.sh`) and on virtio-gpu
(`tools/display-test.sh`). The `display` shell command shows the screen size, what drives it and
who has it.

Since 0.37 AeroForge drives the display engine of AMD Radeon graphics with DCN 2.1 (the Vega
graphics in Renoir, Lucienne and Cezanne APUs, such as the Ryzen 9 5900HX). The firmware's screen
mode stays as it is. A new C++ driver (`drivers/amdgpu/dcn.cpp`, with register offsets from AMD's
MIT-licensed Linux headers) finds the display pipe the firmware shows its screen on, and moves the
pipe's scan-out address at the next vertical blank: a page flip. At boot the kernel checks that the
display engine's address of the screen matches the CPU's, places a second screen buffer after the
first in graphics memory and flips between the two eight times. Only if every flip lands at a
vertical blank do programs' full-screen frames go through the flips from then on: `display_present`
draws into the buffer that is not on screen and returns once the display engine has switched to it,
so frames never tear. Tested on a Minisforum UM590 (Vega 8, 4K screen at 30 Hz): 8 of 8 flips
landed at a vertical blank. The boot also prints each graphics device and the USB devices just before
the prompt, so a photo of the screen tells what was found on real hardware.

Since 0.38 there is a first desktop, in the style of Windows 7 with artwork of our own: `run desktop`
at the prompt. The `desktop` program takes the screen and draws, in software, a blue wallpaper, a
dark glass taskbar (Start orb, window buttons, clock and date, a "show desktop" strip) and three
glass-framed windows (Welcome, System and Notes). Click a window to bring it to the front, drag it by
its title bar, and use its minimize, maximize and close buttons; taskbar buttons switch windows and
the Start menu reopens closed ones. Keys typed while Notes is in front go into it. Start > Exit to
console, or Esc, gives the screen back. A `time` system call (52) gives programs the PC's clock.
It only redraws and presents what changed. Two new system calls give the program that owns the
screen the mouse pointer (`pointer`, 50: position, buttons and a count of left-button presses, so a
quick click between two reads is not lost) and the typed keys (`keys_read`, 51); while a program
owns the screen, keys go to it instead of the shell. The boot test drags the Notes window with
QEMU's mouse, types into it and checks a screenshot.

Since 0.39 the desktop has icons down the left (Computer, Notes, System; click to pick one,
double-click to open it) and a Computer window that browses the disks: folders first, then files
with their sizes, a Back button (or Backspace) to go up, page buttons for long folders and a status
bar. Double-clicking a folder goes into it; double-clicking a text file opens it in Notes. The root
folder also shows the other disks (`sata0p1`, `usb0p1` and so on) as folders. Double-clicking a
title bar maximizes the window. A new system call, `dir_list` (53), gives programs a folder's
entries (`aero::list_dir`). The boot test opens Computer from its icon, goes into `/docs` and opens
the welcome file in Notes.

Since 0.40 windows change size when dragged by an edge or a corner, with a double-headed arrow
pointer over the edges; the boot test makes the Computer window narrower and taller by its corner.

Since 0.41 windows snap like Windows 7's Aero Snap: drag one by its title bar against the top of the
screen to fill it, or against the left or right side to fill that half (a glass outline shows where
first). Dragging a snapped or maximized window away gives back its old size. The boot test snaps
Welcome to the left half, checks a screenshot, and drags it back.

Since 0.42 the Start button is a red, white and blue circle with a black star, and the desktop's
text is Noto Sans Mono (SIL Open Font License, pre-rendered with smooth edges by the
`noto-sans-mono-bitmap` crate), 20 pixels tall instead of the old 8 x 8 bitmap font, so it is clearer
and bigger. Since 0.43 there is a Calculator (desktop icon and Start menu): click its keys or type
digits, `+ - * / .`, `=` or Enter, Backspace and C; the number pad works too. The boot test types
`12+30*2`, clicks `=` and checks for 84.
Since 0.44 Notes has a Save button (Ctrl+S works too) that writes the text back to the file it
opened from Computer, or to `/Notes.txt` for a new note, then reads it back to check it. Ctrl with a
letter now reaches programs as its control code (Ctrl+S is 0x13) from both PS/2 and USB keyboards.
The boot test saves the opened welcome file with Ctrl+S and checks the NVMe disk image with mtools.
Since 0.45 desktop text is bigger (24 pixel Noto Sans Mono instead of 20, the clock 20 instead of 16)
and drawn with sharper edges, and the windows, icons and Start menu are sized to fit it.
Since 0.46 moving the pointer redraws only the pointer and the one button, row, key or menu item it
lights up, each as its own small rectangle, instead of one rectangle joining them (which could span
whole windows). Under QEMU without KVM that took a pointer move from about 100 ms to about 2 ms; the
desktop logs the average at exit and the boot test wants it under 30 ms.
Since 0.47 the pointer has a speed, 1 to 10 (system call 54, `mouse_speed`). Speed 5 moves one pixel
per mouse count; the default is 7 (1.5 pixels per count). Above 5, quick movements go half as far
again, so the pointer crosses the screen fast and still lands precisely. The Start menu has Mouse
speed - and + buttons, and the desktop saves the choice as `mouse speed = N` in `/AeroForge.ini` and
applies it when it starts. The shell has `mouse speed <1-10>`; the boot test uses speed 5 so its
pointer moves are exact, then raises it to 6 from the Start menu and checks the saved file and how far
a quick move goes.

| Area | Status |
|---|---|
| Boot | UEFI only, Limine 9.x, higher-half kernel at `0xffffffff80000000`, user programs loaded as boot modules |
| Consoles and display | COM1 serial log + console drawn off-screen on a procedural Aero-style desktop and presented to the firmware framebuffer or a virtio-gpu; programs can take the whole screen |
| Memory | Binary **buddy** page allocator (4 KiB to 4 MiB blocks, coalescing), **slab** heap (16 B to 2 KiB size classes, larger requests straight from the buddy), per-process address spaces sharing the kernel half |
| CPU tables | Per-CPU GDT and TSS (`rsp0` updated on every switch), double-fault IST stack, per-CPU data through GS with `swapgs` on ring transitions |
| Interrupts | 256-vector IDT generated at build time, exceptions from ring 3 kill only the faulting process |
| Interrupt controllers | Local APIC (per-CPU periodic timer, calibrated against the PIT) and I/O APIC with MADT overrides; the 8259 PIC is masked; MSI-X and MSI for PCIe devices (the USB, NVMe and SATA controllers and igb/igc network cards so far) |
| Scheduler | Preemptive round robin with one run queue per CPU, four priority levels (low, normal, high for programs; kernel threads above them) with preemption on wakeup, work stealing (idle CPUs take waiting user threads from busy ones) and periodic balancing between busy CPUs, idle thread per CPU, one-shot timer with microsecond sleeps and futex timeouts, tickless idle, block/wake, reschedule IPIs for cross-core wakeups (sub-millisecond IPC round trips) |
| Processes | ELF64 loader, ring 3, threads (on any CPU), memory mapping with TLB shootdowns, futexes, `exit` that ends every thread, waiting for child exit codes, `syscall`/`sysret` entry (the `int 0x80` gate still works), x87/SSE/AVX/AVX-512 registers saved per thread with XSAVE, exit and cleanup of address space and kernel stack |
| Objects and IPC | Handles with rights (capabilities), event objects, process handles, waiting on up to 64 handles at once, IPC ports with 256-byte messages, handle transfer in messages, a name service (`publish` / `lookup`, lookups only grant send rights) |
| PCIe | Enumeration through ECAM (ACPI MCFG), 64-bit BARs, bus mastering |
| C++ drivers | Behind `drivers/include/dhi.h` (ABI v3: logging, port I/O, DMA buffers, MMIO mapping, delays, waiting for an interrupt): PS/2 keyboard, an **NVMe** driver (admin + I/O queue pair, MSI-X completion interrupts, Identify, reads and writes up to 8 KiB per command, flush), an **AHCI** (SATA) driver (one command slot per port, MSI completion interrupts, IDENTIFY DEVICE, LBA48 READ/WRITE DMA EXT, FLUSH CACHE EXT) and an **xHCI** (USB 3) driver (command and event rings, device enumeration through hubs (nested up to the USB limit, transaction translators for slow devices behind fast hubs), HID boot keyboard and mouse, bulk-only mass storage with SCSI reads, writes and cache sync, Bluetooth HCI transport with isochronous voice endpoints, polled from a kernel thread) and an **Intel Ethernet** driver (e1000/e1000e: one receive and one transmit ring of legacy descriptors, MAC from the receive-address registers or EEPROM, link and speed) and an **igb/igc** driver (I210/I211/I350/82576 and I225/I226 at up to 2.5 Gb/s: advanced descriptors, PHY power-up and auto-negotiation over MDIO, I225 EEE workaround) and a **MediaTek Bluetooth** set-up driver (MT7921/MT7922 firmware download over the WMT vendor protocol) and an **HD Audio** driver (CORB/RIRB, codec widget graph, output routing, one 48 kHz stereo output stream) and an **AMD display engine** driver (DCN 2.1: the firmware's display pipe, page flips at vertical blank) and a **virtio-gpu** driver (control queue, 2D resources backed by scattered pages, scanout, transfer and flush of changed rectangles) |
| Networking | smoltcp (IPv4, ARP, ICMP, UDP, TCP) on the first Intel NIC (e1000, e1000e, igb or igc), run by the `net` kernel thread (woken by MSI-X on igb/igc, polled on e1000); DHCP client; ICMP echo; UDP and TCP sockets for programs, TCP servers (listen and accept) and DNS lookups |
| Storage | Block device layer, GPT and MBR partitions, read-write **FAT32** with long file names and **exFAT**, read-only **NTFS**, all with case-insensitive lookup, first volume at `/` and the others at `/<device>`; `file_read`, `file_write`, `file_delete` and `dir_create` system calls; CMOS real-time clock for file times |
| Security | NX, SMEP, SMAP, UMIP and CR0.WP on every CPU that has them; W^X kernel image (code read-only, data, heap, stacks and the direct map non-executable); guard pages under every kernel stack (overflow is reported, not silent); random stack canary (RDRAND) checked by the C++ drivers; every system call copies user memory through checked `copy_from_user` / `copy_to_user` (mapped, user-owned, writable for writes); user code W^X, stacks non-executable; a boot audit re-checks all of it |
| Userland | `libaero` system call library, `aerosmss` (reads its session from disk), `echod`, `client`, `crasher`, `sectest` / `nxtest` / `rotest` (security self-test), `melody` (plays sound), `padtest` (reads the gamepads), `savetest` (saves files), `fputest` (floating point and vector registers), `threadtest` (threads, heap, locks), `balancetest` (threads moving between CPUs), `priotest` (thread priorities), `timertest` (precise sleeps and timeouts), `spreadtest` (balancing between busy CPUs), `nettest` (UDP and TCP sockets, DNS, a TCP server), `waittest` (waiting on several handles), `drawtest` (drawing on the whole screen) and `sleeper` (Rust, `no_std` with `alloc`, hardware floating point) |
| Shell | `ps`, `sched`, `run <prog>`, `ports`, `lspci`, `display`, `lsusb`, `mouse`, `ifconfig`, `ping <ip>`, `bt`, `bt scan`, `bt pair`, `gamepad`, `mic`, `mic record`, `sound`, `sound test`, `sound use`, `disks`, `ls`, `cat`, `wc`, `mem`, `irq`, `cpu`, `acpi`, `uptime`, `int3`, `panic` |
| Test | `tools/boot-test.sh` boots headless with an NVMe (GPT) and a SATA (MBR) disk image and a USB stick, and checks every CPU, all three mounts, the USB keyboard and mouse behind a hub, igb and e1000e network cards, a DHCP lease and a ping to the gateway over the igb card, UDP and TCP sockets from a program against an echo server on the host, DNS lookups and a TCP server reached from the host, the MediaTek firmware download, a Bluetooth scan, classic and LE gamepad pairing, input and reconnection and headset pairing, microphone recording and reconnection against the simulated adapter, a 440 Hz test tone and a four-note melody from a user program on the emulated HD Audio card (measured in QEMU's WAV output), disk writes on all three disks and saving, overwriting and deleting files on their FAT32 volumes (checked with fsck.fat and mtools afterwards), an exFAT stick plugged in while running, read, written and checked with fsck.exfat, an NTFS drive's folders and fragmented and sparse files, five programs keeping their floating point and vector registers apart, threads sharing a lock and a heap and ending with their program, a program drawing on the whole screen (checked in QEMU screenshots), interrupts from the USB, NVMe and SATA controllers and the igb card, the security audit and self-test, the config read and the whole IPC demo |

### System calls (`int 0x80`, number in `rax`, args in `rdi rsi rdx r10`)

`exit`, `write`, `yield`, `getpid`, `sleep_ms`, `cpu_id`, `uptime_ms`, `spawn`, `port_create`,
`port_publish`, `port_lookup`, `port_send`, `port_recv`, `handle_close`, `handle_dup`, `file_read`,
`audio_write`, `audio_queued`, `gamepad_read`, `file_write`, `file_delete`, `dir_create`, `mem_map`,
`mem_unmap`, `thread_create`, `thread_exit`, `thread_join`, `futex_wait`, `futex_wake`, `process_wait`,
`thread_id`, `thread_priority`, `sleep_us`, `clock_us`, `socket_open`, `socket_connect`, `socket_send`,
`socket_recv`, `socket_listen`, `socket_accept`, `net_info`, `event_create`, `event_set`, `event_reset`,
`wait_any`, `process_handle`, `process_kill`, `display_acquire`, `display_present`, `display_release`, `pointer`, `keys_read`, `time`, `dir_list`, `mouse_speed`.
The numbers are in `kernel/src/syscall.rs` and `userland/src/lib.rs`.

### Still to do in Phase 1

1. Saving the vector registers lazily or with XSAVES to make switches cheaper; reusing freed address ranges.
2. A real-time class with deadlines and protection against a high-priority thread starving the rest; balancing that keeps cache and NUMA locality in mind (preferring a CPU that shares the thread's L3 cache, which on Ryzen means the same CCX).
3. Handles to threads, and waiting on them with `wait_any`.
4. An ACPICA port (the current table walker never touches AML), TSC-deadline timer mode, deeper CPU sleep states (MWAIT C-states from the ACPI tables), and x2APIC mode.
5. `dhi.idl` and a generator for `dhi.h` / `dhi.rs`; one NVMe queue pair per CPU; AHCI NCQ; USB Attached SCSI (UAS) and xHCI hotplug events; virtio-net, e1000 interrupts, waiting on several sockets at once, and IPv6.
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
driver, also behind USB hubs (including hubs built into monitors). A keyboard or mouse whose
endpoint halts on a transfer error (more likely through several hubs) is reset and keeps
working; since 0.37.1 this lets a Logitech Unifying wireless receiver behind three hubs type on
a Minisforum UM590. Booting and the IPC demo
don't need a keyboard.
