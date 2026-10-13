//! FLAC: a decoder for Free Lossless Audio Codec files (stream info,
//! frames with constant, verbatim, fixed and LPC subframes, Rice coded
//! residuals, and the three stereo decorrelations), written from the format
//! description at xiph.org. Samples come out at their own bit depth.

use alloc::vec::Vec;

/// What the STREAMINFO block says.
#[derive(Clone, Copy)]
pub struct Info {
    pub rate: u32,
    pub channels: u8,
    pub bits: u8,
    /// Samples per channel in the whole stream, 0 when unknown.
    pub samples: u64,
    pub max_block: u16,
}

/// Reads the "fLaC" marker and the metadata blocks; returns the stream
/// info and where the first frame starts.
pub fn open(data: &[u8]) -> Result<(Info, usize), &'static str> {
    let bad = "bad FLAC file";
    // An ID3v2 tag may come first.
    let mut at = 0;
    if data.starts_with(b"ID3") && data.len() >= 10 {
        let size = data[6..10].iter().fold(0usize, |n, &b| (n << 7) | (b & 0x7F) as usize);
        at = 10 + size;
    }
    if data.get(at..at + 4) != Some(b"fLaC") {
        return Err("not a FLAC file");
    }
    at += 4;
    let mut info = None;
    loop {
        let head = data.get(at..at + 4).ok_or(bad)?;
        let last = head[0] & 0x80 != 0;
        let kind = head[0] & 0x7F;
        let len = (head[1] as usize) << 16 | (head[2] as usize) << 8 | head[3] as usize;
        let body = data.get(at + 4..at + 4 + len).ok_or(bad)?;
        if kind == 0 {
            if len < 34 {
                return Err(bad);
            }
            let max_block = u16::from_be_bytes([body[2], body[3]]);
            let x = u64::from_be_bytes([body[10], body[11], body[12], body[13], body[14], body[15], body[16], body[17]]);
            let rate = (x >> 44) as u32;
            let channels = ((x >> 41) & 7) as u8 + 1;
            let bits = ((x >> 36) & 31) as u8 + 1;
            let samples = x & 0xF_FFFF_FFFF;
            if rate == 0 || !(4..=32).contains(&bits) {
                return Err(bad);
            }
            info = Some(Info { rate, channels, bits, samples, max_block });
        }
        at += 4 + len;
        if last {
            break;
        }
    }
    Ok((info.ok_or(bad)?, at))
}

struct Bits<'a> {
    d: &'a [u8],
    /// Position in bits.
    pos: usize,
}

impl Bits<'_> {
    fn read(&mut self, n: u32) -> Option<u32> {
        if n == 0 {
            return Some(0);
        }
        let mut v = 0u64;
        let mut left = n;
        while left > 0 {
            let byte = *self.d.get(self.pos / 8)? as u64;
            let off = (self.pos % 8) as u32;
            let take = (8 - off).min(left);
            v = (v << take) | ((byte >> (8 - off - take)) & ((1 << take) - 1));
            self.pos += take as usize;
            left -= take;
        }
        Some(v as u32)
    }
    fn read_signed(&mut self, n: u32) -> Option<i32> {
        let v = self.read(n)?;
        Some(if n == 0 { 0 } else { ((v << (32 - n)) as i32) >> (32 - n) })
    }
    fn bit(&mut self) -> Option<bool> {
        self.read(1).map(|b| b == 1)
    }
    /// Zeros up to the next one.
    fn unary(&mut self) -> Option<u32> {
        let mut n = 0;
        loop {
            let byte = *self.d.get(self.pos / 8)?;
            let off = self.pos % 8;
            let rest = (byte << off) as u32;
            if rest == 0 {
                n += 8 - off as u32;
                self.pos += 8 - off;
                continue;
            }
            let z = rest.leading_zeros() - 24;
            n += z;
            self.pos += z as usize + 1;
            return Some(n);
        }
    }
    fn align(&mut self) {
        self.pos = self.pos.div_ceil(8) * 8;
    }
}

fn crc8(d: &[u8]) -> u8 {
    let mut c = 0u8;
    for &b in d {
        c ^= b;
        for _ in 0..8 {
            c = if c & 0x80 != 0 { (c << 1) ^ 0x07 } else { c << 1 };
        }
    }
    c
}

/// A frame header, read and checked (its CRC-8) at `at`.
struct Header {
    block: usize,
    rate: u32,
    /// 0-7 independent channels (count - 1), 8 left/side, 9 side/right, 10 mid/side.
    assign: u8,
    bits: u8,
    /// Bytes the header took.
    len: usize,
}

fn header(d: &[u8], at: usize, info: &Info) -> Option<Header> {
    let h = d.get(at..)?;
    if h.len() < 6 || h[0] != 0xFF || h[1] & 0xFE != 0xF8 {
        return None;
    }
    let bs = h[2] >> 4;
    let rc = h[2] & 15;
    let assign = h[3] >> 4;
    let sc = (h[3] >> 1) & 7;
    if assign > 10 || rc == 15 || sc == 3 || h[3] & 1 != 0 {
        return None;
    }
    // The frame or sample number, UTF-8 style.
    let mut p = 4;
    let lead = h[p];
    let extra = if lead < 0x80 { 0 } else if lead & 0xE0 == 0xC0 { 1 } else if lead & 0xF0 == 0xE0 { 2 }
        else if lead & 0xF8 == 0xF0 { 3 } else if lead & 0xFC == 0xF8 { 4 } else if lead & 0xFE == 0xFC { 5 }
        else if lead == 0xFE { 6 } else { return None };
    p += 1;
    for _ in 0..extra {
        if *h.get(p)? & 0xC0 != 0x80 {
            return None;
        }
        p += 1;
    }
    let block = match bs {
        0 => return None,
        1 => 192,
        2..=5 => 576 << (bs - 2),
        6 => { p += 1; *h.get(p - 1)? as usize + 1 }
        7 => { p += 2; u16::from_be_bytes([*h.get(p - 2)?, *h.get(p - 1)?]) as usize + 1 }
        _ => 256 << (bs - 8),
    };
    let rate = match rc {
        0 => info.rate,
        1 => 88_200, 2 => 176_400, 3 => 192_000, 4 => 8_000, 5 => 16_000, 6 => 22_050,
        7 => 24_000, 8 => 32_000, 9 => 44_100, 10 => 48_000, 11 => 96_000,
        12 => { p += 1; *h.get(p - 1)? as u32 * 1000 }
        13 => { p += 2; u16::from_be_bytes([*h.get(p - 2)?, *h.get(p - 1)?]) as u32 }
        _ => { p += 2; u16::from_be_bytes([*h.get(p - 2)?, *h.get(p - 1)?]) as u32 * 10 }
    };
    let bits = match sc {
        0 => info.bits,
        1 => 8, 2 => 12, 4 => 16, 5 => 20, 6 => 24, _ => 32,
    };
    if crc8(h.get(..p)?) != *h.get(p)? {
        return None;
    }
    let channels = if assign < 8 { assign + 1 } else { 2 };
    if channels != info.channels {
        return None;
    }
    Some(Header { block, rate, assign, bits, len: p + 1 })
}

/// Where the next frame starts at or after `from`.
pub fn find_frame(d: &[u8], from: usize, info: &Info) -> Option<usize> {
    let mut at = from;
    while at + 1 < d.len() {
        if d[at] == 0xFF && d[at + 1] & 0xFE == 0xF8 && header(d, at, info).is_some() {
            return Some(at);
        }
        at += 1;
    }
    None
}

fn residual(b: &mut Bits, order: usize, block: usize, out: &mut [i32]) -> Option<()> {
    let method = b.read(2)?;
    if method > 1 {
        return None;
    }
    let pbits = if method == 0 { 4 } else { 5 };
    let porder = b.read(4)?;
    let parts = 1usize << porder;
    if block % parts != 0 || block / parts < order {
        return None;
    }
    let mut i = order;
    for part in 0..parts {
        let count = if part == 0 { block / parts - order } else { block / parts };
        let k = b.read(pbits)?;
        if k == (1 << pbits) - 1 {
            // Escaped: plain signed numbers of a given size.
            let n = b.read(5)?;
            for _ in 0..count {
                out[i] = b.read_signed(n)?;
                i += 1;
            }
        } else {
            for _ in 0..count {
                let q = b.unary()?;
                let r = b.read(k)?;
                let u = (q << k) | r;
                out[i] = ((u >> 1) as i32) ^ -((u & 1) as i32);
                i += 1;
            }
        }
    }
    Some(())
}

fn subframe(b: &mut Bits, block: usize, bits: u32, out: &mut [i32]) -> Option<()> {
    if b.bit()? {
        return None;
    }
    let kind = b.read(6)?;
    let mut wasted = 0;
    if b.bit()? {
        wasted = b.unary()? + 1;
    }
    let bits = bits.checked_sub(wasted).filter(|&n| n > 0)?;
    match kind {
        0 => {
            let v = b.read_signed(bits)?;
            out[..block].fill(v);
        }
        1 => {
            for s in out[..block].iter_mut() {
                *s = b.read_signed(bits)?;
            }
        }
        8..=12 => {
            let order = (kind - 8) as usize;
            if order > block {
                return None;
            }
            for s in out[..order].iter_mut() {
                *s = b.read_signed(bits)?;
            }
            residual(b, order, block, out)?;
            for i in order..block {
                let r = out[i] as i64;
                let p = match order {
                    0 => 0,
                    1 => out[i - 1] as i64,
                    2 => 2 * out[i - 1] as i64 - out[i - 2] as i64,
                    3 => 3 * out[i - 1] as i64 - 3 * out[i - 2] as i64 + out[i - 3] as i64,
                    _ => 4 * out[i - 1] as i64 - 6 * out[i - 2] as i64 + 4 * out[i - 3] as i64 - out[i - 4] as i64,
                };
                out[i] = (p + r) as i32;
            }
        }
        32..=63 => {
            let order = (kind - 31) as usize;
            if order > block {
                return None;
            }
            for s in out[..order].iter_mut() {
                *s = b.read_signed(bits)?;
            }
            let precision = b.read(4)? + 1;
            if precision == 16 {
                return None;
            }
            let shift = b.read_signed(5)?;
            if shift < 0 {
                return None;
            }
            let mut coef = [0i64; 32];
            for c in coef[..order].iter_mut() {
                *c = b.read_signed(precision)? as i64;
            }
            residual(b, order, block, out)?;
            for i in order..block {
                let mut sum = 0i64;
                for j in 0..order {
                    sum += coef[j] * out[i - 1 - j] as i64;
                }
                out[i] = ((sum >> shift) + out[i] as i64) as i32;
            }
        }
        _ => return None,
    }
    if wasted > 0 {
        for s in out[..block].iter_mut() {
            *s <<= wasted;
        }
    }
    Some(())
}

/// Decodes the frame at `at` into `chans` (one list per channel, at the
/// stream's bit depth); returns the frame's length in bytes and its rate.
pub fn frame(d: &[u8], at: usize, info: &Info, chans: &mut Vec<Vec<i32>>) -> Option<(usize, u32)> {
    let h = header(d, at, info)?;
    let n = info.channels as usize;
    chans.resize(n, Vec::new());
    let mut b = Bits { d: &d[at..], pos: h.len * 8 };
    for (c, out) in chans.iter_mut().enumerate() {
        out.clear();
        out.resize(h.block, 0);
        // The side channel has one more bit.
        let side = matches!((h.assign, c), (8, 1) | (9, 0) | (10, 1));
        subframe(&mut b, h.block, h.bits as u32 + side as u32, out)?;
    }
    match h.assign {
        8 => {
            let (l, s) = chans.split_at_mut(1);
            for (l, s) in l[0].iter().zip(s[0].iter_mut()) {
                *s = l - *s;
            }
        }
        9 => {
            let (s, r) = chans.split_at_mut(1);
            for (s, r) in s[0].iter_mut().zip(r[0].iter()) {
                *s += r;
            }
        }
        10 => {
            let (m, s) = chans.split_at_mut(1);
            for (m, s) in m[0].iter_mut().zip(s[0].iter_mut()) {
                let side = *s;
                let mid = ((*m as i64) << 1) | (side as i64 & 1);
                *m = ((mid + side as i64) >> 1) as i32;
                *s = ((mid - side as i64) >> 1) as i32;
            }
        }
        _ => {}
    }
    b.align();
    // Then the CRC-16 of the frame.
    let len = b.pos / 8 + 2;
    if at + len > d.len() {
        return None;
    }
    Some((len, h.rate))
}
