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
