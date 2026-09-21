//! QoderWork API 网关。
//!
//! 从本机 QoderWork 桌面端登录态读取 token，刷新后反代 Qoder 的
//! OpenAI Chat Completions 兼容推理接口。上游强制流式，因此网关在客户端
//! 请求非流式时负责把 SSE 聚合成普通 JSON。

use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use serde_json::Map;
use serde_json::{json, Value};

use crate::modules::logger;

/// 本地监听端口：避开现有 7863-7865 网关。
const PORT: u16 = 7866;
/// 监听地址：绑定全部网卡。容器里的 sub2api 通过 host.docker.internal 访问宿主机网关，
/// 该地址不是 loopback，只绑 127.0.0.1 会连不上。请求仍需本地 API Key，不会裸奔。
const LISTEN_HOST: &str = "0.0.0.0";
/// 推理上游地址，协议为 OpenAI Chat Completions。
const UPSTREAM_CHAT_URL: &str = "https://api2-v2.qoder.sh/model/v1/chat/completions";
/// QoderWork IDE 使用的设备 token 刷新接口。
const DEVICE_TOKEN_REFRESH_URL: &str = "https://openapi.qoder.sh/api/v1/deviceToken/refresh";
/// 本地网关 API Key 存储文件名。
const CONFIG_FILE_NAME: &str = "qoderwork_gateway.json";
/// QoderWork 模型目录：第一个字段是上游真实 ID，第二个字段是用户可见名称。
///
/// `/v1/models` 对外把用户可见名称放在 `id`，因为部分 OpenAI 客户端（包括
/// sub2api 的模型列表）只读取 `id`，不会读取 `display_name`。聊天请求再通过
/// `resolve_upstream_model` 映射回这里的真实 ID。
const AVAILABLE_MODELS: &[(&str, &str)] = &[
    ("qwork-ultimate", "Premium"),
    ("qwork-advanced", "Advanced"),
    ("qwork-auto", "Standard"),
    ("smodel", "Sonus"),
    ("qmodel_38max", "Qwen3.8-Max"),
    ("qfmodel", "Qwen3.8-Flash"),
    ("qmodel_latest", "Qwen3.7-Max"),
    ("qmodel", "Qwen3.7-Plus"),
];
/// 提前刷新窗口，避免请求中途 token 过期。
const TOKEN_REFRESH_SKEW_MS: i64 = 5 * 60 * 1000;
/// 应用是否已尝试启动网关服务。
static STARTED: AtomicBool = AtomicBool::new(false);

/// 内存中的当前 token：刷新后只保留访问 token 与过期毫秒，refresh token 仍在本地凭据里。
#[derive(Clone, Debug)]
struct CachedToken {
    access_token: String,
    expires_at_ms: i64,
    candidate_index: usize,
}

/// 读取当前 token；锁损坏视为未登录，由请求路径返回明确错误。
fn cached_token() -> &'static Mutex<Option<CachedToken>> {
    static TOKEN: OnceLock<Mutex<Option<CachedToken>>> = OnceLock::new();
    TOKEN.get_or_init(|| Mutex::new(None))
}

/// 启动 QoderWork 网关（幂等）。凭据缺失只记录日志，QoderWork 登录后下个请求会自动重读。
pub fn ensure_started() {
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(
        || match tiny_http::Server::http(format!("{LISTEN_HOST}:{PORT}")) {
            Ok(server) => {
                logger::log_info(&format!("[QoderWorkGateway] 端口 {PORT} 已监听"));
                for request in server.incoming_requests() {
                    std::thread::spawn(move || handle_request(request));
                }
            }
            Err(error) => {
                logger::log_error(&format!("[QoderWorkGateway] 绑定 {PORT} 失败: {error}"));
            }
        },
    );
}

/// 分发本地 HTTP 请求。
fn handle_request(request: tiny_http::Request) {
    if *request.method() == tiny_http::Method::Options {
        return answer_preflight(request);
    }
    let url = request.url().split('?').next().unwrap_or("").to_string();
    match (request.method().clone(), url.as_str()) {
        (tiny_http::Method::Post, "/v1/chat/completions") => handle_chat(request),
        (tiny_http::Method::Get, "/v1/models") => {
            if check_auth(&request).is_err() {
                return write_error(
                    request,
                    401,
                    "invalid_api_key",
                    "missing or invalid API key",
                );
            }
            write_json_response(
                request,
                200,
                &json!({
                    "object": "list",
                    "data": AVAILABLE_MODELS
                        .iter()
                        .map(|(upstream_id, display_name)| {
                            json!({
                                // 兼容只读取 id 的客户端，同时保留真实 ID 供排查与兼容调用。
                                "id": display_name,
                                "name": display_name,
                                "display_name": display_name,
                                "canonical_id": upstream_id,
                                "owned_by": "qoderwork",
                                "object": "model",
                            })
                        })
                        .collect::<Vec<_>>(),
                }),
            );
        }
        (tiny_http::Method::Get, "/api/config") => {
            let api_key = resolve_api_key();
            write_json_response(
                request,
                200,
                &json!({
                    "api_key": api_key,
                    "base_url": format!("http://127.0.0.1:{PORT}/v1"),
                    "docker_base_url": docker_base_url(),
                }),
            );
        }
        (tiny_http::Method::Get, "/api/status") => write_status(request),
        _ => write_error(request, 404, "not_found", "unknown endpoint"),
    }
}

/// 返回脱敏后的登录状态，前端只展示账号名与可用性。
fn write_status(request: tiny_http::Request) {
    let domestic_accounts = crate::modules::qoder_account::list_accounts();
    let international_accounts = crate::modules::qoderwork_account::list_accounts();
    let accounts = gateway_accounts(&domestic_accounts, &international_accounts);
    let fallback_refresh_token = read_safe_storage_auth()
        .ok()
        .and_then(|auth| find_refresh_token(&auth));
    let configured_total = accounts.len();
    let healthy = accounts
        .iter()
        .filter(|account| account.refresh_token.is_some())
        .count()
        + usize::from(
            fallback_refresh_token.is_some()
                && !accounts.iter().any(|account| {
                    account.refresh_token.as_deref() == fallback_refresh_token.as_deref()
                }),
        );
    let total = configured_total.max(usize::from(fallback_refresh_token.is_some()));
    let first_account = accounts.first();
    let status = json!({
        "available": healthy > 0 && total > 0,
        "total": total,
        "healthy": healthy.min(total),
        "domestic_total": domestic_accounts.len(),
        "international_total": international_accounts.len(),
        "account": {
            "name": first_account
                .and_then(|account| account.display_name.as_deref())
                .unwrap_or_default(),
            "email": first_account
                .map(|account| account.email.as_str())
                .unwrap_or_default(),
            "tier": first_account
                .and_then(|account| account.plan_type.as_deref())
                .unwrap_or_default(),
        },
    });
    write_json_response(request, 200, &status);
}

#[derive(Clone, Debug)]
struct GatewayAccount {
    id: String,
    email: String,
    display_name: Option<String>,
    plan_type: Option<String>,
    refresh_token: Option<String>,
}

fn gateway_accounts(
    domestic_accounts: &[crate::models::qoder::QoderAccount],
    international_accounts: &[crate::models::qoder::QoderWorkAccount],
) -> Vec<GatewayAccount> {
    domestic_accounts
        .iter()
        .map(|account| GatewayAccount {
            id: account.id.clone(),
            email: account.email.clone(),
            display_name: account.display_name.clone(),
            plan_type: account.plan_type.clone(),
            refresh_token: extract_refresh_token(&account.auth_user_info_raw),
        })
        .chain(international_accounts.iter().map(|account| GatewayAccount {
            id: account.id.clone(),
            email: account.email.clone(),
            display_name: account.display_name.clone(),
            plan_type: account.plan_type.clone(),
            refresh_token: extract_refresh_token(&account.auth_user_info_raw),
        }))
        .collect()
}

/// Bearer 本地 key 校验；读取 key 失败时视为未配置并拒绝。
fn check_auth(request: &tiny_http::Request) -> Result<(), ()> {
    let api_key = resolve_api_key();
    if api_key.is_empty() {
        return Err(());
    }
    let ok = request.headers().iter().any(|header| {
        header.field.equiv("Authorization")
            && header.value.as_str().strip_prefix("Bearer ") == Some(api_key.as_str())
    });
    if ok {
        Ok(())
    } else {
        Err(())
    }
}

/// 处理 Chat Completions：先保证 token，再强制上游流式，最后按客户端要求透传或聚合。
fn handle_chat(mut request: tiny_http::Request) {
    if check_auth(&request).is_err() {
        return write_error(
            request,
            401,
            "invalid_api_key",
            "missing or invalid API key",
        );
    }
    let mut body = Vec::new();
    if let Err(error) = request.as_reader().read_to_end(&mut body) {
        return write_error(
            request,
            400,
            "invalid_request",
            &format!("read body failed: {error}"),
        );
    }
    let Ok(mut payload) = serde_json::from_slice::<Value>(&body) else {
        return write_error(request, 400, "invalid_request", "request body must be JSON");
    };
    if !payload.is_object() {
        return write_error(
            request,
            400,
            "invalid_request",
            "request body must be a JSON object",
        );
    }
    let client_wants_stream = payload
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let requested_model = payload
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if requested_model.is_empty() {
        return write_error(request, 400, "invalid_request", "model is required");
    }
    let upstream_model = resolve_upstream_model(&requested_model);
    // 对外显示名与 Qoder 上游 ID 解耦；旧配置中的 qfmodel/qmodel_38max 仍可继续使用。
    payload["model"] = Value::String(upstream_model.to_string());
    // 上游非流式实现不可靠，这里统一强制 stream:true，非流式由网关聚合。
    payload["stream"] = Value::Bool(true);
    let request_context = add_qoder_request_context(&mut payload);
    let body = serde_json::to_vec(&payload).unwrap_or_default();

    // 鉴权失败或额度耗尽时，按统一账号池顺序逐个尝试。
    let max_attempts = resolve_refresh_tokens().len().max(1);
    for attempt in 0..max_attempts {
        let Some(token) = ensure_token(attempt > 0) else {
            return write_error(
                request,
                503,
                "qoderwork_unavailable",
                "QoderWork 登录凭据不可用",
            );
        };
        let response = chat_client()
            .post(UPSTREAM_CHAT_URL)
            .headers(chat_headers(
                &token.access_token,
                &request_context.request_id,
                &request_context.session_id,
            ))
            .body(body.clone())
            .send();
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                return write_error(
                    request,
                    502,
                    "upstream_connect_failed",
                    &format!("Qoder 上游连接失败: {error}"),
                );
            }
        };
        let status = response.status().as_u16();
        if status == 401 || status == 403 {
            if attempt + 1 < max_attempts {
                clear_cached_token();
                continue;
            }
            return write_error(
                request,
                status,
                "qoderwork_auth_failed",
                "QoderWork 凭据已失效，请重新登录 QoderWork 客户端",
            );
        }
        if status == 402 && attempt == 0 {
            // 账号本身的 refresh token 仍有效，但当前模型额度已耗尽；
            // 让统一账号池继续尝试下一个国内/国际账号。
            let text = response.text().unwrap_or_default();
            logger::log_warn(&format!(
                "[QoderGateway] 当前账号额度不可用，切换下一个账号: {}",
                truncate(&text, 300)
            ));
            clear_cached_token();
            continue;
        }
        if status >= 400 {
            let text = response.text().unwrap_or_default();
            return write_error(
                request,
                status,
                "upstream_error",
                &format!("Qoder 上游返回 HTTP {status}: {}", truncate(&text, 500)),
            );
        }
        logger::log_info(&format!(
            "[QoderWorkGateway] chat model={requested_model} upstream_model={upstream_model} stream={client_wants_stream}"
        ));
        return if client_wants_stream {
            stream_response(request, response)
        } else {
            match aggregate_sse(response) {
                Ok(value) => write_json_response(request, 200, &value),
                Err(error) => write_error(request, 502, "upstream_parse", &error),
            }
        };
    }
    write_error(
        request,
        503,
        "qoderwork_unavailable",
        "QoderWork token refresh failed",
    );
}

/// QoderWork 模型服务要求的请求链路上下文，保持与官方 HTTP 运行时一致。
#[derive(Clone, Debug)]
struct QoderRequestContext {
    request_id: String,
    session_id: String,
}

/// 给 OpenAI 请求补上 QoderWork 运行时自动注入的 metadata。
fn add_qoder_request_context(payload: &mut Value) -> QoderRequestContext {
    if !payload
        .get("metadata")
        .map(Value::is_object)
        .unwrap_or(false)
    {
        payload["metadata"] = json!({});
    }
    let metadata = payload
        .get_mut("metadata")
        .and_then(Value::as_object_mut)
        .expect("metadata must be an object");
    if !metadata
        .get("context")
        .map(Value::is_object)
        .unwrap_or(false)
    {
        metadata.insert("context".into(), json!({}));
    }
    let context = metadata
        .get_mut("context")
        .and_then(Value::as_object_mut)
        .expect("metadata.context must be an object");

    let request_id = context_string(context, "request_id", uuid::Uuid::new_v4().to_string());
    let request_set_id =
        context_string(context, "request_set_id", uuid::Uuid::new_v4().to_string());
    let session_id = context_string(context, "session_id", uuid::Uuid::new_v4().to_string());
    let task_id = context_string(context, "task_id", "common".into());
    // 6 是 QoderWork 官方运行时的 client_type。
    context.insert("client_type".into(), Value::String("6".into()));

    if !metadata
        .get("business")
        .map(Value::is_object)
        .unwrap_or(false)
    {
        metadata.insert(
            "business".into(),
            json!({
                "product": "qoder_work",
                "type": "agent",
                "scene": "assistant",
            }),
        );
    }
    // 官方运行时总是声明需要用量信息，便于上游返回完整结算字段。
    payload["stream_options"] = json!({ "include_usage": true });

    QoderRequestContext {
        request_id,
        session_id,
    }
}

fn context_string(context: &mut Map<String, Value>, key: &str, fallback: String) -> String {
    if let Some(value) = context
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return value.to_string();
    }
    context.insert(key.into(), Value::String(fallback.clone()));
    fallback
}

/// 将对外模型名、旧内部 ID 及常见分隔符/大小写变体映射为上游真实 ID。
fn resolve_upstream_model(model: &str) -> &str {
    let normalized_model = normalize_model_key(model);
    AVAILABLE_MODELS
        .iter()
        .find(|(upstream_id, display_name)| {
            upstream_id.eq_ignore_ascii_case(model)
                || display_name.eq_ignore_ascii_case(model)
                || normalize_model_key(upstream_id) == normalized_model
                || normalize_model_key(display_name) == normalized_model
        })
        .map(|(upstream_id, _)| *upstream_id)
        .unwrap_or(model)
}

/// 删除大小写、空格、点号和连字符差异，兼容客户端自行规范化模型名。
fn normalize_model_key(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// 组装与 QoderWork 官方运行时一致的上游请求头。
fn chat_headers(
    access_token: &str,
    request_id: &str,
    session_id: &str,
) -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    let mut insert = |name: &str, value: String| {
        if let (Ok(name), Ok(value)) = (
            reqwest::header::HeaderName::from_bytes(name.as_bytes()),
            reqwest::header::HeaderValue::from_str(&value),
        ) {
            headers.insert(name, value);
        }
    };
    insert("Authorization", format!("Bearer {access_token}"));
    insert("Content-Type", "application/json".into());
    insert("Accept", "text/event-stream".into());
    insert("X-Request-ID", request_id.into());
    insert("X-Session-ID", session_id.into());
    insert("Cosy-Version", "1.1.26".into());
    insert("Cosy-ClientType", "6".into());
    insert("Cosy-MachineOS", "x86_64_win32".into());
    headers
}

/// SSE 专用客户端：不设总超时，避免长回复被切断。
fn chat_client() -> reqwest::blocking::Client {
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::blocking::Client::builder()
                .timeout(None)
                .connect_timeout(Duration::from_secs(20))
                .build()
                .expect("构建 QoderWork chat 客户端失败")
        })
        .clone()
}

/// 短请求客户端：刷新接口与状态查询使用。
fn json_client() -> reqwest::blocking::Client {
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("构建 QoderWork JSON 客户端失败")
        })
        .clone()
}

/// 读取缓存 token；强制刷新或临近过期时重新从本地凭据刷新。
fn ensure_token(force_refresh: bool) -> Option<CachedToken> {
    let candidates = resolve_refresh_tokens();
    {
        let cached = cached_token().lock().ok()?;
        if let Some(token) = cached.as_ref() {
            if !force_refresh && token.expires_at_ms - TOKEN_REFRESH_SKEW_MS > now_ms() {
                return Some(token.clone());
            }
        }
    }
    if candidates.is_empty() {
        return None;
    }
    let start_index = if force_refresh {
        cached_token()
            .lock()
            .ok()
            .and_then(|cached| cached.as_ref().map(|token| token.candidate_index + 1))
            .unwrap_or(0)
            % candidates.len()
    } else {
        0
    };
    for offset in 0..candidates.len() {
        let candidate_index = (start_index + offset) % candidates.len();
        let Some(refreshed) = refresh_device_token(&candidates[candidate_index]).ok() else {
            continue;
        };
        let token = CachedToken {
            access_token: refreshed.access_token,
            expires_at_ms: refreshed.expires_at_ms,
            candidate_index,
        };
        if let Ok(mut cached) = cached_token().lock() {
            *cached = Some(token.clone());
        }
        return Some(token);
    }
    None
}

/// 统一网关账号池：国内版当前账号优先，其次国际版当前账号，再按账号列表回退。
fn resolve_refresh_tokens() -> Vec<String> {
    let domestic_accounts = crate::modules::qoder_account::list_accounts();
    let international_accounts = crate::modules::qoderwork_account::list_accounts();
    let domestic_current =
        crate::modules::qoder_account::resolve_current_account_id(&domestic_accounts);
    let international_current = crate::modules::qoderwork_account::resolve_current_account_id(
        &international_accounts,
        "qoderwork_intl",
    );
    let mut tokens = Vec::new();
    let accounts = gateway_accounts(&domestic_accounts, &international_accounts);
    // 当前桌面端登录态优先，保证正在使用的 QoderWork 账号先参与请求。
    if let Ok(auth) = read_safe_storage_auth() {
        if let Some(token) = find_refresh_token(&auth) {
            tokens.push(token);
        }
    }
    let mut push_account_token = |account: &GatewayAccount| {
        if let Some(token) = account.refresh_token.as_deref() {
            if !tokens.iter().any(|existing| existing == token) {
                tokens.push(token.to_string());
            }
        }
    };
    for current_id in [domestic_current, international_current]
        .into_iter()
        .flatten()
    {
        if let Some(account) = accounts.iter().find(|account| account.id == current_id) {
            push_account_token(account);
        }
    }
    for account in &accounts {
        push_account_token(account);
    }
    tokens.dedup();
    tokens
}

/// 兼容国内版与国际版账号详情中的 refreshToken / refresh_token 字段。
fn extract_refresh_token(user_info: &Option<Value>) -> Option<String> {
    user_info.as_ref().and_then(find_refresh_token)
}

fn find_refresh_token(value: &Value) -> Option<String> {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                let normalized = key
                    .chars()
                    .filter(|character| character.is_ascii_alphanumeric())
                    .flat_map(char::to_lowercase)
                    .collect::<String>();
                if normalized == "refreshtoken" {
                    if let Some(token) = child
                        .as_str()
                        .map(str::trim)
                        .filter(|token| !token.is_empty())
                    {
                        return Some(token.to_string());
                    }
                }
            }
            map.values().find_map(find_refresh_token)
        }
        Value::Array(items) => items.iter().find_map(find_refresh_token),
        _ => None,
    }
}

/// OAuth 登录或刷新后调用，让下一个请求重新读取账号库。
pub fn clear_cached_token() {
    if let Ok(mut cached) = cached_token().lock() {
        *cached = None;
    }
}

/// 调用 QoderWork 设备 token 刷新接口。
struct RefreshedToken {
    access_token: String,
    expires_at_ms: i64,
}

fn refresh_device_token(refresh_token: &str) -> Result<RefreshedToken, String> {
    let response = json_client()
        .post(DEVICE_TOKEN_REFRESH_URL)
        .json(&json!({ "refresh_token": refresh_token }))
        .send()
        .map_err(|error| format!("refresh request failed: {error}"))?;
    let status = response.status().as_u16();
    let body = response.text().unwrap_or_default();
    if status != 200 {
        return Err(format!(
            "refresh returned HTTP {status}: {}",
            truncate(&body, 300)
        ));
    }
    let value =
        serde_json::from_str::<Value>(&body).map_err(|error| format!("refresh JSON: {error}"))?;
    let access_token = value
        .get("device_token")
        .and_then(Value::as_str)
        .or_else(|| value.get("access_token").and_then(Value::as_str))
        .ok_or("refresh response missing device_token")?
        .to_string();
    let expires_at_ms = parse_expiry_ms(
        value
            .get("expires_at")
            .or_else(|| value.get("expireTime"))
            .unwrap_or(&Value::Null),
    )
    .ok_or("refresh response missing expiry")?;
    Ok(RefreshedToken {
        access_token,
        expires_at_ms,
    })
}

/// 兼容秒/毫秒时间戳与 ISO 字符串。
fn parse_expiry_ms(value: &Value) -> Option<i64> {
    if let Some(number) = value.as_i64() {
        return Some(if number > 1_000_000_000_000 {
            number
        } else {
            number * 1000
        });
    }
    if let Some(number) = value.as_f64() {
        let ms = (number * 1000.0) as i64;
        return Some(if number > 1_000_000_000_000.0 {
            number as i64
        } else {
            ms
        });
    }
    let text = value.as_str()?;
    if let Ok(number) = text.parse::<i64>() {
        return Some(if number > 1_000_000_000_000 {
            number
        } else {
            number * 1000
        });
    }
    chrono::DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|time| time.timestamp_millis())
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// 容器访问基址：供 Docker 里的 sub2api 访问宿主机网关。
/// 固定用 host.docker.internal（Docker Desktop 会自动指向宿主机），因此始终返回。
fn docker_base_url() -> String {
    format!("http://host.docker.internal:{PORT}/v1")
}

fn truncate(value: &str, max: usize) -> String {
    if value.len() <= max {
        value.to_string()
    } else {
        let mut end = max;
        while end > 0 && !value.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &value[..end])
    }
}

/// 读取（或首次生成）本地网关 API Key。
fn resolve_api_key() -> String {
    static KEY_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let _guard = KEY_LOCK.get_or_init(|| Mutex::new(())).lock().ok();
    let Some(data_dir) = data_dir() else {
        return String::new();
    };
    let key_file = data_dir.join(CONFIG_FILE_NAME);
    if let Ok(raw) = std::fs::read_to_string(&key_file) {
        if let Ok(value) = serde_json::from_str::<Value>(&raw) {
            if let Some(key) = value.get("api_key").and_then(Value::as_str) {
                if !key.is_empty() {
                    return key.to_string();
                }
            }
        }
    }
    let key = uuid::Uuid::new_v4().simple().to_string();
    let _ = std::fs::write(
        &key_file,
        serde_json::to_string_pretty(&json!({ "api_key": key })).unwrap_or_default(),
    );
    key
}

fn data_dir() -> Option<PathBuf> {
    crate::modules::account::get_data_dir().ok()
}

/// CORS 预检应答。
fn answer_preflight(request: tiny_http::Request) {
    let mut response = tiny_http::Response::empty(tiny_http::StatusCode(200));
    for header in cors_headers() {
        response = response.with_header(header);
    }
    let _ = request.respond(response);
}

fn cors_headers() -> Vec<tiny_http::Header> {
    vec![
        tiny_http::Header::from_bytes(&b"Access-Control-Allow-Origin"[..], &b"*"[..]).unwrap(),
        tiny_http::Header::from_bytes(
            &b"Access-Control-Allow-Methods"[..],
            &b"GET, POST, OPTIONS"[..],
        )
        .unwrap(),
        tiny_http::Header::from_bytes(
            &b"Access-Control-Allow-Headers"[..],
            &b"Authorization, Content-Type"[..],
        )
        .unwrap(),
    ]
}

fn write_json_response(request: tiny_http::Request, status: u16, value: &Value) {
    let body = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
    let mut response =
        tiny_http::Response::from_data(body).with_status_code(tiny_http::StatusCode(status));
    for header in cors_headers() {
        response = response.with_header(header);
    }
    response.add_header(
        tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap(),
    );
    let _ = request.respond(response);
}

fn write_error(request: tiny_http::Request, status: u16, code: &str, message: &str) {
    write_json_response(
        request,
        status,
        &json!({ "error": { "message": message, "type": "api_error", "code": code } }),
    );
}

/// 读取 QoderWork Electron safeStorage 登录态，返回解密后的 auth JSON。
#[cfg(target_os = "windows")]
fn read_safe_storage_auth() -> Result<Value, String> {
    use base64::Engine as _;

    let app_data = dirs::data_dir().ok_or("找不到 APPDATA 目录")?;
    let qoder_dir = app_data.join("QoderWork");
    let state_file = qoder_dir.join("Local State");
    let auth_file = qoder_dir.join("auth-v2.dat");
    let state = std::fs::read_to_string(&state_file)
        .map_err(|_| format!("QoderWork 未登录或缺少 {}", state_file.display()))?;
    let state_value = serde_json::from_str::<Value>(&state)
        .map_err(|error| format!("Local State JSON 解析失败: {error}"))?;
    let encrypted_key_b64 = state_value
        .pointer("/os_crypt/encrypted_key")
        .and_then(Value::as_str)
        .ok_or("Local State 缺少 os_crypt.encrypted_key")?;
    let encrypted_key = base64::engine::general_purpose::STANDARD
        .decode(encrypted_key_b64)
        .map_err(|error| format!("encrypted_key Base64 解码失败: {error}"))?;
    if encrypted_key.len() <= 5 || &encrypted_key[..5] != b"DPAPI" {
        return Err("encrypted_key 不是 DPAPI 格式".into());
    }
    let aes_key = dpapi_decrypt(&encrypted_key[5..])?;
    if aes_key.len() != 32 {
        return Err(format!("AES key 长度异常: {}", aes_key.len()));
    }
    let encrypted_auth = std::fs::read(&auth_file)
        .map_err(|_| format!("QoderWork 未登录或缺少 {}", auth_file.display()))?;
    let plaintext = decrypt_windows_gcm_v10(&aes_key, &encrypted_auth)?;
    serde_json::from_slice::<Value>(&plaintext)
        .map_err(|error| format!("auth JSON 解析失败: {error}"))
}

#[cfg(not(target_os = "windows"))]
fn read_safe_storage_auth() -> Result<Value, String> {
    Err("QoderWork 网关当前仅在 Windows 支持".into())
}

#[cfg(target_os = "windows")]
fn dpapi_decrypt(encrypted: &[u8]) -> Result<Vec<u8>, String> {
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{CryptUnprotectData, CRYPT_INTEGER_BLOB};

    unsafe {
        let input = CRYPT_INTEGER_BLOB {
            cbData: encrypted.len() as u32,
            pbData: encrypted.as_ptr() as *mut u8,
        };
        let mut output = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: std::ptr::null_mut(),
        };
        CryptUnprotectData(&input, None, None, None, None, 0, &mut output)
            .map_err(|_| "DPAPI CryptUnprotectData 调用失败".to_string())?;
        let result = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
        LocalFree(HLOCAL(output.pbData as *mut _));
        Ok(result)
    }
}

/// 解密 Electron safeStorage v10：3 字节前缀 + 12 字节 nonce + AES-GCM（tag 附在末尾）。
#[cfg(target_os = "windows")]
fn decrypt_windows_gcm_v10(key: &[u8], encrypted: &[u8]) -> Result<Vec<u8>, String> {
    use aes_gcm::aead::generic_array::GenericArray;
    use aes_gcm::aead::Aead;
    use aes_gcm::{Aes256Gcm, KeyInit, Nonce};

    if encrypted.len() < 31 {
        return Err("auth-v2.dat 数据过短".into());
    }
    if &encrypted[..3] != b"v10" {
        return Err("auth-v2.dat 不是 Windows v10 加密格式".into());
    }
    let cipher = Aes256Gcm::new(GenericArray::from_slice(key));
    let nonce = Nonce::from_slice(&encrypted[3..15]);
    cipher
        .decrypt(nonce, &encrypted[15..])
        .map_err(|_| "AES-GCM 解密失败".into())
}

/// 管道读端：把后台泵线程的数据适配为 tiny_http 可读流。
struct PipeReader {
    rx: std::sync::mpsc::Receiver<Vec<u8>>,
    leftover: Vec<u8>,
    offset: usize,
}

impl Read for PipeReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.offset >= self.leftover.len() {
            match self.rx.recv() {
                Ok(chunk) => {
                    self.leftover = chunk;
                    self.offset = 0;
                }
                Err(_) => return Ok(0),
            }
        }
        let remaining = self.leftover.len() - self.offset;
        let count = buf.len().min(remaining);
        buf[..count].copy_from_slice(&self.leftover[self.offset..self.offset + count]);
        self.offset += count;
        Ok(count)
    }
}

/// 先创建泵线程再阻塞响应，避免 tiny_http 阻塞式 respond 造成死锁。
fn stream_response(request: tiny_http::Request, upstream: reqwest::blocking::Response) {
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let reader = PipeReader {
        rx,
        leftover: Vec::new(),
        offset: 0,
    };
    let response = tiny_http::Response::new(
        tiny_http::StatusCode(200),
        vec![
            tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/event-stream"[..]).unwrap(),
            tiny_http::Header::from_bytes(&b"Cache-Control"[..], &b"no-cache"[..]).unwrap(),
            tiny_http::Header::from_bytes(&b"Access-Control-Allow-Origin"[..], &b"*"[..]).unwrap(),
        ],
        reader,
        None::<usize>,
        None::<std::sync::mpsc::Receiver<tiny_http::Header>>,
    );
    std::thread::spawn(move || pump_stream(tx, upstream));
    let _ = request.respond(response);
}

/// 上游是标准 OpenAI SSE，这里只做最低成本透传：遇到 [DONE] 即停止。
fn pump_stream(tx: std::sync::mpsc::Sender<Vec<u8>>, upstream: reqwest::blocking::Response) {
    let mut reader = BufReader::new(upstream);
    let mut line = String::new();
    let mut error_event = false;
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed == "event: error" {
            error_event = true;
            continue;
        }
        if error_event {
            if let Some(payload) = trimmed.strip_prefix("data: ") {
                let error = normalize_sse_error(payload);
                let encoded = format!("data: {}\n\n", error);
                if tx.send(encoded.into_bytes()).is_err() {
                    break;
                }
                let _ = tx.send(b"data: [DONE]\n\n".to_vec());
                break;
            }
            if trimmed.is_empty() {
                continue;
            }
            error_event = false;
        }
        if tx.send(line.clone().into_bytes()).is_err() {
            break;
        }
        if trimmed.starts_with("data: [DONE]") {
            break;
        }
    }
}

/// 把上游 SSE 聚合成普通 chat.completion，供 stream:false 的客户端使用。
fn aggregate_sse(upstream: reqwest::blocking::Response) -> Result<Value, String> {
    let mut reader = BufReader::new(upstream);
    let mut line = String::new();
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut role = "assistant".to_string();
    let mut finish_reason = "stop".to_string();
    let mut id = String::new();
    let mut model = String::new();
    let mut created: i64 = 0;
    let mut usage: Option<Value> = None;
    let mut valid_events = 0u32;
    let mut error_event = false;
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(error) => return Err(format!("read stream failed: {error}")),
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed == "event: error" {
            error_event = true;
            continue;
        }
        if trimmed.starts_with("data: [DONE]") {
            break;
        }
        let Some(payload) = trimmed.strip_prefix("data: ") else {
            if !trimmed.is_empty() {
                error_event = false;
            }
            continue;
        };
        if error_event {
            return Err(format_sse_error(payload));
        }
        let Ok(chunk) = serde_json::from_str::<Value>(payload) else {
            continue;
        };
        error_event = false;
        valid_events += 1;
        if id.is_empty() {
            id = chunk
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
        }
        if model.is_empty() {
            model = chunk
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
        }
        if created == 0 {
            created = chunk.get("created").and_then(Value::as_i64).unwrap_or(0);
        }
        if let Some(item) = chunk.get("usage").filter(|item| !item.is_null()) {
            usage = Some(item.clone());
        }
        for choice in chunk
            .get("choices")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(reason) = choice
                .get("finish_reason")
                .and_then(Value::as_str)
                .filter(|reason| !reason.is_empty())
            {
                finish_reason = reason.to_string();
            }
            let Some(delta) = choice.get("delta").and_then(Value::as_object) else {
                continue;
            };
            if let Some(text) = delta.get("content").and_then(Value::as_str) {
                content.push_str(text);
            }
            if let Some(text) = delta.get("reasoning_content").and_then(Value::as_str) {
                reasoning.push_str(text);
            }
            if let Some(value) = delta.get("role").and_then(Value::as_str) {
                role = value.to_string();
            }
        }
    }
    if valid_events == 0 {
        return Err("upstream stream contained no valid data events".into());
    }
    if id.is_empty() {
        id = format!("chatcmpl-{}", uuid::Uuid::new_v4().simple());
    }
    if created == 0 {
        created = now_ms() / 1000;
    }
    let mut message = json!({ "role": role, "content": content });
    if !reasoning.is_empty() {
        message["reasoning_content"] = Value::String(reasoning);
    }
    let mut response = json!({
        "id": id,
        "object": "chat.completion",
        "created": created,
        "model": model,
        "choices": [{ "index": 0, "message": message, "finish_reason": finish_reason }],
    });
    if let Some(value) = usage {
        response["usage"] = value;
    }
    Ok(response)
}

/// 将 Qoder 的事件错误转换成 OpenAI 客户端可以识别的 SSE error。
fn normalize_sse_error(payload: &str) -> Value {
    match serde_json::from_str::<Value>(payload) {
        Ok(value) if value.get("error").is_some() => value,
        Ok(value) => json!({ "error": value }),
        Err(_) => json!({
            "error": {
                "message": payload,
                "type": "upstream_error",
            }
        }),
    }
}

fn format_sse_error(payload: &str) -> String {
    let error = normalize_sse_error(payload);
    let detail = error.get("error").unwrap_or(&error);
    let code = detail
        .get("code")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(|value| format!(" [{value}]"))
        .unwrap_or_default();
    let message = detail
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("Qoder 上游返回未知错误");
    format!("Qoder 上游错误{code}: {message}")
}
