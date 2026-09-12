//! WorkBuddy 账号自动保活：按配置的间隔天数强制刷新账号 Token，
//! 防止账号长期未使用导致远端会话失效。受「自动保活」开关控制。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use crate::modules::{config, logger, workbuddy_account};

/// 巡检间隔：每小时检查一次是否有账号到达保活时间
const KEEPALIVE_TICK_SECONDS: u64 = 60 * 60;
/// 启动后延迟 10 分钟执行首轮保活，避开启动高峰
const KEEPALIVE_STARTUP_DELAY_SECONDS: u64 = 10 * 60;
/// 单轮最多刷新的账号数，避免一次性刷新过多触发限流
const KEEPALIVE_MAX_PER_CYCLE: usize = 10;
/// 刷新失败后的退避时长（6 小时）
const KEEPALIVE_FAILURE_BACKOFF_SECONDS: i64 = 6 * 60 * 60;

/// 调度是否已启动（幂等标记）
static KEEPALIVE_STARTED: AtomicBool = AtomicBool::new(false);
/// 刷新失败退避表：账号 ID → 下次允许尝试时间（秒级时间戳）
static NEXT_ATTEMPT_AT: LazyLock<Mutex<HashMap<String, i64>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 启动自动保活调度（幂等，在应用启动时调用）
pub fn ensure_started() {
    if KEEPALIVE_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    logger::log_info("[WorkBuddyKeepalive] 自动保活调度已启动（每小时巡检一次）");
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_secs(KEEPALIVE_STARTUP_DELAY_SECONDS)).await;
        loop {
            run_keepalive_cycle().await;
            tokio::time::sleep(Duration::from_secs(KEEPALIVE_TICK_SECONDS)).await;
        }
    });
}

/// 执行一轮保活：对距上次保活超过配置天数的账号强制刷新 Token
async fn run_keepalive_cycle() {
    let user_config = config::get_user_config();
    if !user_config.workbuddy_auto_keepalive_enabled {
        return;
    }
    // 间隔天数至少 1 天（配置层已限制 1~365，这里兜底）
    let interval_ms = user_config.workbuddy_auto_keepalive_days.max(1) * 24 * 60 * 60 * 1000;
    let now_ms = chrono::Utc::now().timestamp_millis();
    let accounts = workbuddy_account::list_accounts();
    let mut refreshed = 0usize;

    for account in accounts {
        if refreshed >= KEEPALIVE_MAX_PER_CYCLE {
            logger::log_warn("[WorkBuddyKeepalive] 本轮达到单轮刷新上限，剩余账号下轮处理");
            break;
        }
        // 从未保活过的账号视为到期，先补一次基准保活
        let due = match account.last_keepalive_at {
            Some(last) => now_ms - last >= interval_ms,
            None => true,
        };
        if !due || !allow_attempt(&account.id) {
            continue;
        }
        refreshed += 1;
        match workbuddy_account::refresh_account_token(&account.id).await {
            Ok(updated) => {
                clear_attempt(&account.id);
                let _ = workbuddy_account::update_last_keepalive_at(&account.id, now_ms);
                logger::log_info(&format!(
                    "[WorkBuddyKeepalive] Token 保活成功: account_id={}, email={}",
                    updated.id, updated.email
                ));
            }
            Err(err) => {
                mark_attempt_failure(&account.id);
                logger::log_warn(&format!(
                    "[WorkBuddyKeepalive] Token 保活失败，{}小时后重试: account_id={}, error={}",
                    KEEPALIVE_FAILURE_BACKOFF_SECONDS / 3600,
                    account.id,
                    err
                ));
            }
        }
    }
}

/// 该账号当前是否允许尝试（失败退避期内跳过）
fn allow_attempt(account_id: &str) -> bool {
    let now = chrono::Utc::now().timestamp();
    let Ok(state) = NEXT_ATTEMPT_AT.lock() else {
        return true;
    };
    state
        .get(account_id)
        .map(|next| *next <= now)
        .unwrap_or(true)
}

/// 保活成功后清除退避记录
fn clear_attempt(account_id: &str) {
    if let Ok(mut state) = NEXT_ATTEMPT_AT.lock() {
        state.remove(account_id);
    }
}

/// 保活失败后记录退避时间
fn mark_attempt_failure(account_id: &str) {
    if let Ok(mut state) = NEXT_ATTEMPT_AT.lock() {
        state.insert(
            account_id.to_string(),
            chrono::Utc::now().timestamp() + KEEPALIVE_FAILURE_BACKOFF_SECONDS,
        );
    }
}
