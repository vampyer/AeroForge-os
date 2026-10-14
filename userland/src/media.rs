//! Pictures and sound files for the desktop: JPEG, PNG and BMP decoded to
//! pixels, and MP3, WAV, FLAC and OGG (Vorbis) decoded to the 48 kHz stereo
//! samples the audio system calls take. JPEG and PNG use the zune-jpeg and
//! zune-png crates, MP3 the nanomp3 crate (a Rust port of minimp3), Vorbis
//! the lewton crate (made no_std, in userland/vendor/lewton); BMP, WAV,
//! FLAC (see flac.rs) and the Ogg pages around Vorbis are read here.

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use crate::flac;
use lewton::audio::{read_audio_packet_generic, PreviousWindowRight};
use lewton::header::{read_header_comment, read_header_ident, read_header_setup, IdentHeader, SetupHeader};
use zune_core::bytestream::ZCursor;
use zune_core::colorspace::ColorSpace;
use zune_core::options::DecoderOptions;

/// A picture as 0xRRGGBB pixels, row by row.
pub struct Image {
    pub w: usize,
    pub h: usize,
    pub px: Vec<u32>,
    /// "JPEG", "PNG" or "BMP".
    pub format: &'static str,
}

/// Decodes a JPEG, PNG or BMP file, telling them apart by their first bytes.
/// See-through parts of a PNG are shown over white.
pub fn decode_image(data: &[u8]) -> Result<Image, &'static str> {
    if data.starts_with(&[0xFF, 0xD8]) {
        jpeg(data)
    } else if data.starts_with(b"\x89PNG") {
        png(data)
    } else if data.starts_with(b"BM") {
        bmp(data)
    } else {
        Err("not a JPEG, PNG or BMP picture")
    }
}

fn rgb(r: u8, g: u8, b: u8) -> u32 {
    (r as u32) << 16 | (g as u32) << 8 | b as u32
}

/// Mixes a pixel with `alpha` (0 to 255) over white.
fn over_white(r: u8, g: u8, b: u8, alpha: u8) -> u32 {
    let mix = |c: u8| ((c as u32 * alpha as u32 + 255 * (255 - alpha as u32)) / 255) as u8;
    rgb(mix(r), mix(g), mix(b))
}

fn jpeg(data: &[u8]) -> Result<Image, &'static str> {
    let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGB);
    let mut d = zune_jpeg::JpegDecoder::new_with_options(ZCursor::new(data), options);
    let raw = d.decode().map_err(|_| "the JPEG picture is damaged or of a kind not supported")?;
    let info = d.info().ok_or("bad JPEG picture")?;
    let (w, h) = (info.width as usize, info.height as usize);
    if raw.len() < w * h * 3 {
        return Err("bad JPEG picture");
    }
    let px = raw.chunks_exact(3).take(w * h).map(|p| rgb(p[0], p[1], p[2])).collect();
    Ok(Image { w, h, px, format: "JPEG" })
}

fn png(data: &[u8]) -> Result<Image, &'static str> {
    let options = DecoderOptions::default().png_set_strip_to_8bit(true);
    let mut d = zune_png::PngDecoder::new_with_options(ZCursor::new(data), options);
    let raw = d.decode().map_err(|_| "the PNG picture is damaged or of a kind not supported")?;
    let raw = raw.u8().ok_or("bad PNG picture")?;
    let (w, h) = d.dimensions().ok_or("bad PNG picture")?;
    let n = d.colorspace().ok_or("bad PNG picture")?.num_components();
    if raw.len() < w * h * n {
        return Err("bad PNG picture");
    }
    let px = raw
        .chunks_exact(n)
        .take(w * h)
        .map(|p| match n {
            1 => rgb(p[0], p[0], p[0]),
            2 => over_white(p[0], p[0], p[0], p[1]),
            3 => rgb(p[0], p[1], p[2]),
            _ => over_white(p[0], p[1], p[2], p[3]),
        })
        .collect();
    Ok(Image { w, h, px, format: "PNG" })
}

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}
fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

/// Uncompressed Windows bitmaps: 24 and 32 bits, and 8, 4 and 1 bits with
/// a palette; rows bottom up, or top down when the height is negative.
fn bmp(data: &[u8]) -> Result<Image, &'static str> {
    let bad = "bad BMP picture";
    let pixels = u32_at(data, 10).ok_or(bad)? as usize;
    let header = u32_at(data, 14).ok_or(bad)? as usize;
    let w = u32_at(data, 18).ok_or(bad)? as i32;
    let h = u32_at(data, 22).ok_or(bad)? as i32;
    let bits = u16_at(data, 28).ok_or(bad)? as usize;
    let compression = u32_at(data, 30).ok_or(bad)?;
    // BI_RGB, or BI_BITFIELDS with the usual 32-bit masks.
    if compression != 0 && !(compression == 3 && bits == 32) {
        return Err("compressed BMP pictures are not supported");
    }
    if w <= 0 || h == 0 || w > 16384 || h.unsigned_abs() > 16384 {
        return Err(bad);
    }
    let (w, top_down, h) = (w as usize, h < 0, h.unsigned_abs() as usize);
    let palette: Vec<u32> = if bits <= 8 {
        let colors = match u32_at(data, 46).ok_or(bad)? {
            0 => 1 << bits,
            n => n as usize,
        };
        (0..colors.min(256))
            .map(|i| {
                let at = 14 + header + 4 * i;
                data.get(at..at + 3).map_or(0, |p| rgb(p[2], p[1], p[0]))
            })
            .collect()
    } else {
        Vec::new()
    };
    if !matches!(bits, 1 | 4 | 8 | 24 | 32) {
        return Err("BMP pictures of that many colors are not supported");
    }
    let stride = (w * bits).div_ceil(32) * 4;
    let mut px = alloc::vec![0u32; w * h];
    for y in 0..h {
        let row_at = pixels + stride * if top_down { y } else { h - 1 - y };
        let row = data.get(row_at..row_at + stride).ok_or(bad)?;
        for x in 0..w {
            px[y * w + x] = match bits {
                24 => rgb(row[3 * x + 2], row[3 * x + 1], row[3 * x]),
                32 => rgb(row[4 * x + 2], row[4 * x + 1], row[4 * x]),
                8 => palette.get(row[x] as usize).copied().unwrap_or(0),
                4 => palette.get((row[x / 2] >> if x % 2 == 0 { 4 } else { 0 } & 0xF) as usize).copied().unwrap_or(0),
                _ => palette.get((row[x / 8] >> (7 - x % 8) & 1) as usize).copied().unwrap_or(0),
            };
        }
    }
    Ok(Image { w, h, px, format: "BMP" })
}

/// `img` made smaller to fit in `max_w` x `max_h`, each new pixel the
/// average of the ones it covers; as it is when it fits already.
pub fn shrink_to_fit(img: Image, max_w: usize, max_h: usize) -> Image {
    if img.w <= max_w && img.h <= max_h {
        return img;
    }
    // The same scale both ways, from the side that has further to shrink.
    let (w, h) = if img.w * max_h > img.h * max_w {
        (max_w, (img.h * max_w / img.w).max(1))
    } else {
        ((img.w * max_h / img.h).max(1), max_h)
    };
    let mut px = alloc::vec![0u32; w * h];
    for y in 0..h {
        let (y0, y1) = (y * img.h / h, ((y + 1) * img.h / h).max(y * img.h / h + 1));
        for x in 0..w {
            let (x0, x1) = (x * img.w / w, ((x + 1) * img.w / w).max(x * img.w / w + 1));
            let (mut r, mut g, mut b) = (0u32, 0u32, 0u32);
            for sy in y0..y1 {
                for &p in &img.px[sy * img.w + x0..sy * img.w + x1] {
                    r += p >> 16;
                    g += p >> 8 & 0xFF;
                    b += p & 0xFF;
                }
            }
            let n = ((y1 - y0) * (x1 - x0)) as u32;
            px[y * w + x] = (r / n) << 16 | (g / n) << 8 | b / n;
        }
    }
    Image { w, h, px, format: img.format }
}

// ------------------------------------------------------------------ sound

/// The rate the audio system calls play at.
const OUT_RATE: u64 = 48_000;

enum Source {
    Mp3 { dec: nanomp3::Decoder, pos: usize },
    /// `float`: 32-bit IEEE samples rather than whole numbers.
    Wav { start: usize, end: usize, pos: usize, bits: u16, block: usize, float: bool },
    Flac { start: usize, pos: usize, info: flac::Info, chans: Vec<Vec<i32>> },
    Vorbis { ogg: Ogg, ident: alloc::boxed::Box<IdentHeader>, setup: alloc::boxed::Box<SetupHeader>, pwr: PreviousWindowRight },
}

/// Reads the packets of the first logical stream in an Ogg file, page by page.
struct Ogg {
    /// Where the next page starts.
    pos: usize,
    serial: Option<u32>,
    /// A packet that goes on into the next page.
    part: Vec<u8>,
    ready: VecDeque<Vec<u8>>,
    /// After a seek: drop the end of a packet begun on an earlier page.
    skip_cont: bool,
}

impl Ogg {
    fn new(pos: usize) -> Ogg {
        Ogg { pos, serial: None, part: Vec::new(), ready: VecDeque::new(), skip_cont: false }
    }

    fn next(&mut self, d: &[u8]) -> Option<Vec<u8>> {
        loop {
            if let Some(p) = self.ready.pop_front() {
                return Some(p);
            }
            // The next page; after a seek or damage, the next "OggS".
            if d.get(self.pos..self.pos + 4) != Some(b"OggS") {
                self.pos = find(d, b"OggS", self.pos)?;
            }
            let h = d.get(self.pos..self.pos + 27)?;
            let flags = h[5];
            let serial = u32::from_le_bytes([h[14], h[15], h[16], h[17]]);
            let segs = h[26] as usize;
            let lacing = d.get(self.pos + 27..self.pos + 27 + segs)?;
            let body_len: usize = lacing.iter().map(|&l| l as usize).sum();
            let mut at = self.pos + 27 + segs;
            if at + body_len > d.len() || h[4] != 0 {
                self.pos += 4;
                continue;
            }
            self.pos = at + body_len;
            if *self.serial.get_or_insert(serial) != serial {
                continue;
            }
            let mut skipping = self.skip_cont && flags & 1 != 0;
            self.skip_cont = false;
            if flags & 1 == 0 {
                self.part.clear();
            }
            for &l in lacing {
                let seg = &d[at..at + l as usize];
                at += l as usize;
                if !skipping {
                    self.part.extend_from_slice(seg);
                }
                if l < 255 {
                    if !skipping {
                        self.ready.push_back(core::mem::take(&mut self.part));
                    }
                    skipping = false;
                }
            }
        }
    }
}

fn find(d: &[u8], what: &[u8], from: usize) -> Option<usize> {
    d.get(from..)?.windows(what.len()).position(|w| w == what).map(|i| from + i)
}

/// Scales a sample of `bits` bits to 16.
fn to16(v: i32, bits: u8) -> i16 {
    if bits > 16 { (v >> (bits - 16)) as i16 } else { (v << (16 - bits)) as i16 }
}

/// A song being played: decoded a piece at a time and turned into 48 kHz
/// stereo.
pub struct Song {
    data: Vec<u8>,
    source: Source,
    /// "MP3", "WAV", "FLAC" or "OGG", its own rate and channels.
    pub format: &'static str,
    pub rate: u32,
    pub channels: u8,
    /// How long it is, in 48 kHz frames (estimated for MP3s without a
    /// length in their header).
    pub frames: u64,
    /// How many 48 kHz frames have been made so far.
    pub made: u64,
    /// Decoded stereo frames at the song's own rate, waiting to be resampled,
    /// and how far into them (in their frames, as a fraction) the next output
    /// frame falls.
    pending: Vec<(i16, i16)>,
    at: u64,
    ended: bool,
}

/// Fractional positions are kept in 1/65536ths of a frame.
const FRAC: u64 = 1 << 16;

impl Song {
    /// Reads a song's header, ready to play from the start.
    pub fn open(data: Vec<u8>) -> Result<Song, &'static str> {
        let wav = data.starts_with(b"RIFF") && data.get(8..12) == Some(b"WAVE");
        let ogg = data.starts_with(b"OggS");
        let is_flac = flac::open(&data).is_ok() || data.starts_with(b"fLaC");
        let mut song = Song {
            data,
            source: Source::Mp3 { dec: nanomp3::Decoder::new(), pos: 0 },
            format: "MP3",
            rate: 0,
            channels: 0,
            frames: 0,
            made: 0,
            pending: Vec::new(),
            at: 0,
            ended: false,
        };
        if wav {
            song.open_wav()?;
        } else if ogg {
            song.open_ogg()?;
        } else if is_flac {
            song.open_flac()?;
        } else {
            song.open_mp3()?;
        }
        Ok(song)
    }

    fn open_wav(&mut self) -> Result<(), &'static str> {
        let bad = "bad WAV file";
        let d = &self.data;
        let mut at = 12;
        let mut fmt = None;
        while at + 8 <= d.len() {
            let id = &d[at..at + 4];
            let len = u32_at(d, at + 4).ok_or(bad)? as usize;
            let body = at + 8;
            if id == b"fmt " {
                let tag = u16_at(d, body).ok_or(bad)?;
                let channels = u16_at(d, body + 2).ok_or(bad)?;
                let rate = u32_at(d, body + 4).ok_or(bad)?;
                let block = u16_at(d, body + 12).ok_or(bad)? as usize;
                let bits = u16_at(d, body + 14).ok_or(bad)?;
                // 1 is PCM and 3 is 32-bit floating point; 0xFFFE
                // (extensible) is either, as its subformat says.
                let kind = if tag == 0xFFFE { u16_at(d, body + 24).unwrap_or(0) } else { tag };
                let float = kind == 3;
                let ok = (kind == 1 && matches!(bits, 8 | 16 | 24 | 32)) || (float && bits == 32);
                if !ok || !(1..=8).contains(&channels) || !(8000..=192_000).contains(&rate) {
                    return Err("only uncompressed (PCM or floating point) WAV files can be played");
                }
                if block < channels as usize * bits as usize / 8 {
                    return Err(bad);
                }
                fmt = Some((channels, rate, block, bits, float));
            } else if id == b"data" {
                let (channels, rate, block, bits, float) = fmt.ok_or(bad)?;
                let end = (body + len).min(d.len());
                self.source = Source::Wav { start: body, end, pos: body, bits, block, float };
                self.format = "WAV";
                self.rate = rate;
                self.channels = channels as u8;
                self.frames = ((end - body) / block) as u64 * OUT_RATE / rate as u64;
                return Ok(());
            }
            at = body + len + (len & 1);
        }
        Err(bad)
    }

    fn open_mp3(&mut self) -> Result<(), &'static str> {
        let start = nanomp3::id3v2_len(&self.data).min(self.data.len());
        // The first frames give the rate and channels; the sizes of all of
        // them, the length.
        let mut samples = 0u64;
        let mut first = None;
        for f in nanomp3::frames(&self.data[start..]) {
            first.get_or_insert((f.sample_rate, f.channels));
            samples += f.samples as u64;
        }
        let (rate, channels) = first.ok_or("not an MP3, WAV, FLAC or OGG file")?;
        self.rate = rate;
        self.channels = channels.num();
        self.frames = samples * OUT_RATE / rate.max(1) as u64;
        self.source = Source::Mp3 { dec: nanomp3::Decoder::new(), pos: start };
        Ok(())
    }

    fn open_flac(&mut self) -> Result<(), &'static str> {
        let (info, start) = flac::open(&self.data)?;
        let first = flac::find_frame(&self.data, start, &info).ok_or("bad FLAC file")?;
        self.format = "FLAC";
        self.rate = info.rate;
        self.channels = info.channels;
        self.frames = info.samples * OUT_RATE / info.rate as u64;
        self.source = Source::Flac { start: first, pos: first, info, chans: Vec::new() };
        Ok(())
    }

    fn open_ogg(&mut self) -> Result<(), &'static str> {
        let bad = "bad OGG file";
        let mut ogg = Ogg::new(0);
        let first = ogg.next(&self.data).ok_or(bad)?;
        if first.starts_with(b"OpusHead") {
            return Err("OGG files with Opus sound can't be played yet");
        }
        let ident = read_header_ident(&first).map_err(|_| "only OGG files with Vorbis sound can be played")?;
        read_header_comment(&ogg.next(&self.data).ok_or(bad)?).map_err(|_| bad)?;
        let setup = read_header_setup(&ogg.next(&self.data).ok_or(bad)?, ident.audio_channels, (ident.blocksize_0, ident.blocksize_1))
            .map_err(|_| bad)?;
        // The length: the last page's granule position counts the samples.
        let mut samples = 0u64;
        let mut at = self.data.len();
        while at > 0 {
            let from = at.saturating_sub(65_536);
            let window = &self.data[from..at];
            if let Some(i) = window.windows(4).rposition(|w| w == b"OggS") {
                let p = from + i;
                if let Some(h) = self.data.get(p..p + 27) {
                    let g = i64::from_le_bytes([h[6], h[7], h[8], h[9], h[10], h[11], h[12], h[13]]);
                    let serial = u32::from_le_bytes([h[14], h[15], h[16], h[17]]);
                    if g > 0 && Some(serial) == ogg.serial {
                        samples = g as u64;
                        break;
                    }
                }
                at = p;
            } else {
                at = from + 3.min(window.len());
                if from == 0 {
                    break;
                }
            }
        }
        self.format = "OGG";
        self.rate = ident.audio_sample_rate;
        self.channels = ident.audio_channels;
        if self.rate == 0 {
            return Err(bad);
        }
        self.frames = samples * OUT_RATE / self.rate as u64;
        self.source = Source::Vorbis { ogg, ident: alloc::boxed::Box::new(ident), setup: alloc::boxed::Box::new(setup), pwr: PreviousWindowRight::new() };
        Ok(())
    }

    /// Whether everything has been made.
    pub fn ended(&self) -> bool {
        self.ended && self.pending.len() as u64 <= self.at / FRAC + 1
    }

    /// Starts again from `frame` (in 48 kHz frames). MP3s land on the frame
    /// boundary nearest the same share of the file.
    pub fn seek(&mut self, frame: u64) {
        let frame = frame.min(self.frames);
        self.pending.clear();
        self.at = 0;
        self.ended = false;
        self.made = frame;
        let share = |len: usize| if self.frames == 0 { 0 } else { (len as u64 * frame / self.frames) as usize };
        match &mut self.source {
            Source::Wav { start, end, pos, block, .. } => {
                let n = (frame * self.rate as u64 / OUT_RATE) as usize;
                *pos = (*start + n * *block).min(*end);
            }
            Source::Flac { start, pos, info, .. } => {
                let at = *start + share(self.data.len() - *start);
                *pos = flac::find_frame(&self.data, at, info).unwrap_or(self.data.len());
            }
            Source::Vorbis { ogg, pwr, .. } => {
                let serial = ogg.serial;
                *ogg = Ogg::new(share(self.data.len()));
                ogg.serial = serial;
                ogg.skip_cont = true;
                *pwr = PreviousWindowRight::new();
            }
            Source::Mp3 { dec, pos } => {
                let start = nanomp3::id3v2_len(&self.data).min(self.data.len());
                *pos = start + share(self.data.len() - start);
                *dec = nanomp3::Decoder::new();
            }
        }
    }

    /// Decodes some more of the song into `pending`; false at its end.
    fn decode_more(&mut self) -> bool {
        match &mut self.source {
            Source::Wav { end, pos, bits, block, float, .. } => {
                let take = (*end - *pos) / *block;
                let take = take.min(4096);
                if take == 0 {
                    return false;
                }
                let float = *float;
                let sample = |b: &[u8]| -> i16 {
                    if float {
                        let v = f32::from_le_bytes([b[0], b[1], b[2], b[3]]) * 32768.0;
                        return v.max(-32768.0).min(32767.0) as i16;
                    }
                    match *bits {
                        8 => ((b[0] as i16) - 128) << 8,
                        16 => i16::from_le_bytes([b[0], b[1]]),
                        24 => i16::from_le_bytes([b[1], b[2]]),
                        _ => i16::from_le_bytes([b[2], b[3]]),
                    }
                };
                let width = *bits as usize / 8;
                for i in 0..take {
                    let f = &self.data[*pos + i * *block..];
                    let l = sample(f);
                    let r = if self.channels > 1 { sample(&f[width..]) } else { l };
                    self.pending.push((l, r));
                }
                *pos += take * *block;
                true
            }
            Source::Flac { pos, info, chans, .. } => {
                // Frames that fail to decode are skipped to the next one.
                for _ in 0..16 {
                    if *pos >= self.data.len() {
                        return false;
                    }
                    match flac::frame(&self.data, *pos, info, chans) {
                        Some((len, rate)) => {
                            *pos += len;
                            let (l, r) = (&chans[0], &chans[if chans.len() > 1 { 1 } else { 0 }]);
                            self.pending.extend(l.iter().zip(r.iter()).map(|(&a, &b)| (to16(a, info.bits), to16(b, info.bits))));
                            if rate != 0 {
                                self.rate = rate;
                            }
                            return true;
                        }
                        None => match flac::find_frame(&self.data, *pos + 1, info) {
                            Some(next) => *pos = next,
                            None => return false,
                        },
                    }
                }
                false
            }
            Source::Vorbis { ogg, ident, setup, pwr } => {
                for _ in 0..64 {
                    let Some(packet) = ogg.next(&self.data) else { return false };
                    let Ok(pcm) = read_audio_packet_generic::<Vec<Vec<i16>>>(ident, setup, &packet, pwr) else { continue };
                    if pcm.is_empty() || pcm[0].is_empty() {
                        continue;
                    }
                    // Vorbis puts the centre second when there are three or more.
                    let right = match pcm.len() { 1 => 0, 2 => 1, _ => 2 };
                    self.pending.extend(pcm[0].iter().zip(pcm[right].iter()).map(|(&a, &b)| (a, b)));
                    return true;
                }
                false
            }
            Source::Mp3 { dec, pos } => {
                let mut pcm = [0i16; nanomp3::MAX_SAMPLES_PER_FRAME];
                // A few tries: junk and the first frames after a seek give no sound.
                for _ in 0..64 {
                    if *pos >= self.data.len() {
                        return false;
                    }
                    let window = &self.data[*pos..(*pos + 16 * 1024).min(self.data.len())];
                    let (used, result) = dec.decode(window, &mut pcm);
                    *pos += used;
                    match result {
                        Ok(info) => {
                            let n = info.samples_produced;
                            if info.channels.num() == 1 {
                                self.pending.extend(pcm[..n].iter().map(|&s| (s, s)));
                            } else {
                                self.pending.extend(pcm[..2 * n].chunks_exact(2).map(|p| (p[0], p[1])));
                            }
                            if info.sample_rate != 0 {
                                self.rate = info.sample_rate;
                            }
                            return true;
                        }
                        Err(nanomp3::DecodeError::NoFrame) if used == 0 => return false,
                        Err(_) => {}
                    }
                }
                false
            }
        }
    }

    /// Fills `out` with up to `out.len() / 2` interleaved 48 kHz stereo
    /// frames; returns how many it made (fewer only at the end).
    pub fn fill(&mut self, out: &mut [i16]) -> usize {
        let step = self.rate.max(1) as u64 * FRAC / OUT_RATE;
        let want = out.len() / 2;
        let mut n = 0;
        while n < want {
            let i = (self.at / FRAC) as usize;
            if i + 1 >= self.pending.len() {
                // Keep the frame we are between, drop the ones behind it.
                let keep = i.min(self.pending.len());
                self.pending.drain(..keep);
                self.at -= keep as u64 * FRAC;
                if self.ended || !self.decode_more() {
                    self.ended = true;
                    if self.pending.len() < 2 {
                        break;
                    }
                    if (self.at / FRAC) as usize + 1 >= self.pending.len() {
                        break;
                    }
                }
                continue;
            }
            let (a, b) = (self.pending[i], self.pending[i + 1]);
            let t = (self.at % FRAC) as i32;
            let mix = |x: i16, y: i16| (x as i32 + ((y as i32 - x as i32) * t >> 16)) as i16;
            out[2 * n] = mix(a.0, b.0);
            out[2 * n + 1] = mix(a.1, b.1);
            self.at += step;
            n += 1;
        }
        self.made += n as u64;
        if n < want {
            self.ended = true;
            self.pending.clear();
            self.at = 0;
        }
        n
    }
}
