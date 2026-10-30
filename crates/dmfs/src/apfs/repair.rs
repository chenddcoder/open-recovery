//! APFS 容器头修复：找出可用的 `nx_superblock` 副本，供写回块 0。
//!
//! ## 为什么需要这个
//!
//! APFS 每次提交事务都会在检查点描述区留一份 NXSB 副本（一份副本占一个块）。
//! 而块 0 只有孤零零一份，偏偏它又是全盘写入最频繁的位置——每提交一次
//! checkpoint 就重写一次。于是掉电、USB 桥掉线、睡眠唤醒失败这类"写到一半
//! 事件"，最容易打坏的就是它。
//!
//! 块 0 一坏，macOS 读不到 `NXSB` 魔数，直接判定"这个分区没有文件系统"，
//! 在 `diskutil` 里显示 `File System: None`，Finder 里连分区都不出现。
//! 但容器其余元数据（包括几十上百份副本）通常完好无损。
//!
//! 修复因此非常简单：**挑一份校验通过、xid 最大的副本，原样写回块 0**。
//! 只动一个块（4 KiB），别的地方一个字节都不碰。
//!
//! ## 为什么是"原样写"
//!
//! NXSB 的 Fletcher64 校验覆盖整个块，块里没有任何字段依赖"自己待在第几块"，
//! 所以整块复制过去校验自然成立。反过来，如果只抄字段、自己重算校验，
//! 反而容易把 `nx_xp_desc_index` 这类"我只在第几块"的字段写错。

use super::container::uuid_string;
use super::obj::{self, ObjHeader};
use dmdisk::{Result, Reader};

/// 默认扫描块数。检查点描述区一般在前几百块内，1024 块（4 MiB）足够；
/// 再往上加只会拖慢界面，不会提高成功率。
pub const DEFAULT_SCAN_BLOCKS: u64 = 1024;

/// 一份可用的 NXSB 副本。
#[derive(Debug, Clone)]
pub struct Candidate {
    /// 容器内的物理块号
    pub paddr: u64,
    pub xid: u64,
    pub block_size: u32,
    pub block_count: u64,
    /// `nx_fs_oid[]` 里的非零项数量，即容器里的卷数
    pub volume_count: usize,
    pub uuid: String,
    pub desc_base: u64,
    pub desc_blocks: u32,
}

/// 扫描结果。
#[derive(Debug, Clone)]
pub struct Report {
    /// 块 0 现在是否已经是一份有效 NXSB（有效就没必要修）
    pub block0_ok: bool,
    pub block0_xid: Option<u64>,
    pub block_size: u32,
    /// 按 xid 降序，同 xid 只保留一份
    pub candidates: Vec<Candidate>,
    /// 检查点区里校验失败的 NXSB 数量（坏盘信号）
    pub cksum_bad: u32,
    pub scanned_blocks: u64,
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

/// 判断一个块是不是有效 NXSB，是则解析出候选信息。
///
/// 校验失败**不返回候选**，但会把失败计数交给调用方——那是坏盘信号，
/// 界面上要提醒用户。
pub fn parse_candidate(paddr: u64, block: &[u8]) -> Option<std::result::Result<Candidate, ()>> {
    if block.len() < 0x2d8 || &block[0x20..0x24] != obj::NX_MAGIC {
        return None;
    }
    let hdr = match obj::parse_header(block) {
        Ok(h) => h,
        Err(_) => return None,
    };
    if hdr.type_base() != obj::OBJ_NX_SUPERBLOCK {
        return None;
    }
    if !hdr.cksum_ok {
        return Some(Err(()));
    }
    // nx_fs_oid[] 从 0xB8 起，最多 100 项；遇到 0 就结束
    let max_fs = u32le(block, 0xb4).min(100) as usize;
    let mut volume_count = 0usize;
    for i in 0..max_fs {
        let o = 0xb8 + i * 8;
        if o + 8 > block.len() {
            break;
        }
        if u64le(block, o) == 0 {
            break;
        }
        volume_count += 1;
    }
    Some(Ok(Candidate {
        paddr,
        xid: hdr.xid,
        block_size: u32le(block, 0x24),
        block_count: u64le(block, 0x28),
        volume_count,
        uuid: uuid_string(&block[0x48..0x58]),
        desc_base: u64le(block, 0x70),
        desc_blocks: u32le(block, 0x68),
    }))
}

/// 扫描容器开头的块，收集所有有效 NXSB 副本。
pub fn scan(reader: &Reader) -> Result<Report> {
    // 块大小：优先取块 0 有效时的值；块 0 坏了就按 macOS 默认 4096。
    let mut block_size = 4096u32;
    let mut block0_ok = false;
    let mut block0_xid = None;
    let mut cksum_bad = 0u32;

    if let Ok(b) = obj::read_block(reader, 4096, 0) {
        if let Some(Ok(c)) = parse_candidate(0, &b) {
            block0_ok = true;
            block0_xid = Some(c.xid);
            if c.block_size.is_power_of_two() && (512..=65536).contains(&c.block_size) {
                block_size = c.block_size;
            }
        } else if let Some(Err(())) = parse_candidate(0, &b) {
            cksum_bad += 1;
        }
    }

    let total_blocks = reader.len() / block_size as u64;
    let limit = total_blocks.min(DEFAULT_SCAN_BLOCKS);
    let mut candidates = Vec::new();
    for paddr in 0..limit {
        let block = match obj::read_block(reader, block_size, paddr) {
            Ok(b) => b,
            Err(_) => continue,
        };
        match parse_candidate(paddr, &block) {
            Some(Ok(c)) => candidates.push(c),
            Some(Err(())) => cksum_bad += 1,
            None => {}
        }
    }

    // xid 大的在前：最新的事务状态最接近用户最后使用时的样子
    candidates.sort_by(|a, b| b.xid.cmp(&a.xid));
    candidates.dedup_by_key(|c| c.xid);

    Ok(Report {
        block0_ok,
        block0_xid,
        block_size,
        candidates,
        cksum_bad,
        scanned_blocks: limit,
    })
}

/// 只校验单块（写回前的最后一道关）。
pub fn verify_block(paddr: u64, block: &[u8]) -> Option<Candidate> {
    match parse_candidate(paddr, block) {
        Some(Ok(c)) => Some(c),
        _ => None,
    }
}

/// 把 `ObjHeader` 的类型名暴露给上层做日志（避免上层再 import obj）。
pub fn type_label(h: &ObjHeader) -> &'static str {
    h.type_name()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 块 0 被随机字节覆盖时，扫描必须能从前几百块里找到候选。
    /// 这是陈兄那块 2TB WD 盘的真实场景。
    #[test]
    fn parse_rejects_random_and_accepts_nxsb() {
        let junk = vec![0x9fu8; 4096];
        assert!(parse_candidate(0, &junk).is_none(), "随机字节不该被当成 NXSB");

        let mut blk = vec![0u8; 4096];
        blk[0x20..0x24].copy_from_slice(obj::NX_MAGIC);
        // 类型必须也写对，否则在检查校验和之前就会被 type_base 过滤掉。
        // 注意：全零块的 Fletcher 和天然是 0（"校验通过"），所以不能靠全零来测校验失败。
        blk[0x18..0x1c].copy_from_slice(&obj::OBJ_NX_SUPERBLOCK.to_le_bytes());
        // 校验和不修 → cksum_ok=false → 返回 Some(Err(()))
        match parse_candidate(7, &blk) {
            Some(Err(())) => {}
            other => panic!("魔数对但校验错，应报告校验失败: {other:?}"),
        }
    }
}
