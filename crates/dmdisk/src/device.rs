//! 块设备抽象层。
//!
//! 目标是同一套解析代码既能跑在物理盘（`/dev/rdisk4`）上，也能跑在镜像文件上。
//! 全部以只读方式打开——恢复工具的第一原则是"先别写坏现场"。

use crate::error::{Error, Result};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

/// 只读块设备。
pub trait BlockDevice: Send + Sync {
    /// 读取 `buf.len()` 字节到 `buf`，返回实际读到多少（可能短读）。
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize>;
    /// 设备总字节数。
    fn size(&self) -> u64;
    /// 逻辑扇区大小（512 / 4096）。
    fn sector_size(&self) -> u32 {
        512
    }
    /// 人类可读标签，例如 `/dev/rdisk4` 或 `/tmp/disk.img`。
    fn label(&self) -> String;
    /// 是否以只读方式打开。写支持留到后续版本。
    fn is_read_only(&self) -> bool {
        true
    }
}

/// 循环读满 `buf`；到设备末尾后剩余补零。
///
/// 之所以补零而不是报错：镜像常被截断，解析器需要能优雅退化。
pub fn read_full(dev: &dyn BlockDevice, offset: u64, buf: &mut [u8]) -> Result<usize> {
    let mut done = 0usize;
    while done < buf.len() {
        let pos = offset.saturating_add(done as u64);
        if pos >= dev.size() {
            break;
        }
        let n = match dev.read_at(pos, &mut buf[done..]) {
            Ok(0) => break,
            Ok(n) => n,
            Err(Error::Bounds { .. }) => break,
            Err(e) => return Err(e),
        };
        done += n;
    }
    if done < buf.len() {
        for b in &mut buf[done..] {
            *b = 0;
        }
    }
    Ok(done)
}

/// 镜像文件（.img / .raw / .dd / 已解包的 .dmg）。
pub struct ImageFile {
    file: File,
    len: u64,
    path: PathBuf,
    sector: u32,
}

impl ImageFile {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new().read(true).open(&path)?;
        let len = file.metadata()?.len();
        if len == 0 {
            return Err(Error::Invalid(format!(
                "{} 是空文件（0 字节），不是有效镜像",
                path.display()
            )));
        }
        Ok(Self { file, len, path, sector: 512 })
    }

    /// 尺寸未知（如稀疏文件/GUI 传入 fd）时允许显式指定。
    pub fn with_len(path: impl AsRef<Path>, len: u64) -> Result<Self> {
        let mut s = Self::open(path)?;
        if len > 0 && len <= s.len {
            s.len = len;
        }
        Ok(s)
    }

    /// 4K 扇区镜像可以手动覆盖，分区表探测会参考它。
    pub fn set_sector_size(&mut self, s: u32) {
        if s.is_power_of_two() && s >= 512 {
            self.sector = s;
        }
    }
}

impl BlockDevice for ImageFile {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        if offset >= self.len {
            return Ok(0);
        }
        let avail = (self.len - offset) as usize;
        let want = buf.len().min(avail);
        platform_read_at(&self.file, offset, &mut buf[..want])
    }

    fn size(&self) -> u64 {
        self.len
    }

    fn sector_size(&self) -> u32 {
        self.sector
    }

    fn label(&self) -> String {
        self.path.display().to_string()
    }
}

#[cfg(unix)]
fn platform_read_at(file: &File, offset: u64, buf: &mut [u8]) -> Result<usize> {
    use std::os::unix::fs::FileExt;
    Ok(file.read_at(buf, offset)?)
}

#[cfg(windows)]
fn platform_read_at(file: &File, offset: u64, buf: &mut [u8]) -> Result<usize> {
    use std::os::windows::fs::FileExt;
    Ok(file.seek_read(buf, offset)?)
}

/// 物理磁盘或分区设备节点。
pub struct RawDevice {
    file: File,
    len: u64,
    path: PathBuf,
    sector: u32,
}

impl RawDevice {
    /// 以只读方式打开设备节点。
    ///
    /// `size` 为 0 时尝试自动探测（`lseek(SEEK_END)` / `diskutil`）。
    pub fn open(path: impl AsRef<Path>, size: u64) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new().read(true).open(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                Error::Permission(format!(
                    "无法只读打开 {}；macOS 需要 sudo（例如 `sudo dmde-rs devices`）",
                    path.display()
                ))
            } else {
                Error::Io(e)
            }
        })?;
        let mut len = size;
        if len == 0 {
            len = probe_size(&path).unwrap_or(0);
        }
        if len == 0 {
            return Err(Error::Invalid(format!(
                "无法确定 {} 的容量，请用 --size 指定",
                path.display()
            )));
        }
        let sector = probe_sector_size(&file, &path).unwrap_or(512);
        // 直读设备时关掉页缓存：坏道读失败会被系统缓存住，且大范围扫描会把内存吃满。
        #[cfg(target_os = "macos")]
        {
            use std::os::unix::io::AsRawFd;
            sys::set_nocache(file.as_raw_fd());
        }
        Ok(Self { file, len, path, sector })
    }
}

impl BlockDevice for RawDevice {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        if offset >= self.len {
            return Ok(0);
        }
        let avail = (self.len - offset) as usize;
        let want = buf.len().min(avail);
        platform_read_at(&self.file, offset, &mut buf[..want])
    }

    fn size(&self) -> u64 {
        self.len
    }

    fn sector_size(&self) -> u32 {
        self.sector
    }

    fn label(&self) -> String {
        self.path.display().to_string()
    }
}

/// 设备节点容量探测。
fn probe_size(path: &Path) -> Option<u64> {
    // 方案一：对字符/块设备的 seek(End) 通常返回容量。
    if let Ok(mut f) = OpenOptions::new().read(true).open(path) {
        use std::io::{Seek, SeekFrom};
        if let Ok(n) = f.seek(SeekFrom::End(0)) {
            if n > 0 {
                return Some(n);
            }
        }
    }
    // 方案二：macOS 走 diskutil 的 plist（需要 BSD 名，例如 disk4）。
    #[cfg(target_os = "macos")]
    {
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| s.trim_start_matches("r"))
            .unwrap_or_default()
            .to_string();
        if !name.is_empty() {
            if let Ok(out) = std::process::Command::new("diskutil")
                .args(["info", "-plist", &name])
                .output()
            {
                if let Ok(text) = String::from_utf8(out.stdout) {
                    if let Ok(v) = crate::plist::parse(&text) {
                        if let Some(n) = v.get("Size").and_then(|x| x.as_u64()) {
                            return Some(n);
                        }
                    }
                }
            }
        }
    }
    None
}

#[cfg(target_os = "macos")]
fn probe_sector_size(file: &File, _path: &Path) -> Option<u32> {
    use std::os::unix::io::AsRawFd;
    let fd = file.as_raw_fd();
    // DKIOCGETBLOCKSIZE = _IOR('d', 24, uint32_t) = 0x40046418
    let mut bs: u32 = 0;
    let rc = unsafe { sys::ioctl(fd, 0x4004_6418, &mut bs as *mut u32) };
    if rc == 0 && (bs == 512 || bs == 1024 || bs == 4096) {
        return Some(bs);
    }
    // 退而求其次：物理块大小 DKIOCGETPHYSICALBLOCKSIZE = 0x40046419
    let mut pbs: u32 = 0;
    let rc = unsafe { sys::ioctl(fd, 0x4004_6419, &mut pbs as *mut u32) };
    if rc == 0 && pbs > 0 {
        return Some(pbs);
    }
    None
}

#[cfg(not(target_os = "macos"))]
fn probe_sector_size(_file: &File, _path: &Path) -> Option<u32> {
    None
}

#[cfg(target_os = "macos")]
pub(crate) mod sys {
    use std::os::unix::io::RawFd;

    pub type CInt = i32;
    pub type CULong = u64;

    extern "C" {
        pub fn ioctl(fd: RawFd, request: CULong, ...) -> CInt;
        pub fn fcntl(fd: RawFd, cmd: CInt, ...) -> CInt;
    }

    /// 让设备读取绕过系统缓存。读坏盘时必须开，否则会缓存住"读失败"的块，
    /// 也会把几十 G 的页缓存塞满。
    pub const F_NOCACHE: CInt = 48;

    /// 打开后立即设置 F_NOCACHE。
    pub fn set_nocache(fd: RawFd) {
        unsafe {
            fcntl(fd, F_NOCACHE, 1);
        }
    }
}
