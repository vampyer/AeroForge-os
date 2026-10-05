//! The cryptographic toolbox of LE legacy pairing (Bluetooth Core, Vol 3,
//! Part H, 2.2): AES-128 and the confirm (c1) and key (s1) functions built
//! on it. Values are written most significant byte first, as the
//! specification prints them; SMP sends them least significant byte first,
//! so callers reverse them on the way in and out.

const SBOX: [u8; 256] = [
    0x63, 0x7c, 0x77, 0x7b, 0xf2, 0x6b, 0x6f, 0xc5, 0x30, 0x01, 0x67, 0x2b, 0xfe, 0xd7, 0xab, 0x76,
    0xca, 0x82, 0xc9, 0x7d, 0xfa, 0x59, 0x47, 0xf0, 0xad, 0xd4, 0xa2, 0xaf, 0x9c, 0xa4, 0x72, 0xc0,
    0xb7, 0xfd, 0x93, 0x26, 0x36, 0x3f, 0xf7, 0xcc, 0x34, 0xa5, 0xe5, 0xf1, 0x71, 0xd8, 0x31, 0x15,
    0x04, 0xc7, 0x23, 0xc3, 0x18, 0x96, 0x05, 0x9a, 0x07, 0x12, 0x80, 0xe2, 0xeb, 0x27, 0xb2, 0x75,
    0x09, 0x83, 0x2c, 0x1a, 0x1b, 0x6e, 0x5a, 0xa0, 0x52, 0x3b, 0xd6, 0xb3, 0x29, 0xe3, 0x2f, 0x84,
    0x53, 0xd1, 0x00, 0xed, 0x20, 0xfc, 0xb1, 0x5b, 0x6a, 0xcb, 0xbe, 0x39, 0x4a, 0x4c, 0x58, 0xcf,
    0xd0, 0xef, 0xaa, 0xfb, 0x43, 0x4d, 0x33, 0x85, 0x45, 0xf9, 0x02, 0x7f, 0x50, 0x3c, 0x9f, 0xa8,
    0x51, 0xa3, 0x40, 0x8f, 0x92, 0x9d, 0x38, 0xf5, 0xbc, 0xb6, 0xda, 0x21, 0x10, 0xff, 0xf3, 0xd2,
    0xcd, 0x0c, 0x13, 0xec, 0x5f, 0x97, 0x44, 0x17, 0xc4, 0xa7, 0x7e, 0x3d, 0x64, 0x5d, 0x19, 0x73,
    0x60, 0x81, 0x4f, 0xdc, 0x22, 0x2a, 0x90, 0x88, 0x46, 0xee, 0xb8, 0x14, 0xde, 0x5e, 0x0b, 0xdb,
    0xe0, 0x32, 0x3a, 0x0a, 0x49, 0x06, 0x24, 0x5c, 0xc2, 0xd3, 0xac, 0x62, 0x91, 0x95, 0xe4, 0x79,
    0xe7, 0xc8, 0x37, 0x6d, 0x8d, 0xd5, 0x4e, 0xa9, 0x6c, 0x56, 0xf4, 0xea, 0x65, 0x7a, 0xae, 0x08,
    0xba, 0x78, 0x25, 0x2e, 0x1c, 0xa6, 0xb4, 0xc6, 0xe8, 0xdd, 0x74, 0x1f, 0x4b, 0xbd, 0x8b, 0x8a,
    0x70, 0x3e, 0xb5, 0x66, 0x48, 0x03, 0xf6, 0x0e, 0x61, 0x35, 0x57, 0xb9, 0x86, 0xc1, 0x1d, 0x9e,
    0xe1, 0xf8, 0x98, 0x11, 0x69, 0xd9, 0x8e, 0x94, 0x9b, 0x1e, 0x87, 0xe9, 0xce, 0x55, 0x28, 0xdf,
    0x8c, 0xa1, 0x89, 0x0d, 0xbf, 0xe6, 0x42, 0x68, 0x41, 0x99, 0x2d, 0x0f, 0xb0, 0x54, 0xbb, 0x16,
];

fn xtime(b: u8) -> u8 {
    (b << 1) ^ if b & 0x80 != 0 { 0x1b } else { 0 }
}

/// AES-128 encryption of one block (FIPS-197).
pub fn aes128(key: &[u8; 16], block: &[u8; 16]) -> [u8; 16] {
    let mut rk = [[0u8; 16]; 11];
    rk[0] = *key;
    let mut rcon = 1u8;
    for r in 1..11 {
        let p = rk[r - 1];
        let t = [SBOX[p[13] as usize] ^ rcon, SBOX[p[14] as usize], SBOX[p[15] as usize], SBOX[p[12] as usize]];
        for i in 0..4 {
            rk[r][i] = p[i] ^ t[i];
        }
        for i in 4..16 {
            rk[r][i] = p[i] ^ rk[r][i - 4];
        }
        rcon = xtime(rcon);
    }
    let mut s = *block;
    for i in 0..16 {
        s[i] ^= rk[0][i];
    }
    for (r, k) in rk.iter().enumerate().skip(1) {
        for b in s.iter_mut() {
            *b = SBOX[*b as usize];
        }
        // Shift rows (the state is column-major: byte 4c + r is row r).
        let t = s;
        for c in 0..4 {
            for row in 0..4 {
                s[4 * c + row] = t[4 * ((c + row) % 4) + row];
            }
        }
        if r != 10 {
            for c in 0..4 {
                let col = [s[4 * c], s[4 * c + 1], s[4 * c + 2], s[4 * c + 3]];
                let all = col[0] ^ col[1] ^ col[2] ^ col[3];
                for row in 0..4 {
                    s[4 * c + row] = col[row] ^ all ^ xtime(col[row] ^ col[(row + 1) % 4]);
                }
            }
        }
        for i in 0..16 {
            s[i] ^= k[i];
        }
    }
    s
}

fn xor(a: &[u8; 16], b: &[u8; 16]) -> [u8; 16] {
    core::array::from_fn(|i| a[i] ^ b[i])
}

/// The confirm value: `preq`/`pres` are the 7-byte pairing request and
/// response, `iat`/`rat` the initiator's and responder's address types
/// (0 public, 1 random) and `ia`/`ra` their addresses.
#[allow(clippy::too_many_arguments)]
pub fn c1(k: &[u8; 16], r: &[u8; 16], preq: &[u8; 7], pres: &[u8; 7], iat: u8, rat: u8, ia: &[u8; 6], ra: &[u8; 6]) -> [u8; 16] {
    let mut p1 = [0u8; 16];
    p1[..7].copy_from_slice(pres);
    p1[7..14].copy_from_slice(preq);
    p1[14] = rat;
    p1[15] = iat;
    let mut p2 = [0u8; 16];
    p2[4..10].copy_from_slice(ia);
    p2[10..].copy_from_slice(ra);
    aes128(k, &xor(&aes128(k, &xor(r, &p1)), &p2))
}

/// The short-term key from the responder's (`r1`) and initiator's (`r2`)
/// random values.
pub fn s1(k: &[u8; 16], r1: &[u8; 16], r2: &[u8; 16]) -> [u8; 16] {
    let mut r = [0u8; 16];
    r[..8].copy_from_slice(&r1[8..]);
    r[8..].copy_from_slice(&r2[8..]);
    aes128(k, &r)
}

/// Reverses a value between SMP (least significant byte first) and the
/// order the functions above use.
pub fn rev<const N: usize>(v: &[u8]) -> [u8; N] {
    core::array::from_fn(|i| v[N - 1 - i])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex<const N: usize>(s: &str) -> [u8; N] {
        core::array::from_fn(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
    }

    #[test]
    fn fips197() {
        let k = hex("000102030405060708090a0b0c0d0e0f");
        let p = hex("00112233445566778899aabbccddeeff");
        assert_eq!(aes128(&k, &p), hex::<16>("69c4e0d86a7b0430d8cdb78070b4c55a"));
    }

    #[test]
    fn confirm_and_key() {
        // Bluetooth Core, Vol 3, Part H, 2.2.3 and 2.2.4.
        let k = [0u8; 16];
        let r = hex("5783D52156AD6F0E6388274EC6702EE0");
        let c = c1(&k, &r, &hex("07071000000101"), &hex("05000800000302"), 1, 0,
            &hex("A1A2A3A4A5A6"), &hex("B1B2B3B4B5B6"));
        assert_eq!(c, hex::<16>("1e1e3fef878988ead2a74dc5bef13b86"));
        let s = s1(&k, &hex("000F0E0D0C0B0A091122334455667788"), &hex("010203040506070899AABBCCDDEEFF00"));
        assert_eq!(s, hex::<16>("9a1fe1f0e8b0f49b5b4216ae796da062"));
    }
}
