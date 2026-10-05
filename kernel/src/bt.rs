//! Bluetooth host: finds USB Bluetooth adapters through the xHCI driver,
//! loads MediaTek firmware where the adapter needs it, brings each adapter
//! up over HCI and scans for classic (inquiry) and Bluetooth LE devices.
//! A kernel thread owns the adapters; the shell talks to it through jobs.

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;

use crate::dhi::{self, UsbDevice};
use crate::sync::IrqMutex;
use crate::{apic, console, modules, sched, usb};

pub struct Adapter {
    pub name: String,
    pub id: i32,
    pub usb: UsbDevice,
    pub chip: Option<String>,
    pub up: bool,
    pub error: Option<String>,
    pub address: [u8; 6],
    pub hci_version: u8,
    pub manufacturer: u16,
    pub le: bool,
    pub acl_mtu: u16,
    pub acl_packets: u16,
}

#[derive(Clone)]
pub struct Found {
    pub adapter: usize,
    pub address: [u8; 6],
    pub le: bool,
    pub random: bool,
    pub rssi: Option<i8>,
    pub class: u32,
    pub appearance: u16,
    pub hid: bool,
    pub name: String,
}

struct ScanJob {
    seconds: u32,
    done: bool,
}

pub static ADAPTERS: IrqMutex<Vec<Adapter>> = IrqMutex::new(Vec::new());
pub static FOUND: IrqMutex<Vec<Found>> = IrqMutex::new(Vec::new());
static SCAN: IrqMutex<Option<ScanJob>> = IrqMutex::new(None);

const BOOT_SCAN_SECONDS: u32 = 3;

/// Lists the Bluetooth adapters the xHCI driver set up. Returns how many.
pub fn probe() -> usize {
    let ctrls: Vec<i32> = usb::CONTROLLERS.lock().iter().map(|c| c.id).collect();
    let mut list = ADAPTERS.lock();
    for ctrl in ctrls {
        for i in 0.. {
            let mut d = UsbDevice::zeroed();
            let id = unsafe { dhi::aero_xhci_bt(ctrl, i, &mut d) };
            if id < 0 {
                break;
            }
            let name = alloc::format!("hci{}", list.len());
            list.push(Adapter {
                name,
                id,
                usb: d,
                chip: None,
                up: false,
                error: None,
                address: [0; 6],
                hci_version: 0,
                manufacturer: 0,
                le: false,
                acl_mtu: 0,
                acl_packets: 0,
            });
        }
    }
    list.len()
}

/// Kernel thread: brings every adapter up, runs one short scan, then serves
/// scan requests from the shell and keeps draining events.
pub fn bt_thread(_: u64) {
    let count = ADAPTERS.lock().len();
    let mut hosts: Vec<Host> = (0..count).map(Host::new).collect();
    for h in hosts.iter_mut() {
        let result = h.bring_up();
        let mut list = ADAPTERS.lock();
        let a = &mut list[h.index];
        match result {
            Ok(()) => {
                a.up = true;
                let text = alloc::format!("{}: Bluetooth {} adapter {:04x}:{:04x} up, address {}, made by {}{}, ACL {} x {} bytes",
                    a.name, version_name(a.hci_version), a.usb.vendor, a.usb.product, addr_string(&a.address),
                    manufacturer_name(a.manufacturer), if a.le { ", LE supported" } else { "" }, a.acl_packets, a.acl_mtu);
                drop(list);
                crate::kok!("{}", text);
            }
            Err(e) => {
                let name = a.name.clone();
                a.error = Some(e.clone());
                drop(list);
                console::print_colored(console::YELLOW, format_args!("[WARN] {}: {}\n", name, e));
            }
        }
    }
    for h in hosts.iter_mut().filter(|h| h.up) {
        h.scan(BOOT_SCAN_SECONDS);
        let found = FOUND.lock().iter().filter(|f| f.adapter == h.index).count();
        crate::kok!("{}: scan found {} device(s)", h.name, found);
        for f in FOUND.lock().iter().filter(|f| f.adapter == h.index) {
            console::print_colored(console::DIM, format_args!("       {}\n", describe(f)));
        }
    }
    loop {
        let job = SCAN.lock().as_ref().filter(|j| !j.done).map(|j| j.seconds);
        if let Some(seconds) = job {
            for h in hosts.iter_mut().filter(|h| h.up) {
                h.scan(seconds);
            }
            if let Some(j) = SCAN.lock().as_mut() {
                j.done = true;
            }
        }
        for h in hosts.iter_mut().filter(|h| h.up) {
            h.pump();
            while let Some(ev) = h.events.pop_front() {
                h.on_event(&ev);
            }
        }
        sched::sleep_ticks(2);
    }
}

/// Runs a scan of `seconds` on every adapter (from the shell) and waits.
pub fn scan(seconds: u32) -> Result<(), &'static str> {
    if !ADAPTERS.lock().iter().any(|a| a.up) {
        return Err("no Bluetooth adapter is up");
    }
    {
        let mut job = SCAN.lock();
        if job.as_ref().is_some_and(|j| !j.done) {
            return Err("a scan is already running");
        }
        FOUND.lock().clear();
        *job = Some(ScanJob { seconds, done: false });
    }
    while SCAN.lock().as_ref().is_some_and(|j| !j.done) {
        sched::sleep_ticks(10);
    }
    Ok(())
}

// ------------------------------------------------------------ one adapter

const OP_INQUIRY: u16 = 0x0401;
const OP_INQUIRY_CANCEL: u16 = 0x0402;
const OP_SET_EVENT_MASK: u16 = 0x0C01;
const OP_RESET: u16 = 0x0C03;
const OP_WRITE_LOCAL_NAME: u16 = 0x0C13;
const OP_WRITE_INQUIRY_MODE: u16 = 0x0C45;
const OP_READ_LOCAL_VERSION: u16 = 0x1001;
const OP_READ_LOCAL_FEATURES: u16 = 0x1003;
const OP_READ_BUFFER_SIZE: u16 = 0x1005;
const OP_READ_BD_ADDR: u16 = 0x1009;
const OP_LE_SET_EVENT_MASK: u16 = 0x2001;
const OP_LE_READ_BUFFER_SIZE: u16 = 0x2002;
const OP_LE_SET_SCAN_PARAMS: u16 = 0x200B;
const OP_LE_SET_SCAN_ENABLE: u16 = 0x200C;

const EV_INQUIRY_COMPLETE: u8 = 0x01;
const EV_INQUIRY_RESULT: u8 = 0x02;
const EV_COMMAND_COMPLETE: u8 = 0x0E;
const EV_COMMAND_STATUS: u8 = 0x0F;
const EV_INQUIRY_RESULT_RSSI: u8 = 0x22;
const EV_EXTENDED_INQUIRY_RESULT: u8 = 0x2F;
const EV_LE_META: u8 = 0x3E;
const LE_ADVERTISING_REPORT: u8 = 0x02;

const COMMAND_TIMEOUT: u64 = 2 * apic::TIMER_HZ;

struct Host {
    index: usize,
    id: i32,
    name: String,
    up: bool,
    evt: Vec<u8>,
    acl: Vec<u8>,
    events: VecDeque<Vec<u8>>,
    inquiry_done: bool,
}

impl Host {
    fn new(index: usize) -> Self {
        let (id, name) = {
            let list = ADAPTERS.lock();
            (list[index].id, list[index].name.clone())
        };
        Host { index, id, name, up: false, evt: Vec::new(), acl: Vec::new(), events: VecDeque::new(), inquiry_done: false }
    }

    fn bring_up(&mut self) -> Result<(), String> {
        let usb = ADAPTERS.lock()[self.index].usb;
        if unsafe { dhi::aero_btmtk_is_mediatek(usb.vendor, usb.product) } != 0 {
            self.mediatek()?;
        }
        self.drain();

        self.cmd(OP_RESET, &[])?;
        let v = self.cmd(OP_READ_LOCAL_VERSION, &[])?;
        let a = self.cmd(OP_READ_BD_ADDR, &[])?;
        let f = self.cmd(OP_READ_LOCAL_FEATURES, &[])?;
        let b = self.cmd(OP_READ_BUFFER_SIZE, &[])?;
        if v.len() < 9 || a.len() < 7 || f.len() < 9 || b.len() < 8 {
            return Err(String::from("short reply to an HCI command"));
        }
        let le = f[1 + 4] & 0x40 != 0;
        // Linux's default mask: every classic event plus LE meta.
        self.cmd(OP_SET_EVENT_MASK, &0x3DBF_F807_FFFB_FFFFu64.to_le_bytes())?;
        self.cmd(OP_WRITE_INQUIRY_MODE, &[2])?; // results with RSSI and extended data
        let mut name = [0u8; 248];
        name[..9].copy_from_slice(b"AeroForge");
        self.cmd(OP_WRITE_LOCAL_NAME, &name)?;
        let (mut mtu, mut packets) = (u16::from_le_bytes([b[1], b[2]]), u16::from_le_bytes([b[4], b[5]]));
        if le {
            self.cmd(OP_LE_SET_EVENT_MASK, &0x1Fu64.to_le_bytes())?;
            let lb = self.cmd(OP_LE_READ_BUFFER_SIZE, &[])?;
            if mtu == 0 && lb.len() >= 4 {
                (mtu, packets) = (u16::from_le_bytes([lb[1], lb[2]]), lb[3] as u16);
            }
        }

        let mut list = ADAPTERS.lock();
        let ad = &mut list[self.index];
        ad.hci_version = v[1];
        ad.manufacturer = u16::from_le_bytes([v[5], v[6]]);
        ad.address.copy_from_slice(&a[1..7]);
        ad.le = le;
        ad.acl_mtu = mtu;
        ad.acl_packets = packets;
        self.up = true;
        Ok(())
    }

    /// MT7921/MT7922: download the firmware patch and switch Bluetooth on.
    fn mediatek(&mut self) -> Result<(), String> {
        let mut chip = dhi::BtmtkChip { dev_id: 0, fw_version: 0, flavor: 0, supported: 0, firmware: [0; 64] };
        if unsafe { dhi::aero_btmtk_chip(&dhi::OPS, self.id, &mut chip) } != 0 {
            return Err(String::from("MediaTek adapter did not report its chip id"));
        }
        let file = dhi::c_field(&chip.firmware);
        let model = alloc::format!("MediaTek MT{:04x}", chip.dev_id & 0xFFFF);
        ADAPTERS.lock()[self.index].chip = Some(model.clone());
        if chip.supported == 0 {
            return Err(alloc::format!("{} is not supported yet", model));
        }
        let Some(fw) = modules::firmware(file) else {
            return Err(alloc::format!("{} needs {}, which is not on the boot volume", model, file));
        };
        let mut r = dhi::BtmtkResult { sections: 0, bytes: 0, error: [0; 48] };
        if unsafe { dhi::aero_btmtk_setup(&dhi::OPS, self.id, fw.as_ptr(), fw.len() as u32, &mut r) } != 0 {
            return Err(alloc::format!("{} firmware load failed: {}", model, dhi::c_field(&r.error)));
        }
        crate::kok!("{}: {} firmware {} loaded ({} section(s), {} bytes)", self.name, model, file, r.sections, r.bytes);
        Ok(())
    }

    /// Throws away whatever arrived during vendor set-up.
    fn drain(&mut self) {
        self.pump();
        self.events.clear();
        self.evt.clear();
        self.acl.clear();
    }

    /// Moves received chunks into the reassembly buffers and splits off
    /// complete events.
    fn pump(&mut self) {
        let mut buf = [0u8; 1024];
        loop {
            let mut kind = 0u8;
            let n = unsafe { dhi::aero_xhci_bt_recv(self.id, &mut kind, buf.as_mut_ptr(), buf.len() as u32) };
            if n <= 0 {
                break;
            }
            match kind {
                dhi::BT_EVENT => self.evt.extend_from_slice(&buf[..n as usize]),
                dhi::BT_ACL => self.acl.extend_from_slice(&buf[..n as usize]),
                _ => {}
            }
        }
        while self.evt.len() >= 2 && self.evt.len() >= 2 + self.evt[1] as usize {
            let len = 2 + self.evt[1] as usize;
            self.events.push_back(self.evt.drain(..len).collect());
        }
        // No L2CAP yet: ACL data is dropped packet by packet.
        while self.acl.len() >= 4 && self.acl.len() >= 4 + u16::from_le_bytes([self.acl[2], self.acl[3]]) as usize {
            let len = 4 + u16::from_le_bytes([self.acl[2], self.acl[3]]) as usize;
            self.acl.drain(..len);
        }
    }

    /// Sends a command and waits for its Command Complete (returns the
    /// parameters, status first) or Command Status. Other events that arrive
    /// meanwhile are handled as usual.
    fn cmd(&mut self, opcode: u16, params: &[u8]) -> Result<Vec<u8>, String> {
        let mut pkt = Vec::with_capacity(3 + params.len());
        pkt.extend_from_slice(&opcode.to_le_bytes());
        pkt.push(params.len() as u8);
        pkt.extend_from_slice(params);
        if unsafe { dhi::aero_xhci_bt_send(self.id, dhi::BT_COMMAND, pkt.as_ptr(), pkt.len() as u32) } != 0 {
            return Err(alloc::format!("could not send HCI command {:04x}", opcode));
        }
        let deadline = sched::ticks() + COMMAND_TIMEOUT;
        loop {
            self.pump();
            while let Some(ev) = self.events.pop_front() {
                let reply = match ev[0] {
                    EV_COMMAND_COMPLETE if ev.len() >= 6 && u16::from_le_bytes([ev[3], ev[4]]) == opcode => Some(ev[5..].to_vec()),
                    EV_COMMAND_STATUS if ev.len() >= 6 && u16::from_le_bytes([ev[4], ev[5]]) == opcode => Some(alloc::vec![ev[2]]),
                    _ => None,
                };
                match reply {
                    Some(r) if r[0] != 0 => {
                        return Err(alloc::format!("HCI command {:04x} failed with status {:#04x}", opcode, r[0]))
                    }
                    Some(r) => return Ok(r),
                    None => self.on_event(&ev),
                }
            }
            if sched::ticks() > deadline {
                return Err(alloc::format!("no answer to HCI command {:04x}", opcode));
            }
            sched::sleep_ticks(1);
        }
    }

    /// Classic inquiry and an LE active scan side by side for `seconds`.
    fn scan(&mut self, seconds: u32) {
        let le = ADAPTERS.lock()[self.index].le;
        let units = ((seconds * 100).div_ceil(128)).clamp(1, 0x30) as u8;
        self.inquiry_done = false;
        if let Err(e) = self.cmd(OP_INQUIRY, &[0x33, 0x8B, 0x9E, units, 0]) {
            console::print_colored(console::YELLOW, format_args!("[WARN] {}: inquiry: {}\n", self.name, e));
            self.inquiry_done = true;
        }
        let le_on = le
            && self.cmd(OP_LE_SET_SCAN_PARAMS, &[1, 0x60, 0, 0x30, 0, 0, 0]).is_ok()
            && self.cmd(OP_LE_SET_SCAN_ENABLE, &[1, 1]).is_ok();
        let end = sched::ticks() + seconds as u64 * apic::TIMER_HZ;
        while sched::ticks() < end || (!self.inquiry_done && sched::ticks() < end + 2 * apic::TIMER_HZ) {
            self.pump();
            while let Some(ev) = self.events.pop_front() {
                self.on_event(&ev);
            }
            sched::sleep_ticks(2);
        }
        if !self.inquiry_done {
            self.cmd(OP_INQUIRY_CANCEL, &[]).ok();
        }
        if le_on {
            self.cmd(OP_LE_SET_SCAN_ENABLE, &[0, 0]).ok();
        }
    }

    fn on_event(&mut self, ev: &[u8]) {
        let p = &ev[2..];
        match ev[0] {
            EV_INQUIRY_COMPLETE => self.inquiry_done = true,
            EV_INQUIRY_RESULT | EV_INQUIRY_RESULT_RSSI if !p.is_empty() => {
                for r in p[1..].chunks_exact(14).take(p[0] as usize) {
                    let rssi = (ev[0] == EV_INQUIRY_RESULT_RSSI).then_some(r[13] as i8);
                    // The RSSI form drops a reserved byte before the class.
                    let c = if ev[0] == EV_INQUIRY_RESULT_RSSI { 8 } else { 9 };
                    let class = u32::from_le_bytes([r[c], r[c + 1], r[c + 2], 0]);
                    self.found(addr(&r[..6]), false, false, rssi, |f| f.class = class);
                }
            }
            EV_EXTENDED_INQUIRY_RESULT if p.len() >= 15 => {
                let class = u32::from_le_bytes([p[9], p[10], p[11], 0]);
                let eir = &p[15..];
                self.found(addr(&p[1..7]), false, false, Some(p[14] as i8), |f| {
                    f.class = class;
                    parse_ad(eir, f);
                });
            }
            EV_LE_META if p.len() >= 2 && p[0] == LE_ADVERTISING_REPORT => {
                let mut r = &p[2..];
                for _ in 0..p[1] {
                    if r.len() < 9 || r.len() < 9 + r[8] as usize + 1 {
                        break;
                    }
                    let dlen = r[8] as usize;
                    let data = &r[9..9 + dlen];
                    let rssi = r[9 + dlen] as i8;
                    self.found(addr(&r[2..8]), true, r[1] & 1 != 0, Some(rssi), |f| parse_ad(data, f));
                    r = &r[10 + dlen..];
                }
            }
            _ => {}
        }
    }

    fn found(&self, address: [u8; 6], le: bool, random: bool, rssi: Option<i8>, update: impl FnOnce(&mut Found)) {
        let mut list = FOUND.lock();
        let i = match list.iter().position(|f| f.adapter == self.index && f.address == address && f.le == le) {
            Some(i) => i,
            None => {
                list.push(Found { adapter: self.index, address, le, random, rssi: None, class: 0, appearance: 0,
                    hid: false, name: String::new() });
                list.len() - 1
            }
        };
        let f = &mut list[i];
        if rssi.is_some() {
            f.rssi = rssi;
        }
        update(f);
    }
}

fn addr(b: &[u8]) -> [u8; 6] {
    [b[0], b[1], b[2], b[3], b[4], b[5]]
}

/// Extended inquiry response / advertising data: name, appearance, HID.
fn parse_ad(mut d: &[u8], f: &mut Found) {
    while d.len() >= 2 && d[0] != 0 && d.len() > d[0] as usize {
        let (kind, v) = (d[1], &d[2..1 + d[0] as usize]);
        match kind {
            0x08 | 0x09 if !v.is_empty() && (kind == 0x09 || f.name.is_empty()) => {
                f.name = String::from_utf8_lossy(v).into_owned();
            }
            0x19 if v.len() >= 2 => f.appearance = u16::from_le_bytes([v[0], v[1]]),
            0x02 | 0x03 => {
                if v.chunks_exact(2).any(|u| u16::from_le_bytes([u[0], u[1]]) == 0x1812) {
                    f.hid = true;
                }
            }
            _ => {}
        }
        d = &d[1 + d[0] as usize..];
    }
}

/// What kind of device this looks like, from its class or appearance.
pub fn kind(f: &Found) -> &'static str {
    match f.appearance {
        0x03C1 => return "keyboard",
        0x03C2 => return "mouse",
        0x03C3 => return "joystick",
        0x03C4 => return "gamepad",
        0x03C0..=0x03CF => return "input device",
        _ => {}
    }
    let (major, minor) = ((f.class >> 8) & 0x1F, (f.class >> 2) & 0x3F);
    match major {
        5 => match (minor >> 4, minor & 0xF) {
            (_, 1) => "joystick",
            (_, 2) => "gamepad",
            (1, _) => "keyboard",
            (2, _) => "mouse",
            _ => "input device",
        },
        4 => match minor {
            1 | 2 => "headset",
            4 => "microphone",
            5 | 6 => "speaker",
            _ => "audio device",
        },
        2 => "phone",
        1 => "computer",
        _ if f.hid => "input device",
        _ => "device",
    }
}

pub fn describe(f: &Found) -> String {
    let rssi = f.rssi.map_or(String::from("  ?"), |r| alloc::format!("{:>3}", r));
    let name = if f.name.is_empty() { String::from("(no name)") } else { alloc::format!("\"{}\"", f.name) };
    alloc::format!("{}  {:<9} {} dBm  {:<12} {}", addr_string(&f.address),
        if !f.le { "classic" } else if f.random { "LE random" } else { "LE" }, rssi, kind(f), name)
}

/// Bluetooth addresses travel least significant byte first.
pub fn addr_string(a: &[u8; 6]) -> String {
    alloc::format!("{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}", a[5], a[4], a[3], a[2], a[1], a[0])
}

pub fn version_name(v: u8) -> &'static str {
    match v {
        0 => "1.0b",
        1 => "1.1",
        2 => "1.2",
        3 => "2.0",
        4 => "2.1",
        5 => "3.0",
        6 => "4.0",
        7 => "4.1",
        8 => "4.2",
        9 => "5.0",
        10 => "5.1",
        11 => "5.2",
        12 => "5.3",
        13 => "5.4",
        14 => "6.0",
        _ => "newer than 6.0",
    }
}

pub fn manufacturer_name(id: u16) -> String {
    let name = match id {
        2 => "Intel",
        10 => "Qualcomm (CSR)",
        13 => "Texas Instruments",
        15 => "Broadcom",
        29 => "Qualcomm",
        70 => "MediaTek",
        93 => "Realtek",
        _ => return alloc::format!("company {}", id),
    };
    String::from(name)
}
