//! `dmdisk` —— 只读块设备访问 + 分区表解析。
//!
//! 设计约束：
//! * 零第三方依赖（纯 std），保证离线可构建、供应链干净；
//! * 一切打开路径都是只读，恢复工具绝不能先动现场；
//! * 所有越界读取都宽容降级为补零，因为恢复对象常常是截断的镜像。

pub mod crc32;
pub mod device;
pub mod error;
pub mod plist;
pub mod platform;
pub mod reader;
pub mod scheme;
pub mod write;

pub use device::{read_full, BlockDevice, ImageFile, RawDevice};
pub use error::{Error, Result};
pub use platform::{list_disks, open_source, DiskInfo, PartInfo};
pub use reader::Reader;
pub use scheme::{Partition, PartitionKind, PartitionTable, Scheme};

use std::sync::Arc;

/// 一个"源"：整盘/分区/镜像，统一抽象成 Reader。
pub struct Source {
    pub reader: Reader,
    pub label: String,
    pub size: u64,
    pub sector_size: u32,
    pub read_only: bool,
}

impl Source {
    pub fn open(path: &str, size: u64) -> Result<Self> {
        let dev = open_source(path, size)?;
        let sector_size = dev.sector_size();
        let size = dev.size();
        let read_only = dev.is_read_only();
        Ok(Self {
            reader: Reader::new(Arc::clone(&dev)),
            label: path.to_string(),
            size,
            sector_size,
            read_only,
        })
    }

    /// 在源内切出一个分区视图。
    pub fn volume(&self, offset: u64, len: u64) -> Reader {
        self.reader.slice(offset, len)
    }

    /// 自动识别分区表。
    pub fn table(&self) -> Result<PartitionTable> {
        scheme::detect(&self.reader)
    }
}

/// 容量格式化，例如 `238.5 GiB`。
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i + 1 < UNITS.len() {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else if v >= 100.0 {
        format!("{v:.1} {}", UNITS[i])
    } else {
        format!("{v:.2} {}", UNITS[i])
    }
}
