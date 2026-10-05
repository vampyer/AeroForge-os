//! SDP client, just enough to read a HID device's report descriptor: one
//! ServiceSearchAttribute request for the HID service (UUID 0x1124) asking
//! for the HIDDescriptorList attribute (0x0206), following continuations.

use alloc::vec::Vec;

pub const PSM: u16 = 0x0001;
const SEARCH_ATTRIBUTE_REQUEST: u8 = 0x06;
const SEARCH_ATTRIBUTE_RESPONSE: u8 = 0x07;
const HID_SERVICE: u16 = 0x1124;
const HID_DESCRIPTOR_LIST: u16 = 0x0206;

/// The request PDU; `continuation` is the state from the previous response.
pub fn request(transaction: u16, continuation: &[u8]) -> Vec<u8> {
    let mut params = alloc::vec![
        0x35, 3, 0x19, (HID_SERVICE >> 8) as u8, HID_SERVICE as u8, // search pattern: UUID16
        0x02, 0x00,                                                   // max attribute bytes: 512
        0x35, 3, 0x09, (HID_DESCRIPTOR_LIST >> 8) as u8, HID_DESCRIPTOR_LIST as u8,
    ];
    params.push(continuation.len() as u8);
    params.extend_from_slice(continuation);
    let mut pdu = alloc::vec![SEARCH_ATTRIBUTE_REQUEST];
    pdu.extend_from_slice(&transaction.to_be_bytes());
    pdu.extend_from_slice(&(params.len() as u16).to_be_bytes());
    pdu.extend_from_slice(&params);
    pdu
}

/// One response: appends its part of the attribute lists to `lists` and
/// returns the continuation state (empty when the answer is complete).
pub fn response(pdu: &[u8], lists: &mut Vec<u8>) -> Result<Vec<u8>, &'static str> {
    if pdu.len() < 7 || pdu[0] != SEARCH_ATTRIBUTE_RESPONSE {
        return Err("unexpected SDP response");
    }
    let count = u16::from_be_bytes([pdu[5], pdu[6]]) as usize;
    let rest = &pdu[7..];
    if rest.len() < count + 1 || rest.len() < count + 1 + rest[count] as usize {
        return Err("short SDP response");
    }
    lists.extend_from_slice(&rest[..count]);
    let n = rest[count] as usize;
    Ok(rest[count + 1..count + 1 + n].to_vec())
}

/// A data element: (type, payload, total length).
fn element(d: &[u8]) -> Option<(u8, &[u8], usize)> {
    let b = *d.first()?;
    let (kind, size) = (b >> 3, b & 7);
    let (header, len) = match size {
        0..=4 if kind == 0 => (1, 0),
        0..=4 => (1, 1usize << size),
        5 => (2, *d.get(1)? as usize),
        6 => (3, u16::from_be_bytes([*d.get(1)?, *d.get(2)?]) as usize),
        _ => (5, u32::from_be_bytes([*d.get(1)?, *d.get(2)?, *d.get(3)?, *d.get(4)?]) as usize),
    };
    let body = d.get(header..header + len)?;
    Some((kind, body, header + len))
}

/// Finds the first report descriptor (class 0x22) in the attribute lists.
pub fn report_descriptor(lists: &[u8]) -> Option<Vec<u8>> {
    // Outer sequence (one per matching record) -> attribute id/value pairs.
    let (_, records, _) = element(lists)?;
    let mut r = records;
    while let Some((kind, attrs, used)) = element(r) {
        r = &r[used..];
        if kind != 6 {
            continue;
        }
        let mut a = attrs;
        while let Some((_, id, used)) = element(a) {
            a = &a[used..];
            let (_, value, used_v) = element(a)?;
            a = &a[used_v..];
            if id.len() != 2 || u16::from_be_bytes([id[0], id[1]]) != HID_DESCRIPTOR_LIST {
                continue;
            }
            // value: sequence of (uint8 class, string descriptor) sequences.
            let mut v = value;
            while let Some((_, pair, used)) = element(v) {
                v = &v[used..];
                let (_, class, used_c) = element(pair)?;
                let (kind, desc, _) = element(&pair[used_c..])?;
                if class == [0x22] && kind == 4 {
                    return Some(desc.to_vec());
                }
            }
        }
    }
    None
}
