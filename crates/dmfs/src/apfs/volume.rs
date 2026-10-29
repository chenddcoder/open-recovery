//! APFS 卷超级块（`apfs_superblock_t` / APSB）与卷级对象映射。

use super::bnode::BTREE_INFO_SIZE;
use super::obj;
use super::omap::{self, Omap};
use dmdisk::{Error, Result, Reader};

/// `apfs_incompat_features`
pub const INCOMPAT_CASE_INSENSITIVE: u64 = 0x0000_0001;
pub const INCOMPAT_DATALESS_SNAPS: u64 = 0x0000_0002;
pub const INCOMPAT_ENC_ROLLED: u64 = 0x0000_0004;
pub const INCOMPAT_NORMALIZATION_INSENSITIVE: u64 = 0x0000_0008;
pub const INCOMPAT_SEALED_VOLUME: u64 = 0x0000_0020;

/// `apfs_fs_flags`
pub const FS_UNENCRYPTED: u64 = 0x1;

pub struct Volume {
    pub apsb_paddr: u64,
    pub xid: u64,
    pub name: String,
    pub uuid: String,
    pub role: u16,
    pub features: u64,
    pub ro_compat: u64,
    pub incompat: u64,
    pub fs_flags: u64,
    pub last_mod: u64,
    pub alloc_count: u64,
    pub num_files: u64,
    pub num_dirs: u64,
    pub num_symlinks: u64,
    pub num_other: u64,
    pub num_snaps: u64,
    pub total_blocks_alloced: u64,
    pub total_blocks_freed: u64,
    /// 卷 omap 对象的块号
    pub omap_paddr: u64,
    pub root_tree_oid: u64,
    pub snap_meta_oid: u64,
    pub cksum_ok: bool,
}

fn u16le(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

fn u64le(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes([
        b[o],
        b[o + 1],
        b[o + 2],
        b[o + 3],
        b[o + 4],
        b[o + 5],
        b[o + 6],
        b[o + 7],
    ])
}

fn cstr(b: &[u8]) -> String {
    let end = b.iter().position(|c| *c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).to_string()
}

pub fn open_at(reader: &Reader, bs: u32, apsb_paddr: u64) -> Result<Volume> {
    let buf = obj::read_block(reader, bs, apsb_paddr)?;
    if buf.len() < BTREE_INFO_SIZE + 0x2c0 {
        return Err(Error::Format(format!(
            "块 {apsb_paddr} 太小，不是 APFS 卷超级块"
        )));
    }
    if &buf[0x20..0x24] != obj::APSB_MAGIC {
        return Err(Error::Format(format!(
            "块 {apsb_paddr} 没有 APSB 魔数（不是卷超级块）"
        )));
    }
    let hdr = obj::parse_header(&buf)?;
    Ok(Volume {
        apsb_paddr,
        xid: hdr.xid,
        name: cstr(&buf[0x2c0..0x3c0]),
        uuid: super::container::uuid_string(&buf[0xf0..0x100]),
        role: u16le(&buf, 0x3c4),
        features: u64le(&buf, 0x28),
        ro_compat: u64le(&buf, 0x30),
        incompat: u64le(&buf, 0x38),
        fs_flags: u64le(&buf, 0x108),
        last_mod: u64le(&buf, 0x100),
        alloc_count: u64le(&buf, 0x58),
        num_files: u64le(&buf, 0xb8),
        num_dirs: u64le(&buf, 0xc0),
        num_symlinks: u64le(&buf, 0xc8),
        num_other: u64le(&buf, 0xd0),
        num_snaps: u64le(&buf, 0xd8),
        total_blocks_alloced: u64le(&buf, 0xe0),
        total_blocks_freed: u64le(&buf, 0xe8),
        omap_paddr: u64le(&buf, 0x80),
        root_tree_oid: u64le(&buf, 0x88),
        snap_meta_oid: u64le(&buf, 0x98),
        cksum_ok: hdr.cksum_ok,
    })
}

impl Volume {
    /// 是否加密卷（FileVault）。加密卷的 fs-tree 键值不可读，只能识别不能解析。
    pub fn is_encrypted(&self) -> bool {
        self.fs_flags & FS_UNENCRYPTED == 0
    }

    /// 是否大小写不敏感（只由 0x1 位决定）。
    ///
    /// 注意别和键格式混为一谈：**规范化不敏感（0x8）的卷也会用 hashed drec 键**，
    /// 但它仍然是大小写敏感的（实测 `newfs_apfs -e` 的卷就是 0x8 + hashed 键）。
    pub fn case_insensitive(&self) -> bool {
        self.incompat & INCOMPAT_CASE_INSENSITIVE != 0
    }

    /// drec 键是否带 22 位名字哈希（大小写不敏感 **或** 规范化不敏感）。
    pub fn hashed_drec(&self) -> bool {
        self.incompat & (INCOMPAT_CASE_INSENSITIVE | INCOMPAT_NORMALIZATION_INSENSITIVE) != 0
    }

    /// 系统卷（sealed）：目录树受哈希树保护，只读访问不受影响。
    pub fn is_sealed(&self) -> bool {
        self.incompat & INCOMPAT_SEALED_VOLUME != 0
    }

    pub fn role_name(&self) -> &'static str {
        match self.role {
            0x0000 => "普通卷",
            0x0001 => "系统卷",
            0x0002 => "用户数据卷",
            0x0004 => "恢复卷",
            0x0008 => "虚拟机卷",
            0x0010 => "预启动卷",
            0x0020 => "Time Machine 备份",
            0x0040 => "Sidecar",
            _ => "其它",
        }
    }

    pub fn omap(&self, reader: &Reader, bs: u32) -> Result<Omap> {
        omap::open(reader, bs, self.omap_paddr)
    }

    /// fs-tree 根节点所在的物理块号。
    pub fn root_tree_paddr(&self, reader: &Reader, bs: u32) -> Result<u64> {
        let om = self.omap(reader, bs)?;
        om.resolve(reader, bs, self.root_tree_oid, self.xid)
    }

    /// 快照元数据树的物理块号（有快照时才有意义）。
    pub fn snap_meta_paddr(&self, reader: &Reader, bs: u32) -> Result<Option<u64>> {
        if self.snap_meta_oid == 0 {
            return Ok(None);
        }
        let om = self.omap(reader, bs)?;
        Ok(om.lookup(reader, bs, self.snap_meta_oid, self.xid)?.map(|v| v.paddr))
    }
}
