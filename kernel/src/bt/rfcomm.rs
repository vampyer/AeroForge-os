//! RFCOMM (a TS 07.10 multiplexer over one L2CAP channel), basic option with
//! credit-based flow control, as either side: we start a session to reach a
//! headset's hands-free channel, and accept one when a headset reconnects to
//! our audio gateway. Only one data channel (DLC) per session is used.

use alloc::vec::Vec;

pub const PSM: u16 = 0x0003;

const SABM: u8 = 0x2F;
const UA: u8 = 0x63;
const DM: u8 = 0x0F;
const DISC: u8 = 0x43;
const UIH: u8 = 0xEF;
const PF: u8 = 0x10;

// Multiplexer control commands (type field without the C/R and EA bits).
const MCC_PN: u8 = 0x20;
const MCC_MSC: u8 = 0x38;
const MCC_RPN: u8 = 0x24;
const MCC_TEST: u8 = 0x08;

/// Frame size we offer, well inside L2CAP's default MTU of 672.
const FRAME_SIZE: u16 = 127;
const CREDITS: u8 = 7;

pub enum Event {
    /// The data channel is open.
    Open,
    Data(Vec<u8>),
    /// The session or the channel failed or was closed.
    Closed(&'static str),
}

pub struct Session {
    initiator: bool,
    mux_up: bool,
    /// Our data channel's DLCI (server channel * 2 + direction bit).
    dlci: u8,
    open: bool,
    credit_flow: bool,
    /// Frames the peer still lets us send / we still let it send.
    tx_credits: u16,
    rx_credits: u16,
    /// Frames to send on the L2CAP channel.
    pub out: Vec<Vec<u8>>,
    pub events: Vec<Event>,
    /// Data waiting for credits.
    pending: Vec<Vec<u8>>,
}

fn crc_table() -> [u8; 256] {
    let mut t = [0u8; 256];
    for (i, e) in t.iter_mut().enumerate() {
        let mut c = i as u8;
        for _ in 0..8 {
            c = if c & 1 != 0 { (c >> 1) ^ 0xE0 } else { c >> 1 };
        }
        *e = c;
    }
    t
}

fn fcs(bytes: &[u8]) -> u8 {
    let t = crc_table();
    0xFF - bytes.iter().fold(0xFFu8, |crc, &b| t[(crc ^ b) as usize])
}

impl Session {
    /// A session we start, to reach `channel` on the remote device.
    pub fn connect(channel: u8) -> Self {
        let mut s = Session::new(true, channel << 1);
        s.send_frame(0, true, SABM | PF, &[], None);
        s
    }

    /// A session the remote device starts.
    pub fn accept() -> Self {
        Session::new(false, 0)
    }

    fn new(initiator: bool, dlci: u8) -> Self {
        Session { initiator, mux_up: false, dlci, open: false, credit_flow: false, tx_credits: 0, rx_credits: 0,
            out: Vec::new(), events: Vec::new(), pending: Vec::new() }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// C/R bit in the address: commands from the initiator and responses
    /// from the responder carry 1.
    fn cr(&self, command: bool) -> u8 {
        (command == self.initiator) as u8
    }

    fn send_frame(&mut self, dlci: u8, command: bool, control: u8, info: &[u8], credits: Option<u8>) {
        let address = (dlci << 2) | (self.cr(command) << 1) | 1;
        let control = if credits.is_some() { control | PF } else { control };
        let mut f = alloc::vec![address, control];
        if info.len() <= 127 {
            f.push(((info.len() as u8) << 1) | 1);
        } else {
            f.push((info.len() << 1) as u8 & 0xFE);
            f.push((info.len() >> 7) as u8);
        }
        // UIH checks only address and control; the others the length too.
        let checked = if control & !PF == UIH { 2 } else { f.len() };
        let check = fcs(&f[..checked]);
        if let Some(c) = credits {
            f.push(c);
        }
        f.extend_from_slice(info);
        f.push(check);
        self.out.push(f);
    }

    fn send_mcc(&mut self, kind: u8, command: bool, value: &[u8]) {
        let mut m = alloc::vec![(kind << 2) | ((command as u8) << 1) | 1, ((value.len() as u8) << 1) | 1];
        m.extend_from_slice(value);
        self.send_frame(0, true, UIH, &m, None);
    }

    fn pn_value(&self, dlci: u8, cl: u8, credits: u8) -> [u8; 8] {
        [dlci, cl, 0, 0, FRAME_SIZE as u8, (FRAME_SIZE >> 8) as u8, 0, credits]
    }

    fn send_msc(&mut self, command: bool) {
        let dlci = self.dlci;
        // Address octet (EA, C/R set), then RTC, RTR and DV set.
        self.send_mcc(MCC_MSC, command, &[(dlci << 2) | 0x03, 0x8D]);
    }

    /// Sends data on the open channel (queued until the peer gives credits).
    pub fn send(&mut self, data: &[u8]) {
        self.pending.push(data.to_vec());
        self.flush();
    }

    fn flush(&mut self) {
        while self.open && !self.pending.is_empty() && (!self.credit_flow || self.tx_credits > 0) {
            let d = self.pending.remove(0);
            if self.credit_flow {
                self.tx_credits -= 1;
            }
            let dlci = self.dlci;
            self.send_frame(dlci, true, UIH, &d, None);
        }
    }

    /// One L2CAP frame from the peer.
    pub fn receive(&mut self, f: &[u8]) {
        if f.len() < 4 {
            return;
        }
        let dlci = f[0] >> 2;
        let control = f[1];
        let (len, header) = if f[2] & 1 != 0 { ((f[2] >> 1) as usize, 3) } else { ((f[2] >> 1) as usize | (f[3] as usize) << 7, 4) };
        let credit_byte = control == UIH | PF && self.credit_flow && dlci != 0;
        let start = header + credit_byte as usize;
        if f.len() < start + len + 1 {
            return;
        }
        let info = &f[start..start + len];
        match control & !PF {
            SABM => {
                if dlci == 0 {
                    self.mux_up = true;
                    self.send_frame(0, false, UA | PF, &[], None);
                } else if !self.initiator && dlci >> 1 == super::sdp::AG_CHANNEL {
                    self.dlci = dlci;
                    self.send_frame(dlci, false, UA | PF, &[], None);
                    self.open = true;
                    self.send_msc(true);
                    self.events.push(Event::Open);
                } else {
                    self.send_frame(dlci, false, DM | PF, &[], None);
                }
            }
            UA => {
                if dlci == 0 && self.initiator && !self.mux_up {
                    self.mux_up = true;
                    let pn = self.pn_value(self.dlci, 0xF0, CREDITS);
                    self.rx_credits = CREDITS as u16;
                    self.send_mcc(MCC_PN, true, &pn);
                } else if dlci == self.dlci && dlci != 0 && !self.open {
                    self.open = true;
                    self.send_msc(true);
                    self.events.push(Event::Open);
                    self.flush();
                }
            }
            DM => {
                self.open = false;
                self.events.push(Event::Closed(if dlci == 0 { "RFCOMM refused" } else { "the device refused the audio channel" }));
            }
            DISC => {
                self.send_frame(dlci, false, UA | PF, &[], None);
                if dlci == 0 || dlci == self.dlci {
                    self.open = false;
                    self.events.push(Event::Closed("the device closed the audio channel"));
                }
            }
            UIH if dlci == 0 => self.on_mcc(info),
            UIH => {
                if credit_byte {
                    self.tx_credits += f[header] as u16;
                }
                if !info.is_empty() && dlci == self.dlci {
                    self.events.push(Event::Data(info.to_vec()));
                    if self.credit_flow {
                        self.rx_credits = self.rx_credits.saturating_sub(1);
                        if self.rx_credits < 3 {
                            let more = CREDITS - self.rx_credits as u8;
                            self.rx_credits += more as u16;
                            self.send_frame(dlci, true, UIH, &[], Some(more));
                        }
                    }
                }
                self.flush();
            }
            _ => {}
        }
    }

    fn on_mcc(&mut self, mut m: &[u8]) {
        while m.len() >= 2 {
            let kind = m[0] >> 2;
            let command = m[0] & 2 != 0;
            let len = (m[1] >> 1) as usize;
            if m.len() < 2 + len {
                return;
            }
            let v = &m[2..2 + len];
            match kind {
                MCC_PN if v.len() >= 8 => {
                    if command {
                        // The peer proposes the channel's parameters: accept
                        // its frame size (or ours if smaller) and credits.
                        self.dlci = v[0] & 0x3F;
                        self.credit_flow = v[1] == 0xF0;
                        self.tx_credits = (v[7] & 7) as u16;
                        self.rx_credits = CREDITS as u16;
                        let mut pn = self.pn_value(self.dlci, if self.credit_flow { 0xE0 } else { 0 }, CREDITS);
                        let n1 = u16::from_le_bytes([v[4], v[5]]).clamp(23, FRAME_SIZE);
                        pn[4..6].copy_from_slice(&n1.to_le_bytes());
                        self.send_mcc(MCC_PN, false, &pn);
                    } else if self.initiator {
                        self.credit_flow = v[1] == 0xE0;
                        self.tx_credits = (v[7] & 7) as u16;
                        let dlci = self.dlci;
                        self.send_frame(dlci, true, SABM | PF, &[], None);
                    }
                }
                MCC_MSC if command => {
                    // Echo the peer's modem status back as the response.
                    self.send_mcc(MCC_MSC, false, v);
                }
                MCC_RPN | MCC_TEST if command => {
                    let echo = v.to_vec();
                    self.send_mcc(kind, false, &echo);
                }
                _ => {}
            }
            m = &m[2 + len..];
        }
    }
}
