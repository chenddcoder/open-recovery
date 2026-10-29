//! NTFS 属性（$STANDARD_INFORMATION / $FILE_NAME / $DATA / $INDEX_ROOT ...）解析。

use super::runlist::{self, Run};
use crate::util::{filetime_to_unix, utf16le_to_string};
use dmdisk::{Error, Result};

pub const ATTR_STD_INFO: u32 = 0x10;
pub const ATTR_ATTR_LIST: u32 = 0x20;
pub const ATTR_FILE_NAME: u32 = 0x30;
pub const ATTR_DATA: u32 = 0x80;
pub const ATTR_INDEX_ROOT: u32 = 0x90;
pub const ATTR_INDEX_ALLOC: u32 = 0xA0;
pub const ATTR_BITMAP: u32 = 0xB0;

pub const ATTR_FLAG_COMPRESSED: u16 = 0x0001;
pub const ATTR_FLAG_ENCRYPTED: u16 = 0x4000;
pub const ATTR_FLAG_SPARSE: u16 = 0x8000;

#[derive(Debug, Clone)]
pub struct Attribute {
    pub type_id: u32,
    pub non_resident: bool,
    pub name: String,
    pub flags: u16,
    pub resident: Vec<u8>,
    pub runs: Vec<Run>,
    pub data_size: u64,
    pub alloc_size: u64,
    pub start_vcn: u64,
}

impl Attribute {
    pub fn is_compressed(&self) -> bool {
        self.flags & ATTR_FLAG_COMPRESSED != 0
    }
    pub fn is_encrypted(&self) -> bool {
        self.flags & ATTR_FLAG_ENCRYPTED != 0
    }
}

/// 解析一条记录里的全部属性。
pub fn parse_all(record: &[u8], first_attr_off: usize, used_size: usize) -> Vec<Attribute> {
    let mut out = Vec::new();
    let mut off = first_attr_off;
    let limit = used_size.min(record.len());
    while off + 8 <= limit {
        let type_id = u32::from_le_bytes([record[off], record[off + 1], record[off + 2], record[off + 3]]);
        if type_id == 0xFFFF_FFFF {
            break;
        }
        let len = u32::from_le_bytes([
            record[off + 4],
            record[off + 5],
            record[off + 6],
            record[off + 7],
        ]) as usize;
        if len < 24 || off + len > record.len() {
            break;
        }
        let non_resident = record[off + 8] != 0;
        let name_len = record[off + 9] as usize;
        let name_off = u16::from_le_bytes([record[off + 10], record[off + 11]]) as usize;
        let flags = u16::from_le_bytes([record[off + 12], record[off + 13]]);
        let name = if name_len > 0 && off + name_off + name_len * 2 <= record.len() {
            utf16le_to_string(&record[off + name_off..off + name_off + name_len * 2])
        } else {
            String::new()
        };

        if !non_resident {
            let value_len = u32::from_le_bytes([
                record[off + 16],
                record[off + 17],
                record[off + 18],
                record[off + 19],
            ]) as usize;
            let value_off = u16::from_le_bytes([record[off + 20], record[off + 21]]) as usize;
            let end = (off + value_off + value_len).min(record.len());
            let value = if off + value_off <= end {
                record[off + value_off..end].to_vec()
            } else {
                Vec::new()
            };
            out.push(Attribute {
                type_id,
                non_resident: false,
                name,
                flags,
                resident: value,
                runs: Vec::new(),
                data_size: value_len as u64,
                alloc_size: value_len as u64,
                start_vcn: 0,
            });
        } else {
            let start_vcn =
                u64::from_le_bytes(record[off + 16..off + 24].try_into().unwrap());
            let alloc_size =
                u64::from_le_bytes(record[off + 40..off + 48].try_into().unwrap());
            let data_size =
                u64::from_le_bytes(record[off + 48..off + 56].try_into().unwrap());
            let run_off = u16::from_le_bytes([record[off + 32], record[off + 33]]) as usize;
            let runs = if off + run_off <= off + len {
                runlist::decode(&record[off + run_off..off + len], start_vcn).unwrap_or_default()
            } else {
                Vec::new()
            };
            out.push(Attribute {
                type_id,
                non_resident: true,
                name,
                flags,
                resident: Vec::new(),
                runs,
                data_size,
                alloc_size,
                start_vcn,
            });
        }
        off += len;
    }
    out
}

/// 找指定类型（可选指定属性名）的第一个属性。
pub fn find<'a>(attrs: &'a [Attribute], type_id: u32, name: Option<&str>) -> Option<&'a Attribute> {
    attrs.iter().find(|a| {
        a.type_id == type_id
            && match name {
                Some(n) => a.name.eq_ignore_ascii_case(n),
                None => a.name.is_empty(),
            }
    })
}

/// $STANDARD_INFORMATION → 修改时间
pub fn std_info_mtime(a: &Attribute) -> Option<i64> {
    if a.resident.len() < 32 {
        return None;
    }
    filetime_to_unix(u64::from_le_bytes(a.resident[16..24].try_into().unwrap()))
}

/// $FILE_NAME 的关键字段
#[derive(Debug, Clone)]
pub struct FileNameAttr {
    pub parent: u64,
    pub name: String,
    pub namespace: u8,
    pub size: u64,
    pub alloc_size: u64,
    pub mtime: Option<i64>,
    pub is_dir: bool,
}

pub fn parse_file_name(a: &Attribute) -> Option<FileNameAttr> {
    let v = &a.resident;
    if v.len() < 66 {
        return None;
    }
    let parent_ref = u64::from_le_bytes(v[0..8].try_into().unwrap());
    let parent = parent_ref & 0x0000_FFFF_FFFF_FFFF;
    let alloc_size = u64::from_le_bytes(v[40..48].try_into().unwrap());
    let size = u64::from_le_bytes(v[48..56].try_into().unwrap());
    let flags = u32::from_le_bytes(v[56..60].try_into().unwrap());
    let name_len = v[64] as usize;
    let namespace = v[65];
    let end = (66 + name_len * 2).min(v.len());
    let name = utf16le_to_string(&v[66..end]);
    Some(FileNameAttr {
        parent,
        name,
        namespace,
        size,
        alloc_size,
        mtime: filetime_to_unix(u64::from_le_bytes(v[24..32].try_into().unwrap())),
        is_dir: flags & 0x1000_0000 != 0,
    })
}

/// $INDEX_ROOT 头部：返回 (索引项数据区起点, 数据区长度, 索引块大小)
pub fn index_root_layout(a: &Attribute) -> Result<(usize, usize, u32)> {
    if a.non_resident || a.resident.len() < 32 {
        return Err(Error::Format("$INDEX_ROOT 结构异常".into()));
    }
    let v = &a.resident;
    let block_size = u32::from_le_bytes(v[8..12].try_into().unwrap());
    let entries_off = u32::from_le_bytes(v[16..20].try_into().unwrap()) as usize;
    let total_size = u32::from_le_bytes(v[20..24].try_into().unwrap()) as usize;
    let start = 16 + entries_off;
    if start > v.len() {
        return Err(Error::Format("$INDEX_ROOT 索引区越界".into()));
    }
    Ok((start, total_size.min(v.len() - start), block_size))
}
