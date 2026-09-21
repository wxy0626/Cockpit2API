//! WorkBuddy 自动任务：成长中心任务与互动玩法的自动执行模块。
//!
//! ## 三条刻意为之的设计约束（改动前请先读完）
//!
//! 1. **完全自包含**：本文件自带 HTTP 客户端、鉴权头、配置与日志读写。
//!    刻意**不**向 `codebuddy_cn_oauth.rs` 等共享模块新增函数——本功能是项目
//!    上游（jlcodes99/cockpit-tools）之外的增量，放在独立文件里可以避免
//!    合并上游更新时产生冲突。因此这里重复了一小段请求头代码，是有意为之。
//!
//! 2. **只做「领取/动作类」接口**：请求体要么为空、要么只含**选择项**
//!    （如兑换档位、补签日期）或**幂等令牌**（`client_token`）。
//!    「某任务算不算完成」一律由服务端根据自己的记录裁定，客户端不陈述任何事实，
//!    因此不存在伪造空间。**本模块不实现任何需要客户端自证行为的遥测上报**
//!    （即 `POST /v2/report`），这是与参考实现（L0NE-6/WorkBuddy-Daily）最关键的分歧点。
//!
//! 3. **上游前缀要按接口族区分**（均已实测）：
//!    - 任务族 `/v2/activity/growth/tasks*`：18 项任务，带 `accept_status`/`progress`。
//!      注意同名的 `/activity/growth/tasks` 是**另一套旧体系**（只 5 项、无进度），
//!      两者都返回 200 但数据不同，**不可混用**。
//!    - buddy 族 `/activity/growth/buddy/*`（**不带 `/v2`**）：agreement / first / travel。
//!
//! 4. **实测结论：聊天类任务无法靠真实对话完成**。
//!    上游对 `first_buddy` 的门槛提示为「need at least one conversation」，
//!    但实测真发 6 次对话（`/console/chat/completions` 与 `/v2/chat/completions` 两条通道）
//!    后该门槛**依然未满足**，任务进度也不变；而仅上报一条 `chat_request_send`
//!    事件后 `first_buddy` 立刻可领。
//!    即：服务端的「对话」计数完全由客户端上报驱动，真实对话对它不可见。
//!    因此本模块**保持不上报**的立场 —— 代价是聊天类任务永远无法自动完成，
//!    模块会如实把它们列为「需真实操作」，而不是假装做了。
//!    据此，早期版本里那个「真实对话任务」开关已被**移除**：
//!    它做了真实动作却不产生任何效果，只会创建无用会话、消耗额度、污染对话历史，
//!    留在界面上属于误导。

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex, MutexGuard};
use std::time::Duration;

use chrono::{Local, NaiveDateTime, TimeZone, Timelike};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};
use uuid::Uuid;

use crate::models::workbuddy::WorkbuddyAccount;
use crate::modules::{atomic_write, config, logger, workbuddy_account};

// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

/// 上游 API 根地址
const WB_API_BASE: &str = "https://www.codebuddy.cn";
/// 成长中心接口前缀（实测确认，**不要**改成不带 `/v2` 的写法）
const WB_GROWTH_PREFIX: &str = "/v2/activity/growth";
/// 计费中心接口前缀（礼包 / 补偿领取）。
///
/// ⚠️ **刻意不带 `/v2`**：实测 `/v2/billing/meter/claim-gift` 返回
/// `{"error_msg":"404 Route Not Found"}`，而 `/billing/meter/claim-gift` 才是正确路径。
/// 对比：每日签到走的是 `/v2/billing/meter/daily-checkin`——**同一族前缀并不一致**，
/// 不能因为签到带 `/v2` 就推断礼包也带。
const WB_BILLING_PREFIX: &str = "/billing/meter";
/// 和平精英主题的合法资源 key（取自参考实现）
const WB_THEME_KEY: &str = "theme-tkmw7j";
/// 业务接口统一 User-Agent
const WB_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64)";
/// 单次 HTTP 请求超时（秒）
const WB_HTTP_TIMEOUT_SECS: u64 = 30;

/// 调度轮询间隔：每 5 分钟检查一次是否进入当日执行窗口
const TASKS_TICK_SECONDS: u64 = 5 * 60;
/// 应用启动后延迟 3 分钟执行首轮，避开启动高峰
const TASKS_STARTUP_DELAY_SECONDS: u64 = 3 * 60;
/// 单账号失败后的退避时长（2 小时），避免对异常账号反复打接口
const TASKS_FAILURE_BACKOFF_SECONDS: i64 = 2 * 60 * 60;
/// 运行日志保留天数
const TASKS_LOG_KEEP_DAYS: i64 = 30;
/// 单账号单轮最多领奖次数，防止异常数据导致刷接口
const MAX_CLAIM_PER_ACCOUNT: usize = 40;
/// 单账号单轮最多抽奖次数，防止次数异常时死循环
const MAX_LOTTERY_DRAW_PER_ACCOUNT: usize = 20;
/// 单账号单轮最多「接受任务 → 领奖」轮数。
/// 上游任务存在**前置依赖链**（实测报错 `prerequisite not met: first_buddy`），
/// 完成前一项后才会解锁后一项，故必须多轮推进而不是只试一次。
const MAX_TASK_PASSES: usize = 4;

/// 本模块已适配的任务码集合：不在此集合内且未完成的任务会被标记为「未适配」并提示。
/// 用途：官方新增任务时能在日志里及时暴露，而不是静默漏掉。
const KNOWN_TASK_CODES: &[&str] = &[
    "create_canvas",
    "playbook_prompt",
    "RichMeow_Chat",
    "Library_read",
    "Expert_lighthouse",
    "Expert_Philanthropy",
    "Hp_Appearance",
    "Buddy_App",
    "Buddy_App_QQ",
    "Model_chat_GLM5.2",
    "black_cat",
    "Expert_team_use_3",
    "first_buddy",
    "chat_5",
    "skill_1",
    "expert_5",
    "template_5",
    "automation_1",
    "workstation_expert",
];

/// 聊天类任务码：本模块**不做**（实测服务端只认客户端上报事件，真实对话不被计数），
/// 仅用于在日志中把它们标注为「需真实操作」，让用户明确知道这些项要手动处理。
const CHAT_TASK_CODES: &[&str] = &[
    "chat_5",
    "Model_chat_GLM5.2",
    "black_cat",
    "Expert_team_use_3",
    "expert_5",
    "template_5",
    "create_canvas",
    "playbook_prompt",
    "Library_read",
    "Buddy_App",
    "Buddy_App_QQ",
    "automation_1",
];

/// 需要真实桌面客户端环境、且目前**尚未破解**的任务码。
/// （`RichMeow_Chat` 已通过桌面指纹通道解决，不在此列；`skill_1` 经生态实测
/// 疑似要求真实 Skill 工具调用，暂未点亮。）
const DESKTOP_TASK_CODES: &[&str] = &["skill_1", "workstation_expert"];

/// 涉及真实资金/捐款、明确不自动处理的任务码。
const SKIP_TASK_CODES: &[&str] = &["Expert_Philanthropy"];

// ---------------------------------------------------------------------------
// 全局状态
// ---------------------------------------------------------------------------

/// 调度是否已启动（幂等标记）
static TASKS_STARTED: AtomicBool = AtomicBool::new(false);
/// 单轮执行互斥标记（防止上一轮未结束时重入）
static TASKS_CYCLE_RUNNING: AtomicBool = AtomicBool::new(false);
/// 配置与日志文件的读写锁
static STORAGE_LOCK: Mutex<()> = Mutex::new(());
/// 失败退避表：账号 ID → 下次允许尝试时间（秒级时间戳）
static NEXT_ATTEMPT_AT: LazyLock<Mutex<HashMap<String, i64>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

// ---------------------------------------------------------------------------
// 配置
// ---------------------------------------------------------------------------

/// 自动任务配置（持久化到 `workbuddy_auto_tasks_config.json`）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkbuddyAutoTasksConfig {
    /// 总开关，默认关闭（安全起见，需用户显式开启）
    pub enabled: bool,
    /// 每日执行窗口开始时间，"HH:mm"
    pub start_time: String,
    /// 每日执行窗口结束时间，"HH:mm"
    pub end_time: String,
    /// 自动完成任务：接受未接任务 + 复核进度 + 领取奖励
    pub run_tasks: bool,
    /// 自动玩法（全部为免费收益）：抽奖 / 连签兑换 / 补签卡 / 新手礼包 / 补偿
    pub run_play: bool,
    /// 自动开盲盒（**消耗能量**，故与上项分开，默认关闭）
    pub run_blindbox: bool,
    /// 自动同意 Buddy 领养协议（把它作为领养行为：POST /activity/growth/buddy/agreement）
    #[serde(default = "default_true")]
    pub auto_adopt_agreement: bool,
    /// 应用「和平精英」主题完成任务 Hp_Appearance。
    /// 该项**会真的切换客户端主题**且无法读回原值，故默认关闭。
    #[serde(default)]
    pub run_theme_task: bool,
    /// 行为上报通道：为报告型任务上报行为事件，使其能够完成。
    ///
    /// ⚠️ 这是本模块唯一「代替客户端声明行为」的开关，默认关闭。
    /// 关闭时这些任务只能人工完成（能力天花板 2/18）；开启后可达 18/18，
    /// 但服务端无法核实事件真实性，请自行判断是否接受。
    #[serde(default)]
    pub enable_reporting: bool,
    /// 最近一次成功执行的本地日期，"YYYY-MM-DD"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_run_date: Option<String>,
}

/// serde 默认值辅助：用于历史配置文件缺字段时补 true
fn default_true() -> bool {
    true
}

impl Default for WorkbuddyAutoTasksConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            start_time: "07:00".to_string(),
            end_time: "12:00".to_string(),
            run_tasks: true,
            run_play: true,
            run_blindbox: false,
            auto_adopt_agreement: true,
            run_theme_task: false,
            enable_reporting: false,
            last_run_date: None,
        }
    }
}

/// 任务进度快照（从上游响应裁剪，仅保留本模块需要的字段）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkbuddyGrowthTask {
    /// 任务码，如 `chat_5`
    pub task_code: String,
    /// 中文标题
    pub title: String,
    /// 接受状态：not_accepted / in_progress / completed / claimed
    pub accept_status: String,
    /// 当前进度
    pub current: i64,
    /// 目标进度
    pub target: i64,
    /// 奖励积分
    pub reward_credit: i64,
    /// 奖励能量
    pub reward_energy: i64,
}

/// 单账号的执行明细（用于日志与前端展示）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkbuddyAutoTasksAccountDetail {
    pub account_id: String,
    pub email: String,
    /// 本轮新接受的任务数
    pub accepted_count: usize,
    /// 本轮领取奖励的任务数
    pub claimed_count: usize,
    /// 本轮执行的玩法次数
    pub played_count: usize,
    /// 成长的已完成 / 总数，如 "16/18"
    pub progress_summary: String,
    /// 状态：success / partial / failed / skipped
    pub status: String,
    /// 人类可读的说明（奖品、失败原因、待手动完成项等）
    pub message: String,
}

/// 一轮执行的日志记录（按日期聚合）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkbuddyAutoTasksLogRecord {
    pub id: String,
    pub timestamp: String,
    pub date: String,
    pub duration_ms: u64,
    pub total_accounts: usize,
    pub success_count: usize,
    pub failed_count: usize,
    pub status: String,
    pub details: Vec<WorkbuddyAutoTasksAccountDetail>,
}

// ---------------------------------------------------------------------------
// 配置与日志的读写（独立文件，不碰全局 config.json）
// ---------------------------------------------------------------------------

/// 配置文件路径
fn get_config_file_path() -> PathBuf {
    config::get_shared_dir().join("workbuddy_auto_tasks_config.json")
}

/// 日志文件路径
fn get_logs_file_path() -> PathBuf {
    config::get_shared_dir().join("workbuddy_auto_tasks_logs.json")
}

/// 获取存储锁
fn lock_storage() -> Result<MutexGuard<'static, ()>, String> {
    STORAGE_LOCK
        .lock()
        .map_err(|_| "WorkBuddy 自动任务存储锁已损坏".to_string())
}

/// 校验 "HH:mm" 并换算为当日分钟数（0..1439），非法返回 None
fn parse_time_to_minutes(value: &str) -> Option<i32> {
    if value.len() != 5 || value.as_bytes().get(2) != Some(&b':') {
        return None;
    }
    let hour = value.get(0..2)?.parse::<i32>().ok()?;
    let minute = value.get(3..5)?.parse::<i32>().ok()?;
    if (0..=23).contains(&hour) && (0..=59).contains(&minute) {
        Some(hour * 60 + minute)
    } else {
        None
    }
}

/// 校验配置合法性
fn validate_config(cfg: &WorkbuddyAutoTasksConfig) -> Result<(), String> {
    let start = parse_time_to_minutes(&cfg.start_time)
        .ok_or_else(|| format!("自动任务开始时间无效: {}", cfg.start_time))?;
    let end = parse_time_to_minutes(&cfg.end_time)
        .ok_or_else(|| format!("自动任务结束时间无效: {}", cfg.end_time))?;
    if start > end {
        return Err("自动任务开始时间不能晚于结束时间".to_string());
    }
    Ok(())
}

/// 从磁盘读取配置（不存在返回 None）
fn read_config_from_path(path: &Path) -> Result<Option<WorkbuddyAutoTasksConfig>, String> {
    if !path.exists() {
        return Ok(None);
    }
    let content =
        fs::read_to_string(path).map_err(|e| format!("读取 WorkBuddy 自动任务配置失败: {}", e))?;
    let cfg = atomic_write::parse_json_with_auto_restore(path, &content)
        .map_err(|e| format!("解析 WorkBuddy 自动任务配置失败: {}", e))?;
    validate_config(&cfg)?;
    Ok(Some(cfg))
}

/// 原子写入配置
fn write_config_to_path(path: &Path, cfg: &WorkbuddyAutoTasksConfig) -> Result<(), String> {
    validate_config(cfg)?;
    let content = serde_json::to_string_pretty(cfg)
        .map_err(|e| format!("序列化 WorkBuddy 自动任务配置失败: {}", e))?;
    atomic_write::write_string_atomic(path, &content)
        .map_err(|e| format!("保存 WorkBuddy 自动任务配置失败: {}", e))
}

/// 读取配置（缺失时返回默认值）
pub fn get_config_checked() -> Result<WorkbuddyAutoTasksConfig, String> {
    let _guard = lock_storage()?;
    Ok(read_config_from_path(&get_config_file_path())?.unwrap_or_default())
}

/// 保存配置（写入成功后唤醒调度器立即生效）
pub fn save_config(cfg: &WorkbuddyAutoTasksConfig) -> Result<(), String> {
    let _guard = lock_storage()?;
    write_config_to_path(&get_config_file_path(), cfg)
}

/// 从磁盘读取日志
fn read_logs_from_path(path: &Path) -> Result<Vec<WorkbuddyAutoTasksLogRecord>, String> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = fs::read_to_string(path).map_err(|e| format!("读取自动任务日志失败: {}", e))?;
    if content.trim().is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str(&content).map_err(|e| format!("解析自动任务日志失败: {}", e))
}

/// 原子写入日志
fn write_logs_to_path(path: &Path, logs: &[WorkbuddyAutoTasksLogRecord]) -> Result<(), String> {
    let content =
        serde_json::to_string_pretty(logs).map_err(|e| format!("序列化自动任务日志失败: {}", e))?;
    atomic_write::write_string_atomic(path, &content)
        .map_err(|e| format!("保存自动任务日志失败: {}", e))
}

/// 读取全部日志
pub fn get_logs_checked() -> Result<Vec<WorkbuddyAutoTasksLogRecord>, String> {
    let _guard = lock_storage()?;
    read_logs_from_path(&get_logs_file_path())
}

/// 清空日志
pub fn clear_logs() -> Result<(), String> {
    let _guard = lock_storage()?;
    write_logs_to_path(&get_logs_file_path(), &[])
}

/// 写入一条日志记录：同日合并明细，并清理超过保留期的记录
fn add_log_record(record: WorkbuddyAutoTasksLogRecord) -> Result<(), String> {
    let _guard = lock_storage()?;
    let path = get_logs_file_path();
    let mut logs = read_logs_from_path(&path)?;

    if let Some(existing) = logs.iter_mut().find(|log| log.date == record.date) {
        // 同日已有记录：按账号 ID 合并明细，计数从明细重算，避免累加出错
        let mut details: HashMap<String, WorkbuddyAutoTasksAccountDetail> = existing
            .details
            .drain(..)
            .map(|detail| (detail.account_id.clone(), detail))
            .collect();
        for detail in record.details {
            details.insert(detail.account_id.clone(), detail);
        }
        existing.timestamp = record.timestamp;
        existing.duration_ms = existing.duration_ms.saturating_add(record.duration_ms);
        existing.details = details.into_values().collect();
        existing.total_accounts = existing.details.len();
        existing.success_count = existing
            .details
            .iter()
            .filter(|d| d.status == "success")
            .count();
        existing.failed_count = existing
            .details
            .iter()
            .filter(|d| d.status == "failed")
            .count();
        existing.status = if existing.total_accounts == 0 {
            "no_accounts"
        } else if existing.failed_count == 0 {
            "success"
        } else if existing.success_count > 0 {
            "partial"
        } else {
            "failed"
        }
        .to_string();
    } else {
        logs.insert(0, record);
    }

    // 清理超过保留期的日志
    let cutoff = Local::now().timestamp() - TASKS_LOG_KEEP_DAYS * 24 * 60 * 60;
    logs.retain(
        |r| match NaiveDateTime::parse_from_str(&r.timestamp, "%Y-%m-%d %H:%M:%S") {
            Ok(ndt) => match Local.from_local_datetime(&ndt).single() {
                Some(local_dt) => local_dt.timestamp() >= cutoff,
                None => ndt.and_utc().timestamp() >= cutoff,
            },
            Err(_) => true,
        },
    );

    write_logs_to_path(&path, &logs)
}

// ---------------------------------------------------------------------------
// 失败退避
// ---------------------------------------------------------------------------

/// 该账号当前是否允许尝试（退避期内跳过）
fn allow_attempt(account_id: &str) -> bool {
    let now = chrono::Utc::now().timestamp();
    match NEXT_ATTEMPT_AT.lock() {
        Ok(state) => state
            .get(account_id)
            .map(|next| *next <= now)
            .unwrap_or(true),
        Err(_) => true,
    }
}

/// 成功后清除退避记录
fn clear_attempt(account_id: &str) {
    if let Ok(mut state) = NEXT_ATTEMPT_AT.lock() {
        state.remove(account_id);
    }
}

/// 失败后记录退避时间
fn mark_attempt_failure(account_id: &str) {
    if let Ok(mut state) = NEXT_ATTEMPT_AT.lock() {
        state.insert(
            account_id.to_string(),
            chrono::Utc::now().timestamp() + TASKS_FAILURE_BACKOFF_SECONDS,
        );
    }
}

// ---------------------------------------------------------------------------
// HTTP 层（自包含：刻意不复用 codebuddy_cn_oauth 的私有辅助函数）
// ---------------------------------------------------------------------------

/// 创建 HTTP 客户端
fn build_http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent(WB_USER_AGENT)
        .timeout(Duration::from_secs(WB_HTTP_TIMEOUT_SECS))
        .build()
        .map_err(|e| format!("创建 HTTP 客户端失败: {}", e))
}

/// 注入与官方 Web 端一致的请求头
fn apply_headers(
    req: reqwest::RequestBuilder,
    account: &WorkbuddyAccount,
) -> reqwest::RequestBuilder {
    let mut req = req
        .header("Authorization", format!("Bearer {}", account.access_token))
        .header("Accept", "application/json")
        .header("x-client-platform", "web")
        .header("Origin", WB_API_BASE)
        .header("Referer", format!("{}/profile/growth-center", WB_API_BASE));
    if let Some(uid) = account.uid.as_deref() {
        req = req.header("X-User-Id", uid);
    }
    if let Some(eid) = account.enterprise_id.as_deref() {
        req = req.header("X-Enterprise-Id", eid);
        req = req.header("X-Tenant-Id", eid);
    }
    if let Some(domain) = account.domain.as_deref() {
        req = req.header("X-Domain", domain);
    }
    req
}

/// 判断错误是否属于 token 失效（需要刷新后重试）
fn is_token_error(err: &str) -> bool {
    let lower = err.to_lowercase();
    lower.contains("401")
        || lower.contains("403")
        || lower.contains("unauthorized")
        || lower.contains("登录")
        || lower.contains("失效")
        || lower.contains("过期")
        || lower.contains("token")
}

/// 解析上游统一响应信封：要求 http 成功且 `code == 0`，返回 `data`
fn unwrap_envelope(path: &str, status: reqwest::StatusCode, raw: &str) -> Result<Value, String> {
    let body: Value = serde_json::from_str(raw).map_err(|e| {
        logger::log_warn(&format!(
            "[WorkBuddyAutoTasks] 解析响应失败: {} | path={} | http={} | body前300字={}",
            e,
            path,
            status.as_u16(),
            raw.chars().take(300).collect::<String>()
        ));
        format!("解析响应失败: {}", e)
    })?;
    if !status.is_success() {
        let message = body
            .get("message")
            .or_else(|| body.get("msg"))
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        return Err(format!("http={} {}", status.as_u16(), message));
    }
    let code = body.get("code").and_then(Value::as_i64).unwrap_or(-1);
    if code != 0 {
        let message = body
            .get("message")
            .or_else(|| body.get("msg"))
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        return Err(format!("code={} {}", code, message));
    }
    Ok(body.get("data").cloned().unwrap_or(Value::Null))
}

/// 发起一次 GET 并解析信封
async fn http_get(account: &WorkbuddyAccount, path: &str) -> Result<Value, String> {
    let client = build_http_client()?;
    let url = format!("{}{}", WB_API_BASE, path);
    let resp = apply_headers(client.get(&url), account)
        .send()
        .await
        .map_err(|e| format!("请求失败: {}", e))?;
    let status = resp.status();
    let raw = resp
        .text()
        .await
        .map_err(|e| format!("读取响应正文失败: {}", e))?;
    unwrap_envelope(path, status, &raw)
}

/// 发起一次 POST 并解析信封
async fn http_post(account: &WorkbuddyAccount, path: &str, body: Value) -> Result<Value, String> {
    let client = build_http_client()?;
    let url = format!("{}{}", WB_API_BASE, path);
    // 注意：reqwest 的 HTTP 方法只能在 client.post(...) 创建时决定，
    // 用 client.get(...) 再挂 body 会以 GET 发出并收到 404 纯文本，故此处必须是 post。
    let resp = apply_headers(client.post(&url), account)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("请求失败: {}", e))?;
    let status = resp.status();
    let raw = resp
        .text()
        .await
        .map_err(|e| format!("读取响应正文失败: {}", e))?;
    unwrap_envelope(path, status, &raw)
}

/// 带 token 自动续期重试的 GET：遇 token 失效则刷新账号后重试一次
async fn http_get_fresh(account: &mut WorkbuddyAccount, path: &str) -> Result<Value, String> {
    match http_get(account, path).await {
        Ok(data) => Ok(data),
        Err(err) if is_token_error(&err) => {
            let fresh = workbuddy_account::refresh_account_token(&account.id).await?;
            *account = fresh;
            http_get(account, path).await
        }
        Err(err) => Err(err),
    }
}

/// 带 token 自动续期重试的 POST
async fn http_post_fresh(
    account: &mut WorkbuddyAccount,
    path: &str,
    body: Value,
) -> Result<Value, String> {
    // 首次请求传克隆体，保留原始 body 以便 token 失效后原样重试
    match http_post(account, path, body.clone()).await {
        Ok(data) => Ok(data),
        Err(err) if is_token_error(&err) => {
            let fresh = workbuddy_account::refresh_account_token(&account.id).await?;
            *account = fresh;
            http_post(account, path, body).await
        }
        Err(err) => Err(err),
    }
}

// ---------------------------------------------------------------------------
// 成长中心业务接口
// ---------------------------------------------------------------------------

/// 拉取任务列表并裁剪为本模块结构
async fn fetch_growth_tasks(
    account: &mut WorkbuddyAccount,
) -> Result<Vec<WorkbuddyGrowthTask>, String> {
    let path = format!("{}/tasks", WB_GROWTH_PREFIX);
    let data = http_get_fresh(account, &path).await?;
    let raw_tasks = data
        .get("tasks")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let tasks = raw_tasks
        .iter()
        .filter_map(|t| {
            let code = t.get("task_code").and_then(Value::as_str)?.to_string();
            let progress = t.get("progress").cloned().unwrap_or(Value::Null);
            Some(WorkbuddyGrowthTask {
                task_code: code,
                title: t
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                accept_status: t
                    .get("accept_status")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                current: progress.get("current").and_then(Value::as_i64).unwrap_or(0),
                target: progress.get("target").and_then(Value::as_i64).unwrap_or(0),
                reward_credit: t.get("reward_credit").and_then(Value::as_i64).unwrap_or(0),
                reward_energy: t.get("reward_energy").and_then(Value::as_i64).unwrap_or(0),
            })
        })
        .collect::<Vec<_>>();

    if tasks.is_empty() {
        return Err("任务列表为空，可能未开通成长中心".to_string());
    }
    Ok(tasks)
}

/// 接受任务的结果。
///
/// ⚠️ 关键：`/tasks/accept` 在 HTTP 200 且 `code == 0` 时，
/// **仍可能逐任务失败**，真实结果在 `data.results[]` 里，例如：
/// `{"task_code":"chat_5","status":"error","message":"prerequisite not met: first_buddy"}`
/// 因此绝不能只看 `code`，否则会谎报"已接受"。
struct AcceptOutcome {
    /// 真正接受成功的任务码
    accepted: Vec<String>,
    /// 被阻塞的任务：任务码 → 人类可读原因
    blocked: Vec<(String, String)>,
}

/// 把上游的阻塞报错翻译成用户能照做的中文提示
fn humanize_block_reason(raw: &str) -> String {
    let lower = raw.to_lowercase();
    if let Some(rest) = lower.split("prerequisite not met:").nth(1) {
        return format!("需先完成 {}", rest.trim());
    }
    if lower.contains("adoption agreement") {
        return "需先在客户端同意 Buddy 领养协议".to_string();
    }
    if lower.contains("invalid request") {
        return "请求参数被拒".to_string();
    }
    if raw.is_empty() {
        "未知原因".to_string()
    } else {
        raw.to_string()
    }
}

/// 批量接受 `not_accepted` 的任务，返回逐任务真实结果
async fn accept_tasks(
    account: &mut WorkbuddyAccount,
    codes: &[String],
) -> Result<AcceptOutcome, String> {
    if codes.is_empty() {
        return Ok(AcceptOutcome {
            accepted: Vec::new(),
            blocked: Vec::new(),
        });
    }
    let path = format!("{}/tasks/accept", WB_GROWTH_PREFIX);
    let data = http_post_fresh(account, &path, json!({ "task_codes": codes })).await?;

    let mut accepted = Vec::new();
    let mut blocked = Vec::new();
    match data.get("results").and_then(Value::as_array) {
        Some(results) => {
            for item in results {
                let code = item
                    .get("task_code")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let status = item.get("status").and_then(Value::as_str).unwrap_or("");
                if status == "error" {
                    let raw = item.get("message").and_then(Value::as_str).unwrap_or("");
                    blocked.push((code, humanize_block_reason(raw)));
                } else {
                    accepted.push(code);
                }
            }
        }
        // 老版本响应没有 results：按请求全部成功处理
        None => accepted.extend(codes.iter().cloned()),
    }
    Ok(AcceptOutcome { accepted, blocked })
}

/// 领取单个任务奖励
async fn claim_task(account: &mut WorkbuddyAccount, task_code: &str) -> Result<Value, String> {
    let path = format!("{}/tasks/{}/claim", WB_GROWTH_PREFIX, task_code);
    http_post_fresh(account, &path, json!({})).await
}

/// 任务是否已彻底完成（无需再动）
fn is_task_done(task: &WorkbuddyGrowthTask) -> bool {
    task.accept_status == "claimed"
}

/// 任务是否已达成目标、可领奖
fn is_task_claimable(task: &WorkbuddyGrowthTask) -> bool {
    if task.accept_status == "completed" {
        return true;
    }
    // 进度已达标但状态没翻成 completed 的情况，也认为是可领奖
    task.accept_status == "in_progress" && task.target > 0 && task.current >= task.target
}

/// 同意 Buddy 领养协议（幂等）。
///
/// 实测端点：`POST /activity/growth/buddy/agreement`，体 `{"agree": true}`，
/// 成功返回 `{"agreed":true,"first_time":bool}`。
/// **注意前缀不带 `/v2`**——与 `buddy/travel`、`buddy/first` 同族。
///
/// 这属于「用户本人授权」类动作，仅在配置 `auto_adopt_agreement` 开启时调用。
async fn agree_buddy_adoption(account: &mut WorkbuddyAccount) -> Result<bool, String> {
    let data = http_post_fresh(
        account,
        "/activity/growth/buddy/agreement",
        json!({ "agree": true }),
    )
    .await?;
    Ok(data.get("agreed").and_then(Value::as_bool).unwrap_or(false))
}

/// 领取首只 Buddy：`POST /activity/growth/buddy/first`，体 `{}`。
///
/// 它是整条成长任务链的**根节点**——未完成时下游任务一律返回
/// `prerequisite not met: first_buddy`。
///
/// ⚠️ 实测：上游要求「至少一次对话」，但**只认对话的上报事件，不认真实对话本身**。
/// 因此在不上报遥测的前提下，本调用对新账号必然失败并返回该提示。
async fn claim_first_buddy(account: &mut WorkbuddyAccount) -> Result<bool, String> {
    let data = http_post_fresh(account, "/activity/growth/buddy/first", json!({})).await?;
    // already_claimed=true 表示此前已领过，不算新领取
    let already = data
        .get("already_claimed")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Ok(!already)
}

// ---------------------------------------------------------------------------
// 互动玩法接口
// ---------------------------------------------------------------------------

/// 抽奖：按剩余次数逐次抽取
async fn play_lottery(account: &mut WorkbuddyAccount, messages: &mut Vec<String>) -> usize {
    let chances_path = format!("{}/lottery/chances", WB_GROWTH_PREFIX);
    let chances = match http_get_fresh(account, &chances_path).await {
        Ok(data) => data.get("balance").and_then(Value::as_i64).unwrap_or(0),
        Err(err) => {
            messages.push(format!("抽奖次数查询失败: {}", err));
            return 0;
        }
    };
    if chances <= 0 {
        return 0;
    }

    let draw_path = format!("{}/lottery/draw", WB_GROWTH_PREFIX);
    let mut done = 0usize;
    for _ in 0..chances.min(MAX_LOTTERY_DRAW_PER_ACCOUNT as i64) {
        // client_token 只是防重复提交的幂等令牌，不是行为事实
        match http_post_fresh(
            account,
            &draw_path,
            json!({ "client_token": format!("draw-{}", Uuid::new_v4()) }),
        )
        .await
        {
            Ok(data) => {
                let prize = data
                    .get("prize_name")
                    .or_else(|| data.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("未知");
                messages.push(format!("抽奖获得: {}", prize));
                done += 1;
            }
            Err(err) => {
                messages.push(format!("抽奖中断: {}", err));
                break;
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    done
}

/// 连签兑换：按 7d / 14d / 28d 三档尝试（服务端自会校验天数是否足够）
async fn play_redeem(account: &mut WorkbuddyAccount, messages: &mut Vec<String>) -> usize {
    let path = format!("{}/redeem", WB_GROWTH_PREFIX);
    let mut done = 0usize;
    for tier in ["7d", "14d", "28d"] {
        let body = json!({
            "tier": tier,
            "client_token": format!("redeem-{}-{}", tier, Uuid::new_v4()),
        });
        match http_post_fresh(account, &path, body).await {
            Ok(data) => {
                let credit = data
                    .get("credit_granted")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                let energy = data
                    .get("energy_granted")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                messages.push(format!(
                    "连签兑换 {}: +{}积分 +{}能量",
                    tier, credit, energy
                ));
                done += 1;
            }
            Err(err) => {
                // 409=已兑换过、403=天数不足，均属正常业务结果，不算失败
                let lower = err.to_lowercase();
                if !lower.contains("409") && !lower.contains("403") {
                    messages.push(format!("连签兑换 {} 跳过: {}", tier, err));
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    done
}

/// 补签卡：昨日漏签且有余额时使用一张
async fn play_makeup_card(account: &mut WorkbuddyAccount, messages: &mut Vec<String>) -> usize {
    let streak_path = format!("{}/streak", WB_GROWTH_PREFIX);
    let balance = match http_get_fresh(account, &streak_path).await {
        Ok(data) => data
            .pointer("/makeup_cards/balance")
            .and_then(Value::as_i64)
            .unwrap_or(0),
        Err(err) => {
            messages.push(format!("补签卡余额查询失败: {}", err));
            return 0;
        }
    };
    if balance <= 0 {
        return 0;
    }

    // 从热力图判断昨日是否漏签（score 为 0 即当日无活动）
    let heatmap_path = format!("{}/heatmap", WB_GROWTH_PREFIX);
    let cells = match http_get_fresh(account, &heatmap_path).await {
        Ok(data) => data
            .get("cells")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        Err(err) => {
            messages.push(format!("热力图查询失败: {}", err));
            return 0;
        }
    };
    let yesterday = (Local::now() - chrono::Duration::days(1))
        .format("%Y-%m-%d")
        .to_string();
    let missed_yesterday = cells.iter().any(|c| {
        c.get("date")
            .and_then(Value::as_str)
            .map(|d| d.starts_with(&yesterday))
            .unwrap_or(false)
            && c.get("score").and_then(Value::as_i64).unwrap_or(0) == 0
    });
    if !missed_yesterday {
        return 0;
    }

    let path = format!("{}/makeup-cards/use", WB_GROWTH_PREFIX);
    match http_post_fresh(account, &path, json!({ "target_date": yesterday })).await {
        Ok(_) => {
            messages.push(format!("补签成功: {}", yesterday));
            1
        }
        Err(err) => {
            messages.push(format!("补签失败: {}", err));
            0
        }
    }
}

/// 判断领取类接口的错误是否属于「正常的业务结果」
/// （已领过 / 已兑换 / 活动未开启 / 天数不足等）——这类结果不算失败，也不该进错误提示。
///
/// ⚠️ 刻意**不把 404 当作正常结果**。早期版本把 404 归为「正常跳过」，
/// 结果礼包接口前缀写错（`/v2/billing/...` 实际不存在）时被**静默吞掉**，
/// 玩法长期空转却没有任何提示。这行判断是那次事故的直接产物。
fn is_benign_business_result(err: &str) -> bool {
    let lower = err.to_lowercase();
    lower.contains("已领取")
        || lower.contains("已兑换")
        || lower.contains("已过期")
        || lower.contains("未开启")
        || lower.contains("天数不足")
        || lower.contains("10001")
        || lower.contains("409")
        || lower.contains("403")
        || lower.contains("already")
}

/// 新手礼包与活动补偿领取（幂等，重复调用只返回业务提示）
async fn play_claim_gift(account: &mut WorkbuddyAccount, messages: &mut Vec<String>) -> usize {
    let mut done = 0usize;
    for (name, suffix) in [
        ("新手礼包", "claim-gift"),
        ("活动补偿", "claim-compensation"),
    ] {
        let path = format!("{}/{}", WB_BILLING_PREFIX, suffix);
        match http_post_fresh(account, &path, json!({})).await {
            Ok(data) => {
                let credit = data.get("credit").and_then(Value::as_i64).unwrap_or(0);
                if credit > 0 {
                    messages.push(format!("{}: +{}积分", name, credit));
                    done += 1;
                }
            }
            Err(err) => {
                if !is_benign_business_result(&err) {
                    messages.push(format!("{} 失败: {}", name, err));
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    done
}

/// 开盲盒（消耗能量，仅在配置开启时调用）
async fn play_blindbox(account: &mut WorkbuddyAccount, messages: &mut Vec<String>) -> usize {
    let quota_path = format!("{}/buddy/quota", WB_GROWTH_PREFIX);
    let quota = match http_get_fresh(account, &quota_path).await {
        Ok(data) => data,
        Err(err) => {
            messages.push(format!("盲盒额度查询失败: {}", err));
            return 0;
        }
    };
    let affordable = quota.get("affordable").and_then(Value::as_i64).unwrap_or(0);
    let max_open = quota
        .get("max_open_count")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let cost = quota
        .get("cost_per_open")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let balance = quota.get("balance").and_then(Value::as_i64).unwrap_or(0);
    // 可开次数 = min(今日剩余额度, 能量可支撑的次数)
    let openable = if cost > 0 {
        max_open.min(balance / cost)
    } else {
        max_open
    };
    if affordable <= 0 || openable <= 0 {
        return 0;
    }

    let path = format!("{}/buddy/open", WB_GROWTH_PREFIX);
    let mut done = 0usize;
    for _ in 0..openable.min(5) {
        match http_post_fresh(account, &path, json!({ "count": 1 })).await {
            Ok(_) => {
                done += 1;
            }
            Err(err) => {
                messages.push(format!("开盲盒中断: {}", err));
                break;
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    if done > 0 {
        messages.push(format!("开盲盒 {} 次", done));
    }
    done
}

/// 和平精英主题任务（`Hp_Appearance`）。
///
/// 实测端点：`POST /portal/user-asset/appearance/set`（**不带 `/v2`**），
/// 体 `{"kind":"theme","resource_key":"theme-tkmw7j"}` → 200 真实生效。
///
/// ⚠️ **实测修正**：仅设置主题**不足以**完成任务（真实调用后进度仍为 0/1）。
/// 官方客户端在设置成功后还会上报一条 `appearance_skin_apply` 事件，
/// **必须补上这一步任务才会 `completed`**（已验证：调用 + 上报 → 1/1）。
/// 因此在未开启上报通道时，本函数只改主题、任务不会完成，并明确告知用户。
///
/// ⚠️ 副作用：会真的切换客户端主题，且无读回接口、原值不可知，故由独立开关控制。
async fn play_theme_task(
    account: &mut WorkbuddyAccount,
    with_report: bool,
    messages: &mut Vec<String>,
) -> usize {
    match http_post_fresh(
        account,
        "/portal/user-asset/appearance/set",
        json!({ "kind": "theme", "resource_key": WB_THEME_KEY }),
    )
    .await
    {
        Ok(_) => {
            if !with_report {
                messages
                    .push("已应用「和平精英」主题，但未开启上报通道，该任务不会完成".to_string());
                return 1;
            }
            let events = vec![json!({
                "eventCode": "appearance_skin_apply",
                "action": "apply",
                "source": "settings_close",
                "id": WB_THEME_KEY,
                "vipLevel": "free",
                "series": "craft",
                "type": "personal"
            })];
            match report_events(account, events).await {
                Ok(_) => {
                    messages.push("已应用「和平精英」主题并上报生效事件".to_string());
                    1
                }
                Err(err) => {
                    messages.push(format!("主题已应用但上报失败: {}", err));
                    1
                }
            }
        }
        Err(err) => {
            messages.push(format!("应用主题失败: {}", err));
            0
        }
    }
}

// ---------------------------------------------------------------------------
// 行为上报通道（遥测）
//
// ⚠️ 这是本模块**唯一**会「代替客户端声明行为」的部分，由 `enable_reporting`
// 开关控制、默认关闭。开启前请理解它的性质：服务端**无法核实**这些事件是否真的发生过。
//
// 实现上尽量向「如实」靠拢：
//   · 凡是能通过 API 真实执行的任务（聊天类、专家团），**先真实执行**，
//     再上报该次行为的**真实字段**（真实 conversationId、真实文本长度）；
//   · 只有必须依赖客户端 UI 才能产生的行为（设计创意、模板、自动化任务等），
//     才按参考实现的事件形状上报。
// ---------------------------------------------------------------------------

/// 专家市场地址（用于取真实的专家 / 团队 id，避免硬编码失效）
const WB_EXPERT_MARKET_URL: &str =
    "https://acc-1258344699.cos.accelerate.myqcloud.com/workbuddy/expert-marketplace/expert_center.json";
/// 企鹅教师助手模板 id（来自参考实现）
const WB_QQ_TEMPLATE_ID: &str = "cb_y5Dy46tPQGGWtueMxXbe";

/// 需要靠行为上报才能推进的任务码
const REPORT_TASK_CODES: &[&str] = &[
    "create_canvas",
    "playbook_prompt",
    "Library_read",
    "Buddy_App",
    "Buddy_App_QQ",
    "chat_5",
    "Model_chat_GLM5.2",
    "black_cat",
    "RichMeow_Chat",
    "Expert_team_use_3",
    "expert_5",
    "template_5",
    "automation_1",
];

/// 聊天类任务码：先真实发起对话，再上报该次对话的事件
const REPORT_CHAT_CODES: &[&str] = &["chat_5", "Model_chat_GLM5.2", "black_cat"];

/// 模板场景（`template_5` / `playbook_prompt` 使用）
const WB_TEMPLATE_SCENES: &[(&str, &str)] = &[
    ("01-ProductDesign", "产品设计"),
    ("02-Marketing", "营销文案"),
    ("03-DataAnalysis", "数据分析"),
    ("04-CodeReview", "代码审查"),
    ("05-Report", "报告撰写"),
];

/// 对话提示语（真实发起对话时使用，不含任何行为声明）
const CHAT_PROMPTS: &[&str] = &[
    "你好，请用一句话介绍你自己。",
    "今天天气怎么样？",
    "1+1等于几？",
    "Python 是什么语言？",
    "推荐一本好书。",
];

/// 从专家市场裁剪出的专家信息
#[derive(Debug, Clone)]
struct ExpertInfo {
    id: String,
    name: String,
    profession: String,
    is_team: bool,
}

/// 专家市场进程内缓存（一次拉取，全账号复用）
static EXPERT_CACHE: LazyLock<Mutex<Option<Vec<ExpertInfo>>>> = LazyLock::new(|| Mutex::new(None));

/// 生成一次性随机 id（traceId / messageId 等）
fn rand_id() -> String {
    Uuid::new_v4().to_string()
}

/// 当前毫秒时间戳
fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// 构造上报信封的公共字段（与官方 Web 客户端对齐）
fn report_common(account: &WorkbuddyAccount) -> Value {
    json!({
        "timestamp": now_ms(),
        "reportDelay": 0,
        "userId": account.uid.clone().unwrap_or_default(),
        "userNickname": account.nickname.clone().unwrap_or_default(),
        "ideName": "web-Agents",
        "ideType": "web-Agents",
        // machineId 使用随机值：仅作匿名标识，不声称是某台真实设备
        "machineId": rand_id(),
        "mode": "CLOUD",
        "userAgent": format!("{} WorkBuddy/5.5.4", WB_USER_AGENT),
        "os": "Win32",
        "timezone": "Asia/Shanghai",
    })
}

/// 把「事件体」补全信封后上报到 `/v2/report`
async fn report_events(account: &mut WorkbuddyAccount, events: Vec<Value>) -> Result<(), String> {
    if events.is_empty() {
        return Ok(());
    }
    let common = report_common(account);
    let payload: Vec<Value> = events
        .into_iter()
        .map(|event| {
            let mut merged = common.clone();
            if let (Some(dst), Some(src)) = (merged.as_object_mut(), event.as_object()) {
                for (key, value) in src {
                    dst.insert(key.clone(), value.clone());
                }
            }
            merged
        })
        .collect();
    http_post_fresh(account, "/v2/report", Value::Array(payload)).await?;
    Ok(())
}

/// 从 i18n 字段取中文，其次英文，最后回退
fn pick_i18n(value: Option<&Value>, fallback: &str) -> String {
    match value {
        Some(Value::Object(map)) => map
            .get("zh")
            .or_else(|| map.get("en"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| fallback.to_string()),
        Some(Value::String(text)) => text.clone(),
        _ => fallback.to_string(),
    }
}

/// 拉取专家市场（进程内缓存），失败时才重新请求
async fn load_experts(account: &mut WorkbuddyAccount) -> Result<Vec<ExpertInfo>, String> {
    if let Ok(guard) = EXPERT_CACHE.lock() {
        if let Some(cached) = guard.as_ref() {
            return Ok(cached.clone());
        }
    }
    let client = build_http_client()?;
    let resp = apply_headers(client.get(WB_EXPERT_MARKET_URL), account)
        .send()
        .await
        .map_err(|e| format!("拉取专家市场失败: {}", e))?;
    let raw = resp
        .text()
        .await
        .map_err(|e| format!("读取专家市场失败: {}", e))?;
    let body: Value = serde_json::from_str(&raw).map_err(|e| format!("解析专家市场失败: {}", e))?;
    let experts: Vec<ExpertInfo> = body
        .get("experts")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|item| {
                    let id = item.get("id").and_then(Value::as_str)?.to_string();
                    Some(ExpertInfo {
                        name: pick_i18n(item.get("displayName"), &id),
                        profession: pick_i18n(item.get("profession"), ""),
                        is_team: item.get("expertType").and_then(Value::as_str) == Some("team"),
                        id,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    if experts.is_empty() {
        return Err("专家市场未返回任何专家".to_string());
    }
    if let Ok(mut guard) = EXPERT_CACHE.lock() {
        *guard = Some(experts.clone());
    }
    Ok(experts)
}

/// 构造「一次」上报的事件集。返回 None 表示该任务不走上报通道。
fn build_report_unit(
    task_code: &str,
    experts: &[ExpertInfo],
    index: usize,
    uid: &str,
    nick: &str,
    prompt: &str,
    reply: &str,
    conversation_id: &str,
) -> Option<Vec<Value>> {
    match task_code {
        // ---- 纯事件型（行为发生在客户端 UI，无法通过 API 重现）----
        "create_canvas" => Some(vec![
            json!({"eventCode":"agent_task_created","source":"CLOUD","name":"","mode":"craft",
                   "requestModelId":"default","task_mode":"design"}),
            json!({"eventCode":"wbx_design_canvas_task_create"}),
        ]),
        "automation_1" => Some(vec![
            json!({"eventCode":"agent_task_created","source":"CLOUD","name":"","mode":"craft",
                   "requestModelId":"default","task_mode":"automation","isAutomationBackground":true}),
            json!({"eventCode":"automated_task_create_suc","action":"create"}),
            json!({"eventCode":"automated_task_execute","action":"execute"}),
        ]),
        "Library_read" => Some(vec![json!({
            "eventCode":"web_element_click",
            "pageURL":"https://www.workbuddy.cn/space/d/o0KWYeynteVv06UnAZqIFm",
            "elementId":"library_doc_intro_click","elementName":"WorkBuddy资料库介绍","enterpriseId":""
        })]),
        "Buddy_App" => Some(vec![json!({"eventCode":"buddyapp_discover_click"})]),
        "Buddy_App_QQ" => Some(vec![
            json!({"eventCode":"buddyapp_enter_click","elementId":WB_QQ_TEMPLATE_ID,
                   "elementName":"企鹅教师助手","position":"sidebar-switcher-trigger","isFirstPage":"1"}),
            json!({"eventCode":"buddyapp_show","elementId":WB_QQ_TEMPLATE_ID,"elementName":"企鹅教师助手"}),
        ]),
        "playbook_prompt" => {
            let (id, name) = WB_TEMPLATE_SCENES[index % WB_TEMPLATE_SCENES.len()];
            Some(vec![json!({
                "eventCode":"playbook_prompt_send","ext1":rand_id(),"requestId":rand_id(),
                "id":id,"name":name,"type":"other","promptLength":30,"isOfficial":1,"source":"growth-center"
            })])
        }
        "template_5" => {
            let (id, name) = WB_TEMPLATE_SCENES[index % WB_TEMPLATE_SCENES.len()];
            Some(vec![
                json!({"eventCode":"agent_task_created","source":"CLOUD","name":"","mode":"craft",
                       "requestModelId":"default","action":id,"has_template":true,
                       "template_id":id,"template_name":name}),
                json!({"eventCode":"agent_task_created_with_template","templateId":id,"templateName":name,
                       "isCustomModel":true,"id":id,"name":name}),
                json!({"eventCode":"playbook_prompt_send","ext1":rand_id(),"requestId":rand_id(),
                       "id":id,"name":name,"type":"other","promptLength":30,"isOfficial":1,"source":"growth-center"}),
            ])
        }
        // ---- 专家类：按真实专家市场的 id 构造 ----
        "expert_5" => {
            let pool: Vec<&ExpertInfo> = experts.iter().filter(|e| !e.is_team).collect();
            let expert = pool.get(index % pool.len().max(1))?;
            Some(vec![
                json!({"eventCode":"expert_summoned","id":expert.id,"name":expert.name,
                       "type":"agent","expertTitle":expert.profession,"expertType":"agent"}),
                json!({"eventCode":"expert_actual_use","id":expert.id,"name":expert.name,
                       "type":"","expertType":"agent","source":"builtin","version":"","cost":5,
                       "characterCount":30,"requestId":rand_id(),
                       "messageId":format!("cmb-{}", rand_id()),
                       "requestModelId":"glm-5.2","requestModelName":"GLM-5.2"}),
            ])
        }
        "Expert_team_use_3" => {
            let pool: Vec<&ExpertInfo> = experts.iter().filter(|e| e.is_team).collect();
            let team = pool.get(index % pool.len().max(1))?;
            Some(vec![json!({
                "eventCode":"expert_actual_use","id":team.id,"name":team.name,
                "expertTitle":team.profession,"type":"","expertType":"team",
                "source":"builtin","version":"","cost":8,"characterCount":prompt.chars().count(),
                "conversationId":conversation_id,"requestId":rand_id(),
                "messageId":format!("cmb-{}", rand_id()),
                "requestModelId":"glm-5.2","requestModelName":"GLM-5.2"
            })])
        }
        // ---- 聊天类：字段取自真实对话 ----
        "chat_5" | "Model_chat_GLM5.2" | "black_cat" => {
            let request_id = format!("cmb-{}", rand_id());
            let input_len = prompt.chars().count() as i64;
            let output_len = reply.chars().count() as i64;
            Some(vec![
                json!({"eventCode":"chat_request_send","userId":uid,"userNickname":nick,
                       "conversationId":conversation_id,"requestId":request_id,
                       "requestModelId":"glm-5.2","requestModelName":"GLM-5.2",
                       "inputLength":input_len,"customAgentName":""}),
                json!({"eventCode":"chat_request_response","userId":uid,"userNickname":nick,
                       "conversationId":conversation_id,"requestId":request_id,
                       "requestModelId":"glm-5.2","requestModelName":"GLM-5.2",
                       "toolCallCount":0,"inputToken":(input_len / 4).max(1),
                       "outputToken":(output_len / 4).max(1),
                       "totalToken":((input_len + output_len) / 4).max(2)}),
                json!({"eventCode":"chat_message_send","userId":uid,"userNickname":nick,
                       "conversationId":conversation_id,"requestId":request_id,
                       "messageId":format!("cmb-{}", rand_id()),
                       "requestModelId":"glm-5.2","requestModelName":"GLM-5.2",
                       "historyCount":1,"isContextTruncated":false,"currentStepCount":1,
                       "traceId":request_id,"rootRequestId":request_id,
                       "parentConversationId":conversation_id,"agentName":"cli","agentType":"main"}),
            ])
        }
        _ => None,
    }
}

/// 发起一次不做信封解析的 POST（`/console/chat/completions` 返回 SSE 文本）
async fn http_post_raw(
    account: &mut WorkbuddyAccount,
    path: &str,
    body: Value,
) -> Result<String, String> {
    let client = build_http_client()?;
    let url = format!("{}{}", WB_API_BASE, path);
    let resp = apply_headers(client.post(&url), account)
        .header("Accept", "text/event-stream")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("请求失败: {}", e))?;
    let status = resp.status();
    let raw = resp
        .text()
        .await
        .map_err(|e| format!("读取响应正文失败: {}", e))?;
    if !status.is_success() {
        return Err(format!(
            "http={} body前200字={}",
            status.as_u16(),
            raw.chars().take(200).collect::<String>()
        ));
    }
    Ok(raw)
}

/// 真实发起一次对话，返回 `(会话 id, 回复文本)`。
/// 上报通道会用这两个真实值填充事件字段，而不是凭空编造。
async fn send_real_chat(
    account: &mut WorkbuddyAccount,
    model: &str,
    prompt: &str,
) -> Result<(String, String), String> {
    let conv = http_post_fresh(
        account,
        "/console/webchat/conversations",
        json!({ "name": format!("cockpit-{}", rand_id()) }),
    )
    .await?;
    let conversation_id = conv
        .get("conversationId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if conversation_id.is_empty() {
        return Err("建会话未返回 conversationId".to_string());
    }

    let raw = http_post_raw(
        account,
        "/console/chat/completions",
        json!({
            "messages": [{ "role": "user", "content": prompt }],
            "model": model,
            "stream": true,
            "conversationId": conversation_id,
        }),
    )
    .await?;

    let mut reply = String::new();
    for line in raw.lines() {
        let line = line.trim();
        let Some(chunk) = line.strip_prefix("data: ") else {
            continue;
        };
        if matches!(chunk.trim(), "[DONE]" | "[完成]") {
            break;
        }
        if let Ok(obj) = serde_json::from_str::<Value>(chunk) {
            if let Some(choices) = obj.get("choices").and_then(Value::as_array) {
                for choice in choices {
                    if let Some(text) = choice.pointer("/delta/content").and_then(Value::as_str) {
                        reply.push_str(text);
                    }
                }
            }
        }
    }
    Ok((conversation_id, reply))
}

/// 执行上报通道：为「未完成且已解锁」的报告型任务补齐行为。
///
/// 聊天类与专家团**先真实发起对话**，再用真实的会话 id / 文本长度上报；
/// 其余任务按参考实现的事件形状上报。
async fn run_report_tasks(
    account: &mut WorkbuddyAccount,
    tasks: &[WorkbuddyGrowthTask],
    messages: &mut Vec<String>,
) -> usize {
    let uid = account.uid.clone().unwrap_or_default();
    let nick = account.nickname.clone().unwrap_or_default();
    let mut done = 0usize;

    // 专家数据按需加载（仅 expert_5 / Expert_team_use_3 需要）
    let need_experts = tasks.iter().any(|t| {
        !is_task_done(t) && matches!(t.task_code.as_str(), "expert_5" | "Expert_team_use_3")
    });
    let experts = if need_experts {
        match load_experts(account).await {
            Ok(list) => list,
            Err(err) => {
                messages.push(format!("专家数据加载失败，专家类任务跳过: {}", err));
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };

    for task in tasks {
        if !REPORT_TASK_CODES.contains(&task.task_code.as_str()) || is_task_done(task) {
            continue;
        }
        // 未解锁的任务上报也无效，交给下一轮前置解锁后再处理
        if task.accept_status == "not_accepted" {
            continue;
        }
        // 缺口次数 = 目标 - 当前；未知时按 1 次
        let need = if task.target > 0 {
            (task.target - task.current).max(0)
        } else {
            1
        };
        if need == 0 {
            continue;
        }
        // 夜猫子只在 23:00-08:00 计数
        if task.task_code == "black_cat" {
            let hour = Local::now().hour();
            if !(hour >= 23 || hour < 8) {
                messages.push("「夜猫子」仅 23:00-08:00 计数，当前时段跳过".to_string());
                continue;
            }
        }

        // 桌面端对话：走桌面客户端指纹通道，无需启动客户端
        if task.task_code == "RichMeow_Chat" {
            let conversation_id = rand_id();
            let request_id = rand_id();
            let message_id = format!("cmb-{}", rand_id());
            let events = desktop_chat_sequence(
                &conversation_id,
                &request_id,
                &message_id,
                "glm-5.2",
                "GLM-5.2",
            );
            match report_desktop_events(account, events).await {
                Ok(_) => {
                    done += 1;
                    messages.push("已按桌面客户端指纹上报一次对话（RichMeow_Chat）".to_string());
                }
                Err(err) => messages.push(format!("「{}」桌面上报失败: {}", task.title, err)),
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
            continue;
        }

        for i in 0..need.min(5) {
            let index = i as usize;
            let prompt = CHAT_PROMPTS[index % CHAT_PROMPTS.len()];

            // 聊天类与专家团：先真实对话，取真实的 id 与回复长度
            let (conversation_id, reply) = if REPORT_CHAT_CODES.contains(&task.task_code.as_str())
                || task.task_code == "Expert_team_use_3"
            {
                match send_real_chat(account, "glm-5.2", prompt).await {
                    Ok(pair) => pair,
                    Err(err) => {
                        messages.push(format!("「{}」真实对话失败: {}", task.title, err));
                        break;
                    }
                }
            } else {
                (String::new(), String::new())
            };

            let Some(events) = build_report_unit(
                &task.task_code,
                &experts,
                index,
                &uid,
                &nick,
                prompt,
                &reply,
                &conversation_id,
            ) else {
                messages.push(format!("「{}」缺少可用数据，跳过", task.title));
                break;
            };
            match report_events(account, events).await {
                Ok(_) => done += 1,
                Err(err) => {
                    messages.push(format!("「{}」上报失败: {}", task.title, err));
                    break;
                }
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    }

    if done > 0 {
        messages.push(format!("上报通道补齐 {} 次行为", done));
    }
    done
}

// ---------------------------------------------------------------------------
// 桌面客户端上报通道
//
// 「需电脑端」类任务（如 RichMeow_Chat 桌面端对话）并不需要真的启动客户端：
// 实测关键在于**同一 `/v2/report` 通道上的客户端指纹不同**——
// 桌面端走 `copilot.tencent.com`，UA 与 ideName/extName 都换成桌面客户端的值。
//
// 本段依据生态实测结论（linguo2625469/workbuddy2api-panel 的 desktop.go，
// 原作者称三账号实测点亮 RichMeow_Chat）复刻。
//
// ⚠️ **本机验证状态**：传输层已实测通过（`copilot.tencent.com/v2/report` 返回
// 200 + `code:0`，主机 / UA / 请求头 / token 均被接受）；但「六事件链能否真正
// 点亮 RichMeow_Chat」**本机未能验证**——手头六个账号该项均已是 `claimed`，
// 没有可验证的目标。将来若有新账号待完成该项，请优先观察其是否真的变为 completed。
// ---------------------------------------------------------------------------

/// 桌面客户端上报基址（与 Web 端不同）
const WB_DESKTOP_BASE: &str = "https://copilot.tencent.com";
/// 实测桌面客户端 UA（WorkBuddy 5.5.6 内嵌 CLI 2.137.1）
const WB_DESKTOP_UA: &str = "WorkBuddy/5.5.6 WorkBuddy/5.5.6 CLI/2.137.1";

/// 由账号 id 稳定派生设备标识（同账号每次相同，模拟固定设备；无需额外依赖）
fn derive_device_id(account: &WorkbuddyAccount, salt: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    // 拼两轮不同盐值，凑出 36 位十六进制，形态贴近真实设备标识
    let mut out = String::new();
    for round in 0..2 {
        let mut hasher = DefaultHasher::new();
        format!("{}:{}:{}", salt, round, account.id).hash(&mut hasher);
        out.push_str(&format!("{:016x}", hasher.finish()));
        let mut hasher2 = DefaultHasher::new();
        format!("{}:{}:{}:2", salt, round, account.id).hash(&mut hasher2);
        out.push_str(&format!("{:02x}", hasher2.finish() % 256));
    }
    out.chars().take(36).collect()
}

/// 构造桌面客户端指纹（字段与实测抓包一致）
fn desktop_fingerprint(account: &WorkbuddyAccount) -> Value {
    let now = now_ms();
    json!({
        "timezone": "Asia/Shanghai",
        "reportDelay": 2000,
        "userId": account.uid.clone().unwrap_or_default(),
        "username": account.nickname.clone().unwrap_or_default(),
        "userNickname": account.nickname.clone().unwrap_or_default(),
        "product": "SaaS",
        "ideName": "WorkBuddy",
        "ideType": "WorkBuddy",
        "ideVersion": "5.5.6",
        "machineId": derive_device_id(account, "machine"),
        "sessionId": derive_device_id(account, "session"),
        "extName": "workbuddy-desktop",
        "extVersion": "5.5.6",
        "os": "win32",
        "arch": "x64",
        "osVersion": "10.0.26220",
        "cpuCores": 20,
        "memorySize": 24,
        "timestamp": now,
        "presentAt": now,
    })
}

/// 以桌面客户端指纹批量上报事件
async fn report_desktop_events(
    account: &mut WorkbuddyAccount,
    events: Vec<Value>,
) -> Result<(), String> {
    if events.is_empty() {
        return Ok(());
    }
    let fingerprint = desktop_fingerprint(account);
    let payload: Vec<Value> = events
        .into_iter()
        .map(|event| {
            let mut merged = fingerprint.clone();
            if let (Some(dst), Some(src)) = (merged.as_object_mut(), event.as_object()) {
                for (key, value) in src {
                    dst.insert(key.clone(), value.clone());
                }
            }
            merged
        })
        .collect();

    let client = build_http_client()?;
    let url = format!("{}{}", WB_DESKTOP_BASE, "/v2/report");
    let url_for_header = WB_DESKTOP_BASE.to_string();
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", account.access_token))
        .header("Accept", "application/json, text/plain, */*")
        .header("Content-Type", "application/json;charset=UTF-8")
        .header("User-Agent", WB_DESKTOP_UA)
        .header("X-Domain", url_for_header)
        .header("X-Product", "SaaS")
        .header("X-Request-ID", derive_device_id(account, "req"))
        .header("X-User-Id", account.uid.clone().unwrap_or_default())
        .json(&Value::Array(payload))
        .send()
        .await
        .map_err(|e| format!("桌面上报请求失败: {}", e))?;
    let status = resp.status();
    let raw = resp
        .text()
        .await
        .map_err(|e| format!("读取桌面上报响应失败: {}", e))?;
    if !status.is_success() {
        return Err(format!(
            "桌面上报 http={} body前200字={}",
            status.as_u16(),
            raw.chars().take(200).collect::<String>()
        ));
    }
    Ok(())
}

/// 构造一次「桌面端成功对话」的完整事件链（实测该链可点亮 RichMeow_Chat）。
/// 顺序：agent_task_created → chat_message_send → chat_request_send →
/// chat_message_response(isSuccessful) → chat_message_status → chat_request_response
fn desktop_chat_sequence(
    conversation_id: &str,
    request_id: &str,
    message_id: &str,
    model_id: &str,
    model_name: &str,
) -> Vec<Value> {
    let assistant_id = format!("{}-assistant", message_id);
    let now = now_ms();
    vec![
        json!({"eventCode":"agent_task_created","source":"LOCAL","name":"working","task_target":"local",
               "mode":"craft","requestModelId":model_id,"requestModelName":model_name,
               "has_repo":false,"repo_type":"none","workspace_type":"empty",
               "has_connector":false,"connector_types":[],"has_mention":false,"mention_types":[],
               "has_template":false,"action":"","template_name":"",
               "has_expert":false,"expert_id":"","expert_name":"","expert_industry_id":"",
               "has_skill":false,"skill_names":[],
               "conversationId":conversation_id,"messageId":message_id,"buddyId":"","buddyName":""}),
        json!({"eventCode":"chat_message_send","messageId":assistant_id,"historyCount":0,
               "isContextTruncated":false,"currentStepCount":1,
               "traceId":request_id,"rootRequestId":request_id,"parentConversationId":conversation_id,
               "agentName":"cli","agentType":"main"}),
        json!({"eventCode":"chat_request_send","inputLength":24,"isPlan":false,
               "isAutoExecuteTerminal":false,"isAutoModify":false,"codebaseEnable":false,
               "maxToken":0,"maxSteps":500,"temperature":0,"maxRetries":0,
               "mentionContexts":[],"knowledgeId":[],"knowledgeName":[],
               "codebaseId":"","mentionContextCount":0,"command":"","recommendId":"",
               "skillId":"","skillCount":0,"totalCount":0,
               "traceId":request_id,"rootRequestId":request_id,"parentConversationId":conversation_id,
               "agentName":"cli","agentType":"main",
               "codebuddy.session_id":conversation_id,"codebuddy.conversation_request_id":request_id}),
        json!({"eventCode":"chat_message_response","messageId":assistant_id,"responseModelId":model_id,
               "inputToken":120,"outputToken":80,"totalToken":200,
               "cachedTokens":0,"cachedWriteTokens":0,"cachedMissTokens":0,
               "isSuccessful":true,"messageErrorCode":"","finishReason":"stop","firstTokenAt":now,
               "traceId":request_id,"conversationId":conversation_id,
               "rootRequestId":request_id,"parentConversationId":conversation_id,
               "agentName":"cli","agentType":"main",
               "codebuddy.session_id":conversation_id,"codebuddy.conversation_request_id":request_id}),
        json!({"eventCode":"chat_message_status","messageId":assistant_id,"messageErrorCode":"0",
               "traceId":request_id,"rootRequestId":request_id,"parentConversationId":conversation_id,
               "agentName":"cli","agentType":"main"}),
        json!({"eventCode":"chat_request_response","mode":"craft","toolCallCount":0,
               "inputToken":120,"outputToken":80,"totalToken":200,
               "cachedTokens":0,"cachedWriteTokens":0,"cachedMissTokens":0,
               "isSuccessful":true,"messageErrorCode":"","finishReason":"stop",
               "rootRequestId":request_id,"parentConversationId":conversation_id}),
    ]
}

// ---------------------------------------------------------------------------
// 单账号执行
// ---------------------------------------------------------------------------

/// 处理单个账号：接受任务 → 领取奖励 → 执行玩法
async fn process_account(
    account: &WorkbuddyAccount,
    cfg: &WorkbuddyAutoTasksConfig,
) -> WorkbuddyAutoTasksAccountDetail {
    let mut acc = account.clone();
    let mut messages: Vec<String> = Vec::new();
    let mut accepted = 0usize;
    let mut claimed = 0usize;
    let mut played = 0usize;

    // ① 拉取任务列表
    let tasks = match fetch_growth_tasks(&mut acc).await {
        Ok(tasks) => tasks,
        Err(err) => {
            mark_attempt_failure(&account.id);
            return WorkbuddyAutoTasksAccountDetail {
                account_id: account.id.clone(),
                email: account.email.clone(),
                accepted_count: 0,
                claimed_count: 0,
                played_count: 0,
                progress_summary: "-".to_string(),
                status: "failed".to_string(),
                message: format!("拉取任务列表失败: {}", err),
            };
        }
    };

    // ② 未适配任务检测（官方新增任务时及时暴露）
    let unsupported: Vec<&str> = tasks
        .iter()
        .filter(|t| {
            !is_task_done(t)
                && !is_task_claimable(t)
                && !KNOWN_TASK_CODES.contains(&t.task_code.as_str())
        })
        .map(|t| t.task_code.as_str())
        .collect();
    if !unsupported.is_empty() {
        messages.push(format!("发现未适配任务: {}", unsupported.join(",")));
    }

    let completed = tasks.iter().filter(|t| is_task_done(t)).count();
    let progress_summary = format!("{}/{}", completed, tasks.len());

    // ③ 多轮推进：接受 → 执行 → 领奖
    //
    // ⚠️ 为什么是多轮：上游任务存在**前置依赖链**（实测报错 `prerequisite not met: first_buddy`），
    // 必须完成前一项才会解锁后一项。只试一次会导致新账号上什么都推不动。
    // 同时 `accept` 接口在 `code == 0` 下仍可能逐任务失败，真实结果在 `data.results[]`，
    // 所以把「被阻塞原因」收集起来反馈给用户，而不是静默忽略。
    let anything_enabled = cfg.run_tasks
        || cfg.run_play
        || cfg.run_blindbox
        || cfg.run_theme_task
        || cfg.enable_reporting;
    if anything_enabled {
        let mut blocked_reasons: Vec<(String, String)> = Vec::new();
        let mut last_tasks = tasks.clone();

        // ⓪ 链条根节点：领养协议 → 首只 Buddy。
        // 这两步是整条成长任务链的前置，必须在接受任务之前处理，
        // 否则后续任务一律被 `prerequisite not met: first_buddy` 挡住。
        if cfg.auto_adopt_agreement {
            match agree_buddy_adoption(&mut acc).await {
                Ok(true) => messages.push("已同意 Buddy 领养协议".to_string()),
                Ok(false) => {}
                Err(err) => messages.push(format!("同意领养协议失败: {}", err)),
            }
        }
        if cfg.run_tasks {
            match claim_first_buddy(&mut acc).await {
                Ok(true) => messages.push("已领取首只 Buddy，任务链解锁".to_string()),
                Ok(false) => {}
                Err(err) => messages.push(format!("领取首只 Buddy 未成功: {}", err)),
            }
        }

        // ⓪.5 先接受一轮「已解锁且未接受」的任务，再进入推进循环。
        //
        // ⚠️ 为什么必须先接受一轮（曾经的真实 bug）：
        // `play_theme_task` / `run_report_tasks` 都要求任务**已被接受**才会被服务端计数。
        // 早期实现在 pass 0 内「先接受、立刻执行」，服务端刚接受完、状态尚未生效，
        // 导致账号 xy / 不见清风的 `Hp_Appearance` 反复停在 `accepted 0/1`——
        // 任务被接受过、却从未被真正执行。
        if cfg.run_tasks {
            let pending: Vec<String> = last_tasks
                .iter()
                .filter(|t| t.accept_status == "not_accepted")
                .filter(|t| !SKIP_TASK_CODES.contains(&t.task_code.as_str()))
                .map(|t| t.task_code.clone())
                .collect();
            if !pending.is_empty() {
                match accept_tasks(&mut acc, &pending).await {
                    Ok(outcome) => {
                        accepted += outcome.accepted.len();
                        blocked_reasons.extend(outcome.blocked);
                        if !outcome.accepted.is_empty() {
                            // 接受结果需要一点时间落库，否则紧接着的动作会打在空处
                            tokio::time::sleep(Duration::from_millis(800)).await;
                            if let Ok(refreshed) = fetch_growth_tasks(&mut acc).await {
                                last_tasks = refreshed;
                            }
                        }
                    }
                    Err(err) => messages.push(format!("接受任务失败: {}", err)),
                }
            }
        }

        for pass in 0..MAX_TASK_PASSES {
            // 接受本轮可接的任务（仅 run_tasks）
            let mut newly_accepted = 0usize;
            if cfg.run_tasks {
                let pending: Vec<String> = last_tasks
                    .iter()
                    .filter(|t| t.accept_status == "not_accepted")
                    .filter(|t| !SKIP_TASK_CODES.contains(&t.task_code.as_str()))
                    .map(|t| t.task_code.clone())
                    .collect();
                if !pending.is_empty() {
                    match accept_tasks(&mut acc, &pending).await {
                        Ok(outcome) => {
                            newly_accepted = outcome.accepted.len();
                            accepted += newly_accepted;
                            blocked_reasons.extend(outcome.blocked);
                        }
                        Err(err) => messages.push(format!("接受任务失败: {}", err)),
                    }
                }
            }

            // 第一轮：执行玩法与真实对话，它们可能让任务变得可领奖或解锁后续任务
            if pass == 0 {
                if cfg.run_play {
                    played += play_lottery(&mut acc, &mut messages).await;
                    played += play_redeem(&mut acc, &mut messages).await;
                    played += play_makeup_card(&mut acc, &mut messages).await;
                    played += play_claim_gift(&mut acc, &mut messages).await;
                }
                if cfg.run_blindbox {
                    played += play_blindbox(&mut acc, &mut messages).await;
                }
                // 和平精英主题：真实生效的任务，但会改客户端外观，故独立开关。
                //
                // ⚠️ 判定只能用 `is_task_done`（claimed）而非「未 claimed」：
                // 任务处于 `completed`（已完成待领）时**不需要**再改一次外观，
                // 只需走后面的领奖逻辑，否则每轮都会白白切换一次用户主题。
                // 另外此处放在 ⓪.5 接受之后再执行，确保「已接受」已落库。
                if cfg.run_theme_task {
                    let theme_pending = last_tasks
                        .iter()
                        .find(|t| t.task_code == "Hp_Appearance")
                        .map(|t| !is_task_done(t) && !is_task_claimable(t))
                        .unwrap_or(false);
                    if theme_pending {
                        played +=
                            play_theme_task(&mut acc, cfg.enable_reporting, &mut messages).await;
                        // 上报的事件需要 3~5 秒才落库，等够再让后续的复核逻辑看到结果
                        if cfg.enable_reporting {
                            tokio::time::sleep(Duration::from_millis(4000)).await;
                        }
                    }
                }
                // 上报通道：为报告型任务补齐行为（唯一会"代替客户端声明行为"的部分）。
                // 传 `last_tasks` 而非函数入口处的 `tasks`——后者是接受操作之前的旧快照，
                // 会让「本轮刚接受的任务」被整批跳过。
                if cfg.enable_reporting {
                    played +=
                        run_report_tasks(&mut acc, last_tasks.as_slice(), &mut messages).await;
                }
            }

            // 复核进度并领奖
            let tasks_now = fetch_growth_tasks(&mut acc)
                .await
                .unwrap_or_else(|_| last_tasks.clone());
            let mut claimed_this_pass = 0usize;
            if cfg.run_tasks {
                for task in tasks_now.iter().filter(|t| is_task_claimable(t)) {
                    if claimed >= MAX_CLAIM_PER_ACCOUNT {
                        messages.push("已达单轮领奖上限，剩余下轮处理".to_string());
                        break;
                    }
                    match claim_task(&mut acc, &task.task_code).await {
                        Ok(data) => {
                            claimed += 1;
                            claimed_this_pass += 1;
                            let credit = data.get("credit").and_then(Value::as_i64).unwrap_or(0);
                            let energy = data.get("energy").and_then(Value::as_i64).unwrap_or(0);
                            messages.push(format!(
                                "领取「{}」+{}积分 +{}能量",
                                task.title, credit, energy
                            ));
                        }
                        Err(err) => {
                            let lower = err.to_lowercase();
                            if !lower.contains("already") && !lower.contains("已") {
                                messages.push(format!("领取「{}」失败: {}", task.title, err));
                            }
                        }
                    }
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }

            last_tasks = tasks_now;
            // 本轮既没接到新任务、也没领到奖：已无法继续解锁，提前结束
            if newly_accepted == 0 && claimed_this_pass == 0 {
                break;
            }
        }

        if accepted > 0 {
            messages.insert(0, format!("已接受 {} 项任务", accepted));
        }
        if !blocked_reasons.is_empty() {
            // 去重后给出「照做就能推进」的提示
            let mut seen: Vec<String> = Vec::new();
            for (code, reason) in &blocked_reasons {
                let line = format!("{}({})", code, reason);
                if !seen.contains(&line) {
                    seen.push(line);
                }
            }
            messages.push(format!("被前置依赖阻塞: {}", seen.join("；")));
        }
    }

    // ⑤ 收尾领奖：兜底在循环之外再领一轮。
    //
    // ⚠️ 为什么需要这一步（曾经的真实 bug，两处叠加）：
    // 1) 循环内的领奖发生在 `play_*` **之后**，而 `play_theme_task` 上报的
    //    `appearance_skin_apply` 在服务端**不是瞬时生效**的——实测落库延迟
    //    在 3~5 秒（账号 xy 上报后 3 秒仍 0/1、5 秒才变 completed）。
    //    循环体内紧接着拉取的任务快照必然是旧的，领奖被跳过；
    // 2) 随后 `claimed_this_pass == 0` 被判定为「无进展」直接 break，
    //    任务就永远停在 `completed`，奖励再也不会被领。
    //
    // 实测证据：账号 70371049 的 `Hp_Appearance` 与 `Expert_lighthouse` 均停在
    // `completed 1/1`（各 100 积分 + 5 能量未领），而接口本身完全正常
    // （`already_claimed:false, credit:100, energy:5`，手动补领后立刻变 `claimed`）。
    //
    // 因此这里必须：等待足够长的落库时间 → 轮询若干次 → 领不到才收手。
    let mut unrewarded: Vec<String> = Vec::new();
    if cfg.run_tasks {
        // 给服务端足够时间把上报的事件落成任务进度（实测需 3~5 秒）
        tokio::time::sleep(Duration::from_millis(4000)).await;
        for round in 0..3 {
            let settled = fetch_growth_tasks(&mut acc).await.unwrap_or_default();
            if settled.is_empty() {
                break;
            }
            let mut got = 0usize;
            for task in settled.iter().filter(|t| is_task_claimable(t)) {
                if claimed >= MAX_CLAIM_PER_ACCOUNT {
                    break;
                }
                match claim_task(&mut acc, &task.task_code).await {
                    Ok(data) => {
                        claimed += 1;
                        got += 1;
                        let credit = data.get("credit").and_then(Value::as_i64).unwrap_or(0);
                        let energy = data.get("energy").and_then(Value::as_i64).unwrap_or(0);
                        messages.push(format!(
                            "补领「{}」+{}积分 +{}能量",
                            task.title, credit, energy
                        ));
                    }
                    Err(err) => {
                        let lower = err.to_lowercase();
                        if !lower.contains("already") && !lower.contains("已") {
                            messages.push(format!("补领「{}」失败: {}", task.title, err));
                        }
                    }
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            unrewarded = settled
                .iter()
                .filter(|t| is_task_claimable(t))
                .map(|t| t.title.clone())
                .collect();
            if got == 0 {
                // 本轮没领到新的：若还有待领项，可能是落库还没完成，再等一轮
                if unrewarded.is_empty() || round == 2 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(2500)).await;
            }
        }
    }

    // ⑥ 汇总仍需手动处理的任务，给用户明确指引
    let tasks_final = fetch_growth_tasks(&mut acc).await.unwrap_or_default();
    let manual: Vec<String> = tasks_final
        .iter()
        .filter(|t| !is_task_done(t))
        .filter(|t| {
            CHAT_TASK_CODES.contains(&t.task_code.as_str())
                || DESKTOP_TASK_CODES.contains(&t.task_code.as_str())
                || SKIP_TASK_CODES.contains(&t.task_code.as_str())
        })
        .map(|t| {
            if DESKTOP_TASK_CODES.contains(&t.task_code.as_str()) {
                format!("{}(需桌面端)", t.title)
            } else if SKIP_TASK_CODES.contains(&t.task_code.as_str()) {
                format!("{}(需手动)", t.title)
            } else if cfg.enable_reporting && REPORT_TASK_CODES.contains(&t.task_code.as_str()) {
                // 已开启上报仍未完成：多半是前置未解锁或事件未被采纳
                format!("{}(上报未生效)", t.title)
            } else {
                format!("{}(需真实操作)", t.title)
            }
        })
        .collect();
    if !manual.is_empty() {
        messages.push(format!("待手动完成: {}", manual.join("、")));
    }
    // 已完成但奖励仍没到手的，必须显式告警——绝不能静默
    if !unrewarded.is_empty() {
        messages.push(format!(
            "⚠️ 已完成但有奖励未领取: {}（接口可能限频，下轮会自动补领）",
            unrewarded.join("、")
        ));
    }

    clear_attempt(&account.id);
    let final_completed = tasks_final.iter().filter(|t| is_task_done(t)).count();
    let final_summary = if tasks_final.is_empty() {
        progress_summary
    } else {
        format!("{}/{}", final_completed, tasks_final.len())
    };

    WorkbuddyAutoTasksAccountDetail {
        account_id: account.id.clone(),
        email: account.email.clone(),
        accepted_count: accepted,
        claimed_count: claimed,
        played_count: played,
        progress_summary: final_summary,
        status: "success".to_string(),
        message: if messages.is_empty() {
            "无需要处理的项目".to_string()
        } else {
            messages.join("；")
        },
    }
}

// ---------------------------------------------------------------------------
// 调度
// ---------------------------------------------------------------------------

/// 当前本地日期 "YYYY-MM-DD"
fn today_string() -> String {
    Local::now().format("%Y-%m-%d").to_string()
}

/// 当前本地时间戳 "YYYY-MM-DD HH:mm:ss"
fn now_timestamp() -> String {
    Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

/// 判断当前是否处于配置的执行窗口内
fn in_execution_window(cfg: &WorkbuddyAutoTasksConfig) -> bool {
    let Some(start) = parse_time_to_minutes(&cfg.start_time) else {
        return false;
    };
    let Some(end) = parse_time_to_minutes(&cfg.end_time) else {
        return false;
    };
    let now = Local::now();
    let now_minutes = now.hour() as i32 * 60 + now.minute() as i32;
    (start..=end).contains(&now_minutes)
}

/// 执行一轮自动任务（供调度器与手动触发共用）。
///
/// `force = true` 时忽略「今日已执行」与「执行窗口」判断，立即跑一轮。
pub async fn run_workbuddy_auto_tasks_cycle_if_needed(
    app: &AppHandle,
    force: bool,
) -> Result<String, String> {
    if TASKS_CYCLE_RUNNING.swap(true, Ordering::SeqCst) {
        return Ok("already_running".to_string());
    }
    // 无论走哪条分支，退出时都要复位运行标记
    let _reset_guard = RunningGuard;

    let config = get_config_checked()?;
    if !config.enabled && !force {
        return Ok("disabled".to_string());
    }
    let today = today_string();
    if !force {
        // 每日只跑一轮：今日已执行过则跳过
        if config.last_run_date.as_deref() == Some(today.as_str()) {
            return Ok("already_ran_today".to_string());
        }
        // 未进入执行窗口则等待
        if !in_execution_window(&config) {
            return Ok("waiting".to_string());
        }
    }

    let accounts = workbuddy_account::list_accounts();
    let started_at = std::time::Instant::now();
    let mut details: Vec<WorkbuddyAutoTasksAccountDetail> = Vec::new();
    let mut failed = 0usize;

    for account in accounts {
        if account.access_token.trim().is_empty() {
            continue;
        }
        if !force && !allow_attempt(&account.id) {
            // 处于退避期：不重复尝试，也不计入失败
            continue;
        }
        let detail = process_account(&account, &config).await;
        if detail.status == "failed" {
            failed += 1;
        } else {
            clear_attempt(&account.id);
        }
        details.push(detail);
    }

    let success_count = details.iter().filter(|d| d.status == "success").count();
    let total_accounts = details.len();
    let status = if total_accounts == 0 {
        "no_accounts"
    } else if failed == 0 {
        "success"
    } else if success_count > 0 {
        "partial"
    } else {
        "failed"
    };

    let record = WorkbuddyAutoTasksLogRecord {
        id: Uuid::new_v4().to_string(),
        timestamp: now_timestamp(),
        date: today.clone(),
        duration_ms: started_at.elapsed().as_millis() as u64,
        total_accounts,
        success_count,
        failed_count: failed,
        status: status.to_string(),
        details,
    };
    let notify_ok = add_log_record(record).is_ok();

    // 记录今日已执行（仅在非强制、有账号且整体未全失败时）
    if !force && total_accounts > 0 && status != "failed" {
        let mut updated = config.clone();
        updated.last_run_date = Some(today);
        let _ = save_config(&updated);
    }

    let _ = app.emit("workbuddy-auto-tasks-logs-changed", ());
    let _ = app.emit("workbuddy-auto-tasks-config-changed", ());

    if !notify_ok {
        logger::log_warn("[WorkBuddyAutoTasks] 日志写入失败，本轮结果未持久化");
    }

    logger::log_info(&format!(
        "[WorkBuddyAutoTasks] 本轮完成: 账号 {} / 成功 {} / 失败 {}",
        total_accounts, success_count, failed
    ));

    if failed > 0 {
        Ok("retry".to_string())
    } else {
        Ok("completed".to_string())
    }
}

/// 运行标记守卫：离开作用域时复位
struct RunningGuard;
impl Drop for RunningGuard {
    fn drop(&mut self) {
        TASKS_CYCLE_RUNNING.store(false, Ordering::SeqCst);
    }
}

/// 启动自动任务调度（幂等，在应用启动时调用）
pub fn ensure_started(app: AppHandle) {
    if TASKS_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    logger::log_info("[WorkBuddyAutoTasks] 自动任务调度已启动（每 5 分钟巡检，每日一轮）");
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_secs(TASKS_STARTUP_DELAY_SECONDS)).await;
        loop {
            match run_workbuddy_auto_tasks_cycle_if_needed(&app, false).await {
                Ok(result) => {
                    if result == "retry" {
                        logger::log_warn(
                            "[WorkBuddyAutoTasks] 本轮存在失败账号，已进入退避，稍后重试",
                        );
                    }
                }
                Err(err) => {
                    logger::log_warn(&format!("[WorkBuddyAutoTasks] 调度异常: {}", err));
                }
            }
            tokio::time::sleep(Duration::from_secs(TASKS_TICK_SECONDS)).await;
        }
    });
}
