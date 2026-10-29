//! exFAT 卷头（VBR）解析。

use dmdisk::{Error, Result, Reader};

#[derive(Debug, Clone)]
pub struct ExFatBoot {
    pub bytes_per_sector: u32,
    pub sectors_per_cluster: u32,
    pub partition_offset: u64,
    pub volume_length: u64,
    pub fat_offset: u32,
    pub fat_length: u32,
    pub cluster_heap_offset: u32,
    pub cluster_count: u32,
    pub root_cluster: u32,
    pub serial: u32,
    pub revision: u16,
    pub flags: u16,
    pub num_fats: u8,
    pub percent_in_use: u8,
}

impl ExFatBoot {
    pub fn cluster_bytes(&self) -> u64 {
        self.bytes_per_sector as u64 * self.sectors_per_cluster as u64
    }

    pub fn cluster_offset(&self, cluster: u32) -> u64 {
        let sector = self.cluster_heap_offset as u64
            + (cluster as u64).saturating_sub(2) * self.sectors_per_cluster as u64;
        sector * self.bytes_per_sector as u64
    }

    pub fn volume_bytes(&self) -> u64 {
        self.volume_length * self.bytes_per_sector as u64
    }
}

pub fn looks_like_exfat(boot: &[u8]) -> bool {
    boot.len() >= 512 && &boot[3..11] == b"EXFAT   " && boot[510] == 0x55 && boot[511] == 0xAA
}

pub fn parse(reader: &Reader) -> Result<ExFatBoot> {
    let b = reader.read_vec(0, 512)?;
    if !looks_like_exfat(&b) {
        return Err(Error::Format("不是 exFAT 引导扇区".into()));
    }
    let bps_shift = b[108];
    let spc_shift = b[109];
    if bps_shift < 9 || bps_shift > 12 {
        return Err(Error::Format(format!("exFAT 扇区位移异常: {bps_shift}")));
    }
    if spc_shift > 25 {
        return Err(Error::Format(format!("exFAT 簇位移异常: {spc_shift}")));
    }
    let bytes_per_sector = 1u32 << bps_shift;
    let sectors_per_cluster = 1u32 << spc_shift;
    let boot = ExFatBoot {
        bytes_per_sector,
        sectors_per_cluster,
        partition_offset: u64::from_le_bytes(b[64..72].try_into().unwrap()),
        volume_length: u64::from_le_bytes(b[72..80].try_into().unwrap()),
        fat_offset: u32::from_le_bytes(b[80..84].try_into().unwrap()),
        fat_length: u32::from_le_bytes(b[84..88].try_into().unwrap()),
        cluster_heap_offset: u32::from_le_bytes(b[88..92].try_into().unwrap()),
        cluster_count: u32::from_le_bytes(b[92..96].try_into().unwrap()),
        root_cluster: u32::from_le_bytes(b[96..100].try_into().unwrap()),
        serial: u32::from_le_bytes(b[100..104].try_into().unwrap()),
        revision: u16::from_le_bytes([b[104], b[105]]),
        flags: u16::from_le_bytes([b[106], b[107]]),
        num_fats: b[110],
        percent_in_use: b[112],
    };
    if boot.cluster_count == 0 || boot.root_cluster < 2 {
        return Err(Error::Format("exFAT 簇堆参数异常".into()));
    }
    Ok(boot)
}
