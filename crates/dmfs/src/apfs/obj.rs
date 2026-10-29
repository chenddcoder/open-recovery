//! APFS 对象头与校验和。
//!
//! 所有 APFS 元数据都是"对象"：32 字节头 + 负载。头里带 Fletcher64 校验和、
//! 对象 id、事务 id（xid）和类型。同一 oid 的多个版本按 xid 共存于容器不同块上，
//! 这是 APFS 写时复制的根基，也是"按 xid 读旧版本"能救删除文件的原因。

use dmdisk::{Error, Result};

pub const NX_MAGIC: &[u8; 4] = b"NXSB";
pub const APSB_MAGIC: &[u8; 4] = b"APSB";

/// 对象类型（低 16 位）。
pub const OBJ_NX_SUPERBLOCK: u32 = 0x01;
pub const OBJ_BTREE: u32 = 0x02;
pub const OBJ_BTREE_NODE: u32 = 0x03;
pub const OBJ_SPACEMAN: u32 = 0x05;
pub const OBJ_OMAP: u32 = 0x0b;
pub const OBJ_CHECKPOINT_MAP: u32 = 0x0c;
pub const OBJ_FS: u32 = 0x0d;
pub const OBJ_FSTREE: u32 = 0x0e;
pub const OBJ_SNAPMETATREE: u32 = 0x0f;

/// 对象存储类别（高 4 位）。
pub const OBJ_FLAG_EPHEMERAL: u32 = 0x8000_0000;
pub const OBJ_FLAG_PHYSICAL: u32 = 0x4000_0000;
pub const OBJ_FLAG_NOHEADER: u32 = 0x2000_0000;
pub const OBJ_FLAG_ENCRYPTED: u32 = 0x1000_0000;

pub const OBJ_TYPE_MASK: u32 = 0x0000_ffff;

/// 解析出的对象头。
#[derive(Debug, Clone, Copy)]
pub struct ObjHeader {
    pub cksum: u64,
    pub oid: u64,
    pub xid: u64,
    pub otype: u32,
    pub subtype: u32,
    /// 校验和是否通过（整块 Fletcher 和为 0）
    pub cksum_ok: bool,
}

impl ObjHeader {
    pub fn type_base(&self) -> u32 {
        self.otype & OBJ_TYPE_MASK
    }

    pub fn is_physical(&self) -> bool {
        self.otype & OBJ_FLAG_PHYSICAL != 0
    }

    /// 类型名（用于报错和调试输出）
    pub fn type_name(&self) -> &'static str {
        match self.type_base() {
            OBJ_NX_SUPERBLOCK => "NXSB",
            OBJ_BTREE => "BTREE",
            OBJ_BTREE_NODE => "BTREE_NODE",
            OBJ_SPACEMAN => "SPACEMAN",
            OBJ_OMAP => "OMAP",
            OBJ_CHECKPOINT_MAP => "CHECKPOINT_MAP",
            OBJ_FS => "APSB",
            OBJ_FSTREE => "FSTREE",
            OBJ_SNAPMETATREE => "SNAPMETATREE",
            _ => "对象",
        }
    }
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

/// 解析对象头。`block` 是整块（从块首开始）。
pub fn parse_header(block: &[u8]) -> Result<ObjHeader> {
    if block.len() < 32 {
        return Err(Error::Format("APFS 对象头不足 32 字节".into()));
    }
    Ok(ObjHeader {
        cksum: u64le(block, 0),
        oid: u64le(block, 8),
        xid: u64le(block, 16),
        otype: u32le(block, 24),
        subtype: u32le(block, 28),
        cksum_ok: verify(block),
    })
}

/// APFS 的 Fletcher64 变体：对 4 字节小端字累加。
///
/// 返回 `(sum1, sum2)`，两者都对 `0xFFFF_FFFF` 取模（不是 2^32）。
pub fn fletcher64(data: &[u8]) -> (u64, u64) {
    const M: u64 = 0xFFFF_FFFF;
    let mut s1: u64 = 0;
    let mut s2: u64 = 0;
    let mut i = 0;
    while i + 4 <= data.len() {
        let w = u32le(data, i) as u64;
        s1 = (s1 + w) % M;
        s2 = (s2 + s1) % M;
        i += 4;
    }
    // 尾部不足 4 字节的按补零处理（正常块大小是 4 的倍数，这里只是容错）
    if i < data.len() {
        let mut tail = [0u8; 4];
        tail[..data.len() - i].copy_from_slice(&data[i..]);
        s1 = (s1 + u32::from_le_bytes(tail) as u64) % M;
        s2 = (s2 + s1) % M;
    }
    (s1, s2)
}

/// 计算一个块的校验和（`block[8..]` 参与计算）。
///
/// 由实测反推：`cksum = (s2 << 32) | (M - ((s1 + s2) % M))`。
/// 反过来验算即可得到"整块（含 cksum 字段）的 s1 恒为 0"这个不变量。
pub fn checksum(block: &[u8]) -> u64 {
    const M: u64 = 0xFFFF_FFFF;
    let (s1, s2) = fletcher64(&block[8..]);
    let lo = (M - ((s1 + s2) % M)) & 0xFFFF_FFFF;
    (s2 << 32) | lo
}

/// 校验整块。APFS 的设计保证：把校验和字段本身也算进去时，s1 ≡ 0。
pub fn verify(block: &[u8]) -> bool {
    if block.len() < 32 || block.len() % 4 != 0 {
        return false;
    }
    fletcher64(block).0 == 0
}

/// 读一个块。
pub fn read_block(reader: &dmdisk::Reader, bs: u32, paddr: u64) -> Result<Vec<u8>> {
    reader.read_vec(paddr.saturating_mul(bs as u64), bs as usize)
}
