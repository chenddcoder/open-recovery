//! NTFS 引导扇区与卷参数。
//!
//! 注意 NTFS 的 BPB 与 FAT 布局不同：TotalSectors / $MFT LCN 都在 0x28 之后，
//! 直接套 FAT 的偏移会读到 0，这是很多人写 NTFS 解析时踩的第一个坑。

use dmdisk::{Error, Result, Reader};

#[derive(Debug, Clone)]
pub struct NtfsBoot {
    pub bytes_per_sector: u32,
    pub sectors_per_cluster: u32,
    pub total_sectors: u64,
    pub mft_lcn: u64,
    pub mft_mirror_lcn: u64,
    pub clusters_per_record: i8,
    pub clusters_per_index: i8,
    pub serial: u64,
    pub cluster_size: u32,
    pub record_size: u32,
}

impl NtfsBoot {
    pub fn volume_bytes(&self) -> u64 {
        self.total_sectors * self.bytes_per_sector as u64
    }
    pub fn lcn_offset(&self, lcn: u64) -> u64 {
        lcn * self.cluster_size as u64
    }
}

pub fn looks_like_ntfs(boot: &[u8]) -> bool {
    boot.len() >= 512 && &boot[3..11] == b"NTFS    " && boot[510] == 0x55 && boot[511] == 0xAA
}

/// NTFS 的 record/index 尺寸字段：正数表示"多少个簇"，负数表示 2^n 字节。
fn size_from_clusters(v: i8, cluster_size: u32) -> u32 {
    if v > 0 {
        v as u32 * cluster_size
    } else {
        1u32 << (-(v as i32)).clamp(1, 30) as u32
    }
}

pub fn parse(reader: &Reader) -> Result<NtfsBoot> {
    let b = reader.read_vec(0, 512)?;
    if !looks_like_ntfs(&b) {
        return Err(Error::Format("不是 NTFS 引导扇区".into()));
    }
    let bytes_per_sector = u16::from_le_bytes([b[11], b[12]]) as u32;
    let sectors_per_cluster = b[13] as u32;
    if !(512..=4096).contains(&bytes_per_sector)
        || sectors_per_cluster == 0
        || !sectors_per_cluster.is_power_of_two()
    {
        return Err(Error::Format("NTFS BPB 参数异常".into()));
    }
    let cluster_size = bytes_per_sector * sectors_per_cluster;
    let total_sectors = u64::from_le_bytes(b[40..48].try_into().unwrap());
    let mft_lcn = u64::from_le_bytes(b[48..56].try_into().unwrap());
    let cpr = b[64] as i8;
    let cpi = b[65] as i8;
    let mut serial_bytes = [0u8; 8];
    serial_bytes.copy_from_slice(&b[69..77]);

    if total_sectors == 0 {
        return Err(Error::Format("NTFS 总扇区数为 0".into()));
    }
    if mft_lcn == 0 || mft_lcn >= total_sectors / sectors_per_cluster as u64 + 1 {
        return Err(Error::Format(format!("NTFS $MFT 位置非法: LCN={mft_lcn}")));
    }

    Ok(NtfsBoot {
        bytes_per_sector,
        sectors_per_cluster,
        total_sectors,
        mft_lcn,
        mft_mirror_lcn: u64::from_le_bytes(b[56..64].try_into().unwrap()),
        clusters_per_record: cpr,
        clusters_per_index: cpi,
        serial: u64::from_le_bytes(serial_bytes),
        cluster_size,
        record_size: size_from_clusters(cpr, cluster_size),
    })
}
