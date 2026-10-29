//! ext2/3/4 inode 解析。

use crate::types::NodeKind;
use crate::util::fmt_unix;

#[derive(Debug, Clone)]
pub struct Inode {
    pub number: u64,
    pub mode: u16,
    pub size: u64,
    pub links: u16,
    pub dtime: u32,
    pub mtime: Option<i64>,
    pub ctime: Option<i64>,
    pub flags: u32,
    pub blocks_512: u32,
    pub i_block: [u8; 60],
}

impl Inode {
    pub fn kind(&self) -> NodeKind {
        match self.mode & 0xF000 {
            0x4000 => NodeKind::Dir,
            0x8000 => NodeKind::File,
            0xA000 => NodeKind::Symlink,
            _ => NodeKind::Other,
        }
    }

    pub fn is_regular(&self) -> bool {
        self.mode & 0xF000 == 0x8000
    }

    pub fn is_dir(&self) -> bool {
        self.mode & 0xF000 == 0x4000
    }

    pub fn is_deleted(&self) -> bool {
        self.links == 0 && self.dtime > 0
    }

    pub fn mode_string(&self) -> String {
        let kind = match self.mode & 0xF000 {
            0x4000 => 'd',
            0x8000 => '-',
            0xA000 => 'l',
            0x2000 => 'c',
            0x6000 => 'b',
            0x1000 => 'p',
            _ => '?',
        };
        let perm = |shift: u16| -> String {
            let bits = (self.mode >> shift) & 0o7;
            format!(
                "{}{}{}",
                if bits & 4 != 0 { 'r' } else { '-' },
                if bits & 2 != 0 { 'w' } else { '-' },
                if bits & 1 != 0 { 'x' } else { '-' }
            )
        };
        format!("{}{}{}{}", kind, perm(6), perm(3), perm(0))
    }

    pub fn mtime_string(&self) -> String {
        fmt_unix(self.mtime)
    }
}

pub fn parse(number: u64, raw: &[u8]) -> Inode {
    let mode = u16::from_le_bytes([raw[0], raw[1]]);
    let size_lo = u32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]) as u64;
    let size_hi = if raw.len() >= 112 {
        u32::from_le_bytes([raw[108], raw[109], raw[110], raw[111]]) as u64
    } else {
        0
    };
    let is_regular = mode & 0xF000 == 0x8000;
    let size = if is_regular { size_lo | (size_hi << 32) } else { size_lo };
    let mut i_block = [0u8; 60];
    let n = 60.min(raw.len().saturating_sub(40));
    i_block[..n].copy_from_slice(&raw[40..40 + n]);

    Inode {
        number,
        mode,
        size,
        links: u16::from_le_bytes([raw[26], raw[27]]),
        dtime: u32::from_le_bytes([raw[20], raw[21], raw[22], raw[23]]),
        mtime: to_unix(raw[16..20].try_into().unwrap()),
        ctime: to_unix(raw[12..16].try_into().unwrap()),
        flags: u32::from_le_bytes([raw[32], raw[33], raw[34], raw[35]]),
        blocks_512: u32::from_le_bytes([raw[28], raw[29], raw[30], raw[31]]),
        i_block,
    }
}

fn to_unix(b: [u8; 4]) -> Option<i64> {
    let v = u32::from_le_bytes(b);
    if v == 0 {
        None
    } else {
        Some(v as i64)
    }
}

pub const EXTENTS_FL: u32 = 0x0008_0000;
