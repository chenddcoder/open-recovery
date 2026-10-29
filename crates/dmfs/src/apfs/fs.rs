//! APFS 只读文件系统。
//!
//! 设计要点：
//! * **不预载整棵树**。列目录 = 对 `(dir_id, DIR_REC)` 做一次前缀范围扫描，
//!   读文件 = 对该 id 的 `FILE_EXTENT` 做一次扫描。1 TB 的卷也不会吃内存。
//! * **按 xid 取版本**。所有 oid→块号都经过卷 omap，并且带 `max_xid`，
//!   因此天然读到"最新可见版本"，不会捡到旧事务的残影。
//! * 删除恢复的实话：APFS 删文件是把 drec/inode 一起删掉，还清空 omap 里的条目，
//!   所以**按目录项基本救不回来**。能救的三条路写在 [`Apfs::recovery_hint`]。

use super::btree::{Resolve, Tree};
use super::container::Container;
use super::omap::Omap;
use super::record::{self, Inode};
use super::volume::{self, Volume};
use crate::types::*;
use dmdisk::{Error, Result, Reader};
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Write;

/// APFS 根目录 inode 号（父目录是 id=1 的虚拟根）
pub const ROOT_DIR_ID: u64 = 2;
/// 单次读取块大小
const IO_CHUNK: usize = 256 * 1024;

pub struct Apfs {
    reader: Reader,
    container: Container,
    vol: Volume,
    bs: u32,
    root_paddr: u64,
    omap: Omap,
    info: FsInfo,
    inode_cache: RefCell<HashMap<u64, Option<Inode>>>,
    paddr_cache: RefCell<HashMap<u64, u64>>,
    /// 卷是否大小写不敏感（决定 drec 键格式优先按哪种解析）
    hashed_names: bool,
    /// 上一次孤儿扫描是否因为记录上限被截断
    truncated: std::cell::Cell<bool>,
}

/// 把虚拟 oid 解析成块号，带一层缓存。
struct VolResolver<'a> {
    reader: &'a Reader,
    bs: u32,
    omap: &'a Omap,
    max_xid: u64,
    cache: &'a RefCell<HashMap<u64, u64>>,
}

impl Resolve for VolResolver<'_> {
    fn node_paddr(&mut self, oid: u64) -> Result<u64> {
        if let Some(p) = self.cache.borrow().get(&oid) {
            return Ok(*p);
        }
        let p = self.omap.resolve(self.reader, self.bs, oid, self.max_xid)?;
        self.cache.borrow_mut().insert(oid, p);
        Ok(p)
    }
}

impl Apfs {
    pub fn open(reader: Reader) -> Result<Self> {
        let container = Container::open(&reader)?;
        let vol = container.volume(&reader, 0)?;
        Self::from_parts(reader, container, vol)
    }

    /// 从已解析的容器 + 卷构造（快照场景可传入旧 APSB）。
    pub fn from_parts(reader: Reader, container: Container, vol: Volume) -> Result<Self> {
        let bs = container.bs;
        if vol.is_encrypted() {
            return Err(Error::Unsupported(format!(
                "卷「{}」是加密卷（FileVault），fs-tree 的键值都是密文。\
                 先在系统里解锁该卷、或对已解密的镜像再分析；本工具不做解密。",
                vol.name
            )));
        }
        let omap = vol.omap(&reader, bs)?;
        let root_paddr = vol.root_tree_paddr(&reader, bs)?;

        let mut info = FsInfo::new(FsKind::Apfs);
        info.label = vol.name.clone();
        info.uuid = if vol.uuid.is_empty() { None } else { Some(vol.uuid.clone()) };
        info.block_size = bs;
        // APSB 里没有"卷有多少块"这个字段——块空间是容器共享的，
        // 容量必须取自 NXSB 的 nx_block_count，否则会读出天文数字。
        info.total_bytes = container.block_count.saturating_mul(bs as u64);
        let free = if vol.alloc_count > 0 {
            Some(
                container
                    .block_count
                    .saturating_sub(vol.alloc_count)
                    .saturating_mul(bs as u64),
            )
        } else {
            None
        };
        info.free_bytes = free;
        if container.fs_oids.len() > 1 {
            info.detail(
                "空间",
                format!("容器内 {} 个卷共享块空间，这里的容量是整个容器的", container.fs_oids.len()),
            );
        }
        info.detail("卷角色", vol.role_name());
        info.detail("卷 xid", vol.xid);
        info.detail("容器块", format!("{} × {}B", container.block_count, bs));
        info.detail("容器 xid", container.xid);
        info.detail(
            "大小写",
            if vol.case_insensitive() { "不敏感" } else { "敏感" },
        );
        info.detail(
            "drec 键",
            if vol.hashed_drec() { "hashed（含名字哈希）" } else { "plain（u16 长度）" },
        );
        if vol.is_sealed() {
            info.detail("系统卷", "已封装（sealed，哈希树保护）");
        }
        info.detail("文件数", vol.num_files);
        info.detail("目录数", vol.num_dirs);
        info.detail("符号链接", vol.num_symlinks);
        info.detail("快照数", vol.num_snaps);
        info.detail("fs-tree 根", format!("oid {} → 块 {}", vol.root_tree_oid, root_paddr));
        info.detail("omap", format!("块 {}（{} 项快照计数）", omap.paddr, omap.snap_count));
        // 多卷容器：把每个卷的名字列出来，省得用户猜 --volume 该给几
        if container.fs_oids.len() > 1 {
            let mut names = Vec::new();
            for (i, oid) in container.fs_oids.iter().enumerate() {
                let label = container
                    .omap(&reader)
                    .and_then(|om| om.resolve(&reader, bs, *oid, container.xid))
                    .and_then(|p| volume::open_at(&reader, bs, p))
                    .map(|v| v.name)
                    .unwrap_or_else(|_| "<无法读取>".into());
                names.push(format!("{i}={label}"));
            }
            info.detail("容器内卷", names.join(", "));
            info.detail(
                "选卷",
                format!("当前是第 {} 个；用 --volume N 切换（N 从 0 开始）", 0),
            );
        }
        for n in &container.notes {
            info.detail("注意", n.clone());
        }
        if !vol.cksum_ok {
            info.detail("注意", "卷超级块校验和不匹配（可能是被改写的镜像）");
        }
        if vol.num_snaps > 0 {
            info.detail(
                "可恢复性",
                "有快照：删除的文件很可能还在快照的旧 fs-tree 里（快照恢复见下）",
            );
        } else {
            info.detail(
                "可恢复性",
                "无本地快照：APFS 删除会销毁目录项，按名字恢复基本无望，需靠雕刻",
            );
        }

        Ok(Self {
            reader,
            container,
            hashed_names: vol.hashed_drec(),
            vol,
            bs,
            root_paddr,
            omap,
            info,
            inode_cache: RefCell::new(HashMap::new()),
            paddr_cache: RefCell::new(HashMap::new()),
            truncated: std::cell::Cell::new(false),
        })
    }

    pub fn volume(&self) -> &Volume {
        &self.vol
    }

    pub fn container(&self) -> &Container {
        &self.container
    }

    pub fn block_size(&self) -> u32 {
        self.bs
    }

    /// 上一次 `deleted()` 是否因记录上限被截断（结果不完整）。
    pub fn truncated(&self) -> bool {
        self.truncated.get()
    }

    /// 给用户看的恢复路径说明（APFS 特有）。
    pub fn recovery_hint(&self) -> Vec<String> {
        super::recover::hint(self.vol.num_snaps)
    }

    /// 全树遍历（只给孤儿扫描用，别在热路径上调）。
    pub(crate) fn walk_all(&self, cb: &mut dyn FnMut(&[u8], &[u8]) -> bool) -> Result<usize> {
        let start = record::make_key(0, 0);
        let tree = self.tree();
        let mut res = self.resolver();
        tree.scan(self.root_paddr, &mut res, &start, cb)
    }

    pub(crate) fn set_truncated(&self, v: bool) {
        self.truncated.set(v);
    }

    fn tree(&self) -> Tree<'_> {
        Tree::fs_tree(&self.reader, self.bs)
    }

    fn resolver(&self) -> VolResolver<'_> {
        VolResolver {
            reader: &self.reader,
            bs: self.bs,
            omap: &self.omap,
            max_xid: self.vol.xid,
            cache: &self.paddr_cache,
        }
    }

    /// 点查 inode（带缓存）。
    pub fn inode(&self, id: u64) -> Result<Option<Inode>> {
        if let Some(hit) = self.inode_cache.borrow().get(&id) {
            return Ok(hit.clone());
        }
        let key = record::make_key(id, record::T_INODE);
        let val = self.tree().lookup(self.root_paddr, &mut self.resolver(), &key)?;
        let ino = val.and_then(|v| record::parse_inode(id, &v));
        self.inode_cache.borrow_mut().insert(id, ino.clone());
        Ok(ino)
    }

    /// 目录聚合大小 `(子项数, 总字节数)`，来自 APFS 自带的 dir stats 记录。
    ///
    /// 这是文件系统**增量维护**的累计值，不需要遍历子树：
    /// 在 700 万文件的卷上递归统计要几分钟，这个点查是 O(log n)。
    pub fn dir_stats(&self, id: u64) -> Option<(u64, u64)> {
        let key = record::make_key(id, record::T_DIR_STATS);
        let v = self
            .tree()
            .lookup(self.root_paddr, &mut self.resolver(), &key)
            .ok()??;
        record::parse_dir_stats(&v)
    }

    /// 诊断：统计 fs-tree 里每种记录的数量（**一次全树扫描**，几千万条时很慢）。
    ///
    /// 用来回答"这个卷到底有没有 dir stats / 快照记录"这类问题：
    /// 有记录而查不到，是键写错了；压根没有记录，就只能自己遍历。
    pub fn type_histogram(&self) -> Result<Vec<(u8, u64)>> {
        let mut counts: std::collections::BTreeMap<u8, u64> = std::collections::BTreeMap::new();
        let start = record::make_key(0, 0);
        let tree = self.tree();
        let mut res = self.resolver();
        tree.scan(self.root_paddr, &mut res, &start, &mut |k, _v| {
            let kind = record::key_kind(k);
            *counts.entry(kind).or_insert(0) += 1;
            true
        })?;
        Ok(counts.into_iter().collect())
    }

    /// 目录下的原始记录 `(名字, drec)`。
    fn dir_records(&self, dir: u64) -> Result<Vec<(String, record::Drec)>> {
        let start = record::make_key(dir, record::T_DIR_REC);
        let mut out = Vec::new();
        self.tree().scan(
            self.root_paddr,
            &mut self.resolver(),
            &start,
            &mut |k, v| {
                if record::key_obj_id(k) != dir || record::key_kind(k) != record::T_DIR_REC {
                    return false;
                }
                if let (Some(name), Some(d)) =
                    (record::drec_name(k, self.hashed_names), record::parse_drec(v))
                {
                    out.push((name, d));
                }
                true
            },
        )?;
        Ok(out)
    }

    /// 某对象的全部数据区段（按逻辑地址升序）。
    /// 按 inode id 扫 extent 记录。
    fn extents_by(&self, id: u64) -> Result<Vec<record::Extent>> {
        let start = record::make_key_with(
            id,
            record::T_FILE_EXTENT,
            &0u64.to_le_bytes(),
        );
        let mut out = Vec::new();
        self.tree().scan(
            self.root_paddr,
            &mut self.resolver(),
            &start,
            &mut |k, v| {
                if record::key_obj_id(k) != id || record::key_kind(k) != record::T_FILE_EXTENT {
                    return false;
                }
                if let Some(e) = record::parse_extent(k, v) {
                    out.push(e);
                }
                true
            },
        )?;
        Ok(out)
    }

    /// 文件的数据块记录。
    ///
    /// 键是**数据流 id**，不是 inode id：`j_inode_val_t.private_id` 才是
    /// 存数据的那个流。普通文件两者相同，但被克隆（clonefile / Time Machine
    /// 快照共享）、从 HFS+ 迁移来的文件会不同——只用 inode id 查，这类文件
    /// 一个 extent 都找不到，症状正是「列表里有大小、导出却是 0 字节」。
    /// Parallels 的 `CentOS Linux-0.hdd/*.hds` 就踩在这个坑上。
    pub fn extents_of(&self, id: u64) -> Result<Vec<record::Extent>> {
        let mut out = self.extents_by(id)?;
        if out.is_empty() {
            if let Some(ino) = self.inode(id)? {
                if ino.private_id != 0 && ino.private_id != id {
                    out = self.extents_by(ino.private_id)?;
                }
            }
        }
        out.sort_by_key(|e| e.logical);
        Ok(out)
    }

    /// 找 xattr 值（按名字）。符号链接目标就存在 `com.apple.fs.symlink` 里。
    fn xattr(&self, id: u64, want: &str) -> Result<Option<Vec<u8>>> {
        let start = record::make_key(id, record::T_XATTR);
        let mut found = None;
        self.tree().scan(
            self.root_paddr,
            &mut self.resolver(),
            &start,
            &mut |k, v| {
                if record::key_obj_id(k) != id || record::key_kind(k) != record::T_XATTR {
                    return false;
                }
                if record::xattr_name(k).as_deref() == Some(want) {
                    found = Some(v.to_vec());
                    return false;
                }
                true
            },
        )?;
        Ok(found)
    }

    /// 该对象的数据是否被 decmpfs 压缩（macOS 透明压缩）。
    fn is_compressed(&self, id: u64) -> bool {
        self.xattr(id, "com.apple.decmpfs")
            .map(|v| v.is_some())
            .unwrap_or(false)
    }

    fn entry_from(&self, parent: u64, name: &str, d: record::Drec) -> Entry {
        let ino = self.inode(d.file_id).ok().flatten();
        let kind = d
            .kind()
            .or_else(|| ino.as_ref().map(|i| i.kind()))
            .unwrap_or(NodeKind::File);
        let mut e = Entry::new(NodeId(d.file_id), NodeId(parent), name, kind);
        if let Some(i) = &ino {
            e.size = i.size;
            e.mtime = record::ns_to_unix(i.mtime);
            e.meta.push(("权限".into(), i.mode_string()));
            e.meta.push(("属主".into(), format!("{}:{}", i.uid, i.gid)));
            if i.nlink > 1 {
                e.meta.push(("链接数".into(), i.nlink.to_string()));
            }
            if i.alloced > i.size && i.size > 0 {
                e.meta.push(("占用".into(), format!("{}B", i.alloced)));
            }
        }
        e
    }

    /// 文件首个物理块（给恢复/雕刻用的线索）。列表不逐条取，避免 N+1 查询。
    pub fn first_extent(&self, id: u64) -> Option<u64> {
        self.extents_of(id).ok().and_then(|v| v.first().map(|e| e.phys))
    }
}

/// 去掉符号链接 xattr 值前面的 4 字节头，并裁掉结尾 NUL。
fn symlink_target(v: &[u8]) -> Vec<u8> {
    let mut body = v;
    if v.len() >= 4 {
        let declared = u16::from_le_bytes([v[2], v[3]]) as usize;
        if declared + 4 == v.len() {
            body = &v[4..];
        }
    }
    match body.iter().position(|c| *c == 0) {
        Some(p) => body[..p].to_vec(),
        None => body.to_vec(),
    }
}

/// 补零（稀疏文件 / 空洞）。
fn write_zeros(out: &mut dyn Write, zeros: &[u8], mut n: u64) -> Result<u64> {
    let total = n;
    while n > 0 {
        let k = (n as usize).min(zeros.len());
        out.write_all(&zeros[..k])?;
        n -= k as u64;
    }
    Ok(total)
}

impl FileSystem for Apfs {
    fn info(&self) -> &FsInfo {
        &self.info
    }

    fn root(&self) -> NodeId {
        NodeId(ROOT_DIR_ID)
    }

    fn list(&self, dir: NodeId) -> Result<Vec<Entry>> {
        let recs = self.dir_records(dir.0)?;
        let mut out: Vec<Entry> = recs
            .into_iter()
            .map(|(name, d)| self.entry_from(dir.0, &name, d))
            .collect();
        out.sort_by(|a, b| match (a.is_dir(), b.is_dir()) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
        });
        Ok(out)
    }

    fn read(&self, entry: &Entry, out: &mut dyn Write) -> Result<u64> {
        let id = entry.id.0;
        // 符号链接：目标是 xattr `com.apple.fs.symlink` 的值。
        // 该值不是裸字符串，前面有 4 字节头 `{u16 kind; u16 含 NUL 的长度}`，
        // 例如 `06 00 0a 00 "a/big.bin\0"`——直接输出会多出控制字符。
        if entry.kind == NodeKind::Symlink {
            if let Some(t) = self.xattr(id, "com.apple.fs.symlink")? {
                let target = symlink_target(&t);
                out.write_all(&target)?;
                return Ok(target.len() as u64);
            }
        }
        if self.is_compressed(id) {
            return Err(Error::Unsupported(
                "该文件被 macOS 透明压缩（decmpfs），内容不是原始数据；\
                 导出后用 `ditto` 或 `afsctool -d` 解压"
                    .into(),
            ));
        }

        let ino = self.inode(id)?;
        let exts = self.extents_of(id)?;
        if exts.is_empty() {
            // 有 inode、有大小、却没有任何数据块记录 = 索引没了，不是空文件。
            // 这里必须报错：以前一律 `Ok(0)`，导出端把 0 字节当成功，
            // 于是一整批文件被写成空壳，用户还看到"0 个失败"。
            let size = ino.as_ref().map(|i| i.size).unwrap_or(0);
            if size > 0 {
                return Err(Error::Unsupported(format!(
                    "找不到数据块记录（size={size}, inode={id}, private_id={}）——\
                     该文件的数据可能已被回收，或 extent 索引损坏",
                    ino.as_ref().map(|i| i.private_id).unwrap_or(0)
                )));
            }
            // 真的是 0 字节空文件
            return Ok(0);
        }
        let mut limit = ino.as_ref().map(|i| i.size).unwrap_or(0);
        if limit == 0 {
            // 没有 DSTREAM（孤儿/损坏）：退化为按 extent 覆盖范围读
            limit = exts
                .iter()
                .map(|e| e.logical.saturating_add(e.len))
                .max()
                .unwrap_or(0);
        }

        let mut written = 0u64;
        let mut buf = vec![0u8; IO_CHUNK];
        let zeros = vec![0u8; IO_CHUNK];
        // 按逻辑地址推进，遇到「缝隙」（没有 extent 覆盖）或「空洞」（phys=0）
        // 必须补零，否则后面的数据会前移，稀疏文件整体错位。
        let mut pos = 0u64;
        for e in &exts {
            if e.logical >= limit {
                break;
            }
            if e.logical > pos {
                written += write_zeros(out, &zeros, e.logical.min(limit) - pos)?;
                pos = e.logical.min(limit);
            }
            let end = e.logical.saturating_add(e.len).min(limit);
            if end <= pos {
                continue;
            }
            let want = end - pos;
            if e.is_hole() {
                written += write_zeros(out, &zeros, want)?;
                pos = end;
                continue;
            }
            let mut done = 0u64;
            while done < want {
                let n = ((want - done) as usize).min(buf.len());
                let off = e
                    .phys
                    .saturating_mul(self.bs as u64)
                    .saturating_add(done);
                self.reader.read(off, &mut buf[..n])?;
                out.write_all(&buf[..n])?;
                done += n as u64;
                written += n as u64;
            }
            pos = end;
        }
        // 末尾缝隙（文件尾部被稀疏化）
        if pos < limit {
            written += write_zeros(out, &zeros, limit - pos)?;
        }
        Ok(written)
    }

    /// 有 inode 但没有目录项指向它（孤儿）——APFS 上真删除通常连 inode 一起回收，
    /// 所以返回空是常态。展开说明见 [`super::recover`]。
    fn dir_stats(&self, dir: NodeId) -> Option<(u64, u64)> {
        Apfs::dir_stats(self, dir.0)
    }

    fn deleted(&self) -> Result<Vec<Entry>> {
        super::recover::orphan_scan(self)
    }

    /// APFS 没有"目录项可救"的删除记录，但可以扫**孤儿 inode**
    /// （有 inode、没有 drec 指向它）。真删时通常连 inode 一起回收，
    /// 所以返回空是常态 —— 但能扫就返回 true，别直接把用户挡在门外。
    fn supports_deleted(&self) -> bool {
        true
    }
}
