//! FAT12/16/32 引导扇区（BPB）解析。
//!
//! 注意 FAT 类型不能靠"BPB 里写的字符串"判断，必须按簇数推导——
//! 这是微软规范里的硬定义，也是识别被改坏的卷最靠谱的依据。

use dmdisk::{Error, Result, Reader};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FatType {
    Fat12,
    Fat16,
    Fat32,
}

#[derive(Debug, Clone)]
pub struct FatBoot {
    pub bps: u32,
    pub spc: u32,
    pub reserved: u32,
    pub num_fats: u32,
    pub root_entries: u32,
    pub total_sectors: u32,
    pub media: u8,
    pub fat_sectors: u32,
    pub root_cluster: u32,
    pub fsinfo_sector: u32,
    pub backup_boot_sector: u32,
    pub fat_type: FatType,
    pub serial: u32,
    pub label: String,
    /// 派生字段
    pub cluster_count: u32,
    pub first_data_sector: u32,
    pub root_dir_sector: u32,
    pub root_dir_sectors: u32,
    pub volume_bytes: u64,
}

impl FatBoot {
    /// 簇号 → 卷内字节偏移
    pub fn cluster_offset(&self, cluster: u32) -> u64 {
        let sector = self.first_data_sector as u64
            + (cluster as u64).saturating_sub(2) * self.spc as u64;
        sector * self.bps as u64
    }

    pub fn cluster_bytes(&self) -> u64 {
        self.spc as u64 * self.bps as u64
    }
}

/// 判断一个 512 字节扇区像不像 FAT 引导扇区（供探针使用）。
pub fn looks_like_fat(boot: &[u8]) -> bool {
    if boot.len() < 512 {
        return false;
    }
    if boot[510] != 0x55 || boot[511] != 0xAA {
        return false;
    }
    let bps = u16::from_le_bytes([boot[11], boot[12]]);
    let spc = boot[13];
    let fats = boot[16];
    matches!(bps, 512 | 1024 | 2048 | 4096) && spc.is_power_of_two() && spc <= 128 && (1..=4).contains(&fats)
}

pub fn parse(reader: &Reader) -> Result<FatBoot> {
    let boot = reader.read_vec(0, 512)?;
    if !looks_like_fat(&boot) {
        return Err(Error::Format("不是 FAT 引导扇区".into()));
    }
    let bps = u16::from_le_bytes([boot[11], boot[12]]) as u32;
    let spc = boot[13] as u32;
    let reserved = u16::from_le_bytes([boot[14], boot[15]]) as u32;
    let num_fats = boot[16] as u32;
    let root_entries = u16::from_le_bytes([boot[17], boot[18]]) as u32;
    let total16 = u16::from_le_bytes([boot[19], boot[20]]) as u32;
    let media = boot[21];
    let fat_size16 = u16::from_le_bytes([boot[22], boot[23]]) as u32;
    let total32 = u32::from_le_bytes([boot[32], boot[33], boot[34], boot[35]]);

    if reserved == 0 || spc == 0 || num_fats == 0 {
        return Err(Error::Format("FAT BPB 关键字段为 0".into()));
    }

    let total_sectors = if total16 != 0 { total16 } else { total32 };
    if total_sectors == 0 {
        return Err(Error::Format("FAT 卷总扇区数为 0".into()));
    }

    let fat_size32 = u32::from_le_bytes([boot[36], boot[37], boot[38], boot[39]]);
    let fat_sectors = if fat_size16 != 0 { fat_size16 } else { fat_size32 };
    if fat_sectors == 0 {
        return Err(Error::Format("FAT 表长度为 0".into()));
    }

    let root_dir_sectors = (root_entries * 32).div_ceil(bps);
    let first_data_sector = reserved + num_fats * fat_sectors + root_dir_sectors;
    if first_data_sector >= total_sectors {
        return Err(Error::Format(format!(
            "FAT 布局自相矛盾：first_data_sector={first_data_sector} >= total={total_sectors}"
        )));
    }
    let data_sectors = total_sectors - first_data_sector;
    let cluster_count = data_sectors / spc;
    let fat_type = if cluster_count < 4085 {
        FatType::Fat12
    } else if cluster_count < 65525 {
        FatType::Fat16
    } else {
        FatType::Fat32
    };

    let root_cluster = if fat_type == FatType::Fat32 {
        u32::from_le_bytes([boot[44], boot[45], boot[46], boot[47]])
    } else {
        0
    };
    let fsinfo_sector = u16::from_le_bytes([boot[48], boot[49]]) as u32;
    let backup_boot_sector = u16::from_le_bytes([boot[50], boot[51]]) as u32;
    // 卷序列号/卷标的偏移在 FAT12/16 与 FAT32 上不同（这是 FAT 规范里最容易搞错的一处）：
    //   FAT12/16: 序列号 0x27，卷标 0x2B
    //   FAT32   : 序列号 0x43，卷标 0x47
    let (serial_off, label_off) = if fat_type == FatType::Fat32 {
        (67usize, 71usize)
    } else {
        (39usize, 43usize)
    };
    let serial = u32::from_le_bytes([
        boot[serial_off],
        boot[serial_off + 1],
        boot[serial_off + 2],
        boot[serial_off + 3],
    ]);
    let label_raw = &boot[label_off..label_off + 11];
    let label = String::from_utf8_lossy(label_raw)
        .trim_matches(|c: char| c == '\0' || c == ' ')
        .to_string();

    Ok(FatBoot {
        bps,
        spc,
        reserved,
        num_fats,
        root_entries,
        total_sectors,
        media,
        fat_sectors,
        root_cluster,
        fsinfo_sector,
        backup_boot_sector,
        fat_type,
        serial,
        label,
        cluster_count,
        first_data_sector,
        root_dir_sector: reserved + num_fats * fat_sectors,
        root_dir_sectors,
        volume_bytes: total_sectors as u64 * bps as u64,
    })
}
