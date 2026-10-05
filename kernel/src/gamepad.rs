//! What user programs see of the gamepads (Bluetooth and USB, bt::GAMEPADS):
//! the raw HID state plus the same pad in the Xbox 360 layout that games
//! expect. For XInput pads that layout is exact; for other pads it is a
//! guess (buttons 1 to 11 as A B X Y LB RB Back Start LS RS Guide, the hat as
//! the d-pad) until a controller mapping database exists.

use crate::bt;

/// The layout of the user's buffer; keep in sync with userland/src/lib.rs.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct State {
    pub name: [u8; 32],
    pub connected: u8,
    /// 1 = Bluetooth, 2 = USB.
    pub link: u8,
    /// 1 if the Xbox layout below is exact, 0 if it is a guess.
    pub exact: u8,
    /// Raw hat: 0 = up, clockwise to 7 = up-left, 8 = centred.
    pub hat: u8,
    /// Raw buttons: bit n = HID button n + 1.
    pub buttons: u32,
    /// Input reports received so far.
    pub reports: u32,
    /// Which raw axes the pad has (bit per X Y Z Rx Ry Rz Gas Brake).
    pub axis_mask: u8,
    pub _pad: [u8; 3],
    /// Raw axes, -127 (left/up) to 127 (right/down).
    pub axes: [i16; 8],
    /// Xbox layout: XInput button bits (d-pad up 0x1, down 0x2, left 0x4,
    /// right 0x8, Start 0x10, Back 0x20, LS 0x40, RS 0x80, LB 0x100, RB
    /// 0x200, Guide 0x400, A 0x1000, B 0x2000, X 0x4000, Y 0x8000).
    pub xbuttons: u16,
    pub left_trigger: u8,
    pub right_trigger: u8,
    /// Left X, left Y, right X, right Y: -32767 to 32767, up positive.
    pub thumbs: [i16; 4],
}

impl State {
    pub fn as_bytes(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self as *const Self as *const u8, core::mem::size_of::<Self>()) }
    }
}

const X: usize = 0;
const Y: usize = 1;
const Z: usize = 2;
const RX: usize = 3;
const RY: usize = 4;
const RZ: usize = 5;
const GAS: usize = 6;
const BRAKE: usize = 7;

/// Button n + 1 to its XInput bit, in the order the XInput translation uses.
const XBUTTONS: [u16; 11] = [0x1000, 0x2000, 0x4000, 0x8000, 0x0100, 0x0200, 0x0020, 0x0010, 0x0040, 0x0080, 0x0400];
/// Hat position to d-pad bits.
const DPAD: [u16; 8] = [0x1, 0x1 | 0x8, 0x8, 0x2 | 0x8, 0x2, 0x2 | 0x4, 0x4, 0x1 | 0x4];

fn thumb(v: i16, flip: bool) -> i16 {
    let v = (v as i32 * 258).clamp(-32767, 32767) as i16;
    if flip { -v } else { v }
}

fn trigger(v: i16) -> u8 {
    ((v as i32 + 127) * 255 / 254).clamp(0, 255) as u8
}

/// Gamepad `index` and how many there are.
pub fn read(index: usize) -> Option<(State, usize)> {
    let pads = bt::GAMEPADS.lock();
    let g = pads.get(index)?;
    let mut s = State::default();
    let name = g.name.as_bytes();
    let n = name.len().min(s.name.len() - 1);
    s.name[..n].copy_from_slice(&name[..n]);
    s.connected = g.connected as u8;
    s.link = if g.usb.is_some() { 2 } else { 1 };
    s.exact = g.xinput as u8;
    s.hat = g.pad.hat.unwrap_or(8);
    s.buttons = g.pad.buttons;
    s.reports = g.reports as u32;
    s.axis_mask = g.axes;
    s.axes = g.pad.axes;

    for (n, bit) in XBUTTONS.iter().enumerate() {
        if g.pad.buttons & (1 << n) != 0 {
            s.xbuttons |= bit;
        }
    }
    if let Some(h) = g.pad.hat {
        s.xbuttons |= DPAD[h as usize & 7];
    }
    let has = |a: usize| g.axes & (1 << a) != 0;
    // XInput pads: right stick on Rx/Ry, triggers on Z/Rz. Most other pads
    // put the right stick on Z/Rz and the triggers (if analog) on Gas/Brake.
    let (right, triggers) = if g.xinput || !(has(Z) && has(RZ)) {
        ((RX, RY), (Z, RZ))
    } else {
        ((Z, RZ), (GAS, BRAKE))
    };
    s.thumbs = [thumb(s.axes[X], false), thumb(s.axes[Y], true), thumb(s.axes[right.0], false),
        thumb(s.axes[right.1], true)];
    if has(triggers.0) {
        s.left_trigger = trigger(s.axes[triggers.0]);
    }
    if has(triggers.1) {
        s.right_trigger = trigger(s.axes[triggers.1]);
    }
    Some((s, pads.len()))
}
