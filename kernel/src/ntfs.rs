//! NTFS, read-only: the filesystem Windows keeps its drives in.
//!
//! Everything on an NTFS volume is a file described by a record in the
//! Master File Table (MFT), the MFT included. A record holds attributes:
//! names, data (inside the record when small, else as a list of cluster
//! runs), and for folders a B-tree index of the names in them. Big or
//! scattered files can spill attributes into more records, listed by an
//! attribute list. Compressed and encrypted files are refused for now, and
//! nothing is written: Windows keeps a journal on NTFS that a writer must
//! honour, which is a project of its own.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use crate::block::BlockDevice;
use crate::vfs;

type Res<T> = Result<T, &'static str>;

const ATTR_LIST: u32 = 0x20;
const ATTR_VOLUME_NAME: u32 = 0x60;
const ATTR_DATA: u32 = 0x80;
const ATTR_INDEX_ROOT: u32 = 0x90;
const ATTR_INDEX_ALLOCATION: u32 = 0xA0;
const ATTR_END: u32 = 0xFFFF_FFFF;

const RECORD_ROOT: u64 = 5;
const RECORD_UPCASE: u64 = 10;
const RECORD_VOLUME: u64 = 3;

const FLAG_COMPRESSED: u16 = 0x0001;
const FLAG_ENCRYPTED: u16 = 0x4000;

const READ_ONLY: &str = "NTFS volumes are read-only for now";

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
    record_size: usize,
    index_block: usize,
    mft: Vec<Run>,
    label: String,
    upcase: Vec<u16>,
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
        let mut vol = Self {
            dev,
            bytes_per_sector,
            cluster,
            record_size,
            index_block,
            // Enough to read record 0, which then describes the whole MFT.
            mft: vec![Run { vcn: 0, lcn: Some(mft_lcn), len: (record_size as u64).div_ceil(cluster).max(1) * 16 }],
            label: String::new(),
            upcase: Vec::new(),
        };
        let mft_data = vol.data_attr(&vol.attributes(0)?, &[]).ok_or("$MFT has no data")?;
        vol.mft = mft_data.runs;
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
        let stride = self.bytes_per_sector as usize;
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
        let mut rec = self.read_runs(&self.mft, n * self.record_size as u64, self.record_size)?;
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
        true
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
            out.push(vfs::DirEntry { name: e.name, is_dir, size });
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
    fn write_file(&self, _: &str, _: &[u8]) -> Res<()> {
        Err(READ_ONLY)
    }
    fn create_dir(&self, _: &str) -> Res<()> {
        Err(READ_ONLY)
    }
    fn remove(&self, _: &str) -> Res<()> {
        Err(READ_ONLY)
    }
}
