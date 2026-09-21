use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Manager};

use crate::models::workbuddy::{WorkbuddyOAuthCompletePayload, WorkbuddyOAuthStartResponse};
use crate::modules::logger;
use crate::modules::workbuddy_realm::{realm_by_id, WorkbuddyRealm, REALM_CN, REALM_INTL};

// ⚠️ 网关域名、接口前缀、platform 参数已统一迁移到 `workbuddy_realm.rs`（REALM_CN / REALM_INTL），
// 本文件不再硬编码任何域名。需要取域名时从传入的 `realm` 参数读取，以支持国际版。
// WorkBuddy 与 CodeBuddy 共用同一网关校验，缺少 User-Agent 会返回 403 / code=10085。
const WORKBUDDY_HTTP_USER_AGENT: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
const OAUTH_TIMEOUT_SECONDS: u64 = 600;
const OAUTH_POLL_INTERVAL_MS: u64 = 1500;
/// 企业版套餐代码（本地标识，用于前端识别企业用量资源）
const ENTERPRISE_PACKAGE_CODE: &str = "TCACA_code_enterprise";

#[derive(Clone)]
struct PendingOAuthState {
    login_id: String,
    /// 归属区域（"cn" / "intl"）。国际版与国内版可并发登录，必须各记各的。
    realm_id: String,
    expires_at: i64,
    state: String,
    cancelled: bool,
}

// 以 login_id 为键存放待处理登录：原先是 Option 单槽位，会被另一个区域的登录覆盖，
// 改为 HashMap 后两个区域可以同时授权，互不影响。
lazy_static::lazy_static! {
    static ref PENDING_OAUTH_STATE: Arc<Mutex<HashMap<String, PendingOAuthState>>> =
        Arc::new(Mutex::new(HashMap::new()));
}

/// 内置授权窗口 label：国内版 / 国际版各一个，两个区域可并发授权、互不覆盖。
const OAUTH_WINDOW_LABEL_CN: &str = "workbuddy-oauth-window-cn";
const OAUTH_WINDOW_LABEL_INTL: &str = "workbuddy-oauth-window-intl";

fn oauth_window_label_for(realm: &WorkbuddyRealm) -> &'static str {
    if realm.id == REALM_INTL.id {
        OAUTH_WINDOW_LABEL_INTL
    } else {
        OAUTH_WINDOW_LABEL_CN
    }
}

/// 从授权地址里取 state 参数：state 是我们自己向上游申请、且只存在于 pending 表里的随机串，
/// 用它反查待处理登录，等价于按登录会话校验地址合法性（前端无法伪造任意地址来开窗）。
fn authorize_state(url: &url::Url) -> Option<String> {
    url.query_pairs()
        .find(|(key, _)| key == "state")
        .map(|(_, value)| value.into_owned())
}

/// 打开 WorkBuddy 内置授权窗口。
///
/// WorkBuddy 的授权是「轮询 state」模型：后端 `auth/token?state=` 轮询拿 token，
/// 所以窗口里不需要拦截回调，用户在上游页面完成登录即可。
///
/// 当前统一使用可信 Chrome 配置，复用本机已登录会话完成授权。
pub async fn open_oauth_window(
    app: &AppHandle,
    auth_url: &str,
    _incognito: bool,
) -> Result<(), String> {
    let parsed = url::Url::parse(auth_url.trim())
        .map_err(|error| format!("WorkBuddy OAuth 授权地址无效: {}", error))?;
    let scheme = parsed.scheme();
    if scheme != "https" && scheme != "http" {
        return Err(format!("WorkBuddy OAuth 授权地址协议不受支持: {}", scheme));
    }
    let state = authorize_state(&parsed)
        .ok_or_else(|| "WorkBuddy OAuth 授权地址缺少 state 参数".to_string())?;
    let pending = {
        let pending = PENDING_OAUTH_STATE
            .lock()
            .map_err(|_| "获取锁失败".to_string())?;
        pending.values().find(|entry| entry.state == state).cloned()
    }
    .ok_or_else(|| "WorkBuddy OAuth 登录会话不存在或已结束".to_string())?;
    if pending.expires_at <= now_timestamp() {
        return Err("WorkBuddy OAuth 登录已过期，请重新发起授权".to_string());
    }

    let realm = realm_by_id(&pending.realm_id);
    let label = oauth_window_label_for(realm);

    // 每次授权都重建窗口：销毁旧窗口可确保每次都是全新的页面状态，不会残留上一次的表单。
    if let Some(window) = app.get_webview_window(label) {
        let _ = window.destroy();
    }

    crate::modules::chrome_oauth::open_trusted(parsed.as_str())?;

    logger::log_info(&format!(
        "[WorkBuddy OAuth][{}] 已打开 Chrome 可信授权窗口: login_id={}",
        realm.id, pending.login_id
    ));
    Ok(())
}

/// 关闭指定区域的内置授权窗口（窗口不存在时静默成功）。
pub fn close_oauth_window_for_realm(app: &AppHandle, realm: &WorkbuddyRealm) -> Result<(), String> {
    let label = oauth_window_label_for(realm);
    let app_for_thread = app.clone();
    // 关窗同样调度到主线程执行（Windows 上跨线程关窗会卡死，同 workbuddy_webview）。
    // 这里不等待结果：关窗失败无补救手段，记日志即可，别拖慢前端取消流程。
    app.run_on_main_thread(move || {
        if let Some(window) = app_for_thread.get_webview_window(label) {
            if let Err(error) = window.destroy() {
                logger::log_warn(&format!(
                    "[WorkBuddy OAuth] 关闭授权窗口失败: label={}, error={}",
                    label, error
                ));
            }
        }
    })
    .map_err(|error| format!("调度主线程关闭授权窗口失败: {}", error))
}

fn now_timestamp() -> i64 {
    chrono::Utc::now().timestamp()
}

fn generate_login_id() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let bytes: Vec<u8> = (0..16).map(|_| rng.gen::<u8>()).collect();
    format!(
        "wb_{}",
        bytes
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect::<String>()
    )
}

const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                  (KHTML, like Gecko) Chrome/147.0.0.0 Safari/537.36";

fn build_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent(UA)
        .timeout(std::time::Duration::from_secs(30))
        .user_agent(WORKBUDDY_HTTP_USER_AGENT)
        .build()
        .map_err(|e| format!("创建 HTTP 客户端失败:{}", e))
}

fn decorate_workbuddy_auth_url(
    raw_url: &str,
    version: Option<&str>,
    login_session_id: &str,
) -> String {
    let Ok(mut url) = url::Url::parse(raw_url) else {
        return raw_url.to_string();
    };
    {
        let mut query = url.query_pairs_mut();
        if let Some(version) = version.map(str::trim).filter(|value| !value.is_empty()) {
            query.append_pair("version", version);
        }
        query.append_pair("loginSessionId", login_session_id);
    }
    url.to_string()
}

fn normalize_non_empty(value: Option<&str>) -> Option<String> {
    value
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
        .map(|v| v.to_string())
}

fn build_user_resource_request_body() -> Value {
    json!({
        "PageNumber": 1,
        "PageSize": 100,
        "ProductCode": "p_tcaca",
        "Status": [0, 3],
        "OnlyValidPeriod": true
    })
}

fn user_resource_items(body: &Value) -> Option<&Vec<Value>> {
    [
        "/data/resources",
        "/data/data/resources",
        "/data/Response/Data/Accounts",
        "/data/data/Response/Data/Accounts",
        "/Response/Data/Accounts",
    ]
    .into_iter()
    .find_map(|path| body.pointer(path).and_then(Value::as_array))
}

fn user_resource_has_payload(body: &Value) -> bool {
    user_resource_items(body).is_some_and(|items| !items.is_empty())
}

fn user_resource_has_shape(body: &Value) -> bool {
    user_resource_items(body).is_some()
}

fn enterprise_usage_data(body: &Value) -> Option<&Value> {
    body.pointer("/data/data")
        .or_else(|| body.get("data"))
        .or(Some(body))
}

fn token_expiry_at(data: &Value) -> Option<i64> {
    let parse = |value: Option<&Value>| {
        value.and_then(|v| {
            v.as_i64()
                .or_else(|| v.as_str()?.trim().parse::<i64>().ok())
        })
    };
    parse(data.get("expiresAt").or_else(|| data.get("expires_at")))
        .map(|value| {
            if value > 0 && value < 100_000_000_000 {
                value.saturating_mul(1000)
            } else {
                value
            }
        })
        .or_else(|| {
            parse(data.get("expiresIn").or_else(|| data.get("expires_in")))
                .map(|seconds| chrono::Utc::now().timestamp_millis() + seconds.saturating_mul(1000))
        })
}
fn json_f64(value: Option<&Value>) -> Option<f64> {
    value.and_then(|v| {
        v.as_f64()
            .or_else(|| v.as_str()?.trim().parse::<f64>().ok())
    })
}
fn json_scalar_string(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

fn clear_pending_login(login_id: &str) -> Result<(), String> {
    let mut pending = PENDING_OAUTH_STATE
        .lock()
        .map_err(|_| "获取锁失败".to_string())?;
    // 按 login_id 精确移除，不影响其他区域正在进行的登录
    pending.remove(login_id);
    Ok(())
}

pub fn clear_pending_oauth_login(login_id: &str) -> Result<(), String> {
    clear_pending_login(login_id)
}

/// 按指定区域启动登录。国内版与国际版共用此函数，域名/平台参数全部来自 realm 配置。
async fn start_login_with_realm(
    realm: &WorkbuddyRealm,
) -> Result<WorkbuddyOAuthStartResponse, String> {
    let client = build_client()?;
    let url = format!(
        "{}{}/auth/state?platform={}",
        realm.api_endpoint, realm.api_prefix, realm.platform_tag
    );

    logger::log_info(&format!("[WorkBuddy OAuth] 请求 auth/state: {}", url));

    let resp = client
        .post(&url)
        .header("X-No-Authorization", "true")
        .header("X-No-User-Id", "true")
        .header("X-No-Enterprise-Id", "true")
        .header("X-No-Department-Info", "true")
        .json(&json!({}))
        .send()
        .await
        .map_err(|e| format!("请求 auth/state 失败:{}", e))?;

    let body: Value = resp
        .json()
        .await
        .map_err(|e| format!("解析 auth/state 响应失败:{}", e))?;

    let data = body.get("data").ok_or_else(|| {
        let mut keys = body
            .as_object()
            .map(|obj| obj.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        keys.sort();
        format!("auth/state 响应缺少 data 字段: body_keys={:?}", keys)
    })?;

    let state = data
        .get("state")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "auth/state 响应缺少 state".to_string())?
        .to_string();

    let auth_url = data
        .get("authUrl")
        .or_else(|| data.get("auth_url"))
        .or_else(|| data.get("url"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let login_id = generate_login_id();
    let login_session_id = uuid::Uuid::new_v4().to_string();
    let configured_path = crate::modules::config::get_user_config().workbuddy_app_path;
    let version = tokio::task::spawn_blocking(move || {
        crate::modules::client_version::detect_client_version("WorkBuddy", Some(&configured_path))
    })
    .await
    .ok()
    .flatten();

    // 上游未下发 authUrl 时，用该区域的登录页域名兜底
    let base_verification_uri = if auth_url.is_empty() {
        format!("{}/login?state={}", realm.login_origin, state)
    } else {
        auth_url.clone()
    };
    let verification_uri = decorate_workbuddy_auth_url(
        &base_verification_uri,
        version.as_deref(),
        &login_session_id,
    );

    {
        let mut pending = PENDING_OAUTH_STATE
            .lock()
            .map_err(|_| "获取锁失败".to_string())?;
        pending.insert(
            login_id.clone(),
            PendingOAuthState {
                login_id: login_id.clone(),
                realm_id: realm.id.to_string(),
                expires_at: now_timestamp() + OAUTH_TIMEOUT_SECONDS as i64,
                state: state.clone(),
                cancelled: false,
            },
        );
    }

    logger::log_info(&format!(
        "[WorkBuddy OAuth][{}] 登录已启动: login_id={}, state={}",
        realm.id, login_id, state
    ));

    Ok(WorkbuddyOAuthStartResponse {
        login_id,
        verification_uri: verification_uri.clone(),
        verification_uri_complete: Some(verification_uri),
        expires_in: OAUTH_TIMEOUT_SECONDS,
        interval_seconds: OAUTH_POLL_INTERVAL_MS / 1000 + 1,
    })
}

/// 国内版登录入口：保持既有签名与行为不变
pub async fn start_login() -> Result<WorkbuddyOAuthStartResponse, String> {
    start_login_with_realm(&REALM_CN).await
}

/// 按区域标识启动登录：`intl` / `global` 走国际版，其余走国内版
pub async fn start_login_for_realm(realm_id: &str) -> Result<WorkbuddyOAuthStartResponse, String> {
    start_login_with_realm(realm_by_id(realm_id)).await
}

pub async fn complete_login(login_id: &str) -> Result<WorkbuddyOAuthCompletePayload, String> {
    let client = build_client()?;
    let start = now_timestamp();

    loop {
        let state_info = {
            let pending = PENDING_OAUTH_STATE
                .lock()
                .map_err(|_| "获取锁失败".to_string())?;
            match pending.get(login_id) {
                None => return Err("没有待处理的登录请求".to_string()),
                Some(s) => {
                    if s.cancelled {
                        return Err("登录已取消".to_string());
                    }
                    if now_timestamp() > s.expires_at {
                        return Err("登录超时".to_string());
                    }
                    s.clone()
                }
            }
        };

        // 从 pending 记录还原该登录所属区域，保证轮询打回同一个网关（国际版不会打到国内版去）
        let realm = realm_by_id(&state_info.realm_id);
        let url = format!(
            "{}{}/auth/token?state={}",
            realm.api_endpoint, realm.api_prefix, state_info.state
        );

        match client
            .get(&url)
            .header("X-No-Authorization", "true")
            .header("X-No-User-Id", "true")
            .header("X-No-Enterprise-Id", "true")
            .header("X-No-Department-Info", "true")
            .send()
            .await
        {
            Ok(resp) => {
                if let Ok(body) = resp.json::<Value>().await {
                    let code = body.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);

                    if code == 0 || code == 200 {
                        if let Some(data) = body.get("data") {
                            let access_token = data
                                .get("accessToken")
                                .or_else(|| data.get("access_token"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();

                            if !access_token.is_empty() {
                                logger::log_info("[WorkBuddy OAuth] 获取 token 成功");

                                let refresh_token = data
                                    .get("refreshToken")
                                    .or_else(|| data.get("refresh_token"))
                                    .and_then(|v| v.as_str())
                                    .map(|s| s.to_string());

                                let expires_at = token_expiry_at(data);

                                let domain = data
                                    .get("domain")
                                    .and_then(|v| v.as_str())
                                    .map(|s| s.to_string());

                                let token_type = data
                                    .get("tokenType")
                                    .or_else(|| data.get("token_type"))
                                    .and_then(|v| v.as_str())
                                    .map(|s| s.to_string());

                                let auth_raw = Some(data.clone());

                                let account_info = fetch_account_info(
                                    &client,
                                    &access_token,
                                    &state_info.state,
                                    domain.as_deref(),
                                    realm,
                                )
                                .await;

                                let (
                                    uid,
                                    nickname,
                                    email,
                                    enterprise_id,
                                    enterprise_name,
                                    profile_raw,
                                ) = match account_info {
                                    Ok(info) => info,
                                    Err(e) => {
                                        logger::log_warn(&format!(
                                            "[WorkBuddy OAuth] 获取账号信息失败:{}",
                                            e
                                        ));
                                        (None, None, String::new(), None, None, None)
                                    }
                                };

                                return Ok(WorkbuddyOAuthCompletePayload {
                                    email,
                                    uid,
                                    nickname,
                                    enterprise_id,
                                    enterprise_name,
                                    access_token,
                                    refresh_token,
                                    token_type,
                                    expires_at,
                                    domain,
                                    plan_type: None,
                                    dosage_notify_code: None,
                                    dosage_notify_zh: None,
                                    dosage_notify_en: None,
                                    payment_type: None,
                                    quota_raw: None,
                                    auth_raw,
                                    profile_raw,
                                    usage_raw: None,
                                    status: Some("normal".to_string()),
                                    status_reason: None,
                                });
                            }
                        }
                    }
                }
            }
            Err(e) => {
                logger::log_warn(&format!("[WorkBuddy OAuth] 轮询 token 请求失败:{}", e));
            }
        }

        if now_timestamp() - start > OAUTH_TIMEOUT_SECONDS as i64 {
            let mut pending = PENDING_OAUTH_STATE
                .lock()
                .map_err(|_| "获取锁失败".to_string())?;
            pending.remove(login_id);
            return Err("登录超时".to_string());
        }

        tokio::time::sleep(std::time::Duration::from_millis(OAUTH_POLL_INTERVAL_MS)).await;
    }
}

pub fn cancel_login(login_id: Option<&str>) -> Result<(), String> {
    let mut pending = PENDING_OAUTH_STATE
        .lock()
        .map_err(|_| "获取锁失败".to_string())?;
    match login_id {
        // 指定 login_id：只取消这一条
        Some(id) => {
            pending.remove(id);
        }
        // 未指定：取消所有待处理登录（含两个区域）
        None => pending.clear(),
    }
    Ok(())
}

async fn fetch_account_info(
    client: &reqwest::Client,
    access_token: &str,
    state: &str,
    domain: Option<&str>,
    realm: &WorkbuddyRealm,
) -> Result<
    (
        Option<String>,
        Option<String>,
        String,
        Option<String>,
        Option<String>,
        Option<Value>,
    ),
    String,
> {
    let url = format!(
        "{}{}/login/account?state={}",
        realm.api_endpoint, realm.api_prefix, state
    );

    let mut req = client
        .get(&url)
        .header("Authorization", format!("Bearer {}", access_token))
        .header("X-No-User-Id", "true")
        .header("X-No-Enterprise-Id", "true")
        .header("X-No-Department-Info", "true");

    if let Some(d) = domain {
        req = req.header("X-Domain", d);
    }

    let resp = req
        .send()
        .await
        .map_err(|e| format!("请求 login/account 失败:{}", e))?;

    let body: Value = resp
        .json()
        .await
        .map_err(|e| format!("解析 login/account 响应失败:{}", e))?;

    let data = body.get("data").cloned().unwrap_or(json!({}));

    let uid = data
        .get("uid")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let nickname = data
        .get("nickname")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let email = data
        .get("email")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let enterprise_id = data
        .get("enterpriseId")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    let enterprise_name = data
        .get("enterpriseName")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    let email_final = if email.is_empty() {
        nickname.clone().or_else(|| uid.clone()).unwrap_or_default()
    } else {
        email
    };

    Ok((
        uid,
        nickname,
        email_final,
        enterprise_id,
        enterprise_name,
        Some(data),
    ))
}

pub async fn refresh_token(
    access_token: &str,
    refresh_token: &str,
    domain: Option<&str>,
    realm: &WorkbuddyRealm,
) -> Result<Value, String> {
    let client = build_client()?;
    let url = format!(
        "{}{}/auth/token/refresh",
        realm.api_endpoint, realm.api_prefix
    );

    let mut req = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", access_token))
        .header("X-Refresh-Token", refresh_token)
        .header("X-Auth-Refresh-Source", "plugin")
        .json(&json!({}));

    if let Some(d) = domain {
        req = req.header("X-Domain", d);
    }

    let resp = req
        .send()
        .await
        .map_err(|e| format!("刷新 token 失败:{}", e))?;

    let body: Value = resp
        .json()
        .await
        .map_err(|e| format!("解析刷新响应失败:{}", e))?;

    let code = body.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
    if code != 0 && code != 200 {
        let msg = body
            .get("message")
            .or_else(|| body.get("msg"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown error");
        return Err(format!("刷新 token 失败 (code={}): {}", code, msg));
    }

    body.get("data")
        .cloned()
        .ok_or_else(|| "刷新响应缺少 data 字段".to_string())
}

pub async fn fetch_dosage_notify(
    access_token: &str,
    uid: Option<&str>,
    enterprise_id: Option<&str>,
    domain: Option<&str>,
    realm: &WorkbuddyRealm,
) -> Result<Value, String> {
    let client = build_client()?;
    let url = format!("{}/v2/billing/meter/get-dosage-notify", realm.api_endpoint);

    let mut req = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", access_token))
        .header("Content-Type", "application/json");

    if let Some(u) = uid {
        req = req.header("X-User-Id", u);
    }
    if let Some(eid) = enterprise_id {
        req = req.header("X-Enterprise-Id", eid);
        req = req.header("X-Tenant-Id", eid);
    }
    if let Some(d) = domain {
        req = req.header("X-Domain", d);
    }

    let resp = req
        .send()
        .await
        .map_err(|e| format!("请求 dosage notify 失败:{}", e))?;

    let body: Value = resp
        .json()
        .await
        .map_err(|e| format!("解析 dosage 响应失败:{}", e))?;

    Ok(body)
}

pub async fn fetch_payment_type(
    access_token: &str,
    uid: Option<&str>,
    enterprise_id: Option<&str>,
    domain: Option<&str>,
    realm: &WorkbuddyRealm,
) -> Result<Value, String> {
    let client = build_client()?;
    let url = format!("{}/v2/billing/meter/get-payment-type", realm.api_endpoint);

    let mut req = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", access_token))
        .header("Content-Type", "application/json");

    if let Some(u) = uid {
        req = req.header("X-User-Id", u);
    }
    if let Some(eid) = enterprise_id {
        req = req.header("X-Enterprise-Id", eid);
        req = req.header("X-Tenant-Id", eid);
    }
    if let Some(d) = domain {
        req = req.header("X-Domain", d);
    }

    let resp = req
        .send()
        .await
        .map_err(|e| format!("请求 payment type 失败:{}", e))?;

    let body: Value = resp
        .json()
        .await
        .map_err(|e| format!("解析 payment type 响应失败:{}", e))?;

    Ok(body)
}

pub async fn fetch_user_resource_with_access_token(
    access_token: &str,
    uid: Option<&str>,
    enterprise_id: Option<&str>,
    domain: Option<&str>,
    product_code: &str,
    status: &[i32],
    _package_end_time_range_begin: &str,
    _package_end_time_range_end: &str,
    _page_number: i32,
    _page_size: i32,
    realm: &WorkbuddyRealm,
) -> Result<Value, String> {
    let _ = (product_code, status);
    let body = build_user_resource_request_body();
    post_user_resource(access_token, uid, enterprise_id, domain, body, realm).await
}

async fn post_user_resource(
    access_token: &str,
    uid: Option<&str>,
    enterprise_id: Option<&str>,
    domain: Option<&str>,
    body: Value,
    realm: &WorkbuddyRealm,
) -> Result<Value, String> {
    let client = build_client()?;
    let url = format!("{}/v2/billing/meter/get-user-resource", realm.api_endpoint);

    let mut req = client
        .post(&url)
        .header("Accept", "application/json, text/plain, */*")
        .header("Accept-Language", "zh-CN,zh;q=0.9")
        .header("Authorization", format!("Bearer {}", access_token))
        .header("Content-Type", "application/json");

    if let Some(u) = uid {
        req = req.header("X-User-Id", u);
    }
    if let Some(eid) = enterprise_id {
        req = req.header("X-Enterprise-Id", eid);
        req = req.header("X-Tenant-Id", eid);
    }
    if let Some(d) = domain {
        req = req.header("X-Domain", d);
    }

    let resp = req
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("请求 user resource（Token）失败:{}", e))?;

    let status_code = resp.status();
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let content_encoding = resp
        .headers()
        .get(reqwest::header::CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let body: Value = resp
        .json()
        .await
        .map_err(|e| {
            format!(
                "解析 user resource（Token）响应失败:{} (http={}, url={}, has_uid={}, has_enterprise_id={}, content_type={}, content_encoding={})",
                e,
                status_code.as_u16(),
                url,
                uid.is_some(),
                enterprise_id.is_some(),
                content_type,
                content_encoding
            )
        })?;

    if !status_code.is_success() {
        let message = body
            .get("message")
            .or_else(|| body.get("msg"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown error");
        return Err(format!(
            "请求 user resource（Token）失败 (http={}): {}",
            status_code.as_u16(),
            message
        ));
    }

    if let Some(code) = body.get("code").and_then(|v| v.as_i64()) {
        if code != 0 && code != 200 {
            let message = body
                .get("message")
                .or_else(|| body.get("msg"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            return Err(format!(
                "请求 user resource（Token）失败 (code={}): {}",
                code, message
            ));
        }
    }

    Ok(body)
}

/// 企业版用户用量（网页端 /profile/usage 实际使用的接口）
pub async fn fetch_enterprise_user_usage(
    access_token: &str,
    uid: Option<&str>,
    enterprise_id: &str,
    domain: Option<&str>,
    realm: &WorkbuddyRealm,
) -> Result<Value, String> {
    let client = build_client()?;
    let url = format!(
        "{}/v2/billing/meter/get-enterprise-user-usage",
        realm.api_endpoint
    );

    let mut req = client
        .post(&url)
        .header("Accept", "application/json, text/plain, */*")
        .header("Accept-Language", "zh-CN,zh;q=0.9")
        .header("Authorization", format!("Bearer {}", access_token))
        .header("Content-Type", "application/json")
        .header("X-Enterprise-Id", enterprise_id)
        .header("X-Tenant-Id", enterprise_id);
    if let Some(uid) = uid {
        req = req.header("X-User-Id", uid);
    }
    if let Some(domain) = domain {
        req = req.header("X-Domain", domain);
    }

    let resp = req
        .json(&json!({}))
        .send()
        .await
        .map_err(|e| format!("请求 enterprise user usage 失败:{}", e))?;

    let status_code = resp.status();
    let body: Value = resp.json().await.map_err(|e| {
        format!(
            "解析 enterprise user usage 响应失败:{} (http={})",
            e,
            status_code.as_u16()
        )
    })?;

    if !status_code.is_success() {
        let message = body
            .get("message")
            .or_else(|| body.get("msg"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown error");
        return Err(format!(
            "请求 enterprise user usage 失败 (http={}):{}",
            status_code.as_u16(),
            message
        ));
    }

    if let Some(code) = body.get("code").and_then(|v| v.as_i64()) {
        if code != 0 && code != 200 {
            let message = body
                .get("message")
                .or_else(|| body.get("msg"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            return Err(format!(
                "请求 enterprise user usage 失败 (code={}):{}",
                code, message
            ));
        }
    }

    if enterprise_usage_data(&body)
        .and_then(|data| data.get("limit_num").or_else(|| data.get("limitNum")))
        .and_then(|value| json_f64(Some(value)))
        .is_none()
    {
        return Err("enterprise user usage 响应缺少 limit_num/limitNum".to_string());
    }

    Ok(body)
}

/// 将企业用量响应包装成前端可识别的 get-user-resource 结构
pub fn wrap_enterprise_usage_as_resource(usage_body: &Value) -> Result<Value, String> {
    let data = enterprise_usage_data(usage_body)
        .ok_or_else(|| "enterprise user usage 响应缺少 data".to_string())?;
    let limit_num = data
        .get("limit_num")
        .or_else(|| data.get("limitNum"))
        .and_then(|value| json_f64(Some(value)))
        .ok_or_else(|| "enterprise user usage 响应缺少 limit_num/limitNum".to_string())?;
    let used_num = data
        .get("used_num")
        .or_else(|| data.get("usedNum"))
        .or_else(|| data.get("credit"))
        .and_then(|value| json_f64(Some(value)))
        .ok_or_else(|| "enterprise user usage 响应缺少 credit/used_num".to_string())?;
    let unlimited = limit_num == -1.0;
    let remain = if unlimited {
        -1.0
    } else {
        (limit_num - used_num).max(0.0)
    };
    let cycle_start_time = json_scalar_string(
        data.get("cycle_start_time")
            .or_else(|| data.get("cycleStartTime")),
    );
    let cycle_end_time = json_scalar_string(
        data.get("cycle_end_time")
            .or_else(|| data.get("cycleEndTime")),
    );
    let cycle_reset_time = json_scalar_string(
        data.get("cycle_reset_time")
            .or_else(|| data.get("cycleResetTime")),
    );

    let account = json!({
        "PackageCode": ENTERPRISE_PACKAGE_CODE,
        "PackageName": "企业版",
        "CycleCapacitySizePrecise": limit_num.to_string(),
        "CycleCapacityRemainPrecise": remain.to_string(),
        "CycleCapacityUsedPrecise": used_num.to_string(),
        "CycleCapacitySize": limit_num,
        "CycleCapacityRemain": remain,
        "CycleCapacityUsed": used_num,
        "CapacitySize": limit_num,
        "CapacityRemain": remain,
        "CapacityUsed": used_num,
        "CapacityUnit": "credits",
        "CycleStartTime": cycle_start_time,
        "CycleEndTime": cycle_end_time,
        "CycleResetTime": cycle_reset_time,
        "Unlimited": unlimited,
        "Status": 0
    });

    Ok(json!({
        "code": 0,
        "msg": "OK",
        "data": {
            "Response": {
                "Data": {
                    "Accounts": [account],
                    "TotalCount": 1,
                    "TotalDosage": used_num
                }
            }
        }
    }))
}

async fn fetch_user_resource_with_access_token_default(
    access_token: &str,
    uid: Option<&str>,
    enterprise_id: Option<&str>,
    domain: Option<&str>,
    realm: &WorkbuddyRealm,
) -> Result<Value, String> {
    let payload = post_user_resource(
        access_token,
        uid,
        enterprise_id,
        domain,
        build_user_resource_request_body(),
        realm,
    )
    .await?;
    if user_resource_has_payload(&payload) {
        Ok(payload)
    } else if user_resource_has_shape(&payload) {
        Err("user resource 响应未包含可用资源".to_string())
    } else {
        Err("user resource 响应缺少 resources/Accounts".to_string())
    }
}

async fn fetch_quota_resource_for_account(
    access_token: &str,
    uid: Option<&str>,
    enterprise_id: Option<&str>,
    domain: Option<&str>,
    realm: &WorkbuddyRealm,
) -> Result<Value, String> {
    if let Some(enterprise_id) = enterprise_id {
        let body =
            fetch_enterprise_user_usage(access_token, uid, enterprise_id, domain, realm).await?;
        wrap_enterprise_usage_as_resource(&body)
    } else {
        fetch_user_resource_with_access_token_default(access_token, uid, None, domain, realm).await
    }
}

async fn refresh_payload_for_account_inner(
    account: &crate::models::workbuddy::WorkbuddyAccount,
    require_user_resource: bool,
    realm: &WorkbuddyRealm,
) -> Result<(WorkbuddyOAuthCompletePayload, Option<String>), String> {
    let mut new_access_token = account.access_token.clone();
    let mut new_refresh_token = account.refresh_token.clone();
    let mut new_expires_at = account.expires_at;
    let mut new_domain = account.domain.clone();

    if let Some(refresh_tk) = account.refresh_token.as_deref() {
        match refresh_token(
            &account.access_token,
            refresh_tk,
            account.domain.as_deref(),
            realm,
        )
        .await
        {
            Ok(token_data) => {
                new_access_token = token_data
                    .get("accessToken")
                    .or_else(|| token_data.get("access_token"))
                    .and_then(|v| v.as_str())
                    .unwrap_or(&account.access_token)
                    .to_string();

                new_refresh_token = token_data
                    .get("refreshToken")
                    .or_else(|| token_data.get("refresh_token"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .or_else(|| account.refresh_token.clone());

                new_expires_at = token_expiry_at(&token_data).or(account.expires_at);

                new_domain = token_data
                    .get("domain")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .or_else(|| account.domain.clone());
            }
            Err(e) => {
                logger::log_warn(&format!(
                    "[WorkBuddy] Token 刷新失败，将使用现有 token 查询配额:{}",
                    e
                ));
            }
        }
    }

    let resolved_email =
        normalize_non_empty(Some(account.email.as_str())).unwrap_or_else(|| account.email.clone());
    let resolved_uid = account.uid.clone();
    let resolved_nickname = account.nickname.clone();
    let resolved_enterprise_id = account.enterprise_id.clone();
    let resolved_enterprise_name = account.enterprise_name.clone();
    let resolved_profile_raw = account.profile_raw.clone();

    let dosage = fetch_dosage_notify(
        &new_access_token,
        resolved_uid.as_deref(),
        resolved_enterprise_id.as_deref(),
        new_domain.as_deref(),
        realm,
    )
    .await
    .ok();

    let payment = fetch_payment_type(
        &new_access_token,
        resolved_uid.as_deref(),
        resolved_enterprise_id.as_deref(),
        new_domain.as_deref(),
        realm,
    )
    .await
    .ok();

    let mut quota_refresh_error: Option<String> = None;
    logger::log_info(&format!(
        "[WorkBuddy][IDE Token] 尝试刷新 user_resource: has_uid={}, has_enterprise_id={}, has_domain={}",
        resolved_uid.is_some(),
        resolved_enterprise_id.is_some(),
        new_domain.is_some()
    ));
    let user_resource = match fetch_quota_resource_for_account(
        new_access_token.as_str(),
        resolved_uid.as_deref(),
        resolved_enterprise_id.as_deref(),
        new_domain.as_deref(),
        realm,
    )
    .await
    {
        Ok(payload) => {
            logger::log_info("[WorkBuddy][IDE Token] 刷新额度成功");
            Some(payload)
        }
        Err(err) => {
            logger::log_warn(&format!(
                "[WorkBuddy][IDE Token] 刷新 user_resource 失败:{}",
                err
            ));
            quota_refresh_error = Some(err.clone());
            if require_user_resource {
                return Err(
                    "使用 IDE token 刷新 user_resource 失败，无法获取资源包配额".to_string()
                );
            }
            None
        }
    };

    let dosage_data = dosage.as_ref().and_then(|v| v.get("data"));
    let dosage_notify_code = dosage_data
        .and_then(|d| d.get("dosageNotifyCode"))
        .map(|v| match v {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            _ => v.to_string(),
        });
    let dosage_notify_zh = dosage_data
        .and_then(|d| d.get("dosageNotifyZh"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let dosage_notify_en = dosage_data
        .and_then(|d| d.get("dosageNotifyEn"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    let payment_data = payment.as_ref().and_then(|v| v.get("data"));
    let payment_type = payment_data
        .and_then(|d| {
            d.as_str().map(|s| s.to_string()).or_else(|| {
                d.get("paymentType")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            })
        })
        .or_else(|| account.payment_type.clone());

    let mut combined_quota = account
        .quota_raw
        .as_ref()
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if let Some(d) = &dosage {
        combined_quota.insert("dosage".to_string(), d.clone());
    }
    if let Some(p) = &payment {
        combined_quota.insert("payment".to_string(), p.clone());
    }
    if let Some(r) = &user_resource {
        combined_quota.insert("userResource".to_string(), r.clone());
    }

    let quota_raw = if combined_quota.is_empty() {
        account.quota_raw.clone()
    } else {
        Some(Value::Object(combined_quota))
    };

    let final_email =
        normalize_non_empty(Some(resolved_email.as_str())).unwrap_or_else(|| account.email.clone());

    Ok((
        WorkbuddyOAuthCompletePayload {
            email: final_email,
            uid: resolved_uid,
            nickname: resolved_nickname,
            enterprise_id: resolved_enterprise_id,
            enterprise_name: resolved_enterprise_name,
            access_token: new_access_token,
            refresh_token: new_refresh_token,
            token_type: account.token_type.clone(),
            expires_at: new_expires_at,
            domain: new_domain,
            plan_type: account.plan_type.clone(),
            dosage_notify_code,
            dosage_notify_zh,
            dosage_notify_en,
            payment_type,
            quota_raw,
            auth_raw: account.auth_raw.clone(),
            profile_raw: resolved_profile_raw,
            usage_raw: user_resource.or_else(|| account.usage_raw.clone()),
            status: account.status.clone(),
            status_reason: account.status_reason.clone(),
        },
        quota_refresh_error,
    ))
}

/// 国内版账号刷新入口：保持既有签名与行为不变
pub async fn refresh_payload_for_account(
    account: &crate::models::workbuddy::WorkbuddyAccount,
) -> Result<(WorkbuddyOAuthCompletePayload, Option<String>), String> {
    refresh_payload_for_account_for_realm(account, &REALM_CN).await
}

/// 按区域刷新账号：国际版账号走国际版网关
pub async fn refresh_payload_for_account_for_realm(
    account: &crate::models::workbuddy::WorkbuddyAccount,
    realm: &WorkbuddyRealm,
) -> Result<(WorkbuddyOAuthCompletePayload, Option<String>), String> {
    refresh_payload_for_account_inner(account, false, realm).await
}

/// 国内版用 token 构建账号：保持既有签名与行为不变
pub async fn build_payload_from_token(
    access_token: &str,
) -> Result<WorkbuddyOAuthCompletePayload, String> {
    build_payload_from_token_for_realm(access_token, &REALM_CN).await
}

/// 按区域用 token 构建账号
pub async fn build_payload_from_token_for_realm(
    access_token: &str,
    realm: &WorkbuddyRealm,
) -> Result<WorkbuddyOAuthCompletePayload, String> {
    let client = build_client()?;

    let url = format!("{}{}/accounts", realm.api_endpoint, realm.api_prefix);

    let resp = client
        .get(&url)
        .header("Authorization", format!("Bearer {}", access_token))
        .send()
        .await
        .map_err(|e| format!("请求 accounts 失败: {}", e))?;

    let body: Value = resp
        .json()
        .await
        .map_err(|e| format!("解析 accounts 响应失败:{}", e))?;

    let accounts = body
        .get("data")
        .and_then(|d| d.get("accounts"))
        .and_then(|a| a.as_array());

    let account_data = accounts
        .and_then(|arr| {
            arr.iter().find(|a| {
                a.get("lastLogin")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
            })
        })
        .or_else(|| accounts.and_then(|arr| arr.first()))
        .cloned()
        .unwrap_or(json!({}));

    let uid = account_data
        .get("uid")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let nickname = account_data
        .get("nickname")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let email = account_data
        .get("email")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let enterprise_id = account_data
        .get("enterpriseId")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    let enterprise_name = account_data
        .get("enterpriseName")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    let dosage = fetch_dosage_notify(
        access_token,
        uid.as_deref(),
        enterprise_id.as_deref(),
        None,
        realm,
    )
    .await
    .ok();

    let payment = fetch_payment_type(
        access_token,
        uid.as_deref(),
        enterprise_id.as_deref(),
        None,
        realm,
    )
    .await
    .ok();

    let user_resource = fetch_user_resource_with_access_token_default(
        access_token,
        uid.as_deref(),
        enterprise_id.as_deref(),
        None,
        realm,
    )
    .await
    .ok();

    let dosage_data = dosage.as_ref().and_then(|v| v.get("data"));
    let dosage_notify_code = dosage_data
        .and_then(|d| d.get("dosageNotifyCode"))
        .map(|v| match v {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            _ => v.to_string(),
        });
    let dosage_notify_zh = dosage_data
        .and_then(|d| d.get("dosageNotifyZh"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let dosage_notify_en = dosage_data
        .and_then(|d| d.get("dosageNotifyEn"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    let payment_data = payment.as_ref().and_then(|v| v.get("data"));
    let payment_type = payment_data.and_then(|d| {
        d.as_str().map(|s| s.to_string()).or_else(|| {
            d.get("paymentType")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
    });

    let mut combined_quota = serde_json::Map::new();
    if let Some(payload) = dosage.as_ref() {
        combined_quota.insert("dosage".to_string(), payload.clone());
    }
    if let Some(payload) = payment.as_ref() {
        combined_quota.insert("payment".to_string(), payload.clone());
    }
    if let Some(payload) = user_resource.as_ref() {
        combined_quota.insert("userResource".to_string(), payload.clone());
    }
    let quota_raw = if combined_quota.is_empty() {
        None
    } else {
        Some(Value::Object(combined_quota))
    };

    let email_final = if email.is_empty() {
        nickname
            .clone()
            .or_else(|| uid.clone())
            .unwrap_or_else(|| "unknown".to_string())
    } else {
        email
    };

    Ok(WorkbuddyOAuthCompletePayload {
        email: email_final,
        uid,
        nickname,
        enterprise_id,
        enterprise_name,
        access_token: access_token.to_string(),
        refresh_token: None,
        token_type: Some("Bearer".to_string()),
        expires_at: None,
        domain: None,
        plan_type: None,
        dosage_notify_code,
        dosage_notify_zh,
        dosage_notify_en,
        payment_type,
        quota_raw,
        auth_raw: None,
        profile_raw: Some(account_data),
        usage_raw: user_resource,
        status: Some("normal".to_string()),
        status_reason: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_url_contains_detected_version_and_login_session() {
        let decorated = decorate_workbuddy_auth_url(
            "https://copilot.tencent.com/login?state=test",
            Some("5.4.5"),
            "00000000-0000-4000-8000-000000000000",
        );
        let parsed = url::Url::parse(&decorated).unwrap();
        let params = parsed
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(
            params.get("version").map(|value| value.as_ref()),
            Some("5.4.5")
        );
        assert_eq!(
            params.get("loginSessionId").map(|value| value.as_ref()),
            Some("00000000-0000-4000-8000-000000000000")
        );
    }

    #[test]
    fn user_resource_body_uses_only_current_official_fields() {
        assert_eq!(
            build_user_resource_request_body(),
            json!({
                "PageNumber": 1,
                "PageSize": 100,
                "ProductCode": "p_tcaca",
                "Status": [0, 3],
                "OnlyValidPeriod": true
            })
        );
    }

    #[test]
    fn token_expiry_accepts_duration_and_epoch_units() {
        let before = chrono::Utc::now().timestamp_millis();
        let relative = token_expiry_at(&json!({ "expiresIn": 60 })).unwrap();
        assert!(relative >= before + 60_000);
        assert_eq!(
            token_expiry_at(&json!({ "expiresAt": 1_793_368_047_633_i64 })),
            Some(1_793_368_047_633_i64)
        );
        assert_eq!(
            token_expiry_at(&json!({ "expiresAt": 1_793_368_047 })),
            Some(1_793_368_047_000)
        );
    }

    #[test]
    fn enterprise_unlimited_is_not_converted_to_zero() {
        let wrapped = wrap_enterprise_usage_as_resource(&json!({
            "data": { "limit_num": -1, "used_num": 12 }
        }))
        .unwrap();
        let resource = wrapped.pointer("/data/Response/Data/Accounts/0").unwrap();
        assert_eq!(resource.get("Unlimited"), Some(&json!(true)));
        assert_eq!(
            resource.get("CycleCapacityRemainPrecise"),
            Some(&json!("-1"))
        );
    }

    #[test]
    fn accepts_double_data_envelope_and_numeric_strings() {
        assert!(user_resource_has_payload(&json!({
            "data": { "data": { "Response": { "Data": { "Accounts": [{}] } } } }
        })));
        let wrapped = wrap_enterprise_usage_as_resource(&json!({
            "data": { "data": { "limitNum": "100", "credit": "20", "cycleResetTime": 1_800_000_000 } }
        })).unwrap();
        let resource = wrapped.pointer("/data/Response/Data/Accounts/0").unwrap();
        assert_eq!(resource["CycleCapacityRemainPrecise"], "80");
        assert_eq!(resource["CycleResetTime"], "1800000000");
    }

    #[test]
    fn derives_expiry_from_expires_in_when_absolute_time_is_missing() {
        let before = chrono::Utc::now().timestamp_millis() + 59_000;
        let expiry = token_expiry_at(&json!({ "expiresIn": "60" })).unwrap();
        let after = chrono::Utc::now().timestamp_millis() + 61_000;
        assert!((before..=after).contains(&expiry));
    }
}
