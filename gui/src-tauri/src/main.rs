// macOS 下不要弹出控制台窗口
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    dmde_gui_lib::run()
}
