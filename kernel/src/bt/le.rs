//! Bluetooth LE gamepads: connecting, pairing (the Security Manager
//! Protocol with LE legacy "Just Works" pairing), and HID over GATT: find
//! the HID service, read the report map and the report characteristics,
//! and turn on their notifications, which then carry the input reports.
//! A paired LE gamepad reconnects when it advertises: the adapter waits for
//! any paired one with a filtered LE Create Connection, and the link is
//! encrypted with the long-term key from pairing.

use alloc::vec::Vec;

use super::{crypto, hid, l2cap, Bond, Host, BONDS, GAMEPADS};
use crate::{apic, console, sched};

pub const ATT_CID: u16 = 0x0004;
pub const SIGNAL_CID: u16 = 0x0005;
pub const SMP_CID: u16 = 0x0006;

const OP_LE_CREATE_CONNECTION: u16 = 0x200D;
pub(super) const OP_LE_CREATE_CONNECTION_CANCEL: u16 = 0x200E;
const OP_LE_CLEAR_ACCEPT_LIST: u16 = 0x2010;
const OP_LE_ADD_ACCEPT_LIST: u16 = 0x2011;
const OP_LE_CONNECTION_UPDATE: u16 = 0x2013;
const OP_LE_ENABLE_ENCRYPTION: u16 = 0x2019;
const OP_DISCONNECT: u16 = 0x0406;

pub(super) const LE_CONNECTION_COMPLETE: u8 = 0x01;

// SMP codes.
const PAIRING_REQUEST: u8 = 0x01;
const PAIRING_RESPONSE: u8 = 0x02;
const PAIRING_CONFIRM: u8 = 0x03;
const PAIRING_RANDOM: u8 = 0x04;
const PAIRING_FAILED: u8 = 0x05;
const ENCRYPTION_INFORMATION: u8 = 0x06;
const CENTRAL_IDENTIFICATION: u8 = 0x07;
const SECURITY_REQUEST: u8 = 0x0B;

// ATT opcodes.
const ATT_ERROR: u8 = 0x01;
const ATT_MTU_REQ: u8 = 0x02;
const ATT_MTU_RSP: u8 = 0x03;
const ATT_FIND_INFO_REQ: u8 = 0x04;
const ATT_FIND_INFO_RSP: u8 = 0x05;
const ATT_READ_REQ: u8 = 0x0A;
const ATT_READ_RSP: u8 = 0x0B;
const ATT_READ_BLOB_REQ: u8 = 0x0C;
const ATT_READ_BLOB_RSP: u8 = 0x0D;
const ATT_READ_GROUP_REQ: u8 = 0x10;
const ATT_READ_GROUP_RSP: u8 = 0x11;
const ATT_WRITE_REQ: u8 = 0x12;
const ATT_WRITE_RSP: u8 = 0x13;
const ATT_NOTIFY: u8 = 0x1B;
const ATT_INDICATE: u8 = 0x1D;
const ATT_CONFIRM: u8 = 0x1E;
const ATT_ERR_NOT_FOUND: u8 = 0x0A;

const UUID_PRIMARY_SERVICE: u16 = 0x2800;
const UUID_CHARACTERISTIC: u16 = 0x2803;
const UUID_CCCD: u16 = 0x2902;
const UUID_REPORT_REFERENCE: u16 = 0x2908;
const UUID_HID_SERVICE: u16 = 0x1812;
const UUID_REPORT_MAP: u16 = 0x2A4B;
const UUID_REPORT: u16 = 0x2A4D;

/// The MTU we ask for: room for any gamepad report in one notification.
const OUR_MTU: u16 = 185;
/// Wait this long before trying to reconnect again after a failure.
const RETRY: u64 = 5 * apic::TIMER_HZ;

/// Keys from LE pairing: what encrypts the link when the device reconnects.
#[derive(Clone, Copy)]
pub struct Keys {
    pub ltk: [u8; 16],
    pub ediv: u16,
    pub rand: [u8; 8],
    /// The device's address is a random (static) one.
    pub random: bool,
}

/// One HID report characteristic.
#[derive(Clone, Copy, Default)]
pub struct Report {
    pub value: u16,
    pub cccd: u16,
    reference: u16,
    pub id: u8,
    pub input: bool,
}

/// Where the LE initiator stands.
#[derive(Clone, Copy, PartialEq)]
pub(super) enum Initiating {
    None,
    /// Connecting to pair with this device.
    Pair([u8; 6], bool),
    /// Waiting for any paired LE device to advertise.
    Background,
    /// Cancelling the background connection; then pair with this one, if any.
    Cancelling(Option<([u8; 6], bool)>),
}

/// GATT discovery, one request at a time.
#[derive(Clone, Copy, PartialEq)]
enum Step {
    Idle,
    Mtu,
    Services(u16),
    Attributes(u16),
    Reference(usize),
    Map,
    Cccd(usize),
    Done,
}

/// LE side of a connection.
pub struct Le {
    pub random: bool,
    pairing: Option<Pairing>,
    /// Keys the device gave us, collected until pairing completes.
    ltk: Option<[u8; 16]>,
    mtu: u16,
    step: Step,
    hid: (u16, u16),
    map_handle: u16,
    map: Vec<u8>,
    /// Every attribute of the HID service: (handle, 16-bit type).
    attrs: Vec<(u16, u16)>,
    reports: Vec<Report>,
}

struct Pairing {
    preq: [u8; 7],
    pres: [u8; 7],
    mrand: [u8; 16],
    sconfirm: Option<[u8; 16]>,
    key_size: usize,
}

impl Le {
    pub fn new(random: bool) -> Self {
        Le { random, pairing: None, ltk: None, mtu: 23, step: Step::Idle, hid: (0, 0), map_handle: 0, map: Vec::new(),
            attrs: Vec::new(), reports: Vec::new() }
    }
}

fn le16(d: &[u8], o: usize) -> u16 {
    d.get(o..o + 2).map_or(0, |b| u16::from_le_bytes([b[0], b[1]]))
}

/// Random bytes for pairing: RDRAND, stirred with the clock through AES
/// in case the CPU has none.
fn random16() -> [u8; 16] {
    let mut v = [0u8; 16];
    for (i, chunk) in v.chunks_mut(8).enumerate() {
        let r = crate::arch::rdrand().unwrap_or(sched::ticks().wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ i as u64);
        chunk.copy_from_slice(&r.to_le_bytes());
    }
    let mut key = [0u8; 16];
    key[..8].copy_from_slice(&sched::ticks().to_le_bytes());
    crypto::aes128(&key, &v)
}

fn smp_reason(code: u8) -> &'static str {
    match code {
        0x03 => "the device needs a pairing method AeroForge does not offer yet (LE Secure Connections or a passkey)",
        0x04 => "confirm value mismatch",
        0x05 => "the device does not allow pairing now (is it in pairing mode?)",
        0x06 => "encryption key too short",
        0x08 => "unspecified reason",
        0x09 => "repeated attempts, try again later",
        _ => "pairing failed",
    }
}

impl Host {
    // ------------------------------------------------------- connecting

    fn le_create(&mut self, address: [u8; 6], random: bool, accept_list: bool) {
        let mut p = Vec::with_capacity(25);
        p.extend_from_slice(&0x0060u16.to_le_bytes()); // scan interval 60 ms
        p.extend_from_slice(&0x0030u16.to_le_bytes()); // scan window 30 ms
        p.push(accept_list as u8);
        p.push(random as u8);
        p.extend_from_slice(&address);
        p.push(0); // our public address
        p.extend_from_slice(&0x0006u16.to_le_bytes()); // interval 7.5 ms ...
        p.extend_from_slice(&0x000Cu16.to_le_bytes()); // ... to 15 ms
        p.extend_from_slice(&0u16.to_le_bytes()); // no latency
        p.extend_from_slice(&0x00C8u16.to_le_bytes()); // supervision timeout 2 s
        p.extend_from_slice(&[0, 0, 0, 0]);
        self.le_init = if accept_list { Initiating::Background } else { Initiating::Pair(address, random) };
        self.send_cmd(OP_LE_CREATE_CONNECTION, &p);
    }

    /// `bt pair` for an LE device.
    pub(super) fn le_pair(&mut self, address: [u8; 6], random: bool) {
        self.pair_deadline = Some(sched::ticks() + super::PAIR_TIMEOUT);
        BONDS.lock().retain(|b| b.address != address);
        if let Some(i) = self.conns.iter().position(|c| c.address == address && c.le.is_some()) {
            self.conns[i].outgoing = true;
            self.smp_start(i);
            return;
        }
        match self.le_init {
            Initiating::None => self.le_create(address, random, false),
            Initiating::Background => {
                self.le_init = Initiating::Cancelling(Some((address, random)));
                self.send_cmd(OP_LE_CREATE_CONNECTION_CANCEL, &[]);
            }
            _ => super::finish_pair(Err(alloc::string::String::from("another LE connection is being set up"))),
        }
    }

    /// Stops waiting for paired devices (before a scan, which the adapter
    /// may not run at the same time).
    pub(super) fn le_pause(&mut self) {
        if self.le_init != Initiating::Background {
            return;
        }
        self.le_init = Initiating::Cancelling(None);
        self.cmd(OP_LE_CREATE_CONNECTION_CANCEL, &[]).ok();
        let end = sched::ticks() + apic::TIMER_HZ;
        while self.le_init != Initiating::None && sched::ticks() < end {
            self.service();
            sched::sleep_ticks(1);
        }
        self.le_init = Initiating::None;
    }

    /// Pairing ran out of time while still connecting.
    pub(super) fn le_pair_timeout(&mut self) {
        if matches!(self.le_init, Initiating::Pair(..)) {
            self.le_init = Initiating::Cancelling(None);
            self.send_cmd(OP_LE_CREATE_CONNECTION_CANCEL, &[]);
        }
    }

    /// Waits for paired LE devices that are not connected (from the
    /// adapter's loop, so it may wait for command answers).
    pub(super) fn check_le(&mut self) {
        if !self.le_capable || self.le_init != Initiating::None || sched::ticks() < self.le_retry {
            return;
        }
        let wanted: Vec<([u8; 6], bool)> = BONDS.lock().iter()
            .filter(|b| b.adapter == self.index && !self.conns.iter().any(|c| c.address == b.address))
            .filter_map(|b| b.le.map(|k| (b.address, k.random)))
            .collect();
        if wanted.is_empty() {
            return;
        }
        let ok = self.cmd(OP_LE_CLEAR_ACCEPT_LIST, &[]).is_ok()
            && wanted.iter().all(|(a, random)| {
                let mut p = alloc::vec![*random as u8];
                p.extend_from_slice(a);
                self.cmd(OP_LE_ADD_ACCEPT_LIST, &p).is_ok()
            });
        if !ok || self.le_init != Initiating::None {
            self.le_retry = sched::ticks() + RETRY;
            return;
        }
        self.le_create([0; 6], false, true);
    }

    /// Command Status for LE Create Connection.
    pub(super) fn le_create_status(&mut self, status: u8) {
        if status == 0 {
            return;
        }
        match core::mem::replace(&mut self.le_init, Initiating::None) {
            Initiating::Pair(a, _) => self.pair_failed(&a, alloc::format!("could not connect: {}", super::hci_error(status))),
            _ => self.le_retry = sched::ticks() + RETRY,
        }
    }

    /// LE Connection Complete.
    pub(super) fn le_connected(&mut self, p: &[u8]) {
        if p.len() < 19 {
            return;
        }
        let status = p[1];
        let handle = le16(p, 2) & 0x0FFF;
        let random = p[5] == 1;
        let a = super::addr(&p[6..12]);
        let was = core::mem::replace(&mut self.le_init, Initiating::None);
        if status != 0 {
            match was {
                Initiating::Cancelling(Some((addr, random))) => self.le_create(addr, random, false),
                Initiating::Pair(addr, _) => self.pair_failed(&addr, alloc::format!("could not connect: {}", super::hci_error(status))),
                Initiating::Background => self.le_retry = sched::ticks() + RETRY,
                _ => {}
            }
            return;
        }
        if p[4] != 0 {
            // We never advertise, so we should never be the peripheral.
            self.send_cmd(OP_DISCONNECT, &[handle as u8, (handle >> 8) as u8, 0x13]);
            return;
        }
        let outgoing = super::pairing_with(&a);
        self.conns.retain(|c| c.address != a);
        let mut c = l2cap::Conn::new(handle, a, outgoing);
        c.le = Some(alloc::boxed::Box::new(Le::new(random)));
        self.conns.push(c);
        let i = self.conns.len() - 1;
        if let Initiating::Cancelling(Some((addr, random))) = was {
            // A paired device came back just as pairing with another began.
            self.le_create(addr, random, false);
        }
        if outgoing {
            self.smp_start(i);
            return;
        }
        let keys = BONDS.lock().iter().find(|b| b.address == a).and_then(|b| b.le);
        match keys {
            Some(k) => self.le_encrypt(handle, &k.rand, k.ediv, &k.ltk),
            None => self.send_cmd(OP_DISCONNECT, &[handle as u8, (handle >> 8) as u8, 0x05]),
        }
    }

    fn le_encrypt(&mut self, handle: u16, rand: &[u8; 8], ediv: u16, ltk: &[u8; 16]) {
        let mut p = handle.to_le_bytes().to_vec();
        p.extend_from_slice(rand);
        p.extend_from_slice(&ediv.to_le_bytes());
        p.extend_from_slice(ltk);
        self.send_cmd(OP_LE_ENABLE_ENCRYPTION, &p);
    }

    /// Encryption Change on an LE link.
    pub(super) fn le_encryption(&mut self, i: usize, ok: bool, status: u8) {
        let (handle, address) = (self.conns[i].handle, self.conns[i].address);
        if !ok {
            if super::pairing_with(&address) {
                self.pair_failed(&address, alloc::format!("encryption failed: {}", super::hci_error(status)));
            } else {
                // The device lost its keys: it has to be paired again.
                BONDS.lock().retain(|b| b.address != address);
                console::print_colored(console::YELLOW, format_args!("[WARN] {}: {} forgot its pairing, pair it again\n",
                    self.name, super::addr_string(&address)));
            }
            self.send_cmd(OP_DISCONNECT, &[handle as u8, (handle >> 8) as u8, 0x05]);
            return;
        }
        self.conns[i].encrypted = true;
        // Pairing: the device sends its keys now (they may even have
        // arrived before this event, and discovery started).
        let le = self.conns[i].le.as_ref().unwrap();
        if le.pairing.is_some() || le.step != Step::Idle {
            return;
        }
        // A reconnect: the device kept its notifications on, but turning
        // them on again costs little and covers devices that forget.
        let reports = BONDS.lock().iter().find(|b| b.address == address).map(|b| b.gatt.clone()).unwrap_or_default();
        if reports.is_empty() {
            self.gatt_start(i);
        } else {
            let le = self.conns[i].le.as_mut().unwrap();
            le.reports = reports;
            le.step = Step::Cccd(0);
            self.gatt_next(i);
        }
    }

    // ------------------------------------------------------- SMP

    fn smp_send(&mut self, i: usize, pdu: &[u8]) {
        self.send_frame(i, SMP_CID, pdu);
    }

    fn smp_start(&mut self, i: usize) {
        // No display or keyboard, bonding, LE legacy pairing; the device
        // gives us its encryption key.
        let preq = [PAIRING_REQUEST, 0x03, 0x00, 0x01, 16, 0x00, 0x01];
        let le = self.conns[i].le.as_mut().unwrap();
        le.pairing = Some(Pairing { preq, pres: [0; 7], mrand: random16(), sconfirm: None, key_size: 16 });
        le.ltk = None;
        self.smp_send(i, &preq);
    }

    fn smp_fail(&mut self, i: usize, reason: u8, why: &str) {
        self.smp_send(i, &[PAIRING_FAILED, reason]);
        let address = self.conns[i].address;
        if let Some(le) = self.conns[i].le.as_mut() {
            le.pairing = None;
        }
        self.pair_failed(&address, alloc::string::String::from(why));
        let handle = self.conns[i].handle;
        self.send_cmd(OP_DISCONNECT, &[handle as u8, (handle >> 8) as u8, 0x05]);
    }

    fn confirm(&self, i: usize, r: &[u8; 16]) -> [u8; 16] {
        let c = &self.conns[i];
        let le = c.le.as_ref().unwrap();
        let p = le.pairing.as_ref().unwrap();
        let ours = super::ADAPTERS.lock()[self.index].address;
        let v = crypto::c1(&[0; 16], &crypto::rev(r), &crypto::rev(&p.preq), &crypto::rev(&p.pres), 0, le.random as u8,
            &crypto::rev(&ours), &crypto::rev(&c.address));
        crypto::rev(&v)
    }

    pub(super) fn on_smp(&mut self, i: usize, pdu: &[u8]) {
        let Some(&code) = pdu.first() else { return };
        let pairing = self.conns[i].le.as_ref().is_some_and(|l| l.pairing.is_some());
        match code {
            PAIRING_RESPONSE if pairing && pdu.len() >= 7 => {
                let key_size = (pdu[4] as usize).min(16);
                if key_size < 7 {
                    return self.smp_fail(i, 0x06, "the device offered too short a key");
                }
                if pdu[6] & 0x01 == 0 {
                    return self.smp_fail(i, 0x08, "the device would not give an encryption key");
                }
                let mrand = {
                    let p = self.conns[i].le.as_mut().unwrap().pairing.as_mut().unwrap();
                    p.pres.copy_from_slice(&pdu[..7]);
                    p.key_size = key_size;
                    p.mrand
                };
                let mut out = alloc::vec![PAIRING_CONFIRM];
                out.extend_from_slice(&self.confirm(i, &mrand));
                self.smp_send(i, &out);
            }
            PAIRING_CONFIRM if pairing && pdu.len() >= 17 => {
                let p = self.conns[i].le.as_mut().unwrap().pairing.as_mut().unwrap();
                p.sconfirm = Some(core::array::from_fn(|k| pdu[1 + k]));
                let mut out = alloc::vec![PAIRING_RANDOM];
                out.extend_from_slice(&p.mrand);
                self.smp_send(i, &out);
            }
            PAIRING_RANDOM if pairing && pdu.len() >= 17 => {
                let srand: [u8; 16] = core::array::from_fn(|k| pdu[1 + k]);
                let (mrand, sconfirm, key_size) = {
                    let p = self.conns[i].le.as_ref().unwrap().pairing.as_ref().unwrap();
                    (p.mrand, p.sconfirm, p.key_size)
                };
                if sconfirm != Some(self.confirm(i, &srand)) {
                    return self.smp_fail(i, 0x04, "the device's confirm value did not match");
                }
                let mut stk = crypto::rev::<16>(&crypto::s1(&[0; 16], &crypto::rev(&srand), &crypto::rev(&mrand)));
                stk[key_size..].fill(0);
                let handle = self.conns[i].handle;
                self.le_encrypt(handle, &[0; 8], 0, &stk);
            }
            ENCRYPTION_INFORMATION if pairing && pdu.len() >= 17 => {
                self.conns[i].le.as_mut().unwrap().ltk = Some(core::array::from_fn(|k| pdu[1 + k]));
            }
            CENTRAL_IDENTIFICATION if pairing && pdu.len() >= 11 => {
                let (address, random, ltk) = {
                    let c = &mut self.conns[i];
                    let le = c.le.as_mut().unwrap();
                    le.pairing = None;
                    (c.address, le.random, le.ltk)
                };
                let Some(ltk) = ltk else {
                    return self.smp_fail(i, 0x08, "the device sent no encryption key");
                };
                let keys = Keys { ltk, ediv: le16(pdu, 1), rand: core::array::from_fn(|k| pdu[3 + k]), random };
                let name = self.device_name(&address);
                {
                    let mut bonds = BONDS.lock();
                    bonds.retain(|b| b.address != address);
                    bonds.push(Bond { adapter: self.index, address, key: [0; 16], name, descriptor: Vec::new(), audio: None,
                        le: Some(keys), gatt: Vec::new() });
                }
                self.gatt_start(i);
            }
            PAIRING_FAILED => {
                let reason = pdu.get(1).copied().unwrap_or(0);
                let address = self.conns[i].address;
                if let Some(le) = self.conns[i].le.as_mut() {
                    le.pairing = None;
                }
                self.pair_failed(&address, alloc::string::String::from(smp_reason(reason)));
            }
            SECURITY_REQUEST => {
                // The device wants security: it is either paired (encrypt
                // with its key) or being paired (we started that already).
                let c = &self.conns[i];
                if !pairing && !c.encrypted {
                    if let Some(k) = BONDS.lock().iter().find(|b| b.address == c.address).and_then(|b| b.le) {
                        let handle = c.handle;
                        self.le_encrypt(handle, &k.rand, k.ediv, &k.ltk);
                    }
                }
            }
            _ => {
                if pairing {
                    self.smp_fail(i, 0x07, "unexpected pairing message"); // command not supported
                }
            }
        }
    }

    // ------------------------------------------------------- LE signalling

    pub(super) fn on_le_signal(&mut self, i: usize, d: &[u8]) {
        if d.len() < 4 {
            return;
        }
        let (code, id) = (d[0], d[1]);
        match code {
            // Connection Parameter Update Request: accept and apply.
            0x12 if d.len() >= 12 => {
                self.send_frame(i, SIGNAL_CID, &[0x13, id, 2, 0, 0, 0]);
                let mut p = self.conns[i].handle.to_le_bytes().to_vec();
                p.extend_from_slice(&d[4..12]);
                p.extend_from_slice(&[0, 0, 0, 0]);
                self.send_cmd(OP_LE_CONNECTION_UPDATE, &p);
            }
            0x01 | 0x13 => {} // rejects and responses
            _ => self.send_frame(i, SIGNAL_CID, &[0x01, id, 2, 0, 0, 0]), // command not understood
        }
    }

    // ------------------------------------------------------- GATT

    fn att_send(&mut self, i: usize, pdu: &[u8]) {
        self.send_frame(i, ATT_CID, pdu);
    }

    fn gatt_start(&mut self, i: usize) {
        let le = self.conns[i].le.as_mut().unwrap();
        le.step = Step::Mtu;
        le.attrs.clear();
        le.reports.clear();
        le.map.clear();
        le.hid = (0, 0);
        self.att_send(i, &[ATT_MTU_REQ, OUR_MTU as u8, (OUR_MTU >> 8) as u8]);
    }

    fn gatt_failed(&mut self, i: usize, why: &str) {
        let address = self.conns[i].address;
        self.conns[i].le.as_mut().unwrap().step = Step::Idle;
        if super::pairing_with(&address) {
            self.pair_failed(&address, alloc::string::String::from(why));
            let handle = self.conns[i].handle;
            self.send_cmd(OP_DISCONNECT, &[handle as u8, (handle >> 8) as u8, 0x13]);
        } else {
            console::print_colored(console::YELLOW, format_args!("[WARN] {}: {}: {}\n", self.name,
                super::addr_string(&address), why));
        }
    }

    /// Sends the request for the current step, moving past finished ones.
    fn gatt_next(&mut self, i: usize) {
        loop {
            let le = self.conns[i].le.as_mut().unwrap();
            let pdu: Vec<u8> = match le.step {
                Step::Services(start) => {
                    let mut p = alloc::vec![ATT_READ_GROUP_REQ];
                    p.extend_from_slice(&start.to_le_bytes());
                    p.extend_from_slice(&0xFFFFu16.to_le_bytes());
                    p.extend_from_slice(&UUID_PRIMARY_SERVICE.to_le_bytes());
                    p
                }
                Step::Attributes(start) => {
                    let mut p = alloc::vec![ATT_FIND_INFO_REQ];
                    p.extend_from_slice(&start.to_le_bytes());
                    p.extend_from_slice(&le.hid.1.to_le_bytes());
                    p
                }
                Step::Reference(n) => match le.reports.get(n) {
                    Some(r) if r.reference != 0 => alloc::vec![ATT_READ_REQ, r.reference as u8, (r.reference >> 8) as u8],
                    Some(_) => {
                        // No Report Reference: an input report without an ID.
                        le.reports[n].input = true;
                        le.step = Step::Reference(n + 1);
                        continue;
                    }
                    None => {
                        le.step = Step::Map;
                        continue;
                    }
                },
                Step::Map => {
                    if le.map_handle == 0 {
                        return self.gatt_failed(i, "the device has no HID report map");
                    }
                    let h = le.map_handle;
                    if le.map.is_empty() {
                        alloc::vec![ATT_READ_REQ, h as u8, (h >> 8) as u8]
                    } else {
                        let off = le.map.len() as u16;
                        alloc::vec![ATT_READ_BLOB_REQ, h as u8, (h >> 8) as u8, off as u8, (off >> 8) as u8]
                    }
                }
                Step::Cccd(n) => match le.reports.get(n) {
                    Some(r) if r.input && r.cccd != 0 => alloc::vec![ATT_WRITE_REQ, r.cccd as u8, (r.cccd >> 8) as u8, 1, 0],
                    Some(_) => {
                        le.step = Step::Cccd(n + 1);
                        continue;
                    }
                    None => {
                        le.step = Step::Done;
                        return self.gatt_done(i);
                    }
                },
                Step::Idle | Step::Mtu | Step::Done => return,
            };
            return self.att_send(i, &pdu);
        }
    }

    /// Discovery (or the reconnect's notification set-up) is complete.
    fn gatt_done(&mut self, i: usize) {
        let address = self.conns[i].address;
        let (reports, map) = {
            let le = self.conns[i].le.as_ref().unwrap();
            (le.reports.clone(), le.map.clone())
        };
        if !map.is_empty() {
            self.conns[i].layout = hid::parse(&map);
            if let Some(b) = BONDS.lock().iter_mut().find(|b| b.address == address) {
                b.descriptor = map;
                b.gatt = reports;
            }
        }
        if self.conns[i].layout.is_empty() {
            return self.gatt_failed(i, "the device is not a gamepad (no axes or buttons in its report map)");
        }
        self.hid_ready(i);
    }

    pub(super) fn on_att(&mut self, i: usize, pdu: &[u8]) {
        let Some(&op) = pdu.first() else { return };
        // Requests to us: we have no attributes of our own.
        match op {
            ATT_MTU_REQ => return self.att_send(i, &[ATT_MTU_RSP, OUR_MTU as u8, (OUR_MTU >> 8) as u8]),
            ATT_INDICATE => return self.att_send(i, &[ATT_CONFIRM]),
            ATT_NOTIFY => return self.on_notify(i, pdu),
            _ if op & 0x01 == 0 && op & 0x40 == 0 && op != ATT_CONFIRM => {
                // Any other request: "attribute not found".
                return self.att_send(i, &[ATT_ERROR, op, pdu.get(1).copied().unwrap_or(0), pdu.get(2).copied().unwrap_or(0),
                    ATT_ERR_NOT_FOUND]);
            }
            _ => {}
        }
        let step = self.conns[i].le.as_ref().unwrap().step;
        let le = self.conns[i].le.as_mut().unwrap();
        match (step, op) {
            (Step::Mtu, ATT_MTU_RSP | ATT_ERROR) => {
                if op == ATT_MTU_RSP {
                    le.mtu = le16(pdu, 1).clamp(23, OUR_MTU);
                }
                le.step = Step::Services(1);
            }
            (Step::Services(_), ATT_READ_GROUP_RSP) if pdu.len() >= 2 => {
                let size = pdu[1] as usize;
                let mut last = 0xFFFF;
                for e in pdu[2..].chunks_exact(size.max(4)) {
                    let (start, end) = (le16(e, 0), le16(e, 2));
                    last = end;
                    if size == 6 && le16(e, 4) == UUID_HID_SERVICE && le.hid == (0, 0) {
                        le.hid = (start, end);
                    }
                }
                le.step = if le.hid != (0, 0) {
                    Step::Attributes(le.hid.0)
                } else if last == 0xFFFF {
                    return self.gatt_failed(i, "the device has no HID service");
                } else {
                    Step::Services(last + 1)
                };
            }
            (Step::Services(_), ATT_ERROR) => return self.gatt_failed(i, "the device has no HID service"),
            (Step::Attributes(_), ATT_FIND_INFO_RSP) if pdu.len() >= 2 => {
                let size = if pdu[1] == 1 { 4 } else { 18 };
                let mut last = le.hid.1;
                for e in pdu[2..].chunks_exact(size) {
                    let handle = le16(e, 0);
                    last = handle;
                    if size == 4 {
                        le.attrs.push((handle, le16(e, 2)));
                    }
                }
                le.step = if last >= le.hid.1 { Self::sort_attributes(le) } else { Step::Attributes(last + 1) };
            }
            (Step::Attributes(_), ATT_ERROR) => le.step = Self::sort_attributes(le),
            (Step::Reference(n), ATT_READ_RSP) if pdu.len() >= 3 => {
                le.reports[n].id = pdu[1];
                le.reports[n].input = pdu[2] == 1;
                le.step = Step::Reference(n + 1);
            }
            (Step::Reference(n), ATT_ERROR) => le.step = Step::Reference(n + 1),
            (Step::Map, ATT_READ_RSP | ATT_READ_BLOB_RSP) => {
                le.map.extend_from_slice(&pdu[1..]);
                // A full answer means there may be more.
                if pdu.len() < le.mtu as usize || le.map.len() >= 512 {
                    le.step = Step::Cccd(0);
                }
            }
            (Step::Map, ATT_ERROR) if !le.map.is_empty() => le.step = Step::Cccd(0), // read past the end
            (Step::Map, ATT_ERROR) => {
                let why = if pdu.get(4).is_some_and(|&e| e == 0x05 || e == 0x0F) {
                    "the device would not share its report map (it wants a stronger pairing)"
                } else {
                    "could not read the device's report map"
                };
                return self.gatt_failed(i, why);
            }
            (Step::Cccd(n), ATT_WRITE_RSP | ATT_ERROR) => le.step = Step::Cccd(n + 1),
            _ => return,
        }
        self.gatt_next(i);
    }

    /// Turns the HID service's attribute list into report characteristics.
    fn sort_attributes(le: &mut Le) -> Step {
        let mut current: Option<usize> = None;
        for &(handle, uuid) in &le.attrs {
            match uuid {
                UUID_CHARACTERISTIC => current = None,
                UUID_REPORT_MAP => le.map_handle = handle,
                UUID_REPORT => {
                    le.reports.push(Report { value: handle, ..Report::default() });
                    current = Some(le.reports.len() - 1);
                }
                UUID_CCCD => {
                    if let Some(r) = current {
                        le.reports[r].cccd = handle;
                    }
                }
                UUID_REPORT_REFERENCE => {
                    if let Some(r) = current {
                        le.reports[r].reference = handle;
                    }
                }
                _ => {}
            }
        }
        Step::Reference(0)
    }

    /// An input report.
    fn on_notify(&mut self, i: usize, pdu: &[u8]) {
        let c = &self.conns[i];
        let handle = le16(pdu, 1);
        let Some(r) = c.le.as_ref().unwrap().reports.iter().find(|r| r.value == handle && r.input) else { return };
        let mut report = Vec::with_capacity(pdu.len());
        if c.layout.uses_report_ids {
            report.push(r.id);
        }
        report.extend_from_slice(pdu.get(3..).unwrap_or(&[]));
        let mut pads = GAMEPADS.lock();
        if let Some(g) = pads.iter_mut().find(|g| g.address == c.address) {
            if g.connected && hid::decode(&c.layout, &report, &mut g.pad) {
                g.reports += 1;
            }
        }
    }
}
