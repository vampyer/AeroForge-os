//! FAT32 filesystem, read-only, with long (VFAT) file names. Name lookups
//! are case-insensitive, like Windows.
//!
//! First filesystem in the kernel: every UEFI PC already has one (the EFI
//! system partition), and it is the simplest way to put files on a disk image.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use crate::block::BlockDevice;

pub struct FatVolume {
    pub dev: Arc<dyn BlockDevice>,
    bytes_per_sector: u32,
    sectors_per_cluster: u32,
    fat_start: u64,
    data_start: u64,
    root_cluster: u32,
    pub label: String,
}

#[derive(Clone, Debug)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u32,
    cluster: u32,
}

impl DirEntry {
    /// A directory entry standing for another volume's mount point.
    pub fn mount_point(name: &str) -> Self {
        Self { name: String::from(name), is_dir: true, size: 0, cluster: 0 }
    }
}

const END_OF_CHAIN: u32 = 0x0FFF_FFF8;

impl FatVolume {
    pub fn mount(dev: Arc<dyn BlockDevice>) -> Result<Self, &'static str> {
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
        let fats = bs[16] as u64;
        let fat_size = u32_at(36) as u64;
        let label = String::from(core::str::from_utf8(&bs[71..82]).unwrap_or("").trim());
        Ok(Self {
            dev,
            bytes_per_sector,
            sectors_per_cluster,
            fat_start: reserved,
            data_start: reserved + fats * fat_size,
            root_cluster: u32_at(44),
            label,
        })
    }

    fn cluster_bytes(&self) -> usize {
        (self.bytes_per_sector * self.sectors_per_cluster) as usize
    }

    fn next_cluster(&self, cluster: u32) -> Result<u32, &'static str> {
        let offset = cluster as u64 * 4;
        let sector = self.fat_start + offset / self.bytes_per_sector as u64;
        let mut buf = vec![0u8; self.bytes_per_sector as usize];
        self.dev.read(sector, &mut buf)?;
        let o = (offset % self.bytes_per_sector as u64) as usize;
        Ok(u32::from_le_bytes(buf[o..o + 4].try_into().unwrap()) & 0x0FFF_FFFF)
    }

    /// Reads a cluster chain, stopping after `limit` bytes.
    fn read_chain(&self, start: u32, limit: usize) -> Result<Vec<u8>, &'static str> {
        let mut out = Vec::new();
        let mut cluster = start;
        let mut buf = vec![0u8; self.cluster_bytes()];
        let mut hops = 0;
        while (2..END_OF_CHAIN).contains(&cluster) && out.len() < limit {
            let lba = self.data_start + (cluster as u64 - 2) * self.sectors_per_cluster as u64;
            self.dev.read(lba, &mut buf)?;
            out.extend_from_slice(&buf);
            cluster = self.next_cluster(cluster)?;
            hops += 1;
            if hops > 1_000_000 {
                return Err("cluster chain loops");
            }
        }
        out.truncate(limit);
        Ok(out)
    }

    fn read_dir(&self, cluster: u32) -> Result<Vec<DirEntry>, &'static str> {
        let raw = self.read_chain(cluster, usize::MAX)?;
        let mut entries = Vec::new();
        let mut lfn: Vec<(u8, [u16; 13])> = Vec::new();
        for e in raw.chunks(32) {
            match e[0] {
                0x00 => break,          // end of directory
                0xE5 => { lfn.clear(); continue } // deleted
                _ => {}
            }
            let attr = e[11];
            if attr == 0x0F {
                let mut part = [0u16; 13];
                for (i, o) in [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30].iter().enumerate() {
                    part[i] = u16::from_le_bytes([e[*o], e[*o + 1]]);
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
                lfn.sort_by_key(|p| p.0);
                let units: Vec<u16> = lfn.iter().flat_map(|p| p.1).take_while(|&c| c != 0 && c != 0xFFFF).collect();
                char::decode_utf16(units).map(|c| c.unwrap_or('?')).collect()
            };
            lfn.clear();
            if name == "." || name == ".." {
                continue;
            }
            let cluster = (u16::from_le_bytes([e[20], e[21]]) as u32) << 16 | u16::from_le_bytes([e[26], e[27]]) as u32;
            entries.push(DirEntry {
                name,
                is_dir: attr & 0x10 != 0,
                size: u32::from_le_bytes(e[28..32].try_into().unwrap()),
                cluster,
            });
        }
        Ok(entries)
    }

    fn lookup(&self, path: &str) -> Result<DirEntry, &'static str> {
        let mut cur = DirEntry { name: String::from("/"), is_dir: true, size: 0, cluster: self.root_cluster };
        for part in path.split('/').filter(|p| !p.is_empty()) {
            if !cur.is_dir {
                return Err("not a directory");
            }
            cur = self
                .read_dir(cur.cluster)?
                .into_iter()
                .find(|e| e.name.eq_ignore_ascii_case(part))
                .ok_or("file not found")?;
        }
        Ok(cur)
    }

    pub fn list(&self, path: &str) -> Result<Vec<DirEntry>, &'static str> {
        let dir = self.lookup(path)?;
        if !dir.is_dir {
            return Ok(vec![dir]);
        }
        self.read_dir(dir.cluster)
    }

    /// Reads up to `limit` bytes of a file.
    pub fn read_file(&self, path: &str, limit: usize) -> Result<Vec<u8>, &'static str> {
        let f = self.lookup(path)?;
        if f.is_dir {
            return Err("is a directory");
        }
        if f.size == 0 {
            return Ok(Vec::new());
        }
        self.read_chain(f.cluster, limit.min(f.size as usize))
    }
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
