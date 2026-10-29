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

/// 在前 4 MiB 内扫出任意一份可用的 NXSB（块 0 损坏时的种子）。
///
/// 不预设块大小：先按字节找 `NXSB` 魔数，魔数前 0x20 就是块起点，
/// 再从块起点 +0x24 读出真实块大小并校验「起点能被块大小整除」。
fn scan_nxsb_copy(reader: &Reader) -> Result<(u32, u64, Vec<u8>)> {
    const WINDOW: u64 = 4 * 1024 * 1024;
    let n = reader.len().min(WINDOW) as usize;
    if n < 0x40 {
        return Err(Error::Format("卷太小，找不到 NXSB 副本".into()));
    }
    let buf = reader.read_vec(0, n)?;
    let mut from = 0usize;
    while let Some(p) = find_sub(&buf[from..], obj::NX_MAGIC) {
        let magic_at = from + p;
        if magic_at >= 0x20 {
            let start = magic_at - 0x20;
            if start + 0x28 <= buf.len() {
                let bs = u32le(&buf, start + 0x24);
                if bs.is_power_of_two()
                    && (512..=65536).contains(&bs)
                    && start % bs as usize == 0
                {
                    let paddr = (start / bs as usize) as u64;
                    if let Ok(blk) = obj::read_block(reader, bs, paddr) {
                        if blk.len() >= 0x24
                            && &blk[0x20..0x24] == obj::NX_MAGIC
                            && obj::parse_header(&blk).map(|h| h.cksum_ok).unwrap_or(false)
                        {
                            return Ok((bs, paddr, blk));
                        }
                    }
                }
            }
        }
        from = magic_at + 4;
    }
    Err(Error::Format(
        "前 4 MiB 内找不到校验通过的 NXSB 副本（容器可能整体被覆盖）".into(),
    ))
}

fn find_sub(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

impl Container {
    pub fn open(reader: &Reader) -> Result<Self> {
        let mut notes = Vec::new();

        // 种子超级块：正常情况是块 0。块 0 被覆盖/损坏时（坏盘、误写、TRIM），
        // 检查点副本（块 2、4、6…）通常仍完好 —— 扫出来继续，而不是直接判"不是 APFS"。
        let (bs, seed_paddr, seed) = match obj::read_block(reader, 4096, 0) {
            Ok(b) if b.len() >= 0x24 && &b[0x20..0x24] == obj::NX_MAGIC => {
                let bs = probe_block_size(reader)?;
                let b = if bs == 4096 { b } else { obj::read_block(reader, bs, 0)? };
                (bs, 0u64, b)
            }
            _ => {
                let (bs, paddr, buf) = scan_nxsb_copy(reader)?;
                notes.push(format!(
                    "容器块 0 的 NXSB 已损坏，已改用检查点副本（块 {paddr}，块大小 {bs}）"
                ));
                (bs, paddr, buf)
            }
        };

        let block_count = {
            let n = u64le(&seed, 0x28);
            if n > 0 { n } else { reader.len() / bs as u64 }
        };

        // 顺序很重要：块 0..16 优先，然后是种子附近，最后是检查点描述区。
        // 同 xid 的副本会因 `>=` 被后者覆盖，所以不能打乱这个由近及远的顺序。
        let mut cands: Vec<u64> = (0..16u64).filter(|b| *b < block_count).collect();
        for d in 1..=8u64 {
            if seed_paddr >= d && !cands.contains(&(seed_paddr - d)) {
                cands.push(seed_paddr - d);
            }
            if seed_paddr + d < block_count && !cands.contains(&(seed_paddr + d)) {
                cands.push(seed_paddr + d);
            }
        }
        let desc_base = u64le(&seed, 0x70);
        let desc_blocks = u32le(&seed, 0x68) as u64;
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
        if std::env::var("DMDE_DEBUG_APFS").is_ok() {
            eprintln!("[apfs] bs={bs} block_count={block_count} seed={seed_paddr} nx_paddr={nx_paddr} xid={xid}");
        }

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
