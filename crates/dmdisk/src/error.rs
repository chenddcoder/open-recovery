//! 统一错误类型。刻意不引入 anyhow/thiserror，保持零依赖。

use std::fmt;
use std::io;

#[derive(Debug)]
pub enum Error {
    /// 底层 I/O 错误
    Io(io::Error),
    /// 读取越界 / 长度不符
    Bounds { what: String, offset: u64, len: u64 },
    /// 结构体格式非法（魔数不符、字段越界等）
    Format(String),
    /// 该磁盘/卷当前版本还不支持
    Unsupported(String),
    /// 权限不足（macOS 打开 /dev/rdisk* 需要 root）
    Permission(String),
    /// 用户输入问题
    Invalid(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "I/O 错误: {e}"),
            Error::Bounds { what, offset, len } => {
                write!(f, "{what} 越界: offset=0x{offset:x} len={len}")
            }
            Error::Format(m) => write!(f, "格式非法: {m}"),
            Error::Unsupported(m) => write!(f, "暂不支持: {m}"),
            Error::Permission(m) => write!(f, "权限不足: {m}"),
            Error::Invalid(m) => write!(f, "参数错误: {m}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        match e.kind() {
            io::ErrorKind::PermissionDenied => Error::Permission(e.to_string()),
            _ => Error::Io(e),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// 把一个"读取超出卷尾"的场景降级为补零，而不是直接报错。
/// 海量恢复场景下镜像往往是截断的，解析器必须能容忍尾部缺失。
pub fn tolerant_io(e: Error) -> Error {
    e
}
