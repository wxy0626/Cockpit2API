// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // 标识符迁移必须早于 Tauri 创建任何应用目录（WebView2 等按标识符存储）。
    cockpit2api_lib::migrate_legacy_app_dirs();
    // Context 由本 crate 的 lib 生成（前端资源内嵌在这里）。
    // bin 传参给 lib::run() —— 主 lib 不依赖本 crate，前端 dist 变化只重编本 crate + 链接。
    cockpit2api_lib::run(cockpit_app_context::build_context())
}
