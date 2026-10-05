//! Bluetooth host: finds USB Bluetooth adapters through the xHCI driver,
//! loads MediaTek firmware where the adapter needs it, brings each adapter
//! up over HCI, scans for classic (inquiry) and Bluetooth LE devices, and
//! pairs with and reads classic Bluetooth gamepads (HID over L2CAP).
//! A kernel thread owns the adapters; the shell talks to it through jobs.

mod hid;
mod l2cap;
mod sdp;

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;

use crate::dhi::{self, UsbDevice};
use crate::sync::IrqMutex;
use crate::{apic, console, modules, sched, usb};

pub use hid::{hat_name, AXIS_NAMES};

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
    /// Paging hints from the inquiry, for a faster connection.
    pub scan_mode: u8,
    pub clock_offset: Option<u16>,
}

/// A paired device. Kept in memory only: there is no writable disk yet, so
/// gamepads have to be paired again after a restart.
pub struct Bond {
    pub adapter: usize,
    pub address: [u8; 6],
    pub key: [u8; 16],
    pub name: String,
    pub descriptor: Vec<u8>,
}

/// A paired gamepad and what it is pressing right now.
pub struct Gamepad {
    pub adapter: usize,
    pub address: [u8; 6],
    pub name: String,
    pub connected: bool,
    pub layout: String,
    /// Which of hid::AXIS_NAMES the gamepad has.
    pub axes: u8,
    pub reports: u64,
    pub pad: hid::Pad,
}

struct ScanJob {
    seconds: u32,
    done: bool,
}

struct PairJob {
    address: [u8; 6],
    started: bool,
    result: Option<Result<String, String>>,
}

pub static ADAPTERS: IrqMutex<Vec<Adapter>> = IrqMutex::new(Vec::new());
pub static FOUND: IrqMutex<Vec<Found>> = IrqMutex::new(Vec::new());
pub static BONDS: IrqMutex<Vec<Bond>> = IrqMutex::new(Vec::new());
pub static GAMEPADS: IrqMutex<Vec<Gamepad>> = IrqMutex::new(Vec::new());
static SCAN: IrqMutex<Option<ScanJob>> = IrqMutex::new(None);
static PAIR: IrqMutex<Option<PairJob>> = IrqMutex::new(None);

const BOOT_SCAN_SECONDS: u32 = 3;
const PAIR_TIMEOUT: u64 = 30 * apic::TIMER_HZ;

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
/// scan and pairing requests from the shell and keeps every connection going.
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
        let pair = PAIR.lock().as_mut().filter(|j| !j.started).map(|j| {
            j.started = true;
            j.address
        });
        if let Some(address) = pair {
            // The adapter that saw the device in a scan, else the first one up.
            let seen = FOUND.lock().iter().find(|f| f.address == address && !f.le).map(|f| f.adapter);
            match hosts.iter_mut().filter(|h| h.up).find(|h| seen.is_none_or(|i| i == h.index)) {
                Some(h) => h.pair(address),
                None => finish_pair(Err(String::from("no Bluetooth adapter is up"))),
            }
        }
        for h in hosts.iter_mut().filter(|h| h.up) {
            h.service();
            h.check_pairing();
        }
        sched::sleep_ticks(1);
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

/// Pairs with the classic Bluetooth device at `address` and, if it is a
/// HID device, connects it (from the shell). Waits for the outcome.
pub fn pair(address: [u8; 6]) -> Result<String, String> {
    if !ADAPTERS.lock().iter().any(|a| a.up) {
        return Err(String::from("no Bluetooth adapter is up"));
    }
    {
        let mut job = PAIR.lock();
        if job.as_ref().is_some_and(|j| j.result.is_none()) {
            return Err(String::from("pairing is already in progress"));
        }
        *job = Some(PairJob { address, started: false, result: None });
    }
    loop {
        if let Some(r) = PAIR.lock().as_ref().and_then(|j| j.result.clone()) {
            return r;
        }
        sched::sleep_ticks(10);
    }
}

fn finish_pair(result: Result<String, String>) {
    if let Some(j) = PAIR.lock().as_mut() {
        if j.result.is_none() {
            j.result = Some(result);
        }
    }
}

fn pairing_with(address: &[u8; 6]) -> bool {
    PAIR.lock().as_ref().is_some_and(|j| j.started && j.result.is_none() && &j.address == address)
}

/// "11:22:33:44:55:66" to the over-the-air (least significant first) order.
pub fn parse_addr(s: &str) -> Option<[u8; 6]> {
    let mut out = [0u8; 6];
    let mut parts = s.split(':');
    for i in (0..6).rev() {
        out[i] = u8::from_str_radix(parts.next()?, 16).ok()?;
    }
    parts.next().is_none().then_some(out)
}

// ------------------------------------------------------------ one adapter

const OP_INQUIRY: u16 = 0x0401;
const OP_INQUIRY_CANCEL: u16 = 0x0402;
const OP_CREATE_CONNECTION: u16 = 0x0405;
const OP_DISCONNECT: u16 = 0x0406;
const OP_ACCEPT_CONNECTION: u16 = 0x0409;
const OP_REJECT_CONNECTION: u16 = 0x040A;
const OP_LINK_KEY_REPLY: u16 = 0x040B;
const OP_LINK_KEY_NEGATIVE_REPLY: u16 = 0x040C;
const OP_PIN_CODE_REPLY: u16 = 0x040D;
const OP_PIN_CODE_NEGATIVE_REPLY: u16 = 0x040E;
const OP_AUTHENTICATION_REQUESTED: u16 = 0x0411;
const OP_SET_CONNECTION_ENCRYPTION: u16 = 0x0413;
const OP_IO_CAPABILITY_REPLY: u16 = 0x042B;
const OP_USER_CONFIRMATION_REPLY: u16 = 0x042C;
const OP_USER_CONFIRMATION_NEGATIVE_REPLY: u16 = 0x042D;
const OP_IO_CAPABILITY_NEGATIVE_REPLY: u16 = 0x0434;
const OP_WRITE_DEFAULT_LINK_POLICY: u16 = 0x080F;
const OP_SET_EVENT_MASK: u16 = 0x0C01;
const OP_RESET: u16 = 0x0C03;
const OP_WRITE_LOCAL_NAME: u16 = 0x0C13;
const OP_WRITE_SCAN_ENABLE: u16 = 0x0C1A;
const OP_WRITE_CLASS_OF_DEVICE: u16 = 0x0C24;
const OP_WRITE_INQUIRY_MODE: u16 = 0x0C45;
const OP_WRITE_SIMPLE_PAIRING_MODE: u16 = 0x0C56;
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
const EV_CONNECTION_COMPLETE: u8 = 0x03;
const EV_CONNECTION_REQUEST: u8 = 0x04;
const EV_DISCONNECTION_COMPLETE: u8 = 0x05;
const EV_AUTHENTICATION_COMPLETE: u8 = 0x06;
const EV_ENCRYPTION_CHANGE: u8 = 0x08;
const EV_COMMAND_COMPLETE: u8 = 0x0E;
const EV_COMMAND_STATUS: u8 = 0x0F;
const EV_NUMBER_OF_COMPLETED_PACKETS: u8 = 0x13;
const EV_PIN_CODE_REQUEST: u8 = 0x16;
const EV_LINK_KEY_REQUEST: u8 = 0x17;
const EV_LINK_KEY_NOTIFICATION: u8 = 0x18;
const EV_INQUIRY_RESULT_RSSI: u8 = 0x22;
const EV_EXTENDED_INQUIRY_RESULT: u8 = 0x2F;
const EV_IO_CAPABILITY_REQUEST: u8 = 0x31;
const EV_USER_CONFIRMATION_REQUEST: u8 = 0x33;
const EV_SIMPLE_PAIRING_COMPLETE: u8 = 0x36;
const EV_LE_META: u8 = 0x3E;
const LE_ADVERTISING_REPORT: u8 = 0x02;

const COMMAND_TIMEOUT: u64 = 2 * apic::TIMER_HZ;

pub(super) struct Host {
    index: usize,
    id: i32,
    name: String,
    up: bool,
    evt: Vec<u8>,
    acl: Vec<u8>,
    events: VecDeque<Vec<u8>>,
    acl_in: VecDeque<Vec<u8>>,
    // Commands wait here until the controller has room (Num_HCI_Command_Packets).
    commands: VecDeque<Vec<u8>>,
    command_credits: u8,
    waiting: Option<u16>,
    reply: Option<Result<Vec<u8>, String>>,
    // ACL packets likewise wait for free controller buffers.
    acl_out: VecDeque<Vec<u8>>,
    acl_credits: u16,
    acl_mtu: u16,
    inquiry_done: bool,
    conns: Vec<l2cap::Conn>,
    pair_deadline: Option<u64>,
}

impl Host {
    fn new(index: usize) -> Self {
        let (id, name) = {
            let list = ADAPTERS.lock();
            (list[index].id, list[index].name.clone())
        };
        Host { index, id, name, up: false, evt: Vec::new(), acl: Vec::new(), events: VecDeque::new(),
            acl_in: VecDeque::new(), commands: VecDeque::new(), command_credits: 1, waiting: None, reply: None,
            acl_out: VecDeque::new(), acl_credits: 0, acl_mtu: 0, inquiry_done: false, conns: Vec::new(),
            pair_deadline: None }
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
        // A desktop computer (some gamepads only reconnect to computers),
        // Secure Simple Pairing, role switches allowed, and page scan on so
        // paired devices can reconnect (not discoverable).
        self.cmd(OP_WRITE_CLASS_OF_DEVICE, &[0x04, 0x01, 0x00])?;
        if let Err(e) = self.cmd(OP_WRITE_SIMPLE_PAIRING_MODE, &[1]) {
            console::print_colored(console::YELLOW, format_args!("[WARN] {}: no Secure Simple Pairing ({}), PIN pairing only\n", self.name, e));
        }
        self.cmd(OP_WRITE_DEFAULT_LINK_POLICY, &[0x05, 0x00]).ok();
        self.cmd(OP_WRITE_SCAN_ENABLE, &[0x02])?;
        let (mut mtu, mut packets) = (u16::from_le_bytes([b[1], b[2]]), u16::from_le_bytes([b[4], b[5]]));
        if le {
            self.cmd(OP_LE_SET_EVENT_MASK, &0x1Fu64.to_le_bytes())?;
            let lb = self.cmd(OP_LE_READ_BUFFER_SIZE, &[])?;
            if mtu == 0 && lb.len() >= 4 {
                (mtu, packets) = (u16::from_le_bytes([lb[1], lb[2]]), lb[3] as u16);
            }
        }
        self.acl_mtu = mtu;
        self.acl_credits = packets;

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
        self.acl_in.clear();
        self.evt.clear();
        self.acl.clear();
    }

    /// Moves received chunks into the reassembly buffers and splits off
    /// complete events and ACL packets.
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
        while self.acl.len() >= 4 && self.acl.len() >= 4 + u16::from_le_bytes([self.acl[2], self.acl[3]]) as usize {
            let len = 4 + u16::from_le_bytes([self.acl[2], self.acl[3]]) as usize;
            self.acl_in.push_back(self.acl.drain(..len).collect());
        }
    }

    /// Handles everything that arrived and sends whatever the controller has
    /// room for.
    fn service(&mut self) {
        self.pump();
        while let Some(ev) = self.events.pop_front() {
            self.on_event(&ev);
        }
        while let Some(pkt) = self.acl_in.pop_front() {
            self.on_acl(&pkt);
        }
        self.flush();
    }

    fn flush(&mut self) {
        while self.command_credits > 0 {
            let Some(pkt) = self.commands.pop_front() else { break };
            if unsafe { dhi::aero_xhci_bt_send(self.id, dhi::BT_COMMAND, pkt.as_ptr(), pkt.len() as u32) } != 0 {
                console::print_colored(console::YELLOW, format_args!("[WARN] {}: could not send an HCI command\n", self.name));
                continue;
            }
            self.command_credits -= 1;
        }
        while self.acl_credits > 0 {
            let Some(pkt) = self.acl_out.pop_front() else { break };
            if unsafe { dhi::aero_xhci_bt_send(self.id, dhi::BT_ACL, pkt.as_ptr(), pkt.len() as u32) } != 0 {
                console::print_colored(console::YELLOW, format_args!("[WARN] {}: could not send ACL data\n", self.name));
                continue;
            }
            self.acl_credits -= 1;
        }
    }

    /// Queues a command without waiting for its answer.
    fn send_cmd(&mut self, opcode: u16, params: &[u8]) {
        let mut pkt = Vec::with_capacity(3 + params.len());
        pkt.extend_from_slice(&opcode.to_le_bytes());
        pkt.push(params.len() as u8);
        pkt.extend_from_slice(params);
        self.commands.push_back(pkt);
        self.flush();
    }

    /// Sends a command and waits for its Command Complete (returns the
    /// parameters, status first) or Command Status. Everything else that
    /// arrives meanwhile is handled as usual.
    fn cmd(&mut self, opcode: u16, params: &[u8]) -> Result<Vec<u8>, String> {
        self.waiting = Some(opcode);
        self.reply = None;
        self.send_cmd(opcode, params);
        let deadline = sched::ticks() + COMMAND_TIMEOUT;
        loop {
            self.service();
            if let Some(r) = self.reply.take() {
                return r;
            }
            if sched::ticks() > deadline {
                self.waiting = None;
                // The controller may never answer; do not wait for its credit.
                self.command_credits = self.command_credits.max(1);
                return Err(alloc::format!("no answer to HCI command {:04x}", opcode));
            }
            sched::sleep_ticks(1);
        }
    }

    /// Queues an ACL packet (handle, first or continuing fragment).
    fn send_acl(&mut self, handle: u16, data: &[u8]) {
        let mtu = self.acl_mtu.max(27) as usize;
        for (i, part) in data.chunks(mtu).enumerate() {
            let flags: u16 = if i == 0 { 0x2000 } else { 0x1000 };
            let mut pkt = Vec::with_capacity(4 + part.len());
            pkt.extend_from_slice(&(handle | flags).to_le_bytes());
            pkt.extend_from_slice(&(part.len() as u16).to_le_bytes());
            pkt.extend_from_slice(part);
            self.acl_out.push_back(pkt);
        }
        self.flush();
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
            self.service();
            sched::sleep_ticks(2);
        }
        if !self.inquiry_done {
            self.cmd(OP_INQUIRY_CANCEL, &[]).ok();
        }
        if le_on {
            self.cmd(OP_LE_SET_SCAN_ENABLE, &[0, 0]).ok();
        }
    }

    // ------------------------------------------------------- pairing

    /// Starts pairing: connect, then authentication runs from the events.
    fn pair(&mut self, address: [u8; 6]) {
        self.pair_deadline = Some(sched::ticks() + PAIR_TIMEOUT);
        BONDS.lock().retain(|b| b.address != address);
        if let Some(c) = self.conns.iter_mut().find(|c| c.address == address) {
            // Already connected (it reconnected on its own): authenticate again.
            c.outgoing = true;
            let handle = c.handle;
            self.send_cmd(OP_AUTHENTICATION_REQUESTED, &handle.to_le_bytes());
            return;
        }
        let (mode, clock) = FOUND.lock().iter().find(|f| f.address == address && !f.le)
            .map_or((1, None), |f| (f.scan_mode, f.clock_offset));
        let clock = clock.map_or(0, |c| c | 0x8000);
        let mut p = Vec::with_capacity(13);
        p.extend_from_slice(&address);
        p.extend_from_slice(&0xCC18u16.to_le_bytes()); // DM1/DH1/DM3/DH3/DM5/DH5
        p.push(mode);
        p.push(0);
        p.extend_from_slice(&clock.to_le_bytes());
        p.push(1); // allow a role switch
        self.send_cmd(OP_CREATE_CONNECTION, &p);
    }

    fn check_pairing(&mut self) {
        if let Some(deadline) = self.pair_deadline {
            if sched::ticks() > deadline {
                self.pair_deadline = None;
                let address = PAIR.lock().as_ref().map(|j| j.address);
                if let Some(c) = address.and_then(|a| self.conns.iter().find(|c| c.address == a)) {
                    let p = [c.handle as u8, (c.handle >> 8) as u8, 0x13];
                    self.send_cmd(OP_DISCONNECT, &p);
                }
                finish_pair(Err(String::from("timed out (is the device in pairing mode?)")));
            }
        }
    }

    fn pair_failed(&mut self, address: &[u8; 6], why: String) {
        if pairing_with(address) {
            self.pair_deadline = None;
            finish_pair(Err(why));
        } else {
            console::print_colored(console::YELLOW, format_args!("[WARN] {}: {}: {}\n", self.name, addr_string(address), why));
        }
    }

    fn device_name(&self, address: &[u8; 6]) -> String {
        if let Some(b) = BONDS.lock().iter().find(|b| &b.address == address && !b.name.is_empty()) {
            return b.name.clone();
        }
        FOUND.lock().iter().find(|f| &f.address == address && !f.name.is_empty())
            .map_or(String::from("(no name)"), |f| f.name.clone())
    }

    /// Called once a connection has its HID channels open.
    fn hid_ready(&mut self, conn: usize) {
        let c = &self.conns[conn];
        let (address, layout, axes) = (c.address, c.layout.summary(), c.layout.axis_mask());
        let name = self.device_name(&address);
        {
            let mut pads = GAMEPADS.lock();
            match pads.iter_mut().find(|p| p.address == address) {
                Some(p) => {
                    p.connected = true;
                    p.layout = layout.clone();
                    p.axes = axes;
                }
                None => pads.push(Gamepad { adapter: self.index, address, name: name.clone(), connected: true,
                    layout: layout.clone(), axes, reports: 0, pad: hid::Pad::default() }),
            }
        }
        crate::kok!("{}: gamepad {} \"{}\" connected ({})", self.name, addr_string(&address), name, layout);
        if pairing_with(&address) {
            self.pair_deadline = None;
            finish_pair(Ok(alloc::format!("\"{}\" paired and connected ({})", name, layout)));
        }
    }

    fn on_event(&mut self, ev: &[u8]) {
        let p = &ev[2..];
        match ev[0] {
            EV_COMMAND_COMPLETE if p.len() >= 3 => {
                self.command_credits = p[0];
                let opcode = u16::from_le_bytes([p[1], p[2]]);
                let ret = &p[3..];
                if opcode != 0 && self.waiting == Some(opcode) {
                    self.waiting = None;
                    self.reply = Some(match ret.first() {
                        Some(&s) if s != 0 => Err(alloc::format!("HCI command {:04x} failed with status {:#04x}", opcode, s)),
                        _ => Ok(ret.to_vec()),
                    });
                }
            }
            EV_COMMAND_STATUS if p.len() >= 4 => {
                self.command_credits = p[1];
                let opcode = u16::from_le_bytes([p[2], p[3]]);
                if opcode != 0 && self.waiting == Some(opcode) {
                    self.waiting = None;
                    self.reply = Some(if p[0] != 0 {
                        Err(alloc::format!("HCI command {:04x} failed with status {:#04x}", opcode, p[0]))
                    } else {
                        Ok(alloc::vec![0])
                    });
                } else if p[0] != 0 && opcode == OP_CREATE_CONNECTION {
                    if let Some(a) = PAIR.lock().as_ref().map(|j| j.address) {
                        self.pair_failed(&a, alloc::format!("could not connect: {}", hci_error(p[0])));
                    }
                }
            }
            EV_NUMBER_OF_COMPLETED_PACKETS if !p.is_empty() => {
                for h in p[1..].chunks_exact(4).take(p[0] as usize) {
                    self.acl_credits += u16::from_le_bytes([h[2], h[3]]);
                }
            }
            EV_INQUIRY_COMPLETE => self.inquiry_done = true,
            EV_INQUIRY_RESULT | EV_INQUIRY_RESULT_RSSI if !p.is_empty() => {
                for r in p[1..].chunks_exact(14).take(p[0] as usize) {
                    let rssi = (ev[0] == EV_INQUIRY_RESULT_RSSI).then_some(r[13] as i8);
                    // The RSSI form drops a reserved byte before the class.
                    let c = if ev[0] == EV_INQUIRY_RESULT_RSSI { 8 } else { 9 };
                    let class = u32::from_le_bytes([r[c], r[c + 1], r[c + 2], 0]);
                    let clock = u16::from_le_bytes([r[c + 3], r[c + 4]]);
                    self.found(addr(&r[..6]), false, false, rssi, |f| {
                        f.class = class;
                        f.scan_mode = r[6];
                        f.clock_offset = Some(clock & 0x7FFF);
                    });
                }
            }
            EV_EXTENDED_INQUIRY_RESULT if p.len() >= 15 => {
                let class = u32::from_le_bytes([p[9], p[10], p[11], 0]);
                let clock = u16::from_le_bytes([p[12], p[13]]);
                let eir = &p[15..];
                self.found(addr(&p[1..7]), false, false, Some(p[14] as i8), |f| {
                    f.class = class;
                    f.scan_mode = p[7];
                    f.clock_offset = Some(clock & 0x7FFF);
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

            // ---- connections
            EV_CONNECTION_REQUEST if p.len() >= 10 => {
                let a = addr(&p[..6]);
                // Only paired devices may connect (a gamepad reconnecting).
                if p[9] == 1 && BONDS.lock().iter().any(|b| b.address == a && b.adapter == self.index) {
                    let mut r = a.to_vec();
                    r.push(0x00); // become the central
                    self.send_cmd(OP_ACCEPT_CONNECTION, &r);
                } else {
                    let mut r = a.to_vec();
                    r.push(0x0F); // unacceptable device address
                    self.send_cmd(OP_REJECT_CONNECTION, &r);
                }
            }
            EV_CONNECTION_COMPLETE if p.len() >= 11 => {
                let a = addr(&p[3..9]);
                if p[0] != 0 {
                    self.pair_failed(&a, alloc::format!("connection failed: {}", hci_error(p[0])));
                    return;
                }
                if p[9] != 1 {
                    return; // SCO link: not used yet
                }
                let handle = u16::from_le_bytes([p[1], p[2]]) & 0x0FFF;
                let outgoing = pairing_with(&a);
                self.conns.retain(|c| c.address != a);
                self.conns.push(l2cap::Conn::new(handle, a, outgoing));
                // Authenticate (pair, or prove the stored key) and encrypt.
                self.send_cmd(OP_AUTHENTICATION_REQUESTED, &handle.to_le_bytes());
            }
            EV_DISCONNECTION_COMPLETE if p.len() >= 4 && p[0] == 0 => {
                let handle = u16::from_le_bytes([p[1], p[2]]) & 0x0FFF;
                if let Some(i) = self.conns.iter().position(|c| c.handle == handle) {
                    let c = self.conns.remove(i);
                    let mut pads = GAMEPADS.lock();
                    if let Some(g) = pads.iter_mut().find(|g| g.address == c.address && g.connected) {
                        g.connected = false;
                        g.pad = hid::Pad::default();
                        let name = g.name.clone();
                        drop(pads);
                        crate::kok!("{}: gamepad {} \"{}\" disconnected ({})", self.name,
                            addr_string(&c.address), name, hci_error(p[3]));
                    } else {
                        drop(pads);
                    }
                    if pairing_with(&c.address) {
                        self.pair_failed(&c.address, alloc::format!("disconnected: {}", hci_error(p[3])));
                    }
                }
            }

            // ---- security
            EV_LINK_KEY_REQUEST if p.len() >= 6 => {
                let a = addr(&p[..6]);
                let key = BONDS.lock().iter().find(|b| b.address == a).map(|b| b.key);
                match key {
                    Some(k) => {
                        let mut r = a.to_vec();
                        r.extend_from_slice(&k);
                        self.send_cmd(OP_LINK_KEY_REPLY, &r);
                    }
                    None => self.send_cmd(OP_LINK_KEY_NEGATIVE_REPLY, &a),
                }
            }
            EV_PIN_CODE_REQUEST if p.len() >= 6 => {
                // Legacy pairing: gamepads without a keypad use 0000.
                let a = addr(&p[..6]);
                if pairing_with(&a) {
                    let mut r = a.to_vec();
                    r.push(4);
                    r.extend_from_slice(b"0000");
                    r.resize(6 + 1 + 16, 0);
                    self.send_cmd(OP_PIN_CODE_REPLY, &r);
                } else {
                    self.send_cmd(OP_PIN_CODE_NEGATIVE_REPLY, &a);
                }
            }
            EV_IO_CAPABILITY_REQUEST if p.len() >= 6 => {
                let a = addr(&p[..6]);
                let mut r = a.to_vec();
                if pairing_with(&a) {
                    // No display, no keyboard: "Just Works", general bonding.
                    r.extend_from_slice(&[0x03, 0x00, 0x04]);
                    self.send_cmd(OP_IO_CAPABILITY_REPLY, &r);
                } else {
                    r.push(0x18); // pairing not allowed
                    self.send_cmd(OP_IO_CAPABILITY_NEGATIVE_REPLY, &r);
                }
            }
            EV_USER_CONFIRMATION_REQUEST if p.len() >= 6 => {
                let a = addr(&p[..6]);
                let op = if pairing_with(&a) { OP_USER_CONFIRMATION_REPLY } else { OP_USER_CONFIRMATION_NEGATIVE_REPLY };
                self.send_cmd(op, &a);
            }
            EV_SIMPLE_PAIRING_COMPLETE if p.len() >= 7 && p[0] != 0 => {
                let a = addr(&p[1..7]);
                self.pair_failed(&a, alloc::format!("pairing failed: {}", hci_error(p[0])));
            }
            EV_LINK_KEY_NOTIFICATION if p.len() >= 22 => {
                let a = addr(&p[..6]);
                let mut key = [0u8; 16];
                key.copy_from_slice(&p[6..22]);
                let name = self.device_name(&a);
                let mut bonds = BONDS.lock();
                match bonds.iter_mut().find(|b| b.address == a) {
                    Some(b) => b.key = key,
                    None => bonds.push(Bond { adapter: self.index, address: a, key, name, descriptor: Vec::new() }),
                }
            }
            EV_AUTHENTICATION_COMPLETE if p.len() >= 3 => {
                let handle = u16::from_le_bytes([p[1], p[2]]) & 0x0FFF;
                let Some(c) = self.conns.iter().find(|c| c.handle == handle) else { return };
                let a = c.address;
                if p[0] != 0 {
                    // A wrong stored key means the device forgot us: drop the bond.
                    if p[0] == 0x06 {
                        BONDS.lock().retain(|b| b.address != a);
                    }
                    self.pair_failed(&a, alloc::format!("authentication failed: {}", hci_error(p[0])));
                    self.send_cmd(OP_DISCONNECT, &[handle as u8, (handle >> 8) as u8, 0x05]);
                    return;
                }
                let mut r = handle.to_le_bytes().to_vec();
                r.push(1);
                self.send_cmd(OP_SET_CONNECTION_ENCRYPTION, &r);
            }
            EV_ENCRYPTION_CHANGE if p.len() >= 4 => {
                let handle = u16::from_le_bytes([p[1], p[2]]) & 0x0FFF;
                let Some(i) = self.conns.iter().position(|c| c.handle == handle) else { return };
                if p[0] != 0 || p[3] == 0 {
                    let a = self.conns[i].address;
                    self.pair_failed(&a, alloc::format!("encryption failed: {}", hci_error(p[0])));
                    self.send_cmd(OP_DISCONNECT, &[handle as u8, (handle >> 8) as u8, 0x05]);
                    return;
                }
                self.conns[i].encrypted = true;
                // A device we paired with just now: read its report
                // descriptor over SDP, then open the HID channels. A device
                // that reconnected opens its channels itself.
                if self.conns[i].outgoing {
                    self.start_sdp(i);
                } else {
                    self.release_held(i);
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
                    hid: false, name: String::new(), scan_mode: 1, clock_offset: None });
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

/// HCI error codes that pairing and connections commonly end with.
pub fn hci_error(code: u8) -> String {
    let text = match code {
        0x04 => "no answer (is it switched on and in pairing mode?)",
        0x05 => "authentication failure",
        0x06 => "the device forgot this pairing (pair it again)",
        0x08 => "connection timed out (out of range?)",
        0x0D | 0x0E | 0x0F => "the device refused the connection",
        0x13 => "the device disconnected (switched off?)",
        0x16 => "disconnected by AeroForge",
        0x18 => "pairing not allowed",
        0x22 => "the device stopped answering",
        _ => return alloc::format!("HCI error {:#04x}", code),
    };
    String::from(text)
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
