//! NTFS 的 Update Sequence Array（fixup）处理。
//!
//! NTFS 在写每个 512 字节扇区时，把末尾 2 字节替换成序列号，真实值存在
//! 记录头部的 USA 数组里。不做 fixup 直接解析，记录尾部字段全是垃圾。

use dmdisk::{Error, Result};

/// 原地还原 fixup。`sector_size` 通常是 512。
pub fn apply(record: &mut [u8], sector_size: u32) -> Result<()> {
    if record.len() < 8 {
        return Err(Error::Format("记录太短，无法应用 fixup".into()));
    }
    let usa_off = u16::from_le_bytes([record[4], record[5]]) as usize;
    let usa_count = u16::from_le_bytes([record[6], record[7]]) as usize;
    if usa_count == 0 || usa_off + usa_count * 2 > record.len() {
        return Err(Error::Format("USA 越界".into()));
    }
    let ss = sector_size as usize;
    for i in 1..usa_count {
        let pos = i * ss;
        if pos < 2 || pos > record.len() {
            break;
        }
        let from = usa_off + i * 2;
        if from + 2 > record.len() {
            break;
        }
        record[pos - 2] = record[from];
        record[pos - 1] = record[from + 1];
    }
    Ok(())
}
