//! 块设备的极小范围写入。
//!
//! 全项目的默认姿态是只读；这里是**唯一**允许写设备的地方，而且刻意做成独立
//! 函数、不挂到 `BlockDevice` trait 上——解析路径永远拿不到写能力。
//!
//! 用途只有一个：把一份校验通过的 APFS 容器超级块副本写回块 0。
//! 所以做了这几条硬限制：
//!
//! * 单次最多 1 MiB（实际只用 4 KiB）；
//! * 必须显式给设备路径，不经过任何缓存层；
//! * 写完 `fsync`，避免掉电时只落一半。

use crate::error::{Error, Result};
use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom};

/// 单次写入上限。容器超级块 4 KiB，留 256 倍余量足够覆盖将来的用法。
pub const MAX_WRITE: usize = 1024 * 1024;

/// 往设备（或镜像文件）的 `offset` 处写入 `data`。
///
/// 只覆盖给定字节区间，前后内容一概不碰。
pub fn write_at_path(path: &str, offset: u64, data: &[u8]) -> Result<()> {
    if data.is_empty() {
        return Err(Error::Invalid("写入长度为 0".into()));
    }
    if data.len() > MAX_WRITE {
        return Err(Error::Invalid(format!(
            "单次写入 {} 字节超出上限 {} 字节",
            data.len(),
            MAX_WRITE
        )));
    }
    let mut f = OpenOptions::new().write(true).open(path)?;

    // 块设备的 metadata().len() 在多数平台上是 0，seek(End) 才拿得到真实容量。
    // 拿不到就跳过边界检查（写入仍受内核保护），拿到就一定要校验。
    let total = f.seek(SeekFrom::End(0)).unwrap_or(0);
    let end = offset.saturating_add(data.len() as u64);
    if total > 0 && end > total {
        return Err(Error::Bounds {
            what: format!("写入 {path}"),
            offset,
            len: data.len() as u64,
        });
    }

    write_all_at(&f, data, offset)?;
    // 必须 sync：这正是掉电最容易丢的地方，落盘才算完成
    f.sync_all()?;
    Ok(())
}

/// 读设备的指定区间（用于写前备份、写后校验）。
///
/// 不复用 `Reader`：那里的 64 KiB 页缓存会把刚写进去的内容盖住，
/// 导致"回读校验"验的是缓存而不是盘。
pub fn read_at_path(path: &str, offset: u64, len: usize) -> Result<Vec<u8>> {
    if len == 0 || len > MAX_WRITE {
        return Err(Error::Invalid(format!("读取长度 {len} 非法")));
    }
    let f = OpenOptions::new().read(true).open(path)?;
    let mut buf = vec![0u8; len];
    read_all_at(&f, &mut buf, offset)?;
    Ok(buf)
}

#[cfg(unix)]
fn write_all_at(f: &File, data: &[u8], offset: u64) -> Result<()> {
    use std::os::unix::fs::FileExt;
    let mut done = 0usize;
    while done < data.len() {
        let n = f.write_at(&data[done..], offset + done as u64)?;
        if n == 0 {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "设备写入返回 0 字节",
            )));
        }
        done += n;
    }
    Ok(())
}

#[cfg(unix)]
fn read_all_at(f: &File, buf: &mut [u8], offset: u64) -> Result<()> {
    use std::os::unix::fs::FileExt;
    let mut done = 0usize;
    while done < buf.len() {
        let n = f.read_at(&mut buf[done..], offset + done as u64)?;
        if n == 0 {
            return Err(Error::Bounds {
                what: "设备读取".into(),
                offset: offset + done as u64,
                len: (buf.len() - done) as u64,
            });
        }
        done += n;
    }
    Ok(())
}

#[cfg(windows)]
fn write_all_at(f: &File, data: &[u8], offset: u64) -> Result<()> {
    use std::os::windows::fs::FileExt;
    let mut done = 0usize;
    while done < data.len() {
        let n = f.seek_write(&data[done..], offset + done as u64)?;
        if n == 0 {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "设备写入返回 0 字节",
            )));
        }
        done += n;
    }
    Ok(())
}

#[cfg(windows)]
fn read_all_at(f: &File, buf: &mut [u8], offset: u64) -> Result<()> {
    use std::os::windows::fs::FileExt;
    let mut done = 0usize;
    while done < buf.len() {
        let n = f.seek_read(&mut buf[done..], offset + done as u64)?;
        if n == 0 {
            return Err(Error::Bounds {
                what: "设备读取".into(),
                offset: offset + done as u64,
                len: (buf.len() - done) as u64,
            });
        }
        done += n;
    }
    Ok(())
}
