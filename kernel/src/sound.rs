//! Sound: finds the High Definition Audio controllers (the motherboard's
//! audio chip, and the audio function of graphics cards for HDMI and
//! DisplayPort), lists their outputs through the C++ HDA driver, and plays
//! 48 kHz 16-bit stereo on the chosen one. Programs (through system calls)
//! and the shell's test tone each queue sound in their own stream; a mixer
//! thread adds the streams together and feeds the sound card.

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

use crate::dhi::{self, HdaInfo, HdaOutput};
use crate::sync::IrqMutex;
use crate::{apic, console, pci, sched};

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
    let name = output_label().ok_or_else(|| String::from("no audio output"))?;
    let frames = (RATE as u64 * ms as u64 / 1000) as usize;
    let fade = (RATE / 200) as usize; // 5 ms
    let step = 2.0 * core::f64::consts::PI * hz as f64 / RATE as f64;
    let mut buf = Vec::with_capacity(2 * 1024);
    let mut done = 0usize;
    while done < frames {
        buf.clear();
        for k in done..(done + 1024).min(frames) {
            let ramp = (k.min(frames - 1 - k) as f64 / fade as f64).min(1.0);
            let v = (8000.0 * ramp * sin(step * k as f64)) as i16;
            buf.extend_from_slice(&[v, v]);
        }
        let mut at = 0;
        while at < buf.len() {
            let n = write(KERNEL, &buf[at..])?;
            at += 2 * n;
            if at < buf.len() {
                sched::sleep_ticks(1);
            }
        }
        done += buf.len() / 2;
    }
    while queued(KERNEL) > 0 {
        sched::sleep_ticks(1);
    }
    Ok(alloc::format!("{} frames on {}", frames, name))
}

fn output_label() -> Option<String> {
    let (c, o) = selected()?;
    let cards = CARDS.lock();
    let card = cards.get(c)?;
    Some(alloc::format!("{} {}", card.name, output_name(card.outputs().get(o)?)))
}

// ------------------------------------------------------------- the mixer

/// The owner id of sound the kernel itself plays (the shell's test tone).
pub const KERNEL: u64 = 0;
/// Each player may queue a quarter of a second.
const STREAM_FRAMES: usize = (RATE / 4) as usize;
/// The mixer keeps about 100 ms in the sound card's buffer.
const CARD_TARGET: i32 = (RATE / 10) as i32;
/// The output stops after half a second of silence.
const IDLE_TICKS: u32 = 50;

/// One player's queued frames (left, right).
struct Stream {
    owner: u64,
    frames: VecDeque<(i16, i16)>,
    last_write: u64,
}

static STREAMS: IrqMutex<Vec<Stream>> = IrqMutex::new(Vec::new());
static CARD_PENDING: AtomicU32 = AtomicU32::new(0);

/// Queues interleaved 48 kHz stereo frames for `owner` (a process id, or
/// KERNEL); returns how many frames fit. Never blocks.
pub fn write(owner: u64, interleaved: &[i16]) -> Result<usize, String> {
    if selected().is_none() {
        return Err(String::from("no audio output"));
    }
    let mut streams = STREAMS.lock();
    let i = match streams.iter().position(|s| s.owner == owner) {
        Some(i) => i,
        None => {
            streams.push(Stream { owner, frames: VecDeque::new(), last_write: 0 });
            streams.len() - 1
        }
    };
    let s = &mut streams[i];
    let n = (STREAM_FRAMES - s.frames.len()).min(interleaved.len() / 2);
    s.frames.extend(interleaved[..2 * n].chunks_exact(2).map(|f| (f[0], f[1])));
    s.last_write = sched::ticks();
    Ok(n)
}

/// Frames `owner` has queued that have not been played yet (including
/// what the mixer already handed to the sound card).
pub fn queued(owner: u64) -> usize {
    let mine = STREAMS.lock().iter().find(|s| s.owner == owner).map_or(0, |s| s.frames.len());
    mine + CARD_PENDING.load(Ordering::Relaxed) as usize
}

/// Kernel thread: mixes every player's queued sound into the selected
/// output, starting the output when there is something to play and
/// stopping it after a little silence.
pub fn mixer_thread(_: u64) {
    let mut running: Option<(i32, usize, usize)> = None; // (driver id, card, output)
    let mut idle = 0u32;
    let mut mix = Vec::with_capacity(2 * CARD_TARGET as usize);
    loop {
        let have = STREAMS.lock().iter().any(|s| !s.frames.is_empty());
        let want = selected();
        if let Some((id, c, o)) = running {
            if want != Some((c, o)) {
                unsafe { dhi::aero_hda_stop(id) };
                running = None;
            }
        }
        if running.is_none() && have {
            if let Some((c, o)) = want {
                let id = CARDS.lock().get(c).map(|card| card.id);
                if let Some(id) = id {
                    let rc = unsafe { dhi::aero_hda_start(id, o as i32) };
                    if rc == 0 {
                        running = Some((id, c, o));
                        idle = 0;
                    } else {
                        console::print_colored(console::YELLOW, format_args!("[WARN] sound: could not start the output ({})\n", rc));
                        STREAMS.lock().clear();
                    }
                }
            }
        }
        if let Some((id, _, _)) = running {
            let pending = unsafe { dhi::aero_hda_pending(id) }.max(0);
            let room = (CARD_TARGET - pending).max(0) as usize;
            mix.clear();
            {
                let mut streams = STREAMS.lock();
                let n = streams.iter().map(|s| s.frames.len()).max().unwrap_or(0).min(room);
                mix.resize(2 * n, 0i32);
                for s in streams.iter_mut() {
                    for k in 0..n.min(s.frames.len()) {
                        let (l, r) = s.frames.pop_front().unwrap_or((0, 0));
                        mix[2 * k] += l as i32;
                        mix[2 * k + 1] += r as i32;
                    }
                }
            }
            if !mix.is_empty() {
                let out: Vec<i16> = mix.iter().map(|&v| v.clamp(i16::MIN as i32, i16::MAX as i32) as i16).collect();
                unsafe { dhi::aero_hda_write(id, out.as_ptr(), (out.len() / 2) as u32) };
                idle = 0;
            } else if pending == 0 {
                idle += 1;
                if idle > IDLE_TICKS {
                    unsafe { dhi::aero_hda_stop(id) };
                    running = None;
                }
            }
            CARD_PENDING.store(unsafe { dhi::aero_hda_pending(id) }.max(0) as u32, Ordering::Relaxed);
        } else {
            CARD_PENDING.store(0, Ordering::Relaxed);
        }
        // Forget players that have gone quiet.
        let now = sched::ticks();
        STREAMS.lock().retain(|s| !s.frames.is_empty() || now - s.last_write < 2 * apic::TIMER_HZ);
        sched::sleep_ticks(1);
    }
}
