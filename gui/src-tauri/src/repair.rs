//! APFS 容器头修复的 GUI 命令。
//!
//! 只暴露两个动作：先看清单、再选一份写回。刻意不做"自动修复"——
//! 这是整个项目里唯一的写盘路径，必须由用户看着清单点确认。
//!
//! 写之前一定把原块 0 备份到用户目录，命令返回里带上备份路径，
//! 用户任何时候都能用 `dd` 回滚。

use crate::AppState;
use dmrpc::NxsbReportDto;
use serde::Serialize;

/// 扫描结果 + 修复所需的定位信息（前端不用自己拼）。
#[derive(Serialize)]
pub struct RepairScanDto {
    pub block0_ok: bool,
    pub block0_xid: Option<u64>,
    pub block_size: u32,
    pub candidates: Vec<dmrpc::NxsbCandidateDto>,
    /// 校验失败的 NXSB 数量（坏盘信号，界面上要提示）
    pub cksum_bad: u32,
    pub scanned_blocks: u64,
    pub device: String,
    pub container_offset: u64,
}

/// 从当前会话里取「设备路径 + 容器偏移 + 块大小」。
///
/// 这三个值都由后端算出（自动选分区时前端不知道最终选了哪块），
/// 所以没打开源就没法修——这是刻意的，避免对着错误的偏移写盘。
fn source_ctx(state: &AppState) -> Result<(String, u64, u32), String> {
    let g = state
        .source_info
        .lock()
        .map_err(|_| "内部状态不可用".to_string())?;
    let dto = g.as_ref().ok_or_else(|| "还没有打开任何源".to_string())?;
    Ok((dto.device.clone(), dto.container_offset, dto.block_size))
}

fn backup_path() -> String {
    let dir = std::env::var("HOME")
        .map(|h| format!("{h}/Library/Application Support/dmde-rs"))
        .unwrap_or_else(|_| std::env::temp_dir().display().to_string());
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{dir}/block0-backup-{stamp}.bin")
}

/// 列出容器里所有可用的 NXSB 副本。
#[tauri::command]
pub fn apfs_repair_scan(
    state: tauri::State<'_, AppState>,
) -> Result<RepairScanDto, String> {
    let (device, offset, bs) = source_ctx(&state)?;
    let mut guard = state.backend.lock().map_err(|_| "内部状态不可用".to_string())?;
    let b = guard.as_mut().ok_or_else(|| "还没有打开任何源".to_string())?;
    let rep: NxsbReportDto = b.scan_nxsb(&device, offset, bs)?;
    Ok(RepairScanDto {
        block0_ok: rep.block0_ok,
        block0_xid: rep.block0_xid,
        block_size: if bs > 0 { bs } else { rep.block_size },
        candidates: rep.candidates,
        cksum_bad: rep.cksum_bad,
        scanned_blocks: rep.scanned_blocks,
        device,
        container_offset: offset,
    })
}

/// 把选中的那份 NXSB 副本写回容器块 0。
///
/// 只写一个块（4 KiB），别的地方一个字节都不碰。写完系统不会立刻认出变化，
/// 需要拔插硬盘或重新扫描，所以返回文案里写清楚了下一步。
#[tauri::command]
pub fn apfs_repair_apply(
    state: tauri::State<'_, AppState>,
    src_paddr: u64,
) -> Result<String, String> {
    let (device, offset, bs) = source_ctx(&state)?;
    let backup = backup_path();
    let mut guard = state.backend.lock().map_err(|_| "内部状态不可用".to_string())?;
    let b = guard.as_mut().ok_or_else(|| "还没有打开任何源".to_string())?;
    let (bytes, xid, path) = b.repair_nxsb(&device, offset, bs, src_paddr, &backup)?;
    Ok(format!(
        "已把块 {src_paddr} 的容器超级块（xid {xid}）写回块 0，共 {bytes} 字节。\n\
         原块 0 已备份到：{path}\n\
         下一步：拔插一次硬盘，或在终端执行 `diskutil unmountDisk force {device}` 让系统重新识别。"
    ))
}
