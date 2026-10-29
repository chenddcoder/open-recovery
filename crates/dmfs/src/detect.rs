//! 文件系统探测与统一分发。

use crate::apfs::Apfs;
use crate::exfat::ExFat;
use crate::fat::{boot as fatboot, Fat};
use crate::types::*;
use dmdisk::{Error, Result, Reader};

/// 探测卷上是什么文件系统（只看签名，不做完整挂载）。
pub fn probe(reader: &Reader) -> FsKind {
    if reader.len() < 512 {
        return FsKind::Unknown;
    }
    let boot = match reader.read_vec(0, 512) {
        Ok(b) => b,
        Err(_) => return FsKind::Unknown,
    };
    if crate::exfat::boot::looks_like_exfat(&boot) {
        return FsKind::ExFat;
    }
    if &boot[3..11] == b"NTFS    " {
        return FsKind::Ntfs;
    }
    if &boot[32..36] == b"NXSB" {
        return FsKind::Apfs;
    }
    if fatboot::looks_like_fat(&boot) {
        if let Ok(b) = fatboot::parse(reader) {
            return match b.fat_type {
                fatboot::FatType::Fat12 => FsKind::Fat12,
                fatboot::FatType::Fat16 => FsKind::Fat16,
                fatboot::FatType::Fat32 => FsKind::Fat32,
            };
        }
    }
    // ext / HFS+ 的超级块固定在 1024 字节处
    if reader.len() >= 2048 {
        if let Ok(sb) = reader.read_vec(1024, 512) {
            if sb.len() == 512 {
                if u16::from_le_bytes([sb[56], sb[57]]) == 0xEF53 {
                    return ext_kind(&sb);
                }
                if &sb[0..2] == b"H+" || &sb[0..2] == b"HX" {
                    return FsKind::HfsPlus;
                }
            }
        }
    }
    FsKind::Unknown
}

/// 分区起点专用的宽容探测：块 0 损坏时靠 NXSB 检查点副本判定 APFS。
///
/// 只能对"已知是某个分区/卷起点"的 reader 调用。整盘/整镜像请用 [`probe`]——
/// 否则会把"文件深处恰好含 NXSB"（例如 dmg 镜像里的分区）误判成整盘就是一个容器。
pub fn probe_partition(reader: &Reader) -> FsKind {
    let k = probe(reader);
    if k == FsKind::Unknown && scan_apfs_nxsb(reader) {
        return FsKind::Apfs;
    }
    k
}

/// 在前 4 MiB 内找任意一份 NXSB（APFS 容器块 0 损坏时的兜底探测）。
pub(crate) fn scan_apfs_nxsb(reader: &Reader) -> bool {
    const WINDOW: u64 = 4 * 1024 * 1024;
    let n = reader.len().min(WINDOW) as usize;
    if n < 0x40 {
        return false;
    }
    let buf = match reader.read_vec(0, n) {
        Ok(b) => b,
        Err(_) => return false,
    };
    let mut from = 0usize;
    while let Some(p) = find_sub(&buf[from..], b"NXSB") {
        let magic_at = from + p;
        if magic_at >= 0x20 {
            let start = magic_at - 0x20;
            if start + 0x28 <= buf.len() {
                let bs = u32::from_le_bytes([
                    buf[start + 0x24],
                    buf[start + 0x25],
                    buf[start + 0x26],
                    buf[start + 0x27],
                ]);
                if bs.is_power_of_two() && (512..=65536).contains(&bs) {
                    return true;
                }
            }
        }
        from = magic_at + 4;
    }
    false
}

fn find_sub(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

fn ext_kind(sb: &[u8]) -> FsKind {
    let incompat = u32::from_le_bytes([sb[96], sb[97], sb[98], sb[99]]);
    let compat = u32::from_le_bytes([sb[92], sb[93], sb[94], sb[95]]);
    if incompat & (0x40 | 0x80 | 0x200 | 0x400) != 0 {
        FsKind::Ext4
    } else if compat & 0x4 != 0 {
        FsKind::Ext3
    } else {
        FsKind::Ext2
    }
}

/// 已挂载的文件系统（枚举分发的门面）。
pub enum AnyFs {
    Fat(Fat),
    ExFat(ExFat),
    Ntfs(crate::ntfs::Ntfs),
    Ext(crate::ext::Ext),
    HfsPlus(crate::hfsplus::HfsPlus),
    Apfs(Apfs),
}

impl AnyFs {
    pub fn open(reader: Reader) -> Result<Self> {
        let kind = probe(&reader);
        match kind {
            FsKind::Fat12 | FsKind::Fat16 | FsKind::Fat32 => Ok(AnyFs::Fat(Fat::open(reader)?)),
            FsKind::ExFat => Ok(AnyFs::ExFat(ExFat::open(reader)?)),
            FsKind::Ntfs => Ok(AnyFs::Ntfs(crate::ntfs::Ntfs::open(reader)?)),
            FsKind::Ext2 | FsKind::Ext3 | FsKind::Ext4 => {
                Ok(AnyFs::Ext(crate::ext::Ext::open(reader)?))
            }
            FsKind::HfsPlus => Ok(AnyFs::HfsPlus(crate::hfsplus::HfsPlus::open(reader)?)),
            FsKind::Apfs => Ok(AnyFs::Apfs(crate::apfs::open_volume(reader, 0)?)),
            FsKind::Unknown => {
                // 块 0 被覆盖的 APFS：魔数没了，但检查点副本通常还在。
                // 探测必须严格（否则会把"含有 NXSB 的镜像/分区表"误判成容器），
                // 但"打开"应当尽量宽容 —— 这正是坏盘能不能救回来的分界线。
                if scan_apfs_nxsb(&reader) {
                    return crate::apfs::open_volume(reader, 0).map(AnyFs::Apfs).map_err(|e| {
                        Error::Format(format!("发现 APFS 检查点副本，但容器打开失败: {e}"))
                    });
                }
                Err(Error::Format("无法识别的文件系统".into()))
            }
        }
    }

    /// 该文件系统专属的"删除后怎么救"说明（目前只有 APFS 有）。
    pub fn recovery_hint(&self) -> Option<Vec<String>> {
        match self {
            AnyFs::Apfs(f) => Some(f.recovery_hint()),
            _ => None,
        }
    }

    /// 诊断用：APFS 卷的记录类型直方图；非 APFS 返回 `None`。
    pub fn type_histogram(&self) -> Option<Result<Vec<(u8, u64)>>> {
        match self {
            AnyFs::Apfs(f) => Some(f.type_histogram()),
            _ => None,
        }
    }

    /// 打开指定卷（只有 APFS 容器有多卷概念，其它文件系统忽略 `index`）。
    pub fn open_volume(reader: Reader, index: usize) -> Result<Self> {
        if probe(&reader) == FsKind::Apfs {
            return Ok(AnyFs::Apfs(crate::apfs::open_volume(reader, index)?));
        }
        // 块 0 损坏的 APFS：同上，靠检查点副本硬开
        if scan_apfs_nxsb(&reader) {
            return crate::apfs::open_volume(reader, index).map(AnyFs::Apfs).map_err(|e| {
                Error::Format(format!("发现 APFS 检查点副本，但容器打开失败: {e}"))
            });
        }
        Self::open(reader)
    }

    /// 向下转型用：拿到具体实现的 `&dyn FileSystem`
    pub fn as_dyn(&self) -> &dyn FileSystem {
        match self {
            AnyFs::Fat(f) => f,
            AnyFs::ExFat(f) => f,
            AnyFs::Ntfs(f) => f,
            AnyFs::Ext(f) => f,
            AnyFs::HfsPlus(f) => f,
            AnyFs::Apfs(f) => f,
        }
    }
}

impl FileSystem for AnyFs {
    fn info(&self) -> &FsInfo {
        self.as_dyn().info()
    }
    fn root(&self) -> NodeId {
        self.as_dyn().root()
    }
    fn list(&self, dir: NodeId) -> Result<Vec<Entry>> {
        self.as_dyn().list(dir)
    }
    fn read(&self, entry: &Entry, out: &mut dyn std::io::Write) -> Result<u64> {
        self.as_dyn().read(entry, out)
    }
    fn deleted(&self) -> Result<Vec<Entry>> {
        self.as_dyn().deleted()
    }
    fn supports_deleted(&self) -> bool {
        self.as_dyn().supports_deleted()
    }
    fn dir_stats(&self, dir: NodeId) -> Option<(u64, u64)> {
        self.as_dyn().dir_stats(dir)
    }
}
