//! 文件系统探测与统一分发。

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
            FsKind::Apfs => Err(Error::Unsupported(
                "APFS 目前只做到容器识别：能认出这是 APFS 容器，但还不能列目录/读文件。\
                 文件级解析（对象映射 B-tree、fs-tree、快照）正在实现中；\
                 `dmde-rs volumes` 可以看到容器与卷的基本信息"
                    .into(),
            )),
            FsKind::Unknown => Err(Error::Format("无法识别的文件系统".into())),
        }
    }

    /// 向下转型用：拿到具体实现的 `&dyn FileSystem`
    pub fn as_dyn(&self) -> &dyn FileSystem {
        match self {
            AnyFs::Fat(f) => f,
            AnyFs::ExFat(f) => f,
            AnyFs::Ntfs(f) => f,
            AnyFs::Ext(f) => f,
            AnyFs::HfsPlus(f) => f,
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
}
