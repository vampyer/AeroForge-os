//! USB: finds xHCI controllers, brings them up through the C++ driver and
//! polls them from a kernel thread. Keyboard input joins the PS/2 key queue,
//! mouse motion updates a pointer position (no cursor is drawn yet), and
//! gamepad reports update the shared gamepad list (bt::GAMEPADS), decoded
//! with the same HID parser as Bluetooth gamepads.

use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, Ordering};

use crate::bt::{self, hid};
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

/// A USB gamepad's report layout; its state lives in bt::GAMEPADS under `location`.
struct Pad {
    ctrl: i32,
    index: i32,
    location: String,
    layout: hid::Layout,
}

static PADS: IrqMutex<Vec<Pad>> = IrqMutex::new(Vec::new());

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
        let location = alloc::format!("{:02x}:{:02x}.{}", dev.bus, dev.dev, dev.func);
        sync_pads(id, &location, false);
        CONTROLLERS.lock().push(Controller { id, location, devices: read_devices(id) });
        ctrls += 1;
        total += count as usize;
    }
    (ctrls, total)
}

/// The controller's devices (entries of unplugged ones are skipped).
fn read_devices(ctrl: i32) -> Vec<UsbDevice> {
    let mut devices = Vec::new();
    for i in 0.. {
        let mut d = UsbDevice::zeroed();
        match unsafe { dhi::aero_xhci_device(ctrl, i, &mut d) } {
            0 => devices.push(d),
            1 => {}
            _ => break,
        }
    }
    devices
}

/// Brings the gamepad list up to date with the controller's gamepads:
/// adds new ones (a pad plugged in again where it was before keeps its
/// entry) and marks unplugged ones as not connected.
fn sync_pads(ctrl: i32, location: &str, hotplug: bool) {
    let mut desc = alloc::vec![0u8; 1024];
    for index in 0.. {
        let mut d = UsbDevice::zeroed();
        let n = unsafe { dhi::aero_xhci_pad(ctrl, index, &mut d, desc.as_mut_ptr(), desc.len() as u32) };
        if n == -1 {
            break;
        }
        let known = PADS.lock().iter().position(|p| p.ctrl == ctrl && p.index == index);
        if n < 0 {
            if let Some(i) = known {
                let pad = PADS.lock().remove(i);
                let mut gamepads = bt::GAMEPADS.lock();
                if let Some(g) = gamepads.iter_mut().find(|g| g.usb.as_deref() == Some(pad.location.as_str())) {
                    g.connected = false;
                    g.pad = hid::Pad::default();
                    let name = g.name.clone();
                    drop(gamepads);
                    crate::kok!("gamepad \"{}\" unplugged from {}", name, pad.location);
                }
            }
            continue;
        }
        if known.is_some() {
            continue;
        }
        let layout = hid::parse(&desc[..n as usize]);
        let name = String::from(dhi::c_field(&d.name));
        let place = if d.parent_slot != 0 {
            alloc::format!("USB {} hub {} port {}", location, d.parent_slot, d.port)
        } else {
            alloc::format!("USB {} port {}", location, d.port)
        };
        let xinput = d.iface_class == 0xFF;
        let kind = if xinput { "Xbox 360 style " } else { "" };
        crate::kok!("{}gamepad \"{}\" {} {} ({})", kind, name, if hotplug { "plugged in on" } else { "on" }, place,
            layout.summary());
        {
            let mut gamepads = bt::GAMEPADS.lock();
            let entry = bt::Gamepad { adapter: usize::MAX, address: [0; 6], name, connected: true,
                layout: layout.summary(), axes: layout.axis_mask(), reports: 0, pad: hid::Pad::default(),
                usb: Some(place.clone()), xinput };
            match gamepads.iter_mut().find(|g| g.usb.as_deref() == Some(place.as_str())) {
                Some(g) => *g = entry,
                None => gamepads.push(entry),
            }
        }
        PADS.lock().push(Pad { ctrl, index, location: place, layout });
    }
}

/// Decodes the controller's waiting gamepad reports.
fn drain_pads(ctrl: i32) {
    let mut report = [0u8; 64];
    loop {
        let mut index = 0i32;
        let n = unsafe { dhi::aero_xhci_pad_report(ctrl, &mut index, report.as_mut_ptr(), report.len() as u32) };
        if n <= 0 {
            break;
        }
        let pads = PADS.lock();
        let Some(p) = pads.iter().find(|p| p.ctrl == ctrl && p.index == index) else { continue };
        let mut gamepads = bt::GAMEPADS.lock();
        if let Some(g) = gamepads.iter_mut().find(|g| g.usb.as_deref() == Some(p.location.as_str())) {
            if hid::decode(&p.layout, &report[..n as usize], &mut g.pad) {
                g.reports += 1;
            }
        }
    }
}

/// Kernel thread: drains every controller's events about every 10 ms.
pub fn poll_thread(_: u64) {
    let ids: Vec<i32> = CONTROLLERS.lock().iter().map(|c| c.id).collect();
    let mut generations: Vec<u32> = ids.iter().map(|&id| unsafe { dhi::aero_xhci_generation(id) }).collect();
    let (w, h) = console::CONSOLE.lock().screen_size().unwrap_or((1280, 800));
    MOUSE_X.store(w as i32 / 2, Ordering::Relaxed);
    MOUSE_Y.store(h as i32 / 2, Ordering::Relaxed);
    let mut events = [InputEvent::default(); 32];
    loop {
        for (k, &id) in ids.iter().enumerate() {
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
            let generation = unsafe { dhi::aero_xhci_generation(id) };
            if generations[k] != generation {
                // Something was plugged in or unplugged.
                generations[k] = generation;
                let location = CONTROLLERS.lock().iter().find(|c| c.id == id).map(|c| c.location.clone());
                if let Some(location) = location {
                    sync_pads(id, &location, true);
                }
                let devices = read_devices(id);
                if let Some(c) = CONTROLLERS.lock().iter_mut().find(|c| c.id == id) {
                    c.devices = devices;
                }
            }
            if PADS.lock().iter().any(|p| p.ctrl == id) {
                drain_pads(id);
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
        6 => "20 Gb/s",
        _ => "?",
    }
}

pub fn class_name(d: &UsbDevice) -> &'static str {
    match (d.iface_class, d.iface_subclass, d.iface_protocol) {
        (3, 1, 1) => "HID keyboard",
        (3, 1, 2) => "HID mouse",
        (3, _, _) => "HID",
        (0xFF, 0x5D, 1) => "gamepad (Xbox 360 style)",
        (8, _, _) => "mass storage",
        (9, _, _) => "hub",
        (1, _, _) => "audio",
        (0xE0, _, _) => "wireless (Bluetooth)",
        _ => "other",
    }
}
