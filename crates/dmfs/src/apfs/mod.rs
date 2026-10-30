//! APFS（Apple File System）只读解析。
//!
//! 分层（自下而上）：
//! ```text
//! obj      ── 32B 对象头 + Fletcher64 校验
//! bnode    ── btree_node_phys 解析（TOC / 键区 / 值区 / root 尾部）
//! btree    ── 点查与范围扫描，物理树/虚拟树统一
//! omap     ── (oid, xid) → paddr
//! container── NXSB、检查点、容器 omap、多卷
//! volume   ── APSB、卷 omap、fs-tree 根
//! record   ── j_key 家族：drec / inode / xfield / extent
//! fs       ── FileSystem 实现（惰性查询，不预载整棵树）
//! ```
//!
//! 与 FAT/NTFS 最大的不同：APFS 是**对象化 + 写时复制**的。同一个逻辑对象在盘上
//! 有多份历史版本，靠 oid/xid 索引；所以"读旧版本"在原理上可行，
//! 这也是快照恢复的基础。

pub mod bnode;
pub mod btree;
pub mod container;
pub mod fs;
pub mod obj;
pub mod omap;
pub mod record;
pub mod recover;
pub mod repair;
pub mod volume;

pub use container::Container;
pub use fs::Apfs;
pub use volume::Volume;

use crate::Result;
use dmdisk::Reader;

/// 容器里一个卷的解析结果（坏卷不拖垮整盘浏览）。
pub type VolumeSlot = (usize, Result<Volume>);

/// 只解析容器和卷超级块，不碰目录树（快，用于列卷）。
pub fn volumes(reader: &Reader) -> Result<(Container, Vec<VolumeSlot>)> {
    let c = Container::open(reader)?;
    let mut list = Vec::with_capacity(c.fs_oids.len());
    for i in 0..c.fs_oids.len() {
        list.push((i, c.volume(reader, i)));
    }
    Ok((c, list))
}

/// 打开第 `index` 个卷。
pub fn open_volume(reader: Reader, index: usize) -> Result<Apfs> {
    let c = Container::open(&reader)?;
    let vol = c.volume(&reader, index)?;
    Apfs::from_parts(reader, c, vol)
}
