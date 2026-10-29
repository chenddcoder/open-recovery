//! 分区表：MBR / GPT / APM 的解析与自动识别。
//!
//! 恢复场景下"识别"比"解析"更关键：坏盘的备份表、被覆盖的表都要靠探针去试。
//! 这里按 GPT → APM → MBR 的顺序尝试，并把每次尝试的结论记进 `notes`。

pub mod apm;
pub mod gpt;
pub mod mbr;

use crate::error::Result;
use crate::reader::Reader;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    /// 无分区表（整盘一个文件系统，常见于 U 盘/镜像）
    None,
    Mbr,
    Gpt,
    Apm,
}

impl Scheme {
    pub fn as_str(&self) -> &'static str {
        match self {
            Scheme::None => "none",
            Scheme::Mbr => "MBR",
            Scheme::Gpt => "GPT",
            Scheme::Apm => "APM",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartitionKind {
    Primary,
    Extended,
    Logical,
    /// GPT 备份表里记录的项
    Backup,
}

#[derive(Debug, Clone)]
pub struct Partition {
    pub index: usize,
    pub scheme: Scheme,
    pub kind: PartitionKind,
    /// 相对磁盘起始的字节偏移
    pub offset: u64,
    pub size: u64,
    pub name: String,
    /// 类型的人类可读名，例如 "Apple APFS" / "Microsoft basic data"
    pub type_name: String,
    /// 原始类型标识（MBR 十六进制 / GPT UUID / APM 名称）
    pub type_id: String,
    pub uuid: Option<String>,
    pub bootable: bool,
    pub attributes: u64,
}

impl Partition {
    pub fn end(&self) -> u64 {
        self.offset.saturating_add(self.size)
    }

    pub fn contains(&self, offset: u64) -> bool {
        offset >= self.offset && offset < self.end()
    }
}

#[derive(Debug, Clone)]
pub struct PartitionTable {
    pub scheme: Scheme,
    pub sector_size: u32,
    pub partitions: Vec<Partition>,
    pub notes: Vec<String>,
    /// 分区表本身是否校验通过（GPT CRC）
    pub verified: bool,
}

impl PartitionTable {
    pub fn empty(sector_size: u32) -> Self {
        Self {
            scheme: Scheme::None,
            sector_size,
            partitions: Vec::new(),
            notes: Vec::new(),
            verified: false,
        }
    }

    /// 找到覆盖某个字节偏移的分区。
    pub fn partition_at(&self, offset: u64) -> Option<&Partition> {
        self.partitions.iter().find(|p| p.contains(offset))
    }
}

/// 自动识别分区表。
pub fn detect(reader: &Reader) -> Result<PartitionTable> {
    let mut notes = Vec::new();

    // 1) GPT：先按常见 512 字节扇区试，再按 4K 试
    for lba in [512u64, 4096u64] {
        if reader.len() < lba * 2 {
            continue;
        }
        if let Ok(head) = reader.read_vec(lba, 8) {
            if head == b"EFI PART" {
                match gpt::parse(reader, lba) {
                    Ok(mut t) => {
                        if lba != 512 {
                            notes.push(format!("GPT 头位于 LBA1={lba} 字节，判定为 4K 扇区盘"));
                        }
                        t.notes.append(&mut notes);
                        return Ok(t);
                    }
                    Err(e) => notes.push(format!("GPT 头存在但解析失败: {e}")),
                }
            }
        }
    }

    // 2) APM：块 1 偏移 0 处是 "PM"
    for bs in [512u64, 2048u64] {
        if let Ok(sig) = reader.read_vec(bs, 2) {
            if sig == b"PM" {
                match apm::parse(reader, bs) {
                    Ok(mut t) => {
                        t.notes.append(&mut notes);
                        return Ok(t);
                    }
                    Err(e) => notes.push(format!("APM 签名存在但解析失败: {e}")),
                }
            }
        }
    }

    // 3) MBR
    match mbr::parse(reader) {
        Ok(Some(mut t)) => {
            t.notes.append(&mut notes);
            return Ok(t);
        }
        Ok(None) => notes.push("无有效 0x55AA 引导签名".into()),
        Err(e) => notes.push(format!("MBR 解析失败: {e}")),
    }

    let mut t = PartitionTable::empty(reader.sector_size());
    t.notes = notes;
    Ok(t)
}
