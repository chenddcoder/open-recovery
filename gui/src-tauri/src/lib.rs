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
use std::sync::Mutex;

#[derive(Default)]
pub struct AppState {
    /// 当前会话：本地进程读 or 特权 helper 读
    pub backend: Mutex<Option<Backend>>,
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            commands::open_source,
            commands::list_dir,
            commands::preview,
            commands::save_file,
            commands::scan_deleted,
            commands::recover_all,
            commands::export_path,
            commands::default_out_dir,
            commands::admin_state,
            commands::request_admin,
            commands::drop_admin,
            commands::current_source
        ])
        .run(tauri::generate_context!())
        .expect("启动 Tauri 应用失败");
}
