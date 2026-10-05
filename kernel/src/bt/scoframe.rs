//! Finding SCO packets in the voice stream.
//!
//! Over USB a SCO packet always starts at the start of an isochronous
//! packet and fills the next few of them exactly (Bluetooth core spec,
//! USB transport: 3 per SCO packet). A lost isochronous packet must not
//! shift the samples, so a packet is taken only when its header starts a
//! chunk, its chunks add up to exactly its length, and the chunk after it
//! starts with the next header of the same link.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

/// Longest voice payload (largest USB alternate setting: 3 x 63 bytes).
const MAX_PAYLOAD: usize = 186;

fn header(chunk: &[u8], handle: u16) -> Option<usize> {
    if chunk.len() < 3 || u16::from_le_bytes([chunk[0], chunk[1]]) & 0x0FFF != handle {
        return None;
    }
    let len = chunk[2] as usize;
    (len > 0 && len % 2 == 0 && len <= MAX_PAYLOAD).then_some(3 + len)
}

/// Takes the next whole packet off the front of `chunks` (isochronous
/// packets as received), dropping chunks that cannot belong to one.
/// Returns None when more chunks are needed.
pub fn next(chunks: &mut VecDeque<Vec<u8>>, handle: u16) -> Option<Vec<u8>> {
    loop {
        let first = chunks.front()?;
        let Some(len) = header(first, handle) else {
            chunks.pop_front();
            continue;
        };
        let mut total = 0;
        let mut n = 0;
        while total < len {
            total += chunks.get(n)?.len();
            n += 1;
        }
        let following = chunks.get(n)?;
        if total != len || header(following, handle) != Some(len) {
            chunks.pop_front();
            continue;
        }
        let mut pkt = Vec::with_capacity(len);
        for c in chunks.drain(..n) {
            pkt.extend_from_slice(&c);
        }
        return Some(pkt);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream(packets: usize) -> Vec<Vec<u8>> {
        let mut chunks = Vec::new();
        for k in 0..packets {
            let mut p = vec![0x42, 0x00, 48];
            for _ in 0..24 {
                p.extend_from_slice(&((k * 1000 + 0x42) as i16).to_le_bytes());
            }
            chunks.extend(p.chunks(17).map(|c| c.to_vec()));
        }
        chunks
    }

    fn frame(chunks: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
        let mut q: VecDeque<_> = chunks.into();
        let mut out = Vec::new();
        while let Some(p) = next(&mut q, 0x42) {
            out.push(p);
        }
        out
    }

    fn check(pkts: &[Vec<u8>]) {
        for p in pkts {
            assert_eq!(p.len(), 51);
            let s: Vec<i16> = p[3..].chunks(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect();
            assert!(s.iter().all(|&v| v == s[0] && (v - 0x42) % 1000 == 0), "{:?}", s);
        }
    }

    #[test]
    fn aligned() {
        let got = frame(stream(5));
        assert_eq!(got.len(), 4); // the last one waits for the next header
        check(&got);
    }

    #[test]
    fn every_loss_pattern() {
        // Lose any one or two chunks: every packet taken is still whole.
        for a in 0..18 {
            for b in a..18 {
                let mut c = stream(6);
                c.remove(b);
                if b != a {
                    c.remove(a);
                }
                check(&frame(c));
            }
        }
    }

    #[test]
    fn short_chunk() {
        // A chunk that arrives one byte short breaks only its own packet.
        let mut c = stream(5);
        c[4].pop();
        let got = frame(c);
        assert_eq!(got.len(), 3);
        check(&got);
    }
}
