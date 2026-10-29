//! exFAT 目录项解析。
//!
//! 一个文件由 1 个 File(0x85) + 1 个 Stream(0xC0) + N 个 FileName(0xC1) 组成。
//! 删除时只清掉每种条目的"在用位"（最高位），内容原样保留，
//! 因此 exFAT 的删除恢复质量通常比 FAT 好：名字、时间、簇号全都还在。

#[derive(Debug, Clone, Default)]
pub struct ExEntry {
    pub name: String,
    pub attr: u16,
    pub cluster: u32,
    pub size: u64,
    pub valid_size: u64,
    pub deleted: bool,
    pub is_dir: bool,
    pub no_fat_chain: bool,
    pub mtime: Option<i64>,
    pub ctime: Option<i64>,
}

const ENTRY_FILE: u8 = 0x85;
const ENTRY_STREAM: u8 = 0xC0;
const ENTRY_NAME: u8 = 0xC1;

fn packed_time(v: u32) -> Option<i64> {
    if v == 0 {
        return None;
    }
    let sec = ((v & 0x1F) * 2) as i64;
    let min = ((v >> 5) & 0x3F) as i64;
    let hour = ((v >> 11) & 0x1F) as i64;
    let day = ((v >> 16) & 0x1F) as i64;
    let month = ((v >> 21) & 0x0F) as i64;
    let year = 1980 + ((v >> 25) & 0x7F) as i64;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    Some(crate::util::dos_to_unix(
        (((year - 1980) as u16) << 9) | ((month as u16) << 5) | day as u16,
        ((hour as u16) << 11) | ((min as u16) << 5) | (sec as u16 / 2),
    )?)
}

pub fn parse_dir(data: &[u8]) -> Vec<ExEntry> {
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut pending: Option<ExEntry> = None;
    let mut name_units: Vec<u16> = Vec::new();
    let mut need_secondary = 0usize;

    while i + 32 <= data.len() {
        let e = &data[i..i + 32];
        i += 32;
        let etype = e[0];
        if etype == 0x00 {
            break; // EndOfDirectory
        }
        let base = etype & 0x7F;
        let in_use = etype & 0x80 != 0;

        // File 条目（在用 0x85 / 已删 0x05）
        if base == 0x05 {
            let secondary = e[1] as usize;
            let attr = u16::from_le_bytes([e[4], e[5]]);
            let ent = ExEntry {
                attr,
                is_dir: attr & 0x10 != 0,
                deleted: !in_use,
                mtime: packed_time(u32::from_le_bytes([e[12], e[13], e[14], e[15]])),
                ctime: packed_time(u32::from_le_bytes([e[8], e[9], e[10], e[11]])),
                ..Default::default()
            };
            name_units.clear();
            if secondary == 0 {
                out.push(ent);
                pending = None;
                need_secondary = 0;
            } else {
                pending = Some(ent);
                need_secondary = secondary;
            }
            continue;
        }

        if pending.is_none() || need_secondary == 0 {
            continue;
        }

        // Stream 扩展（在用 0xC0 / 已删 0x40）
        if base == 0x40 {
            if let Some(p) = pending.as_mut() {
                p.no_fat_chain = e[1] & 0x02 != 0;
                p.cluster = u32::from_le_bytes([e[20], e[21], e[22], e[23]]);
                p.valid_size = u64::from_le_bytes(e[8..16].try_into().unwrap());
                p.size = u64::from_le_bytes(e[24..32].try_into().unwrap());
            }
            need_secondary -= 1;
            if need_secondary == 0 {
                if let Some(mut p) = pending.take() {
                    p.name = String::from_utf16_lossy(&name_units).trim_end_matches('\u{0}').to_string();
                    out.push(p);
                }
                name_units.clear();
            }
            continue;
        }

        // FileName 条目（在用 0xC1 / 已删 0x41）
        if base == 0x41 {
            for k in 0..15 {
                let u = u16::from_le_bytes([e[2 + k * 2], e[3 + k * 2]]);
                if u == 0 {
                    break;
                }
                name_units.push(u);
            }
            need_secondary -= 1;
            if need_secondary == 0 {
                if let Some(mut p) = pending.take() {
                    p.name = String::from_utf16_lossy(&name_units).trim_end_matches('\u{0}').to_string();
                    out.push(p);
                }
                name_units.clear();
            }
            continue;
        }
    }
    out
}

/// 目录项类型是否为已知的元数据条目（用于跳过）
pub fn is_known_type(etype: u8) -> bool {
    matches!(etype & 0x7F, 0x00 | 0x01 | 0x02 | 0x03 | 0x04 | 0x05)
}

pub fn consts() -> (u8, u8, u8) {
    (ENTRY_FILE, ENTRY_STREAM, ENTRY_NAME)
}
