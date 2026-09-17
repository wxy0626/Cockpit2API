// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // Context 由本 crate 的 lib 生成（前端资源内嵌在这里）。
    // bin 传参给 lib::run() —— 主 lib 不依赖本 crate，前端 dist 变化只重编本 crate + 链接。
    antigravity_cockpit_tools_lib::run(cockpit_app_context::build_context())
}
