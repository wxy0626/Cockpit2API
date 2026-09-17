//! WorkBuddy OpenAI 兼容网关（Cockpit 进程内原生实现，无外部子进程）。
//!
//! 端口：7863 = OpenAI 兼容接口（/v1/models /v1/chat/completions）
//!       7864 = 管理接口（/api/status /api/config /api/models /api/accounts）
//! 账号来源：Cockpit 本地 WorkBuddy 账号库（modules::workbuddy_account），
//!          token 刷新复用 refresh_account_token，不落任何独立账号文件。
//! 上游：POST https://copilot.tencent.com/v2/chat/completions（SSE，OpenAI 格式直通）。
//! 协议细节对齐 sidecars/wb2api（Go 参考实现）：请求体改写 + 逐帧规范化 + 错误分级冷却。

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};

use crate::modules::logger;
use crate::modules::workbuddy_account;

/// OpenAI 兼容接口端口
const PORT_OPENAI: u16 = 7863;
/// 管理接口端口（状态 / 配置 / 模型）
const PORT_ADMIN: u16 = 7864;
/// 上游 chat 基址
const CHAT_BASE: &str = "https://copilot.tencent.com";
/// 动态模型列表基址（同 chat 基址）
const MODEL_LIST_PATH: &str = "/console/enterprises/personal/models";
/// 上游 chat 接口路径（SSE）
const UPSTREAM_CHAT_PATH: &str = "/v2/chat/completions";
/// 上游伪装 UA（与 CodeBuddy 官方 CLI 一致）
const CLIENT_UA: &str = "CLI/2.63.2 CodeBuddy/2.63.2";
/// 伪装 Origin/Referer 基址
const ORIGIN_CN: &str = "https://www.codebuddy.cn";
/// 单请求最多换号次数
const MAX_ROTATE: usize = 3;
/// 429/404 软冷却时长
const SOFT_COOLDOWN: Duration = Duration::from_secs(60);
/// 连续 5xx 达到该次数触发 30 分钟熔断
const BREAKER_THRESHOLD: u32 = 3;
/// token 提前刷新窗口（毫秒）：距过期不足 10 分钟先刷新
const REFRESH_SKEW_MS: i64 = 10 * 60 * 1000;
/// 请求体上限 8MB
const BODY_LIMIT: usize = 8 << 20;

/// 宿主是否已启动（防止重复绑定端口）
static STARTED: AtomicBool = AtomicBool::new(false);
/// 轮换计数器（round-robin 选号偏移）
static ROUND_ROBIN: AtomicUsize = AtomicUsize::new(0);

/// 动态模型缓存（1 小时有效，失败 5 分钟负缓存）
struct ModelsCache {
    /// 动态模型信息（id / 上下文窗口 / 最大输出 / 支持档位）
    infos: Vec<ModelInfo>,
    /// 最近一次成功拉取时间
    fetched: Option<Instant>,
    /// 最近一次拉取失败时间（负缓存）
    last_fail: Option<Instant>,
}
static MODELS_CACHE: OnceLock<Mutex<ModelsCache>> = OnceLock::new();

fn models_cache() -> &'static Mutex<ModelsCache> {
    MODELS_CACHE.get_or_init(|| {
        Mutex::new(ModelsCache {
            infos: Vec::new(),
            fetched: None,
            last_fail: None,
        })
    })
}

/// 账号运行态：冷却 / 禁用 / 连续错误计数（内存态，重启即复位）
#[derive(Default, Clone)]
struct AccountRuntime {
    /// 软/硬冷却截止时间（本地时钟）
    cooldown_until: Option<SystemTime>,
    /// 是否禁用（session 死亡，需重新登录）
    disabled: bool,
    /// 连续 5xx 计数（成功清零；达阈值触发熔断）
    err_streak: u32,
}
static ACCOUNT_STATES: OnceLock<Mutex<HashMap<String, AccountRuntime>>> = OnceLock::new();

fn account_states() -> &'static Mutex<HashMap<String, AccountRuntime>> {
    ACCOUNT_STATES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 动态模型信息
#[derive(Clone)]
struct ModelInfo {
    id: String,
    context_window: i64,
    max_tokens: i64,
    /// 该模型支持的 reasoning_effort 档位（空 = 未知，不降级）
    efforts: Vec<String>,
}

/// 上游错误分类（决定账号冷却策略）
#[derive(PartialEq, Clone, Copy, Debug)]
enum ErrKind {
    /// 成功
    None,
    /// 余额不足（402 或关键词）→ 冷却到次日 04:00
    HardCredit,
    /// 429 软限流 → 60 秒
    SoftRate,
    /// 401 + session 失效 → 禁用
    SessionDead,
    /// 404 偶发 → 软冷却，不喂熔断
    NotFound,
    /// 5xx → 喂熔断计数
    Server,
    /// 其他 4xx → 只换号不罚
    Client,
}

/// 余额不足关键词（HTTP 402 或 body 命中即判硬冷却）
const HARD_MARKERS: &[&str] = &[
    "insufficient credit", "no credit", "credit exhausted", "out of credit",
    "quota exceeded", "quota exhaust", "payment required", "credit not enough",
    "not enough credit",
    "积分不足", "额度不足", "余额不足", "积分用完", "额度用尽", "没有积分",
];

/// session 失效关键词（命中即禁用账号）
const SESSION_DEAD_MARKERS: &[&str] = &["Offline user session not found", "12153"];

/// 按 HTTP 状态码 + body 判定错误类别（对齐 Go 版 Classify）
fn classify(status: u16, body: &str) -> ErrKind {
    if status == 402 {
        return ErrKind::HardCredit;
    }
    let lower = body.to_lowercase();
    if HARD_MARKERS.iter().any(|m| lower.contains(&m.to_lowercase()) || body.contains(m)) {
        return ErrKind::HardCredit;
    }
    if SESSION_DEAD_MARKERS.iter().any(|m| body.contains(m)) {
        return ErrKind::SessionDead;
    }
    match status {
        429 => ErrKind::SoftRate,
        404 => ErrKind::NotFound,
        s if s >= 500 => ErrKind::Server,
        s if s >= 400 => ErrKind::Client,
        _ => ErrKind::None,
    }
}

/// ---------------------------------------------------------------------------
/// 启动入口
/// ---------------------------------------------------------------------------

/// 启动原生网关（幂等）：网关是「API 网关」页的服务本体，始终随应用启动；
/// 自动保活开关只决定 token 巡检（workbuddy_keepalive），与网关启停解耦。
pub fn ensure_started() {
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(|| start_port(PORT_OPENAI, handle_openai_request));
    std::thread::spawn(|| start_port(PORT_ADMIN, handle_admin_request));
}

/// 绑定并服务一个端口；绑定失败（端口被占）不放弃，转入接管监视：
/// 覆盖「上一实例正在退出」与「同机另一实例（dev/release）先占端口」两类竞态，
/// 否则本次会话网关将一直不可用，表现为外部客户端首次接入全部失败。
fn start_port(port: u16, handler: fn(tiny_http::Request)) {
    match tiny_http::Server::http(format!("0.0.0.0:{port}")) {
        Ok(server) => {
            logger::log_info(&format!("[WorkBuddyGateway] 端口 {port} 已监听"));
            serve_forever(server, handler);
        }
        Err(error) => {
            logger::log_warn(&format!(
                "[WorkBuddyGateway] 绑定 {port} 失败（{error}），转入接管监视等待端口释放"
            ));
            spawn_takeover_monitor(port, handler);
        }
    }
}

/// 接管监视：每 5 秒重试绑定（前 2 分钟高频，覆盖实例退出窗口期；之后每分钟一次兜底），
/// 端口释放后立即接管并开始服务。
fn spawn_takeover_monitor(port: u16, handler: fn(tiny_http::Request)) {
    std::thread::spawn(move || {
        for attempt in 0.. {
            std::thread::sleep(Duration::from_secs(if attempt < 24 { 5 } else { 60 }));
            match tiny_http::Server::http(format!("0.0.0.0:{port}")) {
                Ok(server) => {
                    logger::log_info(&format!("[WorkBuddyGateway] 端口 {port} 已释放，接管监听成功"));
                    serve_forever(server, handler);
                    return;
                }
                Err(_) => continue,
            }
        }
    });
}

/// tiny_http 服务主循环：逐请求一线程分发（chat 为长连接 SSE，不能串行阻塞）
fn serve_forever(server: tiny_http::Server, handler: fn(tiny_http::Request)) {
    for request in server.incoming_requests() {
        std::thread::spawn(move || handler(request));
    }
}

/// ---------------------------------------------------------------------------
/// API Key 存取（应用数据目录 workbuddy_gateway.json，兼容旧 runtime/config.json 种子）
/// ---------------------------------------------------------------------------

/// 读取（或初始化）网关 API Key：应用配置 → 旧网关配置种子 → 随机生成。
/// 加锁串行化首次生成：并发首请求各自生成不同 key 会互相顶掉，客户端随机 401。
fn resolve_api_key() -> String {
    static KEY_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let _guard = KEY_LOCK.get_or_init(|| Mutex::new(())).lock().ok();
    let data_dir = match crate::modules::account::get_data_dir() {
        Ok(dir) => dir,
        Err(_) => return String::new(),
    };
    let key_file = data_dir.join("workbuddy_gateway.json");
    if let Ok(raw) = std::fs::read_to_string(&key_file) {
        if let Ok(value) = serde_json::from_str::<Value>(&raw) {
            if let Some(key) = value.get("api_key").and_then(Value::as_str) {
                if !key.is_empty() {
                    return key.to_string();
                }
            }
        }
    }
    // 兼容种子：项目内旧网关配置里的 api_key（保证 sub2api 无感切换）
    let mut seeded = String::new();
    if let Ok(exe) = std::env::current_exe() {
        for ancestor in exe.ancestors().skip(1) {
            let candidate = ancestor
                .join("sidecars")
                .join("wb2api")
                .join("runtime")
                .join("config.json");
            if let Ok(raw) = std::fs::read_to_string(&candidate) {
                if let Ok(value) = serde_json::from_str::<Value>(&raw) {
                    if let Some(key) = value.get("api_key").and_then(Value::as_str) {
                        seeded = key.to_string();
                        break;
                    }
                }
            }
        }
    }
    let key = if !seeded.is_empty() {
        seeded
    } else {
        uuid::Uuid::new_v4().simple().to_string()
    };
    let _ = std::fs::write(
        &key_file,
        serde_json::to_string_pretty(&json!({ "api_key": key })).unwrap_or_default(),
    );
    key
}

/// ---------------------------------------------------------------------------
/// OpenAI 端（7863）请求分发
/// ---------------------------------------------------------------------------

/// OpenAI 端请求处理：模型列表 / 对话补全 / 健康探活
fn handle_openai_request(mut request: tiny_http::Request) {
    // CORS 预检直接放行
    if *request.method() == tiny_http::Method::Options {
        return answer_preflight(request);
    }
    let url = request.url().split('?').next().unwrap_or("").to_string();
    let method = request.method().clone();
    match (&method, url.as_str()) {
        (tiny_http::Method::Post, "/v1/chat/completions") => handle_chat(request, InboundProtocol::Chat),
        (tiny_http::Method::Post, "/v1/responses") => handle_chat(request, InboundProtocol::Responses),
        (tiny_http::Method::Get, "/v1/models") => {
            if check_auth(&request).is_err() {
                return write_openai_error(request, 401, "invalid_api_key", "missing or invalid API key");
            }
            // model_list() 已含 {object:"list", data:[...]} 外壳，勿再包裹
            let list = model_list();
            write_json_response(request, 200, &list);
        }
        (tiny_http::Method::Get, "/healthz") => {
            let (total, healthy) = pool_counts();
            write_json_response(request, 200, &json!({ "total": total, "healthy": healthy }));
        }
        _ => write_openai_error(request, 404, "not_found", "unknown endpoint"),
    }
}

/// 鉴权：Bearer api_key 比对（key 为空 = 不鉴权）
fn check_auth(request: &tiny_http::Request) -> Result<(), ()> {
    let api_key = resolve_api_key();
    if api_key.is_empty() {
        return Ok(());
    }
    let ok = request
        .headers()
        .iter()
        .any(|h| {
            h.field.equiv("Authorization")
                && h.value.as_str().strip_prefix("Bearer ") == Some(api_key.as_str())
        });
    if ok { Ok(()) } else { Err(()) }
}

/// CORS 允许头：面板 WebView 经跨域 fetch 访问网关，响应必须携带，否则浏览器拦截
fn cors_headers() -> Vec<tiny_http::Header> {
    vec![
        tiny_http::Header::from_bytes(&b"Access-Control-Allow-Origin"[..], &b"*"[..]).unwrap(),
        tiny_http::Header::from_bytes(&b"Access-Control-Allow-Methods"[..], &b"GET, POST, OPTIONS"[..]).unwrap(),
        tiny_http::Header::from_bytes(&b"Access-Control-Allow-Headers"[..], &b"Authorization, Content-Type"[..]).unwrap(),
    ]
}

/// 预检请求直接应答 200 + CORS 头
fn answer_preflight(request: tiny_http::Request) {
    let response = tiny_http::Response::empty(tiny_http::StatusCode(200))
        .with_header(tiny_http::Header::from_bytes(&b"Access-Control-Allow-Origin"[..], &b"*"[..]).unwrap())
        .with_header(tiny_http::Header::from_bytes(&b"Access-Control-Allow-Methods"[..], &b"GET, POST, OPTIONS"[..]).unwrap())
        .with_header(tiny_http::Header::from_bytes(&b"Access-Control-Allow-Headers"[..], &b"Authorization, Content-Type"[..]).unwrap());
    let _ = request.respond(response);
}

/// 写 JSON 响应（附带一行请求日志，网关行为可从应用日志直接核对）
fn write_json_response(request: tiny_http::Request, status: u16, value: &Value) {
    let method = request.method().clone();
    let url = request.url().split('?').next().unwrap_or("").to_string();
    let body = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
    let mut response = tiny_http::Response::from_data(body)
        .with_status_code(tiny_http::StatusCode(status));
    for header in cors_headers() {
        response = response.with_header(header);
    }
    response.add_header(tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap());
    let _ = request.respond(response);
    logger::log_info(&format!("[WorkBuddyGateway] {method} {url} → {status}"));
}

/// 写 OpenAI 错误格式响应
fn write_openai_error(request: tiny_http::Request, status: u16, code: &str, msg: &str) {
    write_json_response(
        request,
        status,
        &json!({ "error": { "message": msg, "type": "api_error", "code": code } }),
    );
}

/// 静态模型表（动态接口失败时的回退，对齐 Go 版 staticModels）
const STATIC_MODELS: &[&str] = &[
    "glm-5.2", "glm-5.1", "glm-5v-turbo", "kimi-k2.7", "minimax-m3",
    "hy3", "hy3-preview", "hy3-preview-agent", "deepseek-v4-pro", "deepseek-v4-flash",
];

/// 模型列表：优先动态（缓存 1h），失败回退静态表
fn model_list() -> Value {
    let entries = match fetch_dynamic_models() {
        Some(infos) if !infos.is_empty() => infos
            .iter()
            .map(|m| {
                json!({
                    "id": m.id,
                    "object": "model",
                    "created": 1753600000i64,
                    "owned_by": "workbuddy",
                    "context_length": if m.context_window > 0 { m.context_window } else { 131072 },
                    "max_output_tokens": m.max_tokens,
                })
            })
            .collect::<Vec<_>>(),
        _ => STATIC_MODELS
            .iter()
            .map(|id| json!({ "id": id, "object": "model", "created": 1753600000i64, "owned_by": "workbuddy", "context_length": 131072 }))
            .collect::<Vec<_>>(),
    };
    json!({ "object": "list", "data": entries })
}

/// 从可用账号拉取动态模型列表（缓存 1h；失败 5 分钟负缓存）。
/// 逐个尝试候选账号（最多 3 个）：临近过期的 token 先刷新再请求，
/// 单账号失败换下一个 —— 外部客户端首次拉模型就能拿到完整动态列表，
/// 不再因单个失效账号回落静态表、直到面板进一次网关页才被修复。
fn fetch_dynamic_models() -> Option<Vec<ModelInfo>> {
    {
        let cache = models_cache().lock().ok()?;
        if let Some(fetched) = cache.fetched {
            if !cache.infos.is_empty() && fetched.elapsed() < Duration::from_secs(3600) {
                return Some(cache.infos.clone());
            }
        }
        if let Some(last_fail) = cache.last_fail {
            if last_fail.elapsed() < Duration::from_secs(300) {
                return None;
            }
        }
    }
    let mut tried: Vec<String> = Vec::new();
    for _ in 0..3 {
        let Some(account) = pick_account(&tried) else { break };
        tried.push(account.id.clone());
        // 与 chat 链路一致：临近过期先刷新，避免拿失效 token 拉模型必然 401
        let needs_refresh = account
            .expires_at
            .map(|ms| ms - REFRESH_SKEW_MS < now_ms())
            .unwrap_or(false);
        let fresh = if needs_refresh {
            match tauri::async_runtime::block_on(workbuddy_account::refresh_account_token(&account.id)) {
                Ok(refreshed) => refreshed,
                Err(_) => continue,
            }
        } else {
            account
        };
        let response = json_client()
            .get(format!("{CHAT_BASE}{MODEL_LIST_PATH}"))
            .header("Authorization", format!("Bearer {}", fresh.access_token))
            .header("Accept", "application/json")
            .header("Origin", ORIGIN_CN)
            .header("Referer", format!("{ORIGIN_CN}/"))
            .header("User-Agent", CLIENT_UA)
            .timeout(Duration::from_secs(60))
            .send();
        let response = match response {
            Ok(response) => response,
            Err(_) => continue,
        };
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().unwrap_or_default();
            note_error(&fresh.id, classify(status, &body));
            continue;
        }
        let Ok(value) = response.json::<Value>() else { continue };
        match parse_model_infos(&value) {
            Some(infos) if !infos.is_empty() => {
                let mut cache = models_cache().lock().ok()?;
                cache.infos = infos.clone();
                cache.fetched = Some(Instant::now());
                cache.last_fail = None;
                return Some(infos);
            }
            _ => continue,
        }
    }
    mark_models_fail();
    None
}

/// 解析上游模型接口响应：取 cli agent 可用模型及其上下文/输出上限/effort 档位
fn parse_model_infos(value: &Value) -> Option<Vec<ModelInfo>> {
    let models = value.pointer("/data/models")?.as_array()?.clone();
    let cli_ids = value
        .pointer("/data/agents")
        .and_then(Value::as_array)
        .and_then(|agents| {
            agents
                .iter()
                .find(|a| a.get("name").and_then(Value::as_str) == Some("cli"))
                .and_then(|a| a.get("models").and_then(Value::as_array).cloned())
        })?;
    let find = |id: &str| models.iter().find(|m| m.get("id").and_then(Value::as_str) == Some(id)).cloned();
    let mut infos = Vec::new();
    for id in cli_ids.iter().filter_map(|v| v.as_str()) {
        let Some(model) = find(id) else { continue };
        if model.get("disabled").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        infos.push(ModelInfo {
            id: id.to_string(),
            context_window: model.get("maxInputTokens").and_then(Value::as_i64).unwrap_or(0),
            max_tokens: model.get("maxOutputTokens").and_then(Value::as_i64).unwrap_or(0),
            efforts: model
                .pointer("/reasoning/supportedEfforts")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect())
                .unwrap_or_default(),
        });
    }
    Some(infos)
}

/// 标记模型拉取失败（进入 5 分钟负缓存）
fn mark_models_fail() {
    if let Ok(mut cache) = models_cache().lock() {
        cache.last_fail = Some(Instant::now());
    }
}

/// ---------------------------------------------------------------------------
/// 对话补全：选号轮换 + 请求体改写 + 上游转发 + SSE 回传
/// ---------------------------------------------------------------------------

/// 入站协议：决定请求体翻译方向与响应回写格式。
/// 上游始终是 Chat Completions，Responses 通过翻译层复用同一条转发链路。
#[derive(Clone, Copy, PartialEq, Eq)]
enum InboundProtocol {
    /// OpenAI Chat Completions（/v1/chat/completions）
    Chat,
    /// OpenAI Responses（/v1/responses），入站翻译为 chat 后转发
    Responses,
}

/// 对话补全主流程（对齐 Go 版 chatCompletions 轮换循环）
///
/// `protocol` 指定入站协议：Chat 直接透传语义；Responses 先在入站侧翻译为
/// chat 请求体，出站侧再把结果包装回 Responses 结构。选号/刷新/转发/冷却逻辑
/// 对两种协议完全共用。
fn handle_chat(mut request: tiny_http::Request, protocol: InboundProtocol) {
    if check_auth(&request).is_err() {
        return write_openai_error(request, 401, "invalid_api_key", "missing or invalid API key");
    }
    // 读取请求体（限 8MB）
    let mut body = Vec::new();
    if request.as_reader().take(BODY_LIMIT as u64).read_to_end(&mut body).is_err() {
        return write_openai_error(request, 400, "invalid_request", "read body failed");
    }
    // Responses 入站：翻译为 chat 请求体后再走统一链路；翻译失败即报 400
    // 工具命名空间映射：Responses 入站的 namespace 容器被摊平后，回传 function_call
    // 时需按此映射补回 namespace，客户端才能按「namespace+工具名」匹配执行器
    let mut tool_ns_map: ToolNamespaceMap = ToolNamespaceMap::new();
    let body = if protocol == InboundProtocol::Responses {
        match responses_to_chat(&body) {
            Ok((translated, ns_map)) => {
                tool_ns_map = ns_map;
                translated
            }
            Err(error) => {
                return write_openai_error(request, 400, "invalid_request", &error);
            }
        }
    } else {
        body
    };
    // 是否以流式回写客户端：Chat 尊重 stream 字段（缺省非流式）；
    // Responses 缺省即流式（对齐官方行为）。
    // 请求体不是 JSON 对象时直接 400：静默透传会被上游以
    // "Non-stream chat request is not supported" 之类的晦涩错误拒绝，还白白轮换账号。
    let Some(body_value) = serde_json::from_slice::<Value>(&body).ok().filter(Value::is_object) else {
        return write_openai_error(request, 400, "invalid_request", "request body must be a JSON object");
    };
    let client_wants_stream = body_value
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(protocol == InboundProtocol::Responses);
    let model_name = body_value
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("-")
        .to_string();

    // 单请求轮换：最多 MAX_ROTATE 次，tried 防重复选号
    let mut tried: Vec<String> = Vec::new();
    let mut last_error = String::new();
    for _ in 0..MAX_ROTATE {
        let Some(account) = pick_account(&tried) else {
            break;
        };
        tried.push(account.id.clone());

        // token 临近过期 → 先刷新（复用 Cockpit 账号库的刷新能力，成功即持久化）
        let needs_refresh = account
            .expires_at
            .map(|ms| ms - REFRESH_SKEW_MS < now_ms())
            .unwrap_or(false);
        let fresh = if needs_refresh {
            match tauri::async_runtime::block_on(workbuddy_account::refresh_account_token(&account.id)) {
                Ok(refreshed) => refreshed,
                Err(error) => {
                    last_error = format!("refresh failed: {error}");
                    note_error(&account.id, ErrKind::SessionDead);
                    continue;
                }
            }
        } else {
            account.clone()
        };

        // 转发上游
        let payload = prepare_body(&body);
        let response = chat_client().post(format!("{CHAT_BASE}{UPSTREAM_CHAT_PATH}"))
            .headers(chat_headers(&fresh))
            .body(payload)
            .send();
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                // 传输层抖动：换号不喂熔断
                last_error = format!("transport: {error}");
                logger::log_warn(&format!(
                    "[WorkBuddyGateway] chat 账号 {} 上游连接失败，换号重试: {error}",
                    fresh.uid.as_deref().unwrap_or("-")
                ));
                continue;
            }
        };
        let status = response.status().as_u16();
        if status >= 400 {
            let body_text = response.text().unwrap_or_default();
            let kind = classify(status, &body_text);
            last_error = format!("upstream {kind:?} (http {status}): {}", truncate(&body_text, 200));
            logger::log_warn(&format!(
                "[WorkBuddyGateway] chat 账号 {} 上游失败（{kind:?} http {status}），换号重试",
                fresh.uid.as_deref().unwrap_or("-")
            ));
            note_error(&fresh.id, kind);
            continue;
        }
        note_success(&fresh.id);
        logger::log_info(&format!(
            "[WorkBuddyGateway] chat model={model_name} uid={} attempt={}/{}",
            fresh.uid.as_deref().unwrap_or("-"),
            tried.len(),
            MAX_ROTATE
        ));

        if client_wants_stream {
            return match protocol {
                InboundProtocol::Chat => stream_response(request, response),
                InboundProtocol::Responses => {
                    stream_responses(request, response, tool_ns_map.clone())
                }
            };
        }
        // 非流式：聚合 SSE 为单个 chat.completion（Responses 再包装为 Responses 结构）
        return match aggregate_sse(response) {
            Ok(value) => match protocol {
                InboundProtocol::Chat => write_json_response(request, 200, &value),
                InboundProtocol::Responses => {
                    write_json_response(request, 200, &chat_to_responses(&value))
                }
            },
            Err(error) => write_openai_error(request, 502, "upstream_parse", &error),
        };
    }
    let message = if last_error.is_empty() {
        "all accounts unavailable (cooling/disabled)".to_string()
    } else {
        format!("all accounts unavailable (cooling/disabled): {last_error}")
    };
    logger::log_warn(&format!("[WorkBuddyGateway] chat model={model_name} 无可用账号: {message}"));
    write_openai_error(request, 503, "no_healthy_account", &message);
}

/// 组装上游 chat 请求头（对齐 Go 版 ChatHeaders，缺省字段用 X-No-* 约定）
fn chat_headers(account: &crate::models::workbuddy::WorkbuddyAccount) -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    let mut insert = |name: &str, value: String| {
        if let (Ok(name), Ok(value)) = (
            reqwest::header::HeaderName::from_bytes(name.as_bytes()),
            reqwest::header::HeaderValue::from_str(&value),
        ) {
            headers.insert(name, value);
        }
    };
    insert("Content-Type", "application/json".into());
    insert("Accept", "application/json, text/plain, */*".into());
    insert("X-Requested-With", "XMLHttpRequest".into());
    insert("Origin", ORIGIN_CN.into());
    insert("Referer", format!("{ORIGIN_CN}/"));
    insert("User-Agent", CLIENT_UA.into());
    if account.access_token.is_empty() {
        insert("X-No-Authorization", "1".into());
    } else {
        insert("Authorization", format!("Bearer {}", account.access_token));
    }
    match account.uid.as_deref().filter(|s| !s.is_empty()) {
        Some(uid) => insert("X-User-Id", uid.to_string()),
        None => insert("X-No-User-Id", "1".into()),
    }
    match account.enterprise_id.as_deref().filter(|s| !s.is_empty()) {
        Some(eid) => insert("X-Enterprise-Id", eid.to_string()),
        None => insert("X-No-Enterprise-Id", "1".into()),
    }
    match account.domain.as_deref().filter(|s| !s.is_empty()) {
        Some(domain) => insert("X-Domain", domain.to_string()),
        None => insert("X-No-Department-Info", "1".into()),
    }
    insert("X-Product", "SaaS".into());
    headers
}

/// JSON 专用客户端（120 秒总超时）
fn json_client() -> reqwest::blocking::Client {
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(120))
                .build()
                .expect("构建 JSON 客户端失败")
        })
        .clone()
}

/// chat SSE 专用客户端（无总超时：流式时长不受限）
fn chat_client() -> reqwest::blocking::Client {
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::blocking::Client::builder()
                .timeout(None)
                .build()
                .expect("构建 chat 客户端失败")
        })
        .clone()
}

/// 当前毫秒时间戳
fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// 探测本机局域网 IPv4（供容器/局域网访问），5 分钟缓存：
/// Windows 下解析 ipconfig（GUI 进程派生控制台程序必须 CREATE_NO_WINDOW，
/// 否则面板每次拉取 /api/config 都会闪现控制台窗口），跳过代理 TUN 伪 IP（198.18.x）/
/// 链路本地/虚拟交换机，优先 192.168.x；其他平台或解析失败时回退 UDP 路由法。
fn local_lan_ip() -> Option<String> {
    static CACHE: OnceLock<Mutex<Option<(Instant, Option<String>)>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    if let Ok(guard) = cache.lock() {
        if let Some((at, ip)) = guard.as_ref() {
            if at.elapsed() < Duration::from_secs(300) {
                return ip.clone();
            }
        }
    }
    let ip = detect_lan_ip();
    if let Ok(mut guard) = cache.lock() {
        *guard = Some((Instant::now(), ip.clone()));
    }
    ip
}

/// 实际探测局域网 IPv4
fn detect_lan_ip() -> Option<String> {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        if let Ok(output) = std::process::Command::new("ipconfig")
            .creation_flags(CREATE_NO_WINDOW)
            .output()
        {
            let text = String::from_utf8_lossy(&output.stdout);
            let mut best: Option<(String, u8)> = None;
            for line in text.lines().filter(|line| line.contains("IPv4")) {
                for ip in extract_ipv4(line) {
                    if let Some(rank) = lan_candidate_rank(&ip) {
                        if best.as_ref().map(|(_, r)| rank < *r).unwrap_or(true) {
                            best = Some((ip, rank));
                        }
                    }
                }
            }
            if let Some((ip, _)) = best {
                return Some(ip);
            }
        }
    }
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    let ip = socket.local_addr().ok()?.ip().to_string();
    lan_candidate_rank(&ip).map(|_| ip)
}

/// 判断 IPv4 是否为可对外展示的局域网地址，返回优先级（越小越优先）
fn lan_candidate_rank(ip: &str) -> Option<u8> {
    let octets: Vec<u32> = ip.split('.').filter_map(|s| s.parse().ok()).collect();
    if octets.len() != 4 {
        return None;
    }
    let (a, b) = (octets[0], octets[1]);
    match (a, b) {
        (192, 168) => Some(0),     // 家用/办公局域网，最优先
        (10, _) => Some(1),        // 内网大段
        (172, 16..=31) => Some(2), // 可能是 WSL/Docker 虚拟交换机，次优先
        _ => None,                 // 排除：198.18.x（代理 TUN 伪 IP）、169.254.x、127.x、组播等
    }
}

/// 从一行文本中提取形如 x.x.x.x 的 IPv4 字符串
fn extract_ipv4(line: &str) -> Vec<String> {
    line.split(|c: char| !c.is_ascii_digit() && c != '.')
        .filter(|token| {
            let parts: Vec<&str> = token.split('.').collect();
            parts.len() == 4
                && parts.iter().all(|p| !p.is_empty() && p.len() <= 3 && p.chars().all(|c| c.is_ascii_digit()))
        })
        .map(String::from)
        .collect()
}

/// 截断字符串（日志用）
fn truncate(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.len() > max { format!("{}...", &text[..max]) } else { text.to_string() }
}

/// ---------------------------------------------------------------------------
/// 选号与冷却策略
/// ---------------------------------------------------------------------------

/// 轮换选号：跳过冷却 / 禁用 / 已尝试的账号；成功号记一条统计
fn pick_account(tried: &[String]) -> Option<crate::models::workbuddy::WorkbuddyAccount> {
    let accounts = workbuddy_account::list_accounts();
    if accounts.is_empty() {
        return None;
    }
    let now = SystemTime::now();
    let states = account_states().lock().ok()?;
    let candidates: Vec<_> = accounts
        .into_iter()
        .filter(|a| !tried.iter().any(|t| t == &a.id))
        .filter(|a| {
            match states.get(&a.id) {
                Some(state) => !state.disabled && !state.cooldown_until.map(|t| t > now).unwrap_or(false),
                None => true,
            }
        })
        .collect();
    drop(states);
    if candidates.is_empty() {
        return None;
    }
    let offset = ROUND_ROBIN.fetch_add(1, Ordering::Relaxed) % candidates.len();
    Some(candidates[offset].clone())
}

/// 成功：清零连续错误计数
fn note_success(account_id: &str) {
    if let Ok(mut states) = account_states().lock() {
        let state = states.entry(account_id.to_string()).or_default();
        state.err_streak = 0;
        state.cooldown_until = None;
    }
}

/// 失败：按错误分类施加冷却 / 禁用 / 熔断（对齐 Go 版 applyErrorPolicy）
fn note_error(account_id: &str, kind: ErrKind) {
    if let Ok(mut states) = account_states().lock() {
        let state = states.entry(account_id.to_string()).or_default();
        match kind {
            ErrKind::HardCredit => {
                // 余额不足：冷却到次日 04:00（等签到恢复）
                state.cooldown_until = Some(next_4am());
            }
            ErrKind::SoftRate | ErrKind::NotFound => {
                state.cooldown_until = Some(SystemTime::now() + SOFT_COOLDOWN);
            }
            ErrKind::SessionDead => {
                state.disabled = true;
            }
            ErrKind::Server => {
                state.err_streak += 1;
                if state.err_streak >= BREAKER_THRESHOLD {
                    state.cooldown_until = Some(SystemTime::now() + Duration::from_secs(30 * 60));
                }
            }
            _ => {}
        }
    }
}

/// 计算下一个本地时间 04:00（WorkBuddy 主场景按 UTC+8 固定偏移，误差仅影响冷却到点时刻）
fn next_4am() -> SystemTime {
    let offset = 8 * 3600i64;
    let local_now = now_ms() / 1000 + offset;
    let local_midnight = local_now - local_now % 86400;
    let mut target = local_midnight + 4 * 3600;
    if local_now >= target {
        target += 86400;
    }
    UNIX_EPOCH + Duration::from_secs((target - offset) as u64)
}

/// 统计池子概况：(总数, 健康数)
fn pool_counts() -> (usize, usize) {
    let accounts = workbuddy_account::list_accounts();
    let now = SystemTime::now();
    let states = account_states().lock().ok();
    let total = accounts.len();
    let healthy = accounts
        .iter()
        .filter(|a| match states.as_ref().and_then(|s| s.get(&a.id).cloned()) {
            Some(state) => !state.disabled && !state.cooldown_until.map(|t| t > now).unwrap_or(false),
            None => true,
        })
        .count();
    (total, healthy)
}

/// ---------------------------------------------------------------------------
/// 请求体改写（stream / tool_choice / developer→system / 指纹脱敏 / effort 降级）
/// ---------------------------------------------------------------------------

/// 出站请求体单 pass 改写（对齐 Go 版 PrepareBodyOptWithEfforts）
fn prepare_body(src: &[u8]) -> Vec<u8> {
    let Ok(mut obj) = serde_json::from_slice::<Map<String, Value>>(src) else {
        return src.to_vec();
    };
    // 上游拒绝非流式，强制 stream:true
    obj.insert("stream".into(), Value::Bool(true));
    normalize_tool_choice(&mut obj);
    normalize_roles(&mut obj);
    normalize_reasoning_effort(&mut obj);
    sanitize_messages(&mut obj);
    serde_json::to_vec(&Value::Object(obj)).unwrap_or_else(|_| src.to_vec())
}

/// tool_choice 归一化（上游该字段是 string；对象形式会 400 code=11101）
fn normalize_tool_choice(obj: &mut Map<String, Value>) {
    let Some(tc) = obj.get("tool_choice").cloned() else { return };
    let suppress = |obj: &mut Map<String, Value>| {
        obj.remove("tools");
        obj.remove("functions");
    };
    match tc {
        Value::String(text) if text.eq_ignore_ascii_case("none") => {
            obj.remove("tool_choice");
            suppress(obj);
        }
        Value::Object(map) => {
            let typ = map.get("type").and_then(Value::as_str).unwrap_or("").trim().to_lowercase();
            match typ.as_str() {
                "none" => {
                    obj.remove("tool_choice");
                    suppress(obj);
                }
                "auto" | "required" => {
                    obj.insert("tool_choice".into(), Value::String(typ));
                }
                "function" => {
                    let name = map
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(Value::as_str)
                        .or_else(|| map.get("name").and_then(Value::as_str))
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    obj.insert(
                        "tool_choice".into(),
                        Value::String(if name.is_empty() { "auto".into() } else { name }),
                    );
                }
                _ => {
                    obj.remove("tool_choice");
                }
            }
        }
        _ => {
            obj.remove("tool_choice");
        }
    }
}

/// developer 角色归一为 system（上游 role 白名单不含 developer，命中即 400）
fn normalize_roles(obj: &mut Map<String, Value>) {
    if let Some(Value::Array(messages)) = obj.get_mut("messages") {
        for message in messages.iter_mut() {
            if message.get("role").and_then(Value::as_str).map(|r| r.trim().eq_ignore_ascii_case("developer")).unwrap_or(false) {
                if let Some(map) = message.as_object_mut() {
                    map.insert("role".into(), Value::String("system".into()));
                }
            }
        }
    }
}

/// effort 档位从低到高的排序值
fn effort_rank(text: &str) -> Option<i32> {
    match text.trim().to_lowercase().as_str() {
        "off" => Some(0),
        "minimal" => Some(1),
        "low" => Some(2),
        "medium" => Some(3),
        "high" => Some(4),
        "xhigh" => Some(5),
        "max" => Some(6),
        _ => None,
    }
}

/// 按模型 supportedEfforts 降级 reasoning_effort（未知模型 / 未携带字段一律透传）
fn normalize_reasoning_effort(obj: &mut Map<String, Value>) {
    let efforts: HashMap<String, Vec<String>> = models_cache()
        .lock()
        .map(|cache| {
            cache
                .infos
                .iter()
                .filter(|m| !m.efforts.is_empty())
                .map(|m| (m.id.clone(), m.efforts.clone()))
                .collect()
        })
        .unwrap_or_default();
    if efforts.is_empty() {
        return;
    }
    let model = obj.get("model").and_then(Value::as_str).unwrap_or("").to_string();
    let Some(supported) = efforts.get(&model) else { return };
    let key = if obj.contains_key("reasoning_effort") {
        "reasoning_effort"
    } else if obj.contains_key("reasoningEffort") {
        "reasoningEffort"
    } else {
        return;
    };
    let Some(requested) = obj.get(key).and_then(Value::as_str) else { return };
    let Some(request_idx) = effort_rank(&requested.trim().to_lowercase()) else { return };
    // 在 ≤请求档位的支持档里选最高档
    let mut best: Option<(&String, i32)> = None;
    for effort in supported {
        if let Some(idx) = effort_rank(effort) {
            if idx <= request_idx && best.map(|(_, b)| idx > b).unwrap_or(true) {
                best = Some((effort, idx));
            }
        }
    }
    if let Some((effort, _)) = best {
        if !effort.eq_ignore_ascii_case(requested.trim()) {
            obj.insert(key.into(), Value::String(effort.clone()));
        }
        return;
    }
    // 支持档全部高于请求档：取最低支持档
    let lowest = supported
        .iter()
        .filter_map(|e| effort_rank(e).map(|idx| (e, idx)))
        .min_by_key(|(_, idx)| *idx)
        .map(|(e, _)| e.clone());
    if let Some(effort) = lowest {
        obj.insert(key.into(), Value::String(effort));
    }
}

/// 指纹特征预检关键词（命中才进入净化，普通请求零开销）
const SANITIZE_FEATURES: &[&str] = &[
    "x-anthropic-billing-header",
    "cc_entrypoint=",
    "You are Claude Code",
    "Main branch (",
];

/// 净化改写对（每句只改一个词，语义不变）
const SANITIZE_REWRITES: &[(&str, &str)] = &[
    (
        "You are Claude Code, Anthropic's official CLI for Claude, running within the Claude Agent SDK.",
        "You are a coding assistant running within an agent SDK.",
    ),
    (
        "You are Claude Code, Anthropic's official CLI for Claude.",
        "You are Claude Code, Anthropic's official CLI tool for Claude.",
    ),
    (
        "Main branch (you will usually use this for PRs)",
        "Default branch (you will usually use this for PRs)",
    ),
];

/// header 型指纹剥离正则（整段删除）
fn sanitize_hdr_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"(?i)x-anthropic-billing-header:[^;\n]*;?\s*").expect("有效正则"))
}

/// 尾随裸键值剥离正则（cc_xxx=...; 循环清理）
fn sanitize_kv_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"(?i)\bcc_[a-z0-9_]+=[^;\n]*;?\s*").expect("有效正则"))
}

/// 单段文本净化：预检不中 → 原串返回（零开销）
fn sanitize_text(text: &str) -> String {
    let hit = SANITIZE_FEATURES.iter().any(|f| text.contains(f)) || sanitize_hdr_regex().is_match(text);
    if !hit {
        return text.to_string();
    }
    let mut out = text.to_string();
    for (from, to) in SANITIZE_REWRITES {
        out = out.replace(from, to);
    }
    out = sanitize_hdr_regex().replace_all(&out, "").to_string();
    if out.contains("cc_") {
        loop {
            let replaced = sanitize_kv_regex().replace_all(&out, "").to_string();
            if replaced == out {
                break;
            }
            out = replaced;
        }
    }
    out.trim().to_string()
}

/// 净化 messages 内容（兼容字符串与多模态数组，只动 text part）
fn sanitize_messages(obj: &mut Map<String, Value>) {
    let Some(Value::Array(messages)) = obj.get_mut("messages") else { return };
    for message in messages.iter_mut() {
        let Some(map) = message.as_object_mut() else { continue };
        match map.get_mut("content") {
            Some(Value::String(text)) => {
                *text = sanitize_text(text);
            }
            Some(Value::Array(parts)) => {
                for part in parts.iter_mut() {
                    if let Some(text) = part.get("text").and_then(Value::as_str).map(String::from) {
                        let cleaned = sanitize_text(&text);
                        if cleaned != text {
                            if let Some(part_map) = part.as_object_mut() {
                                part_map.insert("text".into(), Value::String(cleaned));
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

/// ---------------------------------------------------------------------------
/// SSE 回传：逐帧规范化透传 / 聚合
/// ---------------------------------------------------------------------------

/// 上游 SSE 帧规范化（OpenAI 流式白名单重建，剥噪声；对齐 Go 版 normalizeFrame）
fn normalize_frame(payload: &str) -> String {
    let Ok(value) = serde_json::from_str::<Value>(payload) else {
        return payload.to_string();
    };
    let mut out = Map::new();
    for key in ["id", "object", "created", "model", "system_fingerprint", "service_tier"] {
        if let Some(v) = value.get(key).filter(|v| !v.is_null()) {
            out.insert(key.into(), v.clone());
        }
    }
    out.entry("object".to_string()).or_insert_with(|| Value::String("chat.completion.chunk".into()));
    out.entry("id".to_string()).or_insert_with(|| Value::String("chatcmpl-cockpit".into()));
    if let Some(choices) = value.get("choices").and_then(Value::as_array) {
        let mut new_choices = Vec::with_capacity(choices.len());
        for choice in choices {
            let mut new_choice = Map::new();
            if let Some(index) = choice.get("index") {
                new_choice.insert("index".into(), index.clone());
            }
            let mut delta = Map::new();
            if let Some(d) = choice.get("delta").and_then(Value::as_object) {
                for key in ["role", "content", "reasoning_content", "refusal"] {
                    if let Some(v) = d.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()) {
                        delta.insert(key.into(), Value::String(v.to_string()));
                    }
                }
                if let Some(tcs) = d.get("tool_calls").and_then(Value::as_array).filter(|a| !a.is_empty()) {
                    delta.insert("tool_calls".into(), Value::Array(tcs.clone()));
                }
                if let Some(fc) = d.get("function_call") {
                    let keep = fc.as_object().map(|m| {
                        !m.get("name").and_then(Value::as_str).unwrap_or("").is_empty()
                            || !m.get("arguments").and_then(Value::as_str).unwrap_or("").is_empty()
                    }).unwrap_or(true);
                    if keep {
                        delta.insert("function_call".into(), fc.clone());
                    }
                }
            }
            new_choice.insert("delta".into(), Value::Object(delta));
            let finish = choice.get("finish_reason").and_then(Value::as_str).filter(|s| !s.is_empty());
            new_choice.insert("finish_reason".into(), finish.map(Value::from).unwrap_or(Value::Null));
            new_choices.push(Value::Object(new_choice));
        }
        out.insert("choices".into(), Value::Array(new_choices));
    }
    out.insert("usage".into(), value.get("usage").cloned().unwrap_or(Value::Null));
    serde_json::to_string(&Value::Object(out)).unwrap_or_else(|_| payload.to_string())
}

/// 管道读端：把 mpsc 通道适配成 std::io::Read（tiny_http 从这里读出 SSE 流）
struct PipeReader {
    rx: mpsc::Receiver<Vec<u8>>,
    leftover: Vec<u8>,
    offset: usize,
}

impl Read for PipeReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        // 补充缓冲：通道关闭（写端 drop）→ 读到 0 = EOF，tiny_http 正常收尾
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

/// 流式回传：上游 SSE 逐帧规范化后写入管道；保证恰好一个 [DONE]；空流补 error 帧
///
/// 注意 tiny_http 的 `request.respond()` 是**阻塞式**的：它内部会同步调用
/// `raw_print` 读取整个 body 并写回客户端。因此绝不能"先 respond 再泵数据"
/// （respond 阻塞等 channel 出数据、泵数据又在 respond 之后 → 死锁，实测挂起 70s+）。
/// 正确次序：先 spawn 后台线程泵送，再 respond 让主线程消费管道。
fn stream_response(request: tiny_http::Request, upstream: reqwest::blocking::Response) {
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let reader = PipeReader { rx, leftover: Vec::new(), offset: 0 };
    let response = tiny_http::Response::new(
        tiny_http::StatusCode(200),
        vec![
            tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/event-stream"[..]).unwrap(),
            tiny_http::Header::from_bytes(&b"Cache-Control"[..], &b"no-cache"[..]).unwrap(),
            tiny_http::Header::from_bytes(&b"Access-Control-Allow-Origin"[..], &b"*"[..]).unwrap(),
        ],
        reader,
        None::<usize>,
        None::<mpsc::Receiver<tiny_http::Header>>,
    );
    // 先起泵线程：数据源就绪后 respond 才不会被饿死
    std::thread::spawn(move || pump_upstream_stream(tx, upstream));
    logger::log_info(&format!("[WorkBuddyGateway] chat → 200 (SSE 流式回传)"));
    // 阻塞式应答：内部持续从 PipeReader 取帧写回客户端，直到管道 EOF（泵线程结束）
    let _ = request.respond(response);
}

/// 后台泵：把上游 SSE 逐帧规范化后送入管道通道。
/// 客户端断开 → 通道收端销毁 → send 失败 → 提前返回停止读取上游。
fn pump_upstream_stream(tx: mpsc::Sender<Vec<u8>>, upstream: reqwest::blocking::Response) {
    let mut reader = BufReader::new(upstream);
    let mut line = String::new();
    // 已成功送入管道的有效帧数（0 表示上游空流，需补 error 帧）
    let mut valid_frames = 0u32;
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.starts_with("data: [DONE]") {
            break;
        }
        if let Some(payload) = trimmed.strip_prefix("data: ") {
            let normalized = normalize_frame(payload);
            valid_frames += 1;
            if tx.send(format!("data: {normalized}\n\n").into_bytes()).is_err() {
                return;
            }
        } else if !trimmed.is_empty() {
            if tx.send(format!("{line}\n").into_bytes()).is_err() {
                return;
            }
        }
    }
    if valid_frames == 0 {
        let _ = tx.send(br#"data: {"error":{"message":"empty upstream stream","type":"upstream_error"}}"#.to_vec());
    }
    let _ = tx.send(b"data: [DONE]\n\n".to_vec());
}

/// ---------------------------------------------------------------------------
/// Responses ↔ Chat Completions 协议翻译层
/// ---------------------------------------------------------------------------

/// Responses 入站请求 → Chat Completions 请求体。
///
/// 翻译要点：
/// - `input` 支持字符串或 item 数组（message / function_call / function_call_output），
///   逐项映射为 chat 的 `messages`。
/// - `tools[].{name,parameters}` → `tools[].{type:"function",function:{name,parameters}}`。
/// - `tool_choice` / `max_output_tokens` / `temperature` 等直通或改名。
/// 失败时返回中文错误信息，由调用方以 400 回写。
/// chat 工具名 → 所属命名空间。响应回传 function_call 时必须补回 namespace 字段，
/// 否则 Codex 客户端按「namespace + 工具名」匹配执行器会失败（表现为 unsupported call）。
type ToolNamespaceMap = std::collections::HashMap<String, String>;

fn responses_to_chat(src: &[u8]) -> Result<(Vec<u8>, ToolNamespaceMap), String> {
    let Ok(Value::Object(obj)) = serde_json::from_slice::<Value>(src) else {
        return Err("请求体不是合法 JSON 对象".into());
    };
    let mut out = Map::new();
    let mut ns_map: ToolNamespaceMap = HashMap::new();

    // 模型名直通
    if let Some(model) = obj.get("model") {
        out.insert("model".into(), model.clone());
    }
    // max_output_tokens → max_tokens；两者都接受
    if let Some(v) = obj.get("max_output_tokens").or_else(|| obj.get("max_tokens")) {
        out.insert("max_tokens".into(), v.clone());
    }
    for key in ["temperature", "top_p", "stream", "user", "parallel_tool_calls"] {
        if let Some(v) = obj.get(key) {
            out.insert(key.into(), v.clone());
        }
    }
    // instructions 作为 system 消息前置
    let mut messages: Vec<Value> = Vec::new();
    if let Some(text) = obj.get("instructions").and_then(Value::as_str) {
        if !text.trim().is_empty() {
            messages.push(json!({ "role": "system", "content": text }));
        }
    }
    match obj.get("input") {
        Some(Value::String(text)) => {
            messages.push(json!({ "role": "user", "content": text }));
        }
        Some(Value::Array(items)) => {
            for item in items {
                if let Some(m) = responses_item_to_message(item) {
                    messages.push(m);
                }
            }
        }
        _ => return Err("缺少 input 字段或类型不支持".into()),
    }
    if messages.is_empty() {
        return Err("input 未解析出任何消息".into());
    }
    out.insert("messages".into(), Value::Array(messages));

    // 工具定义：Responses 为扁平结构，chat 需嵌一层 function。
    // 注意 namespace 容器（如 functions.collaboration）内的工具必须递归展开，
    // 否则上游只收到一个无参数的命名空间空壳，子代理等工具会整体消失。
    if let Some(Value::Array(tools)) = obj.get("tools") {
        let mut converted: Vec<Value> = Vec::new();
        for tool in tools {
            collect_chat_tools(tool, &mut converted, "", &mut ns_map);
        }
        if !converted.is_empty() {
            out.insert("tools".into(), Value::Array(converted));
        }
    }
    // tool_choice：字符串直通；"required"/"auto"/"none" 语义一致
    if let Some(tc) = obj.get("tool_choice") {
        out.insert("tool_choice".into(), tc.clone());
    }
    let encoded = serde_json::to_vec(&Value::Object(out))
        .map_err(|e| format!("序列化翻译结果失败: {e}"))?;
    Ok((encoded, ns_map))
}

/// 递归收集 chat 可用的工具定义。
///
/// Codex 会用 `{type:"namespace", name:"functions.collaboration", tools:[...]}` 这类容器
/// 包裹协作类工具（spawn_agent / wait_agent / send_message 等）。chat 协议没有命名空间
/// 概念，必须把容器内的函数工具逐项摊平，否则它们对上游模型完全不可见
/// —— 表现为「模型声称没有可调用的子代理」。
fn collect_chat_tools(
    tool: &Value,
    out: &mut Vec<Value>,
    ns: &str,
    ns_map: &mut ToolNamespaceMap,
) {
    let Some(map) = tool.as_object() else { return };
    let kind = map.get("type").and_then(Value::as_str).unwrap_or("");
    // 命名空间容器：递归展开其 tools 子数组（可能多层嵌套），并向下传递容器名
    if kind.eq_ignore_ascii_case("namespace") {
        let container_ns = map.get("name").and_then(Value::as_str).unwrap_or(ns);
        if let Some(Value::Array(inner)) = map.get("tools") {
            for sub in inner {
                collect_chat_tools(sub, out, container_ns, ns_map);
            }
        }
        return;
    }
    // 已是 chat 格式则原样保留
    if map.contains_key("function") {
        out.push(tool.clone());
        return;
    }
    // 标准 Responses 扁平工具 → chat 嵌套格式；无 name 的项直接跳过
    let Some(name) = map.get("name").and_then(Value::as_str) else { return };
    // 记录「工具名 → 命名空间」，供响应侧还原 function_call.namespace
    if !ns.is_empty() {
        ns_map.insert(name.to_string(), ns.to_string());
    }
    let mut func = Map::new();
    func.insert("name".into(), Value::String(name.to_string()));
    if let Some(desc) = map.get("description") {
        func.insert("description".into(), desc.clone());
    }
    if let Some(params) = map.get("parameters") {
        func.insert("parameters".into(), params.clone());
    }
    out.push(json!({ "type": "function", "function": Value::Object(func) }));
}

/// 单个 Responses input item → chat message；无法映射时返回 None。
fn responses_item_to_message(item: &Value) -> Option<Value> {
    let map = item.as_object()?;
    let typ = map.get("type").and_then(Value::as_str).unwrap_or("message");
    match typ {
        // 普通消息：content 可为字符串或 [{type:"input_text"/"output_text",text}]
        "message" => {
            let role = map.get("role").and_then(Value::as_str).unwrap_or("user");
            let content = responses_content_to_text(map.get("content")?);
            Some(json!({ "role": role, "content": content }))
        }
        // 模型发起的历史工具调用 → assistant.tool_calls
        "function_call" => {
            let name = map.get("name").and_then(Value::as_str).unwrap_or("");
            let args = map.get("arguments").and_then(Value::as_str).unwrap_or("{}");
            let call_id = map.get("call_id").and_then(Value::as_str).unwrap_or("");
            Some(json!({
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "id": call_id,
                    "type": "function",
                    "function": { "name": name, "arguments": args }
                }]
            }))
        }
        // 工具执行结果 → role=tool
        "function_call_output" => {
            let call_id = map.get("call_id").and_then(Value::as_str).unwrap_or("");
            let output = map
                .get("output")
                .map(|v| match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .unwrap_or_default();
            Some(json!({ "role": "tool", "tool_call_id": call_id, "content": output }))
        }
        // reasoning / 其他 item 跳过（chat 侧无需回填）
        _ => None,
    }
}

/// Responses content（字符串或 item 数组）→ 纯文本。
fn responses_content_to_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(items) => {
            let mut text = String::new();
            for item in items {
                if let Some(t) = item.get("text").and_then(Value::as_str) {
                    text.push_str(t);
                }
            }
            text
        }
        other => other.to_string(),
    }
}

/// Chat Completions 响应 → Responses 响应结构（非流式）。
fn chat_to_responses(chat: &Value) -> Value {
    let message = chat
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"));
    let finish = chat
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("finish_reason"))
        .and_then(Value::as_str)
        .unwrap_or("stop");

    let mut output: Vec<Value> = Vec::new();
    // 推理内容作为 reasoning item（部分客户端会读取）
    if let Some(reasoning) = message
        .and_then(|m| m.get("reasoning_content"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        output.push(json!({
            "id": format!("rs_{}", now_ms()),
            "type": "reasoning",
            "summary": [{ "type": "summary_text", "text": reasoning }]
        }));
    }
    // 正文 message item
    let text = message
        .and_then(|m| m.get("content"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if !text.is_empty() {
        output.push(json!({
            "id": format!("msg_{}", now_ms()),
            "type": "message",
            "role": "assistant",
            "status": "completed",
            "content": [{ "type": "output_text", "text": text, "annotations": [] }]
        }));
    }
    // 工具调用 item
    if let Some(calls) = message.and_then(|m| m.get("tool_calls")).and_then(Value::as_array) {
        for call in calls {
            let name = call
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let args = call
                .get("function")
                .and_then(|f| f.get("arguments"))
                .and_then(Value::as_str)
                .unwrap_or("{}");
            let call_id = call.get("id").and_then(Value::as_str).unwrap_or("");
            output.push(json!({
                "id": format!("fc_{}", now_ms()),
                "type": "function_call",
                "call_id": call_id,
                "name": name,
                "arguments": args,
                "status": "completed"
            }));
        }
    }

    let mut result = json!({
        "id": chat.get("id").cloned().unwrap_or_else(|| Value::String(format!("resp_{}", now_ms()))),
        "object": "response",
        "created_at": chat.get("created").and_then(Value::as_i64).unwrap_or_else(|| now_ms() / 1000),
        "model": chat.get("model").cloned().unwrap_or(Value::Null),
        "status": if finish == "length" { "incomplete" } else { "completed" },
        "output": output,
    });
    // usage 必须是对象才转换：上游缺 usage 时为 null，直接透传会产出 input_tokens:null。
    // 复用流式同款转换，保证 GLM 风格缓存字段（prompt_cache_hit_tokens）不丢失。
    if let Some(usage) = chat.get("usage").filter(|v| v.is_object()) {
        result["usage"] = responses_usage_from_chat(usage);
    }
    result
}

/// Responses 协议流式回写：上游 chat chunk → Responses 事件序列。
///
/// 与 `stream_response` 同样遵守 tiny_http 的阻塞式 respond 契约：
/// **先 spawn 泵线程，再 respond**，否则死锁。
fn stream_responses(
    request: tiny_http::Request,
    upstream: reqwest::blocking::Response,
    tool_ns_map: ToolNamespaceMap,
) {
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let reader = PipeReader { rx, leftover: Vec::new(), offset: 0 };
    let response = tiny_http::Response::new(
        tiny_http::StatusCode(200),
        vec![
            tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/event-stream"[..]).unwrap(),
            tiny_http::Header::from_bytes(&b"Cache-Control"[..], &b"no-cache"[..]).unwrap(),
            tiny_http::Header::from_bytes(&b"Access-Control-Allow-Origin"[..], &b"*"[..]).unwrap(),
        ],
        reader,
        None::<usize>,
        None::<mpsc::Receiver<tiny_http::Header>>,
    );
    std::thread::spawn(move || pump_chat_to_responses(tx, upstream, tool_ns_map));
    logger::log_info("[WorkBuddyGateway] responses → 200 (SSE 流式回传)");
    let _ = request.respond(response);
}

/// 发送一个 Responses SSE 事件（`event:` + `data:` 成对）；返回 false 表示客户端已断开。
fn send_responses_event(tx: &mpsc::Sender<Vec<u8>>, event: &str, data: Value) -> bool {
    let payload = format!("event: {event}\ndata: {data}\n\n");
    tx.send(payload.into_bytes()).is_ok()
}

/// 工具项在 output 数组中的起始下标。
/// 正文 message 占用下标 0，工具项从 100 起编，避免与正文冲突（下游仅要求下标稳定唯一）。
const TOOL_OUTPUT_INDEX_BASE: u32 = 100;

/// Responses 流中单个工具项的稳定标识与参数累积态。
///
/// **关键约束**：同一次工具调用的所有参数分片必须复用同一个 `output_index` 与 `item_id`。
/// 上游 chat 协议把一次调用的参数拆成多帧，若每帧分配新下标，下游（如 sub2api）按
/// `output_index` 反查归属时会匹配失败并丢弃后续分片，最终参数被截断（实测只剩 `{"`）。
struct ToolItemState {
    /// 该工具项在 output 数组中的稳定下标
    output_index: u32,
    /// 稳定 item_id：所有分片与 done 事件复用
    item_id: String,
    /// 上游 call_id
    call_id: String,
    /// 函数名（仅首帧到达）
    name: String,
    /// 累积的参数 JSON 片段
    arguments: String,
    /// 是否已发出 response.output_item.added
    announced: bool,
}

/// 后台泵：把上游 chat SSE 逐帧翻译为 Responses 事件流。
///
/// 事件序列（对齐官方）：
/// `response.created` → `response.output_item.added` → 若干
/// `response.output_text.delta` / `response.function_call_arguments.delta`
/// → `response.output_item.done` → `response.completed`。
fn pump_chat_to_responses(
    tx: mpsc::Sender<Vec<u8>>,
    upstream: reqwest::blocking::Response,
    tool_ns_map: ToolNamespaceMap,
) {
    let mut reader = BufReader::new(upstream);
    let mut line = String::new();
    let resp_id = format!("resp_{}", now_ms());
    let mut created_at = now_ms() / 1000;
    let mut model = Value::Null;
    // 是否已发出首个 output_item.added（message 项）
    let mut message_item_open = false;
    // message 项的稳定 item_id：所有 output_text.delta 复用，避免每帧新生成
    let mut message_item_id = String::new();
    let mut text_index: u32 = 0;
    // 累积 message 项正文：协议要求收尾的 output_item.done 与 response.completed
    // 必须携带完整文本，否则客户端只看得到 delta、拿不到最终内容（表现为界面空白无响应）
    let mut message_text = String::new();
    // 工具项状态：按上游 tool_calls[].index 定位，跨帧复用同一 output_index/item_id
    let mut tool_items: Vec<ToolItemState> = Vec::new();
    // 上游 chat usage：流末尾的 usage 专用块（choices 缺省）也含真实用量，
    // 捕获最新一条，收尾注入 response.completed —— 否则 sub2api 等计费网关
    // 拿不到用量，WB2 账号使用记录全部为 0
    let mut upstream_usage: Option<Value> = None;

    // response.created 起始事件
    let created = json!({
        "type": "response.created",
        "response": {
            "id": resp_id,
            "object": "response",
            "created_at": created_at,
            "model": model,
            "status": "in_progress",
            "output": []
        }
    });
    if !send_responses_event(&tx, "response.created", created) {
        return;
    }

    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.starts_with("data: [DONE]") {
            break;
        }
        let Some(payload) = trimmed.strip_prefix("data: ") else {
            continue;
        };
        let Ok(chunk) = serde_json::from_str::<Value>(payload) else {
            continue;
        };
        // 先捕获 usage 再判 choices：usage 专用块没有 delta，漏捕获即计费为 0
        if let Some(u) = chunk.get("usage").filter(|v| v.is_object()) {
            upstream_usage = Some(u.clone());
        }
        if model.is_null() {
            if let Some(m) = chunk.get("model") {
                model = m.clone();
            }
        }
        if let Some(c) = chunk.get("created").and_then(Value::as_i64) {
            created_at = c;
        }
        let Some(delta) = chunk
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("delta"))
        else {
            continue;
        };

        // 正文增量
        if let Some(text) = delta.get("content").and_then(Value::as_str) {
            if !text.is_empty() {
                if !message_item_open {
                    message_item_open = true;
                    message_item_id = format!("msg_{}", now_ms());
                    let added = json!({
                        "type": "response.output_item.added",
                        "output_index": text_index,
                        "item": {
                            "id": message_item_id.clone(),
                            "type": "message",
                            "role": "assistant",
                            "status": "in_progress",
                            "content": []
                        }
                    });
                    if !send_responses_event(&tx, "response.output_item.added", added) {
                        return;
                    }
                    // 补充 content_part.added：官方序列在 message 项宣告后紧接内容部件
                    let part_added = json!({
                        "type": "response.content_part.added",
                        "item_id": message_item_id.clone(),
                        "output_index": text_index,
                        "content_index": 0,
                        "part": { "type": "output_text", "text": "", "annotations": [] }
                    });
                    if !send_responses_event(&tx, "response.content_part.added", part_added) {
                        return;
                    }
                }
                // 累积正文，供收尾事件回填完整文本
                message_text.push_str(text);
                let event = json!({
                    "type": "response.output_text.delta",
                    "item_id": message_item_id.clone(),
                    "output_index": text_index,
                    "content_index": 0,
                    "delta": text
                });
                if !send_responses_event(&tx, "response.output_text.delta", event) {
                    return;
                }
            }
        }

        // 工具调用增量：按上游 index 归并到同一工具项，保证 output_index/item_id 跨帧稳定
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                // 上游用 index 标识「同一次调用的不同参数分片」；缺省按 0 处理
                let upstream_index = call
                    .get("index")
                    .and_then(Value::as_i64)
                    .unwrap_or(0)
                    .max(0) as usize;
                // 首次见到该 index：登记工具项，分配稳定 output_index 与 item_id
                while tool_items.len() <= upstream_index {
                    let ordinal = tool_items.len() as u32;
                    tool_items.push(ToolItemState {
                        output_index: TOOL_OUTPUT_INDEX_BASE + ordinal,
                        item_id: format!("fc_{}_{}", now_ms(), ordinal),
                        call_id: String::new(),
                        name: String::new(),
                        arguments: String::new(),
                        announced: false,
                    });
                }
                let item = &mut tool_items[upstream_index];
                if let Some(id) = call.get("id").and_then(Value::as_str) {
                    if !id.is_empty() {
                        item.call_id = id.to_string();
                    }
                }
                if let Some(name) = call
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                {
                    if !name.is_empty() {
                        item.name = name.to_string();
                    }
                }
                // 拿到函数名后才能构造合法的 function_call item，故此时才发 added（每项仅一次）
                if !item.announced && !item.name.is_empty() {
                    item.announced = true;
                    let mut added_item = json!({
                        "id": item.item_id.clone(),
                        "type": "function_call",
                        "call_id": item.call_id.clone(),
                        "name": item.name.clone(),
                        "arguments": "",
                        "status": "in_progress"
                    });
                    // 按「工具名→命名空间」映射补回 namespace，客户端据此路由执行器
                    if let Some(ns) = tool_ns_map.get(item.name.as_str()) {
                        added_item["namespace"] = json!(ns);
                    }
                    let added = json!({
                        "type": "response.output_item.added",
                        "output_index": item.output_index,
                        "item": added_item
                    });
                    if !send_responses_event(&tx, "response.output_item.added", added) {
                        return;
                    }
                }
                // 参数增量：累积后透传，复用同一 output_index/item_id（修复分片被下游丢弃）
                if let Some(args) = call
                    .get("function")
                    .and_then(|f| f.get("arguments"))
                    .and_then(Value::as_str)
                {
                    if !args.is_empty() {
                        item.arguments.push_str(args);
                        let event = json!({
                            "type": "response.function_call_arguments.delta",
                            "item_id": item.item_id.clone(),
                            "output_index": item.output_index,
                            "delta": args
                        });
                        if !send_responses_event(&tx, "response.function_call_arguments.delta", event) {
                            return;
                        }
                    }
                }
            }
        }
    }

    // 收尾：output_text.done / content_part.done / output_item.done + response.completed
    if message_item_open {
        let text_done = json!({
            "type": "response.output_text.done",
            "item_id": message_item_id.clone(),
            "output_index": text_index,
            "content_index": 0,
            "text": message_text.clone()
        });
        let _ = send_responses_event(&tx, "response.output_text.done", text_done);
        let part_done = json!({
            "type": "response.content_part.done",
            "item_id": message_item_id.clone(),
            "output_index": text_index,
            "content_index": 0,
            "part": { "type": "output_text", "text": message_text.clone(), "annotations": [] }
        });
        let _ = send_responses_event(&tx, "response.content_part.done", part_done);
        let done = json!({
            "type": "response.output_item.done",
            "output_index": text_index,
            "item": {
                "id": message_item_id.clone(),
                "type": "message",
                "role": "assistant",
                "status": "completed",
                "content": [{ "type": "output_text", "text": message_text.clone(), "annotations": [] }]
            }
        });
        let _ = send_responses_event(&tx, "response.output_item.done", done);
    }
    // 每个工具项补发 arguments.done 与 output_item.done（携带累积完成的完整参数，对齐官方事件序列）
    for item in &tool_items {
        if !item.announced {
            continue;
        }
        let args_done = json!({
            "type": "response.function_call_arguments.done",
            "item_id": item.item_id.clone(),
            "output_index": item.output_index,
            "arguments": item.arguments.clone()
        });
        let _ = send_responses_event(&tx, "response.function_call_arguments.done", args_done);
        let mut done_item = json!({
            "id": item.item_id.clone(),
            "type": "function_call",
            "call_id": item.call_id.clone(),
            "name": item.name.clone(),
            "arguments": item.arguments.clone(),
            "status": "completed"
        });
        // 回传 namespace：客户端按「namespace+工具名」路由执行器
        if let Some(ns) = tool_ns_map.get(item.name.as_str()) {
            done_item["namespace"] = json!(ns);
        }
        let item_done = json!({
            "type": "response.output_item.done",
            "output_index": item.output_index,
            "item": done_item
        });
        let _ = send_responses_event(&tx, "response.output_item.done", item_done);
    }
    // 汇总所有已宣告的 output 项：客户端以 completed.output 作为最终结果来渲染，
    // 此前写死为空数组会导致「请求成功但界面一片空白、看似毫无响应」
    let mut output_items: Vec<Value> = Vec::new();
    if message_item_open {
        output_items.push(json!({
            "id": message_item_id.clone(),
            "type": "message",
            "role": "assistant",
            "status": "completed",
            "content": [{ "type": "output_text", "text": message_text.clone(), "annotations": [] }]
        }));
    }
    for item in &tool_items {
        if !item.announced {
            continue;
        }
        let mut ns_item = json!({
            "id": item.item_id.clone(),
            "type": "function_call",
            "call_id": item.call_id.clone(),
            "name": item.name.clone(),
            "arguments": item.arguments.clone(),
            "status": "completed"
        });
        // completed.output 是客户端渲染最终结果的依据，同样需要 namespace
        if let Some(ns) = tool_ns_map.get(item.name.as_str()) {
            ns_item["namespace"] = json!(ns);
        }
        output_items.push(ns_item);
    }
    let mut completed = json!({
        "type": "response.completed",
        "response": {
            "id": resp_id,
            "object": "response",
            "created_at": created_at,
            "model": model,
            "status": "completed",
            "output": output_items
        }
    });
    if let Some(u) = &upstream_usage {
        completed["response"]["usage"] = responses_usage_from_chat(u);
    }
    let _ = send_responses_event(&tx, "response.completed", completed);
}

/// 上游 chat usage → Responses usage 形状：缺失字段补 0，保证 sub2api 等下游计费网关可解析。
fn responses_usage_from_chat(chat_usage: &Value) -> Value {
    let input = chat_usage.get("prompt_tokens").and_then(Value::as_i64).unwrap_or(0);
    let output = chat_usage.get("completion_tokens").and_then(Value::as_i64).unwrap_or(0);
    // 缓存命中：优先 OpenAI 标准字段；GLM 风格上游两者恒为 0，回退 prompt_cache_hit_tokens
    let cached = chat_usage
        .pointer("/prompt_tokens_details/cached_tokens")
        .or_else(|| chat_usage.get("cached_tokens"))
        .and_then(Value::as_i64)
        .filter(|&v| v > 0)
        .or_else(|| chat_usage.get("prompt_cache_hit_tokens").and_then(Value::as_i64))
        .unwrap_or(0);
    let reasoning = chat_usage
        .pointer("/completion_tokens_details/reasoning_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    json!({
        "input_tokens": input,
        "output_tokens": output,
        "total_tokens": chat_usage.get("total_tokens").and_then(Value::as_i64).unwrap_or(input + output),
        "input_tokens_details": { "cached_tokens": cached },
        "output_tokens_details": { "reasoning_tokens": reasoning }
    })
}

/// 非流式聚合：读取上游 SSE 全部帧，合成单个 chat.completion（对齐 Go 版 Aggregate）
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
    let mut tool_calls: Vec<(i64, Value)> = Vec::new();
    let mut valid_events = 0u32;
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(error) => return Err(format!("read stream: {error}")),
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.starts_with("data: [DONE]") {
            break;
        }
        let Some(payload) = trimmed.strip_prefix("data: ") else { continue };
        let Ok(chunk) = serde_json::from_str::<Value>(payload) else { continue };
        valid_events += 1;
        if id.is_empty() {
            id = chunk.get("id").and_then(Value::as_str).unwrap_or("").to_string();
        }
        if model.is_empty() {
            model = chunk.get("model").and_then(Value::as_str).unwrap_or("").to_string();
        }
        if created == 0 {
            created = chunk.get("created").and_then(Value::as_i64).unwrap_or(0);
        }
        if let Some(u) = chunk.get("usage").filter(|u| !u.is_null()) {
            usage = Some(u.clone());
        }
        let Some(choices) = chunk.get("choices").and_then(Value::as_array) else { continue };
        for choice in choices {
            if let Some(fr) = choice.get("finish_reason").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                finish_reason = fr.to_string();
            }
            let Some(delta) = choice.get("delta").and_then(Value::as_object) else { continue };
            if let Some(r) = delta.get("role").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                role = r.to_string();
            }
            if let Some(text) = delta.get("content").and_then(Value::as_str) {
                content.push_str(text);
            }
            if let Some(rc) = delta.get("reasoning_content").and_then(Value::as_str) {
                reasoning.push_str(rc);
            }
            if let Some(tcs) = delta.get("tool_calls").and_then(Value::as_array) {
                for tc in tcs {
                    let index = tc.get("index").and_then(Value::as_i64).unwrap_or(0);
                    if let Some(existing) = tool_calls.iter_mut().find(|(i, _)| *i == index) {
                        merge_tool_call(&mut existing.1, tc);
                    } else {
                        let mut merged = json!({ "index": index });
                        merge_tool_call(&mut merged, tc);
                        tool_calls.push((index, merged));
                    }
                }
            }
        }
    }
    if valid_events == 0 {
        return Err("upstream stream contained no valid data events".into());
    }
    if id.is_empty() {
        id = format!("chatcmpl-{}", now_ms());
    }
    if created == 0 {
        created = now_ms() / 1000;
    }
    let mut message = json!({ "role": role, "content": content });
    if !reasoning.is_empty() {
        message["reasoning_content"] = Value::String(reasoning);
    }
    if !tool_calls.is_empty() {
        tool_calls.sort_by_key(|(index, _)| *index);
        message["tool_calls"] = Value::Array(tool_calls.into_iter().map(|(_, v)| v).collect());
    }
    let mut response = json!({
        "id": id,
        "object": "chat.completion",
        "created": created,
        "model": model,
        "choices": [{ "index": 0, "message": message, "finish_reason": finish_reason }],
    });
    if let Some(u) = usage {
        response["usage"] = u;
    }
    Ok(response)
}

/// 合并流式 tool_call 分片：id/type/function.name 直覆盖，arguments 拼接
fn merge_tool_call(merged: &mut Value, delta: &Value) {
    let map = merged.as_object_mut().expect("merged 必为对象");
    if let Some(id) = delta.get("id").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        map.insert("id".into(), Value::String(id.to_string()));
    }
    if let Some(t) = delta.get("type").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        map.insert("type".into(), Value::String(t.to_string()));
    }
    let Some(function) = delta.get("function").and_then(Value::as_object) else { return };
    let function_value = map
        .entry("function".to_string())
        .or_insert_with(|| json!({}));
    let Some(function_map) = function_value.as_object_mut() else { return };
    if let Some(name) = function.get("name").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        function_map.insert("name".into(), Value::String(name.to_string()));
    }
    if let Some(arguments) = function.get("arguments").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        let prev = function_map.get("arguments").and_then(Value::as_str).unwrap_or("").to_string();
        let mut next = prev;
        next.push_str(arguments);
        function_map.insert("arguments".into(), Value::String(next));
    }
}

/// ---------------------------------------------------------------------------
/// 管理端（7864）请求分发：面板读取状态 / 配置 / 模型 / 账号
/// ---------------------------------------------------------------------------

/// 管理端请求处理
fn handle_admin_request(mut request: tiny_http::Request) {
    // CORS 预检直接放行
    if *request.method() == tiny_http::Method::Options {
        return answer_preflight(request);
    }
    let url = request.url().split('?').next().unwrap_or("").to_string();
    match url.as_str() {
        "/api/status" => handle_admin_status(request),
        "/api/config" => {
            let key = resolve_api_key();
            // 容器/局域网视角的访问地址：宿主机 LAN IP（供 Docker 容器或局域网内其他机器使用）
            let lan_base = local_lan_ip()
                .map(|ip| json!(format!("http://{ip}:{PORT_OPENAI}/v1")))
                .unwrap_or(Value::Null);
            write_json_response(
                request,
                200,
                &json!({ "config": { "api_key": key, "schedule": { "checkin_hours": [], "keepalive_hours": [] } }, "lan_base_url": lan_base }),
            );
        }
        "/api/models" => {
            let list = model_list();
            // 面板下拉只取 id 字段
            let ids: Vec<Value> = list
                .get("data")
                .and_then(Value::as_array)
                .map(|items| items.iter().map(|m| json!({ "id": m.get("id") })).collect())
                .unwrap_or_default();
            write_json_response(request, 200, &json!({ "data": ids }));
        }
        "/api/accounts" => {
            let accounts = workbuddy_account::list_accounts();
            let list: Vec<Value> = accounts
                .iter()
                .map(|a| {
                    json!({
                        "uid": a.uid.clone().unwrap_or_default(),
                        "nickname": a.nickname.clone().unwrap_or_else(|| a.email.clone()),
                        "file": format!("workbuddy-{}.json", a.id),
                        "checked": false,
                    })
                })
                .collect();
            write_json_response(request, 200, &json!(list));
        }
        _ => {
            let body = b"404 page not found".to_vec();
            let mut response = tiny_http::Response::from_data(body)
                .with_status_code(tiny_http::StatusCode(404));
            for header in cors_headers() {
                response = response.with_header(header);
            }
            let _ = request.respond(response);
        }
    }
}

/// 管理端状态：账号运行态 + 池子统计（面板已改为读本地账号库，此接口保留兼容）
fn handle_admin_status(request: tiny_http::Request) {
    let accounts = workbuddy_account::list_accounts();
    let now = SystemTime::now();
    let states = account_states().lock().ok();
    let mut items = Vec::new();
    let mut cooling = 0usize;
    let mut disabled = 0usize;
    for account in &accounts {
        let state = states.as_ref().and_then(|s| s.get(&account.id)).cloned().unwrap_or_default();
        let is_cooling = state.cooldown_until.map(|t| t > now).unwrap_or(false);
        if is_cooling { cooling += 1; }
        if state.disabled { disabled += 1; }
        items.push(json!({
            "uid": account.uid.clone().unwrap_or_else(|| account.id.clone()),
            "nickname": account.nickname.clone().unwrap_or_else(|| account.email.clone()),
            "email": account.email,
            "cooling": is_cooling,
            "disabled": state.disabled,
            "err_streak": state.err_streak,
        }));
    }
    let healthy = items.iter().filter(|i| {
        !i.get("cooling").and_then(Value::as_bool).unwrap_or(false)
            && !i.get("disabled").and_then(Value::as_bool).unwrap_or(false)
    }).count();
    write_json_response(
        request,
        200,
        &json!({
            "accounts": items,
            "total": accounts.len(),
            "healthy": healthy,
            "cooling": cooling,
            "disabled": disabled,
            "in_flight_full": 0,
            "sticky_sessions": 0,
            "redis_mode": "noop",
        }),
    );
}
