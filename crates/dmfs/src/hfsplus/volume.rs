//! HFS+ 卷头（Volume Header）解析。

use dmdisk::{Error, Result, Reader};

#[derive(Debug, Clone)]
pub struct Fork {
    pub logical_size: u64,
    pub clump_size: u32,
    pub total_blocks: u32,
    /// (起始块, 块数)
    pub extents: Vec<(u32, u32)>,
}

impl Fork {
    pub fn blocks_covered(&self) -> u64 {
        self.extents.iter().map(|(_, n)| *n as u64).sum()
    }
}

#[derive(Debug, Clone)]
pub struct HfsVolume {
    pub signature: [u8; 2],
    pub version: u16,
    pub block_size: u32,
    pub total_blocks: u64,
    pub free_blocks: u64,
    pub file_count: u32,
    pub folder_count: u32,
    pub catalog: Fork,
    pub extents_file: Fork,
    pub allocation: Fork,
}

pub fn looks_like_hfs(vh: &[u8]) -> bool {
    vh.len() >= 512 && (&vh[0..2] == b"H+" || &vh[0..2] == b"HX")
}

pub fn parse_fork(b: &[u8]) -> Fork {
    if b.len() < 80 {
        return Fork { logical_size: 0, clump_size: 0, total_blocks: 0, extents: Vec::new() };
    }
    let logical_size = u64::from_be_bytes(b[0..8].try_into().unwrap());
    let clump_size = u32::from_be_bytes(b[8..12].try_into().unwrap());
    let total_blocks = u32::from_be_bytes(b[12..16].try_into().unwrap());
    let mut extents = Vec::new();
    for i in 0..8 {
        let off = 16 + i * 8;
        let start = u32::from_be_bytes(b[off..off + 4].try_into().unwrap());
        let count = u32::from_be_bytes(b[off + 4..off + 8].try_into().unwrap());
        if count > 0 {
            extents.push((start, count));
        }
    }
    Fork { logical_size, clump_size, total_blocks, extents }
}

pub fn parse(reader: &Reader) -> Result<HfsVolume> {
    let vh = reader.read_vec(1024, 512)?;
    if !looks_like_hfs(&vh) {
        return Err(Error::Format("不是 HFS+ 卷头".into()));
    }
    let block_size = u32::from_be_bytes(vh[40..44].try_into().unwrap());
    if block_size < 512 || !block_size.is_power_of_two() {
        return Err(Error::Format(format!("HFS+ 块大小异常: {block_size}")));
    }
    let total_blocks = u32::from_be_bytes(vh[44..48].try_into().unwrap()) as u64;
    let free_blocks = u32::from_be_bytes(vh[48..52].try_into().unwrap()) as u64;
    Ok(HfsVolume {
        signature: [vh[0], vh[1]],
        version: u16::from_be_bytes([vh[2], vh[3]]),
        block_size,
        total_blocks,
        free_blocks,
        file_count: u32::from_be_bytes(vh[32..36].try_into().unwrap()),
        folder_count: u32::from_be_bytes(vh[36..40].try_into().unwrap()),
        allocation: parse_fork(&vh[112..192]),
        extents_file: parse_fork(&vh[192..272]),
        catalog: parse_fork(&vh[272..352]),
    })
}
