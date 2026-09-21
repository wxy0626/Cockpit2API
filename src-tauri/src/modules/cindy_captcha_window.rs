//! Cindy 邮箱验证码的「人机验证窗口」。
//!
//! 为什么必须借官方验证页：
//! 上游 siteverify 会校验 token 的签发环境（页面主机名 / action / cData）。
//! 我们在应用内自渲染（origin = tauri.localhost 或 localhost:7865，缺 action/cData）
//! 拿到的 token 一律被判 CAPTCHA_INVALID —— 实测确认。而官方客户端用的是
//! auth 服务自带的页面：
//!
//!   getLoginCaptchaChallengeUrl() = authServerUrl(activeAuthRealm) + "/captcha/turnstile"
//!
//! 该页把 token 通过三条通道回传（见页面实现）：ReactNativeWebView 桥 /
//! 同源父页 postMessage / 顶层窗口的 URL hash。官方桌面端就是开一个独立窗口
//! 加载它并读 hash。
//!
//! 我们的做法与官方同构：
//!   1. 用独立窗口加载官方验证页（https://auth.cindy.app/captcha/turnstile）；
//!   2. 注入 `window.ReactNativeWebView` 桥 —— 页面拿到 token 后会优先走这条通道；
//!   3. 桥把 token 转发到 sidecar 的 /captcha/callback（localhost，无跨域问题），
//!      由 sidecar 完成「带 token 发验证码」并记录会话状态；
//!   4. 随后自动关掉这个窗口，主窗口轮询会话状态展示结果。
//!
//! 结果不直接回给前端的原因：token 是一次性的，交给 sidecar 立刻用掉最稳妥，
//! 也避免 token 在前端 JS 里多绕一圈。
//!
//! ── 本模块同时承载 Cindy 的**授权窗口**（见文件末尾）──
//! 与验证码窗口同族：都是一个独立的 WebView2 窗口 + on_navigation 拦收尾。

use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

use crate::modules::logger;

/// 官方验证页（与官方客户端 getLoginCaptchaChallengeUrl 等价）
const CINDY_CAPTCHA_URL: &str = "https://auth.cindy.app/captcha/turnstile";
/// 验证窗口的 label（重复发起时先销毁旧的）
const CINDY_CAPTCHA_WINDOW_LABEL: &str = "cindy-captcha";
/// sidecar 的收尾地址（localhost 来源，页面跨域跳转过来即可）
const CALLBACK_ORIGIN: &str = "http://localhost:7865";
const CALLBACK_PATH: &str = "/captcha/callback";

/// 注入脚本模板：拦截官方页的 ReactNativeWebView 桥，把 token 转交 sidecar。
///
/// 占位符 __SESSION__ 会被替换成 JSON 字符串（含引号），避免拼接注入问题。
const BRIDGE_SCRIPT: &str = r#"
(function () {
  window.ReactNativeWebView = {
    postMessage: function (raw) {
      try {
        var payload = JSON.parse(raw);
        var base = '__ORIGIN__' + '__PATH__' + '?session=' + __SESSION__;
        var target = payload && payload.ok
          ? base + '&token=' + encodeURIComponent(payload.token)
          : base + '&error=' + encodeURIComponent(String((payload && payload.code) || 'failed'));
        window.location.href = target;
      } catch (error) {
        window.location.href = '__ORIGIN__' + '__PATH__' + '?session=' + __SESSION__ + '&error=bridge_error';
      }
    }
  };
})();
"#;

fn is_captcha_callback(url: &tauri::Url) -> bool {
    url.scheme() == "http"
        && url.host_str() == Some("localhost")
        && url.port() == Some(7865)
        && url.path() == CALLBACK_PATH
}

/// 打开官方验证页窗口；结果经 sidecar 会话回传，前端轮询取用。
#[tauri::command]
pub async fn cindy_captcha_window_open(app: AppHandle, session: String) -> Result<(), String> {
    if let Some(window) = app.get_webview_window(CINDY_CAPTCHA_WINDOW_LABEL) {
        let _ = window.destroy();
    }

    let parsed = tauri::Url::parse(CINDY_CAPTCHA_URL)
        .map_err(|error| format!("Cindy 验证页地址无效: {error}"))?;

    // session 由前端生成（uuid），这里用 JSON 序列化保证注入安全
    let session_literal = serde_json::to_string(&session).map_err(|error| error.to_string())?;
    let script = BRIDGE_SCRIPT
        .replace("__ORIGIN__", CALLBACK_ORIGIN)
        .replace("__PATH__", CALLBACK_PATH)
        .replace("__SESSION__", &session_literal);

    let app_for_nav = app.clone();
    WebviewWindowBuilder::new(
        &app,
        CINDY_CAPTCHA_WINDOW_LABEL,
        WebviewUrl::External(parsed),
    )
    .title("Cindy 安全校验")
    .inner_size(460.0, 380.0)
    .min_inner_size(380.0, 320.0)
    .center()
    .resizable(false)
    .initialization_script(&script)
    .on_navigation(move |url| {
        if is_captcha_callback(url) {
            // 交给 sidecar 收尾（它会用 token 发验证码）。稍后自动关窗，
            // 让用户看到「已完成」而不是窗口突然消失。
            let app = app_for_nav.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(1500));
                if let Some(window) = app.get_webview_window(CINDY_CAPTCHA_WINDOW_LABEL) {
                    let _ = window.destroy();
                }
            });
            return true;
        }
        // 只允许 HTTPS 与本机回调，避免验证窗口被带到别处
        matches!(url.scheme(), "https" | "about")
    })
    .build()
    .map_err(|error| format!("创建 Cindy 验证窗口失败: {error}"))?;

    Ok(())
}

// ── 授权窗口 ────────────────────────────────────────────────────────────────

/// 授权窗口的 label（重复发起时先销毁旧的）
const CINDY_OAUTH_WINDOW_LABEL: &str = "cindy-oauth";
/// 允许打开授权窗口的授权域，与 sidecar 的 `AuthBaseGlobal` / `AuthBaseCN` 对齐。
/// 只放行这两个域，避免把窗口带到任意站点。
const CINDY_AUTH_HOSTS: [&str; 2] = ["auth.cindy.app", "auth.cindy.com.cn"];

fn is_cindy_auth_url(url: &tauri::Url) -> bool {
    url.scheme() == "https"
        && url
            .host_str()
            .is_some_and(|host| CINDY_AUTH_HOSTS.iter().any(|allowed| *allowed == host))
}

/// 打开 Cindy 授权窗口，统一使用可信配置。
///
/// 统一使用可信 Chrome 配置，使 Google/Apple 等登录能复用本机信任状态。
///
/// 授权是「sidecar 轮询」模型：不需要拦截回调取 code —— 上游 redirect 到
/// `/api/auth/desktop/callback` 后，sidecar 自己的轮询就会把授权码取走。
/// 这里拦这个跳转只为了**收尾关窗**（等用户看一眼再关，别让窗口突然消失）。
///
/// 实现风格刻意与上面的验证码窗口一致（async 命令 + 直接 build）：本模块的窗口
/// 没有自定义 `data_directory`，不涉及 WebView2 环境创建，无需调度主线程建窗。
#[tauri::command]
pub async fn cindy_oauth_window_open(app: AppHandle, authorize_url: String) -> Result<(), String> {
    let parsed = tauri::Url::parse(authorize_url.trim())
        .map_err(|error| format!("Cindy 授权地址无效: {error}"))?;
    if !is_cindy_auth_url(&parsed) {
        return Err(format!(
            "Cindy 授权地址不在允许的授权域内: {}",
            parsed.host_str().unwrap_or("<无主机名>")
        ));
    }

    // 兼容升级前残留的内置窗口，避免旧窗口继续遮挡新的 Chrome 窗口。
    if let Some(window) = app.get_webview_window(CINDY_OAUTH_WINDOW_LABEL) {
        let _ = window.destroy();
    }

    crate::modules::chrome_oauth::open_oauth_url(parsed.as_str())?;

    logger::log_info("[Cindy OAuth] 已打开 Chrome 可信授权窗口");
    Ok(())
}

/// 兼容旧版内置窗口；Chrome 授权窗口由用户关闭。
#[tauri::command]
pub fn cindy_oauth_window_close(app: AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window(CINDY_OAUTH_WINDOW_LABEL) {
        window
            .destroy()
            .map_err(|error| format!("关闭 Cindy 授权窗口失败: {error}"))?;
    }
    Ok(())
}
