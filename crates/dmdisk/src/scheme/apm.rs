//! Apple Partition Map（APM）解析。
//!
//! APM 用 512 或 2048 字节的"块"描述分区，第一项描述的是分区表自身。
//! 老 PowerPC Mac、部分 U 盘和 Windows 装机盘还在用。

use super::{Partition, PartitionKind, PartitionTable, Scheme};
use crate::error::{Error, Result};
use crate::reader::Reader;

fn cstr32(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).trim().to_string()
}

fn type_friendly(t: &str) -> String {
    let base = t.split_whitespace().next().unwrap_or(t);
    match base {
        "Apple_partition_map" => "Apple 分区表".into(),
        "Apple_HFS" => "Apple HFS/HFS+".into(),
        "Apple_HFSX" => "Apple HFSX".into(),
        "Apple_Boot" => "Apple 启动分区".into(),
        "Apple_Free" => "空白".into(),
        "Apple_Driver" => "Apple 驱动".into(),
        "Apple_Driver43" => "Apple 驱动 4.3".into(),
        "Apple_Rhapsody" | "Apple_UNIX_SVR2" => "Apple UFS".into(),
        "Apple_APFS" => "Apple APFS".into(),
        "Apple_RAID" => "Apple RAID".into(),
        "Apple_RAID_Offline" => "Apple RAID (离线)".into(),
        "Apple_Scratch" => "Apple 暂存".into(),
        other => other.to_string(),
    }
}

/// `block` 为块 1 的字节偏移（512 或 2048）。
pub fn parse(reader: &Reader, block: u64) -> Result<PartitionTable> {
    let b1 = reader.read_vec(block, 512)?;
    if &b1[0..2] != b"PM" {
        return Err(Error::Format("APM 签名不是 PM".into()));
    }
    let map_count = u16::from_be_bytes([b1[2], b1[3]]) as usize;
    if map_count == 0 || map_count > 1024 {
        return Err(Error::Format(format!("APM 分区项数量异常: {map_count}")));
    }

    let mut partitions = Vec::new();
    let mut notes = Vec::new();
    for i in 1..=map_count {
        let off = block * (1 + i as u64);
        let raw = match reader.read_vec(off, 512) {
            Ok(v) => v,
            Err(_) => {
                notes.push(format!("读取第 {i} 项分区表失败（镜像被截断？）"));
                break;
            }
        };
        if &raw[0..2] != b"PM" {
            notes.push(format!("第 {i} 项分区表签名异常，停止解析"));
            break;
        }
        let name = cstr32(&raw[16..48]);
        let ptype = cstr32(&raw[48..80]);
        let start_blk = u32::from_be_bytes([raw[84], raw[85], raw[86], raw[87]]) as u64;
        let blk_count = u32::from_be_bytes([raw[88], raw[89], raw[90], raw[91]]) as u64;
        let status = u32::from_be_bytes([raw[92], raw[93], raw[94], raw[95]]);
        if blk_count == 0 {
            continue;
        }
        if ptype.starts_with("Apple_Free") {
            continue;
        }
        partitions.push(Partition {
            index: partitions.len() + 1,
            scheme: Scheme::Apm,
            kind: PartitionKind::Primary,
            offset: start_blk * block,
            size: blk_count * block,
            name,
            type_name: type_friendly(&ptype),
            type_id: ptype,
            uuid: None,
            bootable: status & 0x8 != 0 || status & 0x40 != 0,
            attributes: status as u64,
        });
    }

    Ok(PartitionTable {
        scheme: Scheme::Apm,
        sector_size: block as u32,
        partitions,
        notes,
        verified: true,
    })
}
