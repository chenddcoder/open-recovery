//! Windows 磁盘枚举占位实现。
//!
//! 后续版本会用 `CreateFileW("\\\\.\\PhysicalDriveN")` + `DeviceIoControl`
//! (IOCTL_DISK_GET_DRIVE_GEOMETRY_EX / IOCTL_STORAGE_QUERY_PROPERTY) 实现。
//! 本版本先明确报"不支持"，避免用户以为能用却静默失败。

use super::DiskInfo;
use crate::error::{Error, Result};

pub fn list_disks() -> Result<Vec<DiskInfo>> {
    Err(Error::Unsupported(
        "Windows 平台枚举尚未实现（当前版本只支持 macOS / Linux），后续会通过 PhysicalDrive 接口补齐".into(),
    ))
}
