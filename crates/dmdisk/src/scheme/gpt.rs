//! GUID 分区表（GPT）解析，含头/分区项 CRC32 校验。
//!
//! 校验失败时不直接放弃：坏盘上"CRC 坏了但内容还能用"是常态，
//! 只把 `verified=false` 记下来，让上层决定提示强度。

use super::{Partition, PartitionKind, PartitionTable, Scheme};
use crate::crc32::crc32;
use crate::error::{Error, Result};
use crate::reader::Reader;

/// GPT 分区类型 GUID → 名称
fn type_name(guid: &str) -> Option<&'static str> {
    Some(match guid.to_ascii_uppercase().as_str() {
        "C12A7328-F81F-11D2-BA4B-00A0C93EC93B" => "EFI 系统分区",
        "21686148-6449-6E6F-744E-656564454649" => "BIOS 启动分区",
        "0FC63DAF-8483-4772-8E79-3D69D8477DE4" => "Linux 文件系统",
        "0657FD6D-A4AB-43C4-84E5-0933C84B4F4F" => "Linux swap",
        "E6D6D379-F507-44C2-A23C-238F2A3DF928" => "Linux LVM",
        "933AC7E1-2EB4-4F13-B844-0E14E2AEF915" => "Linux /home",
        "A19D880F-05FC-4D3B-A006-743F0F84911E" => "Linux RAID",
        "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7" => "Microsoft 基本数据",
        "E3C9E316-0B5C-4DB8-817D-F92DF00215AE" => "Microsoft 保留分区",
        "DE94BBA4-06D1-4D40-A16A-BFD50179D6AC" => "Windows 恢复分区",
        "5808C8AA-7E8F-42E0-85D2-E1E90434CFB3" => "Windows LDM 元数据",
        "7C3457EF-0000-11AA-AA11-00306543ECAC" => "Apple APFS",
        "48465300-0000-11AA-AA11-00306543ECAC" => "Apple HFS+",
        "426F6F74-0000-11AA-AA11-00306543ECAC" => "Apple 启动分区",
        "52414944-0000-11AA-AA11-00306543ECAC" => "Apple RAID",
        "53746F72-6167-11AA-AA11-00306543ECAC" => "Apple Core Storage",
        "4C616265-6C00-11AA-AA11-00306543ECAC" => "Apple 标签",
        "6A898CC3-1DD2-11B2-99A6-080020736631" => "Apple ZFS",
        "CEF5A9AD-73BC-4601-89F3-CDEEEEE321A1" => "Core Storage",
        _ => return None,
    })
}

fn guid_to_string(b: &[u8]) -> String {
    format!(
        "{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
        u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        u16::from_le_bytes([b[4], b[5]]),
        u16::from_le_bytes([b[6], b[7]]),
        b[8],
        b[9],
        b[10],
        b[11],
        b[12],
        b[13],
        b[14],
        b[15]
    )
}

fn utf16le_to_string(b: &[u8]) -> String {
    let mut units = Vec::with_capacity(b.len() / 2);
    let mut i = 0;
    while i + 1 < b.len() {
        let u = u16::from_le_bytes([b[i], b[i + 1]]);
        if u == 0 {
            break;
        }
        units.push(u);
        i += 2;
    }
    String::from_utf16_lossy(&units).trim().to_string()
}

struct Header {
    current_lba: u64,
    backup_lba: u64,
    first_usable: u64,
    last_usable: u64,
    entries_lba: u64,
    num_entries: u32,
    entry_size: u32,
    entries_crc: u32,
    header_crc_ok: bool,
    header_crc_stored: u32,
    disk_guid: String,
}

fn read_header(reader: &Reader, sector: u64) -> Result<Header> {
    let raw = reader.read_vec(sector, 92)?;
    if &raw[0..8] != b"EFI PART" {
        return Err(Error::Format("GPT 签名不是 EFI PART".into()));
    }
    let header_size = u32::from_le_bytes([raw[12], raw[13], raw[14], raw[15]]) as usize;
    if !(92..=sector as usize).contains(&header_size) {
        return Err(Error::Format(format!("GPT 头长度异常: {header_size}")));
    }
    let stored_crc = u32::from_le_bytes([raw[16], raw[17], raw[18], raw[19]]);
    let mut calc = raw[..header_size].to_vec();
    for b in &mut calc[16..20] {
        *b = 0;
    }
    let header_crc_ok = crc32(&calc) == stored_crc;

    Ok(Header {
        current_lba: u64::from_le_bytes(raw[24..32].try_into().unwrap()),
        backup_lba: u64::from_le_bytes(raw[32..40].try_into().unwrap()),
        first_usable: u64::from_le_bytes(raw[40..48].try_into().unwrap()),
        last_usable: u64::from_le_bytes(raw[48..56].try_into().unwrap()),
        disk_guid: guid_to_string(&raw[56..72]),
        entries_lba: u64::from_le_bytes(raw[72..80].try_into().unwrap()),
        num_entries: u32::from_le_bytes(raw[80..84].try_into().unwrap()),
        entry_size: u32::from_le_bytes(raw[84..88].try_into().unwrap()),
        entries_crc: u32::from_le_bytes(raw[88..92].try_into().unwrap()),
        header_crc_ok,
        header_crc_stored: stored_crc,
    })
}

/// `sector` 为 LBA1 的字节偏移（512 或 4096）。
pub fn parse(reader: &Reader, sector: u64) -> Result<PartitionTable> {
    let hdr = read_header(reader, sector)?;
    if hdr.entry_size < 128 || hdr.num_entries == 0 || hdr.num_entries > 4096 {
        return Err(Error::Format(format!(
            "GPT 分区项参数异常: num={} size={}",
            hdr.num_entries, hdr.entry_size
        )));
    }

    let arr_len = hdr.num_entries as usize * hdr.entry_size as usize;
    let arr = reader.read_vec(hdr.entries_lba * sector, arr_len)?;
    let entries_crc_ok = crc32(&arr) == hdr.entries_crc;

    let mut partitions = Vec::new();
    let mut notes = Vec::new();
    for i in 0..hdr.num_entries as usize {
        let e = &arr[i * hdr.entry_size as usize..(i + 1) * hdr.entry_size as usize];
        if e[0..16].iter().all(|&b| b == 0) {
            continue;
        }
        let type_guid = guid_to_string(&e[0..16]);
        let part_guid = guid_to_string(&e[16..32]);
        let first = u64::from_le_bytes(e[32..40].try_into().unwrap());
        let last = u64::from_le_bytes(e[40..48].try_into().unwrap());
        let attrs = u64::from_le_bytes(e[48..56].try_into().unwrap());
        let name = utf16le_to_string(&e[56..128.min(e.len())]);
        let name_str = type_name(&type_guid).unwrap_or("未知分区类型").to_string();
        if last < first {
            notes.push(format!("分区 {} 的 LBA 区间倒置，已跳过", i + 1));
            continue;
        }
        partitions.push(Partition {
            index: partitions.len() + 1,
            scheme: Scheme::Gpt,
            kind: PartitionKind::Primary,
            offset: first * sector,
            size: (last - first + 1) * sector,
            name,
            type_name: name_str,
            type_id: type_guid,
            uuid: Some(part_guid),
            bootable: false,
            attributes: attrs,
        });
    }

    if !hdr.header_crc_ok {
        notes.push(format!(
            "GPT 头 CRC32 不匹配（存储 {:#010x}），可能是坏盘或被改写",
            hdr.header_crc_stored
        ));
    }
    if !entries_crc_ok {
        notes.push("GPT 分区项 CRC32 不匹配".to_string());
    }
    if hdr.current_lba != 1 && sector != 512 {
        notes.push(format!("GPT current_lba={} 与扇区推断不符", hdr.current_lba));
    }
    if hdr.backup_lba == 0 {
        notes.push("无备份 GPT（可能是早期写入工具生成）".to_string());
    }
    notes.push(format!(
        "可用 LBA 范围 {}..{}，磁盘 GUID {}",
        hdr.first_usable, hdr.last_usable, hdr.disk_guid
    ));

    Ok(PartitionTable {
        scheme: Scheme::Gpt,
        sector_size: sector as u32,
        partitions,
        notes,
        verified: hdr.header_crc_ok && entries_crc_ok,
    })
}
