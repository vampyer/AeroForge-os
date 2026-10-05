//! SDP, both ways. As a client: ServiceSearchAttribute requests to read a
//! HID device's report descriptor or a headset's RFCOMM channel, following
//! continuations. As a server: the one record a headset looks for, the
//! hands-free audio gateway on RFCOMM channel 1.

use alloc::vec::Vec;

pub const PSM: u16 = 0x0001;
const ERROR_RESPONSE: u8 = 0x01;
const SEARCH_REQUEST: u8 = 0x02;
const SEARCH_RESPONSE: u8 = 0x03;
const ATTRIBUTE_REQUEST: u8 = 0x04;
const ATTRIBUTE_RESPONSE: u8 = 0x05;
const SEARCH_ATTRIBUTE_REQUEST: u8 = 0x06;
const SEARCH_ATTRIBUTE_RESPONSE: u8 = 0x07;

pub const HID_SERVICE: u16 = 0x1124;
pub const HANDSFREE: u16 = 0x111E;
pub const HANDSFREE_AG: u16 = 0x111F;
pub const HEADSET: u16 = 0x1108;
pub const HEADSET_HS: u16 = 0x1131;
pub const HEADSET_AG: u16 = 0x1112;
const GENERIC_AUDIO: u16 = 0x1203;
const L2CAP_UUID: u16 = 0x0100;
const RFCOMM_UUID: u16 = 0x0003;

pub const PROTOCOL_DESCRIPTORS: u16 = 0x0004;
pub const HID_DESCRIPTOR_LIST: u16 = 0x0206;

/// Our audio gateway's RFCOMM server channel.
pub const AG_CHANNEL: u8 = 1;
const AG_RECORD_HANDLE: u32 = 0x0001_0001;

/// The request PDU: records with `uuid`, attribute `attr` only;
/// `continuation` is the state from the previous response.
pub fn request(transaction: u16, uuid: u16, attr: u16, continuation: &[u8]) -> Vec<u8> {
    let mut params = alloc::vec![
        0x35, 3, 0x19, (uuid >> 8) as u8, uuid as u8, // search pattern: UUID16
        0x02, 0x00,                                    // max attribute bytes: 512
        0x35, 3, 0x09, (attr >> 8) as u8, attr as u8,
    ];
    params.push(continuation.len() as u8);
    params.extend_from_slice(continuation);
    pdu(SEARCH_ATTRIBUTE_REQUEST, transaction, &params)
}

fn pdu(id: u8, transaction: u16, params: &[u8]) -> Vec<u8> {
    let mut p = alloc::vec![id];
    p.extend_from_slice(&transaction.to_be_bytes());
    p.extend_from_slice(&(params.len() as u16).to_be_bytes());
    p.extend_from_slice(params);
    p
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

/// The value of attribute `attr` in the first record that has it.
fn attribute(lists: &[u8], attr: u16) -> Option<&[u8]> {
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
            let (_, _, used_v) = element(a)?;
            let value = &a[..used_v];
            a = &a[used_v..];
            if id.len() == 2 && u16::from_be_bytes([id[0], id[1]]) == attr {
                return Some(value);
            }
        }
    }
    None
}

/// The first report descriptor (class 0x22) in a HIDDescriptorList.
pub fn report_descriptor(lists: &[u8]) -> Option<Vec<u8>> {
    let (_, mut v, _) = element(attribute(lists, HID_DESCRIPTOR_LIST)?)?;
    // A sequence of (uint8 class, string descriptor) sequences.
    while let Some((_, pair, used)) = element(v) {
        v = &v[used..];
        let (_, class, used_c) = element(pair)?;
        let (kind, desc, _) = element(&pair[used_c..])?;
        if class == [0x22] && kind == 4 {
            return Some(desc.to_vec());
        }
    }
    None
}

/// The RFCOMM server channel in a ProtocolDescriptorList.
pub fn rfcomm_channel(lists: &[u8]) -> Option<u8> {
    let (_, mut v, _) = element(attribute(lists, PROTOCOL_DESCRIPTORS)?)?;
    while let Some((_, proto, used)) = element(v) {
        v = &v[used..];
        let (kind, uuid, used_u) = element(proto)?;
        if kind == 3 && uuid == RFCOMM_UUID.to_be_bytes() {
            let (_, ch, _) = element(&proto[used_u..])?;
            return ch.first().copied();
        }
    }
    None
}

// ------------------------------------------------------------- server

fn uuid16(out: &mut Vec<u8>, u: u16) {
    out.extend_from_slice(&[0x19, (u >> 8) as u8, u as u8]);
}

fn seq(out: &mut Vec<u8>, body: &[u8]) {
    out.extend_from_slice(&[0x35, body.len() as u8]);
    out.extend_from_slice(body);
}

/// Our hands-free audio gateway record: (attribute id, encoded value).
fn ag_record() -> Vec<(u16, Vec<u8>)> {
    let mut handle = alloc::vec![0x0A];
    handle.extend_from_slice(&AG_RECORD_HANDLE.to_be_bytes());
    let mut classes = Vec::new();
    uuid16(&mut classes, HANDSFREE_AG);
    uuid16(&mut classes, GENERIC_AUDIO);
    let mut l2cap = Vec::new();
    uuid16(&mut l2cap, L2CAP_UUID);
    let mut rfcomm = Vec::new();
    uuid16(&mut rfcomm, RFCOMM_UUID);
    rfcomm.extend_from_slice(&[0x08, AG_CHANNEL]);
    let mut protocols = Vec::new();
    seq(&mut protocols, &l2cap);
    seq(&mut protocols, &rfcomm);
    let mut profile = Vec::new();
    uuid16(&mut profile, HANDSFREE);
    profile.extend_from_slice(&[0x09, 0x01, 0x07]); // version 1.7
    let mut profiles = Vec::new();
    seq(&mut profiles, &profile);
    let name = b"Hands-Free Audio Gateway";
    let mut name_el = alloc::vec![0x25, name.len() as u8];
    name_el.extend_from_slice(name);
    let wrap = |b: &[u8]| {
        let mut v = Vec::new();
        seq(&mut v, b);
        v
    };
    alloc::vec![
        (0x0000, handle),
        (0x0001, wrap(&classes)),
        (0x0004, wrap(&protocols)),
        (0x0009, wrap(&profiles)),
        (0x0100, name_el),
        (0x0301, alloc::vec![0x08, 0x01]),       // network: can reject calls
        (0x0311, alloc::vec![0x09, 0x00, 0x00]), // supported features: none
    ]
}

/// UUIDs in a search pattern.
fn pattern_uuids(d: &[u8]) -> Option<(Vec<u16>, usize)> {
    let (_, mut body, used) = element(d)?;
    let mut out = Vec::new();
    while let Some((kind, u, n)) = element(body) {
        body = &body[n..];
        if kind == 3 && u.len() == 2 {
            out.push(u16::from_be_bytes([u[0], u[1]]));
        } else if kind == 3 && u.len() == 4 {
            out.push(u16::from_be_bytes([u[2], u[3]]));
        } else if kind == 3 && u.len() == 16 {
            out.push(u16::from_be_bytes([u[2], u[3]]));
        }
    }
    Some((out, used))
}

/// Attribute ID list: single IDs and ranges.
fn wanted(d: &[u8]) -> Option<(Vec<(u16, u16)>, usize)> {
    let (_, mut body, used) = element(d)?;
    let mut out = Vec::new();
    while let Some((_, v, n)) = element(body) {
        body = &body[n..];
        match v.len() {
            2 => {
                let id = u16::from_be_bytes([v[0], v[1]]);
                out.push((id, id));
            }
            4 => out.push((u16::from_be_bytes([v[0], v[1]]), u16::from_be_bytes([v[2], v[3]]))),
            _ => {}
        }
    }
    Some((out, used))
}

fn matches(uuids: &[u16]) -> bool {
    const OURS: [u16; 5] = [HANDSFREE_AG, GENERIC_AUDIO, L2CAP_UUID, RFCOMM_UUID, HANDSFREE];
    !uuids.is_empty() && uuids.iter().all(|u| OURS.contains(u))
}

fn attribute_list(ranges: &[(u16, u16)]) -> Vec<u8> {
    let mut body = Vec::new();
    for (id, value) in ag_record() {
        if ranges.iter().any(|&(lo, hi)| id >= lo && id <= hi) {
            body.extend_from_slice(&[0x09, (id >> 8) as u8, id as u8]);
            body.extend_from_slice(&value);
        }
    }
    let mut out = Vec::new();
    seq(&mut out, &body);
    out
}

/// Answers one request from a remote device. Our answers are small, so
/// they never need a continuation.
pub fn serve(req: &[u8]) -> Vec<u8> {
    let error = |t: u16, code: u16| pdu(ERROR_RESPONSE, t, &code.to_be_bytes());
    if req.len() < 5 {
        return error(0, 0x0003);
    }
    let t = u16::from_be_bytes([req[1], req[2]]);
    let p = &req[5..];
    match req[0] {
        SEARCH_REQUEST => {
            let Some((uuids, _)) = pattern_uuids(p) else { return error(t, 0x0003) };
            let mut r = Vec::new();
            if matches(&uuids) {
                r.extend_from_slice(&[0, 1, 0, 1]);
                r.extend_from_slice(&AG_RECORD_HANDLE.to_be_bytes());
            } else {
                r.extend_from_slice(&[0, 0, 0, 0]);
            }
            r.push(0);
            pdu(SEARCH_RESPONSE, t, &r)
        }
        ATTRIBUTE_REQUEST if p.len() >= 6 => {
            let handle = u32::from_be_bytes([p[0], p[1], p[2], p[3]]);
            if handle != AG_RECORD_HANDLE {
                return error(t, 0x0002);
            }
            let Some((ranges, _)) = wanted(&p[6..]) else { return error(t, 0x0003) };
            let list = attribute_list(&ranges);
            let mut r = (list.len() as u16).to_be_bytes().to_vec();
            r.extend_from_slice(&list);
            r.push(0);
            pdu(ATTRIBUTE_RESPONSE, t, &r)
        }
        SEARCH_ATTRIBUTE_REQUEST => {
            let Some((uuids, used)) = pattern_uuids(p) else { return error(t, 0x0003) };
            let Some((ranges, _)) = p.get(used + 2..).and_then(wanted) else { return error(t, 0x0003) };
            let mut records = Vec::new();
            if matches(&uuids) {
                records.extend_from_slice(&attribute_list(&ranges));
            }
            let mut lists = Vec::new();
            seq(&mut lists, &records);
            let mut r = (lists.len() as u16).to_be_bytes().to_vec();
            r.extend_from_slice(&lists);
            r.push(0);
            pdu(SEARCH_ATTRIBUTE_RESPONSE, t, &r)
        }
        _ => error(t, 0x0003),
    }
}
