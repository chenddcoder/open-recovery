//! 传给前端的 DTO。
//!
//! 同时派生 `Deserialize`：GUI 在"本地模式"下由本进程生成，在"提权模式"下
//! 从 helper 的 JSON 响应反序列化，两条路径拿到的是同一个结构。

use dmdisk::{human_size, DiskInfo};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskDto {
    pub dev_node: String,
    pub bsd: String,
    pub size: u64,
    pub size_human: String,
    pub model: String,
    pub bus: String,
    pub internal: bool,
    pub removable: bool,
}

impl From<&DiskInfo> for DiskDto {
    fn from(d: &DiskInfo) -> Self {
        Self {
            dev_node: d.dev_node.clone(),
            bsd: d.bsd.clone(),
            size: d.size,
            size_human: human_size(d.size),
            model: d.model.clone(),
            bus: d.bus.clone(),
            internal: d.internal,
            removable: d.removable,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartDto {
    pub index: usize,
    pub type_name: String,
    pub size: u64,
    pub size_human: String,
    pub offset: u64,
    pub fs_kind: String,
    pub mountable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceDto {
    pub desc: String,
    pub scheme: String,
    pub verified: bool,
    pub notes: Vec<String>,
    pub partitions: Vec<PartDto>,
    pub fs_kind: String,
    pub label: String,
    pub uuid: String,
    pub block_size: u32,
    pub total_bytes: u64,
    pub total_human: String,
    pub free_bytes: Option<u64>,
    pub details: Vec<(String, String)>,
    /// 当前文件系统在设备内的字节偏移（整盘/镜像 = 0，分区 = 分区起始）。
    ///
    /// 只给「修复容器头」用：它要把"容器块号"换算成设备偏移。自动选分区时
    /// 前端并不知道最终选的是哪一块，端侧推不出来，所以必须由后端权威给出。
    pub container_offset: u64,
    /// 实际打开的路径（`/dev/rdisk4` 或镜像文件），修复写回时要用。
    pub device: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntryDto {
    pub name: String,
    pub path: String,
    pub id: u64,
    pub is_dir: bool,
    pub size: u64,
    pub size_human: String,
    pub deleted: bool,
    pub mtime: Option<i64>,
    pub name_lost: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreviewDto {
    pub text: String,
    pub mode: String,
    pub size: u64,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoverDto {
    pub ok: u64,
    pub failed: u64,
    pub bytes: u64,
    pub bytes_human: String,
    pub out_dir: String,
    pub saved: Vec<String>,
}

/// 一份可用的 NXSB 副本（容器头修复用）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NxsbCandidateDto {
    /// 容器内的物理块号
    pub paddr: u64,
    pub xid: u64,
    /// `nx_fs_oid[]` 里的卷数
    pub volume_count: usize,
    pub uuid: String,
}

/// 容器头扫描结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NxsbReportDto {
    /// 块 0 现在是否已经是有效 NXSB（是的话不需要修）
    pub block0_ok: bool,
    pub block0_xid: Option<u64>,
    pub block_size: u32,
    /// 按 xid 降序
    pub candidates: Vec<NxsbCandidateDto>,
    /// 校验失败的 NXSB 数量（坏盘信号）
    pub cksum_bad: u32,
    pub scanned_blocks: u64,
}
