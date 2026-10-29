//! ext2/3/4 超级块解析。

use crate::types::FsKind;
use dmdisk::{Error, Result, Reader};

pub const EXT_MAGIC: u16 = 0xEF53;
pub const INCOMPAT_EXTENTS: u32 = 0x40;
pub const INCOMPAT_64BIT: u32 = 0x80;
pub const INCOMPAT_META_BG: u32 = 0x10;

#[derive(Debug, Clone)]
pub struct ExtSuper {
    pub block_size: u32,
    pub blocks_count: u64,
    pub inodes_count: u32,
    pub free_blocks: u64,
    pub free_inodes: u32,
    pub first_data_block: u32,
    pub blocks_per_group: u32,
    pub inodes_per_group: u32,
    pub inode_size: u32,
    pub desc_size: u32,
    pub first_ino: u32,
    pub incompat: u32,
    pub compat: u32,
    pub ro_compat: u32,
    pub journal_inum: u32,
    pub label: String,
    pub uuid: String,
    pub kind: FsKind,
    pub gdt_offset: u64,
}

impl ExtSuper {
    pub fn group_count(&self) -> u64 {
        let blocks = self.blocks_count.saturating_sub(self.first_data_block as u64);
        blocks.div_ceil(self.blocks_per_group as u64)
    }

    pub fn has_extents(&self) -> bool {
        self.incompat & INCOMPAT_EXTENTS != 0
    }

    pub fn is_64bit(&self) -> bool {
        self.incompat & INCOMPAT_64BIT != 0
    }
}

pub fn looks_like_ext(sb: &[u8]) -> bool {
    sb.len() >= 120 && u16::from_le_bytes([sb[56], sb[57]]) == EXT_MAGIC
}

fn uuid_string(b: &[u8]) -> String {
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}

pub fn parse(reader: &Reader) -> Result<ExtSuper> {
    let sb = reader.read_vec(1024, 1024)?;
    if !looks_like_ext(&sb) {
        return Err(Error::Format("不是 ext2/3/4 超级块".into()));
    }
    let log_block = u32::from_le_bytes(sb[24..28].try_into().unwrap());
    if log_block > 6 {
        return Err(Error::Format(format!("ext 块大小位移异常: {log_block}")));
    }
    let block_size = 1024u32 << log_block;
    let compat = u32::from_le_bytes(sb[92..96].try_into().unwrap());
    let incompat = u32::from_le_bytes(sb[96..100].try_into().unwrap());
    let ro_compat = u32::from_le_bytes(sb[100..104].try_into().unwrap());

    let mut blocks_count = u32::from_le_bytes(sb[4..8].try_into().unwrap()) as u64;
    let mut free_blocks = u32::from_le_bytes(sb[12..16].try_into().unwrap()) as u64;
    if incompat & INCOMPAT_64BIT != 0 && sb.len() >= 340 {
        let hi = u32::from_le_bytes(sb[336..340].try_into().unwrap()) as u64;
        blocks_count |= hi << 32;
        let hi_free = u16::from_le_bytes([sb[0x158], sb[0x159]]) as u64;
        free_blocks |= hi_free << 32;
    }
    let inode_size = u16::from_le_bytes([sb[88], sb[89]]) as u32;
    let inode_size = if inode_size == 0 { 128 } else { inode_size };
    let desc_size = if incompat & INCOMPAT_64BIT != 0 {
        u16::from_le_bytes([sb[254], sb[255]]) as u32
    } else {
        32
    };
    let desc_size = if desc_size < 32 { 32 } else { desc_size };

    let first_data_block = u32::from_le_bytes(sb[20..24].try_into().unwrap());
    let kind = if incompat & (INCOMPAT_EXTENTS | INCOMPAT_64BIT | 0x200 | 0x400) != 0 {
        FsKind::Ext4
    } else if compat & 0x4 != 0 {
        FsKind::Ext3
    } else {
        FsKind::Ext2
    };

    let label_end = sb[120..136].iter().position(|&c| c == 0).unwrap_or(16);
    let label = String::from_utf8_lossy(&sb[120..120 + label_end]).to_string();

    Ok(ExtSuper {
        block_size,
        blocks_count,
        inodes_count: u32::from_le_bytes(sb[0..4].try_into().unwrap()),
        free_blocks,
        free_inodes: u32::from_le_bytes(sb[16..20].try_into().unwrap()),
        first_data_block,
        blocks_per_group: u32::from_le_bytes(sb[32..36].try_into().unwrap()),
        inodes_per_group: u32::from_le_bytes(sb[40..44].try_into().unwrap()),
        inode_size,
        desc_size,
        first_ino: u32::from_le_bytes(sb[84..88].try_into().unwrap()),
        incompat,
        compat,
        ro_compat,
        journal_inum: u32::from_le_bytes(sb[240..244].try_into().unwrap()),
        label,
        uuid: uuid_string(&sb[104..120]),
        kind,
        gdt_offset: (first_data_block as u64 + 1) * block_size as u64,
    })
}
