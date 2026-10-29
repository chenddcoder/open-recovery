//! NTFS 目录索引项解析（$INDEX_ROOT 与 INDX 块共用一套结构）。

use crate::util::{filetime_to_unix, utf16le_to_string};

#[derive(Debug, Clone)]
pub struct IndexEntry {
    pub file_ref: u64,
    pub name: String,
    pub size: u64,
    pub is_dir: bool,
    pub mtime: Option<i64>,
    pub child_vcn: Option<u64>,
}

/// 解析一段索引项（从 `off` 到 `end`）。
pub fn parse_entries(buf: &[u8], off: usize, end: usize) -> Vec<IndexEntry> {
    let mut out = Vec::new();
    let mut off = off;
    let end = end.min(buf.len());
    while off + 16 <= end {
        let file_ref = u64::from_le_bytes(buf[off..off + 8].try_into().unwrap());
        let entry_len = u16::from_le_bytes([buf[off + 8], buf[off + 9]]) as usize;
        let key_len = u16::from_le_bytes([buf[off + 10], buf[off + 11]]) as usize;
        let flags = u16::from_le_bytes([buf[off + 12], buf[off + 13]]);
        if entry_len < 16 || off + entry_len > end {
            break;
        }
        // 0x02 = 最后一项（无 key）
        if flags & 0x02 != 0 {
            break;
        }
        // 0x01 = 该索引项带子节点 VCN
        let child_vcn = if flags & 0x01 != 0 && entry_len >= 8 {
            let p = off + entry_len - 8;
            Some(u64::from_le_bytes(buf[p..p + 8].try_into().unwrap()))
        } else {
            None
        };
        let key_start = off + 16;
        let key_end = (key_start + key_len).min(end);
        if key_len >= 66 && key_start + 66 <= key_end {
            let key = &buf[key_start..key_end];
            let size = u64::from_le_bytes(key[48..56].try_into().unwrap());
            let fflags = u32::from_le_bytes(key[56..60].try_into().unwrap());
            let name_len = key[64] as usize;
            let nend = (66 + name_len * 2).min(key.len());
            let name = utf16le_to_string(&key[66..nend]);
            out.push(IndexEntry {
                file_ref: file_ref & 0x0000_FFFF_FFFF_FFFF,
                name,
                size,
                is_dir: fflags & 0x1000_0000 != 0,
                mtime: filetime_to_unix(u64::from_le_bytes(key[24..32].try_into().unwrap())),
                child_vcn,
            });
        }
        off += entry_len;
    }
    out
}

/// 解析一个 INDX 块（已做 fixup 的原始字节）。
pub fn parse_indx_block(buf: &[u8]) -> Vec<IndexEntry> {
    if buf.len() < 40 || &buf[0..4] != b"INDX" {
        return Vec::new();
    }
    let entries_off = u32::from_le_bytes(buf[24..28].try_into().unwrap()) as usize;
    let total = u32::from_le_bytes(buf[28..32].try_into().unwrap()) as usize;
    let start = 24 + entries_off;
    if start >= buf.len() {
        return Vec::new();
    }
    let end = (start + total).min(buf.len());
    parse_entries(buf, start, end)
}
