//! APFS 容器（`nx_superblock_t`）与检查点。
//!
//! 容器 = 磁盘上一整块 APFS 区域，里面可以有一个或多个卷。容器超级块 NXSB
//! 在块 0，且检查点区域里有若干副本；每写一次事务就新增一份（xid 递增）。
//! 打开时必须挑 **xid 最大且校验通过** 的那一份，否则会读到旧目录树。

use super::bnode::BTREE_INFO_SIZE;
use super::obj::{self, ObjHeader};
use super::{omap, volume};
use dmdisk::{Error, Result, Reader};

/// 检查点描述区的扫描上限（块）。正常卷远小于这个值，纯粹防坏盘。
const MAX_DESC_SCAN: u64 = 512;

pub struct Container {
    pub bs: u32,
    pub block_count: u64,
    /// 选中的超级块所在块号
    pub nx_paddr: u64,
    pub xid: u64,
    pub omap_paddr: u64,
    /// 各卷的 APFS 卷对象 oid（`nx_fs_oid[]`），索引即卷序号
    pub fs_oids: Vec<u64>,
    pub uuid: String,
    pub features: u64,
    pub incompat: u64,
    /// 校验失败的块数（坏盘信号）
    pub cksum_bad: u32,
    pub notes: Vec<String>,
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

pub fn uuid_string(b: &[u8]) -> String {
    if b.len() < 16 {
        return String::new();
    }
    let h: Vec<String> = b[..16].iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        h[0..4].concat(),
        h[4..6].concat(),
        h[6..8].concat(),
        h[8..10].concat(),
        h[10..16].concat()
    )
}

/// 探测块大小：APFS 块大小存在 NXSB 的 0x24，但读它本身需要先假定一个值。
/// macOS 上恒为 4096，这里保留探测逻辑以防稀疏镜像用 512。
fn probe_block_size(reader: &Reader) -> Result<u32> {
    let head = reader.read_vec(0, 4096.min(reader.len() as usize).max(64))?;
    if head.len() >= 0x28 && &head[0x20..0x24] == obj::NX_MAGIC {
        let bs = u32le(&head, 0x24);
        if bs.is_power_of_two() && (512..=65536).contains(&bs) {
            return Ok(bs);
        }
    }
    Ok(4096)
}

impl Container {
    pub fn open(reader: &Reader) -> Result<Self> {
        let bs = probe_block_size(reader)?;
        let first = obj::read_block(reader, bs, 0)?;
        if &first[0x20..0x24] != obj::NX_MAGIC {
            return Err(Error::Format(
                "不是 APFS 容器（块 0 处没有 NXSB 魔数）".into(),
            ));
        }
        let block_count = u64le(&first, 0x28);

        // 候选：块 0..16 + 检查点描述区
        let mut cands: Vec<u64> = (0..16u64).filter(|b| *b < block_count).collect();
        let desc_base = u64le(&first, 0x70);
        let desc_blocks = u32le(&first, 0x68) as u64;
        if desc_blocks > 0 && desc_base < block_count {
            let end = (desc_base + desc_blocks.min(MAX_DESC_SCAN)).min(block_count);
            for b in desc_base..end {
                if !cands.contains(&b) {
                    cands.push(b);
                }
            }
        }

        let mut best: Option<(u64, u64, Vec<u8>, ObjHeader)> = None;
        let mut cksum_bad = 0u32;
        for paddr in cands {
            let buf = match obj::read_block(reader, bs, paddr) {
                Ok(b) => b,
                Err(_) => continue,
            };
            if buf.len() < BTREE_INFO_SIZE + 0x20 || &buf[0x20..0x24] != obj::NX_MAGIC {
                continue;
            }
            let hdr = obj::parse_header(&buf)?;
            if hdr.type_base() != obj::OBJ_NX_SUPERBLOCK {
                continue;
            }
            if !hdr.cksum_ok {
                cksum_bad += 1;
                continue;
            }
            if best.as_ref().map(|(xid, _, _, _)| hdr.xid >= *xid).unwrap_or(true) {
                best = Some((hdr.xid, paddr, buf, hdr));
            }
        }

        let (xid, nx_paddr, buf, _hdr) = best.ok_or_else(|| {
            Error::Format("找不到校验通过的 NXSB 超级块（容器可能被破坏）".into())
        })?;

        let omap_paddr = u64le(&buf, 0xa0);
        let mut fs_oids = Vec::new();
        // nx_fs_oid[] 最多 100 项，遇到 0 结束
        for i in 0..100usize {
            let o = 0xb8 + i * 8;
            if o + 8 > buf.len() {
                break;
            }
            let oid = u64le(&buf, o);
            if oid == 0 {
                break;
            }
            fs_oids.push(oid);
        }
        let mut notes = Vec::new();
        if cksum_bad > 0 {
            notes.push(format!(
                "{cksum_bad} 份超级块副本校验失败（坏盘迹象，已改用最新有效副本）"
            ));
        }
        if fs_oids.is_empty() {
            return Err(Error::Format("容器里没有卷（nx_fs_oid 全为 0）".into()));
        }
        Ok(Self {
            bs,
            block_count,
            nx_paddr,
            xid,
            omap_paddr,
            fs_oids,
            uuid: uuid_string(&buf[0x48..0x58]),
            features: u64le(&buf, 0x30),
            incompat: u64le(&buf, 0x40),
            cksum_bad,
            notes,
        })
    }

    pub fn omap(&self, reader: &Reader) -> Result<omap::Omap> {
        omap::open(reader, self.bs, self.omap_paddr)
    }

    /// 打开第 `index` 个卷。
    pub fn volume(&self, reader: &Reader, index: usize) -> Result<volume::Volume> {
        let oid = *self
            .fs_oids
            .get(index)
            .ok_or_else(|| Error::Invalid(format!("容器只有 {} 个卷", self.fs_oids.len())))?;
        let om = self.omap(reader)?;
        let paddr = om.resolve(reader, self.bs, oid, self.xid)?;
        volume::open_at(reader, self.bs, paddr)
    }

    /// 直接按物理块号打开一个卷超级块（快照里的旧 APSB 就是这种形态）。
    pub fn volume_at(&self, reader: &Reader, apsb_paddr: u64) -> Result<volume::Volume> {
        volume::open_at(reader, self.bs, apsb_paddr)
    }
}
