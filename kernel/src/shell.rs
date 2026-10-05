//! AeroShell (kernel edition): a line-based command prompt running as a
//! kernel thread on cpu0, until a user-mode shell exists.

use alloc::string::String;
use core::sync::atomic::Ordering;

use crate::console::{self, CYAN, YELLOW};
use crate::{acpi, apic, arch, block, dhi, interrupts, ipc, kprint, kprintln, memory, modules, net, pci, percpu, process, sched, smp, usb, vfs};

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
                kprintln!("  disks       disks and partitions");
                kprintln!("  ls [path]   list a directory on the mounted disk");
                kprintln!("  cat <path>  print a text file");
                kprintln!("  wc <path>   size, lines and FNV-1a checksum of a file");
                kprintln!("  mem         buddy allocator, slab heap and paging");
                kprintln!("  cpu         processor and SMP status");
                kprintln!("  acpi        firmware tables found");
                kprintln!("  uptime      time since boot");
                kprintln!("  int3        raise a breakpoint exception (and survive it)");
                kprintln!("  panic       trigger a kernel panic on purpose");
            }
            "about" => {
                kprintln!("  AeroForge OS 0.6, AeroKernel (Rust) with C++ drivers over the DHI.");
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
                    let link = match net::link(n.id) {
                        Some(speed) => alloc::format!("up, {} Mb/s", speed),
                        None => String::from("down"),
                    };
                    kprintln!("  {}  Intel {:04x}:{:04x} at {}, MAC {}, link {}",
                        n.name, n.pci_id.0, n.pci_id.1, n.location, net::mac_string(&n.info.mac), link);
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
            "disks" => {
                for d in block::DEVICES.lock().iter() {
                    match vfs::mount_point_of(d.name()) {
                        Some(at) => kprintln!("  {:<9} {}  [mounted at {}]", d.name(), d.describe(), at),
                        None => kprintln!("  {:<9} {}", d.name(), d.describe()),
                    }
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
    for (i, m) in modules::list().iter().enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        s.push_str(&m.name);
    }
    s
}
