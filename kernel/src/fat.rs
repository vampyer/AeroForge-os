//! FAT32 filesystem with long (VFAT) file names. Name lookups are
//! case-insensitive, like Windows. Files can be created, overwritten and
//! deleted, and directories created and removed.
//!
//! First filesystem in the kernel: every UEFI PC already has one (the EFI
//! system partition), and it is the simplest way to put files on a disk image.
//!
//! Updates go in the order data, FAT, directory entry, so a crash part way
//! leaves at worst lost clusters (which a disk check frees), never a file
//! pointing at someone else's data. Each volume has one lock: one update or
//! read at a time.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use crate::block::BlockDevice;
use crate::sync::SleepMutex;
use crate::vfs;

pub struct FatVolume {
    pub dev: Arc<dyn BlockDevice>,
    bytes_per_sector: u32,
    sectors_per_cluster: u32,
    fat_start: u64,
    fats: u32,
    fat_size: u64,
    data_start: u64,
    root_cluster: u32,
    /// Data clusters: valid cluster numbers are 2..clusters + 2.
    clusters: u32,
    fsinfo: Option<u64>,
    label: String,
    state: SleepMutex<State>,
}

/// What the volume lock guards: the allocation hints and a one-sector FAT cache.
struct State {
    next_free: u32,
    /// Free clusters, when the FSInfo sector gave a believable count.
    free: Option<u32>,
    fat_cache: Option<(u64, Vec<u8>)>,
}

#[derive(Clone, Debug)]
struct DirEntry {
    name: String,
    is_dir: bool,
    size: u32,
    /// Last written, as rtc::DateTime::packed (0: unknown).
    modified: u64,
    cluster: u32,
    short: [u8; 11],
    /// Index of the 8.3 entry in its directory, and how many long-name entries precede it.
    slot: u32,
    lfn: u32,
}

const END_OF_CHAIN: u32 = 0x0FFF_FFF8;
const EOC_MARK: u32 = 0x0FFF_FFFF;
const ATTR_DIR: u8 = 0x10;
const ATTR_ARCHIVE: u8 = 0x20;
const ATTR_LFN: u8 = 0x0F;
const FREE_UNKNOWN: u32 = 0xFFFF_FFFF;

type Res<T> = Result<T, &'static str>;

impl FatVolume {
    pub fn mount(dev: Arc<dyn BlockDevice>) -> Res<Self> {
        let mut bs = vec![0u8; dev.block_size() as usize];
        dev.read(0, &mut bs)?;
        let u16_at = |o: usize| u16::from_le_bytes([bs[o], bs[o + 1]]) as u32;
        let u32_at = |o: usize| u32::from_le_bytes(bs[o..o + 4].try_into().unwrap());
        if bs[510] != 0x55 || bs[511] != 0xAA || &bs[82..87] != b"FAT32" {
            return Err("not a FAT32 volume");
        }
        let bytes_per_sector = u16_at(11);
        if bytes_per_sector != dev.block_size() {
            return Err("sector size differs from the device block size");
        }
        let sectors_per_cluster = bs[13] as u32;
        let reserved = u16_at(14) as u64;
        let fats = bs[16] as u32;
        let fat_size = u32_at(36) as u64;
        let total = if u16_at(19) != 0 { u16_at(19) as u64 } else { u32_at(32) as u64 };
        let data_start = reserved + fats as u64 * fat_size;
        if sectors_per_cluster == 0 || fats == 0 || total <= data_start {
            return Err("bad FAT32 boot sector");
        }
        // The FAT itself must have room for every cluster the size implies.
        let clusters = ((total - data_start) / sectors_per_cluster as u64)
            .min(fat_size * bytes_per_sector as u64 / 4 - 2)
            .min(0x0FFF_FFF5) as u32;
        let label = String::from(core::str::from_utf8(&bs[71..82]).unwrap_or("").trim());

        // FSInfo keeps the free count and where to look for free clusters.
        let mut state = State { next_free: 2, free: None, fat_cache: None };
        let fsinfo_sector = u16_at(48) as u64;
        let mut fsinfo = None;
        if fsinfo_sector != 0 && fsinfo_sector != 0xFFFF && fsinfo_sector < reserved {
            let mut fi = vec![0u8; bytes_per_sector as usize];
            dev.read(fsinfo_sector, &mut fi)?;
            let at = |o: usize| u32::from_le_bytes(fi[o..o + 4].try_into().unwrap());
            if at(0) == 0x4161_5252 && at(484) == 0x6141_7272 {
                fsinfo = Some(fsinfo_sector);
                if at(488) <= clusters {
                    state.free = Some(at(488));
                }
                if (2..clusters + 2).contains(&at(492)) {
                    state.next_free = at(492);
                }
            }
        }
        Ok(Self {
            dev,
            bytes_per_sector,
            sectors_per_cluster,
            fat_start: reserved,
            fats,
            fat_size,
            data_start,
            root_cluster: u32_at(44),
            clusters,
            fsinfo,
            label,
            state: SleepMutex::new(state),
        })
    }

    // ------------------------------------------------------------ public

    pub fn list(&self, path: &str) -> Res<Vec<vfs::DirEntry>> {
        let st = &mut *self.state.lock();
        let dir = self.lookup(st, path)?;
        let entries = if dir.is_dir { parse_dir(&self.read_dir_raw(st, dir.cluster)?.1) } else { vec![dir] };
        Ok(entries.into_iter().map(|e| vfs::DirEntry { name: e.name, is_dir: e.is_dir, size: e.size as u64, modified: e.modified }).collect())
    }

    /// Reads up to `limit` bytes of a file.
    pub fn read_file(&self, path: &str, limit: usize) -> Res<Vec<u8>> {
        let st = &mut *self.state.lock();
        let f = self.lookup(st, path)?;
        if f.is_dir {
            return Err("is a directory");
        }
        if f.size == 0 {
            return Ok(Vec::new());
        }
        self.read_chain(st, f.cluster, limit.min(f.size as usize))
    }

    /// Creates the file at `path`, or replaces its contents if it exists.
    pub fn write_file(&self, path: &str, data: &[u8]) -> Res<()> {
        if data.len() > u32::MAX as usize {
            return Err("file too large for FAT32");
        }
        let st = &mut *self.state.lock();
        let (parent, name) = self.parent_of(st, path)?;
        let (chain, raw) = self.read_dir_raw(st, parent.cluster)?;
        let existing = parse_dir(&raw).into_iter().find(|e| e.name.eq_ignore_ascii_case(name));
        if existing.as_ref().is_some_and(|e| e.is_dir) {
            return Err("is a directory");
        }
        let need = data.len().div_ceil(self.cluster_bytes()) as u32;
        let new = match self.find_free(st, need) {
            Ok(c) => c,
            // Full: the old contents may be what is in the way.
            Err(e) => match &existing {
                Some(old) if old.cluster >= 2 => {
                    self.free_chain(st, old.cluster)?;
                    let mut entry = slot_bytes(&raw, old.slot);
                    set_cluster(&mut entry, 0);
                    entry[28..32].fill(0);
                    self.put_slots(&chain, old.slot, &entry)?;
                    self.find_free(st, need)?
                }
                _ => return Err(e),
            },
        };
        self.write_clusters(&new, data)?;
        self.link(st, &new)?;
        let first = new.first().copied().unwrap_or(0);
        let (time, date) = timestamp();
        match existing {
            Some(old) => {
                // Re-read: the entry may have been changed above.
                let raw = self.read_dir_raw(st, parent.cluster)?.1;
                let mut entry = slot_bytes(&raw, old.slot);
                let old_cluster = cluster_of(&entry);
                set_cluster(&mut entry, first);
                entry[11] |= ATTR_ARCHIVE;
                entry[18..20].copy_from_slice(&date.to_le_bytes());
                entry[22..24].copy_from_slice(&time.to_le_bytes());
                entry[24..26].copy_from_slice(&date.to_le_bytes());
                entry[28..32].copy_from_slice(&(data.len() as u32).to_le_bytes());
                self.put_slots(&chain, old.slot, &entry)?;
                if old_cluster >= 2 {
                    self.free_chain(st, old_cluster)?;
                }
            }
            None => self.add_entry(st, &parent, name, ATTR_ARCHIVE, first, data.len() as u32)?,
        }
        self.finish(st)
    }

    /// Creates an empty directory at `path`.
    pub fn create_dir(&self, path: &str) -> Res<()> {
        let st = &mut *self.state.lock();
        let (parent, name) = self.parent_of(st, path)?;
        let raw = self.read_dir_raw(st, parent.cluster)?.1;
        if parse_dir(&raw).iter().any(|e| e.name.eq_ignore_ascii_case(name)) {
            return Err("already exists");
        }
        let cluster = self.find_free(st, 1)?;
        let (time, date) = timestamp();
        let mut block = vec![0u8; self.cluster_bytes()];
        let parent_cluster = if parent.cluster == self.root_cluster { 0 } else { parent.cluster };
        block[..32].copy_from_slice(&short_entry(b".          ", 0, ATTR_DIR, cluster[0], 0, time, date));
        block[32..64].copy_from_slice(&short_entry(b"..         ", 0, ATTR_DIR, parent_cluster, 0, time, date));
        self.write_clusters(&cluster, &block)?;
        self.link(st, &cluster)?;
        self.add_entry(st, &parent, name, ATTR_DIR, cluster[0], 0)?;
        self.finish(st)
    }

    /// Deletes the file, or the empty directory, at `path`.
    pub fn remove(&self, path: &str) -> Res<()> {
        let st = &mut *self.state.lock();
        let (parent, name) = self.parent_of(st, path)?;
        let (chain, raw) = self.read_dir_raw(st, parent.cluster)?;
        let e = parse_dir(&raw).into_iter().find(|e| e.name.eq_ignore_ascii_case(name)).ok_or("file not found")?;
        if e.is_dir && e.cluster >= 2 && !parse_dir(&self.read_dir_raw(st, e.cluster)?.1).is_empty() {
            return Err("directory not empty");
        }
        let first = e.slot - e.lfn;
        let mut slots = Vec::with_capacity(32 * (e.lfn as usize + 1));
        for s in first..=e.slot {
            let mut b = slot_bytes(&raw, s);
            b[0] = 0xE5;
            slots.extend_from_slice(&b);
        }
        self.put_slots(&chain, first, &slots)?;
        if e.cluster >= 2 {
            self.free_chain(st, e.cluster)?;
        }
        self.finish(st)
    }

    // ----------------------------------------------------------- lookups

    fn cluster_bytes(&self) -> usize {
        (self.bytes_per_sector * self.sectors_per_cluster) as usize
    }

    fn cluster_lba(&self, cluster: u32) -> u64 {
        self.data_start + (cluster as u64 - 2) * self.sectors_per_cluster as u64
    }

    fn valid(&self, cluster: u32) -> bool {
        (2..self.clusters + 2).contains(&cluster)
    }

    fn root(&self) -> DirEntry {
        DirEntry { name: String::from("/"), is_dir: true, size: 0, modified: 0, cluster: self.root_cluster, short: [b' '; 11], slot: 0, lfn: 0 }
    }

    fn lookup(&self, st: &mut State, path: &str) -> Res<DirEntry> {
        let mut cur = self.root();
        for part in path.split('/').filter(|p| !p.is_empty()) {
            if !cur.is_dir {
                return Err("not a directory");
            }
            cur = parse_dir(&self.read_dir_raw(st, cur.cluster)?.1)
                .into_iter()
                .find(|e| e.name.eq_ignore_ascii_case(part))
                .ok_or("file not found")?;
        }
        Ok(cur)
    }

    /// The directory `path` lives in, and its last component (checked as a file name).
    fn parent_of<'p>(&self, st: &mut State, path: &'p str) -> Res<(DirEntry, &'p str)> {
        let path = path.trim_end_matches('/');
        let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
        check_name(name)?;
        let parent = self.lookup(st, dir)?;
        if !parent.is_dir {
            return Err("not a directory");
        }
        Ok((parent, name))
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
        let o = (offset % self.bytes_per_sector as u64) as usize;
        Ok(u32::from_le_bytes(buf[o..o + 4].try_into().unwrap()) & 0x0FFF_FFFF)
    }

    /// Sets FAT entries, a sector at a time, in every copy of the FAT.
    fn fat_set(&self, st: &mut State, mut updates: Vec<(u32, u32)>) -> Res<()> {
        updates.sort_unstable_by_key(|u| u.0);
        let bps = self.bytes_per_sector as u64;
        let mut i = 0;
        while i < updates.len() {
            let sector = updates[i].0 as u64 * 4 / bps;
            let mut buf = vec![0u8; bps as usize];
            self.dev.read(self.fat_start + sector, &mut buf)?;
            while i < updates.len() && updates[i].0 as u64 * 4 / bps == sector {
                let (cluster, value) = updates[i];
                let o = (cluster as u64 * 4 % bps) as usize;
                let old = u32::from_le_bytes(buf[o..o + 4].try_into().unwrap());
                // The top four bits are reserved and must be kept.
                buf[o..o + 4].copy_from_slice(&((old & 0xF000_0000) | (value & 0x0FFF_FFFF)).to_le_bytes());
                i += 1;
            }
            for f in 0..self.fats as u64 {
                self.dev.write(self.fat_start + f * self.fat_size + sector, &buf)?;
            }
        }
        st.fat_cache = None;
        Ok(())
    }

    fn chain(&self, st: &mut State, start: u32) -> Res<Vec<u32>> {
        let mut out = Vec::new();
        let mut cluster = start;
        while self.valid(cluster) {
            out.push(cluster);
            if out.len() > self.clusters as usize {
                return Err("cluster chain loops");
            }
            cluster = self.fat_get(st, cluster)?;
        }
        if cluster < END_OF_CHAIN && !out.is_empty() {
            return Err("cluster chain is broken");
        }
        Ok(out)
    }

    /// Reads a cluster chain, stopping after `limit` bytes.
    fn read_chain(&self, st: &mut State, start: u32, limit: usize) -> Res<Vec<u8>> {
        let mut out = Vec::new();
        let mut cluster = start;
        let mut buf = vec![0u8; self.cluster_bytes()];
        let mut hops = 0;
        while self.valid(cluster) && out.len() < limit {
            self.dev.read(self.cluster_lba(cluster), &mut buf)?;
            out.extend_from_slice(&buf);
            cluster = self.fat_get(st, cluster)?;
            hops += 1;
            if hops > self.clusters {
                return Err("cluster chain loops");
            }
        }
        out.truncate(limit);
        Ok(out)
    }

    /// Finds `n` free clusters without claiming them.
    fn find_free(&self, st: &mut State, n: u32) -> Res<Vec<u32>> {
        if st.free.is_some_and(|f| f < n) {
            return Err("disk full");
        }
        let mut found = Vec::with_capacity(n as usize);
        let start = if self.valid(st.next_free) { st.next_free } else { 2 };
        let mut c = start;
        for _ in 0..self.clusters {
            if found.len() == n as usize {
                break;
            }
            if self.fat_get(st, c)? == 0 {
                found.push(c);
            }
            c = if c + 1 >= self.clusters + 2 { 2 } else { c + 1 };
        }
        if found.len() < n as usize {
            st.free = Some(found.len() as u32);
            return Err("disk full");
        }
        Ok(found)
    }

    /// Claims clusters found by `find_free`, chaining them in order.
    fn link(&self, st: &mut State, clusters: &[u32]) -> Res<()> {
        if clusters.is_empty() {
            return Ok(());
        }
        let updates = clusters.iter().enumerate()
            .map(|(i, &c)| (c, clusters.get(i + 1).copied().unwrap_or(EOC_MARK)))
            .collect();
        self.fat_set(st, updates)?;
        let last = *clusters.last().unwrap();
        st.next_free = if last + 1 >= self.clusters + 2 { 2 } else { last + 1 };
        if let Some(f) = st.free.as_mut() {
            *f -= clusters.len() as u32;
        }
        Ok(())
    }

    fn free_chain(&self, st: &mut State, start: u32) -> Res<()> {
        let chain = self.chain(st, start)?;
        let n = chain.len() as u32;
        self.fat_set(st, chain.into_iter().map(|c| (c, 0)).collect())?;
        if let Some(f) = st.free.as_mut() {
            *f = (*f + n).min(self.clusters);
        }
        Ok(())
    }

    /// Writes `data` (zero padded) across `clusters`, a run of neighbours at a time.
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

    /// Writes FSInfo back and flushes the drive's cache.
    fn finish(&self, st: &mut State) -> Res<()> {
        if let Some(sector) = self.fsinfo {
            let mut fi = vec![0u8; self.bytes_per_sector as usize];
            self.dev.read(sector, &mut fi)?;
            fi[488..492].copy_from_slice(&st.free.unwrap_or(FREE_UNKNOWN).to_le_bytes());
            fi[492..496].copy_from_slice(&st.next_free.to_le_bytes());
            self.dev.write(sector, &fi)?;
        }
        self.dev.flush()
    }

    // ------------------------------------------------------- directories

    /// A directory's cluster chain and its raw contents.
    fn read_dir_raw(&self, st: &mut State, cluster: u32) -> Res<(Vec<u32>, Vec<u8>)> {
        let chain = self.chain(st, cluster)?;
        let mut raw = vec![0u8; chain.len() * self.cluster_bytes()];
        for (i, &c) in chain.iter().enumerate() {
            let cb = self.cluster_bytes();
            self.dev.read(self.cluster_lba(c), &mut raw[i * cb..(i + 1) * cb])?;
        }
        Ok((chain, raw))
    }

    /// Writes 32-byte directory slots starting at slot `first`.
    fn put_slots(&self, chain: &[u32], first: u32, data: &[u8]) -> Res<()> {
        let cb = self.cluster_bytes();
        let bps = self.bytes_per_sector as usize;
        let mut off = first as usize * 32;
        let mut done = 0;
        let mut sector = vec![0u8; bps];
        while done < data.len() {
            let cluster = *chain.get(off / cb).ok_or("directory slot out of range")?;
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

    /// Adds a name to directory `parent`, with long-name entries when the
    /// name is not a plain 8.3 one, growing the directory if it is full.
    fn add_entry(&self, st: &mut State, parent: &DirEntry, name: &str, attr: u8, cluster: u32, size: u32) -> Res<()> {
        let (mut chain, raw) = self.read_dir_raw(st, parent.cluster)?;
        let existing: Vec<[u8; 11]> = parse_dir(&raw).iter().map(|e| e.short).collect();
        let (short, case, lfn) = match exact_short(name) {
            Some((short, case)) if !existing.contains(&short) => (short, case, Vec::new()),
            _ => {
                let short = basis_short(name, &existing).ok_or("too many similar names")?;
                (short, 0, lfn_entries(name, checksum(&short)))
            }
        };
        let (time, date) = timestamp();
        let mut slots = Vec::with_capacity(lfn.len() * 32 + 32);
        for l in &lfn {
            slots.extend_from_slice(l);
        }
        slots.extend_from_slice(&short_entry(&short, case, attr, cluster, size, time, date));

        // A run of free slots long enough; past the end marker every slot is free.
        let need = slots.len() / 32;
        let total = raw.len() / 32;
        let mut run_start = 0;
        let mut run = 0;
        let mut at = None;
        for s in 0..total {
            let b = raw[s * 32];
            if b == 0x00 {
                if run == 0 {
                    run_start = s;
                }
                run += total - s;
                break;
            }
            if b == 0xE5 {
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
        if run >= need {
            at = Some(run_start);
        }
        let first = match at {
            Some(s) => s,
            None => {
                // Grow the directory by zeroed clusters until the name fits.
                let start = if run > 0 { run_start } else { total };
                let per = self.cluster_bytes() / 32;
                let more = (need - run).div_ceil(per) as u32;
                let added = self.find_free(st, more)?;
                self.write_clusters(&added, &[])?;
                self.link(st, &added)?;
                self.fat_set(st, vec![(*chain.last().unwrap(), added[0])])?;
                chain.extend_from_slice(&added);
                start
            }
        };
        self.put_slots(&chain, first as u32, &slots)
    }
}

// ------------------------------------------------------- directory entries

fn slot_bytes(raw: &[u8], slot: u32) -> [u8; 32] {
    raw[slot as usize * 32..slot as usize * 32 + 32].try_into().unwrap()
}

fn cluster_of(e: &[u8]) -> u32 {
    (u16::from_le_bytes([e[20], e[21]]) as u32) << 16 | u16::from_le_bytes([e[26], e[27]]) as u32
}

fn set_cluster(e: &mut [u8], cluster: u32) {
    e[20..22].copy_from_slice(&((cluster >> 16) as u16).to_le_bytes());
    e[26..28].copy_from_slice(&(cluster as u16).to_le_bytes());
}

fn parse_dir(raw: &[u8]) -> Vec<DirEntry> {
    let mut entries = Vec::new();
    let mut lfn: Vec<(u8, [u16; 13])> = Vec::new();
    for (slot, e) in raw.chunks(32).enumerate() {
        match e[0] {
            0x00 => break,          // end of directory
            0xE5 => { lfn.clear(); continue } // deleted
            _ => {}
        }
        let attr = e[11];
        if attr == ATTR_LFN {
            let mut part = [0u16; 13];
            for (i, o) in [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30].iter().enumerate() {
                part[i] = u16::from_le_bytes([e[*o], e[*o + 1]]);
            }
            if e[0] & 0x40 != 0 {
                lfn.clear(); // the first (last-part) entry of a new long name
            }
            lfn.push((e[0] & 0x1F, part));
            continue;
        }
        if attr & 0x08 != 0 {
            lfn.clear(); // volume label
            continue;
        }
        let name = if lfn.is_empty() {
            short_name(e)
        } else {
            let mut parts = lfn.clone();
            parts.sort_by_key(|p| p.0);
            let units: Vec<u16> = parts.iter().flat_map(|p| p.1).take_while(|&c| c != 0 && c != 0xFFFF).collect();
            char::decode_utf16(units).map(|c| c.unwrap_or('?')).collect()
        };
        let lfn_count = lfn.len() as u32;
        lfn.clear();
        if name == "." || name == ".." {
            continue;
        }
        entries.push(DirEntry {
            name,
            is_dir: attr & ATTR_DIR != 0,
            size: u32::from_le_bytes(e[28..32].try_into().unwrap()),
            modified: crate::rtc::DateTime::from_dos(u16::from_le_bytes([e[24], e[25]]), u16::from_le_bytes([e[22], e[23]]))
                .map_or(0, |t| t.packed()),
            cluster: cluster_of(e),
            short: e[0..11].try_into().unwrap(),
            slot: slot as u32,
            lfn: lfn_count,
        });
    }
    entries
}

fn short_name(e: &[u8]) -> String {
    let lower_base = e[12] & 0x08 != 0;
    let lower_ext = e[12] & 0x10 != 0;
    let conv = |b: &[u8], lower: bool| -> String {
        let s = core::str::from_utf8(b).unwrap_or("?").trim_end();
        if lower { s.to_ascii_lowercase() } else { String::from(s) }
    };
    let base = conv(&e[0..8], lower_base);
    let ext = conv(&e[8..11], lower_ext);
    if ext.is_empty() { base } else { alloc::format!("{}.{}", base, ext) }
}

fn short_entry(short: &[u8; 11], case: u8, attr: u8, cluster: u32, size: u32, time: u16, date: u16) -> [u8; 32] {
    let mut e = [0u8; 32];
    e[0..11].copy_from_slice(short);
    e[11] = attr;
    e[12] = case;
    e[14..16].copy_from_slice(&time.to_le_bytes()); // created
    e[16..18].copy_from_slice(&date.to_le_bytes());
    e[18..20].copy_from_slice(&date.to_le_bytes()); // last access
    e[22..24].copy_from_slice(&time.to_le_bytes()); // modified
    e[24..26].copy_from_slice(&date.to_le_bytes());
    set_cluster(&mut e, cluster);
    e[28..32].copy_from_slice(&size.to_le_bytes());
    e
}

/// FAT time and date words for now (1980-01-01 if the clock can't be read).
pub fn timestamp() -> (u16, u16) {
    match crate::rtc::now() {
        Some(t) if (1980..2108).contains(&t.year) => (
            (t.hour as u16) << 11 | (t.minute as u16) << 5 | (t.second as u16 / 2),
            (t.year - 1980) << 9 | (t.month as u16) << 5 | t.day as u16,
        ),
        _ => (0, 1 << 5 | 1),
    }
}

// ------------------------------------------------------------------- names

/// Windows' rules: no control characters or "*/:<>?\|, not just dots or
/// spaces, no trailing dot or space, at most 255 UTF-16 units.
pub fn check_name(name: &str) -> Res<()> {
    if name.is_empty() || name.trim_matches(|c| c == '.' || c == ' ').is_empty() {
        return Err("bad file name");
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return Err("bad file name");
    }
    if name.chars().any(|c| (c as u32) < 0x20 || "\"*/:<>?\\|".contains(c)) {
        return Err("bad file name");
    }
    if name.encode_utf16().count() > 255 {
        return Err("file name too long");
    }
    Ok(())
}

fn short_char_ok(c: u8) -> bool {
    c.is_ascii_uppercase() || c.is_ascii_digit() || b"$%'-_@~`!(){}^#&".contains(&c)
}

/// The 8.3 name and case flags if `name` needs no long name: one dot at
/// most, base of 1-8 and extension of 0-3 legal characters, and each part
/// all upper or all lower case (flags 0x08 and 0x10, as Windows writes).
fn exact_short(name: &str) -> Option<([u8; 11], u8)> {
    let (base, ext) = name.rsplit_once('.').unwrap_or((name, ""));
    if base.is_empty() || base.len() > 8 || ext.len() > 3 || base.contains('.') {
        return None;
    }
    let mut short = [b' '; 11];
    let mut case = 0;
    for (part, at, flag) in [(base, 0, 0x08), (ext, 8, 0x10)] {
        let lower = part.bytes().any(|c| c.is_ascii_lowercase());
        let upper = part.bytes().any(|c| c.is_ascii_uppercase());
        if lower && upper {
            return None;
        }
        if lower {
            case |= flag;
        }
        for (i, c) in part.bytes().enumerate() {
            let c = c.to_ascii_uppercase();
            if !short_char_ok(c) {
                return None;
            }
            short[at + i] = c;
        }
    }
    Some((short, case))
}

/// A unique 8.3 alias for a long name, "BASIS~N.EXT" as Windows makes them.
fn basis_short(name: &str, existing: &[[u8; 11]]) -> Option<[u8; 11]> {
    let clean = |s: &str| -> Vec<u8> {
        s.chars()
            .filter(|&c| c != ' ' && c != '.')
            .map(|c| {
                let u = c.to_ascii_uppercase();
                if u.is_ascii() && short_char_ok(u as u8) { u as u8 } else { b'_' }
            })
            .collect()
    };
    let trimmed = name.trim_start_matches('.');
    let (base, ext) = match trimmed.rsplit_once('.') {
        Some((b, e)) if !b.is_empty() => (clean(b), clean(e)),
        _ => (clean(trimmed), Vec::new()),
    };
    let base = if base.is_empty() { vec![b'_'] } else { base };
    for n in 1..1_000_000u32 {
        let tail = alloc::format!("~{}", n);
        let keep = base.len().min(8 - tail.len());
        let mut short = [b' '; 11];
        short[..keep].copy_from_slice(&base[..keep]);
        short[keep..keep + tail.len()].copy_from_slice(tail.as_bytes());
        for (i, &c) in ext.iter().take(3).enumerate() {
            short[8 + i] = c;
        }
        if !existing.contains(&short) {
            return Some(short);
        }
    }
    None
}

fn checksum(short: &[u8; 11]) -> u8 {
    short.iter().fold(0u8, |sum, &c| (sum >> 1 | sum << 7).wrapping_add(c))
}

/// Long-name entries in on-disk order (the last part first).
fn lfn_entries(name: &str, sum: u8) -> Vec<[u8; 32]> {
    let mut units: Vec<u16> = name.encode_utf16().collect();
    let count = units.len().div_ceil(13);
    if units.len() % 13 != 0 {
        units.push(0);
    }
    units.resize(count * 13, 0xFFFF);
    let mut out = Vec::with_capacity(count);
    for seq in (1..=count).rev() {
        let mut e = [0u8; 32];
        e[0] = seq as u8 | if seq == count { 0x40 } else { 0 };
        e[11] = ATTR_LFN;
        e[13] = sum;
        for (i, o) in [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30].iter().enumerate() {
            e[*o..*o + 2].copy_from_slice(&units[(seq - 1) * 13 + i].to_le_bytes());
        }
        out.push(e);
    }
    out
}

impl vfs::Volume for FatVolume {
    fn kind(&self) -> &'static str {
        "FAT32"
    }
    fn space(&self) -> (u64, Option<u64>) {
        let cluster = self.bytes_per_sector as u64 * self.sectors_per_cluster as u64;
        (self.clusters as u64 * cluster, self.state.lock().free.map(|f| f as u64 * cluster))
    }
    fn label(&self) -> &str {
        &self.label
    }
    fn dev(&self) -> &Arc<dyn BlockDevice> {
        &self.dev
    }
    fn list(&self, path: &str) -> Res<Vec<vfs::DirEntry>> {
        FatVolume::list(self, path)
    }
    fn read_file(&self, path: &str, limit: usize) -> Res<Vec<u8>> {
        FatVolume::read_file(self, path, limit)
    }
    fn write_file(&self, path: &str, data: &[u8]) -> Res<()> {
        FatVolume::write_file(self, path, data)
    }
    fn create_dir(&self, path: &str) -> Res<()> {
        FatVolume::create_dir(self, path)
    }
    fn remove(&self, path: &str) -> Res<()> {
        FatVolume::remove(self, path)
    }
}
