//! A small SMB2 client, for Windows shares and Samba: enough to sign in
//! with a user name and password (NTLMv2), open a share and list, read,
//! write, make, rename and delete files and folders in it.
//!
//! It speaks dialects 2.0.2 and 2.1 over TCP (port 445 unless another is
//! given) and signs every request once signed in (HMAC-SHA256), which
//! Windows 11 asks for. Sign-in sends the bare NTLMSSP messages, which
//! Windows and Samba both take without a SPNEGO wrapper. Reads and writes
//! go in pieces of up to 64 KiB, one credit each.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::net::{self, Endpoint, Socket};
use crate::{DirEntry, E_CLOSED, E_EXISTS, E_FULL, E_INVAL, E_NOTFOUND, E_RIGHTS, E_TIMEDOUT};

// ------------------------------------------------------------------ hashes

/// MD4 (RFC 1320), only for the NT password hash.
pub fn md4(data: &[u8]) -> [u8; 16] {
    let mut h: [u32; 4] = [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476];
    let mut msg = data.to_vec();
    let bits = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bits.to_le_bytes());
    for block in msg.chunks(64) {
        let x: Vec<u32> = block.chunks(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
        let [mut a, mut b, mut c, mut d] = h;
        let f = |x: u32, y: u32, z: u32| (x & y) | (!x & z);
        let g = |x: u32, y: u32, z: u32| (x & y) | (x & z) | (y & z);
        let hh = |x: u32, y: u32, z: u32| x ^ y ^ z;
        for &i in &[0, 4, 8, 12] {
            a = a.wrapping_add(f(b, c, d)).wrapping_add(x[i]).rotate_left(3);
            d = d.wrapping_add(f(a, b, c)).wrapping_add(x[i + 1]).rotate_left(7);
            c = c.wrapping_add(f(d, a, b)).wrapping_add(x[i + 2]).rotate_left(11);
            b = b.wrapping_add(f(c, d, a)).wrapping_add(x[i + 3]).rotate_left(19);
        }
        for &i in &[0, 1, 2, 3] {
            a = a.wrapping_add(g(b, c, d)).wrapping_add(x[i]).wrapping_add(0x5a827999).rotate_left(3);
            d = d.wrapping_add(g(a, b, c)).wrapping_add(x[i + 4]).wrapping_add(0x5a827999).rotate_left(5);
            c = c.wrapping_add(g(d, a, b)).wrapping_add(x[i + 8]).wrapping_add(0x5a827999).rotate_left(9);
            b = b.wrapping_add(g(c, d, a)).wrapping_add(x[i + 12]).wrapping_add(0x5a827999).rotate_left(13);
        }
        for &i in &[0, 2, 1, 3] {
            a = a.wrapping_add(hh(b, c, d)).wrapping_add(x[i]).wrapping_add(0x6ed9eba1).rotate_left(3);
            d = d.wrapping_add(hh(a, b, c)).wrapping_add(x[i + 8]).wrapping_add(0x6ed9eba1).rotate_left(9);
            c = c.wrapping_add(hh(d, a, b)).wrapping_add(x[i + 4]).wrapping_add(0x6ed9eba1).rotate_left(11);
            b = b.wrapping_add(hh(c, d, a)).wrapping_add(x[i + 12]).wrapping_add(0x6ed9eba1).rotate_left(15);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
    }
    let mut out = [0u8; 16];
    for (i, v) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
    }
    out
}

/// MD5 (RFC 1321), for HMAC-MD5 in NTLMv2.
pub fn md5(data: &[u8]) -> [u8; 16] {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20,
        4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    const K: [u32; 64] = [
        0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613, 0xfd469501, 0x698098d8,
        0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821, 0xf61e2562, 0xc040b340,
        0x265e5a51, 0xe9b6c7aa, 0xd62f105d, 0x02441453, 0xd8a1e681, 0xe7d3fbc8, 0x21e1cde6, 0xc33707d6, 0xf4d50d87,
        0x455a14ed, 0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a, 0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c,
        0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70, 0x289b7ec6, 0xeaa127fa, 0xd4ef3085, 0x04881d05, 0xd9d4d039,
        0xe6db99e5, 0x1fa27cf8, 0xc4ac5665, 0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92,
        0xffeff47d, 0x85845dd1, 0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb,
        0xeb86d391,
    ];
    let mut h: [u32; 4] = [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476];
    let mut msg = data.to_vec();
    let bits = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bits.to_le_bytes());
    for block in msg.chunks(64) {
        let m: Vec<u32> = block.chunks(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
        let [mut a, mut b, mut c, mut d] = h;
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let t = d;
            d = c;
            c = b;
            b = b.wrapping_add(a.wrapping_add(f).wrapping_add(K[i]).wrapping_add(m[g]).rotate_left(S[i]));
            a = t;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
    }
    let mut out = [0u8; 16];
    for (i, v) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
    }
    out
}

/// SHA-256 (FIPS 180-4), for SMB 2 signing.
pub fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98,
        0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786,
        0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8,
        0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13,
        0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819,
        0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a,
        0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] =
        [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19];
    let mut msg = data.to_vec();
    let bits = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bits.to_be_bytes());
    for block in msg.chunks(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([block[4 * i], block[4 * i + 1], block[4 * i + 2], block[4 * i + 3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let mut v = h;
        for i in 0..64 {
            let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
            let ch = (v[4] & v[5]) ^ (!v[4] & v[6]);
            let t1 = v[7].wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
            let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
            let t2 = s0.wrapping_add(maj);
            v = [t1.wrapping_add(t2), v[0], v[1], v[2], v[3].wrapping_add(t1), v[4], v[5], v[6]];
        }
        for i in 0..8 {
            h[i] = h[i].wrapping_add(v[i]);
        }
    }
    let mut out = [0u8; 32];
    for (i, v) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_be_bytes());
    }
    out
}

/// HMAC (RFC 2104) over a 64-byte-block hash.
fn hmac<const N: usize>(hash: fn(&[u8]) -> [u8; N], key: &[u8], parts: &[&[u8]]) -> [u8; N] {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..N].copy_from_slice(&hash(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner: Vec<u8> = k.iter().map(|b| b ^ 0x36).collect();
    for p in parts {
        inner.extend_from_slice(p);
    }
    let mut outer: Vec<u8> = k.iter().map(|b| b ^ 0x5c).collect();
    outer.extend_from_slice(&hash(&inner));
    hash(&outer)
}

pub fn hmac_md5(key: &[u8], parts: &[&[u8]]) -> [u8; 16] {
    hmac(md5, key, parts)
}

pub fn hmac_sha256(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    hmac(sha256, key, parts)
}

fn utf16(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
}

fn from_utf16(b: &[u8]) -> String {
    let units: Vec<u16> = b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    String::from_utf16_lossy(&units)
}

// -------------------------------------------------------------------- NTLM

const NTLM_UNICODE: u32 = 0x0000_0001;
const NTLM_REQUEST_TARGET: u32 = 0x0000_0004;
const NTLM_NTLM: u32 = 0x0000_0200;
const NTLM_ALWAYS_SIGN: u32 = 0x0000_8000;
const NTLM_EXTENDED_SECURITY: u32 = 0x0008_0000;
const NTLM_TARGET_INFO: u32 = 0x0080_0000;
const NTLM_128: u32 = 0x2000_0000;
const NTLM_56: u32 = 0x8000_0000;
const NTLM_FLAGS: u32 =
    NTLM_UNICODE | NTLM_REQUEST_TARGET | NTLM_NTLM | NTLM_ALWAYS_SIGN | NTLM_EXTENDED_SECURITY | NTLM_TARGET_INFO | NTLM_128 | NTLM_56;

fn ntlm_negotiate() -> Vec<u8> {
    let mut m = Vec::from(&b"NTLMSSP\0"[..]);
    m.extend_from_slice(&1u32.to_le_bytes());
    m.extend_from_slice(&NTLM_FLAGS.to_le_bytes());
    m.extend_from_slice(&[0u8; 16]); // no domain or workstation given
    m
}

/// The parts of the server's CHALLENGE message the answer needs.
struct Challenge {
    server_challenge: [u8; 8],
    target_info: Vec<u8>,
    flags: u32,
    /// MsvAvTimestamp from the target info, when the server sent one.
    timestamp: Option<[u8; 8]>,
}

fn ntlm_challenge(m: &[u8]) -> Option<Challenge> {
    if m.len() < 48 || &m[..8] != b"NTLMSSP\0" || u32::from_le_bytes(m[8..12].try_into().ok()?) != 2 {
        return None;
    }
    let flags = u32::from_le_bytes(m[20..24].try_into().ok()?);
    let server_challenge: [u8; 8] = m[24..32].try_into().ok()?;
    let len = u16::from_le_bytes([m[40], m[41]]) as usize;
    let off = u32::from_le_bytes(m[44..48].try_into().ok()?) as usize;
    let target_info = m.get(off..off + len)?.to_vec();
    let mut timestamp = None;
    let mut i = 0;
    while i + 4 <= target_info.len() {
        let id = u16::from_le_bytes([target_info[i], target_info[i + 1]]);
        let l = u16::from_le_bytes([target_info[i + 2], target_info[i + 3]]) as usize;
        if id == 0 {
            break;
        }
        if id == 7 && l == 8 {
            timestamp = target_info.get(i + 4..i + 12).and_then(|t| t.try_into().ok());
        }
        i += 4 + l;
    }
    Some(Challenge { server_challenge, target_info, flags, timestamp })
}

/// Now as a Windows FILETIME (100 ns steps since 1601), from the PC clock.
fn filetime_now() -> [u8; 8] {
    let Some(t) = crate::now() else { return [0; 8] };
    // Days from 1970-01-01 (Howard Hinnant's days_from_civil).
    let (y, m, d) = (t.year as i64 - if t.month <= 2 { 1 } else { 0 }, t.month as i64, t.day as i64);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    let secs = days * 86400 + t.hour as i64 * 3600 + t.minute as i64 * 60 + t.second as i64;
    (((secs + 11_644_473_600) as u64) * 10_000_000).to_le_bytes()
}

/// Eight bytes that differ every time (the clock, stirred).
fn nonce() -> [u8; 8] {
    let mut x = crate::clock_us() ^ 0x9E37_79B9_7F4A_7C15;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    x.to_le_bytes()
}

/// The AUTHENTICATE message for `user` and `password` (NTLMv2), and the
/// session key both sides now share.
fn ntlm_authenticate(ch: &Challenge, domain: &str, user: &str, password: &str) -> (Vec<u8>, [u8; 16]) {
    let nt_hash = md4(&utf16(password));
    let mut who = user.to_uppercase();
    who.push_str(domain);
    let ntowf = hmac_md5(&nt_hash, &[&utf16(&who)]);
    let mut blob = vec![1u8, 1, 0, 0, 0, 0, 0, 0];
    blob.extend_from_slice(&ch.timestamp.unwrap_or_else(filetime_now));
    blob.extend_from_slice(&nonce());
    blob.extend_from_slice(&[0; 4]);
    blob.extend_from_slice(&ch.target_info);
    blob.extend_from_slice(&[0; 4]);
    let proof = hmac_md5(&ntowf, &[&ch.server_challenge, &blob]);
    let session_key = hmac_md5(&ntowf, &[&proof]);
    let mut nt = proof.to_vec();
    nt.extend_from_slice(&blob);
    let lm = [0u8; 24];
    let (dom, usr, ws) = (utf16(domain), utf16(user), utf16("AEROFORGE"));

    let mut m = Vec::from(&b"NTLMSSP\0"[..]);
    m.extend_from_slice(&3u32.to_le_bytes());
    let mut payload = Vec::new();
    let header = 64;
    let mut field = |m: &mut Vec<u8>, data: &[u8]| {
        let off = header + payload.len();
        m.extend_from_slice(&(data.len() as u16).to_le_bytes());
        m.extend_from_slice(&(data.len() as u16).to_le_bytes());
        m.extend_from_slice(&(off as u32).to_le_bytes());
        payload.extend_from_slice(data);
    };
    field(&mut m, &lm);
    field(&mut m, &nt);
    field(&mut m, &dom);
    field(&mut m, &usr);
    field(&mut m, &ws);
    field(&mut m, &[]); // no encrypted session key: no key exchange asked for
    m.extend_from_slice(&(NTLM_FLAGS & ch.flags | NTLM_UNICODE).to_le_bytes());
    m.extend_from_slice(&payload);
    (m, session_key)
}

// -------------------------------------------------------------------- SMB2

const NEGOTIATE: u16 = 0;
const SESSION_SETUP: u16 = 1;
const TREE_CONNECT: u16 = 3;
const CREATE: u16 = 5;
const CLOSE: u16 = 6;
const READ: u16 = 8;
const WRITE: u16 = 9;
const QUERY_DIRECTORY: u16 = 14;
const SET_INFO: u16 = 17;

const STATUS_OK: u32 = 0;
const STATUS_PENDING: u32 = 0x0000_0103;
const STATUS_MORE_PROCESSING: u32 = 0xC000_0016;
const STATUS_NO_MORE_FILES: u32 = 0x8000_0006;
const STATUS_END_OF_FILE: u32 = 0xC000_0011;

const FLAG_SIGNED: u32 = 0x8;

/// Biggest piece read or written at once.
const CHUNK: usize = 64 * 1024;

/// A failed request: what went wrong, as an `aero` error code.
fn status_error(status: u32) -> i64 {
    match status {
        0xC000_0034 | 0xC000_003A | 0xC000_000F | 0xC000_00CC => E_NOTFOUND,
        0xC000_0022 | 0xC000_006D | 0xC000_00BA | 0xC000_0043 => E_RIGHTS,
        0xC000_0035 | 0xC000_0101 => E_EXISTS,
        0xC000_007F => E_FULL,
        _ => E_INVAL,
    }
}

/// What went wrong when connecting, for people.
fn status_text(status: u32) -> &'static str {
    match status {
        0xC000_006D => "the user name or password is wrong",
        0xC000_0072 => "that account is turned off",
        0xC000_0071 => "that password has expired",
        0xC000_00CC => "there is no share with that name",
        0xC000_0022 => "access is denied",
        _ => "the server refused",
    }
}

/// A file or folder open on the share.
#[derive(Clone, Copy)]
struct FileId([u8; 16]);

/// One signed-in connection to one share.
pub struct Client {
    sock: Socket,
    message_id: u64,
    session_id: u64,
    tree_id: u32,
    sign_key: Option<[u8; 16]>,
    /// Bytes received but not yet taken as a message.
    pending: Vec<u8>,
}

/// One answer: its header fields that matter and its body.
struct Reply {
    status: u32,
    session_id: u64,
    tree_id: u32,
    body: Vec<u8>,
}

impl Client {
    /// Connects to `\\host\share` (host may be "name", "1.2.3.4" or
    /// "1.2.3.4:4450") and signs in. `user` may be "DOMAIN\user".
    pub fn connect(host: &str, share: &str, user: &str, password: &str) -> Result<Client, String> {
        let (name, port) = match host.rsplit_once(':') {
            Some((n, p)) => (n, p.parse().map_err(|_| String::from("the port number is not right"))?),
            None => (host, 445),
        };
        let addr = net::resolve(name, 3_000_000).map_err(|e| {
            String::from(if e == E_TIMEDOUT { "no answer when looking up that name" } else { "there is no computer by that name" })
        })?;
        let sock = Socket::tcp().map_err(|_| String::from("no network"))?;
        sock.connect(Endpoint::new(addr, port), 5_000_000)
            .map_err(|_| alloc::format!("{} did not answer on port {}", net::Ip(addr), port))?;
        let mut c = Client { sock, message_id: 0, session_id: 0, tree_id: 0, sign_key: None, pending: Vec::new() };

        // Negotiate: 2.0.2 or 2.1, signing enabled.
        let mut body = Vec::new();
        body.extend_from_slice(&36u16.to_le_bytes());
        body.extend_from_slice(&2u16.to_le_bytes());
        body.extend_from_slice(&1u16.to_le_bytes()); // signing enabled
        body.extend_from_slice(&[0; 2]);
        body.extend_from_slice(&[0; 4]); // capabilities
        body.extend_from_slice(&nonce());
        body.extend_from_slice(&nonce()); // client GUID
        body.extend_from_slice(&[0; 8]);
        body.extend_from_slice(&0x0202u16.to_le_bytes());
        body.extend_from_slice(&0x0210u16.to_le_bytes());
        let r = c.call(NEGOTIATE, &body).map_err(|_| String::from("it does not speak SMB 2"))?;
        if r.status != STATUS_OK || r.body.len() < 64 {
            return Err(String::from("it would not agree on an SMB version (2.0 or 2.1)"));
        }
        let signing_required = r.body[2] & 2 != 0;

        // Sign in: NEGOTIATE, then AUTHENTICATE answering the challenge.
        let (domain, user) = user.split_once('\\').unwrap_or(("", user));
        let r = c.session_setup(&ntlm_negotiate()).map_err(|_| String::from("the connection dropped while signing in"))?;
        if r.status != STATUS_MORE_PROCESSING {
            return Err(alloc::format!("signing in failed: {}", status_text(r.status)));
        }
        c.session_id = r.session_id;
        let blob = security_buffer(&r.body).ok_or_else(|| String::from("the server's sign-in answer made no sense"))?;
        let ch = ntlm_challenge(blob).ok_or_else(|| String::from("the server's sign-in answer made no sense"))?;
        let (auth, key) = ntlm_authenticate(&ch, domain, user, password);
        let r = c.session_setup(&auth).map_err(|_| String::from("the connection dropped while signing in"))?;
        if r.status != STATUS_OK {
            return Err(alloc::format!("signing in failed: {}", status_text(r.status)));
        }
        let guest = r.body.len() >= 4 && u16::from_le_bytes([r.body[2], r.body[3]]) & 3 != 0;
        if !guest {
            c.sign_key = Some(key);
        } else if signing_required {
            return Err(String::from("the server let us in only as a guest, but wants signing"));
        }

        // The share.
        let path = utf16(&alloc::format!("\\\\{}\\{}", name, share));
        let mut body = Vec::new();
        body.extend_from_slice(&9u16.to_le_bytes());
        body.extend_from_slice(&[0; 2]);
        body.extend_from_slice(&72u16.to_le_bytes());
        body.extend_from_slice(&(path.len() as u16).to_le_bytes());
        body.extend_from_slice(&path);
        let r = c.call(TREE_CONNECT, &body).map_err(|_| String::from("the connection dropped"))?;
        if r.status != STATUS_OK {
            return Err(alloc::format!("opening the share failed: {}", status_text(r.status)));
        }
        c.tree_id = r.tree_id;
        Ok(c)
    }

    fn session_setup(&mut self, token: &[u8]) -> Result<Reply, i64> {
        let mut body = Vec::new();
        body.extend_from_slice(&25u16.to_le_bytes());
        body.push(0); // flags
        body.push(1); // signing enabled
        body.extend_from_slice(&[0; 4]); // capabilities
        body.extend_from_slice(&[0; 4]); // channel
        body.extend_from_slice(&(64u16 + 24).to_le_bytes());
        body.extend_from_slice(&(token.len() as u16).to_le_bytes());
        body.extend_from_slice(&[0; 8]); // previous session
        body.extend_from_slice(token);
        self.call(SESSION_SETUP, &body)
    }

    /// Sends one request and waits for its answer (skipping "pending" notes).
    fn call(&mut self, command: u16, body: &[u8]) -> Result<Reply, i64> {
        let id = self.message_id;
        self.message_id += 1;
        let mut m = Vec::with_capacity(64 + body.len());
        m.extend_from_slice(b"\xfeSMB");
        m.extend_from_slice(&64u16.to_le_bytes());
        m.extend_from_slice(&0u16.to_le_bytes()); // credit charge
        m.extend_from_slice(&0u32.to_le_bytes()); // status
        m.extend_from_slice(&command.to_le_bytes());
        m.extend_from_slice(&8u16.to_le_bytes()); // credits asked for
        let signed = self.sign_key.is_some() && command != NEGOTIATE && command != SESSION_SETUP;
        m.extend_from_slice(&(if signed { FLAG_SIGNED } else { 0 }).to_le_bytes());
        m.extend_from_slice(&0u32.to_le_bytes()); // next command
        m.extend_from_slice(&id.to_le_bytes());
        m.extend_from_slice(&0xFEFFu32.to_le_bytes()); // process id
        m.extend_from_slice(&self.tree_id.to_le_bytes());
        m.extend_from_slice(&self.session_id.to_le_bytes());
        m.extend_from_slice(&[0; 16]);
        m.extend_from_slice(body);
        if let (true, Some(key)) = (signed, self.sign_key) {
            let mac = hmac_sha256(&key, &[&m]);
            m[48..64].copy_from_slice(&mac[..16]);
        }
        let mut framed = Vec::with_capacity(4 + m.len());
        framed.extend_from_slice(&(m.len() as u32).to_be_bytes());
        framed.extend_from_slice(&m);
        self.sock.send_all(&framed)?;
        loop {
            let msg = self.receive()?;
            if msg.len() < 64 || &msg[..4] != b"\xfeSMB" {
                return Err(E_INVAL);
            }
            let status = u32::from_le_bytes(msg[8..12].try_into().unwrap());
            let flags = u32::from_le_bytes(msg[16..20].try_into().unwrap());
            let mid = u64::from_le_bytes(msg[24..32].try_into().unwrap());
            // An interim "still working on it" (async) answer: the real one follows.
            if mid != id || (status == STATUS_PENDING && flags & 2 != 0) {
                continue;
            }
            return Ok(Reply {
                status,
                tree_id: u32::from_le_bytes(msg[36..40].try_into().unwrap()),
                session_id: u64::from_le_bytes(msg[40..48].try_into().unwrap()),
                body: msg[64..].to_vec(),
            });
        }
    }

    /// One whole message from the server.
    fn receive(&mut self) -> Result<Vec<u8>, i64> {
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            if self.pending.len() >= 4 {
                let len = u32::from_be_bytes(self.pending[..4].try_into().unwrap()) as usize & 0xFF_FFFF;
                if self.pending.len() >= 4 + len {
                    let msg = self.pending[4..4 + len].to_vec();
                    self.pending.drain(..4 + len);
                    return Ok(msg);
                }
            }
            let n = self.sock.recv(&mut buf, 15_000_000)?;
            if n == 0 {
                return Err(E_CLOSED);
            }
            self.pending.extend_from_slice(&buf[..n]);
        }
    }

    /// Opens `path` (inside the share, "/"-separated).
    fn create(&mut self, path: &str, access: u32, disposition: u32, options: u32) -> Result<(FileId, u64), i64> {
        let name = utf16(&path.trim_matches('/').replace('/', "\\"));
        let mut body = Vec::new();
        body.extend_from_slice(&57u16.to_le_bytes());
        body.push(0); // security flags
        body.push(0); // no oplock
        body.extend_from_slice(&2u32.to_le_bytes()); // impersonation
        body.extend_from_slice(&[0; 16]);
        body.extend_from_slice(&access.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes()); // attributes
        body.extend_from_slice(&7u32.to_le_bytes()); // share read, write, delete
        body.extend_from_slice(&disposition.to_le_bytes());
        body.extend_from_slice(&options.to_le_bytes());
        body.extend_from_slice(&(64u16 + 56).to_le_bytes());
        body.extend_from_slice(&(name.len() as u16).to_le_bytes());
        body.extend_from_slice(&[0; 8]); // no create contexts
        body.extend_from_slice(&name);
        if name.is_empty() {
            body.push(0);
        }
        let r = self.call(CREATE, &body)?;
        if r.status != STATUS_OK {
            return Err(status_error(r.status));
        }
        if r.body.len() < 80 {
            return Err(E_INVAL);
        }
        let size = u64::from_le_bytes(r.body[48..56].try_into().unwrap());
        Ok((FileId(r.body[64..80].try_into().unwrap()), size))
    }

    fn close(&mut self, f: FileId) -> Result<(), i64> {
        let mut body = Vec::new();
        body.extend_from_slice(&24u16.to_le_bytes());
        body.extend_from_slice(&[0; 6]);
        body.extend_from_slice(&f.0);
        let r = self.call(CLOSE, &body)?;
        if r.status != STATUS_OK { Err(status_error(r.status)) } else { Ok(()) }
    }

    /// The files and folders in `path` ("" or "/" is the share itself).
    pub fn list(&mut self, path: &str) -> Result<Vec<DirEntry>, i64> {
        let (dir, _) = self.create(path, 0x0010_0081, 1, 0x1)?;
        let mut out = Vec::new();
        let mut first = true;
        let result = loop {
            let mut body = Vec::new();
            body.extend_from_slice(&33u16.to_le_bytes());
            body.push(1); // FileDirectoryInformation
            body.push(if first { 1 } else { 0 }); // restart scans
            body.extend_from_slice(&[0; 4]);
            body.extend_from_slice(&dir.0);
            let pattern = utf16("*");
            body.extend_from_slice(&96u16.to_le_bytes());
            body.extend_from_slice(&(pattern.len() as u16).to_le_bytes());
            body.extend_from_slice(&(CHUNK as u32).to_le_bytes());
            body.extend_from_slice(&pattern);
            first = false;
            let r = match self.call(QUERY_DIRECTORY, &body) {
                Ok(r) => r,
                Err(e) => break Err(e),
            };
            if r.status == STATUS_NO_MORE_FILES {
                break Ok(());
            }
            if r.status != STATUS_OK || r.body.len() < 8 {
                break Err(status_error(r.status));
            }
            let off = u16::from_le_bytes([r.body[2], r.body[3]]) as usize;
            let len = u32::from_le_bytes(r.body[4..8].try_into().unwrap()) as usize;
            let Some(data) = r.body.get(off.saturating_sub(64)..(off + len).saturating_sub(64)) else { break Err(E_INVAL) };
            let mut i = 0;
            loop {
                let Some(e) = data.get(i..) else { break };
                if e.len() < 64 {
                    break;
                }
                let next = u32::from_le_bytes(e[0..4].try_into().unwrap()) as usize;
                let size = u64::from_le_bytes(e[40..48].try_into().unwrap());
                let attrs = u32::from_le_bytes(e[56..60].try_into().unwrap());
                let nlen = u32::from_le_bytes(e[60..64].try_into().unwrap()) as usize;
                let name = from_utf16(e.get(64..64 + nlen).unwrap_or(&[]));
                // Hidden and system files stay hidden, as in Explorer.
                if name != "." && name != ".." && attrs & 0x6 == 0 {
                    let is_dir = attrs & 0x10 != 0;
                    let modified = crate::packed_filetime(u64::from_le_bytes(e[24..32].try_into().unwrap()));
                    out.push(DirEntry { name, is_dir, size: if is_dir { 0 } else { size }, modified });
                }
                if next == 0 {
                    break;
                }
                i += next;
            }
        };
        let _ = self.close(dir);
        result.map(|_| out)
    }

    /// Reads up to `limit` bytes of a file.
    pub fn read(&mut self, path: &str, limit: usize) -> Result<Vec<u8>, i64> {
        let (f, size) = self.create(path, 0x0012_0089, 1, 0x40)?;
        let want = (size as usize).min(limit);
        let mut out = Vec::with_capacity(want);
        let result = loop {
            if out.len() >= want {
                break Ok(());
            }
            let n = (want - out.len()).min(CHUNK);
            let mut body = Vec::new();
            body.extend_from_slice(&49u16.to_le_bytes());
            body.push(0x50);
            body.push(0);
            body.extend_from_slice(&(n as u32).to_le_bytes());
            body.extend_from_slice(&(out.len() as u64).to_le_bytes());
            body.extend_from_slice(&f.0);
            body.extend_from_slice(&[0; 16]); // minimum, channel, remaining, channel info
            body.push(0);
            let r = match self.call(READ, &body) {
                Ok(r) => r,
                Err(e) => break Err(e),
            };
            if r.status == STATUS_END_OF_FILE {
                break Ok(());
            }
            if r.status != STATUS_OK || r.body.len() < 8 {
                break Err(status_error(r.status));
            }
            let off = r.body[2] as usize;
            let len = u32::from_le_bytes(r.body[4..8].try_into().unwrap()) as usize;
            match r.body.get(off.saturating_sub(64)..off.saturating_sub(64) + len) {
                Some(d) if !d.is_empty() => out.extend_from_slice(d),
                _ => break Ok(()),
            }
        };
        let _ = self.close(f);
        result.map(|_| out)
    }

    /// Creates the file, or replaces what it holds.
    pub fn write(&mut self, path: &str, data: &[u8]) -> Result<(), i64> {
        let (f, _) = self.create(path, 0x0012_0196, 5, 0x40)?;
        let mut at = 0;
        let result = loop {
            if at >= data.len() {
                break Ok(());
            }
            let piece = &data[at..(at + CHUNK).min(data.len())];
            let mut body = Vec::new();
            body.extend_from_slice(&49u16.to_le_bytes());
            body.extend_from_slice(&(64u16 + 48).to_le_bytes());
            body.extend_from_slice(&(piece.len() as u32).to_le_bytes());
            body.extend_from_slice(&(at as u64).to_le_bytes());
            body.extend_from_slice(&f.0);
            body.extend_from_slice(&[0; 16]); // channel, remaining, channel info, flags
            body.extend_from_slice(piece);
            match self.call(WRITE, &body) {
                Ok(r) if r.status == STATUS_OK && r.body.len() >= 8 => {
                    let n = u32::from_le_bytes(r.body[4..8].try_into().unwrap()) as usize;
                    if n == 0 {
                        break Err(E_FULL);
                    }
                    at += n;
                }
                Ok(r) => break Err(status_error(r.status)),
                Err(e) => break Err(e),
            }
        };
        let closed = self.close(f);
        result.and(closed)
    }

    /// Makes a folder.
    pub fn create_dir(&mut self, path: &str) -> Result<(), i64> {
        let (f, _) = self.create(path, 0x0010_0081, 2, 0x1)?;
        self.close(f)
    }

    /// Deletes a file or an empty folder.
    pub fn delete(&mut self, path: &str, is_dir: bool) -> Result<(), i64> {
        let options = 0x1000 | if is_dir { 0x1 } else { 0x40 };
        let (f, _) = self.create(path, 0x0011_0080, 1, options)?;
        self.close(f)
    }

    /// Renames or moves `from` to `to`, both inside the share.
    pub fn rename(&mut self, from: &str, to: &str) -> Result<(), i64> {
        let (f, _) = self.create(from, 0x0011_0080, 1, 0)?;
        let name = utf16(&to.trim_matches('/').replace('/', "\\"));
        let mut info = Vec::new();
        info.push(0); // don't replace
        info.extend_from_slice(&[0; 7]);
        info.extend_from_slice(&[0; 8]); // root directory
        info.extend_from_slice(&(name.len() as u32).to_le_bytes());
        info.extend_from_slice(&name);
        let mut body = Vec::new();
        body.extend_from_slice(&33u16.to_le_bytes());
        body.push(1); // file information
        body.push(10); // FileRenameInformation
        body.extend_from_slice(&(info.len() as u32).to_le_bytes());
        body.extend_from_slice(&96u16.to_le_bytes());
        body.extend_from_slice(&[0; 2]);
        body.extend_from_slice(&[0; 4]);
        body.extend_from_slice(&f.0);
        body.extend_from_slice(&info);
        let r = self.call(SET_INFO, &body);
        let _ = self.close(f);
        match r {
            Ok(r) if r.status == STATUS_OK => Ok(()),
            Ok(r) => Err(status_error(r.status)),
            Err(e) => Err(e),
        }
    }
}

/// The security blob in a SESSION_SETUP answer.
fn security_buffer(body: &[u8]) -> Option<&[u8]> {
    if body.len() < 8 {
        return None;
    }
    let off = u16::from_le_bytes([body[4], body[5]]) as usize;
    let len = u16::from_le_bytes([body[6], body[7]]) as usize;
    body.get(off.checked_sub(64)?..off - 64 + len)
}
