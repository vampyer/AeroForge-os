//! NTFS: the filesystem Windows keeps its drives in.
//!
//! Everything on an NTFS volume is a file described by a record in the
//! Master File Table (MFT), the MFT included. A record holds attributes:
//! names, data (inside the record when small, else as a list of cluster
//! runs), and for folders a B-tree index of the names in them. Big or
//! scattered files can spill attributes into more records, listed by an
//! attribute list. Compressed and encrypted files are refused for now.
//!
//! Writing is off until asked for, drive by drive (`set_writable`), and is
//! refused unless Windows left the volume clean: not marked dirty, its
//! journal ($LogFile) with nothing left to replay, and not hibernated
//! (Fast Startup). We do not keep the journal ourselves; instead, as
//! ntfs-3g does, the journal is emptied before the first write, and every
//! change runs between setting and clearing the volume's dirty flag: if
//! AeroForge stops in the middle, Windows sees the flag and runs chkdsk.
//! Within a change the order is: take space (bitmaps), write data, write
//! the file's record, link it into its folder, and only then free what is
//! no longer used, so a crash can leak space but never hand it out twice.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::cmp::Ordering;
use core::sync::atomic::{AtomicBool, Ordering as Atomic};

use crate::block::BlockDevice;
use crate::sync::SleepMutex;
use crate::vfs;

type Res<T> = Result<T, &'static str>;

const ATTR_STANDARD_INFORMATION: u32 = 0x10;
const ATTR_LIST: u32 = 0x20;
const ATTR_FILE_NAME: u32 = 0x30;
const ATTR_VOLUME_NAME: u32 = 0x60;
const ATTR_VOLUME_INFORMATION: u32 = 0x70;
const ATTR_DATA: u32 = 0x80;
const ATTR_INDEX_ROOT: u32 = 0x90;
const ATTR_INDEX_ALLOCATION: u32 = 0xA0;
const ATTR_BITMAP: u32 = 0xB0;
const ATTR_END: u32 = 0xFFFF_FFFF;

const RECORD_MFT: u64 = 0;
const RECORD_LOGFILE: u64 = 2;
const RECORD_VOLUME: u64 = 3;
const RECORD_ROOT: u64 = 5;
const RECORD_BITMAP: u64 = 6;
const RECORD_UPCASE: u64 = 10;
/// Records below this are the volume's own; new files get records from here.
const FIRST_USER_RECORD: u64 = 24;

const FLAG_COMPRESSED: u16 = 0x0001;
const FLAG_ENCRYPTED: u16 = 0x4000;
const FLAG_SPARSE: u16 = 0x8000;

/// $VOLUME_INFORMATION flags: chkdsk has to run.
const VOLUME_DIRTY: u16 = 0x0001;
/// File attributes ($STANDARD_INFORMATION, $FILE_NAME).
const FILE_READ_ONLY: u32 = 0x0001;
const FILE_ARCHIVE: u32 = 0x0020;
const FILE_REPARSE_POINT: u32 = 0x0400;
const FILE_NAME_DIRECTORY: u32 = 0x1000_0000;
/// File name namespaces: Win32, and Win32 and DOS at once (an 8.3 name).
const NAMESPACE_WIN32: u8 = 1;
const NAMESPACE_DOS: u8 = 2;
const NAMESPACE_BOTH: u8 = 3;

/// Index entry flags: has a child block, is the last (nameless) entry.
const ENTRY_CHILD: u16 = 1;
const ENTRY_LAST: u16 = 2;

/// Update sequences protect every 512 bytes, whatever the sector size.
const USA_STRIDE: usize = 512;

const NOT_ALLOWED: &str = "writing to this NTFS drive is not turned on";

/// A piece of an attribute's data: `len` clusters from virtual cluster `vcn`,
/// on the disk at `lcn`, or a hole (reads as zeros) when `lcn` is None.
#[derive(Clone, Copy, Debug)]
struct Run {
    vcn: u64,
    lcn: Option<u64>,
    len: u64,
}

#[derive(Clone, Debug)]
struct Attr {
    kind: u32,
    name: Vec<u16>,
    flags: u16,
    /// Resident value, or None when the data lives in `runs`.
    value: Option<Vec<u8>>,
    runs: Vec<Run>,
    start_vcn: u64,
    size: u64,
    initialized: u64,
}

impl Attr {
    fn size(&self) -> u64 {
        self.value.as_ref().map_or(self.size, |v| v.len() as u64)
    }
}

struct Name {
    record: u64,
    name: String,
}

pub struct NtfsVolume {
    dev: Arc<dyn BlockDevice>,
    bytes_per_sector: u64,
    cluster: u64,
    /// Clusters on the volume.
    clusters: u64,
    record_size: usize,
    index_block: usize,
    /// Where the MFT is (it grows when new files need records).
    mft: spin::Mutex<Vec<Run>>,
    /// Where $MFTMirr keeps copies of the first records, and how many.
    mirror_lcn: u64,
    mirror_records: u64,
    label: String,
    upcase: Vec<u16>,
    /// Writing was turned on (and Windows had left the volume clean).
    writable: AtomicBool,
    /// Held for the whole of every change.
    state: SleepMutex<State>,
}

struct State {
    /// Where to look for free clusters next.
    next_lcn: u64,
    /// The journal was emptied (done once, before the first change).
    log_reset: bool,
}

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn u64_at(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

/// True if the boot sector is a BitLocker-encrypted volume's.
pub fn is_bitlocker(boot: &[u8]) -> bool {
    boot.len() >= 11 && &boot[3..11] == b"-FVE-FS-"
}

impl NtfsVolume {
    pub fn mount(dev: Arc<dyn BlockDevice>) -> Res<Self> {
        let mut bs = vec![0u8; dev.block_size() as usize];
        dev.read(0, &mut bs)?;
        if is_bitlocker(&bs) {
            return Err("BitLocker-encrypted volume");
        }
        if &bs[3..11] != b"NTFS    " || bs[510] != 0x55 || bs[511] != 0xAA {
            return Err("not an NTFS volume");
        }
        let bytes_per_sector = u16_at(&bs, 11) as u64;
        if bytes_per_sector != dev.block_size() as u64 {
            return Err("sector size differs from the device block size");
        }
        let spc = bs[13];
        let sectors_per_cluster = if spc > 0x80 { 1u64 << (256 - spc as u32).min(20) } else { spc as u64 };
        let cluster = bytes_per_sector * sectors_per_cluster;
        // Record and index block sizes: clusters if positive, else 2^-n bytes.
        let size_of = |v: u8| -> usize {
            let v = v as i8;
            if v > 0 { v as usize * cluster as usize } else { 1usize << (-(v as i32)).clamp(9, 16) }
        };
        let record_size = size_of(bs[64]);
        let index_block = size_of(bs[68]);
        if cluster == 0 || cluster > 2 * 1024 * 1024 || record_size < 512 || record_size > 64 * 1024 {
            return Err("bad NTFS boot sector");
        }
        let mft_lcn = u64_at(&bs, 48);
        let clusters = u64_at(&bs, 40) / sectors_per_cluster;
        let mut vol = Self {
            dev,
            bytes_per_sector,
            cluster,
            clusters,
            record_size,
            index_block,
            // Enough to read record 0, which then describes the whole MFT.
            mft: spin::Mutex::new(vec![Run { vcn: 0, lcn: Some(mft_lcn), len: (record_size as u64).div_ceil(cluster).max(1) * 16 }]),
            mirror_lcn: u64_at(&bs, 56),
            mirror_records: if cluster <= 4 * record_size as u64 { 4 } else { cluster / record_size as u64 },
            label: String::new(),
            upcase: Vec::new(),
            writable: AtomicBool::new(false),
            // New data goes after the zone Windows keeps free for the MFT to grow into.
            state: SleepMutex::new(State { next_lcn: (mft_lcn + clusters / 8).min(clusters.saturating_sub(1)), log_reset: false }),
        };
        let mft_data = vol.data_attr(&vol.attributes(0)?, &[]).ok_or("$MFT has no data")?;
        vol.mft = spin::Mutex::new(mft_data.runs);
        // The case table Windows compares names with, and the volume label.
        vol.upcase = (0..=0xFFFFu32).map(|c| c as u16).collect();
        if let Ok(attrs) = vol.attributes(RECORD_UPCASE) {
            if let Some(a) = vol.data_attr(&attrs, &[]) {
                let table = vol.read_attr(&a, 0x20000)?;
                for (i, w) in table.chunks_exact(2).enumerate() {
                    vol.upcase[i] = u16::from_le_bytes([w[0], w[1]]);
                }
            }
        }
        if let Ok(attrs) = vol.attributes(RECORD_VOLUME) {
            if let Some(v) = attrs.iter().find(|a| a.kind == ATTR_VOLUME_NAME).and_then(|a| a.value.as_ref()) {
                let units: Vec<u16> = v.chunks_exact(2).map(|w| u16::from_le_bytes([w[0], w[1]])).collect();
                vol.label = char::decode_utf16(units).map(|c| c.unwrap_or('?')).collect();
            }
        }
        // The root folder must be readable.
        vol.dir_names(RECORD_ROOT)?;
        Ok(vol)
    }

    // ------------------------------------------------------ data streams

    /// Reads `len` bytes from offset `start` of the data the runs describe.
    fn read_runs(&self, runs: &[Run], start: u64, len: usize) -> Res<Vec<u8>> {
        let mut out = vec![0u8; len];
        let end = start + len as u64;
        for r in runs {
            let r_start = r.vcn * self.cluster;
            let r_end = r_start + r.len * self.cluster;
            let from = start.max(r_start);
            let to = end.min(r_end);
            if from >= to {
                continue;
            }
            let Some(lcn) = r.lcn else { continue }; // a hole: zeros
            // Whole sectors around [from, to) of this run.
            let disk = lcn * self.cluster + (from - r_start);
            let first = disk / self.bytes_per_sector;
            let skip = (disk % self.bytes_per_sector) as usize;
            let bytes = (skip as u64 + (to - from)).div_ceil(self.bytes_per_sector) * self.bytes_per_sector;
            let mut buf = vec![0u8; bytes as usize];
            self.dev.read(first, &mut buf)?;
            let at = (from - start) as usize;
            out[at..at + (to - from) as usize].copy_from_slice(&buf[skip..skip + (to - from) as usize]);
        }
        Ok(out)
    }

    /// Undoes the update sequence: the last two bytes of every sector were
    /// swapped for a check value, so a torn write shows up.
    fn fixup(&self, buf: &mut [u8]) -> Res<()> {
        let off = u16_at(buf, 4) as usize;
        let count = u16_at(buf, 6) as usize;
        let stride = USA_STRIDE;
        if count == 0 || off + 2 * count > buf.len() || (count - 1) * stride > buf.len() {
            return Err("bad NTFS update sequence");
        }
        let check = [buf[off], buf[off + 1]];
        for i in 1..count {
            let pos = i * stride - 2;
            if buf[pos..pos + 2] != check {
                return Err("torn NTFS record");
            }
            buf[pos] = buf[off + 2 * i];
            buf[pos + 1] = buf[off + 2 * i + 1];
        }
        Ok(())
    }

    fn record(&self, n: u64) -> Res<Vec<u8>> {
        let mut rec = self.read_runs(&self.mft.lock().clone(), n * self.record_size as u64, self.record_size)?;
        if &rec[0..4] != b"FILE" {
            return Err("bad MFT record");
        }
        self.fixup(&mut rec)?;
        if u16_at(&rec, 22) & 1 == 0 {
            return Err("file not found"); // record not in use
        }
        Ok(rec)
    }

    fn parse_attrs(rec: &[u8]) -> Res<Vec<Attr>> {
        let mut out = Vec::new();
        let mut p = u16_at(rec, 20) as usize;
        while p + 16 <= rec.len() {
            let kind = u32_at(rec, p);
            if kind == ATTR_END {
                break;
            }
            let len = u32_at(rec, p + 4) as usize;
            if len < 16 || p + len > rec.len() {
                return Err("bad NTFS attribute");
            }
            let a = &rec[p..p + len];
            let name_len = a[9] as usize;
            let name_off = u16_at(a, 10) as usize;
            if name_off + 2 * name_len > len {
                return Err("bad NTFS attribute");
            }
            let name = (0..name_len).map(|i| u16_at(a, name_off + 2 * i)).collect();
            let flags = u16_at(a, 12);
            let attr = if a[8] == 0 {
                let vlen = u32_at(a, 16) as usize;
                let voff = u16_at(a, 20) as usize;
                if voff + vlen > len {
                    return Err("bad NTFS attribute");
                }
                Attr { kind, name, flags, value: Some(a[voff..voff + vlen].to_vec()), runs: Vec::new(), start_vcn: 0, size: vlen as u64, initialized: vlen as u64 }
            } else {
                if len < 64 {
                    return Err("bad NTFS attribute");
                }
                let start_vcn = u64_at(a, 16);
                Attr {
                    kind,
                    name,
                    flags,
                    value: None,
                    runs: decode_runs(&a[u16_at(a, 32) as usize..], start_vcn)?,
                    start_vcn,
                    size: u64_at(a, 48),
                    initialized: u64_at(a, 56),
                }
            };
            out.push(attr);
            p += len;
        }
        Ok(out)
    }

    /// A file's attributes, gathered from every record its attribute list
    /// names, with the pieces of split attributes joined.
    fn attributes(&self, n: u64) -> Res<Vec<Attr>> {
        let mut attrs = Self::parse_attrs(&self.record(n)?)?;
        let Some(list) = attrs.iter().find(|a| a.kind == ATTR_LIST).cloned() else { return Ok(attrs) };
        let list = self.read_attr(&list, 1 << 20)?;
        let mut others: Vec<u64> = Vec::new();
        let mut p = 0;
        while p + 26 <= list.len() {
            let len = u16_at(&list, p + 4) as usize;
            let rec = u64_at(&list, p + 16) & 0xFFFF_FFFF_FFFF;
            if rec != n && !others.contains(&rec) {
                others.push(rec);
            }
            if len == 0 {
                break;
            }
            p += len;
        }
        for r in others {
            attrs.extend(Self::parse_attrs(&self.record(r)?)?);
        }
        // Join non-resident pieces of the same attribute, in VCN order.
        attrs.retain(|a| a.kind != ATTR_LIST);
        attrs.sort_by_key(|a| (a.kind, a.name.clone(), a.start_vcn));
        let mut joined: Vec<Attr> = Vec::new();
        for a in attrs {
            match joined.last_mut() {
                Some(prev) if prev.kind == a.kind && prev.name == a.name && prev.value.is_none() && a.value.is_none() && a.start_vcn > 0 => {
                    prev.runs.extend(a.runs);
                }
                _ => joined.push(a),
            }
        }
        Ok(joined)
    }

    fn data_attr(&self, attrs: &[Attr], name: &[u16]) -> Option<Attr> {
        attrs.iter().find(|a| a.kind == ATTR_DATA && a.name == name).cloned()
    }

    fn read_attr(&self, a: &Attr, limit: usize) -> Res<Vec<u8>> {
        if let Some(v) = &a.value {
            return Ok(v[..v.len().min(limit)].to_vec());
        }
        if a.flags & FLAG_COMPRESSED != 0 {
            return Err("compressed NTFS files are not supported yet");
        }
        if a.flags & FLAG_ENCRYPTED != 0 {
            return Err("encrypted NTFS files cannot be read");
        }
        // Holes in sparse files are runs without a location: they read as zeros.
        let want = (a.size.min(limit as u64)) as usize;
        let valid = (a.initialized.min(want as u64)) as usize;
        let mut out = self.read_runs(&a.runs, 0, valid)?;
        out.resize(want, 0); // past the initialized size reads as zeros
        Ok(out)
    }

    // ----------------------------------------------------------- folders

    /// The names in folder `n`, walking its index B-tree.
    fn dir_names(&self, n: u64) -> Res<Vec<Name>> {
        let attrs = self.attributes(n)?;
        let i30: Vec<u16> = "$I30".encode_utf16().collect();
        let root = attrs.iter().find(|a| a.kind == ATTR_INDEX_ROOT && a.name == i30).ok_or("not a directory")?;
        let root = root.value.as_ref().ok_or("bad NTFS index")?;
        if root.len() < 32 {
            return Err("bad NTFS index");
        }
        let mut names = Vec::new();
        let mut pending = Vec::new();
        let start = 16 + u32_at(root, 16) as usize;
        let end = (16 + u32_at(root, 20) as usize).min(root.len());
        parse_index(root, start, end, &mut names, &mut pending)?;

        if !pending.is_empty() {
            let alloc = attrs.iter().find(|a| a.kind == ATTR_INDEX_ALLOCATION && a.name == i30).ok_or("bad NTFS index")?;
            // Index VCNs count clusters, or sectors when blocks are smaller than a cluster.
            let unit = if self.index_block as u64 >= self.cluster { self.cluster } else { self.bytes_per_sector };
            let mut seen = Vec::new();
            while let Some(vcn) = pending.pop() {
                if seen.contains(&vcn) || seen.len() > 1_000_000 {
                    continue;
                }
                seen.push(vcn);
                let mut block = self.read_runs(&alloc.runs, vcn * unit, self.index_block)?;
                if &block[0..4] != b"INDX" {
                    return Err("bad NTFS index block");
                }
                self.fixup(&mut block)?;
                let start = 0x18 + u32_at(&block, 0x18) as usize;
                let end = (0x18 + u32_at(&block, 0x1C) as usize).min(block.len());
                parse_index(&block, start, end, &mut names, &mut pending)?;
            }
        }
        Ok(names)
    }

    fn same_name(&self, a: &str, b: &str) -> bool {
        let up = |s: &str| -> Vec<u16> { s.encode_utf16().map(|c| self.upcase[c as usize]).collect() };
        up(a) == up(b)
    }

    /// The record number `path` names.
    fn lookup(&self, path: &str) -> Res<u64> {
        let mut cur = RECORD_ROOT;
        for part in path.split('/').filter(|p| !p.is_empty()) {
            cur = self.dir_names(cur)?.into_iter().find(|e| self.same_name(&e.name, part)).ok_or("file not found")?.record;
        }
        Ok(cur)
    }

    fn is_dir(&self, n: u64) -> Res<bool> {
        Ok(u16_at(&self.record(n)?, 22) & 2 != 0)
    }
}

/// Decodes a mapping-pairs array: per run a header byte (low nibble: bytes
/// of length, high nibble: bytes of signed LCN change; no LCN bytes = hole).
fn decode_runs(b: &[u8], start_vcn: u64) -> Res<Vec<Run>> {
    let mut runs = Vec::new();
    let mut vcn = start_vcn;
    let mut lcn: i64 = 0;
    let mut p = 0;
    while p < b.len() && b[p] != 0 {
        let lb = (b[p] & 0x0F) as usize;
        let ob = (b[p] >> 4) as usize;
        if lb == 0 || lb > 8 || ob > 8 || p + 1 + lb + ob > b.len() {
            return Err("bad NTFS run list");
        }
        let mut len = 0u64;
        for i in 0..lb {
            len |= (b[p + 1 + i] as u64) << (8 * i);
        }
        let run_lcn = if ob == 0 {
            None
        } else {
            let mut delta = 0i64;
            for i in 0..ob {
                delta |= (b[p + 1 + lb + i] as i64) << (8 * i);
            }
            let shift = 64 - 8 * ob as u32;
            delta = (delta << shift) >> shift; // sign-extend
            lcn += delta;
            if lcn < 0 {
                return Err("bad NTFS run list");
            }
            Some(lcn as u64)
        };
        runs.push(Run { vcn, lcn: run_lcn, len });
        vcn += len;
        p += 1 + lb + ob;
    }
    Ok(runs)
}

/// Index entries between `start` and `end`: names found and child blocks to visit.
fn parse_index(buf: &[u8], mut p: usize, end: usize, names: &mut Vec<Name>, pending: &mut Vec<u64>) -> Res<()> {
    while p + 16 <= end {
        let len = u16_at(buf, p + 8) as usize;
        let key_len = u16_at(buf, p + 10) as usize;
        let flags = u16_at(buf, p + 12);
        if len < 16 || p + len > buf.len() {
            return Err("bad NTFS index entry");
        }
        if flags & 1 != 0 {
            pending.push(u64_at(buf, p + len - 8));
        }
        if flags & 2 != 0 {
            break; // the last entry carries no name
        }
        if key_len >= 66 && p + 16 + key_len <= buf.len() {
            let key = &buf[p + 16..p + 16 + key_len];
            let name_len = key[64] as usize;
            let namespace = key[65];
            if namespace != 2 && 66 + 2 * name_len <= key.len() {
                // Namespace 2 is a DOS 8.3 alias of a name listed anyway.
                let units: Vec<u16> = (0..name_len).map(|i| u16_at(key, 66 + 2 * i)).collect();
                names.push(Name {
                    record: u64_at(buf, p) & 0xFFFF_FFFF_FFFF,
                    name: char::decode_utf16(units).map(|c| c.unwrap_or('?')).collect(),
                });
            }
        }
        p += len;
    }
    Ok(())
}

// ============================================================== writing

fn put16(b: &mut [u8], o: usize, v: u16) {
    b[o..o + 2].copy_from_slice(&v.to_le_bytes());
}
fn put32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}
fn put64(b: &mut [u8], o: usize, v: u64) {
    b[o..o + 8].copy_from_slice(&v.to_le_bytes());
}
fn align8(n: usize) -> usize {
    (n + 7) & !7
}

/// Where attribute `kind` named `name` starts in a record, if it is there.
fn find_attr(rec: &[u8], kind: u32, name: &[u16]) -> Option<usize> {
    let mut p = u16_at(rec, 20) as usize;
    while p + 16 <= rec.len() {
        let k = u32_at(rec, p);
        if k == ATTR_END {
            return None;
        }
        let len = u32_at(rec, p + 4) as usize;
        if len < 16 || p + len > rec.len() {
            return None;
        }
        let (nl, no) = (rec[p + 9] as usize, u16_at(rec, p + 10) as usize);
        if k == kind && nl == name.len() && p + no + 2 * nl <= rec.len() && (0..nl).all(|i| u16_at(rec, p + no + 2 * i) == name[i]) {
            return Some(p);
        }
        p += len;
    }
    None
}

/// Every attribute of `kind` in a record: (offset, length).
fn attrs_of(rec: &[u8], kind: u32) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut p = u16_at(rec, 20) as usize;
    while p + 16 <= rec.len() && u32_at(rec, p) != ATTR_END {
        let len = u32_at(rec, p + 4) as usize;
        if len < 16 || p + len > rec.len() {
            break;
        }
        if u32_at(rec, p) == kind {
            out.push((p, len));
        }
        p += len;
    }
    out
}

/// A resident attribute's value.
fn resident_value(rec: &[u8], p: usize) -> Option<&[u8]> {
    if rec[p + 8] != 0 {
        return None;
    }
    let (len, off) = (u32_at(rec, p + 16) as usize, u16_at(rec, p + 20) as usize);
    rec.get(p + off..p + off + len)
}

/// Replaces `old_len` bytes at `at` in a record with `new`, moving what
/// follows; refuses when the record would overflow.
fn splice(rec: &mut [u8], at: usize, old_len: usize, new: &[u8]) -> Res<()> {
    let used = u32_at(rec, 24) as usize;
    let size = (u32_at(rec, 28) as usize).min(rec.len());
    let new_used = used - old_len + new.len();
    if new_used > size || at + old_len > used {
        return Err("no room left in the file's record");
    }
    rec.copy_within(at + old_len..used, at + new.len());
    rec[at..at + new.len()].copy_from_slice(new);
    if new_used < used {
        rec[new_used..used].fill(0);
    }
    put32(rec, 24, new_used as u32);
    Ok(())
}

/// Adds an attribute (built with id 0) in type order, giving it the next id.
fn add_attr(rec: &mut [u8], mut attr: Vec<u8>) -> Res<()> {
    let id = u16_at(rec, 40);
    put16(&mut attr, 14, id);
    let kind = u32_at(&attr, 0);
    let mut p = u16_at(rec, 20) as usize;
    while p + 16 <= rec.len() && u32_at(rec, p) != ATTR_END && u32_at(rec, p) <= kind {
        p += u32_at(rec, p + 4) as usize;
    }
    splice(rec, p, 0, &attr)?;
    put16(rec, 40, id.wrapping_add(1));
    Ok(())
}

/// Puts `attr` (built with id 0) in place of the attribute at `p`, keeping its id.
fn replace_attr(rec: &mut [u8], p: usize, mut attr: Vec<u8>) -> Res<()> {
    put16(&mut attr, 14, u16_at(rec, p + 14));
    let old = u32_at(rec, p + 4) as usize;
    splice(rec, p, old, &attr)
}

fn resident_attr(kind: u32, name: &[u16], value: &[u8], indexed: bool) -> Vec<u8> {
    let value_off = align8(0x18 + 2 * name.len());
    let len = align8(value_off + value.len());
    let mut a = vec![0u8; len];
    put32(&mut a, 0, kind);
    put32(&mut a, 4, len as u32);
    a[9] = name.len() as u8;
    put16(&mut a, 10, 0x18);
    put32(&mut a, 16, value.len() as u32);
    put16(&mut a, 20, value_off as u16);
    a[22] = indexed as u8;
    for (i, &c) in name.iter().enumerate() {
        put16(&mut a, 0x18 + 2 * i, c);
    }
    a[value_off..value_off + value.len()].copy_from_slice(value);
    a
}

fn nonresident_attr(kind: u32, name: &[u16], runs: &[Run], allocated: u64, size: u64) -> Vec<u8> {
    let runs_off = align8(0x40 + 2 * name.len());
    let pairs = encode_runs(runs);
    let len = align8(runs_off + pairs.len());
    let clusters: u64 = runs.iter().map(|r| r.len).sum();
    let mut a = vec![0u8; len];
    put32(&mut a, 0, kind);
    put32(&mut a, 4, len as u32);
    a[8] = 1;
    a[9] = name.len() as u8;
    put16(&mut a, 10, 0x40);
    put64(&mut a, 0x18, clusters.wrapping_sub(1));
    put16(&mut a, 0x20, runs_off as u16);
    put64(&mut a, 0x28, allocated);
    put64(&mut a, 0x30, size);
    put64(&mut a, 0x38, size);
    for (i, &c) in name.iter().enumerate() {
        put16(&mut a, 0x40 + 2 * i, c);
    }
    a[runs_off..runs_off + pairs.len()].copy_from_slice(&pairs);
    a
}

/// The fewest bytes that hold `v` as a signed number.
fn signed_bytes(v: i64) -> usize {
    (1..=8).find(|&n| {
        let shift = 64 - 8 * n as u32;
        (v << shift) >> shift == v
    })
    .unwrap_or(8)
}

/// The mapping-pairs array for runs (the inverse of `decode_runs`).
fn encode_runs(runs: &[Run]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut prev: i64 = 0;
    for r in runs {
        let lcn = r.lcn.unwrap_or(0) as i64;
        let (lb, ob) = (signed_bytes(r.len as i64), signed_bytes(lcn - prev));
        out.push((ob << 4 | lb) as u8);
        out.extend_from_slice(&r.len.to_le_bytes()[..lb]);
        out.extend_from_slice(&(lcn - prev).to_le_bytes()[..ob]);
        prev = lcn;
    }
    out.push(0);
    out
}

/// Joins runs that follow on from each other on the disk.
fn merge_runs(runs: Vec<Run>) -> Vec<Run> {
    let mut out: Vec<Run> = Vec::new();
    for r in runs {
        match out.last_mut() {
            Some(l) if l.lcn.is_some() && r.lcn == Some(l.lcn.unwrap() + l.len) => l.len += r.len,
            _ => out.push(r),
        }
    }
    out
}

/// An index entry: a file reference, its $FILE_NAME as the key, and the
/// child block it leads to, if any.
fn index_entry(file: u64, key: &[u8], child: Option<u64>) -> Vec<u8> {
    let base = align8(16 + key.len());
    let len = base + if child.is_some() { 8 } else { 0 };
    let mut e = vec![0u8; len];
    put64(&mut e, 0, file);
    put16(&mut e, 8, len as u16);
    put16(&mut e, 10, key.len() as u16);
    e[16..16 + key.len()].copy_from_slice(key);
    if let Some(vcn) = child {
        put16(&mut e, 12, ENTRY_CHILD);
        put64(&mut e, len - 8, vcn);
    }
    e
}

fn last_entry(child: Option<u64>) -> Vec<u8> {
    let mut e = index_entry(0, &[], child);
    let flags = u16_at(&e, 12) | ENTRY_LAST;
    put16(&mut e, 12, flags);
    e
}

fn entry_flags(e: &[u8]) -> u16 {
    u16_at(e, 12)
}
fn entry_is_last(e: &[u8]) -> bool {
    entry_flags(e) & ENTRY_LAST != 0
}
fn entry_child(e: &[u8]) -> Option<u64> {
    (entry_flags(e) & ENTRY_CHILD != 0).then(|| u64_at(e, e.len() - 8))
}
/// The same entry leading to `child` instead (or to none).
fn with_child(e: &[u8], child: Option<u64>) -> Vec<u8> {
    let base = if entry_flags(e) & ENTRY_CHILD != 0 { e.len() - 8 } else { e.len() };
    let mut out = e[..base].to_vec();
    let mut flags = entry_flags(e) & !ENTRY_CHILD;
    if let Some(vcn) = child {
        out.extend_from_slice(&vcn.to_le_bytes());
        flags |= ENTRY_CHILD;
    }
    let len = out.len() as u16;
    put16(&mut out, 8, len);
    put16(&mut out, 12, flags);
    out
}
fn entry_name(e: &[u8]) -> Vec<u16> {
    let key = &e[16..16 + u16_at(e, 10) as usize];
    if key.len() < 66 {
        return Vec::new();
    }
    (0..key[64] as usize).filter(|i| 66 + 2 * i + 2 <= key.len()).map(|i| u16_at(key, 66 + 2 * i)).collect()
}

fn parse_entries(buf: &[u8], hdr: usize) -> Res<Vec<Vec<u8>>> {
    let mut p = hdr + u32_at(buf, hdr) as usize;
    let end = (hdr + u32_at(buf, hdr + 4) as usize).min(buf.len());
    let mut out = Vec::new();
    while p + 16 <= end {
        let len = u16_at(buf, p + 8) as usize;
        if len < 16 || p + len > end {
            return Err("bad NTFS index entry");
        }
        out.push(buf[p..p + len].to_vec());
        if entry_is_last(&buf[p..p + len]) {
            return Ok(out);
        }
        p += len;
    }
    Err("bad NTFS index")
}

/// A node of a folder's index: the root (in the folder's record) or a block.
struct Node {
    /// The block's VCN, or None for the root.
    vcn: Option<u64>,
    /// The last entry is the nameless one; with children, all have one.
    entries: Vec<Vec<u8>>,
}

impl Node {
    fn real(&self) -> usize {
        self.entries.len() - 1
    }
    fn bytes(&self) -> usize {
        self.entries.iter().map(|e| e.len()).sum()
    }
}

/// What a folder's index blocks are kept in.
struct IndexBlocks {
    runs: Vec<Run>,
    /// Bytes of $INDEX_ALLOCATION, and its $BITMAP (one bit per block).
    size: u64,
    bitmap: Vec<u8>,
}

/// Why a name can't be a file name on NTFS (as Windows sees it), if so.
fn bad_name(name: &str) -> Option<&'static str> {
    if name.is_empty() || name == "." || name == ".." || name.encode_utf16().count() > 255 {
        return Some("bad file name");
    }
    if name.chars().any(|c| (c as u32) < 0x20 || "\\/:*?\"<>|".contains(c)) || name.ends_with(' ') || name.ends_with('.') {
        return Some("bad file name");
    }
    None
}

/// Whether a name is already a DOS 8.3 name (upper case), so one name
/// serves both namespaces.
fn is_dos_name(name: &str) -> bool {
    let (base, ext) = name.split_once('.').unwrap_or((name, ""));
    let ok = |s: &str, max: usize| {
        !s.is_empty() && s.len() <= max && s.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b"!#$%&'()-@^_`{}~".contains(&b))
    };
    ok(base, 8) && (ext.is_empty() || ok(ext, 3)) && !ext.contains('.')
}

impl NtfsVolume {
    fn check_writable(&self) -> Res<()> {
        if self.writable.load(Atomic::Relaxed) { Ok(()) } else { Err(NOT_ALLOWED) }
    }

    /// Why Windows' state of the volume forbids writing to it, if it does.
    pub fn unclean(&self) -> Option<&'static str> {
        let Ok(rec) = self.record(RECORD_VOLUME) else { return Some("its volume information can't be read") };
        let Some(info) = find_attr(&rec, ATTR_VOLUME_INFORMATION, &[]).and_then(|p| resident_value(&rec, p)) else {
            return Some("its volume information can't be read");
        };
        if info.len() < 12 || info[8] != 3 || info[9] != 1 {
            return Some("it is not NTFS 3.1 (Windows XP or later)");
        }
        if u16_at(info, 10) & VOLUME_DIRTY != 0 {
            return Some("Windows has to check it first (run chkdsk on it in Windows)");
        }
        match self.journal_clean() {
            Ok(true) => {}
            Ok(false) => return Some("Windows did not shut it down fully (hibernated, or Fast Startup is on)"),
            Err(_) => return Some("its journal can't be read"),
        }
        // A hibernated Windows keeps its open files in hiberfil.sys.
        if let Ok(names) = self.dir_names(RECORD_ROOT) {
            if let Some(h) = names.iter().find(|e| self.same_name(&e.name, "hiberfil.sys")) {
                if let Some(a) = self.attributes(h.record).ok().and_then(|attrs| self.data_attr(&attrs, &[])) {
                    if self.read_attr(&a, 4).is_ok_and(|b| b.eq_ignore_ascii_case(b"hibr")) {
                        return Some("Windows is hibernated on it (or Fast Startup is on)");
                    }
                }
            }
        }
        None
    }

    /// Whether $LogFile has nothing for Windows to replay: emptied (all
    /// 0xFF), or its newest restart area says the volume is clean.
    fn journal_clean(&self) -> Res<bool> {
        let attrs = self.attributes(RECORD_LOGFILE)?;
        let log = self.data_attr(&attrs, &[]).ok_or("no journal")?;
        let head = self.read_attr(&log, 2 * 4096)?;
        if head.iter().all(|&b| b == 0xFF) {
            return Ok(true);
        }
        let mut best: Option<(u64, bool)> = None;
        let page = |at: usize| -> Option<(u64, bool, usize)> {
            let mut p = head.get(at..at + 4096)?.to_vec();
            if &p[0..4] != b"RSTR" || self.fixup(&mut p).is_err() {
                return None;
            }
            let size = u32_at(&p, 0x10) as usize;
            let ra = u16_at(&p, 0x18) as usize;
            if ra + 16 > p.len() {
                return None;
            }
            let lsn = u64_at(&p, ra);
            let in_use = u16_at(&p, ra + 0x0C);
            let clean = in_use == 0xFFFF || u16_at(&p, ra + 0x0E) & 0x0002 != 0;
            Some((lsn, clean, size))
        };
        if let Some((lsn, clean, size)) = page(0) {
            best = Some((lsn, clean));
            if (512..=4096).contains(&size) {
                if let Some((l2, c2, _)) = page(size) {
                    if l2 > lsn {
                        best = Some((l2, c2));
                    }
                }
            }
        }
        best.map(|(_, clean)| clean).ok_or("bad journal")
    }

    /// Stamps the update sequence on a record or index block before it is
    /// written (the inverse of `fixup`).
    fn protect(&self, buf: &mut [u8]) -> Res<()> {
        let off = u16_at(buf, 4) as usize;
        let count = u16_at(buf, 6) as usize;
        if count == 0 || off + 2 * count > buf.len() || (count - 1) * USA_STRIDE > buf.len() {
            return Err("bad NTFS update sequence");
        }
        let mut usn = u16_at(buf, off).wrapping_add(1);
        if usn == 0 || usn == 0xFFFF {
            usn = 1;
        }
        put16(buf, off, usn);
        for i in 1..count {
            let pos = i * USA_STRIDE - 2;
            buf[off + 2 * i] = buf[pos];
            buf[off + 2 * i + 1] = buf[pos + 1];
            put16(buf, pos, usn);
        }
        Ok(())
    }

    /// Writes `data` at offset `start` of what the runs describe (whole
    /// sectors; partial ones are read first).
    fn write_runs(&self, runs: &[Run], start: u64, data: &[u8]) -> Res<()> {
        let end = start + data.len() as u64;
        let bps = self.bytes_per_sector;
        let mut done = 0u64;
        for r in runs {
            let r_start = r.vcn * self.cluster;
            let r_end = r_start + r.len * self.cluster;
            let from = start.max(r_start);
            let to = end.min(r_end);
            if from >= to {
                continue;
            }
            let lcn = r.lcn.ok_or("can't write into a hole")?;
            let disk = lcn * self.cluster + (from - r_start);
            if lcn + r.len > self.clusters {
                return Err("file outside the volume");
            }
            let first = disk / bps;
            let skip = (disk % bps) as usize;
            let n = (to - from) as usize;
            let bytes = (skip as u64 + n as u64).div_ceil(bps) * bps;
            let src = &data[(from - start) as usize..(from - start) as usize + n];
            if skip == 0 && n as u64 == bytes {
                self.dev.write(first, src)?;
            } else {
                let mut buf = vec![0u8; bytes as usize];
                self.dev.read(first, &mut buf)?;
                buf[skip..skip + n].copy_from_slice(src);
                self.dev.write(first, &buf)?;
            }
            done += n as u64;
        }
        if done != data.len() as u64 {
            return Err("write past the end of the file's space");
        }
        Ok(())
    }

    /// A record as it is on the disk (fixed up), in use or not; None when
    /// it was never set up.
    fn raw_record(&self, n: u64) -> Res<Option<Vec<u8>>> {
        let mut rec = self.read_runs(&self.mft.lock().clone(), n * self.record_size as u64, self.record_size)?;
        if &rec[0..4] != b"FILE" || self.fixup(&mut rec).is_err() {
            return Ok(None);
        }
        Ok(Some(rec))
    }

    /// Writes record `n` (and its copy in $MFTMirr for the first few).
    fn write_record(&self, n: u64, rec: &[u8]) -> Res<()> {
        let mut buf = rec.to_vec();
        self.protect(&mut buf)?;
        self.write_runs(&self.mft.lock().clone(), n * self.record_size as u64, &buf)?;
        if n < self.mirror_records {
            let mirror = [Run { vcn: 0, lcn: Some(self.mirror_lcn), len: (self.mirror_records * self.record_size as u64).div_ceil(self.cluster) }];
            self.write_runs(&mirror, n * self.record_size as u64, &buf)?;
        }
        Ok(())
    }

    /// The reference other records use for record `n`: its number and sequence.
    fn reference(&self, n: u64) -> Res<u64> {
        Ok(n | (u16_at(&self.record(n)?, 16) as u64) << 48)
    }

    // ---------------------------------------------- start and end of a change

    /// Before a change: empty the journal (once) and mark the volume dirty.
    fn begin(&self, st: &mut State) -> Res<()> {
        if !st.log_reset {
            // Windows replays a journal that is not empty; ours would not
            // match what it describes. All 0xFF means "start a new one".
            let attrs = self.attributes(RECORD_LOGFILE)?;
            let log = self.data_attr(&attrs, &[]).ok_or("no journal")?;
            if log.value.is_some() || log.runs.iter().any(|r| r.lcn.is_none()) {
                return Err("bad journal");
            }
            let chunk = vec![0xFFu8; 1 << 20];
            let mut at = 0;
            while at < log.size {
                let n = (log.size - at).min(chunk.len() as u64) as usize;
                self.write_runs(&log.runs, at, &chunk[..n])?;
                at += n as u64;
            }
            self.dev.flush()?;
            st.log_reset = true;
        }
        self.set_dirty(true)
    }

    /// After a change: everything to the disk, then the volume clean again.
    fn end(&self) -> Res<()> {
        self.dev.flush()?;
        self.set_dirty(false)
    }

    fn set_dirty(&self, on: bool) -> Res<()> {
        let mut rec = self.record(RECORD_VOLUME)?;
        let p = find_attr(&rec, ATTR_VOLUME_INFORMATION, &[]).ok_or("no volume information")?;
        let off = p + u16_at(&rec, p + 20) as usize;
        let flags = u16_at(&rec, off + 10);
        put16(&mut rec, off + 10, if on { flags | VOLUME_DIRTY } else { flags & !VOLUME_DIRTY });
        self.write_record(RECORD_VOLUME, &rec)?;
        self.dev.flush()
    }

    /// Runs a change between `begin` and `end`. When it fails part way the
    /// volume is left marked dirty, for chkdsk.
    fn change<T>(&self, st: &mut State, f: impl FnOnce(&Self, &mut State) -> Res<T>) -> Res<T> {
        self.begin(st)?;
        let out = f(self, st)?;
        self.end()?;
        Ok(out)
    }

    // -------------------------------------------------------------- space

    fn volume_bitmap(&self) -> Res<Attr> {
        let attrs = self.attributes(RECORD_BITMAP)?;
        let a = self.data_attr(&attrs, &[]).ok_or("no cluster bitmap")?;
        if a.value.is_some() {
            return Err("bad cluster bitmap");
        }
        Ok(a)
    }

    /// Takes `n` clusters, in as few runs as it can, from where the last
    /// ones were taken onwards.
    fn alloc_clusters(&self, st: &mut State, n: u64) -> Res<Vec<Run>> {
        if n == 0 {
            return Ok(Vec::new());
        }
        let bm = self.volume_bitmap()?;
        let mut got = Vec::new();
        let mut need = n;
        let hint = st.next_lcn.min(self.clusters);
        for (from, to) in [(hint, self.clusters), (0, hint)] {
            if need > 0 {
                self.take_free(&bm, from, to, &mut need, &mut got)?;
            }
        }
        let got = merge_runs(got);
        if need > 0 {
            self.free_clusters(&got)?;
            return Err("disk full");
        }
        if let Some(last) = got.last() {
            st.next_lcn = (last.lcn.unwrap() + last.len) % self.clusters;
        }
        Ok(got)
    }

    /// Takes free clusters from `from` up to `to`, a piece of bitmap at a time.
    fn take_free(&self, bm: &Attr, from: u64, to: u64, need: &mut u64, got: &mut Vec<Run>) -> Res<()> {
        const CHUNK_BITS: u64 = 512 * 1024;
        let mut start = from;
        while start < to && *need > 0 {
            let end = (start - start % CHUNK_BITS + CHUNK_BITS).min(to);
            let base = start / 8;
            let mut buf = self.read_runs(&bm.runs, base, (end.div_ceil(8) - base) as usize)?;
            let at = |c: u64| ((c / 8 - base) as usize, 1u8 << (c % 8));
            let mut c = start;
            let mut changed = false;
            while c < end && *need > 0 {
                let (i, b) = at(c);
                if buf[i] & b != 0 {
                    c += 1;
                    continue;
                }
                let first = c;
                while c < end && c - first < *need {
                    let (i, b) = at(c);
                    if buf[i] & b != 0 {
                        break;
                    }
                    buf[i] |= b;
                    c += 1;
                }
                got.push(Run { vcn: 0, lcn: Some(first), len: c - first });
                *need -= c - first;
                changed = true;
            }
            if changed {
                self.write_runs(&bm.runs, base, &buf)?;
            }
            start = end;
        }
        Ok(())
    }

    fn free_clusters(&self, runs: &[Run]) -> Res<()> {
        let bm = self.volume_bitmap()?;
        for r in runs {
            let Some(lcn) = r.lcn else { continue };
            if r.len == 0 {
                continue;
            }
            let (first, last) = (lcn / 8, (lcn + r.len - 1) / 8);
            let mut buf = self.read_runs(&bm.runs, first, (last - first + 1) as usize)?;
            for c in lcn..lcn + r.len {
                buf[(c / 8 - first) as usize] &= !(1 << (c % 8));
            }
            self.write_runs(&bm.runs, first, &buf)?;
        }
        Ok(())
    }

    /// The MFT's own bitmap of records in use.
    fn mft_bitmap(&self) -> Res<(Attr, u64)> {
        let attrs = self.attributes(RECORD_MFT)?;
        let data = self.data_attr(&attrs, &[]).ok_or("$MFT has no data")?;
        let bm = attrs.iter().find(|a| a.kind == ATTR_BITMAP && a.name.is_empty()).cloned().ok_or("$MFT has no bitmap")?;
        if bm.value.is_some() {
            return Err("the file table is too small to add to");
        }
        Ok((bm, data.initialized.min(data.size) / self.record_size as u64))
    }

    /// Takes a free record, growing the MFT when it has none.
    fn alloc_record(&self, st: &mut State) -> Res<u64> {
        let (bm, records) = self.mft_bitmap()?;
        let records = records.min(bm.size * 8);
        let mut buf = self.read_runs(&bm.runs, 0, records.div_ceil(8) as usize)?;
        let Some(n) = (FIRST_USER_RECORD..records).find(|&n| buf[(n / 8) as usize] & (1 << (n % 8)) == 0) else {
            self.grow_mft(st)?;
            return self.alloc_record(st);
        };
        buf[(n / 8) as usize] |= 1 << (n % 8);
        self.write_runs(&bm.runs, n / 8, &buf[(n / 8) as usize..(n / 8) as usize + 1])?;
        Ok(n)
    }

    /// Adds 64 records to the MFT (and room for them in its bitmap). The
    /// new records are written out empty before the MFT says it has them.
    fn grow_mft(&self, st: &mut State) -> Res<()> {
        const FULL: &str = "the drive's file table is full";
        let rs = self.record_size as u64;
        let mut rec = self.record(RECORD_MFT)?;
        if find_attr(&rec, ATTR_LIST, &[]).is_some() {
            return Err(FULL);
        }
        let p = find_attr(&rec, ATTR_DATA, &[]).ok_or("$MFT has no data")?;
        if rec[p + 8] == 0 || u64_at(&rec, p + 16) != 0 {
            return Err(FULL);
        }
        let mut runs = decode_runs(&rec[p + u16_at(&rec, p + 32) as usize..p + u32_at(&rec, p + 4) as usize], 0)?;
        let (allocated, size) = (u64_at(&rec, p + 0x28), u64_at(&rec, p + 0x30));
        if u64_at(&rec, p + 0x38) != size || size % rs != 0 {
            return Err(FULL);
        }
        let new_size = size + 64 * rs;
        let mut new = Vec::new();
        let mut alloc = allocated;
        if new_size > allocated {
            let need = (new_size - allocated).div_ceil(self.cluster);
            new = self.alloc_clusters(st, need)?;
            let mut vcn = allocated / self.cluster;
            for r in &new {
                runs.push(Run { vcn, lcn: r.lcn, len: r.len });
                vcn += r.len;
            }
            alloc += need * self.cluster;
        }
        let runs = merge_runs(runs);
        // Empty records, written where the MFT is about to reach.
        for n in size / rs..new_size / rs {
            let mut r = vec![0u8; rs as usize];
            r[0..4].copy_from_slice(b"FILE");
            let count = rs as usize / USA_STRIDE + 1;
            put16(&mut r, 4, 0x30);
            put16(&mut r, 6, count as u16);
            put16(&mut r, 16, 1);
            let first = align8(0x30 + 2 * count);
            put16(&mut r, 20, first as u16);
            put32(&mut r, 24, (first + 8) as u32);
            put32(&mut r, 28, rs as u32);
            put32(&mut r, 44, n as u32);
            put32(&mut r, first, ATTR_END);
            self.protect(&mut r)?;
            self.write_runs(&runs, n * rs, &r)?;
        }
        // Room in $MFT's bitmap: a bit per record, in steps of 8 bytes.
        let b = find_attr(&rec, ATTR_BITMAP, &[]).ok_or("$MFT has no bitmap")?;
        if rec[b + 8] == 0 {
            self.free_clusters(&new)?;
            return Err(FULL);
        }
        let mut bruns = decode_runs(&rec[b + u16_at(&rec, b + 32) as usize..b + u32_at(&rec, b + 4) as usize], 0)?;
        let (balloc, bsize) = (u64_at(&rec, b + 0x28), u64_at(&rec, b + 0x30));
        let bneed = align8((new_size / rs).div_ceil(8) as usize) as u64;
        let (mut bmore, mut balloc2) = (Vec::new(), balloc);
        if bneed > balloc {
            bmore = self.alloc_clusters(st, (bneed - balloc).div_ceil(self.cluster))?;
            let mut vcn = balloc / self.cluster;
            for r in &bmore {
                bruns.push(Run { vcn, lcn: r.lcn, len: r.len });
                vcn += r.len;
                balloc2 += r.len * self.cluster;
            }
        }
        let bruns = merge_runs(bruns);
        if bneed > bsize {
            self.write_runs(&bruns, bsize, &vec![0u8; (bneed - bsize) as usize])?;
        }
        self.dev.flush()?;
        let placed = replace_attr(&mut rec, p, nonresident_attr(ATTR_DATA, &[], &runs, alloc, new_size)).and_then(|_| {
            let b = find_attr(&rec, ATTR_BITMAP, &[]).ok_or("$MFT has no bitmap")?;
            replace_attr(&mut rec, b, nonresident_attr(ATTR_BITMAP, &[], &bruns, balloc2, bneed.max(bsize)))
        });
        if placed.is_err() {
            self.free_clusters(&new)?;
            self.free_clusters(&bmore)?;
            return Err(FULL);
        }
        self.write_record(RECORD_MFT, &rec)?;
        *self.mft.lock() = runs;
        Ok(())
    }

    fn free_record(&self, n: u64) -> Res<()> {
        let (bm, _) = self.mft_bitmap()?;
        let mut b = self.read_runs(&bm.runs, n / 8, 1)?;
        b[0] &= !(1 << (n % 8));
        self.write_runs(&bm.runs, n / 8, &b)
    }

    /// An empty record `n`, keeping the sequence number it had.
    fn blank_record(&self, n: u64, dir: bool) -> Res<Vec<u8>> {
        let old = self.raw_record(n)?;
        let rs = self.record_size;
        let mut r = vec![0u8; rs];
        r[0..4].copy_from_slice(b"FILE");
        let count = rs / USA_STRIDE + 1;
        put16(&mut r, 4, 0x30);
        put16(&mut r, 6, count as u16);
        let seq = old.as_ref().map_or(1, |o| u16_at(o, 16)).max(1);
        if let Some(o) = &old {
            let off = u16_at(o, 4) as usize;
            if off + 2 <= o.len() {
                put16(&mut r, 0x30, u16_at(o, off));
            }
        }
        let first = align8(0x30 + 2 * count);
        put16(&mut r, 16, seq);
        put16(&mut r, 18, 1);
        put16(&mut r, 20, first as u16);
        put16(&mut r, 22, if dir { 3 } else { 1 });
        put32(&mut r, 24, (first + 8) as u32);
        put32(&mut r, 28, rs as u32);
        put32(&mut r, 44, n as u32);
        put32(&mut r, first, ATTR_END);
        Ok(r)
    }

    // ------------------------------------------------------------- indexes

    fn upcased(&self, s: &[u16]) -> Vec<u16> {
        s.iter().map(|&c| self.upcase[c as usize]).collect()
    }

    /// The order of names in a folder: ignoring case as Windows does, then by case.
    fn collate(&self, a: &[u16], b: &[u16]) -> Ordering {
        self.upcased(a).cmp(&self.upcased(b)).then_with(|| a.cmp(b))
    }

    fn i30() -> Vec<u16> {
        "$I30".encode_utf16().collect()
    }

    fn index_unit(&self) -> u64 {
        if self.index_block as u64 >= self.cluster { self.cluster } else { self.bytes_per_sector }
    }

    fn root_node(&self, dir: u64) -> Res<Node> {
        let rec = self.record(dir)?;
        let p = find_attr(&rec, ATTR_INDEX_ROOT, &Self::i30()).ok_or("not a directory")?;
        let v = resident_value(&rec, p).ok_or("bad NTFS index")?;
        Ok(Node { vcn: None, entries: parse_entries(v, 16)? })
    }

    fn blocks(&self, dir: u64) -> Res<Option<IndexBlocks>> {
        let attrs = self.attributes(dir)?;
        let i30 = Self::i30();
        let Some(alloc) = attrs.iter().find(|a| a.kind == ATTR_INDEX_ALLOCATION && a.name == i30) else { return Ok(None) };
        let bitmap = attrs.iter().find(|a| a.kind == ATTR_BITMAP && a.name == i30).ok_or("bad NTFS index")?;
        Ok(Some(IndexBlocks { runs: alloc.runs.clone(), size: alloc.size, bitmap: self.read_attr(bitmap, 1 << 20)? }))
    }

    fn block_node(&self, dir: u64, vcn: u64) -> Res<Node> {
        let b = self.blocks(dir)?.ok_or("bad NTFS index")?;
        let mut block = self.read_runs(&b.runs, vcn * self.index_unit(), self.index_block)?;
        if &block[0..4] != b"INDX" {
            return Err("bad NTFS index block");
        }
        self.fixup(&mut block)?;
        Ok(Node { vcn: Some(vcn), entries: parse_entries(&block, 0x18)? })
    }

    /// Writes a node back. Err("full") when it does not fit.
    fn store(&self, dir: u64, node: &Node) -> Res<()> {
        let children = node.entries.last().is_some_and(|e| entry_child(e).is_some());
        match node.vcn {
            None => {
                let mut rec = self.record(dir)?;
                let i30 = Self::i30();
                let p = find_attr(&rec, ATTR_INDEX_ROOT, &i30).ok_or("not a directory")?;
                let old = resident_value(&rec, p).ok_or("bad NTFS index")?;
                let mut v = old[..16].to_vec();
                let len = 16 + node.bytes();
                let mut hdr = [0u8; 16];
                put32(&mut hdr, 0, 16);
                put32(&mut hdr, 4, len as u32);
                put32(&mut hdr, 8, len as u32);
                hdr[12] = children as u8;
                v.extend_from_slice(&hdr);
                for e in &node.entries {
                    v.extend_from_slice(e);
                }
                replace_attr(&mut rec, p, resident_attr(ATTR_INDEX_ROOT, &i30, &v, false)).map_err(|_| "full")?;
                self.write_record(dir, &rec)
            }
            Some(vcn) => {
                let b = self.blocks(dir)?.ok_or("bad NTFS index")?;
                let at = vcn * self.index_unit();
                let mut block = self.read_runs(&b.runs, at, self.index_block)?;
                if &block[0..4] != b"INDX" {
                    return Err("bad NTFS index block");
                }
                self.fixup(&mut block)?;
                let off = u32_at(&block, 0x18) as usize;
                let alloc = u32_at(&block, 0x18 + 8) as usize;
                let len = off + node.bytes();
                if len > alloc || 0x18 + len > block.len() {
                    return Err("full");
                }
                let mut p = 0x18 + off;
                for e in &node.entries {
                    block[p..p + e.len()].copy_from_slice(e);
                    p += e.len();
                }
                block[p..0x18 + alloc].fill(0);
                put32(&mut block, 0x18 + 4, len as u32);
                block[0x18 + 12] = children as u8;
                self.protect(&mut block)?;
                self.write_runs(&b.runs, at, &block)
            }
        }
    }

    /// A new, empty index block for folder `dir` (adding $INDEX_ALLOCATION
    /// the first time). Returns its VCN.
    fn new_block(&self, st: &mut State, dir: u64) -> Res<u64> {
        let i30 = Self::i30();
        let per_block = (self.index_block as u64).div_ceil(self.cluster).max(1);
        let unit = self.index_unit();
        let step = self.index_block as u64 / unit; // VCNs per block
        let mut rec = self.record(dir)?;
        let index = match self.blocks(dir)? {
            None => {
                let runs = self.alloc_clusters(st, per_block)?;
                let bytes = per_block * self.cluster;
                let runs = merge_runs(Self::number(runs));
                let mut bitmap = vec![0u8; 8];
                bitmap[0] = 1;
                let added = add_attr(&mut rec, nonresident_attr(ATTR_INDEX_ALLOCATION, &i30, &runs, bytes, bytes))
                    .and_then(|_| add_attr(&mut rec, resident_attr(ATTR_BITMAP, &i30, &bitmap, false)));
                if let Err(e) = added {
                    self.free_clusters(&runs)?;
                    return Err(e);
                }
                self.format_block(&runs, 0)?;
                self.write_record(dir, &rec)?;
                return Ok(0);
            }
            Some(b) => b,
        };
        let blocks = index.size / self.index_block as u64;
        let free = (0..blocks).find(|&i| index.bitmap.get((i / 8) as usize).is_some_and(|b| b & (1 << (i % 8)) == 0));
        let (i, runs) = match free {
            Some(i) => (i, index.runs.clone()),
            None => {
                // Grow $INDEX_ALLOCATION by a block.
                let more = self.alloc_clusters(st, per_block)?;
                let have: u64 = index.runs.iter().map(|r| r.len).sum();
                let mut runs = index.runs.clone();
                let mut vcn = have;
                for r in more.iter() {
                    runs.push(Run { vcn, lcn: r.lcn, len: r.len });
                    vcn += r.len;
                }
                let runs = merge_runs(runs);
                let size = index.size + self.index_block as u64;
                let p = find_attr(&rec, ATTR_INDEX_ALLOCATION, &i30).ok_or("bad NTFS index")?;
                if rec[p + 8] == 0 || u64_at(&rec, p + 16) != 0 {
                    self.free_clusters(&more)?;
                    return Err("this folder's index is too scattered to grow");
                }
                if let Err(e) = replace_attr(&mut rec, p, nonresident_attr(ATTR_INDEX_ALLOCATION, &i30, &runs, (have + per_block) * self.cluster, size)) {
                    self.free_clusters(&more)?;
                    return Err(e);
                }
                (blocks, runs)
            }
        };
        // Mark it used in the folder's $BITMAP (resident: 8 bytes or more).
        let p = find_attr(&rec, ATTR_BITMAP, &i30).ok_or("bad NTFS index")?;
        let mut bitmap = resident_value(&rec, p).ok_or("this folder's index is too big to grow here")?.to_vec();
        while bitmap.len() <= (i / 8) as usize {
            bitmap.extend_from_slice(&[0; 8]);
        }
        bitmap[(i / 8) as usize] |= 1 << (i % 8);
        replace_attr(&mut rec, p, resident_attr(ATTR_BITMAP, &i30, &bitmap, false))?;
        self.format_block(&runs, i * step)?;
        self.write_record(dir, &rec)?;
        Ok(i * step)
    }

    /// Gives runs from `alloc_clusters` their VCNs, from 0.
    fn number(runs: Vec<Run>) -> Vec<Run> {
        let mut vcn = 0;
        runs.into_iter()
            .map(|r| {
                let out = Run { vcn, lcn: r.lcn, len: r.len };
                vcn += r.len;
                out
            })
            .collect()
    }

    /// Writes an empty index block at `vcn`.
    fn format_block(&self, runs: &[Run], vcn: u64) -> Res<()> {
        let size = self.index_block;
        let mut b = vec![0u8; size];
        b[0..4].copy_from_slice(b"INDX");
        let count = size / USA_STRIDE + 1;
        put16(&mut b, 4, 0x28);
        put16(&mut b, 6, count as u16);
        put64(&mut b, 0x10, vcn);
        let off = align8(0x28 + 2 * count) - 0x18;
        let end = last_entry(None);
        put32(&mut b, 0x18, off as u32);
        put32(&mut b, 0x18 + 4, (off + end.len()) as u32);
        put32(&mut b, 0x18 + 8, (size - 0x18) as u32);
        b[0x18 + off..0x18 + off + end.len()].copy_from_slice(&end);
        self.protect(&mut b)?;
        self.write_runs(runs, vcn * self.index_unit(), &b)
    }

    fn free_block(&self, dir: u64, vcn: u64) -> Res<()> {
        let mut rec = self.record(dir)?;
        let i30 = Self::i30();
        let p = find_attr(&rec, ATTR_BITMAP, &i30).ok_or("bad NTFS index")?;
        let mut bitmap = resident_value(&rec, p).ok_or("bad NTFS index")?.to_vec();
        let i = vcn / (self.index_block as u64 / self.index_unit());
        if let Some(b) = bitmap.get_mut((i / 8) as usize) {
            *b &= !(1 << (i % 8));
        }
        replace_attr(&mut rec, p, resident_attr(ATTR_BITMAP, &i30, &bitmap, false))?;
        self.write_record(dir, &rec)
    }

    /// Where `name` goes (or is) in a node: the first entry not before it.
    fn position(&self, node: &Node, name: &[u16]) -> (usize, bool) {
        for (i, e) in node.entries.iter().enumerate() {
            if entry_is_last(e) {
                return (i, false);
            }
            match self.collate(name, &entry_name(e)) {
                Ordering::Less => return (i, false),
                Ordering::Equal => return (i, true),
                Ordering::Greater => {}
            }
        }
        (node.entries.len() - 1, false)
    }

    /// Adds an entry (without a child) to folder `dir`'s index.
    fn index_add(&self, st: &mut State, dir: u64, entry: Vec<u8>) -> Res<()> {
        let name = entry_name(&entry);
        let mut path: Vec<(Node, usize)> = Vec::new();
        let mut node = self.root_node(dir)?;
        loop {
            let (i, found) = self.position(&node, &name);
            if found {
                return Err("already exists");
            }
            match entry_child(&node.entries[i]) {
                Some(child) => {
                    let next = self.block_node(dir, child)?;
                    path.push((node, i));
                    node = next;
                }
                None => {
                    node.entries.insert(i, entry);
                    break;
                }
            }
        }
        // Store it, splitting full blocks upwards.
        loop {
            match self.store(dir, &node) {
                Ok(()) => return Ok(()),
                Err("full") => {}
                Err(e) => return Err(e),
            }
            if node.vcn.is_none() {
                return self.push_root_down(st, dir, node);
            }
            // Split: the first half goes to a new block, the middle entry up.
            if node.real() < 2 {
                return Err("bad NTFS index (entry too big)");
            }
            let half = node.bytes() / 2;
            let mut m = 0;
            let mut acc = 0;
            while m < node.real() - 1 && acc + node.entries[m].len() < half {
                acc += node.entries[m].len();
                m += 1;
            }
            let left_vcn = self.new_block(st, dir)?;
            let mid = node.entries[m].clone();
            let mut left: Vec<Vec<u8>> = node.entries[..m].to_vec();
            left.push(last_entry(entry_child(&mid)));
            self.store(dir, &Node { vcn: Some(left_vcn), entries: left })?;
            node.entries.drain(..=m);
            self.store(dir, &node)?;
            let (mut parent, i) = path.pop().ok_or("bad NTFS index")?;
            parent.entries.insert(i, with_child(&mid, Some(left_vcn)));
            node = parent;
        }
    }

    /// The root no longer fits in the folder's record: its entries move to
    /// a new block, and the root keeps just a pointer to it.
    fn push_root_down(&self, st: &mut State, dir: u64, root: Node) -> Res<()> {
        let vcn = match self.new_block(st, dir) {
            // No room for the index's attributes beside the old root: empty
            // it first (its entries are in hand, and the volume is dirty).
            Err("no room left in the file's record") => {
                self.store(dir, &Node { vcn: None, entries: vec![last_entry(None)] })?;
                self.new_block(st, dir)?
            }
            r => r?,
        };
        self.store(dir, &Node { vcn: Some(vcn), entries: root.entries })?;
        self.store(dir, &Node { vcn: None, entries: vec![last_entry(Some(vcn))] })
    }

    /// Removes `name` from folder `dir`'s index.
    fn index_remove(&self, st: &mut State, dir: u64, name: &[u16]) -> Res<()> {
        let mut path: Vec<(Node, usize)> = Vec::new();
        let mut node = self.root_node(dir)?;
        loop {
            let (i, found) = self.position(&node, name);
            if found {
                match entry_child(&node.entries[i]) {
                    None => {
                        node.entries.remove(i);
                        return self.store_or_collapse(st, dir, node, path);
                    }
                    Some(child) => {
                        // Swap in the name just before it: the last of the
                        // rightmost leaf under its child.
                        let mut down: Vec<(Node, usize)> = Vec::new();
                        let mut leaf = self.block_node(dir, child)?;
                        while let Some(c) = entry_child(leaf.entries.last().unwrap()) {
                            let next = self.block_node(dir, c)?;
                            let at = leaf.entries.len() - 1;
                            down.push((leaf, at));
                            leaf = next;
                        }
                        if leaf.real() == 0 {
                            return Err("bad NTFS index (empty block)");
                        }
                        let pred = leaf.entries.remove(leaf.real() - 1);
                        node.entries[i] = with_child(&pred, Some(child));
                        self.store(dir, &node)?;
                        path.push((node, i));
                        path.extend(down);
                        return self.store_or_collapse(st, dir, leaf, path);
                    }
                }
            }
            let child = entry_child(&node.entries[i]).ok_or("file not found")?;
            let next = self.block_node(dir, child)?;
            path.push((node, i));
            node = next;
        }
    }

    /// Stores a node that lost an entry. An empty block goes away: the
    /// entry above it comes down into a neighbour (put back with `index_add`).
    fn store_or_collapse(&self, st: &mut State, dir: u64, node: Node, mut path: Vec<(Node, usize)>) -> Res<()> {
        let leaf = entry_child(node.entries.last().unwrap()).is_none();
        if node.vcn.is_none() || node.real() > 0 || !leaf {
            return self.store(dir, &node);
        }
        let vcn = node.vcn.unwrap();
        let (mut parent, i) = path.pop().ok_or("bad NTFS index")?;
        let mut again: Option<Vec<u8>> = None;
        if !entry_is_last(&parent.entries[i]) {
            // The entry pointing at it goes; what was under it is empty.
            again = Some(with_child(&parent.entries.remove(i), None));
        } else if i > 0 {
            // It was under the last entry: the one before takes its place.
            let before = parent.entries.remove(i - 1);
            let last = parent.entries.len() - 1;
            parent.entries[last] = last_entry(entry_child(&before));
            again = Some(with_child(&before, None));
        } else {
            // The parent had nothing else: it becomes an empty leaf too.
            parent.entries = vec![last_entry(None)];
        }
        self.free_block(dir, vcn)?;
        self.store_or_collapse(st, dir, parent, path)?;
        match again {
            Some(e) => self.index_add(st, dir, e),
            None => Ok(()),
        }
    }

    /// Updates the sizes and times copied into `dir`'s index entry for a file.
    fn index_touch(&self, dir: u64, name: &[u16], allocated: u64, size: u64, now: u64) -> Res<()> {
        let mut node = self.root_node(dir)?;
        loop {
            let (i, found) = self.position(&node, name);
            if found {
                let e = &mut node.entries[i];
                put64(e, 16 + 0x10, now);
                put64(e, 16 + 0x18, now);
                put64(e, 16 + 0x20, now);
                put64(e, 16 + 0x28, allocated);
                put64(e, 16 + 0x30, size);
                return self.store(dir, &node);
            }
            let child = entry_child(&node.entries[i]).ok_or("file not found")?;
            node = self.block_node(dir, child)?;
        }
    }

    // --------------------------------------------------------------- files

    fn now(&self) -> u64 {
        crate::rtc::now().map_or(0, |t| t.filetime())
    }

    /// The folder and name a path is made of, with the folder's record.
    fn split<'a>(&self, path: &'a str) -> Res<(u64, &'a str)> {
        let t = path.trim_matches('/');
        let (dir, name) = t.rsplit_once('/').unwrap_or(("", t));
        let d = self.lookup(dir)?;
        if !self.is_dir(d)? {
            return Err("not a directory");
        }
        Ok((d, name))
    }

    fn find_in(&self, dir: u64, name: &str) -> Res<Option<u64>> {
        Ok(self.dir_names(dir)?.into_iter().find(|e| self.same_name(&e.name, name)).map(|e| e.record))
    }

    /// What stops a file's record from being changed by us, if anything.
    fn can_change(&self, n: u64, rec: &[u8]) -> Res<()> {
        if n < FIRST_USER_RECORD {
            return Err("that is one of Windows' own files");
        }
        if find_attr(rec, ATTR_LIST, &[]).is_some() || u64_at(rec, 32) != 0 {
            return Err("that file is too fragmented to change here");
        }
        // More than one name besides a DOS one: hard links.
        let names = attrs_of(rec, ATTR_FILE_NAME).into_iter().filter(|&(p, _)| resident_value(rec, p).is_some_and(|v| v.len() > 65 && v[65] != NAMESPACE_DOS)).count();
        if names > 1 {
            return Err("that file has more than one name (hard links)");
        }
        let si = find_attr(rec, ATTR_STANDARD_INFORMATION, &[]).and_then(|p| resident_value(rec, p)).ok_or("bad NTFS record")?;
        if si.len() >= 0x24 && u32_at(si, 0x20) & FILE_REPARSE_POINT != 0 {
            return Err("that is a link or a cloud file (reparse point)");
        }
        Ok(())
    }

    fn write_file_locked(&self, st: &mut State, path: &str, data: &[u8]) -> Res<()> {
        let (dir, name) = self.split(path)?;
        match self.find_in(dir, name)? {
            Some(n) => {
                let rec = self.record(n)?;
                if u16_at(&rec, 22) & 2 != 0 {
                    return Err("is a directory");
                }
                self.can_change(n, &rec)?;
                let si = find_attr(&rec, ATTR_STANDARD_INFORMATION, &[]).and_then(|p| resident_value(&rec, p)).unwrap_or(&[]);
                if si.len() >= 0x24 && u32_at(si, 0x20) & FILE_READ_ONLY != 0 {
                    return Err("that file is marked read-only");
                }
                if let Some(p) = find_attr(&rec, ATTR_DATA, &[]) {
                    if rec[p + 8] != 0 && u16_at(&rec, p + 12) & (FLAG_COMPRESSED | FLAG_ENCRYPTED | FLAG_SPARSE) != 0 {
                        return Err("compressed, encrypted and sparse NTFS files can't be changed here yet");
                    }
                }
                self.change(st, |v, st| {
                    let (allocated, size) = v.set_data(st, n, data)?;
                    let now = v.now();
                    for (p, _) in attrs_of(&v.record(n)?, ATTR_FILE_NAME) {
                        let rec = v.record(n)?;
                        let fname = resident_value(&rec, p).ok_or("bad NTFS record")?;
                        let units: Vec<u16> = (0..fname[64] as usize).map(|i| u16_at(fname, 66 + 2 * i)).collect();
                        v.index_touch(dir, &units, allocated, size, now)?;
                    }
                    Ok(())
                })
            }
            None => self.create_locked(st, path, Some(data)),
        }
    }

    /// Creates a file holding `data`, or a folder when `data` is None.
    fn create_locked(&self, st: &mut State, path: &str, data: Option<&[u8]>) -> Res<()> {
        let (dir, name) = self.split(path)?;
        if let Some(why) = bad_name(name) {
            return Err(why);
        }
        if self.find_in(dir, name)?.is_some() {
            return Err("already exists");
        }
        let parent_ref = self.reference(dir)?;
        let parent = self.record(dir)?;
        let parent_si = find_attr(&parent, ATTR_STANDARD_INFORMATION, &[]).and_then(|p| resident_value(&parent, p)).ok_or("bad NTFS record")?.to_vec();
        let root_head = find_attr(&parent, ATTR_INDEX_ROOT, &Self::i30()).and_then(|p| resident_value(&parent, p)).ok_or("not a directory")?[..16].to_vec();
        let is_dir = data.is_none();
        self.change(st, |v, st| {
            let n = v.alloc_record(st)?;
            let made = v.make_record(st, n, parent_ref, &parent_si, &root_head, name, data);
            let (allocated, size) = match made {
                Ok(sizes) => sizes,
                Err(e) => {
                    let _ = v.release(n).or_else(|_| v.free_record(n));
                    return Err(e);
                }
            };
            let units: Vec<u16> = name.encode_utf16().collect();
            let now = v.now();
            let key = file_name_value(parent_ref, &units, is_dir, allocated, size, now, if is_dos_name(name) { NAMESPACE_BOTH } else { NAMESPACE_WIN32 });
            let file_ref = v.reference(n)?;
            if let Err(e) = v.index_add(st, dir, index_entry(file_ref, &key, None)) {
                // Not linked in: give back what the record took.
                let _ = v.release(n);
                return Err(e);
            }
            Ok(())
        })
    }

    /// Sets up record `n` for a new file or folder named `name` in `dir`.
    /// Returns the data's allocated and real size.
    #[allow(clippy::too_many_arguments)]
    fn make_record(&self, st: &mut State, n: u64, parent_ref: u64, parent_si: &[u8], root_head: &[u8], name: &str, data: Option<&[u8]>) -> Res<(u64, u64)> {
        let is_dir = data.is_none();
        let now = self.now();
        let mut rec = self.blank_record(n, is_dir)?;
        // $STANDARD_INFORMATION, with the folder's security descriptor.
        let mut si = vec![0u8; 72];
        for o in [0, 8, 16, 24] {
            put64(&mut si, o, now);
        }
        put32(&mut si, 0x20, if is_dir { 0 } else { FILE_ARCHIVE });
        if parent_si.len() >= 0x38 {
            put32(&mut si, 0x34, u32_at(parent_si, 0x34));
        }
        add_attr(&mut rec, resident_attr(ATTR_STANDARD_INFORMATION, &[], &si, false))?;
        let units: Vec<u16> = name.encode_utf16().collect();
        let ns = if is_dos_name(name) { NAMESPACE_BOTH } else { NAMESPACE_WIN32 };
        add_attr(&mut rec, resident_attr(ATTR_FILE_NAME, &[], &file_name_value(parent_ref, &units, is_dir, 0, 0, now, ns), true))?;
        if is_dir {
            let mut v = root_head.to_vec();
            let end = last_entry(None);
            let mut hdr = [0u8; 16];
            put32(&mut hdr, 0, 16);
            put32(&mut hdr, 4, (16 + end.len()) as u32);
            put32(&mut hdr, 8, (16 + end.len()) as u32);
            v.extend_from_slice(&hdr);
            v.extend_from_slice(&end);
            add_attr(&mut rec, resident_attr(ATTR_INDEX_ROOT, &Self::i30(), &v, false))?;
            self.write_record(n, &rec)?;
            return Ok((0, 0));
        }
        add_attr(&mut rec, resident_attr(ATTR_DATA, &[], &[], false))?;
        self.write_record(n, &rec)?;
        self.set_data(st, n, data.unwrap())
    }

    /// Replaces what file `n` holds. Returns its allocated and real size.
    fn set_data(&self, st: &mut State, n: u64, data: &[u8]) -> Res<(u64, u64)> {
        let mut rec = self.record(n)?;
        let p = find_attr(&rec, ATTR_DATA, &[]).ok_or("file has no data")?;
        let old_runs = if rec[p + 8] != 0 { decode_runs(&rec[p + u16_at(&rec, p + 32) as usize..p + u32_at(&rec, p + 4) as usize], 0)? } else { Vec::new() };
        let len = data.len() as u64;
        // Small enough to stay in the record?
        let mut small = rec.clone();
        if len <= self.record_size as u64 / 2 && replace_attr(&mut small, p, resident_attr(ATTR_DATA, &[], data, false)).is_ok() {
            touch_si(&mut small, self.now());
            self.write_record(n, &small)?;
            self.free_clusters(&old_runs)?;
            return Ok((align8(data.len()) as u64, len));
        }
        let need = len.div_ceil(self.cluster);
        let have: u64 = old_runs.iter().map(|r| r.len).sum();
        let mut runs: Vec<Run> = Vec::new();
        let mut spare: Vec<Run> = Vec::new();
        let mut kept = 0;
        for r in &old_runs {
            if r.lcn.is_none() {
                return Err("sparse NTFS files can't be changed here yet");
            }
            let take = r.len.min(need - kept);
            if take > 0 {
                runs.push(Run { vcn: kept, lcn: r.lcn, len: take });
                kept += take;
            }
            if take < r.len {
                spare.push(Run { vcn: 0, lcn: Some(r.lcn.unwrap() + take), len: r.len - take });
            }
        }
        let mut new = Vec::new();
        if need > have {
            new = self.alloc_clusters(st, need - have)?;
            for r in &new {
                runs.push(Run { vcn: kept, lcn: r.lcn, len: r.len });
                kept += r.len;
            }
        }
        let runs = merge_runs(runs);
        // The data, then zeros to the end of its last cluster.
        let mut padded = data.to_vec();
        padded.resize((need * self.cluster) as usize, 0);
        let written = self.write_runs(&runs, 0, &padded).and_then(|_| self.dev.flush());
        let allocated = need * self.cluster;
        let placed = written.and_then(|_| replace_attr(&mut rec, p, nonresident_attr(ATTR_DATA, &[], &runs, allocated, len)));
        if let Err(e) = placed {
            self.free_clusters(&new)?;
            return Err(if e == "no room left in the file's record" { "that file would be too fragmented" } else { e });
        }
        touch_si(&mut rec, self.now());
        self.write_record(n, &rec)?;
        self.free_clusters(&spare)?;
        Ok((allocated, len))
    }

    fn remove_locked(&self, st: &mut State, path: &str) -> Res<()> {
        let (dir, name) = self.split(path)?;
        let n = self.find_in(dir, name)?.ok_or("file not found")?;
        let rec = self.record(n)?;
        self.can_change(n, &rec)?;
        if u16_at(&rec, 22) & 2 != 0 && !self.dir_names(n)?.is_empty() {
            return Err("directory not empty");
        }
        // Every name it has in this folder (a long one and its DOS twin).
        let names: Vec<Vec<u16>> = attrs_of(&rec, ATTR_FILE_NAME)
            .into_iter()
            .filter_map(|(p, _)| resident_value(&rec, p).map(|v| (0..v[64] as usize).map(|i| u16_at(v, 66 + 2 * i)).collect()))
            .collect();
        self.change(st, |v, st| {
            for units in &names {
                match v.index_remove(st, dir, units) {
                    Ok(()) | Err("file not found") => {}
                    Err(e) => return Err(e),
                }
            }
            v.release(n)
        })
    }

    /// Frees a record no folder lists any more, and its clusters.
    fn release(&self, n: u64) -> Res<()> {
        let mut rec = self.record(n)?;
        let mut runs = Vec::new();
        for a in Self::parse_attrs(&rec)? {
            runs.extend(a.runs.iter().copied());
        }
        let flags = u16_at(&rec, 22) & !3;
        put16(&mut rec, 22, flags);
        let seq = u16_at(&rec, 16).wrapping_add(1);
        put16(&mut rec, 16, if seq == 0 { 1 } else { seq });
        self.write_record(n, &rec)?;
        self.free_record(n)?;
        self.free_clusters(&runs)
    }
}

/// Checks of what the writer leaves behind, for tools/ntfs-check on the
/// build machine (not built into the kernel).
#[cfg(not(target_os = "none"))]
impl NtfsVolume {
    /// Starts a change and stops there, as if the machine had gone off.
    pub fn crash(&self) -> Res<()> {
        let st = &mut *self.state.lock();
        self.check_writable()?;
        self.begin(st)
    }

    /// Walks every folder's index: names in order, the tree balanced, its
    /// blocks exactly those its bitmap marks, and every entry naming a
    /// record in use that has that name in that folder. Returns the
    /// number of names seen.
    pub fn verify(&self) -> Res<usize> {
        let mut todo = vec![RECORD_ROOT];
        let mut done = Vec::new();
        let mut seen = 0;
        while let Some(dir) = todo.pop() {
            if done.contains(&dir) {
                continue;
            }
            done.push(dir);
            let mut used = Vec::new();
            let mut leaf_depth = None;
            let root = self.root_node(dir)?;
            seen += self.verify_node(dir, &root, None, None, 0, &mut leaf_depth, &mut used, &mut todo)?;
            if let Some(b) = self.blocks(dir)? {
                let blocks = b.size / self.index_block as u64;
                let step = self.index_block as u64 / self.index_unit();
                for i in 0..blocks {
                    let marked = b.bitmap.get((i / 8) as usize).is_some_and(|x| x & (1 << (i % 8)) != 0);
                    if marked != used.contains(&(i * step)) {
                        return Err(if marked { "index block marked but not used" } else { "index block used but not marked" });
                    }
                }
            } else if !used.is_empty() {
                return Err("index blocks without $INDEX_ALLOCATION");
            }
        }
        Ok(seen)
    }

    #[allow(clippy::too_many_arguments)]
    fn verify_node(&self, dir: u64, node: &Node, lo: Option<Vec<u16>>, hi: Option<Vec<u16>>, depth: usize, leaf_depth: &mut Option<usize>, used: &mut Vec<u64>, todo: &mut Vec<u64>) -> Res<usize> {
        let children = entry_child(node.entries.last().unwrap()).is_some();
        let mut prev = lo;
        let mut n = 0;
        for (i, e) in node.entries.iter().enumerate() {
            if entry_is_last(e) != (i == node.entries.len() - 1) {
                return Err("last-entry flag out of place");
            }
            if entry_child(e).is_some() != children {
                return Err("entries of a node differ in having children");
            }
            let name = if entry_is_last(e) { None } else { Some(entry_name(e)) };
            if let Some(c) = entry_child(e) {
                if used.contains(&c) {
                    return Err("index block used twice");
                }
                used.push(c);
                let child = self.block_node(dir, c)?;
                n += self.verify_node(dir, &child, prev.clone(), name.clone().or(hi.clone()), depth + 1, leaf_depth, used, todo)?;
            }
            if let Some(name) = name {
                if let Some(p) = &prev {
                    if self.collate(p, &name) != Ordering::Less {
                        return Err("index names out of order");
                    }
                }
                if let Some(h) = &hi {
                    if self.collate(&name, h) != Ordering::Less {
                        return Err("index name above its parent's");
                    }
                }
                let file = u64_at(e, 0);
                let rec = self.record(file & 0xFFFF_FFFF_FFFF).map_err(|_| "index entry for a record not in use")?;
                if u16_at(&rec, 16) as u64 != file >> 48 {
                    return Err("index entry with a stale sequence number");
                }
                let named = attrs_of(&rec, ATTR_FILE_NAME).into_iter().any(|(p, _)| {
                    resident_value(&rec, p).is_some_and(|v| u64_at(v, 0) & 0xFFFF_FFFF_FFFF == dir && (0..v[64] as usize).map(|i| u16_at(v, 66 + 2 * i)).collect::<Vec<_>>() == name)
                });
                if !named {
                    return Err("index entry whose record does not have that name");
                }
                if u16_at(&rec, 22) & 2 != 0 && entry_name(e).len() > 0 && e[16 + 65] != NAMESPACE_DOS {
                    todo.push(file & 0xFFFF_FFFF_FFFF);
                }
                n += 1;
                prev = Some(name);
            }
        }
        if !children {
            match leaf_depth {
                Some(d) if *d != depth => return Err("index tree not balanced"),
                _ => *leaf_depth = Some(depth),
            }
        }
        Ok(n)
    }
}

/// Sets the modified, changed and accessed times in a record's $STANDARD_INFORMATION.
fn touch_si(rec: &mut [u8], now: u64) {
    if let Some(p) = find_attr(rec, ATTR_STANDARD_INFORMATION, &[]) {
        let off = p + u16_at(rec, p + 20) as usize;
        if u32_at(rec, p + 16) >= 32 {
            for o in [8, 16, 24] {
                put64(rec, off + o, now);
            }
        }
    }
}

/// A $FILE_NAME value: the folder it is in, times, sizes, flags and name.
fn file_name_value(parent: u64, name: &[u16], dir: bool, allocated: u64, size: u64, now: u64, namespace: u8) -> Vec<u8> {
    let mut v = vec![0u8; 66 + 2 * name.len()];
    put64(&mut v, 0, parent);
    for o in [8, 16, 24, 32] {
        put64(&mut v, o, now);
    }
    put64(&mut v, 40, allocated);
    put64(&mut v, 48, size);
    put32(&mut v, 56, if dir { FILE_NAME_DIRECTORY } else { FILE_ARCHIVE });
    v[64] = name.len() as u8;
    v[65] = namespace;
    for (i, &c) in name.iter().enumerate() {
        put16(&mut v, 66 + 2 * i, c);
    }
    v
}

impl vfs::Volume for NtfsVolume {
    fn kind(&self) -> &'static str {
        "NTFS"
    }
    fn label(&self) -> &str {
        &self.label
    }
    fn dev(&self) -> &Arc<dyn BlockDevice> {
        &self.dev
    }
    fn read_only(&self) -> bool {
        !self.writable.load(Atomic::Relaxed)
    }
    fn set_writable(&self, on: bool) -> Res<()> {
        if on {
            if let Some(why) = self.unclean() {
                return Err(why);
            }
        }
        self.writable.store(on, Atomic::Relaxed);
        Ok(())
    }
    fn list(&self, path: &str) -> Res<Vec<vfs::DirEntry>> {
        let n = self.lookup(path)?;
        let at_root = n == RECORD_ROOT;
        let names = match self.dir_names(n) {
            Ok(names) => names,
            Err("not a directory") => {
                let name = path.trim_end_matches('/').rsplit('/').next().unwrap_or("");
                vec![Name { record: n, name: String::from(name) }]
            }
            Err(e) => return Err(e),
        };
        let mut out = Vec::new();
        for e in names {
            // The metadata files ($MFT, $Bitmap, ...) and Windows' hidden
            // $Recycle.Bin live in the root; Explorer does not show them.
            if at_root && (e.record < 16 || e.name.starts_with('$')) {
                continue;
            }
            let Ok(attrs) = self.attributes(e.record) else { continue };
            let is_dir = attrs.iter().any(|a| a.kind == ATTR_INDEX_ROOT);
            let size = if is_dir { 0 } else { self.data_attr(&attrs, &[]).map_or(0, |a| a.size()) };
            // $STANDARD_INFORMATION's second time is when the data last changed.
            let modified = attrs
                .iter()
                .find(|a| a.kind == ATTR_STANDARD_INFORMATION)
                .and_then(|a| a.value.as_ref())
                .filter(|v| v.len() >= 16)
                .and_then(|v| crate::rtc::DateTime::from_filetime(u64::from_le_bytes(v[8..16].try_into().unwrap())))
                .map_or(0, |t| t.packed());
            out.push(vfs::DirEntry { name: e.name, is_dir, size, modified });
        }
        Ok(out)
    }
    fn read_file(&self, path: &str, limit: usize) -> Res<Vec<u8>> {
        let n = self.lookup(path)?;
        if self.is_dir(n)? {
            return Err("is a directory");
        }
        let attrs = self.attributes(n)?;
        let data = self.data_attr(&attrs, &[]).ok_or("file has no data")?;
        self.read_attr(&data, limit)
    }
    fn write_file(&self, path: &str, data: &[u8]) -> Res<()> {
        let st = &mut *self.state.lock();
        self.check_writable()?;
        self.write_file_locked(st, path, data)
    }
    fn create_dir(&self, path: &str) -> Res<()> {
        let st = &mut *self.state.lock();
        self.check_writable()?;
        self.create_locked(st, path, None)
    }
    fn remove(&self, path: &str) -> Res<()> {
        let st = &mut *self.state.lock();
        self.check_writable()?;
        self.remove_locked(st, path)
    }
}
