//! Tauri 入口：状态管理与命令注册。
//!
//! 命令本体放在 `commands` 子模块里——除了结构更清晰，还绕开了
//! `#[tauri::command]` 在 crate 根模块展开时与 `#[macro_export]` 的命名冲突
//! （E0255: `__cmd__xxx` defined multiple times）。
//!
//! `priv_helper` 管提权：macOS 读 `/dev/rdisk*` 必须 root，但 GUI 不该整体以
//! root 跑（WebView + TCC 都会出问题），所以只把"读"交给一个只读 helper。

pub mod commands;
mod priv_helper;

use dmrpc::Backend;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

#[derive(Default)]
pub struct AppState {
    /// 当前会话：本地进程读 or 特权 helper 读
    pub backend: Mutex<Option<Backend>>,
    /// 导出暂停标志。
    ///
    /// 单独放在这里、而不是塞进 Backend：导出期间 backend 已被 `take()` 走，
    /// 暂停命令若去抢 `backend` 那把锁，就会一直等到导出结束——
    /// 表现正是"点了暂停没反应"。
    pub export_cancel: Arc<AtomicBool>,
    /// 正在进行的长时间任务（扫描/导出）。
    ///
    /// 这类任务会把 `backend` 整个取走独占，此时任何列目录都会失败；
    /// 有了这个标记，界面能显示「正在扫描，请稍候」而不是误导人的「还没有打开任何卷」。
    pub busy: Mutex<Option<String>>,
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            commands::boot_path,
            commands::ui_log,
            commands::list_disks,
            commands::open_source,
            commands::list_dir,
            commands::preview,
            commands::save_file,
            commands::scan_deleted,
            commands::recover_all,
            commands::export_path,
            commands::cancel_export,
            commands::default_out_dir,
            commands::admin_state,
            commands::request_admin,
            commands::drop_admin,
            commands::current_source
        ])
        .run(tauri::generate_context!())
        .expect("启动 Tauri 应用失败");
}
