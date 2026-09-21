use tauri::{AppHandle, Manager};

/// QoderWork 旧内置窗口的 label，兼容升级前残留窗口。
const OAUTH_WINDOW_LABEL: &str = "qoderwork-oauth-window";
/// OAuth 授权页只允许 qoder.com 发起，避免前端传入任意地址开窗。
const ALLOWED_OAUTH_HOST: &str = "qoder.com";

/// 使用 Chrome 可信窗口打开 QoderWork 授权页。
pub async fn open_oauth_window(app: &AppHandle, auth_url: &str) -> Result<(), String> {
    let parsed = url::Url::parse(auth_url.trim())
        .map_err(|error| format!("QoderWork OAuth 授权地址无效: {}", error))?;
    if parsed.scheme() != "https" {
        return Err(format!(
            "QoderWork OAuth 授权地址协议不受支持: {}",
            parsed.scheme()
        ));
    }
    if parsed.host_str() != Some(ALLOWED_OAUTH_HOST) {
        return Err("QoderWork OAuth 授权地址不是官方 qoder.com 域名".to_string());
    }

    // 兼容升级前残留的内置窗口，避免旧窗口继续遮挡新的 Chrome 窗口。
    if let Some(window) = app.get_webview_window(OAUTH_WINDOW_LABEL) {
        let _ = window.destroy();
    }

    crate::modules::chrome_oauth::open_trusted(parsed.as_str())
}

/// 关闭升级前的 QoderWork 内置授权窗口。
pub fn close_oauth_window(app: &AppHandle) -> Result<(), String> {
    let app_for_thread = app.clone();
    app.run_on_main_thread(move || {
        if let Some(window) = app_for_thread.get_webview_window(OAUTH_WINDOW_LABEL) {
            if let Err(error) = window.destroy() {
                crate::modules::logger::log_warn(&format!(
                    "[QoderWork OAuth] 关闭旧授权窗口失败: {}",
                    error
                ));
            }
        }
    })
    .map_err(|error| format!("调度主线程关闭旧授权窗口失败: {}", error))
}
