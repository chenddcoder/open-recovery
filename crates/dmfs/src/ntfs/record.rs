//! MFT 记录解析（含 fixup 还原）。

use super::attr::{self, Attribute};
use super::fixup;
use dmdisk::Result;

#[derive(Debug, Clone)]
pub struct Record {
    pub number: u64,
    pub in_use: bool,
    pub is_dir: bool,
    pub base_ref: u64,
    pub attrs: Vec<Attribute>,
    pub name: String,
    pub namespace: u8,
    pub parent: u64,
    pub size: u64,
    pub mtime: Option<i64>,
}

/// 解析一条记录。`data` 会被原地做 fixup，长度应等于卷的记录大小。
pub fn parse(number: u64, data: &mut [u8], sector_size: u32) -> Result<Option<Record>> {
    if data.len() < 48 {
        return Ok(None);
    }
    let magic = &data[0..4];
    if magic != b"FILE" && magic != b"BAAD" {
        return Ok(None);
    }
    let corrupted = magic == b"BAAD";
    if !corrupted {
        let _ = fixup::apply(data, sector_size);
    }
    if data.len() < 24 {
        return Ok(None);
    }
    let first_attr = u16::from_le_bytes([data[20], data[21]]) as usize;
    let flags = u16::from_le_bytes([data[22], data[23]]);
    let used = u32::from_le_bytes(data[24..28].try_into().unwrap()) as usize;
    let base_ref = u64::from_le_bytes(data[32..40].try_into().unwrap()) & 0x0000_FFFF_FFFF_FFFF;

    let attrs = attr::parse_all(data, first_attr, used.max(48));

    // 选择显示名：优先 POSIX(3)/WIN32(1)，退到 DOS(0)
    let mut best: Option<(u8, attr::FileNameAttr)> = None;
    for a in attrs.iter().filter(|a| a.type_id == attr::ATTR_FILE_NAME) {
        if let Some(f) = attr::parse_file_name(a) {
            let score = match f.namespace {
                3 => 4,
                1 => 3,
                2 => 2,
                _ => 1,
            };
            if best.as_ref().map(|(s, _)| score > *s).unwrap_or(true) {
                best = Some((score, f));
            }
        }
    }
    let (name, namespace, parent, fn_size, fn_mtime) = match &best {
        Some((_, f)) => (
            f.name.clone(),
            f.namespace,
            f.parent,
            f.size,
            f.mtime,
        ),
        None => (String::new(), 0, 0, 0, None),
    };

    let data_attr = attr::find(&attrs, attr::ATTR_DATA, None);
    let size = match data_attr {
        Some(a) if a.non_resident => a.data_size,
        Some(a) => a.resident.len() as u64,
        None => fn_size,
    };
    let mtime = attr::find(&attrs, attr::ATTR_STD_INFO, None)
        .and_then(attr::std_info_mtime)
        .or(fn_mtime);

    Ok(Some(Record {
        number,
        in_use: flags & 0x01 != 0,
        is_dir: flags & 0x02 != 0,
        base_ref,
        attrs,
        name,
        namespace,
        parent,
        size,
        mtime,
    }))
}
