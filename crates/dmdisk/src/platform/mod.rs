//! 平台相关的磁盘枚举与打开。

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "windows")]
pub mod windows;

use crate::device::{BlockDevice, ImageFile, RawDevice};
use crate::error::Result;
use std::path::Path;
use std::sync::Arc;

/// 一块物理盘（或可寻址的存储介质）的摘要信息。
#[derive(Debug, Clone, Default)]
pub struct DiskInfo {
    /// BSD 名，例如 `disk4`
    pub bsd: String,
    /// 原始设备节点，例如 `/dev/rdisk4`
    pub dev_node: String,
    pub size: u64,
    pub model: String,
    pub bus: String,
    pub internal: bool,
    pub removable: bool,
    pub content: String,
    pub partitions: Vec<PartInfo>,
}

#[derive(Debug, Clone, Default)]
pub struct PartInfo {
    pub bsd: String,
    pub name: String,
    pub size: u64,
    pub content: String,
}

/// 枚举本机所有物理盘。不受支持的平台返回空列表 + 说明。
pub fn list_disks() -> Result<Vec<DiskInfo>> {
    #[cfg(target_os = "macos")]
    {
        return macos::list_disks();
    }
    #[cfg(target_os = "linux")]
    {
        return linux::list_disks();
    }
    #[cfg(target_os = "windows")]
    {
        return windows::list_disks();
    }
    #[allow(unreachable_code)]
    Ok(Vec::new())
}

/// 把路径统一打开成只读块设备：`/dev/*` 走原始设备，其它当镜像文件。
pub fn open_source(path: &str, size: u64) -> Result<Arc<dyn BlockDevice>> {
    let p = Path::new(path);
    let is_dev = path.starts_with("/dev/")
        || path.starts_with("\\\\.\\")
        || path.starts_with("\\\\?\\");
    if is_dev {
        let dev = RawDevice::open(p, size)?;
        Ok(Arc::new(dev))
    } else {
        let img = ImageFile::open(p)?;
        Ok(Arc::new(img))
    }
}
