//! Tauri 入口：状态管理与命令注册。
//!
//! 命令本体放在 `commands` 子模块里——除了结构更清晰，还绕开了
//! `#[tauri::command]` 在 crate 根模块展开时与 `#[macro_export]` 的命名冲突
//! （E0255: `__cmd__xxx` defined multiple times）。

mod commands;

use dmfs::AnyFs;
use std::sync::Mutex;

pub struct Session {
    pub source: String,
    pub desc: String,
    pub fs: AnyFs,
}

#[derive(Default)]
pub struct AppState {
    pub session: Mutex<Option<Session>>,
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            commands::list_disks,
            commands::open_source,
            commands::list_dir,
            commands::preview,
            commands::save_file,
            commands::scan_deleted,
            commands::recover_all,
            commands::default_out_dir,
            commands::current_source
        ])
        .run(tauri::generate_context!())
        .expect("启动 Tauri 应用失败");
}
