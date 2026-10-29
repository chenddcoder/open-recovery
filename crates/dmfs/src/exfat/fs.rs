//! exFAT 只读文件系统 + 删除恢复。

use super::boot::{self, ExFatBoot};
use super::dir::{self, ExEntry};
use crate::types::*;
use dmdisk::{Error, Result, Reader};
use std::io::Write;

/// NodeId 的 bit62 用来标记"目录数据是连续存放的（NoFatChain）"
const CONTIG_FLAG: u64 = 1 << 62;

fn encode(cluster: u32, contig: bool) -> NodeId {
    NodeId(cluster as u64 | if contig { CONTIG_FLAG } else { 0 })
}

fn decode(id: NodeId) -> (u32, bool) {
    ((id.0 & !CONTIG_FLAG) as u32, id.0 & CONTIG_FLAG != 0)
}

pub struct ExFat {
    reader: Reader,
    boot: ExFatBoot,
    info: FsInfo,
}

impl ExFat {
    pub fn open(reader: Reader) -> Result<Self> {
        let boot = boot::parse(&reader)?;
        let mut info = FsInfo::new(FsKind::ExFat);
        info.block_size = boot.cluster_bytes() as u32;
        info.total_bytes = boot.volume_bytes();
        info.uuid = Some(format!(
            "{:04X}-{:04X}",
            boot.serial >> 16,
            boot.serial & 0xFFFF
        ));
        if boot.percent_in_use <= 100 && boot.percent_in_use > 0 {
            let used = boot.volume_bytes() / 100 * boot.percent_in_use as u64;
            info.free_bytes = Some(boot.volume_bytes().saturating_sub(used));
        }
        info.detail("扇区大小", boot.bytes_per_sector);
        info.detail("每簇扇区", boot.sectors_per_cluster);
        info.detail("簇总数", boot.cluster_count);
        info.detail("FAT 起始扇区", boot.fat_offset);
        info.detail("FAT 长度(扇区)", boot.fat_length);
        info.detail("簇堆起始扇区", boot.cluster_heap_offset);
        info.detail("根目录簇", boot.root_cluster);
        if boot.num_fats > 1 {
            info.detail("FAT 表数", boot.num_fats);
        }
        let mut me = Self { reader, boot, info };
        if let Some(label) = me.volume_label() {
            me.info.label = label;
        }
        Ok(me)
    }

    fn fat_next(&self, cluster: u32) -> Result<u32> {
        let off = self.boot.fat_offset as u64 * self.boot.bytes_per_sector as u64
            + cluster as u64 * 4;
        let v = self.reader.u32_at(off)?;
        Ok(v & 0x0FFF_FFFF)
    }

    fn chain(&self, start: u32, contig: bool) -> Result<Vec<u32>> {
        let mut out = Vec::new();
        let mut cur = start;
        let max = self.boot.cluster_count + 2;
        let mut guard = 0u32;
        while guard < max && cur >= 2 {
            guard += 1;
            out.push(cur);
            if contig {
                // 连续分配：下一个簇就是 cur+1
                if cur + 1 > self.boot.cluster_count + 1 {
                    break;
                }
                cur += 1;
                if out.len() as u64 * self.boot.cluster_bytes() > 64 * 1024 * 1024 && out.len() > 4096 {
                    break;
                }
                continue;
            }
            let nxt = self.fat_next(cur)?;
            if nxt < 2 || nxt == 0x0FFF_FFFF || nxt == cur || out.contains(&nxt) {
                break;
            }
            cur = nxt;
        }
        Ok(out)
    }

    /// 目录字节流：读到 EndOfDirectory 条目为止。
    fn dir_bytes(&self, cluster: u32, contig: bool) -> Result<Vec<u8>> {
        let cb = self.boot.cluster_bytes() as usize;
        let mut out = Vec::new();
        let mut cur = cluster;
        let mut guard = 0u32;
        let max = self.boot.cluster_count + 2;
        while cur >= 2 && guard < max {
            guard += 1;
            let off = self.boot.cluster_offset(cur);
            if off >= self.reader.len() {
                break;
            }
            let buf = self.reader.read_vec(off, cb)?;
            out.extend_from_slice(&buf);
            if has_end_marker(&out) || out.len() > 32 * 1024 * 1024 {
                break;
            }
            cur = if contig {
                cur + 1
            } else {
                let n = self.fat_next(cur)?;
                if n < 2 || n == 0x0FFF_FFFF {
                    break;
                }
                n
            };
        }
        Ok(out)
    }

    fn convert(&self, parent: NodeId, raw: ExEntry) -> Entry {
        let kind = if raw.is_dir {
            NodeKind::Dir
        } else {
            NodeKind::File
        };
        let mut e = Entry::new(
            encode(raw.cluster, raw.is_dir || raw.no_fat_chain),
            parent,
            raw.name.clone(),
            kind,
        );
        e.size = raw.size;
        e.deleted = raw.deleted;
        e.mtime = raw.mtime;
        e.start = raw.cluster as u64;
        if raw.deleted {
            e.meta
                .push(("恢复方式".into(), "连续簇读取（簇链已清空）".into()));
        }
        if raw.no_fat_chain {
            e.meta.push(("分配".into(), "连续".into()));
        }
        if raw.valid_size != raw.size {
            e.meta
                .push(("有效长度".into(), raw.valid_size.to_string()));
        }
        e
    }

    fn volume_label(&self) -> Option<String> {
        let bytes = self.dir_bytes(self.boot.root_cluster, false).ok()?;
        let mut i = 0;
        while i + 32 <= bytes.len() {
            let e = &bytes[i..i + 32];
            if e[0] == 0x83 {
                let count = e[1] as usize;
                let chars: Vec<u16> = (0..count.min(11))
                    .map(|k| u16::from_le_bytes([e[2 + k * 2], e[3 + k * 2]]))
                    .collect();
                return Some(String::from_utf16_lossy(&chars));
            }
            i += 32;
        }
        None
    }
}

fn has_end_marker(buf: &[u8]) -> bool {
    let mut i = 0;
    while i + 32 <= buf.len() {
        if buf[i] == 0x00 {
            return true;
        }
        i += 32;
    }
    false
}

impl FileSystem for ExFat {
    fn info(&self) -> &FsInfo {
        &self.info
    }

    fn root(&self) -> NodeId {
        encode(self.boot.root_cluster, false)
    }

    fn list(&self, dir: NodeId) -> Result<Vec<Entry>> {
        let (cluster, contig) = decode(dir);
        let bytes = self.dir_bytes(cluster, contig)?;
        Ok(dir::parse_dir(&bytes)
            .into_iter()
            .filter(|e| !e.name.is_empty())
            .map(|e| self.convert(dir, e))
            .collect())
    }

    fn read(&self, entry: &Entry, out: &mut dyn Write) -> Result<u64> {
        if entry.is_dir() {
            return Err(Error::Invalid("目录不能按文件读取".into()));
        }
        if entry.size == 0 {
            return Ok(0);
        }
        if entry.start < 2 {
            return Err(Error::Invalid(format!(
                "{} 的起始簇无效，需要签名雕刻",
                entry.name
            )));
        }
        let contig = entry.deleted || entry.id.0 & CONTIG_FLAG != 0;
        let chain = self.chain(entry.start as u32, contig)?;
        let cb = self.boot.cluster_bytes();
        let mut remaining = entry.size;
        let mut written = 0u64;
        for c in chain {
            if remaining == 0 {
                break;
            }
            let n = remaining.min(cb) as usize;
            let buf = self.reader.read_vec(self.boot.cluster_offset(c), n)?;
            out.write_all(&buf)?;
            written += buf.len() as u64;
            remaining -= buf.len() as u64;
        }
        Ok(written)
    }

    fn deleted(&self) -> Result<Vec<Entry>> {
        let mut out = Vec::new();
        let mut queue = vec![(self.root(), 0usize)];
        let mut seen = std::collections::HashSet::new();
        seen.insert(self.root());
        while let Some((dir, depth)) = queue.pop() {
            if depth > 32 {
                continue;
            }
            let entries = match self.list(dir) {
                Ok(v) => v,
                Err(_) => continue,
            };
            for e in entries {
                if e.deleted {
                    if !e.is_dir() && e.size > 0 {
                        out.push(e);
                    }
                    continue;
                }
                if e.is_dir() && seen.insert(e.id) {
                    queue.push((e.id, depth + 1));
                }
            }
            if out.len() > 200_000 {
                break;
            }
        }
        out.sort_by(|a, b| b.size.cmp(&a.size));
        Ok(out)
    }
}
