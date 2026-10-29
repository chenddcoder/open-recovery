//! 对象映射表（`omap_phys_t`）：`(oid, xid) → 物理块号`。
//!
//! 这是 APFS 的地址翻译层。容器有一张（映射各卷超级块），每个卷再有一张
//! （映射 fs-tree 等虚拟对象）。删除文件后旧版本的对象还散落在盘上，
//! 只要知道旧 xid 就能把它们找回来——这是"按事务号恢复"的入口。

use super::btree::{PhysicalOid, Tree};
use super::obj;
use dmdisk::{Error, Result, Reader};

/// omap 树固定键长：`omap_key_t { oid, xid }`
const KEY_LEN: usize = 16;
/// omap 树固定值长：`omap_val_t { flags, size, paddr }`
const VAL_LEN: usize = 16;

/// omap 值的标志位
pub const OMAP_VAL_DELETED: u32 = 0x1;
pub const OMAP_VAL_SAVED: u32 = 0x2;
pub const OMAP_VAL_ENCRYPTED: u32 = 0x4;
pub const OMAP_VAL_NOHEADER: u32 = 0x8;

#[derive(Debug, Clone, Copy)]
pub struct OmapVal {
    pub flags: u32,
    /// 对象字节大小
    pub size: u32,
    pub paddr: u64,
}

impl OmapVal {
    pub fn is_deleted(&self) -> bool {
        self.flags & OMAP_VAL_DELETED != 0
    }
}

pub struct Omap {
    pub paddr: u64,
    pub tree_paddr: u64,
    pub flags: u32,
    pub snap_count: u32,
    pub snapshot_tree_paddr: u64,
}

impl Omap {
    pub fn lookup(
        &self,
        reader: &Reader,
        bs: u32,
        oid: u64,
        max_xid: u64,
    ) -> Result<Option<OmapVal>> {
        let tree = Tree::fixed(reader, bs, KEY_LEN, VAL_LEN);
        let mut start = Vec::with_capacity(KEY_LEN);
        start.extend_from_slice(&oid.to_le_bytes());
        start.extend_from_slice(&0u64.to_le_bytes());

        let mut best: Option<(u64, OmapVal)> = None;
        let mut err: Option<Error> = None;
        tree.scan(self.tree_paddr, &mut PhysicalOid, &start, &mut |k, v| {
            if k.len() < KEY_LEN || v.len() < VAL_LEN {
                return true;
            }
            let key_oid = u64::from_le_bytes(k[0..8].try_into().unwrap());
            if key_oid != oid {
                return false; // 走过头了
            }
            let xid = u64::from_le_bytes(k[8..16].try_into().unwrap());
            if xid > max_xid {
                return true;
            }
            let val = OmapVal {
                flags: u32::from_le_bytes(v[0..4].try_into().unwrap()),
                size: u32::from_le_bytes(v[4..8].try_into().unwrap()),
                paddr: u64::from_le_bytes(v[8..16].try_into().unwrap()),
            };
            if best.map(|(bx, _)| xid >= bx).unwrap_or(true) {
                best = Some((xid, val));
            }
            true
        })?;
        if let Some(e) = err.take() {
            return Err(e);
        }
        Ok(best.map(|(_, v)| v))
    }

    /// 解出 oid 对应的块号，并顺带校验该块的对象头确实是这个 oid。
    pub fn resolve(&self, reader: &Reader, bs: u32, oid: u64, max_xid: u64) -> Result<u64> {
        let val = self
            .lookup(reader, bs, oid, max_xid)?
            .ok_or_else(|| Error::Format(format!("omap 里找不到对象 oid={oid}（xid≤{max_xid}）")))?;
        if val.paddr == 0 {
            return Err(Error::Format(format!("对象 oid={oid} 的物理地址为 0（空洞）")));
        }
        let buf = obj::read_block(reader, bs, val.paddr)?;
        let hdr = obj::parse_header(&buf)?;
        if hdr.oid != oid && hdr.oid != 0 {
            return Err(Error::Format(format!(
                "omap 指向的块 {} 里对象 oid={}，与期望的 {oid} 不符",
                val.paddr, hdr.oid
            )));
        }
        Ok(val.paddr)
    }
}

/// 解析一个 omap 对象。
pub fn open(reader: &Reader, bs: u32, omap_paddr: u64) -> Result<Omap> {
    let buf = obj::read_block(reader, bs, omap_paddr)?;
    let hdr = obj::parse_header(&buf)?;
    if hdr.type_base() != obj::OBJ_OMAP {
        return Err(Error::Format(format!(
            "块 {omap_paddr} 不是 omap 对象（类型 {}）",
            hdr.type_name()
        )));
    }
    let u32at = |o: usize| u32::from_le_bytes(buf[o..o + 4].try_into().unwrap());
    let u64at = |o: usize| u64::from_le_bytes(buf[o..o + 8].try_into().unwrap());
    let tree_paddr = u64at(0x30);
    if tree_paddr == 0 {
        return Err(Error::Format("omap 的树指针为 0".into()));
    }
    Ok(Omap {
        paddr: omap_paddr,
        tree_paddr,
        flags: u32at(0x20),
        snap_count: u32at(0x24),
        snapshot_tree_paddr: u64at(0x38),
    })
}
