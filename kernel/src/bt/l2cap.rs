//! L2CAP over an ACL connection: the signalling channel, connection-oriented
//! channels in basic mode, and what runs on them (SDP to read a HID report
//! descriptor, then the HID control and interrupt channels).

use alloc::vec::Vec;

use super::{hid, sdp, Host, BONDS, GAMEPADS};

const SIGNALLING_CID: u16 = 0x0001;
const FIRST_DYNAMIC_CID: u16 = 0x0040;
pub const PSM_HID_CONTROL: u16 = 0x0011;
pub const PSM_HID_INTERRUPT: u16 = 0x0013;

const COMMAND_REJECT: u8 = 0x01;
const CONNECTION_REQUEST: u8 = 0x02;
const CONNECTION_RESPONSE: u8 = 0x03;
const CONFIGURE_REQUEST: u8 = 0x04;
const CONFIGURE_RESPONSE: u8 = 0x05;
const DISCONNECTION_REQUEST: u8 = 0x06;
const DISCONNECTION_RESPONSE: u8 = 0x07;
const ECHO_REQUEST: u8 = 0x08;
const ECHO_RESPONSE: u8 = 0x09;
const INFORMATION_REQUEST: u8 = 0x0A;
const INFORMATION_RESPONSE: u8 = 0x0B;

/// HID over Bluetooth: transaction type DATA, report type Input.
const HID_DATA_INPUT: u8 = 0xA1;

pub struct Channel {
    psm: u16,
    local: u16,
    remote: u16,
    /// Our Configure Request was accepted.
    configured_out: bool,
    /// We accepted theirs.
    configured_in: bool,
    open: bool,
}

pub struct Conn {
    pub handle: u16,
    pub address: [u8; 6],
    /// We connected to pair (we open the channels); false when a paired
    /// device reconnected on its own (it opens them).
    pub outgoing: bool,
    pub encrypted: bool,
    pub layout: hid::Layout,
    channels: Vec<Channel>,
    rx: Vec<u8>,
    next_cid: u16,
    next_id: u8,
    sdp_transaction: u16,
    sdp_lists: Vec<u8>,
    ready: bool,
    /// Channel requests that arrived before encryption: (psm, remote CID, id).
    held: Vec<(u16, u16, u8)>,
}

impl Conn {
    pub fn new(handle: u16, address: [u8; 6], outgoing: bool) -> Self {
        // A known device keeps the descriptor read when it was paired.
        let layout = BONDS.lock().iter().find(|b| b.address == address && !b.descriptor.is_empty())
            .map(|b| hid::parse(&b.descriptor)).unwrap_or_default();
        Conn { handle, address, outgoing, encrypted: false, layout, channels: Vec::new(), rx: Vec::new(),
            next_cid: FIRST_DYNAMIC_CID, next_id: 1, sdp_transaction: 0, sdp_lists: Vec::new(), ready: false,
            held: Vec::new() }
    }

    fn channel(&mut self, local: u16) -> Option<&mut Channel> {
        self.channels.iter_mut().find(|c| c.local == local)
    }
}

impl Host {
    /// One ACL packet from the controller: reassemble the L2CAP frame.
    pub(super) fn on_acl(&mut self, pkt: &[u8]) {
        let head = u16::from_le_bytes([pkt[0], pkt[1]]);
        let (handle, boundary) = (head & 0x0FFF, (head >> 12) & 3);
        let Some(i) = self.conns.iter().position(|c| c.handle == handle) else { return };
        let data = &pkt[4..];
        let c = &mut self.conns[i];
        if boundary == 1 {
            if c.rx.is_empty() {
                return; // continuation without a start
            }
            c.rx.extend_from_slice(data);
        } else {
            c.rx.clear();
            c.rx.extend_from_slice(data);
        }
        if c.rx.len() < 4 {
            return;
        }
        let len = u16::from_le_bytes([c.rx[0], c.rx[1]]) as usize;
        if c.rx.len() < 4 + len {
            return;
        }
        let frame: Vec<u8> = c.rx.drain(..).collect();
        let cid = u16::from_le_bytes([frame[2], frame[3]]);
        let payload = &frame[4..4 + len];
        if cid == SIGNALLING_CID {
            self.on_signalling(i, payload);
        } else {
            self.on_channel_data(i, cid, payload);
        }
    }

    fn send_frame(&mut self, conn: usize, cid: u16, payload: &[u8]) {
        let mut f = Vec::with_capacity(4 + payload.len());
        f.extend_from_slice(&(payload.len() as u16).to_le_bytes());
        f.extend_from_slice(&cid.to_le_bytes());
        f.extend_from_slice(payload);
        let handle = self.conns[conn].handle;
        self.send_acl(handle, &f);
    }

    fn send_signal(&mut self, conn: usize, code: u8, id: Option<u8>, data: &[u8]) {
        let id = id.unwrap_or_else(|| {
            let c = &mut self.conns[conn];
            let id = c.next_id;
            c.next_id = c.next_id.wrapping_add(1).max(1);
            id
        });
        let mut s = alloc::vec![code, id];
        s.extend_from_slice(&(data.len() as u16).to_le_bytes());
        s.extend_from_slice(data);
        self.send_frame(conn, SIGNALLING_CID, &s);
    }

    /// Opens a channel to `psm` on the device.
    fn connect_channel(&mut self, conn: usize, psm: u16) {
        let c = &mut self.conns[conn];
        let local = c.next_cid;
        c.next_cid += 1;
        c.channels.push(Channel { psm, local, remote: 0, configured_out: false, configured_in: false, open: false });
        let mut d = psm.to_le_bytes().to_vec();
        d.extend_from_slice(&local.to_le_bytes());
        self.send_signal(conn, CONNECTION_REQUEST, None, &d);
    }

    fn close_channel(&mut self, conn: usize, local: u16) {
        let Some(ch) = self.conns[conn].channel(local) else { return };
        let mut d = ch.remote.to_le_bytes().to_vec();
        d.extend_from_slice(&local.to_le_bytes());
        ch.open = false;
        self.send_signal(conn, DISCONNECTION_REQUEST, None, &d);
    }

    /// Our side of configuration: no options, so the defaults (MTU 672,
    /// basic mode) apply.
    fn send_configure(&mut self, conn: usize, remote: u16) {
        let mut d = remote.to_le_bytes().to_vec();
        d.extend_from_slice(&0u16.to_le_bytes());
        self.send_signal(conn, CONFIGURE_REQUEST, None, &d);
    }

    fn on_signalling(&mut self, conn: usize, mut s: &[u8]) {
        // A signalling frame may carry several commands.
        while s.len() >= 4 {
            let (code, id) = (s[0], s[1]);
            let len = u16::from_le_bytes([s[2], s[3]]) as usize;
            if s.len() < 4 + len {
                break;
            }
            let d = &s[4..4 + len];
            self.on_signal(conn, code, id, d);
            if conn >= self.conns.len() {
                return;
            }
            s = &s[4 + len..];
        }
    }

    fn on_signal(&mut self, conn: usize, code: u8, id: u8, d: &[u8]) {
        let le16 = |o: usize| d.get(o..o + 2).map_or(0, |b| u16::from_le_bytes([b[0], b[1]]));
        match code {
            CONNECTION_REQUEST if d.len() >= 4 => {
                // The device opening its HID channels after a reconnect.
                let (psm, remote) = (le16(0), le16(2));
                if matches!(psm, PSM_HID_CONTROL | PSM_HID_INTERRUPT) && !self.conns[conn].encrypted {
                    // Not encrypted yet: answer "pending, authentication
                    // pending" and finish once encryption is on.
                    self.conns[conn].held.push((psm, remote, id));
                    self.connection_response(conn, id, 0, remote, 1, 1);
                } else {
                    self.accept_channel(conn, psm, remote, id);
                }
            }
            CONNECTION_RESPONSE if d.len() >= 8 => {
                let (remote, local, result) = (le16(0), le16(2), le16(4));
                match result {
                    0 => {
                        if let Some(ch) = self.conns[conn].channel(local) {
                            ch.remote = remote;
                            self.send_configure(conn, remote);
                        }
                    }
                    1 => {} // pending: another response follows
                    _ => {
                        let psm = self.conns[conn].channel(local).map_or(0, |ch| ch.psm);
                        self.conns[conn].channels.retain(|ch| ch.local != local);
                        let a = self.conns[conn].address;
                        self.pair_failed(&a, alloc::format!("the device refused channel {:#06x} (result {})", psm, result));
                    }
                }
            }
            CONFIGURE_REQUEST if d.len() >= 4 => {
                let local = le16(0);
                let remote = self.conns[conn].channel(local).map(|ch| {
                    ch.configured_in = true;
                    ch.remote
                });
                let mut r = remote.unwrap_or(0).to_le_bytes().to_vec();
                r.extend_from_slice(&0u16.to_le_bytes()); // flags
                // Accept whatever they asked for (MTU, flush timeout).
                r.extend_from_slice(&0u16.to_le_bytes()); // success
                if remote.is_some() {
                    self.send_signal(conn, CONFIGURE_RESPONSE, Some(id), &r);
                    self.check_open(conn, local);
                } else {
                    // Invalid CID in request.
                    self.send_signal(conn, COMMAND_REJECT, Some(id), &[0x02, 0x00, d[0], d[1], 0, 0]);
                }
            }
            CONFIGURE_RESPONSE if d.len() >= 6 => {
                let (local, result) = (le16(0), le16(4));
                if result == 0 {
                    if let Some(ch) = self.conns[conn].channel(local) {
                        ch.configured_out = true;
                    }
                    self.check_open(conn, local);
                } else {
                    let a = self.conns[conn].address;
                    self.pair_failed(&a, alloc::format!("channel configuration refused (result {})", result));
                }
            }
            DISCONNECTION_REQUEST if d.len() >= 4 => {
                let (local, remote) = (le16(0), le16(2));
                self.conns[conn].channels.retain(|ch| ch.local != local);
                let mut r = local.to_le_bytes().to_vec();
                r.extend_from_slice(&remote.to_le_bytes());
                self.send_signal(conn, DISCONNECTION_RESPONSE, Some(id), &r);
            }
            DISCONNECTION_RESPONSE if d.len() >= 4 => {
                let local = le16(2);
                self.conns[conn].channels.retain(|ch| ch.local != local);
            }
            ECHO_REQUEST => self.send_signal(conn, ECHO_RESPONSE, Some(id), d),
            INFORMATION_REQUEST if d.len() >= 2 => {
                let kind = le16(0);
                let mut r = kind.to_le_bytes().to_vec();
                match kind {
                    // Extended features: none (basic mode only).
                    2 => r.extend_from_slice(&[0, 0, 0, 0, 0, 0]),
                    // Fixed channels: signalling only.
                    3 => r.extend_from_slice(&[0, 0, 0x02, 0, 0, 0, 0, 0, 0, 0]),
                    _ => r.extend_from_slice(&[1, 0]), // not supported
                }
                self.send_signal(conn, INFORMATION_RESPONSE, Some(id), &r);
            }
            COMMAND_REJECT | INFORMATION_RESPONSE | ECHO_RESPONSE => {}
            _ => self.send_signal(conn, COMMAND_REJECT, Some(id), &[0, 0]), // not understood
        }
    }

    /// Accepts (HID channels) or refuses an incoming channel request.
    fn accept_channel(&mut self, conn: usize, psm: u16, remote: u16, id: u8) {
        if !matches!(psm, PSM_HID_CONTROL | PSM_HID_INTERRUPT) {
            self.connection_response(conn, id, 0, remote, 2, 0); // PSM not supported
            return;
        }
        let c = &mut self.conns[conn];
        let local = c.next_cid;
        c.next_cid += 1;
        c.channels.retain(|ch| ch.psm != psm);
        c.channels.push(Channel { psm, local, remote, configured_out: false, configured_in: false, open: false });
        self.connection_response(conn, id, local, remote, 0, 0);
        self.send_configure(conn, remote);
    }

    fn connection_response(&mut self, conn: usize, id: u8, local: u16, remote: u16, result: u16, status: u16) {
        let mut r = local.to_le_bytes().to_vec();
        r.extend_from_slice(&remote.to_le_bytes());
        r.extend_from_slice(&result.to_le_bytes());
        r.extend_from_slice(&status.to_le_bytes());
        self.send_signal(conn, CONNECTION_RESPONSE, Some(id), &r);
    }

    /// Encryption is on: answer the channel requests that were waiting for it.
    pub(super) fn release_held(&mut self, conn: usize) {
        for (psm, remote, id) in core::mem::take(&mut self.conns[conn].held) {
            self.accept_channel(conn, psm, remote, id);
        }
    }

    /// Both directions configured: the channel is open; start what runs on it.
    fn check_open(&mut self, conn: usize, local: u16) {
        let Some(ch) = self.conns[conn].channel(local) else { return };
        if ch.open || !ch.configured_in || !ch.configured_out {
            return;
        }
        ch.open = true;
        let psm = ch.psm;
        match psm {
            sdp::PSM => {
                let c = &mut self.conns[conn];
                c.sdp_transaction = 1;
                c.sdp_lists.clear();
                let req = sdp::request(1, &[]);
                self.send_frame(conn, local_remote(&self.conns[conn], local), &req);
            }
            PSM_HID_CONTROL => {
                if self.conns[conn].outgoing {
                    self.connect_channel(conn, PSM_HID_INTERRUPT);
                }
            }
            PSM_HID_INTERRUPT => {
                let c = &mut self.conns[conn];
                if !c.ready {
                    c.ready = true;
                    self.hid_ready(conn);
                }
            }
            _ => {}
        }
    }

    /// After pairing: read the HID report descriptor over SDP first.
    pub(super) fn start_sdp(&mut self, conn: usize) {
        if !self.conns[conn].layout.is_empty() {
            self.connect_channel(conn, PSM_HID_CONTROL);
        } else {
            self.connect_channel(conn, sdp::PSM);
        }
    }

    fn on_channel_data(&mut self, conn: usize, local: u16, data: &[u8]) {
        let Some(psm) = self.conns[conn].channel(local).filter(|ch| ch.open).map(|ch| ch.psm) else { return };
        match psm {
            sdp::PSM => self.on_sdp(conn, local, data),
            PSM_HID_INTERRUPT | PSM_HID_CONTROL if data.first() == Some(&HID_DATA_INPUT) => {
                let c = &mut self.conns[conn];
                let mut pads = GAMEPADS.lock();
                if let Some(g) = pads.iter_mut().find(|g| g.address == c.address) {
                    if hid::decode(&c.layout, &data[1..], &mut g.pad) {
                        g.reports += 1;
                    }
                }
            }
            _ => {}
        }
    }

    fn on_sdp(&mut self, conn: usize, local: u16, pdu: &[u8]) {
        let c = &mut self.conns[conn];
        let mut lists = core::mem::take(&mut c.sdp_lists);
        let result = sdp::response(pdu, &mut lists);
        c.sdp_lists = lists;
        match result {
            Ok(cont) if !cont.is_empty() => {
                c.sdp_transaction = c.sdp_transaction.wrapping_add(1);
                let req = sdp::request(c.sdp_transaction, &cont);
                let remote = local_remote(c, local);
                self.send_frame(conn, remote, &req);
            }
            Ok(_) => {
                let descriptor = sdp::report_descriptor(&c.sdp_lists);
                let address = c.address;
                self.close_channel(conn, local);
                match descriptor {
                    Some(d) if !hid::parse(&d).is_empty() => {
                        self.conns[conn].layout = hid::parse(&d);
                        if let Some(b) = BONDS.lock().iter_mut().find(|b| b.address == address) {
                            b.descriptor = d;
                        }
                        self.connect_channel(conn, PSM_HID_CONTROL);
                    }
                    Some(_) => self.pair_failed(&address, "it has no gamepad controls (not a gamepad?)".into()),
                    None => self.pair_failed(&address, "it does not offer the HID service (not a gamepad?)".into()),
                }
            }
            Err(e) => {
                let address = c.address;
                self.close_channel(conn, local);
                self.pair_failed(&address, alloc::format!("SDP: {}", e));
            }
        }
    }
}

fn local_remote(c: &Conn, local: u16) -> u16 {
    c.channels.iter().find(|ch| ch.local == local).map_or(0, |ch| ch.remote)
}
