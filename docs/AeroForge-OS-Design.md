# AeroForge OS: Architecture, Specifications and Roadmap

Version 0.1 (design draft) · 2026-10-04 · Requested by Eddie

---

## 0. Read this first: a reality check

AeroForge is buildable, but not every goal in the brief is equally buildable. Projects that tried pieces of this (ReactOS, Haiku, SerenityOS, Redox, Fuchsia) show where the real cost sits. Being honest about this up front is what keeps the project alive past year one.

| Goal | Feasibility for a from-scratch kernel | Realistic path |
|---|---|---|
| Custom Rust/C++ hybrid kernel on x86-64 | **Feasible.** Hobby-to-serious kernels exist (Redox, SerenityOS, Theseus). | Build it. This is the fun, tractable part. |
| Windows 7 *look and feel* | **Feasible** as an original lookalike theme. | Re-create the style with original art. Shipping Microsoft's actual bitmaps, icons, fonts (Segoe UI), sounds or the Windows logo is copyright/trademark infringement. Users may optionally import assets from a Windows license they own. Call it "Aero-style", never "Windows 7". |
| Run Windows `.exe` (Chrome, RetroBat, RPGs) | **Very hard, but proven possible** via a Win32 reimplementation. Wine took ~30 years to get where it is. | Port **Wine** (LGPL) onto an AeroForge POSIX-ish personality instead of re-writing Win32. Write our own "AeroNT" syscall personality only for the hot paths. |
| Modern Chrome | **Very hard natively** (Chromium needs a mature POSIX/Linux-like environment, sandbox primitives, GPU process). | Port **Chromium natively** (it already supports Linux, Fuchsia, BSDs) once the POSIX layer is mature; Windows Chrome under Wine is a fallback, not the main plan. |
| Native NVIDIA / AMD GPU drivers | **Hardest single item.** Linux's amdgpu alone is several million lines. NVIDIA's open kernel module still depends on proprietary GSP firmware and a userspace stack written for Linux. | Phase it: virtio-gpu in QEMU → AMD first (the owner's own Radeon) by porting Linux DRM drivers through a **driver compatibility shim** (the approach FreeBSD's LinuxKPI uses) → Intel → NVIDIA via the open `nvidia-open` kernel module + Mesa NVK, also via the shim. Never "from scratch". |
| DirectX 11/12 → Vulkan | **Feasible once Vulkan works.** | Reuse **DXVK** and **VKD3D-Proton** unchanged. They only need Vulkan + Wine. |
| PCSX2, RPCS3, Dolphin, DuckStation | **Feasible** natively; all are open source and portable (Qt or SDL + Vulkan). | Native ports. Much better than running their Windows builds under Wine. |
| RetroBat | **Windows-only frontend** (bundles Windows emulator builds). | Run under Wine, or offer **ES-DE** (EmulationStation Desktop Edition, the open, cross-platform base RetroBat builds on) as the native equivalent. |
| DOSBox-X integration | **Very feasible.** Open source, SDL-based. | Native port plus shell integration. This is one of the easiest "wow" features. |
| Zero telemetry, no accounts | **Trivially feasible**; it's a policy decision. | Enforced by design (Section 3.7). |
| Google Chrome (branded) in a FOSS store | **Conflict.** Chrome is proprietary and Google doesn't ship a build for a new OS. | Store ships **Chromium** (BSD licence) or **ungoogled-chromium**. Branded Chrome could only appear as a Windows build under Wine, listed in a clearly marked "proprietary via compatibility layer" section, which breaks the "strictly FOSS" rule. Recommendation: Chromium only. |

**Time and team reality:** a small team (3 to 5 strong systems engineers) could reach "boots to an Aero-style desktop and runs DOSBox-X and native emulators in QEMU" in roughly 2 to 3 years. "Runs Chrome and DX12 games on a real AMD GPU" is a 5+ year horizon. A solo developer should treat the roadmap's first three phases as the realistic personal scope. These are estimates from comparable projects, not measurements.

**Recommended strategic choice:** *own the kernel, the desktop, the store and the integration; reuse proven open source for everything users actually judge you on* (browser engine, Win32, graphics stack, emulators). That's the only way the "from scratch" kernel goal and the "runs modern software" goal can both be true.

---

## 1. Kernel & Architectural Blueprint

### 1.1 Kernel style: a hybrid ("AeroKernel")

A pure microkernel is cleaner but costs IPC on every GPU submission and file read, which hurts the gaming/emulator goal. A monolithic kernel is fast but makes a bug anywhere fatal. AeroKernel is a **hybrid**, in the NT sense:

- **In the kernel (ring 0, Rust):** memory manager, scheduler, process/thread objects, IPC, capability and security system, interrupt and exception dispatch, the object manager core, timekeeping.
- **In the kernel (ring 0, C++, behind narrow Rust-owned interfaces):** performance-critical drivers that need DMA and interrupts with minimum latency: GPU kernel driver (via the DRM shim), NVMe, xHCI USB, network NIC fast path, HD Audio.
- **In user space (ring 3, mostly Rust, some C++):** filesystems (optional in-kernel for the root FS for speed), network stack, input services, compositor, window manager, Win32 personality, POSIX personality, package manager.

Drivers start in user space by default (crash = restart the driver, not the machine) and are promoted into ring 0 only when profiling proves it matters.

### 1.2 Layer diagram

```
┌──────────────────────────────────────────────────────────────────────┐
│  Apps: Chromium, PCSX2, RPCS3, Dolphin, DuckStation, DOSBox-X, games │
├───────────────┬──────────────────┬───────────────────┬───────────────┤
│ Native AeroAPI│ POSIX personality│ Win32 personality │ DOS subsystem │
│ (Rust/C++ SDK)│ (libc: relibc or │ (Wine port +      │ (DOSBox-X     │
│               │  musl port)      │  DXVK/VKD3D)      │  containers)  │
├───────────────┴──────────────────┴───────────────────┴───────────────┤
│ User-space services: AeroShell (desktop), Compositor (Glass), VFS,   │
│ NetStack, InputSvc, AudioSvc, PkgSvc (AeroCenter), Mesa userspace    │
├──────────────────────────────────────────────────────────────────────┤
│  Syscall gate (Rust): capability-checked, versioned ABI              │
├──────────────────────────────────────────────────────────────────────┤
│  AeroKernel core (Rust, ring 0)                                      │
│  MM · Scheduler · Objects/Handles · IPC · Security · IRQ · Timers    │
├──────────────────────────────────────────────────────────────────────┤
│  Driver Host Interface (DHI): stable C ABI, Rust-owned               │
│  ├─ C++ in-kernel drivers (NVMe, xHCI, HDA, NIC fast path)           │
│  └─ LinuxKPI-style shim → ported Linux DRM drivers (i915/xe, amdgpu, │
│     nouveau/nova) ── C, compiled against shim headers                │
├──────────────────────────────────────────────────────────────────────┤
│  HAL: x86-64 (APIC/x2APIC, HPET/TSC, IOMMU, ACPI via ACPICA, PCIe)   │
├──────────────────────────────────────────────────────────────────────┤
│  Firmware: UEFI only · Limine or custom Rust UEFI loader             │
└──────────────────────────────────────────────────────────────────────┘
```

### 1.3 How Rust and C++ talk to each other

This is the part most hybrid designs get wrong. The rule: **Rust owns all lifetimes and all security decisions; C++ owns hardware.** They meet only across a narrow, versioned C ABI called the **Driver Host Interface (DHI)**.

- **ABI:** plain `extern "C"` functions and `#[repr(C)]` structs. No C++ classes, exceptions, or RTTI cross the boundary. No Rust generics or trait objects either.
- **Generation:** interface definitions live in one IDL file (`dhi.idl`). A build tool emits Rust bindings (`bindgen`-style) and C++ headers, so the two sides can't drift.
- **Memory:** C++ never calls a global allocator. It receives allocation callbacks from the Rust MM (`dhi_alloc_dma(size, align, flags) -> DmaBuf`). Every buffer is a handle with a Rust-tracked lifetime; C++ can borrow, never free behind Rust's back.
- **Concurrency:** C++ driver code runs in a context the Rust scheduler knows about (driver threads or IRQ bottom halves). Locks it needs are Rust spinlocks/mutexes exposed through the DHI, so lockdep-style deadlock checking covers both languages.
- **C++ dialect:** C++20, freestanding, `-fno-exceptions -fno-rtti`, a small in-house `kstd` (span, optional, intrusive lists, fixed vectors). No STL containers that allocate.
- **Failure isolation:** C++ drivers run with an IOMMU domain per device, so a buggy driver's DMA can't scribble over kernel memory. Optional: run a driver in a separate ring-0 address space guarded by Intel PKS (protection keys for supervisor).
- **Panics:** a Rust panic in the core halts with a crash dump. A fault inside a C++ driver context is caught by the exception dispatcher, the driver instance is torn down, and its device is reset when the device supports FLR (function-level reset).

### 1.4 Boot path

1. **UEFI firmware** loads the bootloader. No legacy BIOS (keeps scope sane).
2. **Bootloader:** start with **Limine** (mature, supports x86-64 handoff, framebuffer, memory map, modules). Later replace with an in-house Rust UEFI loader (`uefi-rs`) that supports Secure Boot with our own keys and A/B system images.
3. **Early kernel (Rust, `no_std`):** set up GDT/IDT, paging (4-level, 5-level if LA57 present), parse the memory map, bring up a serial and framebuffer console.
4. **ACPI:** port **ACPICA** (C, permissive licence) through a thin Rust wrapper; parse MADT, HPET, MCFG, FADT. Writing your own AML interpreter is a known multi-year trap.
5. **SMP bring-up:** wake APs via INIT-SIPI-SIPI, per-CPU data in `GS`, x2APIC.
6. **Init:** spawn `aerosmss` (session manager, the first user process), which starts services from a dependency graph (no shell scripts at boot).

### 1.5 Memory management (Rust)

- **Physical:** buddy allocator for pages, per-NUMA-node free lists, plus a slab allocator for kernel objects.
- **Virtual:** per-process address spaces with a VMA tree; demand paging, copy-on-write, memory-mapped files, shared sections (needed by Wine and Chromium), huge pages (2 MiB/1 GiB) for emulators that map big guest RAM (RPCS3, Dolphin).
- **Executable memory:** JIT emulators and JS engines need W^X with fast toggling. Provide dual-mapping (one RW view, one RX view of the same pages) so JITs never need truly RWX memory.
- **Hardening:** KASLR, SMEP, SMAP, UMIP, CET shadow stacks and IBT where supported, guard pages, kernel stack canaries.
- **Commit accounting** NT-style (Win32 apps expect `VirtualAlloc` commit semantics), with an OOM policy that never kills the foreground app first.

### 1.6 Scheduler (Rust)

Goal: games and emulators get the hardware.

- Per-CPU run queues, an EEVDF-style fair class for normal work, plus:
  - **Interactive/Game class:** foreground window's process gets boosted priority and preferred placement on P-cores / highest-boost cores (via Intel HFI/Thread Director and AMD CPPC preferred-core data).
  - **Real-time class:** audio and compositor threads, bounded and budgeted so they can't starve the system.
- **Cache/CCD awareness:** on multi-CCD Ryzen/X3D chips, keep a game's threads on the V-Cache CCD.
- **Emulator hints:** an API (`aero_thread_hint(LATENCY_CRITICAL | PIN_PHYSICAL_CORE)`) PCSX2/RPCS3 ports can call; the Win32 layer maps `SetThreadAffinityMask` and `SetThreadPriority` onto it.
- Tickless idle, high-resolution timers (TSC deadline), and a 1 ms-or-better timer resolution for games that ask for it without the Windows-style global penalty.

### 1.7 Objects, handles, IPC and security

- **Object manager:** everything (process, thread, section, event, file, window station, GPU context) is a kernel object with a refcount, a type and a security descriptor. Processes hold **handles** that carry rights. This maps almost 1:1 onto NT semantics, which makes the Win32 layer much easier.
- **Capabilities:** handles *are* capabilities. No ambient authority: a sandboxed process can only touch objects it was handed. This is the foundation for both the Chromium sandbox and the AeroCenter app sandbox.
- **IPC:** synchronous message passing with shared-memory bulk transfer ("ports", similar to NT ALPC / Zircon channels), plus futexes and event objects. Asynchronous I/O via a completion-ring model (io_uring-like) that the Win32 layer exposes as IOCP.
- **Users:** local accounts only. Optional password, optional disk encryption (TPM-sealed key). No online account path exists in the code.

### 1.8 Storage and filesystems

- **Root filesystem:** a copy-on-write FS with snapshots (port **OpenZFS** is heavy; recommended: write a simpler CoW FS later, but start by porting an existing one, or ship ext4/btrfs read-write via a port). Snapshots power safe updates and package rollback.
- **Compatibility:** NTFS (read/write, via a port of `ntfs-3g` logic or the Paragon-derived Linux ntfs3 driver through the shim), exFAT, FAT32, ISO9660/UDF (emulator disc images).
- **Case handling:** the Win32 view of the filesystem is case-insensitive (per-directory casefolding flag), which Wine and Windows games need.

### 1.9 x86-64 hardware interface summary

| Hardware | Approach |
|---|---|
| CPU features | CPUID-driven: AVX2/AVX-512 (RPCS3 benefits strongly), XSAVE/XSAVEC for large FPU state, FSGSBASE, PCID/INVPCID for cheap context switches |
| Interrupts | x2APIC, MSI/MSI-X for all PCIe devices, IOAPIC only for legacy |
| Timers | Invariant TSC primary, HPET fallback |
| PCIe | ECAM via MCFG, BAR mapping, IOMMU (VT-d / AMD-Vi) domains per device |
| Storage | NVMe (C++ driver, multiple queues per CPU), AHCI |
| USB | xHCI (C++), HID class for controllers (XInput-compatible mapping, DualShock/DualSense via HID) |
| Bluetooth | USB Bluetooth controllers (HCI over USB, firmware loading for Realtek/Intel/MediaTek chips); BLE HID for gamepads (Xbox, DualShock/DualSense, 8BitDo), Classic HID; HFP/HSP for headset mics, A2DP for audio out. Host stack in user space (Rust), ideally a port or adaptation of an existing stack (e.g. BlueZ's protocol logic or Android's Fluoride/Gabeldorsche) rather than written from scratch |
| Audio | Intel HDA first, USB Audio Class 2 next |
| Network | virtio-net, then Intel **igc** (I225/I226 2.5 GbE, the development machine's wired NIC) and e1000e (older Intel, and QEMU's e1000e for testing), then Realtek r8169; Wi-Fi: **MediaTek mt7921/mt7922** first (the development machine's card, ported from Linux mt76 via the shim), Intel iwlwifi later |
| Power | ACPI S3/S0ix later; CPU P-states via CPPC/HWP |

---

## 2. Compatibility Layer Strategy

### 2.1 Four "personalities" on one kernel

Like NT's original design, the kernel is neutral and **personalities** give apps the API they expect:

1. **Native AeroAPI** (Rust and C++ SDK): for the shell, AeroCenter and first-party apps.
2. **POSIX personality:** a C library (port **relibc** from Redox, written in Rust, or **musl**) plus a Linux-compatible-enough syscall surface (`mmap`, `futex`, `epoll`, `eventfd`, `memfd`, signals, `pthread`). Almost all FOSS (Chromium, Qt, SDL, Mesa, emulators, Wine itself) comes through here.
3. **Win32 personality:** Wine, ported. Runs `.exe` files.
4. **DOS subsystem:** DOSBox-X instances (Section 5 of the roadmap / 2.6 below).

### 2.2 Why the POSIX personality is the keystone

Every high-value target in the brief, including the Windows compatibility layer, is a POSIX program. So the order is fixed: **POSIX first, then everything else lands as a port.** Shortcut worth considering: implement enough of the **Linux syscall ABI** to run unmodified static Linux binaries (as FreeBSD's Linuxulator and Windows' WSL1 did). That lets us bootstrap compilers, Mesa and test suites before native ports exist.

### 2.3 Running Windows binaries (Chrome, RetroBat, RPGs)

**Recommendation: port Wine, then accelerate it, rather than rewriting Win32.**

```
 game.exe (PE/COFF, x86-64)
   │  calls kernel32 / user32 / d3d11 / xinput ...
   ▼
 Wine PE DLLs (ntdll, kernel32, user32, gdi32 ... built as PE)
   │  ntdll "unix side"
   ▼
 AeroNT bridge (new)  ── maps NT semantics directly onto AeroKernel objects
   │                     (handles, sections, events, IOCP, waits)
   ▼
 AeroKernel syscalls
```

Key points:

- **Loader:** Wine's PE loader runs as-is. AeroKernel adds a PE-aware `exec` path so double-clicking an `.exe` launches it through the Win32 personality automatically.
- **AeroNT bridge (our differentiator):** on Linux, Wine emulates NT synchronization in user space (wineserver, esync/fsync, and since 2024 ntsync in the Linux kernel). Because AeroKernel's object model *is* NT-shaped (1.7), the bridge maps `NtWaitForMultipleObjects`, events, mutants, semaphores and sections straight onto kernel objects. That removes wineserver round-trips, which is where Wine loses most performance in games.
- **Graphics:** D3D9/10/11 → **DXVK**, D3D12 → **VKD3D-Proton**, OpenGL → Mesa. All produce Vulkan on our native driver stack (Section 4).
- **Windowing:** Wine's `winex11`/`winewayland` driver is replaced by a new `wineaero.drv` that talks to our compositor directly, so Win32 windows get Aero glass frames, taskbar buttons and peek previews like native apps.
- **Audio:** `wineaero` audio backend to AudioSvc. **Input:** XInput/DirectInput/RawInput mapped from InputSvc (controllers appear as XInput devices).
- **Anti-cheat:** kernel-level anti-cheat (EAC/BattlEye kernel modes, Vanguard) will not work, same as on Linux. Games using it are out of scope; say so in the store.
- **DRM/launchers:** Steam, Epic, GOG Galaxy run under Wine on Linux today with varying success; same expectation here. GOG (DRM-free) titles are the easiest win.

**What about Chrome specifically?**

1. **Primary path: native Chromium** via the POSIX personality. Chromium supports Linux, ChromeOS, Fuchsia, and community BSD ports, so its platform layer is designed to be ported. Work required:
   - `base/` platform layer (threads, files, shared memory, process launching),
   - **sandbox:** Chromium's Linux sandbox uses seccomp-BPF and namespaces. On AeroForge we instead implement a sandbox backend on our capability model: renderer processes start with an empty handle table plus a pre-granted IPC channel and shared-memory sections. This is arguably *stronger* than seccomp, because there's no ambient authority to filter.
   - **GPU process:** ANGLE over Vulkan, Skia over Vulkan/Graphite.
   - **Ozone platform:** a new `ozone/platform/aero` backend for windows and input.
   - V8, Blink, HTML5, WebAssembly, WebGPU (Dawn over Vulkan) all come "for free" once the above work.
2. **Fallback:** Windows Chromium builds under Wine. Works today on Linux with caveats (sandbox must often be disabled, which is unacceptable as a default for a browser). Use only for testing.
3. **Branded Google Chrome:** not distributable in a FOSS store and not built by Google for us. Ship Chromium / ungoogled-chromium. Firefox is a strong second browser to port, also via POSIX.

**RetroBat:** a Windows-only frontend that bundles Windows emulator builds. Two options:
- Run RetroBat under Wine with the native-performance emulators substituted where possible, or
- Ship **ES-DE** natively, themed and preconfigured to match RetroBat's experience, and call that the default "AeroForge Retro" frontend. **Recommended**: native, faster, and maintainable.

### 2.4 Native emulators (PCSX2, RPCS3, Dolphin, DuckStation)

All four are open source, C++, use Qt or SDL, and target Vulkan. Native ports beat Wine:

| Emulator | Needs from AeroForge |
|---|---|
| PCSX2 | Qt6, Vulkan, JIT (W^X dual-mapping), many threads (EE/VU/GS), AVX2 |
| RPCS3 | Qt6, Vulkan, LLVM (for its PPU/SPU recompilers), huge guest memory reservations (reserves tens of GB of address space), AVX-512 optional but strongly beneficial, many threads |
| Dolphin | Qt6, Vulkan, JIT, fastmem (relies on mapping guest memory and catching page faults: needs fast user-mode exception delivery) |
| DuckStation | Qt6 or its newer native UI, Vulkan, JIT |

Kernel features they force us to get right: fast signal/exception delivery for fastmem (Dolphin, RPCS3), large sparse address space reservations, dual-mapped JIT memory, precise high-resolution timers, and scheduler thread placement (1.6).

### 2.5 Libraries and toolchain to port, in order

1. LLVM/Clang + LLD (also needed by RPCS3 and Mesa), Rust toolchain target `x86_64-unknown-aeroforge`.
2. libc (relibc or musl), libc++ / libstdc++, zlib, zstd, OpenSSL or BoringSSL.
3. SDL3 (abstracts video/audio/input for emulators and DOSBox-X: a single SDL backend unlocks many apps).
4. Mesa (Vulkan + GL drivers), libdrm.
5. Qt6 (QPA platform plugin for our compositor).
6. FreeType, HarfBuzz, fontconfig, ICU.
7. Wine, DXVK, VKD3D-Proton.
8. Chromium.

### 2.6 DOS subsystem (DOSBox-X)

**Integration model:**
- DOSBox-X is ported natively via SDL3 and registered as the **DOS personality** handler.
- The shell's file-type resolver inspects files before launching: a `.exe` with an `MZ` header but **no PE header** (or a NE/LE header, for Win16 and DOS-extender binaries) is routed to DOS; `.com` and `.bat` always route to DOS. PE files go to the Win32 personality. Win16 `NE` executables can go to DOSBox-X running Windows 3.x (user-supplied) or Wine's Win16 support.
- **Isolation:** each launch creates a **DOS container**: a fresh sandboxed process with access only to the game's folder (mounted as `C:`), a per-game save overlay (writes go to a CoW overlay so the original files stay pristine), optional CD image mounts, and no network unless the profile enables IPX/modem emulation.

**Dynamic profiles ("DOSProfile engine"):**
1. **Identify** the game: hash the main executable and folder layout; look it up in a community **DOSProfile database** (shipped in AeroCenter as a signed data package, same format as apps).
2. **Known title:** apply the curated profile: CPU type and cycle count (e.g. fixed 3000 cycles for speed-sensitive 286-era games, `max` for late-DOS 3D titles), memory (EMS/XMS), sound device (Sound Blaster 16 at A220 I5 D1 H5 / Gravis Ultrasound / Roland MT-32 with user-supplied ROMs / General MIDI via FluidSynth), video (VGA/SVGA/Tandy/CGA composite), aspect correction, and a CRT shader (e.g. crt-lottes, crt-geom).
3. **Unknown title:** heuristics: scan the binary for strings (`SETSOUND`, `DOS/4GW`, `Sound Blaster`, `ULTRASND`), check for DOS extenders, check file dates to estimate era, then pick a conservative profile (SB16, `auto` cycles, aspect correction on). Offer a "Tweak profile" panel in the window's system menu; the user's tweaks are saved locally and can optionally be exported as a contribution file (manual, no automatic upload).
4. **Setup programs:** run `SETUP.EXE`/`INSTALL.EXE` automatically on first launch when the database says the game needs it, with the profile's sound settings pre-answered where possible.

Explorer gets a "DOS Properties" tab on DOS executables showing the active profile, like Windows' old PIF settings.

---

## 3. AeroCenter App Store Subsystem

### 3.1 Principles

- **Decentralized:** any number of repositories ("feeds"), each just static files over HTTPS (or IPFS / BitTorrent mirrors). No server-side logic, so no server can track anything beyond what a plain file mirror sees.
- **No accounts, no payments, no analytics**, enforced in code and by policy: the client sends no identifiers, no install counts, no crash reports unless the user exports one manually.
- **Reproducible and verifiable:** every package is signed; builds are reproducible so anyone can verify a binary matches its source.

### 3.2 Repository structure

```
https://repo.aeroforge.example/stable/
├── feed.json            # repo metadata, current snapshot pointer
├── feed.json.sig        # signature (Ed25519 / minisign), key pinned in OS
├── snapshots/
│   └── 2026-10-04.1/
│       ├── index.json.zst       # all package metadata for this snapshot
│       ├── index.json.zst.sig
│       └── sections.json        # curated sections, ordering, screenshots refs
├── pool/
│   ├── c/chromium/chromium_130.0.6723.58-1_x86_64.afp
│   ├── p/pcsx2/pcsx2_2.2.0-1_x86_64.afp
│   ├── r/rpcs3/...
│   └── ...
├── deltas/              # binary diffs between versions (zstd --patch-from)
├── recipes/             # build recipes (source of truth, in git too)
└── media/               # icons, screenshots (content-addressed)
```

- Metadata uses **The Update Framework (TUF)**-style roles: root key (offline), targets key, snapshot key, timestamp key. This protects against rollback, freeze and mix-and-match attacks by a malicious mirror.
- Mirrors are dumb file hosts. Community mirrors can be added by URL or by scanning a QR code; the client picks mirrors randomly per request so no single mirror sees a user's full install list.

### 3.3 Package format: `.afp` (AeroForge Package)

An `.afp` is a **squashfs-like read-only image** (zstd-compressed, content-addressed chunks) plus a manifest:

```toml
# manifest.toml
[package]
id          = "org.pcsx2.PCSX2"
version     = "2.2.0-1"
license     = "GPL-3.0-or-later"
source      = "https://github.com/PCSX2/pcsx2/archive/v2.2.0.tar.gz"
source_sha256 = "..."
recipe      = "recipes/pcsx2.toml"
personality = "posix"            # posix | win32 | dos | native

[runtime]
requires = ["org.aeroforge.Runtime.Qt6//6.8", "org.aeroforge.Runtime.Vulkan//1"]

[permissions]                     # shown to user at install time
gpu        = true
audio      = true
controllers= true
filesystem = ["user-choice"]      # only files the user picks via portal
network    = false

[desktop]
name = "PCSX2"
categories = ["Emulation"]
mime = ["application/x-ps2-iso"]
```

Design choices:
- **Runtimes, not per-app dependency trees:** like Flatpak, apps depend on a few shared, versioned runtimes (Base, Qt6, SDL3, Vulkan/Mesa, Wine, DXVK). This is how "automated dependency resolution" stays simple and robust: resolve a handful of runtimes, not hundreds of libraries.
- **Content-addressed storage:** identical files across packages are stored once (OSTree-style dedup).
- **Build from source option:** each package has a recipe; "Compile locally" runs the recipe in a sandboxed builder and checks that the output hash matches the published binary (reproducible-build verification).

### 3.4 Curated sections

`sections.json` drives the store front page. Sections are signed data, curated by maintainers via pull requests to a public git repo:

| Section | Contents (examples) | Notes |
|---|---|---|
| Browsers & Utilities | Chromium, ungoogled-chromium, Firefox, 7-Zip-compatible archiver (p7zip/7-Zip Linux build), text editors, system monitors | Google Chrome is proprietary and cannot be in the FOSS feed (see Section 0) |
| Emulation Frontends & Standalones | ES-DE ("AeroForge Retro"), PCSX2, RPCS3, Dolphin, DuckStation, PPSSPP, RetroArch + cores, DOSBox-X, ScummVM | RetroBat listed as a Win32-personality package if its licence allows redistribution, clearly marked as running under compatibility |
| Indie & Retro Games | FOSS games (SuperTuxKart, Veloren, OpenMW, 0 A.D., Cataclysm DDA), freeware/open-source DOS games | Commercial games aren't in the store; they come from GOG/Steam under Wine |
| Compatibility Wrappers | Wine runtime versions, DXVK, VKD3D-Proton, Mesa updates | Pulled automatically as dependencies |
| Drivers & Firmware | GPU firmware blobs (redistributable ones), controller profiles | Firmware for AMD/NVIDIA GPUs is binary but redistributable; keep it in a separate, clearly labelled "non-free firmware" feed so the main feed stays strictly FOSS |

Emulators never ship BIOS files or game ROMs. Setup wizards point users to dump their own (required for legality: PS2/PS3 BIOS and firmware are copyrighted).

### 3.5 Install flow and dependency resolution

1. User clicks **Install** on PCSX2.
2. Client reads the manifest, resolves runtimes with a small SAT-free resolver (runtimes are versioned branches; pick the newest compatible branch already installed or download it).
3. Shows a **permissions sheet** (GPU, audio, controllers, user-picked files).
4. Downloads chunks in parallel from random mirrors, verifies each chunk hash against the signed index.
5. Mounts the image read-only at `/apps/org.pcsx2.PCSX2/2.2.0-1/`, writes the shell integration (Start Menu entry, file associations).
6. For Win32 packages, also prepares a per-app Wine prefix and pulls the Wine/DXVK runtime versions the package pins.
7. Driver updates go through the same path but install into the system image (see 3.7) and require confirmation.

### 3.6 Sandboxing downloaded apps

Built on the capability system (1.7), not bolted on:
- Each app runs in an **app container**: its own namespace view of the filesystem (app image read-only, its own data dir read-write, nothing else).
- **Portals** for user-mediated access: file picker, "open with", screenshot, camera, microphone. The app receives a handle to *only* the file the user picked.
- **Device access** is a capability granted at launch per the manifest (GPU device node, audio stream, HID controllers).
- **Network** is a capability; denied unless the manifest requests it, and visible in a per-app toggle.
- Win32 apps get the same container, with the Wine prefix inside the app's data dir. DOS containers are stricter still (2.6).
- Users can tighten (never silently loosen) permissions after install.

### 3.7 Updates without tracking

- **Pull only.** The client fetches `timestamp.json` (a tiny file, same for everyone) on a schedule the user controls (default daily, configurable, or manual only). There's nothing user-specific in any request: no IDs, no install list, no cookies; requests carry a generic User-Agent.
- **Privacy of the install list:** the client downloads the whole index snapshot (compressed, typically a few MB), so mirrors never learn *which* packages you're interested in until download. Optional Tor/onion mirrors and randomized mirror selection reduce what any one mirror sees.
- **Deltas:** binary deltas between versions to keep updates small.
- **Atomic system updates:** the base OS is an immutable image with A/B slots. Updates write to the inactive slot, then switch on reboot; a failed boot rolls back automatically. App updates are atomic by nature (new image mounted, old one kept until the app closes, then garbage-collected; one previous version kept for rollback).
- **No forced updates.** Security updates are highlighted, never forced. No restarts without consent.

---

## 4. Graphics & Driver Architecture

### 4.1 Honest framing

Writing a modern GPU driver from scratch for NVIDIA or AMD is beyond any small team: AMD's Linux kernel driver is several million lines (much of it generated register headers), and the userspace Vulkan driver (RADV) is another large codebase. Intel's is similar. The design below therefore **reuses the Linux open-source graphics stack** through a compatibility shim, which is exactly how FreeBSD gets modern GPU support. AeroForge keeps its own kernel; the drivers are guests.

### 4.2 Stack overview

```
 Game (DX11/12 via DXVK/VKD3D-Proton) · RPCS3 · Chromium (ANGLE/Skia/Dawn)
                │ Vulkan / OpenGL
                ▼
 Mesa userspace drivers: RADV (AMD) · ANV (Intel) · NVK (NVIDIA) · venus/virgl (VM)
                │ libdrm ioctls (unchanged)
                ▼
 /dev/dri compatibility node in AeroKernel  (DRM uAPI emulation)
                │
                ▼
 LinuxKPI-style shim ("AeroKPI")  ── C/C++, implements Linux kernel APIs
   (kmalloc, workqueues, dma-buf, dma-fence, PCI, firmware loading, IRQ...)
   on top of the Driver Host Interface → Rust MM/scheduler
                │
                ▼
 Ported Linux DRM drivers: virtio-gpu · i915/xe · amdgpu · nouveau/nova
                │
                ▼
 GPU hardware (PCIe BARs, MSI-X, IOMMU domain)
```

Licence note: Linux DRM drivers are GPL-2.0 (many core files are dual MIT/GPL, amdgpu is largely MIT-licensed). Linking GPL drivers into the kernel likely makes the combined kernel image GPL-2.0, or requires keeping them as separately loaded modules with careful legal review. **Decide the kernel licence early** (recommendation: GPL-2.0-compatible, e.g. MIT/Apache-2.0 for our own code so it can combine with GPL drivers).

### 4.3 GPU bring-up sequence

| Stage | Target | Why |
|---|---|---|
| G0 | UEFI GOP framebuffer | Pixels on screen on day one, any machine |
| G1 | **virtio-gpu in QEMU** (2D, then 3D via **venus** = Vulkan passthrough to host) | Fast iteration, real Vulkan in a VM, host GPU does the work. Lets the compositor, Mesa and DXVK be developed years before bare-metal drivers |
| G2 | **AMD** (amdgpu via shim + RADV) | First bare-metal target: it is the development machine's GPU (Ryzen 9 + Radeon). Strong open stack, amdgpu is largely MIT-licensed, and RADV is the best Vulkan driver for gaming on Linux. Needs AMD's redistributable firmware blobs |
| G3 | **Intel** (i915/xe via shim + ANV) | Best documented, open firmware story, iGPUs everywhere; second because there is no Intel test machine yet |
| G4 | **NVIDIA** (nouveau or the newer Rust `nova` driver, or NVIDIA's open kernel module, + Mesa NVK) | Turing (RTX 20) and newer only; depends on GSP firmware. NVK performance has been catching up but trails the proprietary driver. NVIDIA's proprietary userspace driver is not an option on a new OS without NVIDIA's cooperation |

### 4.4 Display and compositor ("Glass")

- **Kernel mode setting (KMS)** via the ported DRM drivers: atomic modesetting, multi-monitor, variable refresh rate (FreeSync/G-Sync Compatible), HDR metadata later.
- **Compositor** in C++ (user space, real-time scheduling class): a Vulkan-based scene graph.
  - Windows are GPU buffers (dma-buf handles) shared zero-copy with apps.
  - **Aero Glass effect:** per-window dual-Kawase blur of the content behind the frame, tinted with the user's colour and a specular "glass streak" highlight texture, rounded corners via an SDF mask, soft drop shadows. All done in a single pass per frame for windows that changed.
  - **Direct scanout / fullscreen bypass:** when a game is fullscreen (or borderless-fullscreen and on top), the compositor hands its buffer straight to a display plane: zero compositing latency, identical to exclusive fullscreen.
  - **Tearing control and VRR** honoured for games; the desktop stays tear-free.
  - **Peek / thumbnails:** live taskbar previews are just the window buffers sampled at small size, no extra rendering by apps.
- **Protocol:** a Wayland-like, capability-based protocol ("AeroWin"). Consider implementing actual **Wayland** protocol compatibility so Qt/SDL/GTK ports work with less effort.

### 4.5 Graphics API translation for games

| App API | Path |
|---|---|
| Vulkan | Native via Mesa |
| OpenGL | Mesa (radeonsi/iris/zink-on-Vulkan) |
| Direct3D 8/9/10/11 | DXVK → Vulkan |
| Direct3D 12 | VKD3D-Proton → Vulkan (needs Vulkan 1.3 + descriptor-heap-style extensions RADV/ANV/NVK provide) |
| Direct3D 7 and earlier | WineD3D → OpenGL/Vulkan |
| Metal-style / WebGPU | Dawn (Chromium) on Vulkan |

**Shader compile stutter:** ship Mesa's on-disk shader cache plus Vulkan `VK_EXT_graphics_pipeline_library` support (DXVK uses it to remove most stutter), and support community **pre-compiled pipeline caches** distributed as AeroCenter data packages per game + GPU family (no telemetry: caches are built by volunteers, downloaded like any other package).

### 4.6 Demanding emulator requirements (RPCS3, PCSX2 hardware renderer, Dolphin)

- Vulkan 1.3 with `VK_EXT_shader_object` or pipeline libraries, `VK_KHR_dynamic_rendering`, robust buffer access, external memory host import (RPCS3 uses host-visible memory heavily).
- Async compute queues (exposed by RADV/ANV/NVK).
- Low-latency present: `VK_KHR_present_wait` / mailbox mode, and compositor bypass.
- Resizable BAR support in the PCIe layer for large host-visible VRAM mappings.

### 4.7 Other drivers

| Class | Plan |
|---|---|
| Audio | HDA codec driver (C++), USB Audio, Bluetooth audio (HFP/HSP mics, A2DP out); AudioSvc mixer with low-latency mode (≤ 5 ms buffers) for emulators; WASAPI-compatible behaviour exposed to Wine |
| Input | xHCI + HID, Bluetooth HID (gamepads paired once in Settings, reconnect automatically); controller database (SDL's `gamecontrollerdb`) built into InputSvc so every controller maps consistently in RetroBat/ES-DE, emulators and Wine |
| Network | virtio-net → Intel igc/e1000e → Realtek r8169 → MediaTek Wi-Fi (mt76: mt7921/mt7922) → Intel Wi-Fi (iwlwifi), the Wi-Fi drivers ported from Linux through AeroKPI |
| Storage | NVMe/AHCI native C++ |

---

## 5. UI / UX: Aero-style desktop (design notes)

(Supporting section for the roadmap; the brief's Section 2.)

- **AeroShell** (C++ with a Rust core for state/services): desktop, taskbar, Start Menu, notification area, Explorer, Control Panel.
- **Start Menu:** two-column layout: left pinned + recently used programs, a cascading **All Programs** tree, an instant search box (local indexer service, no web results, no ads); right column with user folders, Control Panel, Devices, Run, power button with a submenu.
- **Taskbar:** distinct window buttons (option for "never combine" labels, Windows 7 classic mode), Jump Lists, hover **peek previews** with close buttons, "Aero Peek" show-desktop button at the far right, notification area with overflow chevron, clock with calendar flyout.
- **Window management:** Aero Snap (drag to edges), Aero Shake (shake to minimize others), Flip 3D style switcher (Win+Tab) as an optional effect, Alt+Tab with live thumbnails.
- **Explorer:** navigation pane (Favorites, Libraries, Computer, Network), breadcrumb address bar, a classic command bar (Organize, Open, Share-free, Burn, New folder) with an optional menu bar (Alt), details/preview panes, and a detailed status bar (item count, selection size, free space). DOS and Win32 executables show their personality and profile in Properties.
- **Visual assets:** all original. Commission a glass theme, an icon set and sounds in the *spirit* of 2009-era Aero. Use an open font with similar metrics to Segoe UI (e.g. Selawik, which Microsoft released under the OFL as a Segoe UI fallback, or Open Sans / Noto Sans). Offer an "import theme assets from your own Windows installation" tool, run locally, for users who own a licence. The theme engine should read a documented `.aerotheme` format so the community can create themes.
- **Performance budget:** idle desktop under 400 MB RAM and ~0 % CPU; no background services that aren't user-visible in a Services panel.

---

## 6. Step-by-Step Development Roadmap

Each phase ends with a demo that proves it. Durations assume a small dedicated team (3 to 5 engineers); they are rough estimates, not commitments. A solo developer should expect Phases 0 to 3 to be a multi-year personal project on their own.

### Phase 0: Foundations (≈ 2 to 3 months)
- Repo layout, licence decision (recommend MIT/Apache-2.0 for own code), coding standards for Rust and C++.
- Cross toolchain: Rust custom target, Clang/LLD for C++, build system (Cargo for Rust + a top-level `xtask` or GN/Meson orchestrating C++ and image creation).
- CI that boots every commit in **QEMU** (OVMF UEFI) and runs a kernel test suite headless.
- **Demo:** "Hello from AeroKernel" on serial and framebuffer via Limine.

### Phase 1: Kernel core (≈ 6 to 9 months)
- GDT/IDT, exceptions, paging, physical + virtual MM, slab allocator, KASLR.
- ACPI via ACPICA, x2APIC, HPET/TSC, SMP bring-up.
- Scheduler (per-CPU queues, priorities), threads, processes, user mode (ring 3), syscall gate.
- Object manager, handles/capabilities, IPC ports, futexes, events.
- Driver Host Interface + first C++ drivers: **NVMe** first (the development machine boots from NVMe), then **AHCI** for its SATA drive (small, and it reuses NVMe's block layer), then **xHCI** for USB 3 keyboard/mouse (Ryzen boards have no PS/2), then virtio-net, then **Intel Ethernet** (igc for I225/I226, e1000e; tested on QEMU's e1000e/igb) for the development machine's wired port.
- **Demo:** multi-core kernel running user-mode processes that talk over IPC, reading files from a disk image.

### Phase 2: Userland and POSIX personality (≈ 6 to 9 months)
- VFS, root filesystem (start with a port or a simple FS; CoW FS later), FAT32/exFAT.
- Port relibc or musl, then a shell, coreutils (uutils, in Rust), LLVM/Clang self-hosted.
- Partial Linux-syscall ABI for bootstrapping static binaries.
- Network stack (smoltcp in Rust to start), TLS (rustls), DNS, DHCP, over virtio-net and the Intel wired driver.
- **Demo:** AeroForge compiles a C++ program on itself and downloads a file over HTTPS.

### Phase 3: Graphics, input, audio in a VM (≈ 6 to 9 months)
- GOP framebuffer driver → virtio-gpu 2D → virtio-gpu 3D (**venus**) via the AeroKPI shim.
- Port libdrm + Mesa (venus/lavapipe first), SDL3, FreeType/HarfBuzz.
- Glass compositor v1 (Vulkan, window buffers, input routing, blur).
- USB HID input, controllers; HDA audio (the board's codec, and the HDA controller on the Radeon that carries HDMI/DisplayPort audio), AudioSvc.
- Bluetooth: HCI over USB, pairing, HID gamepads first (emulators need them), then HFP/HSP mic and A2DP audio. QEMU can pass a real USB Bluetooth dongle through for testing.
- Wi-Fi: MediaTek mt7921/mt7922 (the development machine's card) by porting Linux's mt76 driver through AeroKPI, with WPA2/WPA3 in user space. These MediaTek cards usually carry the Bluetooth radio too (on USB, Linux btmtk), so Wi-Fi and Bluetooth share firmware loading work.
- **Port DOSBox-X here** (SDL3 only), the earliest flagship app.
- **Demo:** Vulkan triangle and DOSBox-X playing a DOS game in QEMU with sound and a CRT shader.

### Phase 4: Aero-style desktop (≈ 6 to 12 months, overlaps Phase 5)
- AeroShell: taskbar, Start Menu, notification area, window decorations, snap/peek, Alt+Tab.
- Explorer with command bar, details view, status bar, file-type resolver.
- Control Panel basics (display, sound, users, updates, privacy page that states "nothing is collected").
- Qt6 platform plugin so Qt apps get native-looking windows.
- **DOS subsystem integration:** double-click `.exe/.com/.bat` routing, DOS containers, DOSProfile engine + first community profile database.
- **Demo:** boot to the Aero-style desktop in QEMU, browse files in Explorer, double-click a DOS game, it launches with an auto-generated SB16 profile.

### Phase 5: AeroCenter (≈ 4 to 6 months)
- `.afp` format, image mounting, runtimes, TUF-style signed feeds, mirror client, deltas.
- App containers + portals on the capability system.
- Store UI with curated sections; recipes repo and reproducible build farm (volunteer-run builders).
- A/B immutable system image and rollback.
- **Demo:** install DuckStation and DOSBox-X from AeroCenter with zero manual dependency steps; update and roll back.

### Phase 6: Native emulators (≈ 6 to 9 months)
- Kernel work: dual-mapped JIT memory, fast user exception delivery (fastmem), huge reservations, thread hints, AVX-512 state handling.
- Ports: DuckStation → Dolphin → PCSX2 → RPCS3 (LLVM required) → PPSSPP, RetroArch.
- ES-DE as the native "AeroForge Retro" frontend, themed, with controller mapping via InputSvc.
- **Demo:** all four headline emulators running in QEMU with venus at full speed for lighter titles.

### Phase 7: Bare-metal graphics (≈ 12 to 24 months, can start in parallel from Phase 4)
- Harden AeroKPI; port amdgpu + RADV first (RDNA2/RDNA3/RDNA4), on the Ryzen + Radeon development machine. Firmware feed in AeroCenter.
- Native display output on the Radeon through amdgpu's display core (DC): mode setting on HDMI and DisplayPort, multiple monitors, high refresh rates. HDMI audio follows from it: DC enables the audio endpoint and hands the monitor's audio capabilities (ELD) to the HDA driver. Until then, every display (HDMI included) runs at the mode the UEFI firmware set, with no HDMI audio.
- Then i915/xe → Intel + ANV on real hardware.
- nouveau/nova + NVK for Turing+ NVIDIA.
- KMS multi-monitor, VRR, direct scanout.
- **Demo:** RPCS3 and a Vulkan game running on a real AMD Radeon desktop.

### Phase 8: Win32 personality (≈ 12 to 18 months, start after Phase 4)
- Port Wine (POSIX side first, unmodified), then `wineaero.drv` windowing/audio/input.
- AeroNT bridge: NT sync objects and sections mapped to kernel objects.
- DXVK + VKD3D-Proton runtimes in AeroCenter; per-app prefixes; the shell routes PE files to Win32.
- RetroBat under Wine as a compatibility showcase; GOG DRM-free games as the test corpus.
- **Demo:** a DX11 game and a DX12 game running from Explorer double-click on real AMD hardware.

### Phase 9: Native Chromium (≈ 12 to 18 months, start after Phase 5)
- `ozone/platform/aero`, `base/` port, capability-based sandbox backend, GPU process on Vulkan (ANGLE, Skia Graphite, Dawn).
- Ship Chromium / ungoogled-chromium in AeroCenter; port Firefox as a second engine.
- **Demo:** sandboxed Chromium passing Web Platform Tests subsets, YouTube with hardware video decode (VA-API-equivalent via Mesa).

### Phase 10: Polish, hardware breadth, 1.0 (ongoing)
- More Wi-Fi chipsets (Intel iwlwifi, Realtek), more Bluetooth chipsets and profiles, sleep/resume, laptops' power management.
- Installer, disk encryption, Secure Boot with own keys, recovery environment.
- Accessibility (screen reader, high contrast, keyboard navigation), localisation.
- Security audit of the kernel syscall surface and the sandbox; fuzzing (syzkaller-style) in CI.
- **1.0 definition:** installs on a mainstream AMD or Intel desktop, boots to the Aero-style desktop, runs Chromium, the four headline emulators, DOSBox-X games via double-click, and a curated list of DX11/DX12 games via Wine.

### Dependency view

```
P0 → P1 → P2 → P3 ─┬→ P4 → P5 ─┬→ P6
                   │           ├→ P8 (needs P7 for real HW, P3 venus for VM)
                   └→ P7 ──────┤
                               └→ P9
```

---

## 7. Biggest risks and mitigations

| Risk | Mitigation |
|---|---|
| Scope explosion (this is several large projects in one) | Strict phase demos; reuse upstream projects; say no to features outside 1.0 |
| GPU drivers on real hardware | Live in QEMU/venus as long as possible; shim approach instead of rewrites |
| Legal: Windows 7 assets and trademarks | Original artwork only; "Aero-style" naming; optional local import of user-owned assets |
| Legal: GPL drivers in a permissively licensed kernel | Pick licences in Phase 0; get a review before shipping GPL-derived binaries |
| Legal: emulator BIOS / ROMs | Never distribute; dump-your-own wizards |
| Chrome branding | Ship Chromium; don't promise "Google Chrome" |
| Kernel anti-cheat games | Explicitly unsupported, documented in the store |
| Contributor burnout | Small, frequent, visible milestones (DOSBox-X in Phase 3 is the first "wow") |

## 8. Suggested immediate next steps

1. Decide licences (kernel, shell, store) and the project name's trademark check.
2. Set up the repo with the Rust target, Limine boot, QEMU CI (Phase 0).
3. Write the DHI IDL and the first Rust ↔ C++ driver (serial or virtio-blk) to prove the language boundary early.
4. Prototype the Glass blur shader on an existing OS (e.g. a small Vulkan app on Linux or Windows) so the look is nailed before the compositor exists.
