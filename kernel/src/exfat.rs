//! exFAT filesystem (Microsoft's exFAT specification), read and write.
//! USB sticks and SD cards over 32 GB come formatted with it.
//!
//! The allocation bitmap is kept in memory and is what says which clusters
//! are in use; the FAT is only needed for files that are not in one piece
//! (Windows marks one-piece files "no FAT chain"). Names are compared with
//! the volume's own up-case table, so lookups are case-insensitive the way
//! Windows does it.
//!
//! As with FAT32, updates go data, then FAT and bitmap, then the directory
//! entry set, then a cache flush; each volume has one lock.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use crate::block::BlockDevice;
use crate::sync::SleepMutex;
use crate::vfs;

type Res<T> = Result<T, &'static str>;

const ENTRY_END: u8 = 0x00;
const ENTRY_BITMAP: u8 = 0x81;
const ENTRY_UPCASE: u8 = 0x82;
const ENTRY_LABEL: u8 = 0x83;
const ENTRY_FILE: u8 = 0x85;
const ENTRY_STREAM: u8 = 0xC0;
const ENTRY_NAME: u8 = 0xC1;
const IN_USE: u8 = 0x80;

const ATTR_DIR: u16 = 0x10;
const ATTR_ARCHIVE: u16 = 0x20;
const FLAG_ALLOC_POSSIBLE: u8 = 0x01;
const FLAG_NO_FAT_CHAIN: u8 = 0x02;
const EOC: u32 = 0xFFFF_FFFF;

pub struct ExfatVolume {
    dev: Arc<dyn BlockDevice>,
    bytes_per_sector: u32,
    sectors_per_cluster: u32,
    fat_start: u64,
    heap_start: u64,
    clusters: u32,
    root_cluster: u32,
    label: String,
    upcase: Vec<u16>,
    state: SleepMutex<State>,
}

struct State {
    /// One bit per cluster, cluster 2 first: the truth about what is free.
    bitmap: Vec<u8>,
    bitmap_clusters: Vec<u32>,
    dirty: Vec<bool>, // per bitmap sector
    free: u32,
    next_free: u32,
    fat_cache: Option<(u64, Vec<u8>)>,
}

/// Where a file's or directory's data lives.
#[derive(Clone, Copy, Debug)]
struct Stream {
    first: u32,
    len: u64,
    valid: u64,
    contiguous: bool,
}

/// A file or directory entry set, as found in its directory.
#[derive(Clone)]
struct Entry {
    name: String,
    attrs: u16,
    stream: Stream,
    /// The whole entry set (file, stream extension, names) and its first slot.
    raw: Vec<u8>,
    slot: u32,
    /// Clusters of the directory holding the set; None for the root.
    parent: Option<Vec<u32>>,
}

impl Entry {
    fn is_dir(&self) -> bool {
        self.attrs & ATTR_DIR != 0
    }
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

impl ExfatVolume {
    pub fn mount(dev: Arc<dyn BlockDevice>) -> Res<Self> {
        let mut bs = vec![0u8; dev.block_size() as usize];
        dev.read(0, &mut bs)?;
        if &bs[3..11] != b"EXFAT   " || bs[510] != 0x55 || bs[511] != 0xAA {
            return Err("not an exFAT volume");
        }
        let bps_shift = bs[108] as u32;
        let spc_shift = bs[109] as u32;
        if !(9..=12).contains(&bps_shift) || bps_shift + spc_shift > 25 {
            return Err("bad exFAT boot sector");
        }
        let bytes_per_sector = 1u32 << bps_shift;
        if bytes_per_sector != dev.block_size() {
            return Err("sector size differs from the device block size");
        }
        let fats = bs[110] as u64;
        let active = if fats == 2 && u16_at(&bs, 106) & 1 != 0 { 1 } else { 0 };
        let clusters = u32_at(&bs, 92);
        if fats == 0 || clusters == 0 || clusters > 0xFFFF_FFF5 {
            return Err("bad exFAT boot sector");
        }
        let mut vol = Self {
            dev,
            bytes_per_sector,
            sectors_per_cluster: 1 << spc_shift,
            fat_start: u32_at(&bs, 80) as u64 + active * u32_at(&bs, 84) as u64,
            heap_start: u32_at(&bs, 88) as u64,
            clusters,
            root_cluster: u32_at(&bs, 96),
            label: String::new(),
            upcase: Vec::new(),
            state: SleepMutex::new(State {
                bitmap: Vec::new(),
                bitmap_clusters: Vec::new(),
                dirty: Vec::new(),
                free: 0,
                next_free: 2,
                fat_cache: None,
            }),
        };
        if !vol.valid(vol.root_cluster) {
            return Err("bad exFAT root directory");
        }

        // The root holds the bitmap, the up-case table and the label.
        let (label, table) = vol.load_root()?;
        vol.label = label;
        vol.upcase = table;
        Ok(vol)
    }

    fn load_root(&self) -> Res<(String, Vec<u16>)> {
        let vol = self;
        let clusters = self.clusters;
        let bytes_per_sector = self.bytes_per_sector;
        let mut label = String::new();
        let mut guard = vol.state.lock();
        let st = &mut *guard;
        let root = vol.root_stream(st)?;
        let raw = vol.read_stream(st, &root, usize::MAX)?;
        let mut bitmap = None;
        let mut upcase = None;
        for e in raw.chunks(32) {
            match e[0] {
                ENTRY_END => break,
                ENTRY_BITMAP if e[1] & 1 == 0 => bitmap = Some((u32_at(e, 20), u64_at(e, 24))),
                ENTRY_UPCASE => upcase = Some((u32_at(e, 20), u64_at(e, 24))),
                ENTRY_LABEL => {
                    let n = (e[1] as usize).min(11);
                    let units: Vec<u16> = (0..n).map(|i| u16_at(e, 2 + 2 * i)).collect();
                    label = char::decode_utf16(units).map(|c| c.unwrap_or('?')).collect();
                }
                _ => {}
            }
        }
        let (bm_first, bm_len) = bitmap.ok_or("exFAT allocation bitmap missing")?;
        if bm_len < (clusters as u64).div_ceil(8) {
            return Err("exFAT allocation bitmap too small");
        }
        st.bitmap_clusters = vol.system_clusters(st, bm_first, bm_len)?;
        let bm_stream = Stream { first: bm_first, len: bm_len, valid: bm_len, contiguous: false };
        st.bitmap = vol.read_clusters(&st.bitmap_clusters, bm_stream.len as usize)?;
        st.bitmap.truncate((clusters as usize).div_ceil(8));
        st.dirty = vec![false; st.bitmap.len().div_ceil(bytes_per_sector as usize)];
        st.free = (0..clusters).filter(|&i| st.bitmap[i as usize / 8] & (1 << (i % 8)) == 0).count() as u32;

        // Up-case table: runs of identity mappings are compressed as FFFF, count.
        let mut table: Vec<u16> = (0..=0xFFFFu32).map(|c| c as u16).collect();
        if let Some((first, len)) = upcase {
            let clusters_of = vol.system_clusters(st, first, len)?;
            let data = vol.read_clusters(&clusters_of, len as usize)?;
            let mut i = 0usize;
            let mut words = data.chunks_exact(2).map(|w| u16::from_le_bytes([w[0], w[1]]));
            while let Some(w) = words.next() {
                if i >= table.len() {
                    break;
                }
                if w == 0xFFFF {
                    i += words.next().unwrap_or(0) as usize;
                } else {
                    table[i] = w;
                    i += 1;
                }
            }
        } else {
            for c in b'a'..=b'z' {
                table[c as usize] = (c - 32) as u16;
            }
        }
        Ok((label, table))
    }

    // ------------------------------------------------------------ public

    fn list_entries(&self, path: &str) -> Res<Vec<vfs::DirEntry>> {
        let st = &mut *self.state.lock();
        let e = self.lookup(st, path)?;
        let entries = if e.is_dir() { self.read_dir(st, &e.stream)?.1 } else { vec![e] };
        Ok(entries.into_iter().map(|e| vfs::DirEntry { is_dir: e.is_dir(), size: e.stream.len, name: e.name }).collect())
    }

    fn read(&self, path: &str, limit: usize) -> Res<Vec<u8>> {
        let st = &mut *self.state.lock();
        let e = self.lookup(st, path)?;
        if e.is_dir() {
            return Err("is a directory");
        }
        let want = limit.min(e.stream.len as usize);
        let mut out = self.read_stream(st, &e.stream, want.min(e.stream.valid as usize))?;
        out.resize(want, 0); // past the valid data length reads as zeros
        Ok(out)
    }

    fn write(&self, path: &str, data: &[u8]) -> Res<()> {
        let st = &mut *self.state.lock();
        let (parent, name) = self.parent_of(st, path)?;
        let (dir_clusters, entries) = self.read_dir(st, &parent.stream)?;
        let existing = entries.into_iter().find(|e| self.same_name(&e.name, name));
        if existing.as_ref().is_some_and(|e| e.is_dir()) {
            return Err("is a directory");
        }
        let need = data.len().div_ceil(self.cluster_bytes()) as u32;
        let mut existing = existing;
        let new = match self.allocate(st, need) {
            Ok(c) => c,
            // Full: the old contents may be what is in the way.
            Err(e) => match existing.as_mut() {
                Some(old) if old.stream.first != 0 => {
                    let old_stream = old.stream;
                    old.stream = Stream { first: 0, len: 0, valid: 0, contiguous: false };
                    self.put_set(st, old)?;
                    self.release(st, &old_stream)?;
                    self.allocate(st, need)?
                }
                _ => return Err(e),
            },
        };
        let stream = self.claim(st, &new, data)?;
        match existing {
            Some(mut old) => {
                let old_stream = old.stream;
                old.stream = stream;
                old.attrs |= ATTR_ARCHIVE;
                let now = timestamp();
                old.raw[4..6].copy_from_slice(&old.attrs.to_le_bytes());
                old.raw[12..16].copy_from_slice(&now.to_le_bytes()); // modified
                old.raw[16..20].copy_from_slice(&now.to_le_bytes()); // accessed
                old.raw[21] = 0;
                self.put_set(st, &old)?;
                self.release(st, &old_stream)?;
            }
            None => {
                let raw = new_set(name, ATTR_ARCHIVE, &stream, &self.upcase);
                self.add_set(st, &parent, dir_clusters, raw)?;
            }
        }
        self.finish(st)
    }

    fn mkdir(&self, path: &str) -> Res<()> {
        let st = &mut *self.state.lock();
        let (parent, name) = self.parent_of(st, path)?;
        let (dir_clusters, entries) = self.read_dir(st, &parent.stream)?;
        if entries.iter().any(|e| self.same_name(&e.name, name)) {
            return Err("already exists");
        }
        let cluster = self.allocate(st, 1)?;
        let block = vec![0u8; self.cluster_bytes()];
        let stream = self.claim(st, &cluster, &block)?;
        let raw = new_set(name, ATTR_DIR, &stream, &self.upcase);
        self.add_set(st, &parent, dir_clusters, raw)?;
        self.finish(st)
    }

    fn delete(&self, path: &str) -> Res<()> {
        let st = &mut *self.state.lock();
        let (parent, name) = self.parent_of(st, path)?;
        let mut e = self.read_dir(st, &parent.stream)?.1
            .into_iter()
            .find(|e| self.same_name(&e.name, name))
            .ok_or("file not found")?;
        if e.is_dir() && !self.read_dir(st, &e.stream)?.1.is_empty() {
            return Err("directory not empty");
        }
        for chunk in e.raw.chunks_mut(32) {
            chunk[0] &= !IN_USE;
        }
        self.put_set(st, &e)?;
        self.release(st, &e.stream)?;
        self.finish(st)
    }

    // ----------------------------------------------------------- lookups

    fn cluster_bytes(&self) -> usize {
        (self.bytes_per_sector * self.sectors_per_cluster) as usize
    }

    fn cluster_lba(&self, cluster: u32) -> u64 {
        self.heap_start + (cluster as u64 - 2) * self.sectors_per_cluster as u64
    }

    fn valid(&self, cluster: u32) -> bool {
        cluster >= 2 && cluster - 2 < self.clusters
    }

    fn same_name(&self, a: &str, b: &str) -> bool {
        let up = |s: &str| -> Vec<u16> { s.encode_utf16().map(|c| self.upcase[c as usize]).collect() };
        up(a) == up(b)
    }

    /// The root directory: a FAT chain from the boot sector, no stream entry.
    fn root_stream(&self, st: &mut State) -> Res<Stream> {
        let n = self.chain(st, self.root_cluster, u32::MAX)?.len() as u64;
        let len = n * self.cluster_bytes() as u64;
        Ok(Stream { first: self.root_cluster, len, valid: len, contiguous: false })
    }

    fn root(&self, st: &mut State) -> Res<Entry> {
        Ok(Entry { name: String::from("/"), attrs: ATTR_DIR, stream: self.root_stream(st)?, raw: Vec::new(), slot: 0, parent: None })
    }

    fn lookup(&self, st: &mut State, path: &str) -> Res<Entry> {
        let mut cur = self.root(st)?;
        for part in path.split('/').filter(|p| !p.is_empty()) {
            if !cur.is_dir() {
                return Err("not a directory");
            }
            cur = self.read_dir(st, &cur.stream)?.1
                .into_iter()
                .find(|e| self.same_name(&e.name, part))
                .ok_or("file not found")?;
        }
        Ok(cur)
    }

    fn parent_of<'p>(&self, st: &mut State, path: &'p str) -> Res<(Entry, &'p str)> {
        let path = path.trim_end_matches('/');
        let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
        crate::fat::check_name(name)?;
        let parent = self.lookup(st, dir)?;
        if !parent.is_dir() {
            return Err("not a directory");
        }
        Ok((parent, name))
    }

    /// A directory's clusters and the file and folder entry sets in it.
    fn read_dir(&self, st: &mut State, stream: &Stream) -> Res<(Vec<u32>, Vec<Entry>)> {
        let clusters = self.clusters_of(st, stream)?;
        let raw = self.read_clusters(&clusters, stream.len as usize)?;
        let mut entries = Vec::new();
        let total = raw.len() / 32;
        let mut s = 0;
        while s < total {
            let e = &raw[s * 32..s * 32 + 32];
            if e[0] == ENTRY_END {
                break;
            }
            if e[0] != ENTRY_FILE {
                s += 1;
                continue;
            }
            let count = e[1] as usize + 1;
            if count < 3 || s + count > total {
                s += 1;
                continue;
            }
            let set = &raw[s * 32..(s + count) * 32];
            if set[32] != ENTRY_STREAM || u16_at(set, 2) != set_checksum(set) {
                s += 1; // not a valid entry set: skip it, as the spec says
                continue;
            }
            let name_len = set[32 + 3] as usize;
            let mut units = Vec::with_capacity(name_len);
            for n in set[64..].chunks(32).take_while(|n| n[0] == ENTRY_NAME) {
                for i in 0..15 {
                    if units.len() < name_len {
                        units.push(u16_at(n, 2 + 2 * i));
                    }
                }
            }
            let flags = set[32 + 1];
            entries.push(Entry {
                name: char::decode_utf16(units).map(|c| c.unwrap_or('?')).collect(),
                attrs: u16_at(set, 4),
                stream: Stream {
                    first: if flags & FLAG_ALLOC_POSSIBLE != 0 { u32_at(set, 32 + 20) } else { 0 },
                    len: u64_at(set, 32 + 24),
                    valid: u64_at(set, 32 + 8),
                    contiguous: flags & FLAG_NO_FAT_CHAIN != 0,
                },
                raw: set.to_vec(),
                slot: s as u32,
                parent: Some(clusters.clone()),
            });
            s += count;
        }
        Ok((clusters, entries))
    }

    // ---------------------------------------------------------- clusters

    fn fat_get(&self, st: &mut State, cluster: u32) -> Res<u32> {
        let offset = cluster as u64 * 4;
        let sector = self.fat_start + offset / self.bytes_per_sector as u64;
        if st.fat_cache.as_ref().map(|c| c.0) != Some(sector) {
            let mut buf = vec![0u8; self.bytes_per_sector as usize];
            self.dev.read(sector, &mut buf)?;
            st.fat_cache = Some((sector, buf));
        }
        let buf = &st.fat_cache.as_ref().unwrap().1;
        Ok(u32_at(buf, (offset % self.bytes_per_sector as u64) as usize))
    }

    fn fat_set(&self, st: &mut State, mut updates: Vec<(u32, u32)>) -> Res<()> {
        updates.sort_unstable_by_key(|u| u.0);
        let bps = self.bytes_per_sector as u64;
        let mut i = 0;
        while i < updates.len() {
            let sector = updates[i].0 as u64 * 4 / bps;
            let mut buf = vec![0u8; bps as usize];
            self.dev.read(self.fat_start + sector, &mut buf)?;
            while i < updates.len() && updates[i].0 as u64 * 4 / bps == sector {
                let o = (updates[i].0 as u64 * 4 % bps) as usize;
                buf[o..o + 4].copy_from_slice(&updates[i].1.to_le_bytes());
                i += 1;
            }
            self.dev.write(self.fat_start + sector, &buf)?;
        }
        st.fat_cache = None;
        Ok(())
    }

    /// Follows a FAT chain for at most `max` clusters.
    fn chain(&self, st: &mut State, first: u32, max: u32) -> Res<Vec<u32>> {
        let mut out = Vec::new();
        let mut c = first;
        while self.valid(c) && (out.len() as u32) < max {
            out.push(c);
            if out.len() as u32 > self.clusters {
                return Err("cluster chain loops");
            }
            c = self.fat_get(st, c)?;
        }
        Ok(out)
    }

    fn clusters_of(&self, st: &mut State, s: &Stream) -> Res<Vec<u32>> {
        if s.first == 0 || s.len == 0 {
            return Ok(Vec::new());
        }
        let n = s.len.div_ceil(self.cluster_bytes() as u64);
        if n > self.clusters as u64 {
            return Err("file larger than the volume");
        }
        if s.contiguous {
            if !self.valid(s.first) || !self.valid(s.first + n as u32 - 1) {
                return Err("file outside the volume");
            }
            return Ok((s.first..s.first + n as u32).collect());
        }
        let c = self.chain(st, s.first, n as u32)?;
        if (c.len() as u64) < n {
            return Err("cluster chain is broken");
        }
        Ok(c)
    }

    /// The bitmap's and up-case table's clusters: chained in the FAT by
    /// some formatters, merely contiguous by others.
    fn system_clusters(&self, st: &mut State, first: u32, len: u64) -> Res<Vec<u32>> {
        let n = len.div_ceil(self.cluster_bytes() as u64);
        let mut out = Vec::new();
        let mut c = first;
        for _ in 0..n {
            if !self.valid(c) {
                return Err("exFAT system file outside the volume");
            }
            out.push(c);
            let next = self.fat_get(st, c)?;
            c = if self.valid(next) { next } else { c + 1 };
        }
        Ok(out)
    }

    fn read_clusters(&self, clusters: &[u32], limit: usize) -> Res<Vec<u8>> {
        let cb = self.cluster_bytes();
        let mut out = Vec::with_capacity(limit.min(clusters.len() * cb));
        let mut i = 0;
        while i < clusters.len() && out.len() < limit {
            let mut j = i + 1;
            while j < clusters.len() && clusters[j] == clusters[j - 1] + 1 && j - i < 64 {
                j += 1;
            }
            let at = out.len();
            out.resize(at + (j - i) * cb, 0);
            self.dev.read(self.cluster_lba(clusters[i]), &mut out[at..])?;
            i = j;
        }
        out.truncate(limit);
        Ok(out)
    }

    fn read_stream(&self, st: &mut State, s: &Stream, limit: usize) -> Res<Vec<u8>> {
        let clusters = self.clusters_of(st, s)?;
        self.read_clusters(&clusters, limit.min(s.len as usize))
    }

    fn used(st: &State, cluster: u32) -> bool {
        let i = (cluster - 2) as usize;
        st.bitmap[i / 8] & (1 << (i % 8)) != 0
    }

    fn mark(&self, st: &mut State, cluster: u32, used: bool) {
        let i = (cluster - 2) as usize;
        let was = Self::used(st, cluster);
        if used {
            st.bitmap[i / 8] |= 1 << (i % 8);
        } else {
            st.bitmap[i / 8] &= !(1 << (i % 8));
        }
        if was != used {
            if used { st.free -= 1 } else { st.free += 1 }
        }
        st.dirty[i / 8 / self.bytes_per_sector as usize] = true;
    }

    /// Finds `n` free clusters, one run if there is one, without claiming them.
    fn allocate(&self, st: &mut State, n: u32) -> Res<Vec<u32>> {
        if n == 0 {
            return Ok(Vec::new());
        }
        if st.free < n {
            return Err("disk full");
        }
        let start = if self.valid(st.next_free) { st.next_free } else { 2 };
        let order = (start..self.clusters + 2).chain(2..start);
        let mut scattered = Vec::new();
        let mut run: Vec<u32> = Vec::new();
        for c in order {
            if Self::used(st, c) {
                run.clear();
                continue;
            }
            if run.last().is_some_and(|&l| l + 1 != c) {
                run.clear();
            }
            run.push(c);
            if run.len() == n as usize {
                return Ok(run);
            }
            if scattered.len() < n as usize {
                scattered.push(c);
            }
        }
        if scattered.len() == n as usize { Ok(scattered) } else { Err("disk full") }
    }

    /// Writes `data` (zero padded) to `clusters`, records them in the FAT
    /// (unless they are one run) and the bitmap, and describes the result.
    fn claim(&self, st: &mut State, clusters: &[u32], data: &[u8]) -> Res<Stream> {
        if clusters.is_empty() {
            return Ok(Stream { first: 0, len: 0, valid: 0, contiguous: false });
        }
        self.write_clusters(clusters, data)?;
        let contiguous = clusters.windows(2).all(|w| w[1] == w[0] + 1);
        if !contiguous {
            let links = clusters.iter().enumerate().map(|(i, &c)| (c, clusters.get(i + 1).copied().unwrap_or(EOC))).collect();
            self.fat_set(st, links)?;
        }
        for &c in clusters {
            self.mark(st, c, true);
        }
        let last = *clusters.last().unwrap();
        st.next_free = if last + 1 >= self.clusters + 2 { 2 } else { last + 1 };
        self.write_bitmap(st)?;
        let len = data.len() as u64;
        Ok(Stream { first: clusters[0], len, valid: len, contiguous })
    }

    fn release(&self, st: &mut State, s: &Stream) -> Res<()> {
        for c in self.clusters_of(st, s)? {
            self.mark(st, c, false);
        }
        self.write_bitmap(st)
    }

    fn write_clusters(&self, clusters: &[u32], data: &[u8]) -> Res<()> {
        let cb = self.cluster_bytes();
        let mut i = 0;
        while i < clusters.len() {
            let mut j = i + 1;
            while j < clusters.len() && clusters[j] == clusters[j - 1] + 1 && j - i < 64 {
                j += 1;
            }
            let mut buf = vec![0u8; (j - i) * cb];
            let from = (i * cb).min(data.len());
            let to = (j * cb).min(data.len());
            buf[..to - from].copy_from_slice(&data[from..to]);
            self.dev.write(self.cluster_lba(clusters[i]), &buf)?;
            i = j;
        }
        Ok(())
    }

    fn write_bitmap(&self, st: &mut State) -> Res<()> {
        let bps = self.bytes_per_sector as usize;
        let per_cluster = self.sectors_per_cluster as usize;
        for s in 0..st.dirty.len() {
            if !st.dirty[s] {
                continue;
            }
            let mut buf = vec![0u8; bps];
            let from = s * bps;
            let to = (from + bps).min(st.bitmap.len());
            let lba = self.cluster_lba(st.bitmap_clusters[s / per_cluster]) + (s % per_cluster) as u64;
            if to - from < bps {
                self.dev.read(lba, &mut buf)?; // keep whatever follows the bitmap
            }
            buf[..to - from].copy_from_slice(&st.bitmap[from..to]);
            self.dev.write(lba, &buf)?;
            st.dirty[s] = false;
        }
        Ok(())
    }

    /// Writes PercentInUse and flushes the drive's cache.
    fn finish(&self, st: &mut State) -> Res<()> {
        let mut bs = vec![0u8; self.bytes_per_sector as usize];
        self.dev.read(0, &mut bs)?;
        let used = (self.clusters - st.free) as u64;
        let percent = (used * 100 / self.clusters as u64) as u8;
        if bs[112] != 0xFF && bs[112] != percent {
            bs[112] = percent; // not covered by the boot checksum
            self.dev.write(0, &bs)?;
        }
        self.dev.flush()
    }

    // ------------------------------------------------------- directories

    /// Writes an entry set back where it came from, with a fresh checksum.
    fn put_set(&self, st: &mut State, e: &Entry) -> Res<()> {
        let mut raw = e.raw.clone();
        if raw[0] & IN_USE != 0 {
            write_stream(&mut raw, &e.stream);
            let sum = set_checksum(&raw);
            raw[2..4].copy_from_slice(&sum.to_le_bytes());
        }
        let _ = st;
        self.put_slots(e.parent.as_deref().ok_or("cannot change the root")?, e.slot, &raw)
    }

    fn put_slots(&self, clusters: &[u32], first: u32, data: &[u8]) -> Res<()> {
        let cb = self.cluster_bytes();
        let bps = self.bytes_per_sector as usize;
        let mut off = first as usize * 32;
        let mut done = 0;
        let mut sector = vec![0u8; bps];
        while done < data.len() {
            let cluster = *clusters.get(off / cb).ok_or("directory slot out of range")?;
            let within = off % cb;
            let lba = self.cluster_lba(cluster) + (within / bps) as u64;
            let o = within % bps;
            let n = (bps - o).min(data.len() - done);
            self.dev.read(lba, &mut sector)?;
            sector[o..o + n].copy_from_slice(&data[done..done + n]);
            self.dev.write(lba, &sector)?;
            off += n;
            done += n;
        }
        Ok(())
    }

    /// Puts a new entry set in directory `dir`, growing it if no run of free
    /// slots is long enough.
    fn add_set(&self, st: &mut State, dir: &Entry, mut clusters: Vec<u32>, raw: Vec<u8>) -> Res<()> {
        let data = self.read_clusters(&clusters, dir.stream.len as usize)?;
        let need = raw.len() / 32;
        let total = data.len() / 32;
        let mut run_start = 0;
        let mut run = 0;
        for s in 0..total {
            let t = data[s * 32];
            if t == ENTRY_END {
                if run == 0 {
                    run_start = s;
                }
                run += total - s;
                break;
            }
            if t & IN_USE == 0 {
                if run == 0 {
                    run_start = s;
                }
                run += 1;
                if run >= need {
                    break;
                }
            } else {
                run = 0;
            }
        }
        if run < need {
            // Grow by zeroed clusters until the set fits.
            let start = if run > 0 { run_start } else { total };
            let per = self.cluster_bytes() / 32;
            let more = (need - run).div_ceil(per) as u32;
            let added = self.allocate(st, more)?;
            self.write_clusters(&added, &[])?;
            let mut stream = dir.stream;
            let last = *clusters.last().ok_or("directory has no clusters")?;
            let still_contiguous = stream.contiguous && added[0] == last + 1 && added.windows(2).all(|w| w[1] == w[0] + 1);
            if !still_contiguous {
                // Chain everything in the FAT from now on.
                let all: Vec<u32> = clusters.iter().chain(added.iter()).copied().collect();
                let from = if stream.contiguous { 0 } else { clusters.len() - 1 };
                let links = (from..all.len()).map(|i| (all[i], all.get(i + 1).copied().unwrap_or(EOC))).collect();
                self.fat_set(st, links)?;
                stream.contiguous = false;
            }
            for &c in &added {
                self.mark(st, c, true);
            }
            self.write_bitmap(st)?;
            clusters.extend_from_slice(&added);
            stream.len += (added.len() * self.cluster_bytes()) as u64;
            stream.valid = stream.len;
            if dir.parent.is_some() {
                let mut d = dir.clone();
                d.stream = stream;
                self.put_set(st, &d)?;
            }
            return self.put_slots(&clusters, start as u32, &raw);
        }
        self.put_slots(&clusters, run_start as u32, &raw)
    }
}

impl vfs::Volume for ExfatVolume {
    fn kind(&self) -> &'static str {
        "exFAT"
    }
    fn space(&self) -> (u64, Option<u64>) {
        let cluster = self.bytes_per_sector as u64 * self.sectors_per_cluster as u64;
        (self.clusters as u64 * cluster, Some(self.state.lock().free as u64 * cluster))
    }
    fn label(&self) -> &str {
        &self.label
    }
    fn dev(&self) -> &Arc<dyn BlockDevice> {
        &self.dev
    }
    fn list(&self, path: &str) -> Res<Vec<vfs::DirEntry>> {
        self.list_entries(path)
    }
    fn read_file(&self, path: &str, limit: usize) -> Res<Vec<u8>> {
        self.read(path, limit)
    }
    fn write_file(&self, path: &str, data: &[u8]) -> Res<()> {
        self.write(path, data)
    }
    fn create_dir(&self, path: &str) -> Res<()> {
        self.mkdir(path)
    }
    fn remove(&self, path: &str) -> Res<()> {
        self.delete(path)
    }
}

// ------------------------------------------------------------ entry sets

fn set_checksum(set: &[u8]) -> u16 {
    let mut sum: u16 = 0;
    for (i, &b) in set.iter().enumerate() {
        if i == 2 || i == 3 {
            continue;
        }
        sum = (sum >> 1 | sum << 15).wrapping_add(b as u16);
    }
    sum
}

fn name_hash(units: &[u16], upcase: &[u16]) -> u16 {
    let mut hash: u16 = 0;
    for &u in units {
        for b in upcase[u as usize].to_le_bytes() {
            hash = (hash >> 1 | hash << 15).wrapping_add(b as u16);
        }
    }
    hash
}

/// exFAT timestamps are the FAT date and time words in one u32.
fn timestamp() -> u32 {
    let (time, date) = crate::fat::timestamp();
    (date as u32) << 16 | time as u32
}

fn write_stream(set: &mut [u8], s: &Stream) {
    let st = &mut set[32..64];
    st[1] = if s.first != 0 { FLAG_ALLOC_POSSIBLE } else { 0 } | if s.contiguous { FLAG_NO_FAT_CHAIN } else { 0 };
    st[8..16].copy_from_slice(&s.valid.to_le_bytes());
    st[20..24].copy_from_slice(&s.first.to_le_bytes());
    st[24..32].copy_from_slice(&s.len.to_le_bytes());
}

/// A file entry, stream extension and name entries for a new name.
fn new_set(name: &str, attrs: u16, stream: &Stream, upcase: &[u16]) -> Vec<u8> {
    let units: Vec<u16> = name.encode_utf16().collect();
    let names = units.len().div_ceil(15);
    let mut set = vec![0u8; 32 * (2 + names)];
    let now = timestamp();
    set[0] = ENTRY_FILE;
    set[1] = (1 + names) as u8;
    set[4..6].copy_from_slice(&attrs.to_le_bytes());
    for o in [8, 12, 16] {
        set[o..o + 4].copy_from_slice(&now.to_le_bytes()); // created, modified, accessed
    }
    set[32] = ENTRY_STREAM;
    set[32 + 3] = units.len() as u8;
    set[32 + 4..32 + 6].copy_from_slice(&name_hash(&units, upcase).to_le_bytes());
    write_stream(&mut set, stream);
    for (i, chunk) in units.chunks(15).enumerate() {
        let e = &mut set[64 + 32 * i..96 + 32 * i];
        e[0] = ENTRY_NAME;
        for (k, &u) in chunk.iter().enumerate() {
            e[2 + 2 * k..4 + 2 * k].copy_from_slice(&u.to_le_bytes());
        }
    }
    let sum = set_checksum(&set);
    set[2..4].copy_from_slice(&sum.to_le_bytes());
    set
}
