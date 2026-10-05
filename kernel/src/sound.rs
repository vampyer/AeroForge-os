//! Sound: finds the High Definition Audio controllers (the motherboard's
//! audio chip, and the audio function of graphics cards for HDMI and
//! DisplayPort), lists their outputs through the C++ HDA driver, and plays
//! 48 kHz 16-bit stereo on the chosen one. For now the shell is the only
//! user (`sound test`); an audio service for programs comes later.

use alloc::string::String;
use alloc::vec::Vec;

use crate::dhi::{self, HdaInfo, HdaOutput};
use crate::sync::IrqMutex;
use crate::{console, pci, sched};

pub const RATE: u32 = 48_000;

pub struct Card {
    pub name: String,
    pub id: i32,
    pub location: String,
    pub pci_id: (u16, u16),
    pub info: HdaInfo,
}

impl Card {
    pub fn outputs(&self) -> &[HdaOutput] {
        &self.info.outputs[..self.info.output_count as usize]
    }
}

pub static CARDS: IrqMutex<Vec<Card>> = IrqMutex::new(Vec::new());
/// The output sound goes to: (card, output index).
static SELECTED: IrqMutex<Option<(usize, usize)>> = IrqMutex::new(None);
/// Held while something plays, so two players do not share the stream.
static PLAYING: IrqMutex<bool> = IrqMutex::new(false);

/// Brings up every HDA controller. Returns how many came up.
pub fn probe() -> usize {
    let mut cards = CARDS.lock();
    for dev in pci::devices().iter().filter(|d| d.class == 0x04 && d.subclass == 0x03) {
        let location = alloc::format!("{:02x}:{:02x}.{}", dev.bus, dev.dev, dev.func);
        let Some(bar0) = dev.bar(0) else { continue };
        dev.enable_mmio_and_dma();
        let mut info = HdaInfo::default();
        let id = unsafe { dhi::aero_hda_init(&dhi::OPS, bar0, &mut info) };
        if id < 0 {
            console::print_colored(console::YELLOW, format_args!(
                "[WARN] audio controller {:04x}:{:04x} at {}: init failed ({})\n", dev.vendor, dev.device, location, id));
            continue;
        }
        let name = alloc::format!("snd{}", cards.len());
        cards.push(Card { name, id, location, pci_id: (dev.vendor, dev.device), info });
    }
    // Default output: something plugged into an analog jack, else any
    // analog output, else HDMI.
    let rank = |o: &HdaOutput| match (o.kind, o.plugged) {
        (dhi::HDA_HDMI, 1) => 5,
        (dhi::HDA_HDMI, _) => 6,
        (dhi::HDA_SPDIF, _) => 4,
        (dhi::HDA_HEADPHONES, 1) => 0,
        (_, 1) => 1,
        (_, 2) => 2,
        _ => 3,
    };
    let best = cards.iter().enumerate()
        .flat_map(|(c, card)| card.outputs().iter().enumerate().map(move |(i, o)| (rank(o), c, i)))
        .min();
    *SELECTED.lock() = best.map(|(_, c, i)| (c, i));
    cards.len()
}

pub fn selected() -> Option<(usize, usize)> {
    *SELECTED.lock()
}

pub fn select(card: usize, output: usize) -> Result<(), &'static str> {
    let cards = CARDS.lock();
    if cards.get(card).is_none_or(|c| output >= c.outputs().len()) {
        return Err("no such output (see 'sound')");
    }
    *SELECTED.lock() = Some((card, output));
    Ok(())
}

pub fn card_name(vendor: u16) -> &'static str {
    match vendor {
        0x8086 => "Intel",
        0x1022 => "AMD",
        0x1002 => "AMD Radeon",
        0x10DE => "NVIDIA",
        0x1B36 | 0x1AF4 => "QEMU",
        _ => "",
    }
}

pub fn codec_name(id: u32) -> String {
    let vendor = match id >> 16 {
        0x10EC => "Realtek",
        0x1002 => "AMD HDMI",
        0x8086 => "Intel HDMI",
        0x10DE => "NVIDIA HDMI",
        0x14F1 => "Conexant",
        0x111D | 0x92F1 => "IDT",
        0x1106 => "VIA",
        0x1AF4 => "QEMU",
        _ => return alloc::format!("codec {:08x}", id),
    };
    alloc::format!("{} {:04x}", vendor, id & 0xFFFF)
}

/// "Line out (rear, green)", "Headphones (front)", "HDMI/DisplayPort".
pub fn output_name(o: &HdaOutput) -> String {
    let kind = match o.kind {
        dhi::HDA_SPEAKER => "Speaker",
        dhi::HDA_HEADPHONES => "Headphones",
        dhi::HDA_SPDIF => "S/PDIF",
        dhi::HDA_HDMI => return String::from("HDMI/DisplayPort"),
        _ => "Line out",
    };
    let place = match (o.location >> 4, o.location & 0xF) {
        (1, _) => "internal",
        (_, 1) => "rear",
        (_, 2) => "front",
        (_, 3) => "left",
        (_, 4) => "right",
        (_, 5) => "top",
        (_, 6) => "bottom",
        _ => "",
    };
    let color = match o.color {
        1 => "black",
        2 => "grey",
        3 => "blue",
        4 => "green",
        5 => "red",
        6 => "orange",
        7 => "yellow",
        8 => "purple",
        9 => "pink",
        0xE => "white",
        _ => "",
    };
    let detail: Vec<&str> = [place, color].into_iter().filter(|s| !s.is_empty()).collect();
    if detail.is_empty() {
        String::from(kind)
    } else {
        alloc::format!("{} ({})", kind, detail.join(", "))
    }
}

/// sin(x) without libm: range reduction and a Taylor polynomial, plenty for
/// test tones.
fn sin(x: f64) -> f64 {
    const PI: f64 = core::f64::consts::PI;
    let mut x = x % (2.0 * PI);
    if x > PI {
        x -= 2.0 * PI;
    } else if x < -PI {
        x += 2.0 * PI;
    }
    // Fold into [-pi/2, pi/2].
    if x > PI / 2.0 {
        x = PI - x;
    } else if x < -PI / 2.0 {
        x = -PI - x;
    }
    let x2 = x * x;
    x * (1.0 - x2 / 6.0 * (1.0 - x2 / 20.0 * (1.0 - x2 / 42.0 * (1.0 - x2 / 72.0))))
}

/// Plays a sine tone of `hz` for `ms` on the selected output, at about a
/// quarter of full scale, with short fades so it does not click. Waits until
/// it has been played.
pub fn tone(hz: u32, ms: u32) -> Result<String, String> {
    let frames = (RATE as u64 * ms as u64 / 1000) as usize;
    let fade = (RATE / 200) as usize; // 5 ms
    let step = 2.0 * core::f64::consts::PI * hz as f64 / RATE as f64;
    let mut phase = 0.0f64;
    let mut done = 0usize;
    play(|buf| {
        let mut n = 0;
        for f in buf.chunks_exact_mut(2) {
            if done == frames {
                break;
            }
            let ramp = (done.min(frames - 1 - done) as f64 / fade as f64).min(1.0);
            let v = (8000.0 * ramp * sin(phase)) as i16;
            phase += step;
            f[0] = v;
            f[1] = v;
            done += 1;
            n += 1;
        }
        n
    })
}

/// Runs a player: `fill` writes interleaved stereo frames into the slice it
/// gets and returns how many it wrote (0 = finished).
pub fn play(mut fill: impl FnMut(&mut [i16]) -> usize) -> Result<String, String> {
    let Some((c, o)) = selected() else { return Err(String::from("no audio output")) };
    let (id, name) = {
        let cards = CARDS.lock();
        let card = &cards[c];
        (card.id, alloc::format!("{} {}", card.name, output_name(&card.outputs()[o])))
    };
    {
        let mut playing = PLAYING.lock();
        if *playing {
            return Err(String::from("something is already playing"));
        }
        *playing = true;
    }
    let result = unsafe { dhi::aero_hda_start(id, o as i32) };
    if result != 0 {
        *PLAYING.lock() = false;
        return Err(alloc::format!("could not start the output ({})", result));
    }
    let mut buf = [0i16; 2 * 1024];
    let mut len = 0usize; // frames in buf not yet accepted
    let mut total = 0u64;
    loop {
        if len == 0 {
            len = fill(&mut buf);
            if len == 0 {
                break;
            }
        }
        let n = unsafe { dhi::aero_hda_write(id, buf.as_ptr(), len as u32) };
        if n < 0 {
            break;
        }
        let n = n as usize;
        total += n as u64;
        buf.copy_within(2 * n..2 * len, 0);
        len -= n;
        if len > 0 {
            sched::sleep_ticks(1);
        }
    }
    // Let the rest play out, then a little silence before stopping.
    while unsafe { dhi::aero_hda_pending(id) } > 0 {
        sched::sleep_ticks(1);
    }
    sched::sleep_ticks(5);
    unsafe { dhi::aero_hda_stop(id) };
    *PLAYING.lock() = false;
    Ok(alloc::format!("{} frames on {}", total, name))
}
