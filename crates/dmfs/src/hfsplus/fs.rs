//! HFS+ 只读文件系统。
//!
//! 删除恢复说明：HFS+ 删除文件时直接从 catalog B-tree 移除记录（不像 FAT/NTFS
//! 保留残骸），因此**无法按目录项恢复**。这里如实返回空列表并说明，
//! 真正的恢复要靠后续的签名雕刻（carving）模块。

use super::catalog::{self, CatEntry};
use super::volume::{self, Fork, HfsVolume};
use crate::types::*;
use dmdisk::{Error, Result, Reader};
use std::collections::HashMap;
use std::io::Write;

pub const ROOT_CNID: u32 = 2;

pub struct HfsPlus {
    reader: Reader,
    vol: HfsVolume,
    info: FsInfo,
    children: HashMap<u32, Vec<CatEntry>>,
    by_id: HashMap<u32, CatEntry>,
}

impl HfsPlus {
    pub fn open(reader: Reader) -> Result<Self> {
        let vol = volume::parse(&reader)?;
        let mut info = FsInfo::new(FsKind::HfsPlus);
        info.block_size = vol.block_size;
        info.total_bytes = vol.total_blocks * vol.block_size as u64;
        info.free_bytes = Some(vol.free_blocks * vol.block_size as u64);
        info.label = String::new();
        info.detail("卷版本", format!("{:#06x}", vol.version));
        info.detail("文件数", vol.file_count);
        info.detail("文件夹数", vol.folder_count);
        info.detail("catalog 大小", vol.catalog.logical_size);
        info.detail("catalog 片段", vol.catalog.extents.len());
        if vol.catalog.total_blocks as u64 > vol.catalog.blocks_covered() {
            info.detail(
                "注意",
                "catalog 的 extents 溢出到 overflow 文件，超大目录可能列不全",
            );
        }

        let mut fsh = Self {
            reader,
            vol,
            info,
            children: HashMap::new(),
            by_id: HashMap::new(),
        };
        let cat_data = fsh.read_fork(&fsh.vol.catalog.clone(), 512 * 1024 * 1024)?;
        let ns = catalog::node_size(&cat_data, 0).unwrap_or(4096);
        let entries = catalog::parse_catalog(&cat_data, ns);
        for e in entries {
            // 根卷名就是"父目录为 1 的那条记录"（HFS+ 不把卷名存在卷头里）
            if e.parent == 1 && !e.name.is_empty() {
                fsh.info.label = e.name.clone();
            }
            // 空名记录是 HFS+ 的元数据残留（无法作为文件存在），不进入目录树
            if e.name.is_empty() {
                continue;
            }
            fsh.by_id.insert(e.cnid, e.clone());
            fsh.children.entry(e.parent).or_default().push(e);
        }
        Ok(fsh)
    }

    fn read_fork(&self, fork: &Fork, cap: u64) -> Result<Vec<u8>> {
        let bs = self.vol.block_size;
        let want = fork.logical_size.min(cap);
        let mut out = Vec::with_capacity(want.min(64 * 1024 * 1024) as usize);
        for (start, count) in &fork.extents {
            if out.len() as u64 >= want {
                break;
            }
            let off = *start as u64 * bs as u64;
            let len = (*count as u64 * bs as u64).min(want - out.len() as u64);
            if off >= self.reader.len() {
                continue;
            }
            let buf = self.reader.read_vec(off, len as usize)?;
            out.extend_from_slice(&buf);
        }
        if out.len() as u64 > want {
            out.truncate(want as usize);
        }
        Ok(out)
    }

    fn convert(&self, parent: NodeId, c: &CatEntry) -> Entry {
        let kind = if c.is_dir { NodeKind::Dir } else { NodeKind::File };
        let mut e = Entry::new(NodeId(c.cnid as u64), parent, c.name.clone(), kind);
        e.size = c.size;
        e.mtime = c.mtime;
        e.start = c.extents.first().map(|x| x.0 as u64).unwrap_or(0);
        e.meta.push(("CNID".into(), c.cnid.to_string()));
        if !c.is_dir {
            let covered = c.extents.iter().map(|x| x.1 as u64).sum::<u64>();
            if covered < c.total_blocks as u64 {
                e.meta.push((
                    "注意".into(),
                    format!("extents 溢出（{covered}/{} 块）", c.total_blocks),
                ));
            }
        }
        e
    }
}

impl FileSystem for HfsPlus {
    fn info(&self) -> &FsInfo {
        &self.info
    }

    fn root(&self) -> NodeId {
        NodeId(ROOT_CNID as u64)
    }

    fn list(&self, dir: NodeId) -> Result<Vec<Entry>> {
        let parent = dir.0 as u32;
        Ok(self
            .children
            .get(&parent)
            .map(|v| v.iter().map(|c| self.convert(dir, c)).collect())
            .unwrap_or_default())
    }

    fn read(&self, entry: &Entry, out: &mut dyn Write) -> Result<u64> {
        if entry.is_dir() {
            return Err(Error::Invalid("目录不能按文件读取".into()));
        }
        let cat = self
            .by_id
            .get(&(entry.id.0 as u32))
            .ok_or_else(|| Error::Invalid(format!("catalog 中找不到 CNID {}", entry.id.0)))?;
        let bs = self.vol.block_size;
        let mut written = 0u64;
        for (start, count) in &cat.extents {
            if written >= entry.size.max(cat.size) {
                break;
            }
            let off = *start as u64 * bs as u64;
            let want = ((*count as u64 * bs as u64).min(cat.size - written)) as usize;
            if want == 0 {
                break;
            }
            let buf = self.reader.read_vec(off, want)?;
            out.write_all(&buf)?;
            written += buf.len() as u64;
        }
        Ok(written)
    }

    fn deleted(&self) -> Result<Vec<Entry>> {
        // catalog 记录已被删除，没有可枚举的残骸
        Ok(Vec::new())
    }

    fn supports_deleted(&self) -> bool {
        false
    }
}
