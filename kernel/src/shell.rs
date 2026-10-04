//! AeroShell (kernel edition): a line-based command prompt running as a
//! kernel thread on cpu0, until a user-mode shell exists.

use alloc::string::String;
use core::sync::atomic::Ordering;

use crate::console::{self, CYAN, YELLOW};
use crate::{acpi, apic, arch, interrupts, ipc, kprint, kprintln, memory, modules, percpu, process, sched, smp};

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
                kprintln!("  mem         buddy allocator, slab heap and paging");
                kprintln!("  cpu         processor and SMP status");
                kprintln!("  acpi        firmware tables found");
                kprintln!("  uptime      time since boot");
                kprintln!("  int3        raise a breakpoint exception (and survive it)");
                kprintln!("  panic       trigger a kernel panic on purpose");
            }
            "about" => {
                kprintln!("  AeroForge OS 0.2, AeroKernel (Rust) with C++ drivers over the DHI.");
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
