// 前端资源在 generate_context! 宏展开时内嵌进本 crate。
// 把宏隔离在这里的目的：dist 变化只重编本 crate（几十行）+ 链接，
// 不再触发 28 万行的主 crate 重编译。
pub fn build_context() -> tauri::Context<tauri::Wry> {
    tauri::generate_context!()
}
