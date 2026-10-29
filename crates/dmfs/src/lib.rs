//! `dmfs` —— 只读文件系统解析与删除文件恢复。
//!
//! 支持：FAT12/16/32、exFAT、NTFS、ext2/3/4、HFS+、APFS（含对象映射与 fs-tree）。
//! 所有实现都基于 `dmdisk::Reader`，不持有自己的设备句柄，方便一个进程里
//! 同时挂载多个分区。

pub mod apfs;
pub mod detect;
pub mod exfat;
pub mod ext;
pub mod fat;
pub mod hfsplus;
pub mod ntfs;
pub mod types;
pub mod util;

pub use detect::{probe, AnyFs};
pub use dmdisk::{Error, Result};
pub use types::{resolve, sanitize_name, Entry, FileSystem, FsInfo, FsKind, NodeId, NodeKind};
