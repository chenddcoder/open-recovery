//! FAT 目录项解析，含长文件名（LFN）与已删除项还原。
//!
//! 删除文件时 FAT 只把首字节写成 0xE5，长文件名条目同样被标 0xE5，
//! 但字符内容通常还在原处——按顺序拼回去就能还原大部分文件名。
//! 这是"能恢复出原名"的关键，很多工具直接放弃了。

use crate::util::dos_to_unix;

#[derive(Debug, Clone)]
pub struct RawDirEntry {
    pub name: String,
    pub short: String,
    pub attr: u8,
    pub cluster: u32,
    pub size: u32,
    pub deleted: bool,
    pub mtime: Option<i64>,
    pub ctime: Option<i64>,
    pub name_from_lfn: bool,
}

fn attr_is_dir(a: u8) -> bool {
    a & 0x10 != 0
}

pub fn is_dir_attr(a: u8) -> bool {
    attr_is_dir(a)
}

/// 解析一整段目录数据。
pub fn parse_dir(data: &[u8]) -> Vec<RawDirEntry> {
    let mut out = Vec::new();
    let mut lfn_chars: Vec<u16> = Vec::new();
    let mut lfn_lost = false;

    let mut i = 0usize;
    while i + 32 <= data.len() {
        let e = &data[i..i + 32];
        i += 32;

        let first = e[0];
        if first == 0x00 {
            // 目录结束标记；后面的槽位理论上全是 0
            break;
        }
        let attr = e[11];
        if attr == 0x0F {
            // LFN 槽位
            if first == 0xE5 {
                lfn_lost = true;
            }
            let mut chars = Vec::with_capacity(13);
            for &(s, n) in &[(1usize, 5usize), (14, 6), (28, 2)] {
                for k in 0..n {
                    let u = u16::from_le_bytes([e[s + k * 2], e[s + k * 2 + 1]]);
                    if u == 0 || u == 0xFFFF {
                        break;
                    }
                    chars.push(u);
                }
            }
            // LFN 槽位在目录里从后往前排，所以要前插
            let mut merged = chars;
            merged.extend(lfn_chars.iter().copied());
            lfn_chars = merged;
            continue;
        }
        if attr & 0x08 != 0 {
            // 卷标
            lfn_chars.clear();
            lfn_lost = false;
            continue;
        }

        let base: String = String::from_utf8_lossy(&e[0..8]).trim_end().to_string();
        let ext: String = String::from_utf8_lossy(&e[8..11]).trim_end().to_string();
        let mut short_name: String;
        if first == 0xE5 {
            // 首字符已丢失
            let rest = &base[base.char_indices().nth(1).map(|(i, _)| i).unwrap_or(base.len())..];
            short_name = if ext.is_empty() {
                format!("?{rest}")
            } else {
                format!("?{rest}.{ext}")
            };
        } else {
            short_name = if ext.is_empty() {
                base.clone()
            } else {
                format!("{base}.{ext}")
            };
            // 0x05 表示真正的首字符是 0xE5
            if first == 0x05 {
                short_name = format!("\u{00E5}{}", &short_name[1..]);
            }
            // 小写标志位
            if e[12] & 0x08 != 0 {
                short_name = short_name.to_lowercase();
            }
        }

        let deleted = first == 0xE5;
        let name = if !lfn_chars.is_empty() && (deleted || !lfn_lost) {
            let s = String::from_utf16_lossy(&lfn_chars).trim_end_matches('\u{0}').to_string();
            if s.is_empty() {
                short_name.clone()
            } else {
                s
            }
        } else {
            short_name.clone()
        };
        let name_from_lfn = !lfn_chars.is_empty() && name != short_name;

        let cluster_hi = u16::from_le_bytes([e[20], e[21]]) as u32;
        let cluster_lo = u16::from_le_bytes([e[26], e[27]]) as u32;
        let cluster = (cluster_hi << 16) | cluster_lo;
        let size = u32::from_le_bytes([e[28], e[29], e[30], e[31]]);
        let wtime = u16::from_le_bytes([e[22], e[23]]);
        let wdate = u16::from_le_bytes([e[24], e[25]]);
        let ctime = u16::from_le_bytes([e[14], e[15]]);
        let cdate = u16::from_le_bytes([e[16], e[17]]);

        // 跳过 "." 与 ".."
        if !deleted && (short_name == "." || short_name == "..") {
            lfn_chars.clear();
            lfn_lost = false;
            continue;
        }
        // 跳过 0xE5 全零短名（空槽）
        if deleted && cluster == 0 && size == 0 && attr == 0 {
            lfn_chars.clear();
            lfn_lost = false;
            continue;
        }

        out.push(RawDirEntry {
            name,
            short: short_name,
            attr,
            cluster,
            size,
            deleted,
            mtime: dos_to_unix(wdate, wtime),
            ctime: dos_to_unix(cdate, ctime),
            name_from_lfn,
        });
        lfn_chars.clear();
        lfn_lost = false;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn short_entry(name8: &[u8], ext3: &[u8], attr: u8, cluster: u32, size: u32) -> [u8; 32] {
        let mut e = [0u8; 32];
        e[..name8.len()].copy_from_slice(name8);
        e[8..8 + ext3.len()].copy_from_slice(ext3);
        e[11] = attr;
        e[26..28].copy_from_slice(&(cluster as u16).to_le_bytes());
        e[28..32].copy_from_slice(&size.to_le_bytes());
        e
    }

    #[test]
    fn parses_short_names_and_drops_dot_entries() {
        let mut data = Vec::new();
        data.extend_from_slice(&short_entry(b".       ", b"   ", 0x10, 0, 0));
        data.extend_from_slice(&short_entry(b"..      ", b"   ", 0x10, 0, 0));
        data.extend_from_slice(&short_entry(b"HELLO   ", b"TXT", 0x20, 42, 100));
        let e = parse_dir(&data);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].name, "HELLO.TXT");
        assert_eq!(e[0].cluster, 42);
        assert_eq!(e[0].size, 100);
        assert!(!e[0].deleted);
    }

    #[test]
    fn marks_deleted_entry_and_lost_first_char() {
        let mut e = short_entry(b"GOODBYE ", b"BIN", 0x20, 7, 512);
        e[0] = 0xE5;
        let parsed = parse_dir(&e);
        assert_eq!(parsed.len(), 1);
        assert!(parsed[0].deleted);
        assert!(parsed[0].name.starts_with('?'), "name={}", parsed[0].name);
    }

    #[test]
    fn stops_at_zero_filler() {
        let mut data = Vec::new();
        data.extend_from_slice(&short_entry(b"A       ", b"TXT", 0x20, 3, 1));
        data.extend_from_slice(&[0u8; 64]);
        let e = parse_dir(&data);
        assert_eq!(e.len(), 1);
    }
}
