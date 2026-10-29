//! 统一后端：同一套调用在"本地进程读镜像"和"特权 helper 读设备"之间无感切换。

use crate::client::Client;
use crate::model::{EntryDto, PreviewDto, SourceDto};
use crate::session::{OpenError, Session};
use std::io::Write;

pub enum Backend {
    /// 当前进程直接读（镜像文件、或有权限的设备）
    Local(Session),
    /// 特权 helper（/dev/rdisk*）
    Remote(Client),
}

impl Backend {
    pub fn kind(&self) -> &'static str {
        match self {
            Backend::Local(_) => "local",
            Backend::Remote(_) => "admin",
        }
    }

    pub fn desc(&self) -> String {
        match self {
            Backend::Local(s) => s.desc.clone(),
            Backend::Remote(_) => String::new(),
        }
    }

    pub fn list_dir(&mut self, path: &str, show_deleted: bool) -> Result<Vec<EntryDto>, String> {
        match self {
            Backend::Local(s) => s.list_dir(path, show_deleted),
            Backend::Remote(c) => c.list_dir(path, show_deleted),
        }
    }

    pub fn preview(&mut self, path: &str, max: usize) -> Result<PreviewDto, String> {
        match self {
            Backend::Local(s) => s.preview(path, max),
            Backend::Remote(c) => c.preview(path, max),
        }
    }

    pub fn preview_id(&mut self, id: u64, max: usize) -> Result<PreviewDto, String> {
        match self {
            Backend::Local(s) => s.preview_by_id(id, max),
            Backend::Remote(c) => c.preview_id(id, max),
        }
    }

    pub fn deleted(&mut self, limit: usize) -> Result<Vec<EntryDto>, String> {
        match self {
            Backend::Local(s) => s.deleted(limit),
            Backend::Remote(c) => c.deleted(limit),
        }
    }

    pub fn read_id(&mut self, id: u64, out: &mut dyn Write) -> Result<u64, String> {
        match self {
            Backend::Local(s) => s.read_id(id, out),
            Backend::Remote(c) => c.read_id(id, out),
        }
    }
}

/// 先在本地打开；只有"权限不足"才值得升级到提权路径。
pub fn open_local(source: &str, partition: Option<usize>) -> Result<(Session, SourceDto), OpenError> {
    crate::session::open(source, partition)
}
