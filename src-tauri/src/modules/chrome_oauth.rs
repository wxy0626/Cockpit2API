//! 使用本机 Google Chrome 打开 OAuth 授权页。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use url::Url;

#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;

#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 兼容旧调用名：现在所有 OAuth 都使用可信 Chrome 配置。
pub fn open_incognito(url: &str) -> Result<(), String> {
    open_trusted(url)
}

/// 使用 Chrome 当前用户配置打开授权地址，保留 Google 的可信设备状态。
pub fn open_trusted(url: &str) -> Result<(), String> {
    let parsed = parse_http_url(url)?;
    let chrome = find_chrome().ok_or_else(|| {
        "未找到 Google Chrome，无法打开可信授权窗口。请安装 Chrome 后重试，或手动复制授权链接。"
            .to_string()
    })?;

    let mut command = Command::new(&chrome);
    command
        .args([
            "--new-window",
            "--no-first-run",
            "--no-default-browser-check",
        ])
        .arg(parsed.as_str())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(target_os = "windows")]
    command.creation_flags(CREATE_NO_WINDOW);

    command
        .spawn()
        .map_err(|error| format!("启动 Google Chrome 可信授权窗口失败: {}", error))?;

    crate::modules::logger::log_info(&format!(
        "[OAuth] 已使用 Chrome 可信用户配置打开授权页: {}",
        parsed
    ));
    Ok(())
}

/// 所有平台 OAuth 都使用同一份可信 Chrome 配置。
pub fn open_oauth_url(url: &str) -> Result<(), String> {
    open_trusted(url)
}

fn parse_http_url(url: &str) -> Result<Url, String> {
    let parsed = Url::parse(url.trim()).map_err(|error| format!("授权地址无效: {}", error))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(format!(
            "授权地址协议不受支持，只允许 http/https: {}",
            parsed.scheme()
        ));
    }
    Ok(parsed)
}

fn find_chrome() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        let mut candidates = Vec::new();
        for variable in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
            if let Some(root) = std::env::var_os(variable) {
                candidates.push(
                    PathBuf::from(root)
                        .join("Google")
                        .join("Chrome")
                        .join("Application")
                        .join("chrome.exe"),
                );
            }
        }
        if let Some(path) = existing_path(candidates) {
            return Some(path);
        }
        return find_from_path("where.exe", "chrome.exe");
    }

    #[cfg(target_os = "macos")]
    {
        let mut candidates = vec![PathBuf::from(
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        )];
        if let Some(home) = dirs::home_dir() {
            candidates.push(
                home.join("Applications")
                    .join("Google Chrome.app")
                    .join("Contents")
                    .join("MacOS")
                    .join("Google Chrome"),
            );
        }
        return existing_path(candidates);
    }

    #[cfg(target_os = "linux")]
    {
        for binary in ["google-chrome", "google-chrome-stable"] {
            if let Some(path) = find_from_path("which", binary) {
                return Some(path);
            }
        }
        return None;
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

fn existing_path(candidates: Vec<PathBuf>) -> Option<PathBuf> {
    candidates.into_iter().find(|path| path.is_file())
}

fn find_from_path(checker: &str, binary: &str) -> Option<PathBuf> {
    let output = Command::new(checker)
        .arg(binary)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .map(Path::new)
        .find(|path| path.is_file())
        .map(PathBuf::from)
}
