//! USB: finds xHCI controllers, brings them up through the C++ driver and
//! polls them from a kernel thread. Keyboard input joins the PS/2 key queue,
//! mouse motion updates a pointer position (no cursor is drawn yet).

use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, Ordering};

use crate::dhi::{self, InputEvent, UsbDevice};
use crate::sync::IrqMutex;
use crate::{console, interrupts, pci, sched};

pub struct Controller {
    pub id: i32,
    pub location: String,
    pub devices: Vec<UsbDevice>,
}

pub static CONTROLLERS: IrqMutex<Vec<Controller>> = IrqMutex::new(Vec::new());

pub static MOUSE_X: AtomicI32 = AtomicI32::new(0);
pub static MOUSE_Y: AtomicI32 = AtomicI32::new(0);
pub static MOUSE_BUTTONS: AtomicU32 = AtomicU32::new(0);
pub static MOUSE_EVENTS: AtomicU64 = AtomicU64::new(0);
pub static KEY_EVENTS: AtomicU64 = AtomicU64::new(0);

/// Brings up every xHCI controller. Returns (controllers, devices).
pub fn probe() -> (usize, usize) {
    let (mut ctrls, mut total) = (0, 0);
    for dev in pci::devices().iter().filter(|d| d.class == 0x0C && d.subclass == 0x03 && d.prog_if == 0x30) {
        let Some(bar0) = dev.bar(0) else { continue };
        dev.enable_mmio_and_dma();
        let mut count = 0i32;
        let id = unsafe { dhi::aero_xhci_init(&dhi::OPS, bar0, &mut count) };
        if id < 0 {
            console::print_colored(console::YELLOW, format_args!(
                "[WARN] xHCI at {:02x}:{:02x}.{}: init failed ({})\n", dev.bus, dev.dev, dev.func, id));
            continue;
        }
        let devices = (0..count)
            .filter_map(|i| {
                let mut d = UsbDevice::zeroed();
                (unsafe { dhi::aero_xhci_device(id, i, &mut d) } == 0).then_some(d)
            })
            .collect();
        CONTROLLERS.lock().push(Controller {
            id,
            location: alloc::format!("{:02x}:{:02x}.{}", dev.bus, dev.dev, dev.func),
            devices,
        });
        ctrls += 1;
        total += count as usize;
    }
    (ctrls, total)
}

/// Kernel thread: drains every controller's events about every 10 ms.
pub fn poll_thread(_: u64) {
    let ids: Vec<i32> = CONTROLLERS.lock().iter().map(|c| c.id).collect();
    let (w, h) = console::CONSOLE.lock().screen_size().unwrap_or((1280, 800));
    MOUSE_X.store(w as i32 / 2, Ordering::Relaxed);
    MOUSE_Y.store(h as i32 / 2, Ordering::Relaxed);
    let mut events = [InputEvent::default(); 32];
    loop {
        for &id in &ids {
            let n = unsafe { dhi::aero_xhci_poll(id, events.as_mut_ptr(), events.len() as i32) };
            for ev in &events[..n.max(0) as usize] {
                match ev.kind {
                    dhi::INPUT_KEY => {
                        KEY_EVENTS.fetch_add(1, Ordering::Relaxed);
                        if ev.key.pressed == 1 && ev.key.ascii != 0 {
                            interrupts::push_key(ev.key.ascii);
                        }
                    }
                    dhi::INPUT_MOUSE => {
                        MOUSE_EVENTS.fetch_add(1, Ordering::Relaxed);
                        let x = (MOUSE_X.load(Ordering::Relaxed) + ev.dx as i32).clamp(0, w as i32 - 1);
                        let y = (MOUSE_Y.load(Ordering::Relaxed) + ev.dy as i32).clamp(0, h as i32 - 1);
                        MOUSE_X.store(x, Ordering::Relaxed);
                        MOUSE_Y.store(y, Ordering::Relaxed);
                        MOUSE_BUTTONS.store(ev.buttons as u32, Ordering::Relaxed);
                    }
                    _ => {}
                }
            }
        }
        sched::sleep_ticks(1);
    }
}

pub fn speed_name(speed: u8) -> &'static str {
    match speed {
        1 => "12 Mb/s",
        2 => "1.5 Mb/s",
        3 => "480 Mb/s",
        4 => "5 Gb/s",
        5 => "10 Gb/s",
        _ => "?",
    }
}

pub fn class_name(d: &UsbDevice) -> &'static str {
    match (d.iface_class, d.iface_subclass, d.iface_protocol) {
        (3, 1, 1) => "HID keyboard",
        (3, 1, 2) => "HID mouse",
        (3, _, _) => "HID",
        (8, _, _) => "mass storage",
        (9, _, _) => "hub",
        (1, _, _) => "audio",
        (0xE0, _, _) => "wireless (Bluetooth)",
        _ => "other",
    }
}
