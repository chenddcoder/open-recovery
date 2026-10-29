//! APFS B-Tree 节点（`btree_node_phys_t`）。
//!
//! 节点布局（块首起）：
//! ```text
//! 0x00 obj_phys_t        32B  对象头（含 Fletcher64 校验和）
//! 0x20 btn_flags         u16  ROOT/LEAF/FIXED_KV/...
//! 0x22 btn_level         u16  0 = 叶
//! 0x24 btn_nkeys         u32
//! 0x28 btn_table_space   nloc { u16 off; u16 len }   相对 btn_data(0x38)
//! 0x2c btn_free_space    nloc
//! 0x30 btn_key_free_list nloc
//! 0x34 btn_val_free_list nloc
//! 0x38 btn_data[]        表项(TOC) | 空闲 | 键区 → …… ← 值区 | [btree_info]
//! ```
//!
//! 两条实测出来的关键规则（容易踩坑）：
//! 1. **键区起点** = `0x38 + table_space.off + table_space.len`；
//! 2. **值定位**：TOC 里的 `v` 是「值区末尾往回数的偏移」，
//!    值起点 = `块尾 - (root ? 40 : 0) - v`。root 节点末尾有 40 字节 `btree_info_t`，
//!    不减掉它就会整体错位 40 字节——这是最容易误以为"记录解析失败"的地方。

use dmdisk::{Error, Result};

pub const BTNODE_ROOT: u16 = 0x0001;
pub const BTNODE_LEAF: u16 = 0x0002;
pub const BTNODE_FIXED_KV: u16 = 0x0004;
pub const BTNODE_HASHED: u16 = 0x0008;
pub const BTNODE_NOHEADER: u16 = 0x0010;

/// root 节点末尾的 `btree_info_t` 大小
pub const BTREE_INFO_SIZE: usize = 40;
const HDR_SIZE: usize = 0x38;

fn u16le(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

/// 解析后的节点。
pub struct BNode {
    pub paddr: u64,
    pub flags: u16,
    pub level: u16,
    /// 固定长度键值（omap / spaceman 这类）
    pub fixed: bool,
    pub is_root: bool,
    /// (键, 值) —— 已切好，键不补零
    pub entries: Vec<(Vec<u8>, Vec<u8>)>,
}

impl BNode {
    pub fn is_leaf(&self) -> bool {
        self.level == 0
    }

    /// 解析一个节点块。
    ///
    /// 固定长度节点必须给出 `key_len` / `val_len`（TOC 里只存偏移，不存长度）。
    pub fn parse(
        paddr: u64,
        buf: &[u8],
        key_len: Option<usize>,
        val_len: Option<usize>,
    ) -> Result<Self> {
        if buf.len() < HDR_SIZE + BTREE_INFO_SIZE {
            return Err(Error::Format(format!(
                "APFS 节点块太小（{} 字节，块 {paddr}）",
                buf.len()
            )));
        }
        let flags = u16le(buf, 0x20);
        let level = u16le(buf, 0x22);
        let nkeys = u32le(buf, 0x24) as usize;
        let toff = u16le(buf, 0x28) as usize;
        let tlen = u16le(buf, 0x2a) as usize;
        let fixed = flags & BTNODE_FIXED_KV != 0;
        let is_root = flags & BTNODE_ROOT != 0;

        let key_base = HDR_SIZE + toff + tlen;
        let val_base = buf.len() - if is_root { BTREE_INFO_SIZE } else { 0 };
        let step = if fixed { 4 } else { 8 };

        // 表项本身必须在键区之前，否则说明 nkeys/table_space 是坏的
        if key_base > val_base || nkeys.saturating_mul(step) > key_base.saturating_sub(HDR_SIZE) {
            return Err(Error::Format(format!(
                "APFS 节点 {paddr} 表项越界：nkeys={nkeys} key_base={key_base} val_base={val_base}"
            )));
        }
        if fixed && (key_len.is_none() || val_len.is_none()) {
            return Err(Error::Format(format!(
                "APFS 节点 {paddr} 是定长键值，但未提供键/值长度"
            )));
        }

        let mut entries = Vec::with_capacity(nkeys.min(4096));
        for i in 0..nkeys {
            let t = HDR_SIZE + toff + i * step;
            let (ko, klen, vo, vlen) = if fixed {
                // 索引节点（level>0）的值固定是 8 字节子节点 oid，
                // 与叶子节点的 val_size 无关。按 16 字节取会把值区顶出块外，
                // 表现为「第 0 项值越界（4048..4064）」这种差 8 字节的报错。
                let vl = if level > 0 { 8 } else { val_len.unwrap() };
                (u16le(buf, t) as usize, key_len.unwrap(), u16le(buf, t + 2) as usize, vl)
            } else {
                (
                    u16le(buf, t) as usize,
                    u16le(buf, t + 2) as usize,
                    u16le(buf, t + 4) as usize,
                    u16le(buf, t + 6) as usize,
                )
            };
            let ks = key_base + ko;
            let ke = ks + klen;
            if ke > val_base {
                return Err(Error::Format(format!(
                    "APFS 节点 {paddr} 第 {i} 项键越界（{ks}..{ke} > {val_base}）"
                )));
            }
            let vs = match val_base.checked_sub(vo) {
                Some(v) => v,
                None => {
                    return Err(Error::Format(format!(
                        "APFS 节点 {paddr} 第 {i} 项值偏移非法（v={vo}）"
                    )))
                }
            };
            let ve = vs + vlen;
            if ve > val_base || vs < ke {
                return Err(Error::Format(format!(
                    "APFS 节点 {paddr} 第 {i} 项值越界（{vs}..{ve}）"
                )));
            }
            entries.push((buf[ks..ke].to_vec(), buf[vs..ve].to_vec()));
        }
        Ok(Self { paddr, flags, level, fixed, is_root, entries })
    }

    /// root 节点的 `btree_info_t`（用于识别树类型）。
    pub fn btree_info(buf: &[u8], is_root: bool) -> Option<BTreeInfo> {
        if !is_root || buf.len() < BTREE_INFO_SIZE {
            return None;
        }
        let o = buf.len() - BTREE_INFO_SIZE;
        Some(BTreeInfo {
            flags: u32le(buf, o),
            node_size: u32le(buf, o + 4),
            key_size: u32le(buf, o + 8),
            val_size: u32le(buf, o + 12),
            key_count: u64::from_le_bytes(buf[o + 24..o + 32].try_into().ok()?),
            node_count: u64::from_le_bytes(buf[o + 32..o + 40].try_into().ok()?),
        })
    }
}

/// `btree_info_t`：根节点末尾的树描述。
#[derive(Debug, Clone, Copy)]
pub struct BTreeInfo {
    pub flags: u32,
    pub node_size: u32,
    pub key_size: u32,
    pub val_size: u32,
    pub key_count: u64,
    pub node_count: u64,
}

pub const BTREE_UINT64_KEYS: u32 = 0x1;
pub const BTREE_SEQUENTIAL_INSERT: u32 = 0x2;
pub const BTREE_ALLOW_GHOSTS: u32 = 0x4;
pub const BTREE_EPHEMERAL: u32 = 0x8;
pub const BTREE_PHYSICAL: u32 = 0x10;

impl BTreeInfo {
    pub fn is_physical(&self) -> bool {
        self.flags & BTREE_PHYSICAL != 0
    }

    pub fn allow_ghosts(&self) -> bool {
        self.flags & BTREE_ALLOW_GHOSTS != 0
    }
}
