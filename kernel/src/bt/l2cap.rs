//! L2CAP over an ACL connection: the signalling channel, connection-oriented
//! channels in basic mode, and what runs on them: SDP to find out what the
//! device is (a gamepad's HID report descriptor, or a headset's hands-free
//! RFCOMM channel), then the HID control and interrupt channels, or RFCOMM
//! carrying the hands-free AT commands. We also answer SDP queries, since a
//! headset that reconnects looks up our audio gateway first.

use alloc::vec::Vec;

use super::{hfp, hid, le, rfcomm, sdp, Host, BONDS, GAMEPADS};

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
    /// We opened it (else the device did).
    ours: bool,
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
    /// The service the running SDP query looks for.
    sdp_uuid: u16,
    /// A headset: its hands-free RFCOMM channel and whether it only has the
    /// older Headset profile.
    pub audio: Option<(u8, bool)>,
    /// RFCOMM session (on the channel with this local CID) and the
    /// hands-free commands running on it.
    rfcomm: Option<(u16, rfcomm::Session)>,
    ag: Option<hfp::Ag>,
    pub audio_ready: bool,
    /// A reconnected headset that has not opened RFCOMM by then gets it
    /// opened by us.
    pub rfcomm_deadline: Option<u64>,
    /// An LE link: pairing and GATT state.
    pub le: Option<alloc::boxed::Box<le::Le>>,
}

impl Conn {
    pub fn new(handle: u16, address: [u8; 6], outgoing: bool) -> Self {
        // A known device keeps the descriptor read when it was paired.
        let (layout, audio) = BONDS.lock().iter().find(|b| b.address == address)
            .map(|b| (if b.descriptor.is_empty() { hid::Layout::default() } else { hid::parse(&b.descriptor) }, b.audio))
            .unwrap_or_default();
        Conn { handle, address, outgoing, encrypted: false, layout, channels: Vec::new(), rx: Vec::new(),
            next_cid: FIRST_DYNAMIC_CID, next_id: 1, sdp_transaction: 0, sdp_lists: Vec::new(), ready: false,
            held: Vec::new(), sdp_uuid: 0, audio, rfcomm: None, ag: None, audio_ready: false, rfcomm_deadline: None, le: None }
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
        if self.conns[i].le.is_some() {
            match cid {
                le::ATT_CID => self.on_att(i, payload),
                le::SIGNAL_CID => self.on_le_signal(i, payload),
                le::SMP_CID => self.on_smp(i, payload),
                _ => {}
            }
        } else if cid == SIGNALLING_CID {
            self.on_signalling(i, payload);
        } else {
            self.on_channel_data(i, cid, payload);
        }
    }

    pub(super) fn send_frame(&mut self, conn: usize, cid: u16, payload: &[u8]) {
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
        c.channels.push(Channel { psm, ours: true, local, remote: 0, configured_out: false, configured_in: false, open: false });
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
                // The device opening its HID channels (or RFCOMM) after a
                // reconnect, or querying our SDP records.
                let (psm, remote) = (le16(0), le16(2));
                if matches!(psm, PSM_HID_CONTROL | PSM_HID_INTERRUPT | rfcomm::PSM) && !self.conns[conn].encrypted {
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
                self.channel_closed(conn, local);
                let mut r = local.to_le_bytes().to_vec();
                r.extend_from_slice(&remote.to_le_bytes());
                self.send_signal(conn, DISCONNECTION_RESPONSE, Some(id), &r);
            }
            DISCONNECTION_RESPONSE if d.len() >= 4 => {
                let local = le16(2);
                self.channel_closed(conn, local);
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

    fn channel_closed(&mut self, conn: usize, local: u16) {
        let c = &mut self.conns[conn];
        c.channels.retain(|ch| ch.local != local);
        if c.rfcomm.as_ref().is_some_and(|r| r.0 == local) {
            c.rfcomm = None;
            c.ag = None;
            if c.audio_ready {
                c.audio_ready = false;
                self.audio_lost(conn);
            }
        }
    }

    /// Accepts (HID, SDP, RFCOMM) or refuses an incoming channel request.
    fn accept_channel(&mut self, conn: usize, psm: u16, remote: u16, id: u8) {
        if !matches!(psm, PSM_HID_CONTROL | PSM_HID_INTERRUPT | sdp::PSM | rfcomm::PSM) {
            self.connection_response(conn, id, 0, remote, 2, 0); // PSM not supported
            return;
        }
        let c = &mut self.conns[conn];
        let local = c.next_cid;
        c.next_cid += 1;
        c.channels.retain(|ch| ch.psm != psm);
        c.channels.push(Channel { psm, ours: false, local, remote, configured_out: false, configured_in: false, open: false });
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
        let (psm, ours) = (ch.psm, ch.ours);
        match psm {
            sdp::PSM if ours => self.sdp_query(conn, local, sdp::HID_SERVICE),
            rfcomm::PSM => {
                let c = &mut self.conns[conn];
                let headset = c.audio.is_some_and(|a| a.1);
                let session = match c.audio {
                    Some((channel, _)) if ours => rfcomm::Session::connect(channel),
                    _ => rfcomm::Session::accept(),
                };
                c.rfcomm = Some((local, session));
                c.ag = Some(hfp::Ag::new(headset));
                c.rfcomm_deadline = None;
                self.rfcomm_pump(conn);
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

    /// After pairing: find out over SDP what the device is (unless that is
    /// known from before), then open its HID channels or RFCOMM.
    pub(super) fn start_sdp(&mut self, conn: usize) {
        let c = &self.conns[conn];
        if !c.layout.is_empty() {
            self.connect_channel(conn, PSM_HID_CONTROL);
        } else if c.audio.is_some() {
            self.open_rfcomm(conn);
        } else {
            self.connect_channel(conn, sdp::PSM);
        }
    }

    pub(super) fn open_rfcomm(&mut self, conn: usize) {
        let c = &mut self.conns[conn];
        if c.rfcomm.is_none() && !c.channels.iter().any(|ch| ch.psm == rfcomm::PSM) {
            c.rfcomm_deadline = None;
            self.connect_channel(conn, rfcomm::PSM);
        }
    }

    /// Asks the device for its records of service `uuid`: the HID
    /// descriptor list for HID, the protocol list (RFCOMM channel) otherwise.
    fn sdp_query(&mut self, conn: usize, local: u16, uuid: u16) {
        let c = &mut self.conns[conn];
        c.sdp_uuid = uuid;
        c.sdp_transaction = c.sdp_transaction.wrapping_add(1);
        c.sdp_lists.clear();
        let req = sdp::request(c.sdp_transaction, uuid, sdp_attribute(uuid), &[]);
        let remote = local_remote(c, local);
        self.send_frame(conn, remote, &req);
    }

    /// Sends what the RFCOMM session queued and handles what it reported.
    fn rfcomm_pump(&mut self, conn: usize) {
        loop {
            let c = &mut self.conns[conn];
            let Some((local, s)) = c.rfcomm.as_mut() else { return };
            let local = *local;
            let out = core::mem::take(&mut s.out);
            let events = core::mem::take(&mut s.events);
            if out.is_empty() && events.is_empty() {
                return;
            }
            let remote = local_remote(c, local);
            for f in out {
                self.send_frame(conn, remote, &f);
            }
            for e in events {
                let c = &mut self.conns[conn];
                match e {
                    rfcomm::Event::Open => {}
                    rfcomm::Event::Data(d) => {
                        let (Some(ag), Some((_, s))) = (c.ag.as_mut(), c.rfcomm.as_mut()) else { continue };
                        for r in ag.feed(&d) {
                            s.send(&r);
                        }
                    }
                    rfcomm::Event::Closed(why) => {
                        let address = c.address;
                        self.close_channel(conn, local);
                        self.channel_closed(conn, local);
                        self.pair_failed(&address, why.into());
                        return;
                    }
                }
            }
            let c = &mut self.conns[conn];
            let open = c.rfcomm.as_ref().is_some_and(|r| r.1.is_open());
            if open && c.ag.as_ref().is_some_and(|a| a.ready) && !c.audio_ready {
                c.audio_ready = true;
                self.audio_ready(conn);
            }
        }
    }

    fn on_channel_data(&mut self, conn: usize, local: u16, data: &[u8]) {
        let Some(psm) = self.conns[conn].channel(local).filter(|ch| ch.open).map(|ch| ch.psm) else { return };
        let ours = self.conns[conn].channel(local).is_some_and(|ch| ch.ours);
        match psm {
            sdp::PSM if ours => self.on_sdp(conn, local, data),
            sdp::PSM => {
                let reply = sdp::serve(data);
                let remote = local_remote(&self.conns[conn], local);
                self.send_frame(conn, remote, &reply);
            }
            rfcomm::PSM => {
                if let Some((_, s)) = self.conns[conn].rfcomm.as_mut() {
                    s.receive(data);
                }
                self.rfcomm_pump(conn);
            }
            PSM_HID_INTERRUPT | PSM_HID_CONTROL if data.first() == Some(&HID_DATA_INPUT) => {
                let c = &mut self.conns[conn];
                let mut pads = GAMEPADS.lock();
                if let Some(g) = pads.iter_mut().find(|g| g.usb.is_none() && g.address == c.address) {
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
                let req = sdp::request(c.sdp_transaction, c.sdp_uuid, sdp_attribute(c.sdp_uuid), &cont);
                let remote = local_remote(c, local);
                self.send_frame(conn, remote, &req);
            }
            Ok(_) if c.sdp_uuid == sdp::HID_SERVICE => {
                let descriptor = sdp::report_descriptor(&c.sdp_lists);
                let address = c.address;
                match descriptor {
                    Some(d) if !hid::parse(&d).is_empty() => {
                        self.close_channel(conn, local);
                        self.conns[conn].layout = hid::parse(&d);
                        if let Some(b) = BONDS.lock().iter_mut().find(|b| b.address == address) {
                            b.descriptor = d;
                        }
                        self.connect_channel(conn, PSM_HID_CONTROL);
                    }
                    Some(_) => {
                        self.close_channel(conn, local);
                        self.pair_failed(&address, "it has no gamepad controls (not a gamepad?)".into());
                    }
                    // Not a HID device: perhaps a headset.
                    None => self.sdp_query(conn, local, sdp::HANDSFREE),
                }
            }
            Ok(_) => {
                let channel = sdp::rfcomm_channel(&c.sdp_lists);
                let (address, uuid) = (c.address, c.sdp_uuid);
                match channel {
                    Some(ch) => {
                        self.close_channel(conn, local);
                        let audio = Some((ch, uuid == sdp::HEADSET));
                        self.conns[conn].audio = audio;
                        if let Some(b) = BONDS.lock().iter_mut().find(|b| b.address == address) {
                            b.audio = audio;
                        }
                        self.open_rfcomm(conn);
                    }
                    None if uuid == sdp::HANDSFREE => self.sdp_query(conn, local, sdp::HEADSET),
                    None => {
                        self.close_channel(conn, local);
                        self.pair_failed(&address, "it is neither a gamepad nor a headset (no HID or hands-free service)".into());
                    }
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

fn sdp_attribute(uuid: u16) -> u16 {
    if uuid == sdp::HID_SERVICE { sdp::HID_DESCRIPTOR_LIST } else { sdp::PROTOCOL_DESCRIPTORS }
}

fn local_remote(c: &Conn, local: u16) -> u16 {
    c.channels.iter().find(|ch| ch.local == local).map_or(0, |ch| ch.remote)
}
