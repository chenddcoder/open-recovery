//! ext2/3/4 只读文件系统 + 删除恢复。

use super::dir::{self, ExtDirEntry};
use super::extent::{self, Extent};
use super::inode::{self, Inode};
use super::superblock::{self, ExtSuper};
use crate::types::*;
use dmdisk::{Error, Result, Reader};
use std::io::Write;

const ROOT_INODE: u64 = 2;

struct GroupDesc {
    inode_table: u64,
    flags: u16,
}

pub struct Ext {
    reader: Reader,
    sup: ExtSuper,
    info: FsInfo,
    filetype_feature: bool,
}

impl Ext {
    pub fn open(reader: Reader) -> Result<Self> {
        let sup = superblock::parse(&reader)?;
        if sup.blocks_per_group == 0 || sup.inodes_per_group == 0 {
            return Err(Error::Format("ext 超级块的组参数为 0".into()));
        }
        let mut info = FsInfo::new(sup.kind);
        info.label = sup.label.clone();
        info.uuid = Some(sup.uuid.clone());
        info.block_size = sup.block_size;
        info.total_bytes = sup.blocks_count * sup.block_size as u64;
        info.free_bytes = Some(sup.free_blocks * sup.block_size as u64);
        info.detail("块大小", sup.block_size);
        info.detail("inode 大小", sup.inode_size);
        info.detail("inode 总数", sup.inodes_count);
        info.detail("块组数", sup.group_count());
        info.detail("每块组块数", sup.blocks_per_group);
        info.detail("每块组 inode", sup.inodes_per_group);
        info.detail("inode 表起始", sup.gdt_offset);
        info.detail("特性", format!("incompat={:#x} ro_compat={:#x}", sup.incompat, sup.ro_compat));
        if sup.journal_inum != 0 {
            info.detail("日志 inode", sup.journal_inum);
        }
        if sup.incompat & superblock::INCOMPAT_META_BG != 0 {
            info.detail("注意", "启用了 meta_bg，超大卷的 inode 表定位可能不准");
        }
        let filetype_feature = sup.incompat & 0x2 != 0;
        Ok(Self { reader, sup, info, filetype_feature })
    }

    fn group_desc(&self, group: u64) -> Result<GroupDesc> {
        let off = self.sup.gdt_offset + group * self.sup.desc_size as u64;
        let d = self.reader.read_vec(off, self.sup.desc_size as usize)?;
        if d.len() < 32 {
            return Err(Error::Format("块组描述符读取失败".into()));
        }
        let inode_table_lo = u32::from_le_bytes([d[8], d[9], d[10], d[11]]) as u64;
        let inode_table_hi = if self.sup.is_64bit() && d.len() >= 44 {
            u32::from_le_bytes([d[40], d[41], d[42], d[43]]) as u64
        } else {
            0
        };
        Ok(GroupDesc {
            inode_table: inode_table_lo | (inode_table_hi << 32),
            flags: u16::from_le_bytes([d[18], d[19]]),
        })
    }

    fn inode_offset(&self, ino: u64) -> Result<u64> {
        if ino == 0 || ino > self.sup.inodes_count as u64 {
            return Err(Error::Invalid(format!("inode 号越界: {ino}")));
        }
        let idx = ino - 1;
        let group = idx / self.sup.inodes_per_group as u64;
        let within = idx % self.sup.inodes_per_group as u64;
        let gd = self.group_desc(group)?;
        if gd.flags & 0x4 != 0 {
            // BLOCK_UNINIT 不影响 inode 表位置，仅提示
        }
        Ok(gd.inode_table * self.sup.block_size as u64 + within * self.sup.inode_size as u64)
    }

    fn read_inode(&self, ino: u64) -> Result<Inode> {
        let off = self.inode_offset(ino)?;
        let raw = self.reader.read_vec(off, self.sup.inode_size as usize)?;
        if raw.len() < 32 {
            return Err(Error::Format(format!("inode {ino} 读取不完整")));
        }
        Ok(inode::parse(ino, &raw))
    }

    fn extents_for(&self, ino: &Inode) -> Result<Vec<Extent>> {
        let mut out = Vec::new();
        let bs = self.sup.block_size;
        if ino.flags & inode::EXTENTS_FL != 0 && self.sup.has_extents() {
            extent::walk(&self.reader, &ino.i_block, &mut out, 0, bs)?;
        } else {
            let blocks = ino.size.div_ceil(bs as u64);
            extent::indirect_blocks(&self.reader, &ino.i_block, bs, blocks.max(1), &mut out)?;
        }
        out.sort_by_key(|e| e.logical);
        Ok(out)
    }

    /// 按 extents 读取 inode 数据，空洞补零。
    fn read_data(&self, inode: &Inode, out: &mut dyn Write, limit: u64) -> Result<u64> {
        let bs = self.sup.block_size as u64;
        let limit = limit.min(inode.size);
        if limit == 0 {
            return Ok(0);
        }
        // 符号链接的内联目标
        if inode.kind() == NodeKind::Symlink && inode.blocks_512 == 0 && inode.size <= 60 {
            let n = inode.size.min(60) as usize;
            out.write_all(&inode.i_block[..n])?;
            return Ok(n as u64);
        }
        let extents = self.extents_for(inode)?;
        let mut logical = 0u64;
        let mut written = 0u64;
        let total_blocks = limit.div_ceil(bs);
        for e in &extents {
            if logical >= total_blocks {
                break;
            }
            if e.logical > logical {
                let holes = (e.logical - logical).min(total_blocks - logical);
                written += write_zeros(out, holes * bs, limit - written)?;
                logical = e.logical;
            }
            for i in 0..e.len {
                if logical >= total_blocks {
                    break;
                }
                let want = (limit.saturating_sub(written)).min(bs) as usize;
                if e.uninit || e.physical == 0 {
                    write_zeros(out, want as u64, want as u64)?;
                } else {
                    let phys = (e.physical + i) * self.sup.block_size as u64;
                    let buf = self.reader.read_vec(phys, want)?;
                    out.write_all(&buf)?;
                }
                written += want as u64;
                logical += 1;
            }
            if written >= limit {
                break;
            }
        }
        if written < limit {
            // 没有 extent 覆盖的尾部
            written += write_zeros(out, limit - written, limit - written)?;
        }
        Ok(written)
    }

    fn dir_entries(&self, inode: &Inode) -> Result<Vec<ExtDirEntry>> {
        let bs = self.sup.block_size;
        let extents = self.extents_for(inode)?;
        let mut out = Vec::new();
        let mut count = 0u64;
        for e in &extents {
            for i in 0..e.len {
                if e.uninit {
                    continue;
                }
                let phys = (e.physical + i) * bs as u64;
                if phys >= self.reader.len() {
                    continue;
                }
                let buf = self.reader.read_vec(phys, bs as usize)?;
                out.extend(dir::parse_dir_block(&buf, self.filetype_feature));
                count += 1;
                if count > 1_000_000 {
                    return Ok(out);
                }
            }
        }
        Ok(out)
    }

    fn convert(&self, parent: NodeId, e: &ExtDirEntry) -> Option<Entry> {
        if e.inode == 0 {
            // 目录项残留：名字还在，inode 已清空
            let mut ent = Entry::new(NodeId(0), parent, e.name.clone(), NodeKind::File);
            ent.deleted = true;
            ent.meta
                .push(("说明".into(), "目录项残留（inode 已清零）".into()));
            return Some(ent);
        }
        let key = self.read_inode(e.inode as u64).ok()?;
        let mut ent = Entry::new(NodeId(e.inode as u64), parent, e.name.clone(), key.kind());
        ent.size = key.size;
        ent.mtime = key.mtime;
        ent.start = first_physical_block(&key, self.sup.has_extents());
        ent.meta.push(("权限".into(), key.mode_string()));
        ent.meta.push(("链接数".into(), key.links.to_string()));
        if key.is_deleted() {
            ent.deleted = true;
            ent.meta.push(("说明".into(), "inode 已释放（链接数 0）".into()));
        }
        Some(ent)
    }
}

/// 从 inode 里 O(1) 取出首个物理块号（不读设备），用于列表展示。
fn first_physical_block(ino: &Inode, has_extents_fs: bool) -> u64 {
    if ino.flags & inode::EXTENTS_FL != 0 && has_extents_fs {
        // extent header: magic(2) entries(2) max(2) depth(2) gen(4)
        let magic = u16::from_le_bytes([ino.i_block[0], ino.i_block[1]]);
        if magic == 0xF30A {
            let entries = u16::from_le_bytes([ino.i_block[2], ino.i_block[3]]);
            let depth = u16::from_le_bytes([ino.i_block[6], ino.i_block[7]]);
            if entries > 0 && depth == 0 {
                let hi = u16::from_le_bytes([ino.i_block[18], ino.i_block[19]]) as u64;
                let lo = u32::from_le_bytes([
                    ino.i_block[20],
                    ino.i_block[21],
                    ino.i_block[22],
                    ino.i_block[23],
                ]) as u64;
                return (hi << 32) | lo;
            }
        }
        0
    } else {
        u32::from_le_bytes([ino.i_block[0], ino.i_block[1], ino.i_block[2], ino.i_block[3]]) as u64
    }
}

fn write_zeros(out: &mut dyn Write, bytes: u64, cap: u64) -> Result<u64> {    let n = bytes.min(cap);
    let zeros = vec![0u8; n.min(1 << 20) as usize];
    let mut left = n;
    while left > 0 {
        let k = left.min(zeros.len() as u64);
        out.write_all(&zeros[..k as usize])?;
        left -= k;
    }
    Ok(n)
}

impl FileSystem for Ext {
    fn info(&self) -> &FsInfo {
        &self.info
    }

    fn root(&self) -> NodeId {
        NodeId(ROOT_INODE)
    }

    fn list(&self, dir: NodeId) -> Result<Vec<Entry>> {
        let inode = self.read_inode(dir.0)?;
        if !inode.is_dir() {
            return Err(Error::Invalid(format!("inode {} 不是目录", dir.0)));
        }
        let raws = self.dir_entries(&inode)?;
        let mut out = Vec::new();
        for r in raws {
            if let Some(e) = self.convert(dir, &r) {
                out.push(e);
            }
        }
        Ok(out)
    }

    fn read(&self, entry: &Entry, out: &mut dyn Write) -> Result<u64> {
        if entry.is_dir() {
            return Err(Error::Invalid("目录不能按文件读取".into()));
        }
        if entry.id.0 == 0 {
            return Err(Error::Invalid(format!(
                "{} 的 inode 已丢失，需要按签名雕刻恢复",
                entry.name
            )));
        }
        let inode = self.read_inode(entry.id.0)?;
        let limit = if entry.size > 0 { entry.size } else { inode.size };
        self.read_data(&inode, out, limit)
    }

    fn deleted(&self) -> Result<Vec<Entry>> {
        let mut out: Vec<Entry> = Vec::new();

        // 1) 目录里的孤儿条目：能拿到真实文件名
        let mut queue = vec![(self.root(), 0usize)];
        let mut seen = std::collections::HashSet::new();
        seen.insert(self.root());
        while let Some((node, depth)) = queue.pop() {
            if depth > 32 {
                continue;
            }
            let inode = match self.read_inode(node.0) {
                Ok(i) => i,
                Err(_) => continue,
            };
            let raws = match self.dir_entries(&inode) {
                Ok(v) => v,
                Err(_) => continue,
            };
            for r in raws {
                if r.inode == 0 {
                    out.push(Entry {
                        id: NodeId(0),
                        parent: node,
                        name: r.name.clone(),
                        kind: NodeKind::File,
                        size: 0,
                        deleted: true,
                        mtime: None,
                        start: 0,
                        name_lost: false,
                        meta: vec![("说明".into(), "目录项残留，inode 已清零".into())],
                    });
                } else if r.file_type == dir::FT_DIR || r.file_type == 0 {
                    let id = NodeId(r.inode as u64);
                    if seen.insert(id) {
                        queue.push((id, depth + 1));
                    }
                }
            }
        }

        // 2) 扫 inode 表：能拿到大小和块映射，但没有文件名
        let mut scanned = 0u64;
        for ino in self.sup.first_ino.max(11) as u64..=self.sup.inodes_count as u64 {
            scanned += 1;
            if scanned > 4_000_000 && out.len() > 0 {
                break;
            }
            let inode = match self.read_inode(ino) {
                Ok(i) => i,
                Err(_) => continue,
            };
            if !inode.is_regular() || !inode.is_deleted() || inode.size == 0 {
                continue;
            }
            let mut e = Entry::new(
                NodeId(ino),
                NodeId(0),
                format!("ino{ino}.rec"),
                NodeKind::File,
            );
            e.size = inode.size;
            e.deleted = true;
            e.mtime = inode.mtime;
            e.name_lost = true;
            e.meta.push(("inode".into(), ino.to_string()));
            e.meta.push(("块占用".into(), format!("{} 个 512B 块", inode.blocks_512)));
            out.push(e);
            if out.len() > 200_000 {
                break;
            }
        }

        out.sort_by(|a, b| b.size.cmp(&a.size));
        Ok(out)
    }
}
