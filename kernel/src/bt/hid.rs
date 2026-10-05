//! HID report descriptors and gamepad state. A Bluetooth gamepad describes
//! its input reports with a USB HID report descriptor (fetched over SDP);
//! this parser finds the axes, the hat switch and the buttons in it and then
//! decodes each input report into a `Pad`.

use alloc::string::String;
use alloc::vec::Vec;

pub const MAX_AXES: usize = 8;
pub const AXIS_NAMES: [&str; MAX_AXES] = ["X", "Y", "Z", "Rx", "Ry", "Rz", "Gas", "Brake"];

#[derive(Clone, Copy, PartialEq)]
enum Use {
    Axis(usize),
    Hat,
    Button(u32),
    /// An array slot holding the index of a pressed button; `first` is the
    /// button usage that logical minimum stands for.
    ButtonArray { first: u32 },
}

#[derive(Clone, Copy)]
struct Field {
    report_id: u8,
    offset: u32,
    size: u32,
    use_: Use,
    min: i32,
    max: i32,
}

/// The input fields of one device.
#[derive(Clone, Default)]
pub struct Layout {
    fields: Vec<Field>,
    pub uses_report_ids: bool,
}

impl Layout {
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    pub fn axis_mask(&self) -> u8 {
        self.fields.iter().fold(0, |m, f| match f.use_ {
            Use::Axis(a) => m | 1 << a,
            _ => m,
        })
    }

    pub fn axes(&self) -> usize {
        self.axis_mask().count_ones() as usize
    }

    pub fn buttons(&self) -> u32 {
        self.fields.iter().filter_map(|f| match f.use_ {
            Use::Button(n) => Some(n + 1),
            Use::ButtonArray { first } => Some(first + (f.max - f.min).max(0) as u32),
            _ => None,
        }).max().unwrap_or(0)
    }

    pub fn has_hat(&self) -> bool {
        self.fields.iter().any(|f| f.use_ == Use::Hat)
    }

    pub fn summary(&self) -> String {
        alloc::format!("{} axes{}, {} buttons", self.axes(), if self.has_hat() { ", hat" } else { "" }, self.buttons())
    }
}

/// Parses a report descriptor. Only input items matter; collections are
/// flattened, which is enough for game controllers.
pub fn parse(desc: &[u8]) -> Layout {
    let mut layout = Layout::default();
    // Global state, with a small push/pop stack.
    #[derive(Clone, Copy, Default)]
    struct Global {
        page: u32,
        min: i32,
        max: i32,
        size: u32,
        count: u32,
        id: u8,
    }
    let mut g = Global::default();
    let mut stack: Vec<Global> = Vec::new();
    let mut usages: Vec<u32> = Vec::new(); // page << 16 | usage
    let (mut umin, mut umax): (Option<u32>, Option<u32>) = (None, None);
    let mut offsets: Vec<(u8, u32)> = Vec::new(); // bit offset per report id

    let mut i = 0;
    while i < desc.len() {
        let b = desc[i];
        if b == 0xFE {
            // Long item: size in the next byte.
            let n = desc.get(i + 1).copied().unwrap_or(0) as usize;
            i += 3 + n;
            continue;
        }
        let size = [0, 1, 2, 4][(b & 3) as usize];
        if i + 1 + size > desc.len() {
            break;
        }
        let data = &desc[i + 1..i + 1 + size];
        let unsigned = data.iter().rev().fold(0u32, |v, &x| v << 8 | x as u32);
        let signed = match size {
            1 => data[0] as i8 as i32,
            2 => i16::from_le_bytes([data[0], data[1]]) as i32,
            4 => unsigned as i32,
            _ => 0,
        };
        let (kind, tag) = ((b >> 2) & 3, b >> 4);
        match (kind, tag) {
            (1, 0) => g.page = unsigned,
            (1, 1) => g.min = signed,
            // A maximum that only fits unsigned (0..255 written as FF) is unsigned.
            (1, 2) => g.max = if signed < g.min { unsigned as i32 } else { signed },
            (1, 7) => g.size = unsigned,
            (1, 8) => {
                g.id = unsigned as u8;
                layout.uses_report_ids = true;
            }
            (1, 9) => g.count = unsigned,
            (1, 10) => stack.push(g),
            (1, 11) => g = stack.pop().unwrap_or(g),
            (2, 0) => usages.push(if size == 4 { unsigned } else { g.page << 16 | unsigned }),
            (2, 1) => umin = Some(if size == 4 { unsigned } else { g.page << 16 | unsigned }),
            (2, 2) => umax = Some(if size == 4 { unsigned } else { g.page << 16 | unsigned }),
            (0, 8) => {
                // Input item.
                let slot = match offsets.iter().position(|o| o.0 == g.id) {
                    Some(p) => p,
                    None => {
                        offsets.push((g.id, 0));
                        offsets.len() - 1
                    }
                };
                let constant = unsigned & 1 != 0;
                let variable = unsigned & 2 != 0;
                let base = offsets[slot].1;
                if !constant {
                    let mut list = usages.clone();
                    if let (Some(lo), Some(hi)) = (umin, umax) {
                        if hi >= lo && hi - lo < 256 {
                            list.extend(lo..=hi);
                        }
                    }
                    if variable {
                        for k in 0..g.count {
                            let usage = list.get(k as usize).or(list.last()).copied();
                            if let Some(u) = usage.and_then(classify) {
                                layout.fields.push(Field { report_id: g.id, offset: base + k * g.size, size: g.size,
                                    use_: u, min: g.min, max: g.max });
                            }
                        }
                    } else if let (Some(lo), Some(_)) = (umin, umax) {
                        // Array of button indices: each slot names a pressed button.
                        if lo >> 16 == 9 && g.size <= 16 {
                            for k in 0..g.count {
                                layout.fields.push(Field { report_id: g.id, offset: base + k * g.size, size: g.size,
                                    use_: Use::ButtonArray { first: lo & 0xFFFF }, min: g.min, max: g.max });
                            }
                        }
                    }
                }
                offsets[slot].1 = base + g.size * g.count;
                usages.clear();
                (umin, umax) = (None, None);
            }
            (0, _) => {
                usages.clear();
                (umin, umax) = (None, None);
            }
            _ => {}
        }
        i += 1 + size;
    }
    layout
}

fn classify(usage: u32) -> Option<Use> {
    match (usage >> 16, usage & 0xFFFF) {
        (1, u @ 0x30..=0x35) => Some(Use::Axis((u - 0x30) as usize)),
        (1, 0x39) => Some(Use::Hat),
        (2, 0xC4) => Some(Use::Axis(6)),
        (2, 0xC5) => Some(Use::Axis(7)),
        (9, n @ 1..=32) => Some(Use::Button(n - 1)),
        _ => None,
    }
}

/// The current state of a gamepad.
#[derive(Clone, Copy, Default)]
pub struct Pad {
    pub buttons: u32,
    /// -127 (left/up) to 127 (right/down), 0 at rest.
    pub axes: [i16; MAX_AXES],
    /// 0 = up, then clockwise to 7 = up-left; None at rest.
    pub hat: Option<u8>,
}

fn bits(report: &[u8], offset: u32, size: u32) -> Option<u32> {
    if size == 0 || size > 32 || (offset + size).div_ceil(8) as usize > report.len() {
        return None;
    }
    let mut v = 0u64;
    for k in 0..(offset % 8 + size).div_ceil(8) {
        v |= (report[(offset / 8 + k) as usize] as u64) << (8 * k);
    }
    Some(((v >> (offset % 8)) & ((1u64 << size) - 1)) as u32)
}

/// Decodes one input report (without the Bluetooth HID header byte).
/// Returns false if the report does not belong to the layout.
pub fn decode(layout: &Layout, report: &[u8], pad: &mut Pad) -> bool {
    let (id, body) = if layout.uses_report_ids {
        match report.split_first() {
            Some((&id, rest)) => (id, rest),
            None => return false,
        }
    } else {
        (0, report)
    };
    let mut any = false;
    let mut array_buttons = 0u32;
    let mut has_array = false;
    for f in layout.fields.iter().filter(|f| f.report_id == id) {
        let Some(raw) = bits(body, f.offset, f.size) else { continue };
        any = true;
        let value = if f.min < 0 && f.size < 32 && raw & (1 << (f.size - 1)) != 0 {
            raw as i32 - (1i32 << f.size)
        } else {
            raw as i32
        };
        match f.use_ {
            Use::Axis(a) => {
                let span = (f.max - f.min).max(1) as i64;
                let v = ((value.clamp(f.min, f.max) - f.min) as i64 * 254 / span) as i16 - 127;
                pad.axes[a] = v;
            }
            Use::Hat => {
                pad.hat = (value >= f.min && value <= f.max && f.max - f.min == 7).then(|| (value - f.min) as u8);
            }
            Use::ButtonArray { first } => {
                has_array = true;
                let usage = first as i32 + value - f.min;
                if value >= f.min && value <= f.max && (1..=32).contains(&usage) {
                    array_buttons |= 1 << (usage - 1);
                }
            }
            Use::Button(n) => {
                if value != 0 {
                    pad.buttons |= 1 << n;
                } else {
                    pad.buttons &= !(1 << n);
                }
            }
        }
    }
    if has_array {
        pad.buttons = array_buttons;
    }
    any
}

pub fn hat_name(h: Option<u8>) -> &'static str {
    match h {
        Some(0) => "up",
        Some(1) => "up-right",
        Some(2) => "right",
        Some(3) => "down-right",
        Some(4) => "down",
        Some(5) => "down-left",
        Some(6) => "left",
        Some(7) => "up-left",
        _ => "centre",
    }
}
