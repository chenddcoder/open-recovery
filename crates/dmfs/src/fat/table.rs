//! FAT 表访问与簇链遍历。
//!
//! FAT 表可能很大（32 GiB 卷 4 MiB 左右，2 TiB 卷可达 256 MiB），
//! 因此不整表载入，用 4 KiB 分块 + 有界 LRU 缓存。

use super::boot::{FatBoot, FatType};
use dmdisk::{Error, Result, Reader};
use std::collections::HashMap;
use std::sync::Mutex;

const CHUNK: u64 = 4096;
const MAX_CHUNKS: usize = 512; // 2 MiB

pub struct FatTable {
    kind: FatType,
    start: u64,
    len: u64,
    reader: Reader,
    cache: Mutex<HashMap<u64, Vec<u8>>>,
}

impl FatTable {
    pub fn new(reader: Reader, boot: &FatBoot) -> Self {
        let start = boot.reserved as u64 * boot.bps as u64;
        let len = boot.fat_sectors as u64 * boot.bps as u64;
        Self {
            kind: boot.fat_type,
            start,
            len,
            reader,
            cache: Mutex::new(HashMap::new()),
        }
    }

    fn bytes_at(&self, off: u64) -> Result<Vec<u8>> {
        let idx = off / CHUNK;
        {
            let c = self.cache.lock().unwrap();
            if let Some(v) = c.get(&idx) {
                return Ok(v.clone());
            }
        }
        let read_off = self.start + idx * CHUNK;
        let n = CHUNK.min(self.len.saturating_sub(idx * CHUNK)) as usize;
        let mut buf = vec![0u8; n];
        if n > 0 {
            self.reader.read(read_off, &mut buf)?;
        }
        let mut c = self.cache.lock().unwrap();
        if c.len() >= MAX_CHUNKS {
            if let Some(k) = c.keys().next().copied() {
                c.remove(&k);
            }
        }
        c.insert(idx, buf.clone());
        Ok(buf)
    }

    /// 读取簇的下一个簇号（原始值）。
    pub fn next_raw(&self, cluster: u32) -> Result<u32> {
        match self.kind {
            FatType::Fat12 => {
                let off = (cluster as u64 * 3) / 2;
                let b = self.bytes_at(off)?;
                let i = (off % CHUNK) as usize;
                if i + 1 >= b.len() {
                    return Err(Error::Bounds {
                        what: "FAT12 表项".into(),
                        offset: off,
                        len: 2,
                    });
                }
                let v = u16::from_le_bytes([b[i], b[i + 1]]);
                Ok(if cluster & 1 == 1 { (v >> 4) as u32 } else { (v & 0x0FFF) as u32 })
            }
            FatType::Fat16 => {
                let off = cluster as u64 * 2;
                let b = self.bytes_at(off)?;
                let i = (off % CHUNK) as usize;
                if i + 1 >= b.len() {
                    return Ok(0xFFFF);
                }
                Ok(u16::from_le_bytes([b[i], b[i + 1]]) as u32)
            }
            FatType::Fat32 => {
                let off = cluster as u64 * 4;
                let b = self.bytes_at(off)?;
                let i = (off % CHUNK) as usize;
                if i + 3 >= b.len() {
                    return Ok(0x0FFF_FFFF);
                }
                Ok(u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]) & 0x0FFF_FFFF)
            }
        }
    }

    pub fn kind(&self) -> FatType {
        self.kind
    }

    pub fn is_eoc(&self, v: u32) -> bool {
        match self.kind {
            FatType::Fat12 => v >= 0x0FF8,
            FatType::Fat16 => v >= 0xFFF8,
            FatType::Fat32 => v >= 0x0FFF_FFF8,
        }
    }

    pub fn is_bad(&self, v: u32) -> bool {
        match self.kind {
            FatType::Fat12 => v == 0x0FF7,
            FatType::Fat16 => v == 0xFFF7,
            FatType::Fat32 => v == 0x0FFF_FFF7,
        }
    }

    pub fn is_free(&self, v: u32) -> bool {
        v == 0
    }

    /// 遍历簇链。带环检测与长度上限，坏盘上 FAT 成环很常见。
    pub fn chain(&self, start: u32, max_clusters: u32) -> Result<Vec<u32>> {
        let mut out = Vec::new();
        let mut cur = start;
        let mut guard = 0u32;
        while guard < max_clusters {
            guard += 1;
            if cur < 2 {
                break;
            }
            out.push(cur);
            let nxt = self.next_raw(cur)?;
            if self.is_eoc(nxt) || self.is_free(nxt) || self.is_bad(nxt) || nxt < 2 {
                break;
            }
            if out.contains(&nxt) {
                // 成环：坏盘典型症状，直接截断
                break;
            }
            cur = nxt;
        }
        Ok(out)
    }
}
