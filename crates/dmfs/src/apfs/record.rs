//! fs-tree 记录解析：`j_key` 家族。
//!
//! 每条记录的键都以 `j_key_t` 开头：低 60 位是对象 id，高 4 位是记录类型。
//! 同一个 id 会按类型分组出现（inode / drec / extent / xattr），
//! 排序规则让"按父目录前缀扫描"变成一次 B-Tree 范围查询。

use crate::types::NodeKind;

pub const T_SNAP_METADATA: u8 = 1;
pub const T_EXTENT: u8 = 2;
pub const T_INODE: u8 = 3;
pub const T_XATTR: u8 = 4;
pub const T_SIBLING_LINK: u8 = 5;
pub const T_DSTREAM_ID: u8 = 6;
pub const T_CRYPTO_STATE: u8 = 7;
pub const T_FILE_EXTENT: u8 = 8;
pub const T_DIR_REC: u8 = 9;
pub const T_DIR_STATS: u8 = 10;
pub const T_SNAP_NAME: u8 = 11;
pub const T_SIBLING_MAP: u8 = 12;

/// inode 记录的固定部分长度（`j_inode_val_t`，不含 xfields）。
/// apfs.h 里是 86，但 macOS 26 实测多 6 字节尾部填充；
/// 这里不写死，用 [`xf_offset`] 按内容自校验定位。
const INODE_FIXED: usize = 86;

/// xfield 类型
pub const INO_EXT_TYPE_NAME: u8 = 4;
pub const INO_EXT_TYPE_DSTREAM: u8 = 8;
pub const INO_EXT_TYPE_FINDER_INFO: u8 = 7;
pub const INO_EXT_TYPE_RDEV: u8 = 14;

fn u16le(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
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

pub fn key_obj_id(k: &[u8]) -> u64 {
    if k.len() < 8 {
        return 0;
    }
    u64le(k, 0) & 0x0FFF_FFFF_FFFF_FFFF
}

pub fn key_kind(k: &[u8]) -> u8 {
    if k.len() < 8 {
        return 0;
    }
    (k[7] >> 4) as u8
}

pub fn make_key(obj: u64, kind: u8) -> Vec<u8> {
    make_key_with(obj, kind, &[])
}

pub fn make_key_with(obj: u64, kind: u8, extra: &[u8]) -> Vec<u8> {
    let hdr = (obj & 0x0FFF_FFFF_FFFF_FFFF) | ((kind as u64) << 60);
    let mut k = Vec::with_capacity(8 + extra.len());
    k.extend_from_slice(&hdr.to_le_bytes());
    k.extend_from_slice(extra);
    k
}

/// 目录项记录（`j_drec_key_t`）里的文件名。
///
/// 两种键长格式并存：
/// * case-insensitive 卷用 `j_drec_hashed_key_t`：`u32 name_len_and_hash`
///   （**低 10 位**才是名字长度），名字从 12 字节处开始；
/// * 其余卷用 `j_drec_key_t`：`u16 name_len`，名字从 10 字节处开始。
///
/// 不靠卷标志硬猜，而是用「名字长度必须等于键剩余长度」来判定，两种格式都能吃。
pub fn drec_name(k: &[u8], prefer_hashed: bool) -> Option<String> {
    let parse = |off: usize, len: usize| -> Option<String> {
        if len == 0 || off + len > k.len() {
            return None;
        }
        let mut raw = &k[off..off + len];
        if let Some(p) = raw.iter().position(|c| *c == 0) {
            raw = &raw[..p];
        }
        Some(String::from_utf8_lossy(raw).to_string())
    };

    let hashed = |k: &[u8]| -> Option<String> {
        if k.len() < 12 {
            return None;
        }
        let nl = (u32le(k, 8) as usize) & 0x3FF;
        if nl == k.len() - 12 {
            parse(12, nl.saturating_sub(1))
        } else {
            None
        }
    };
    let plain = |k: &[u8]| -> Option<String> {
        if k.len() < 10 {
            return None;
        }
        let nl = u16le(k, 8) as usize;
        if nl == k.len() - 10 {
            parse(10, nl.saturating_sub(1))
        } else {
            None
        }
    };

    if prefer_hashed {
        hashed(k).or_else(|| plain(k))
    } else {
        plain(k).or_else(|| hashed(k))
    }
}

/// 目录项值（`j_drec_val_t`）
#[derive(Debug, Clone, Copy)]
pub struct Drec {
    pub file_id: u64,
    pub date_added: u64,
    pub flags: u16,
}

pub fn parse_drec(v: &[u8]) -> Option<Drec> {
    if v.len() < 18 {
        return None;
    }
    Some(Drec {
        file_id: u64le(v, 0),
        date_added: u64le(v, 8),
        flags: u16le(v, 16),
    })
}

/// 目录项 flags
pub const DREC_TYPE_MASK: u16 = 0x000F;
pub const DREC_TYPE_FILE: u16 = 8;
pub const DREC_TYPE_DIR: u16 = 4;
pub const DREC_TYPE_SYMLINK: u16 = 10;

impl Drec {
    pub fn kind(&self) -> Option<NodeKind> {
        match self.flags & DREC_TYPE_MASK {
            DREC_TYPE_DIR => Some(NodeKind::Dir),
            DREC_TYPE_FILE => Some(NodeKind::File),
            DREC_TYPE_SYMLINK => Some(NodeKind::Symlink),
            _ => None,
        }
    }
}

/// inode 记录。
#[derive(Debug, Clone, Default)]
pub struct Inode {
    pub id: u64,
    pub parent: u64,
    pub private_id: u64,
    pub create_time: u64,
    pub mtime: u64,
    pub ctime: u64,
    pub atime: u64,
    pub internal_flags: u64,
    pub nlink: u32,
    pub mode: u16,
    pub uid: u32,
    pub gid: u32,
    pub bsd_flags: u32,
    /// 逻辑大小（来自 DSTREAM xfield）
    pub size: u64,
    /// 实际占用字节（来自 DSTREAM xfield）
    pub alloced: u64,
    /// inode 自带的名字（仅部分 inode 有，用于无目录项的孤儿）
    pub name: Option<String>,
    pub has_finder_info: bool,
}

impl Inode {
    pub fn kind(&self) -> NodeKind {
        match self.mode & 0xF000 {
            0x4000 => NodeKind::Dir,
            0x8000 => NodeKind::File,
            0xA000 => NodeKind::Symlink,
            _ => NodeKind::Other,
        }
    }

    pub fn mode_string(&self) -> String {
        let t = match self.kind() {
            NodeKind::Dir => 'd',
            NodeKind::File => '-',
            NodeKind::Symlink => 'l',
            NodeKind::Other => '?',
        };
        let bit = |sh: u16, c: char| if self.mode & (1 << sh) != 0 { c } else { '-' };
        format!(
            "{}{}{}{}{}{}{}{}{}{}",
            t,
            bit(8, 'r'),
            bit(7, 'w'),
            bit(6, 'x'),
            bit(5, 'r'),
            bit(4, 'w'),
            bit(3, 'x'),
            bit(2, 'r'),
            bit(1, 'w'),
            bit(0, 'x'),
        )
    }
}

/// xfields 在 inode 值里的起点。
///
/// 判定方式：`u16 num_exts` 合理、每个字段头（类型/长度）自洽、数据区不越界。
/// 三个条件同时成立才认，避免把随机字节当成字段表。
pub fn xf_offset(v: &[u8]) -> Option<usize> {
    // apfs.h 是 86；macOS 26 实测 92。两个都试，先试观测值。
    for cand in [92usize, INODE_FIXED, 88, 90] {
        if valid_xfields(v, cand) {
            return Some(cand);
        }
    }
    None
}

fn valid_xfields(v: &[u8], off: usize) -> bool {
    if off + 4 > v.len() {
        return false;
    }
    let n = u16le(v, off) as usize;
    if n == 0 || n > 16 {
        return false;
    }
    let hdr = off + 2 + n + (n & 1);
    if hdr + n * 4 > v.len() {
        return false;
    }
    let mut data = hdr + n * 4;
    for i in 0..n {
        let p = hdr + i * 4;
        let ty = v[p];
        let sz = u16le(v, p + 2) as usize;
        if ty == 0 || ty > 20 || sz > 4096 {
            return false;
        }
        data += (sz + 7) & !7;
        if data > v.len() {
            return false;
        }
    }
    true
}

/// 遍历 xfields，回调 `(类型, 数据)`。
pub fn for_each_xfield<F: FnMut(u8, &[u8])>(v: &[u8], mut f: F) {
    let off = match xf_offset(v) {
        Some(o) => o,
        None => return,
    };
    let n = u16le(v, off) as usize;
    let hdr = off + 2 + n + (n & 1);
    let mut data = hdr + n * 4;
    for i in 0..n {
        let p = hdr + i * 4;
        let ty = v[p];
        let sz = u16le(v, p + 2) as usize;
        let end = (data + sz).min(v.len());
        if data < end {
            f(ty, &v[data..end]);
        }
        data += (sz + 7) & !7;
        if data > v.len() {
            break;
        }
    }
}

pub fn parse_inode(id: u64, v: &[u8]) -> Option<Inode> {
    if v.len() < INODE_FIXED {
        return None;
    }
    let mut ino = Inode {
        id,
        parent: u64le(v, 0),
        private_id: u64le(v, 8),
        create_time: u64le(v, 16),
        mtime: u64le(v, 24),
        ctime: u64le(v, 32),
        atime: u64le(v, 40),
        internal_flags: u64le(v, 48),
        nlink: u32le(v, 56),
        mode: u16le(v, 80),
        uid: u32le(v, 72),
        gid: u32le(v, 76),
        bsd_flags: u32le(v, 68),
        ..Default::default()
    };
    for_each_xfield(v, |ty, data| match ty {
        INO_EXT_TYPE_DSTREAM => {
            if data.len() >= 16 {
                ino.size = u64le(data, 0);
                ino.alloced = u64le(data, 8);
            }
        }
        INO_EXT_TYPE_NAME => {
            if !data.is_empty() {
                let mut raw = data;
                if let Some(p) = raw.iter().position(|c| *c == 0) {
                    raw = &raw[..p];
                }
                ino.name = Some(String::from_utf8_lossy(raw).to_string());
            }
        }
        INO_EXT_TYPE_FINDER_INFO => ino.has_finder_info = true,
        _ => {}
    });
    Some(ino)
}

/// 文件数据区段（`j_file_extent_val_t`）
#[derive(Debug, Clone, Copy)]
pub struct Extent {
    /// 文件内逻辑偏移
    pub logical: u64,
    /// 长度（字节，56 位）
    pub len: u64,
    /// 起始物理块号
    pub phys: u64,
    pub crypto_id: u64,
}

impl Extent {
    pub fn is_hole(&self) -> bool {
        self.phys == 0
    }
}

pub fn parse_extent(k: &[u8], v: &[u8]) -> Option<Extent> {
    if k.len() < 16 || v.len() < 24 {
        return None;
    }
    let len_and_flags = u64le(v, 0);
    Some(Extent {
        logical: u64le(k, 8),
        len: len_and_flags & 0x00FF_FFFF_FFFF_FFFF,
        phys: u64le(v, 8),
        crypto_id: u64le(v, 16),
    })
}

/// APFS 时间戳（1970 起的**纳秒**）→ Unix 秒
pub fn ns_to_unix(ns: u64) -> Option<i64> {
    if ns == 0 {
        return None;
    }
    Some((ns / 1_000_000_000) as i64)
}

/// xattr 名（与 drec 类似但只有一种格式：u16 长度 + 名字）
pub fn xattr_name(k: &[u8]) -> Option<String> {
    if k.len() < 10 {
        return None;
    }
    let nl = u16le(k, 8) as usize;
    if nl == 0 || nl != k.len() - 10 {
        return None;
    }
    let mut raw = &k[10..10 + nl];
    if let Some(p) = raw.iter().position(|c| *c == 0) {
        raw = &raw[..p];
    }
    Some(String::from_utf8_lossy(raw).to_string())
}
