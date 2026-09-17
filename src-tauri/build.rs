fn main() {
    let target = std::env::var("TARGET").unwrap();
    println!("cargo:rustc-env=COCKPIT_RUST_TARGET={target}");
    println!("cargo:rerun-if-env-changed=TARGET");
    println!("cargo:rerun-if-changed=build.rs");
    // 原由 tauri_build::build() 注入的 cfg 别名。本 crate 不再调用 tauri_build
    // （它会把前端 dist 纳入监视，导致每次 dist 变化都重编 28 万行主 lib），
    // 这里手动补齐 lib 代码用到的别名（tauri-build 只注入 desktop = !mobile）。
    println!("cargo:rustc-check-cfg=cfg(desktop)");
    println!("cargo:rustc-check-cfg=cfg(mobile)");
    println!("cargo:rustc-cfg=desktop");
}
