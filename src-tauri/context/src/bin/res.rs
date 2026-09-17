// 资源占位 bin：tauri-build 的 rustc-link-arg-bins 要求包内有 bin 才合法。
// 注意它会连带重编本包所有 bin —— 别在这里加任何逻辑。
fn main() {}
