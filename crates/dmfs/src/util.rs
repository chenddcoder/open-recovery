//! 各文件系统共用的小工具：DOS 时间、FILETIME、字节读取等。

/// FAT 的 DOS 日期/时间 → Unix 秒（本地时间按 UTC 处理，恢复场景够用）。
pub fn dos_to_unix(date: u16, time: u16) -> Option<i64> {
    if date == 0 {
        return None;
    }
    let year = 1980 + ((date >> 9) & 0x7F) as i64;
    let month = ((date >> 5) & 0x0F) as i64;
    let day = (date & 0x1F) as i64;
    let hour = ((time >> 11) & 0x1F) as i64;
    let min = ((time >> 5) & 0x3F) as i64;
    let sec = ((time & 0x1F) * 2) as i64;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    Some(civil_to_unix(year, month, day, hour, min, sec))
}

/// NTFS FILETIME（100ns since 1601-01-01）→ Unix 秒
pub fn filetime_to_unix(ft: u64) -> Option<i64> {
    if ft == 0 {
        return None;
    }
    const EPOCH_DIFF: u64 = 116_444_736_000_000_000; // 1601→1970，单位 100ns
    if ft < EPOCH_DIFF {
        return None;
    }
    Some(((ft - EPOCH_DIFF) / 10_000_000) as i64)
}

/// HFS+/APFS 时间（1904-01-01 起的秒数）→ Unix 秒
pub fn hfs_to_unix(secs: u32) -> Option<i64> {
    if secs == 0 {
        return None;
    }
    const EPOCH_DIFF: i64 = 2_082_844_800;
    Some(secs as i64 - EPOCH_DIFF)
}

fn civil_to_unix(y: i64, m: i64, d: i64, hh: i64, mm: i64, ss: i64) -> i64 {
    // Howard Hinnant 的 days_from_civil 算法
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = if y2 >= 0 { y2 } else { y2 - 399 } / 400;
    let yoe = y2 - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    days * 86_400 + hh * 3600 + mm * 60 + ss
}

/// UTF-16LE 解码（遇到 0 或长度上限即停止）
pub fn utf16le_to_string(b: &[u8]) -> String {
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
    String::from_utf16_lossy(&units)
}

/// UTF-16BE 解码（HFS+ 用大写端序）
pub fn utf16be_to_string(b: &[u8]) -> String {
    let mut units = Vec::with_capacity(b.len() / 2);
    let mut i = 0;
    while i + 1 < b.len() {
        let u = u16::from_be_bytes([b[i], b[i + 1]]);
        if u == 0 {
            break;
        }
        units.push(u);
        i += 2;
    }
    String::from_utf16_lossy(&units)
}

/// 把 unix 秒格式化成 `YYYY-MM-DD HH:MM:SS`（UTC），失败返回 `-`。
pub fn fmt_unix(ts: Option<i64>) -> String {
    let t = match ts {
        Some(t) if t > 0 => t,
        _ => return "-".to_string(),
    };
    let days = t.div_euclid(86_400);
    let secs = t.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        y,
        m,
        d,
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub fn le16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}

pub fn le32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

pub fn le64(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes([
        b[off],
        b[off + 1],
        b[off + 2],
        b[off + 3],
        b[off + 4],
        b[off + 5],
        b[off + 6],
        b[off + 7],
    ])
}

pub fn be16(b: &[u8], off: usize) -> u16 {
    u16::from_be_bytes([b[off], b[off + 1]])
}

pub fn be32(b: &[u8], off: usize) -> u32 {
    u32::from_be_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

pub fn be64(b: &[u8], off: usize) -> u64 {
    u64::from_be_bytes([
        b[off],
        b[off + 1],
        b[off + 2],
        b[off + 3],
        b[off + 4],
        b[off + 5],
        b[off + 6],
        b[off + 7],
    ])
}
