//! AeroShell (kernel edition): a line-based command prompt running as a
//! kernel thread on cpu0, until a user-mode shell exists.

use alloc::string::String;
use core::sync::atomic::Ordering;

use crate::console::{self, CYAN, YELLOW};
use crate::{acpi, apic, arch, block, bt, dhi, interrupts, ipc, kprint, kprintln, memory, modules, net, pci, percpu, process, sched, smp, sound, usb, vfs};

/// Kernel thread entry.
pub fn run(_: u64) {
    let mut shell = Shell { line: String::new() };
    let mut last_second = u64::MAX;
    // Let the boot messages from aerosmss settle before showing a prompt.
    sched::sleep_ticks(apic::TIMER_HZ * 3);
    shell.prompt();
    loop {
        while let Some(c) = interrupts::pop_key() {
            shell.on_key(c);
        }
        let secs = sched::ticks() / apic::TIMER_HZ;
        if secs != last_second {
            last_second = secs;
            crate::update_tray(secs);
        }
        sched::sleep_ticks(2);
    }
}

struct Shell {
    line: String,
}

impl Shell {
    fn prompt(&self) {
        console::print_colored(CYAN, format_args!("aero"));
        kprint!("> ");
    }

    fn on_key(&mut self, c: u8) {
        match c {
            b'\n' => {
                kprintln!();
                let line = core::mem::take(&mut self.line);
                self.run(line.trim());
                self.prompt();
            }
            8 => {
                if self.line.pop().is_some() {
                    kprint!("\x08");
                }
            }
            0x20..=0x7E if self.line.len() < 120 => {
                self.line.push(c as char);
                kprint!("{}", c as char);
            }
            _ => {}
        }
    }

    fn run(&mut self, cmd: &str) {
        let (name, arg) = cmd.split_once(' ').unwrap_or((cmd, ""));
        match name {
            "" => {}
            "help" => {
                kprintln!("  help        this list");
                kprintln!("  about       what AeroForge is");
                kprintln!("  ps          processes and threads, per CPU");
                kprintln!("  sched       per-CPU run queues and context switches");
                kprintln!("  run <prog>  start a user program ({})", program_list());
                kprintln!("  ports       published IPC ports");
                kprintln!("  lspci       PCIe devices");
                kprintln!("  lsusb       USB controllers and devices");
                kprintln!("  mouse       USB mouse pointer position and buttons");
                kprintln!("  ifconfig    network cards, link and address");
                kprintln!("  ping <ip>   send 4 ICMP echo requests");
                kprintln!("  bt          Bluetooth adapters");
                kprintln!("  bt scan [s] look for Bluetooth devices for s seconds (default 5)");
                kprintln!("  bt pair <address>  pair a Bluetooth gamepad or headset (put it in pairing mode first)");
                kprintln!("  gamepad     Bluetooth and USB gamepads and what they are pressing");
                kprintln!("  mic [record [s]]  Bluetooth headset microphones; record and show level and pitch");
                kprintln!("  sound [test [hz] [s] | use <c>.<o>]  audio outputs; play a test tone; pick the output");
                kprintln!("  disks       disks and partitions");
                kprintln!("  diskwrite <disk> <lba> <n>  write, flush and read back a test pattern (unused blocks only)");
                kprintln!("  ls [path]   list a directory on the mounted disk");
                kprintln!("  cat <path>  print a text file");
                kprintln!("  write <path> <text>  save a line of text as a file (replacing it); quote a path with spaces");
                kprintln!("  mkdir <path>  create a directory");
                kprintln!("  rm <path>   delete a file or an empty directory");
                kprintln!("  wc <path>   size, lines and FNV-1a checksum of a file");
                kprintln!("  mem         buddy allocator, slab heap and paging");
                kprintln!("  irq         device interrupts (MSI-X, MSI) and how often they fired");
                kprintln!("  cpu         processor and SMP status");
                kprintln!("  acpi        firmware tables found");
                kprintln!("  uptime      time since boot");
                kprintln!("  int3        raise a breakpoint exception (and survive it)");
                kprintln!("  panic       trigger a kernel panic on purpose");
            }
            "about" => {
                kprintln!("  AeroForge OS 0.9, AeroKernel (Rust) with C++ drivers over the DHI.");
                kprintln!("  Preemptive multi-core scheduler, ring-3 processes, capability handles");
                kprintln!("  and IPC ports. aerosmss is the first user process.");
            }
            "ps" => {
                kprintln!("  {:>4} {:>4} {:>4}  {:<9} {:>7}  name", "tid", "pid", "cpu", "state", "ticks");
                let threads = sched::THREADS.lock();
                for t in threads.values() {
                    let handles = t.process.as_ref().map_or(0, |p| p.handles.lock().count());
                    kprintln!("  {:>4} {:>4} {:>4}  {:<9} {:>7}  {}{}",
                        t.tid, t.pid(), t.cpu, t.state().name(), t.runtime_ticks.load(Ordering::Relaxed), t.name,
                        if t.process.is_some() { alloc::format!("  ({} handles)", handles) } else { String::new() });
                }
                kprintln!("  {} process(es)", process::PROCESSES.lock().len());
            }
            "sched" => {
                for i in 0..percpu::count() {
                    if let Some(s) = sched::cpu_stats(i) {
                        let ticks = percpu::get(i).map_or(0, |c| c.ticks.load(Ordering::Relaxed));
                        kprintln!("  cpu{}: {} runnable, {} context switches, {} timer ticks, now: {}",
                            i, s.ready, s.switches, ticks, s.current);
                    }
                }
            }
            "run" => match process::spawn(arg, 0) {
                Ok(pid) => kprintln!("  started {} as pid {}", arg, pid),
                Err(e) => console::print_colored(YELLOW, format_args!("  {}: {}\n", arg, e)),
            },
            "ports" => {
                for (name, port) in ipc::NAMES.lock().iter() {
                    kprintln!("  {:<12} port #{}, {} message(s) delivered, {} queued",
                        name, port.id, port.sent.load(Ordering::Relaxed), port.queued());
                }
            }
            "lspci" => {
                for d in pci::devices() {
                    kprintln!("  {:02x}:{:02x}.{}  {:04x}:{:04x}  class {:02x}{:02x}{:02x}  {}",
                        d.bus, d.dev, d.func, d.vendor, d.device, d.class, d.subclass, d.prog_if, d.kind());
                }
            }
            "lsusb" => {
                let ctrls = usb::CONTROLLERS.lock();
                if ctrls.is_empty() {
                    kprintln!("  no USB controllers");
                }
                for c in ctrls.iter() {
                    kprintln!("  xHCI controller {} at {}: {} device(s)", c.id, c.location, c.devices.len());
                    for d in &c.devices {
                        let at = if d.parent_slot == 0 {
                            alloc::format!("port {}", d.port)
                        } else {
                            alloc::format!("hub {} port {}", d.parent_slot, d.port)
                        };
                        kprintln!("    {:<15} slot {:<2} {:04x}:{:04x}  {:<9} {:<13} {}",
                            at, d.slot, d.vendor, d.product, usb::speed_name(d.speed), usb::class_name(d),
                            dhi::c_field(&d.name));
                    }
                }
            }
            "mouse" => {
                use core::sync::atomic::Ordering::Relaxed;
                let b = usb::MOUSE_BUTTONS.load(Relaxed);
                kprintln!("  pointer at ({}, {}), buttons [{}{}{}], {} mouse report(s), {} USB key report(s)",
                    usb::MOUSE_X.load(Relaxed), usb::MOUSE_Y.load(Relaxed),
                    if b & 1 != 0 { 'L' } else { '-' }, if b & 4 != 0 { 'M' } else { '-' }, if b & 2 != 0 { 'R' } else { '-' },
                    usb::MOUSE_EVENTS.load(Relaxed), usb::KEY_EVENTS.load(Relaxed));
            }
            "ifconfig" => {
                let nics = net::NICS.lock();
                if nics.is_empty() {
                    kprintln!("  no network cards");
                }
                for n in nics.iter() {
                    let link = match n.port.link() {
                        Some(speed) => alloc::format!("up, {} Mb/s", speed),
                        None => String::from("down"),
                    };
                    kprintln!("  {}  Intel {:04x}:{:04x} ({} driver) at {}, MAC {}, link {}",
                        n.name, n.pci_id.0, n.pci_id.1, n.driver_name, n.location, net::mac_string(&n.info.mac), link);
                }
                drop(nics);
                if let Some(l) = *net::LEASE.lock() {
                    kprintln!("  eth0  inet {}, gateway {}, DNS {}", l.address, net::opt(l.router), net::opt(l.dns));
                }
                kprintln!("  {} packet(s) received, {} sent, {} receive error(s)",
                    net::RX_PACKETS.load(Ordering::Relaxed), net::TX_PACKETS.load(Ordering::Relaxed),
                    net::RX_ERRORS.load(Ordering::Relaxed));
            }
            "ping" => match arg.parse::<core::net::Ipv4Addr>() {
                Ok(ip) => {
                    if let Err(e) = net::ping(ip, 4) {
                        console::print_colored(YELLOW, format_args!("  ping: {}\n", e));
                    }
                }
                Err(_) => console::print_colored(YELLOW, format_args!("  usage: ping <a.b.c.d>\n")),
            },
            "bt" => match arg.split_once(' ').unwrap_or((arg, "")) {
                ("", _) => {
                    let list = bt::ADAPTERS.lock();
                    if list.is_empty() {
                        kprintln!("  no Bluetooth adapters");
                    }
                    for a in list.iter() {
                        let state = if a.up {
                            alloc::format!("up, Bluetooth {}, address {}, made by {}{}", bt::version_name(a.hci_version),
                                bt::addr_string(&a.address), bt::manufacturer_name(a.manufacturer), if a.le { ", LE" } else { "" })
                        } else if let Some(e) = &a.error {
                            alloc::format!("down: {}", e)
                        } else {
                            String::from("starting")
                        };
                        kprintln!("  {}  USB {:04x}:{:04x}{}  {}", a.name, a.usb.vendor, a.usb.product,
                            a.chip.as_ref().map_or(String::new(), |c| alloc::format!(" ({})", c)), state);
                    }
                    drop(list);
                    for b in bt::BONDS.lock().iter() {
                        kprintln!("  paired: {} \"{}\"", bt::addr_string(&b.address), b.name);
                    }
                    let found = bt::FOUND.lock();
                    if !found.is_empty() {
                        kprintln!("  {} device(s) seen in the last scan, 'bt scan' to refresh", found.len());
                    }
                }
                ("scan", secs) => {
                    let secs = if secs.is_empty() { 5 } else { secs.parse::<u32>().unwrap_or(5).clamp(1, 60) };
                    kprintln!("  scanning for {} s...", secs);
                    match bt::scan(secs) {
                        Ok(()) => {
                            let found = bt::FOUND.lock();
                            for f in found.iter() {
                                kprintln!("  {}", bt::describe(f));
                            }
                            kprintln!("  {} device(s) found", found.len());
                        }
                        Err(e) => console::print_colored(YELLOW, format_args!("  bt: {}\n", e)),
                    }
                }
                ("pair", a) => match bt::parse_addr(a.trim()) {
                    Some(address) => {
                        kprintln!("  pairing with {}...", bt::addr_string(&address));
                        match bt::pair(address) {
                            Ok(what) => kprintln!("  {}", what),
                            Err(e) => console::print_colored(YELLOW, format_args!("  bt pair: {}\n", e)),
                        }
                    }
                    None => console::print_colored(YELLOW, format_args!("  usage: bt pair 11:22:33:44:55:66\n")),
                },
                _ => console::print_colored(YELLOW, format_args!("  usage: bt [scan [seconds] | pair <address>]\n")),
            },
            "gamepad" => {
                let pads = bt::GAMEPADS.lock();
                if pads.is_empty() {
                    kprintln!("  no gamepads; plug one into USB before starting, or pair one with 'bt pair <address>' (see 'bt scan')");
                }
                for g in pads.iter() {
                    if let Some(place) = &g.usb {
                        kprintln!("  \"{}\" on {}, {}, {} report(s)", g.name, place, g.layout, g.reports);
                    } else {
                        let adapter = bt::ADAPTERS.lock().get(g.adapter).map_or(String::from("?"), |a| a.name.clone());
                        kprintln!("  {} \"{}\" on {}, {}, {}, {} report(s)", bt::addr_string(&g.address), g.name, adapter,
                            if g.connected { "connected" } else { "not connected" }, g.layout, g.reports);
                    }
                    let mut buttons = String::new();
                    for n in 0..32 {
                        if g.pad.buttons & (1 << n) != 0 {
                            buttons.push_str(&alloc::format!(" {}", n + 1));
                        }
                    }
                    let mut axes = String::new();
                    for (i, name) in bt::AXIS_NAMES.iter().enumerate().filter(|(i, _)| g.axes & (1 << i) != 0) {
                        axes.push_str(&alloc::format!(" {} {:+}", name, g.pad.axes[i]));
                    }
                    kprintln!("    buttons:{}, hat {}, axes:{}", if buttons.is_empty() { " none" } else { &buttons },
                        bt::hat_name(g.pad.hat), axes);
                }
            }
            "sound" => match arg.split_once(' ').unwrap_or((arg, "")) {
                ("", _) => {
                    let cards = sound::CARDS.lock();
                    if cards.is_empty() {
                        kprintln!("  no audio controllers");
                    }
                    let selected = sound::selected();
                    for (ci, c) in cards.iter().enumerate() {
                        kprintln!("  {}: {} audio {:04x}:{:04x} at {}", c.name, sound::card_name(c.pci_id.0), c.pci_id.0, c.pci_id.1, c.location);
                        for &id in &c.info.codec_ids[..c.info.codec_count as usize] {
                            kprintln!("    codec: {}", sound::codec_name(id));
                        }
                        for (i, o) in c.outputs().iter().enumerate() {
                            let plugged = match o.plugged { 1 => ", plugged in", 0 => ", nothing plugged in", _ => "" };
                            let note = if o.kind == dhi::HDA_HDMI { " (sound needs the display driver, not done yet)" } else { "" };
                            kprintln!("   {} {}.{}  {}{}{}", if selected == Some((ci, i)) { "*" } else { " " }, ci, i,
                                sound::output_name(o), plugged, note);
                        }
                    }
                }
                ("test", rest) => {
                    let mut it = rest.split_whitespace();
                    let hz = it.next().and_then(|v| v.parse::<u32>().ok()).unwrap_or(440).clamp(20, 20_000);
                    let secs = it.next().and_then(|v| v.parse::<u32>().ok()).unwrap_or(1).clamp(1, 10);
                    kprintln!("  playing {} Hz for {} s...", hz, secs);
                    match sound::tone(hz, secs * 1000) {
                        Ok(what) => kprintln!("  played {}", what),
                        Err(e) => console::print_colored(YELLOW, format_args!("  sound: {}\n", e)),
                    }
                }
                ("use", which) => {
                    let parsed = which.trim().split_once('.').and_then(|(c, o)| Some((c.parse::<usize>().ok()?, o.parse::<usize>().ok()?)));
                    match parsed.map(|(c, o)| sound::select(c, o)) {
                        Some(Ok(())) => kprintln!("  sound now goes to {}", which.trim()),
                        Some(Err(e)) => console::print_colored(YELLOW, format_args!("  sound: {}\n", e)),
                        None => console::print_colored(YELLOW, format_args!("  usage: sound use <card>.<output>, e.g. sound use 0.1\n")),
                    }
                }
                _ => console::print_colored(YELLOW, format_args!("  usage: sound [test [hz] [seconds] | use <card>.<output>]\n")),
            },
            "mic" => match arg.split_once(' ').unwrap_or((arg, "")) {
                ("", _) => {
                    let list = bt::HEADSETS.lock();
                    if list.is_empty() {
                        kprintln!("  no microphones; pair a Bluetooth headset with 'bt pair <address>' (see 'bt scan')");
                    }
                    for h in list.iter() {
                        let adapter = bt::ADAPTERS.lock().get(h.adapter).map_or(String::from("?"), |a| a.name.clone());
                        kprintln!("  {} \"{}\" on {}, Bluetooth {}, {}", bt::addr_string(&h.address), h.name, adapter, h.profile,
                            if h.connected { "connected" } else { "not connected" });
                    }
                }
                ("record", secs) => {
                    let secs = if secs.is_empty() { 3 } else { secs.trim().parse::<u32>().unwrap_or(3).clamp(1, 30) };
                    kprintln!("  recording {} s...", secs);
                    match bt::record(secs) {
                        Ok(r) => {
                            let pitch = if r.pitch > 0 { alloc::format!(", pitch about {} Hz", r.pitch) } else { String::from(", silence") };
                            kprintln!("  \"{}\": {}.{} s, {} samples at 8 kHz, peak {}%, RMS {}%{}", r.name, r.tenths / 10,
                                r.tenths % 10, r.samples, r.peak, r.rms, pitch);
                        }
                        Err(e) => console::print_colored(YELLOW, format_args!("  mic: {}\n", e)),
                    }
                }
                _ => console::print_colored(YELLOW, format_args!("  usage: mic [record [seconds]]\n")),
            },
            "disks" => {
                for d in block::DEVICES.lock().iter() {
                    match vfs::mount_point_of(d.name()) {
                        Some(at) => kprintln!("  {:<9} {}  [mounted at {}]", d.name(), d.describe(), at),
                        None => kprintln!("  {:<9} {}", d.name(), d.describe()),
                    }
                }
            }
            // A path with spaces goes in double quotes.
            "write" => match arg.strip_prefix('"').and_then(|a| a.split_once("\" ")).or_else(|| arg.split_once(' ')) {
                Some((path, text)) => {
                    let data = alloc::format!("{}\n", text);
                    match vfs::write(path, data.as_bytes()) {
                        Ok(()) => kprintln!("  wrote {} bytes to {}", data.len(), path),
                        Err(e) => console::print_colored(YELLOW, format_args!("  {}: {}\n", path, e)),
                    }
                }
                None => console::print_colored(YELLOW, format_args!("  usage: write <path> <text>\n")),
            },
            "mkdir" => match vfs::create_dir(arg) {
                Ok(()) => kprintln!("  created directory {}", arg),
                Err(e) => console::print_colored(YELLOW, format_args!("  {}: {}\n", arg, e)),
            },
            "rm" => match vfs::remove(arg) {
                Ok(()) => kprintln!("  deleted {}", arg),
                Err(e) => console::print_colored(YELLOW, format_args!("  {}: {}\n", arg, e)),
            },
            "diskwrite" => {
                let mut args = arg.split_whitespace();
                let dev = args.next().unwrap_or("");
                let lba = args.next().and_then(|a| a.parse().ok());
                let count = args.next().and_then(|a| a.parse().ok());
                match (lba, count) {
                    (Some(lba), Some(count)) => match block::write_test(dev, lba, count) {
                        Ok(msg) => kprintln!("  {}", msg),
                        Err(e) => console::print_colored(YELLOW, format_args!("  diskwrite: {}\n", e)),
                    },
                    _ => console::print_colored(YELLOW, format_args!("  usage: diskwrite <disk> <lba> <blocks>\n")),
                }
            }
            "ls" => {
                let path = if arg.is_empty() { "/" } else { arg };
                match vfs::list(path) {
                    Ok(entries) => {
                        for e in entries {
                            if e.is_dir {
                                console::print_colored(CYAN, format_args!("  {:>9}  {}/\n", "<dir>", e.name));
                            } else {
                                kprintln!("  {:>9}  {}", e.size, e.name);
                            }
                        }
                    }
                    Err(e) => console::print_colored(YELLOW, format_args!("  {}: {}\n", path, e)),
                }
            }
            "cat" => match vfs::read(arg, 16 * 1024) {
                Ok(data) => {
                    let text = core::str::from_utf8(&data).unwrap_or("<binary file>");
                    for line in text.lines() {
                        kprintln!("  {}", line);
                    }
                }
                Err(e) => console::print_colored(YELLOW, format_args!("  {}: {}\n", arg, e)),
            },
            "wc" => match vfs::read(arg, usize::MAX) {
                Ok(data) => {
                    let lines = data.iter().filter(|&&b| b == b'\n').count();
                    let hash = data.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3));
                    kprintln!("  {} bytes, {} lines, fnv1a {:016x}  {}", data.len(), lines, hash, arg);
                }
                Err(e) => console::print_colored(YELLOW, format_args!("  {}: {}\n", arg, e)),
            },
            "irq" => {
                let sources = crate::msi::sources();
                if sources.is_empty() {
                    kprintln!("  no device interrupts (everything is polled)");
                }
                for (vector, name, kind, cpu, count) in sources {
                    kprintln!("  vector {:#04x}  {:<5} -> cpu{}  {:>8} interrupts  {}", vector, kind, cpu, count, name);
                }
            }
            "mem" => {
                let (total, free) = {
                    let b = memory::BUDDY.lock();
                    (b.total_pages, b.free_pages)
                };
                let (used, slab, large) = memory::heap_stats();
                kprintln!("  physical  {} MiB managed, {} MiB free ({} pages in use)",
                    total * 4 / 1024, free * 4 / 1024, total - free);
                kprintln!("  heap      {} KiB in slab objects ({} KiB of slab pages), {} KiB large blocks",
                    used / 1024, slab / 1024, large / 1024);
                kprintln!("  HHDM {:#x}, kernel PML4 {:#x}, active CR3 {:#x}",
                    memory::hhdm_offset(), memory::kernel_pml4(), arch::read_cr3());
                kprintln!("  {} TLB shootdowns since boot", crate::tlb::count());
            }
            "cpu" => {
                let mut buf = [0u8; 48];
                kprintln!("  {}", arch::cpu_brand(&mut buf));
                kprintln!("  {} CPU(s) online, shell on cpu{}", smp::ONLINE.load(Ordering::Relaxed), percpu::this().index);
            }
            "acpi" => match acpi::INFO.lock().as_ref() {
                Some(info) => {
                    kprint!("  ACPI rev {}, tables:", info.revision);
                    for t in &info.tables {
                        kprint!(" {}", core::str::from_utf8(t).unwrap_or("????"));
                    }
                    kprintln!();
                    kprintln!("  MADT: {} local APIC(s), {} I/O APIC(s) at {:#x}, {} IRQ override(s)",
                        info.lapic_count, info.ioapic_count, info.ioapic_address, info.overrides.len());
                }
                None => kprintln!("  ACPI not initialised"),
            },
            "uptime" => {
                let t = sched::ticks();
                kprintln!("  {}.{:02} s ({} ticks at {} Hz)", t / apic::TIMER_HZ, t % apic::TIMER_HZ, t, apic::TIMER_HZ);
            }
            "int3" => unsafe { core::arch::asm!("int3") },
            "panic" => panic!("panic requested from the shell"),
            other => console::print_colored(YELLOW, format_args!("  unknown command '{}', try 'help'\n", other)),
        }
    }
}

fn program_list() -> String {
    let mut s = String::new();
    for (i, m) in modules::programs().enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        s.push_str(&m.name);
    }
    s
}
