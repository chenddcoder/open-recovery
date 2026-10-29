//! 主引导记录（MBR）+ 扩展分区链（EBR）解析。

use super::{Partition, PartitionKind, PartitionTable, Scheme};
use crate::crc32::crc32; // 未使用，但保持模块内一致的类型导入顺序
use crate::error::Result;
use crate::reader::Reader;

const ENTRY_BASE: u64 = 446;
const ENTRY_SIZE: u64 = 16;
const SIG: [u8; 2] = [0x55, 0xAA];

/// MBR 分区类型码 → 名称。只列常见项，未命中就显示 `type 0xNN`。
fn type_name(t: u8) -> &'static str {
    match t {
        0x00 => "空",
        0x01 => "FAT12",
        0x04 => "FAT16 (<32M)",
        0x05 => "扩展分区 (CHS)",
        0x06 => "FAT16B",
        0x07 => "NTFS / exFAT / HPFS",
        0x0B => "FAT32 (CHS)",
        0x0C => "FAT32 (LBA)",
        0x0E => "FAT16 (LBA)",
        0x0F => "扩展分区 (LBA)",
        0x11 => "隐藏 FAT12",
        0x12 => "隐藏 FAT16",
        0x14 => "隐藏 FAT16 (LBA)",
        0x16 => "隐藏 FAT16B",
        0x17 => "隐藏 NTFS",
        0x1B => "隐藏 FAT32",
        0x1C => "隐藏 FAT32 (LBA)",
        0x1E => "隐藏 FAT16 (LBA)",
        0x27 => "Windows 恢复分区",
        0x42 => "Windows 动态盘",
        0x82 => "Linux swap / Solaris",
        0x83 => "Linux",
        0x85 => "Linux 扩展",
        0x8E => "Linux LVM",
        0xA5 => "FreeBSD",
        0xA6 => "OpenBSD",
        0xA8 => "macOS UFS",
        0xA9 => "NetBSD",
        0xAB => "macOS Boot",
        0xAF => "macOS HFS+",
        0xEE => "GPT 保护分区",
        0xEF => "EFI 系统分区",
        0xFD => "Linux RAID",
        _ => "未知类型",
    }
}

struct RawEntry {
    status: u8,
    ptype: u8,
    lba: u32,
    sectors: u32,
}

fn read_entries(reader: &Reader, sector_off: u64) -> Result<Vec<RawEntry>> {
    let sec = reader.read_vec(sector_off, 512)?;
    let mut out = Vec::new();
    for i in 0..4u64 {
        let b = ENTRY_BASE + i * ENTRY_SIZE;
        let e = &sec[b as usize..(b + ENTRY_SIZE) as usize];
        out.push(RawEntry {
            status: e[0],
            ptype: e[4],
            lba: u32::from_le_bytes([e[8], e[9], e[10], e[11]]),
            sectors: u32::from_le_bytes([e[12], e[13], e[14], e[15]]),
        });
    }
    Ok(out)
}

/// 解析 MBR。返回 `None` 表示没有有效引导签名。
pub fn parse(reader: &Reader) -> Result<Option<PartitionTable>> {
    if reader.len() < 512 {
        return Ok(None);
    }
    let sec = reader.read_vec(0, 512)?;
    if sec[510..512] != SIG {
        return Ok(None);
    }

    let sector_size = 512u64;
    let mut partitions = Vec::new();
    let entries = read_entries(reader, 0)?;
    let mut idx = 1usize;
    let mut extended: Vec<(u64, u32)> = Vec::new();

    for e in &entries {
        if e.ptype == 0 || e.sectors == 0 {
            continue;
        }
        let offset = e.lba as u64 * sector_size;
        let size = e.sectors as u64 * sector_size;
        let kind = if e.ptype == 0x05 || e.ptype == 0x0F || e.ptype == 0x85 {
            extended.push((offset, e.sectors));
            PartitionKind::Extended
        } else {
            PartitionKind::Primary
        };
        partitions.push(Partition {
            index: idx,
            scheme: Scheme::Mbr,
            kind,
            offset,
            size,
            name: String::new(),
            type_name: type_name(e.ptype).to_string(),
            type_id: format!("0x{:02X}", e.ptype),
            uuid: None,
            bootable: e.status == 0x80,
            attributes: 0,
        });
        idx += 1;
    }

    // 扩展分区链：EBR 的四个表项里，前两个有意义（自身逻辑分区 + 指向下一个 EBR）
    let mut notes = Vec::new();
    for (ext_off, ext_secs) in extended {
        let mut cur = ext_off;
        let mut guard = 0;
        let mut next_lba: u32 = 0;
        loop {
            guard += 1;
            if guard > 512 {
                notes.push("扩展分区链疑似成环，已中断".into());
                break;
            }
            if cur == 0 || cur >= reader.len() {
                break;
            }
            let sec = match reader.read_vec(cur, 512) {
                Ok(s) => s,
                Err(_) => break,
            };
            if sec[510..512] != SIG {
                break;
            }
            let first = &sec[446..462];
            let second = &sec[462..478];
            let f_type = first[4];
            let f_lba = u32::from_le_bytes([first[8], first[9], first[10], first[11]]);
            let f_secs = u32::from_le_bytes([first[12], first[13], first[14], first[15]]);
            if f_type != 0 && f_secs != 0 {
                partitions.push(Partition {
                    index: idx,
                    scheme: Scheme::Mbr,
                    kind: PartitionKind::Logical,
                    offset: cur + f_lba as u64 * sector_size,
                    size: f_secs as u64 * sector_size,
                    name: String::new(),
                    type_name: type_name(f_type).to_string(),
                    type_id: format!("0x{:02X}", f_type),
                    uuid: None,
                    bootable: first[0] == 0x80,
                    attributes: 0,
                });
                idx += 1;
            }
            let s_lba = u32::from_le_bytes([second[8], second[9], second[10], second[11]]);
            if s_lba == 0 || s_lba == next_lba {
                break;
            }
            next_lba = s_lba;
            cur = ext_off + s_lba as u64 * sector_size;
            if cur > ext_off + ext_secs as u64 * sector_size {
                break;
            }
        }
    }

    // 磁盘签名（偏移 440，4 字节）
    let disk_sig = u32::from_le_bytes([sec[440], sec[441], sec[442], sec[443]]);

    Ok(Some(PartitionTable {
        scheme: Scheme::Mbr,
        sector_size: 512,
        partitions,
        notes,
        verified: disk_sig != 0,
    }))
}

/// 供 CLI/单元测试使用：校验引导签名。
pub fn has_boot_signature(sec: &[u8]) -> bool {
    sec.len() >= 512 && sec[510..512] == SIG
}

/// 计算某段数据的 CRC32（仅用于让公共 API 对称，MBR 本身无校验和）
pub fn checksum(data: &[u8]) -> u32 {
    crc32(data)
}
