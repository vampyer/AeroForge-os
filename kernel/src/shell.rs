//! AeroShell (kernel edition): a tiny line-based command prompt that runs
//! inside the kernel until user mode and a real shell exist.

use alloc::string::String;
use core::sync::atomic::Ordering;

use crate::console::{self, CYAN, YELLOW};
use crate::{acpi, arch, interrupts, kprint, kprintln, memory, pic, smp};

pub struct Shell {
    line: String,
}

impl Shell {
    pub fn new() -> Self {
        Self { line: String::new() }
    }

    pub fn prompt(&self) {
        console::print_colored(CYAN, format_args!("aero"));
        kprint!("> ");
    }

    pub fn on_key(&mut self, c: u8) {
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
        match cmd {
            "" => {}
            "help" => {
                kprintln!("  help     this list");
                kprintln!("  about    what AeroForge is");
                kprintln!("  mem      physical memory, heap and paging");
                kprintln!("  cpu      processor and SMP status");
                kprintln!("  acpi     firmware tables found");
                kprintln!("  uptime   timer ticks since boot");
                kprintln!("  int3     raise a breakpoint exception (and survive it)");
                kprintln!("  panic    trigger a kernel panic on purpose");
            }
            "about" => {
                kprintln!("  AeroForge OS 0.1, AeroKernel (Rust) with C++ drivers over the DHI.");
                kprintln!("  Phase 1 milestone: boots via Limine on UEFI, interrupts, memory, SMP.");
            }
            "mem" => {
                let fa = memory::FRAMES.lock();
                let (used, size) = memory::heap_stats();
                kprintln!("  usable RAM     {} MiB", fa.total_bytes / (1024 * 1024));
                kprintln!("  frames in use  {} ({} KiB)", fa.allocated, fa.allocated * 4);
                kprintln!("  kernel heap    {} / {} KiB at {:#x}", used / 1024, size / 1024, memory::HEAP_START);
                kprintln!("  HHDM offset    {:#x}", memory::hhdm_offset());
                kprintln!("  CR3            {:#x}", arch::read_cr3());
                let probe = memory::HEAP_START;
                match memory::translate(probe) {
                    Some(phys) => kprintln!("  page walk      heap {:#x} -> phys {:#x}", probe, phys),
                    None => kprintln!("  page walk      heap {:#x} is not mapped", probe),
                }
            }
            "cpu" => {
                let mut buf = [0u8; 48];
                kprintln!("  {}", arch::cpu_brand(&mut buf));
                kprintln!("  {} CPU(s) online", smp::ONLINE.load(Ordering::Relaxed));
            }
            "acpi" => match acpi::INFO.lock().as_ref() {
                Some(info) => {
                    kprint!("  ACPI rev {}, tables:", info.revision);
                    for t in &info.tables {
                        kprint!(" {}", core::str::from_utf8(t).unwrap_or("????"));
                    }
                    kprintln!();
                    kprintln!("  MADT: {} local APIC(s), {} I/O APIC(s), LAPIC at {:#x}",
                        info.lapic_count, info.ioapic_count, info.lapic_address);
                }
                None => kprintln!("  ACPI not initialised"),
            },
            "uptime" => {
                let t = interrupts::TICKS.load(Ordering::Relaxed);
                kprintln!("  {} ticks at {} Hz = {}.{:02} s", t, pic::TIMER_HZ, t / pic::TIMER_HZ as u64, t % pic::TIMER_HZ as u64);
            }
            "int3" => unsafe { core::arch::asm!("int3") },
            "panic" => panic!("panic requested from the shell"),
            other => console::print_colored(YELLOW, format_args!("  unknown command '{}', try 'help'\n", other)),
        }
    }
}
