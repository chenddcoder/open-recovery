//! FAT12/16/32 只读文件系统 + 删除文件恢复。
//!
//! 恢复策略说明（很重要）：
//! * 未删除文件：按 FAT 簇链精确读取；
//! * 已删除文件：FAT 链已被清零，只能按"连续簇"假设从起始簇读取 `size` 字节。
//!   这是所有 FAT undelete 工具的通用做法——文件若已碎片化，只能靠签名雕刻补救。
//!   所以这里会把"连续假设"明确写进 `Entry::meta`，避免用户误判恢复质量。

use super::boot::{self, FatBoot, FatType};
use super::dir::{self, RawDirEntry};
use super::table::FatTable;
use crate::types::*;
use dmdisk::{Error, Result, Reader};
use std::io::Write;

/// FAT12/16 的固定根目录用 0 作哨兵（合法簇号从 2 开始）
const ROOT_1216: u64 = 0;

pub struct Fat {
    reader: Reader,
    boot: FatBoot,
    table: FatTable,
    info: FsInfo,
}

impl Fat {
    pub fn open(reader: Reader) -> Result<Self> {
        let boot = boot::parse(&reader)?;
        let table = FatTable::new(reader.slice(0, reader.len()), &boot);
        let kind = match boot.fat_type {
            FatType::Fat12 => FsKind::Fat12,
            FatType::Fat16 => FsKind::Fat16,
            FatType::Fat32 => FsKind::Fat32,
        };
        let mut info = FsInfo::new(kind);
        info.label = boot.label.clone();
        info.uuid = Some(format!(
            "{:04X}-{:04X}",
            boot.serial >> 16,
            boot.serial & 0xFFFF
        ));
        info.block_size = boot.cluster_bytes() as u32;
        info.total_bytes = boot.volume_bytes;
        info.free_bytes = Self::free_bytes_from_fsinfo(&reader, &boot);
        info.detail("每扇区字节", boot.bps);
        info.detail("每簇扇区", boot.spc);
        info.detail("保留扇区", boot.reserved);
        info.detail("FAT 表数", boot.num_fats);
        info.detail("FAT 表长(扇区)", boot.fat_sectors);
        info.detail("簇总数", boot.cluster_count);
        if boot.fat_type == FatType::Fat32 {
            info.detail("根目录簇", boot.root_cluster);
        } else {
            info.detail("根目录项数", boot.root_entries);
        }
        Ok(Self { reader, boot, table, info })
    }

    fn free_bytes_from_fsinfo(reader: &Reader, boot: &FatBoot) -> Option<u64> {
        if boot.fat_type != FatType::Fat32 || boot.fsinfo_sector == 0 {
            return None;
        }
        let off = boot.fsinfo_sector as u64 * boot.bps as u64;
        let sec = reader.read_vec(off, 512).ok()?;
        if sec.len() < 512 {
            return None;
        }
        let lead = u32::from_le_bytes([sec[0], sec[1], sec[2], sec[3]]);
        if lead != 0x4161_5252 {
            return None;
        }
        let free = u32::from_le_bytes([sec[488], sec[489], sec[490], sec[491]]);
        if free == 0xFFFF_FFFF || free > boot.cluster_count {
            return None;
        }
        Some(free as u64 * boot.cluster_bytes())
    }

    /// 读出一整个目录的原始字节。
    fn dir_bytes(&self, dir: NodeId) -> Result<Vec<u8>> {
        if dir.0 == ROOT_1216 {
            let off = self.boot.root_dir_sector as u64 * self.boot.bps as u64;
            let len = self.boot.root_dir_sectors as u64 * self.boot.bps as u64;
            return self.reader.read_vec(off, len as usize);
        }
        let mut out = Vec::new();
        let chain = self.table.chain(dir.0 as u32, self.boot.cluster_count + 2)?;
        let cb = self.boot.cluster_bytes() as usize;
        for c in chain {
            out.extend_from_slice(&self.reader.read_vec(self.boot.cluster_offset(c), cb)?);
            // 目录一般不会特别大，超过 64 MiB 必然是坏链
            if out.len() > 64 * 1024 * 1024 {
                break;
            }
        }
        Ok(out)
    }

    fn convert(&self, dir: NodeId, raw: RawDirEntry) -> Entry {
        let kind = if raw.attr & 0x10 != 0 {
            NodeKind::Dir
        } else if raw.attr & 0x40 != 0 {
            NodeKind::Other
        } else {
            NodeKind::File
        };
        let id = if kind == NodeKind::Dir && raw.cluster == 0 {
            NodeId(ROOT_1216)
        } else {
            NodeId(raw.cluster as u64)
        };
        let mut e = Entry::new(id, dir, raw.name.clone(), kind);
        e.size = raw.size as u64;
        e.deleted = raw.deleted;
        e.mtime = raw.mtime;
        e.start = raw.cluster as u64;
        e.name_lost = raw.deleted && !raw.name_from_lfn && raw.name.starts_with('?');
        e.meta.push(("属性".into(), attr_string(raw.attr)));
        if !raw.short.is_empty() && raw.short != raw.name {
            e.meta.push(("短名".into(), raw.short));
        }
        if raw.deleted {
            e.meta.push((
                "恢复方式".into(),
                "按连续簇假设读取（FAT 链已清空）".into(),
            ));
        }
        e
    }

    /// 从起始簇连续读取 `len` 字节（不依赖 FAT 链，用于删除恢复）。
    fn read_contiguous(&self, start_cluster: u32, len: u64, out: &mut dyn Write) -> Result<u64> {
        if start_cluster < 2 || len == 0 {
            return Ok(0);
        }
        let cb = self.boot.cluster_bytes();
        let mut remaining = len;
        let mut cluster = start_cluster;
        let mut written = 0u64;
        while remaining > 0 {
            if cluster as u64 > self.boot.cluster_count as u64 + 1 {
                break;
            }
            let off = self.boot.cluster_offset(cluster);
            if off >= self.reader.len() {
                break;
            }
            let n = remaining.min(cb) as usize;
            let buf = self.reader.read_vec(off, n)?;
            out.write_all(&buf)?;
            written += buf.len() as u64;
            remaining -= buf.len() as u64;
            cluster += 1;
        }
        Ok(written)
    }
}

fn attr_string(a: u8) -> String {
    let mut v = Vec::new();
    if a & 0x01 != 0 {
        v.push("只读");
    }
    if a & 0x02 != 0 {
        v.push("隐藏");
    }
    if a & 0x04 != 0 {
        v.push("系统");
    }
    if a & 0x08 != 0 {
        v.push("卷标");
    }
    if a & 0x10 != 0 {
        v.push("目录");
    }
    if a & 0x20 != 0 {
        v.push("归档");
    }
    if v.is_empty() {
        "-".into()
    } else {
        v.join("|")
    }
}

impl FileSystem for Fat {
    fn info(&self) -> &FsInfo {
        &self.info
    }

    fn root(&self) -> NodeId {
        if self.boot.fat_type == FatType::Fat32 {
            NodeId(self.boot.root_cluster as u64)
        } else {
            NodeId(ROOT_1216)
        }
    }

    fn list(&self, dir: NodeId) -> Result<Vec<Entry>> {
        let bytes = self.dir_bytes(dir)?;
        let raws = dir::parse_dir(&bytes);
        let mut out = Vec::with_capacity(raws.len());
        for r in raws {
            out.push(self.convert(dir, r));
        }
        Ok(out)
    }

    fn read(&self, entry: &Entry, out: &mut dyn Write) -> Result<u64> {
        if entry.is_dir() {
            return Err(Error::Invalid(format!("{} 是目录，不能按文件读取", entry.name)));
        }
        if entry.size == 0 {
            return Ok(0);
        }
        if entry.start < 2 {
            return Err(Error::Invalid(format!(
                "{} 的起始簇为 {}（簇信息已丢失），需要签名雕刻才能恢复",
                entry.name, entry.start
            )));
        }
        if entry.deleted {
            return self.read_contiguous(entry.start as u32, entry.size, out);
        }
        let chain = self.table.chain(entry.start as u32, self.boot.cluster_count + 2)?;
        let cb = self.boot.cluster_bytes();
        let mut remaining = entry.size;
        let mut written = 0u64;
        for c in chain {
            if remaining == 0 {
                break;
            }
            let off = self.boot.cluster_offset(c);
            let n = remaining.min(cb) as usize;
            let buf = self.reader.read_vec(off, n)?;
            out.write_all(&buf)?;
            written += buf.len() as u64;
            remaining -= buf.len() as u64;
        }
        if written < entry.size {
            // 链比 size 短：坏链或文件被截断，如实返回已读字节数
            return Ok(written);
        }
        Ok(written)
    }

    fn deleted(&self) -> Result<Vec<Entry>> {
        let mut out = Vec::new();
        let mut queue = vec![(self.root(), 0usize)];
        let mut visited = std::collections::HashSet::new();
        visited.insert(self.root());
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
                if e.is_dir() && visited.insert(e.id) {
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
