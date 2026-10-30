//! 统一后端：同一套调用在"本地进程读镜像"和"特权 helper 读设备"之间无感切换。

use crate::client::Client;
use crate::model::{EntryDto, NxsbReportDto, PreviewDto, SourceDto};
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

    /// 取条目信息（文件 or 目录）。
    pub fn stat(&mut self, path: &str) -> Result<Option<EntryDto>, String> {
        match self {
            Backend::Local(s) => s.stat(path),
            Backend::Remote(c) => c.stat(path),
        }
    }

    /// 按路径读文件内容（已删除项没有可信路径时请用 `read_id`）。
    pub fn read_path(&mut self, path: &str, out: &mut dyn Write) -> Result<u64, String> {
        match self {
            Backend::Local(s) => s.read_path(path, out),
            Backend::Remote(c) => c.read_path(path, out),
        }
    }

    /// 列出容器里所有可用的 NXSB 副本（容器头修复前的清单）。
    pub fn scan_nxsb(
        &mut self,
        source: &str,
        container_offset: u64,
        block_size: u32,
    ) -> Result<NxsbReportDto, String> {
        match self {
            // 本地模式（镜像文件 / 已有权限的设备）直接扫
            Backend::Local(_) => crate::repair::scan_nxsb(source, container_offset, block_size),
            Backend::Remote(c) => c.scan_nxsb(source, container_offset, block_size),
        }
    }

    /// 修复 APFS 容器头：把选中的 NXSB 副本写回块 0。
    ///
    /// 这是全项目唯一会写设备的地方，参数已经尽量收窄（只给源块号，
    /// 写什么由内核层校验后自己决定）。
    pub fn repair_nxsb(
        &mut self,
        source: &str,
        container_offset: u64,
        block_size: u32,
        src_paddr: u64,
        backup_path: &str,
    ) -> Result<(u64, u64, String), String> {
        match self {
            Backend::Local(_) => crate::repair::repair_nxsb(
                source,
                container_offset,
                block_size,
                src_paddr,
                backup_path,
            )
            .map(|o| (o.bytes, o.xid, o.backup_path)),
            Backend::Remote(c) => {
                c.repair_nxsb(source, container_offset, block_size, src_paddr, backup_path)
            }
        }
    }
}

/// 先在本地打开；只有"权限不足"才值得升级到提权路径。
pub fn open_local(source: &str, partition: Option<usize>) -> Result<(Session, SourceDto), OpenError> {
    crate::session::open(source, partition)
}
