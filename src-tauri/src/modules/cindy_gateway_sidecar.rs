//! Cindy 反代网关 sidecar（cindy2api）托管。
//!
//! 与 `sidecars/wb2api` 的差别：wb2api 由外部手动启动，**本模块让 cindy2api
//! 随 CockpitTools 启动自动可用**，用户不必手动开进程。
//!
//! 二进制定位范式照抄 `codex_local_access_sidecar_config`：
//!   - 开发期（debug）：`<repo>/sidecars/cindy2api/bin/`
//!   - 打包后（release）：主程序同级目录、以及 macOS 的 `Contents/Resources`
//!
//! 设计取舍：只做「定位 + 拉起 + 不重复拉起」，不做端口清理与崩溃重启状态机。
//! 网关端口被占用时 sidecar 自身会退出并留下日志，比复杂的自愈逻辑更可控。

use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::modules::logger::{log_info, log_warn};

/// sidecar 可执行文件名（不带扩展名）
const CINDY_SIDECAR_BIN_NAME: &str = "cindy2api";

/// 网关默认监听端口。与 `sidecars/cindy2api/runtime/config.json` 的 `listen` 一致，
/// 仅用于「已经在跑就不再拉起」的探测；端口若被改过，探测不到会重复拉起一次，
/// 由 sidecar 自己因端口占用退出，不影响可用性。
const CINDY_GATEWAY_PORT: u16 = 7865;

/// 幂等标记（与其他 sidecar 模块保持同一防御风格）
static CINDY_SIDECAR_STARTED: AtomicBool = AtomicBool::new(false);

/// 往候选表里加入某个目录下的几种可能文件名。
///
/// 打包产物按 Tauri sidecar 规范会带 target triple 后缀，开发产物则是裸名，
/// 两种都列出来，取第一个存在的。
fn push_candidates(candidates: &mut Vec<PathBuf>, dir: &Path) {
    for name in [
        format!("{CINDY_SIDECAR_BIN_NAME}.exe"),
        format!("{CINDY_SIDECAR_BIN_NAME}-x86_64-pc-windows-msvc.exe"),
        format!("{CINDY_SIDECAR_BIN_NAME}-x86_64-pc-windows-gnullvm.exe"),
        CINDY_SIDECAR_BIN_NAME.to_string(),
        format!("{CINDY_SIDECAR_BIN_NAME}-aarch64-apple-darwin"),
    ] {
        let path = dir.join(name);
        if !candidates.contains(&path) {
            candidates.push(path);
        }
    }
}

/// 按优先级列出 sidecar 的候选路径。
fn sidecar_binary_candidates() -> Result<Vec<PathBuf>, String> {
    let exe = std::env::current_exe().map_err(|e| format!("读取当前程序路径失败: {e}"))?;
    let parent = exe
        .parent()
        .ok_or_else(|| format!("当前程序路径缺少父目录: {}", exe.display()))?;

    let mut candidates = Vec::new();
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let dev_sidecar_dir = manifest_dir.join("../sidecars/cindy2api/bin");

    if cfg!(debug_assertions) {
        push_candidates(&mut candidates, &dev_sidecar_dir);
    }
    push_candidates(&mut candidates, parent);
    if let Some(contents_dir) = parent.parent() {
        push_candidates(&mut candidates, &contents_dir.join("Resources"));
    }
    if !cfg!(debug_assertions) {
        push_candidates(&mut candidates, &dev_sidecar_dir);
    }
    Ok(candidates)
}

/// 端口是否已在监听 —— 是则说明已有实例在跑，无需重复拉起。
fn gateway_already_listening() -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], CINDY_GATEWAY_PORT));
    TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok()
}

/// 拉起 sidecar 进程（Windows 下隐藏控制台窗口，日志落盘）。
fn spawn_sidecar(path: &Path) {
    let mut command = Command::new(path);
    command
        .arg("--parent-pid")
        .arg(std::process::id().to_string())
        .stdin(Stdio::null());

    // 把 sidecar 日志落到与主程序同一日志目录，便于用户自查
    let mut log_attached = false;
    if let Ok(log_dir) = crate::modules::logger::get_log_dir() {
        let _ = std::fs::create_dir_all(&log_dir);
        let log_path = log_dir.join("cindy2api.log");
        if let Ok(file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)
        {
            if let Ok(cloned) = file.try_clone() {
                command.stdout(Stdio::from(file));
                command.stderr(Stdio::from(cloned));
                log_attached = true;
            }
        }
    }
    if !log_attached {
        command.stdout(Stdio::null()).stderr(Stdio::null());
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    match command.spawn() {
        Ok(child) => log_info(&format!(
            "Cindy 反代网关已启动（pid {}）：http://127.0.0.1:{CINDY_GATEWAY_PORT}/v1",
            child.id()
        )),
        Err(err) => log_warn(&format!("Cindy 反代网关启动失败：{err}")),
    }
}

/// 拉起 Cindy 反代网关 sidecar；幂等，重复调用直接返回。
///
/// 进程创建可能被安全软件拖慢，因此放到后台线程，不阻塞应用启动流程。
pub fn ensure_started() {
    if CINDY_SIDECAR_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(|| {
        if gateway_already_listening() {
            log_info("Cindy 反代网关已在运行，跳过拉起");
            return;
        }
        let candidates = match sidecar_binary_candidates() {
            Ok(value) => value,
            Err(err) => {
                log_warn(&format!("Cindy 反代网关未启动：{err}"));
                return;
            }
        };
        match candidates.iter().find(|path| path.exists()) {
            Some(path) => spawn_sidecar(path),
            None => log_warn(&format!(
                "Cindy 反代网关未启动：未找到 {CINDY_SIDECAR_BIN_NAME} 可执行文件，\
                 请先执行 sidecars/cindy2api/build-sidecar.ps1。已检查：{}",
                candidates
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join("、")
            )),
        }
    });
}
