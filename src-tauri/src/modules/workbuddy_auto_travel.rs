//! WorkBuddy 自动旅行：为账号自动派发「派猫猫旅行」并领取到达奖励。
//! 受「自动旅行」开关控制（默认开启）。决策逻辑与 wb-switch-app 对齐：
//! idle 且未达今日上限 → 派发；arrived → 领奖；traveling / idle+今日上限 → 无动作。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use crate::models::workbuddy::WorkbuddyAccount;
use crate::modules::{codebuddy_cn_oauth, config, logger, workbuddy_account};

/// 巡检间隔：每 30 分钟一轮（旅行全程约 1 小时，30 分钟能及时派发与领奖）
const TRAVEL_TICK_SECONDS: u64 = 30 * 60;
/// 启动后延迟 2 分钟执行首轮，尽快补上当日的派发
const TRAVEL_STARTUP_DELAY_SECONDS: u64 = 2 * 60;
/// 失败退避时长（1 小时），避免对异常账号反复打接口
const TRAVEL_FAILURE_BACKOFF_SECONDS: i64 = 60 * 60;

/// 调度是否已启动（幂等标记）
static TRAVEL_STARTED: AtomicBool = AtomicBool::new(false);
/// 单轮执行互斥标记（防止上一轮未结束时重入）
static TRAVEL_CYCLE_RUNNING: AtomicBool = AtomicBool::new(false);
/// 失败退避表：账号 ID → 下次允许尝试时间（秒级时间戳）
static NEXT_ATTEMPT_AT: LazyLock<Mutex<HashMap<String, i64>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 启动自动旅行调度（幂等，在应用启动时调用）
pub fn ensure_started() {
    if TRAVEL_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    logger::log_info("[WorkBuddyAutoTravel] 自动旅行调度已启动（每 30 分钟巡检一次）");
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_secs(TRAVEL_STARTUP_DELAY_SECONDS)).await;
        loop {
            run_travel_cycle().await;
            tokio::time::sleep(Duration::from_secs(TRAVEL_TICK_SECONDS)).await;
        }
    });
}

/// 执行一轮自动旅行：逐账号查状态并派发/领奖（串行处理，避免并发限流）
async fn run_travel_cycle() {
    if TRAVEL_CYCLE_RUNNING
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }
    let result = run_travel_cycle_inner().await;
    TRAVEL_CYCLE_RUNNING.store(false, Ordering::SeqCst);
    if let Err(err) = result {
        logger::log_warn(&format!("[WorkBuddyAutoTravel] 本轮执行中断: {}", err));
    }
}

async fn run_travel_cycle_inner() -> Result<(), String> {
    let user_config = config::get_user_config();
    if !user_config.workbuddy_auto_travel_enabled {
        return Ok(());
    }
    let accounts = workbuddy_account::list_accounts();
    let mut dispatched = 0usize;
    let mut claimed = 0usize;
    let mut in_progress = 0usize;
    let mut finished = 0usize;

    for account in accounts {
        if account.access_token.trim().is_empty() || !allow_attempt(&account.id) {
            continue;
        }
        let mut acc = account.clone();
        // 查旅行状态；token 失效自动刷新重试一次
        let status = match get_travel_status_fresh(&mut acc).await {
            Ok(status) => status,
            Err(err) => {
                mark_attempt_failure(&acc.id);
                logger::log_warn(&format!(
                    "[WorkBuddyAutoTravel] 查询旅行状态失败，1 小时后重试: account_id={}, error={}",
                    acc.id, err
                ));
                continue;
            }
        };

        match status.state.as_str() {
            "idle" if !status.daily_limit_reached => {
                if depart_for_account(&acc).await {
                    dispatched += 1;
                    clear_attempt(&acc.id);
                }
            }
            "arrived" => match claim_for_account(&acc, status.record_id).await {
                Ok(Some(reward)) => {
                    claimed += 1;
                    clear_attempt(&acc.id);
                    logger::log_info(&format!(
                        "[WorkBuddyAutoTravel] 旅行奖励已领取: account_id={}, reward={}",
                        acc.id, reward
                    ));
                }
                Ok(None) => {
                    claimed += 1;
                    clear_attempt(&acc.id);
                    logger::log_info(&format!(
                        "[WorkBuddyAutoTravel] 旅行奖励已领取: account_id={}（积分未知）",
                        acc.id
                    ));
                }
                Err(err) => {
                    if codebuddy_cn_oauth::is_token_error(&err) {
                        // 领取时 token 刚好失效：下轮会先刷新重试，不做额外退避
                        logger::log_warn(&format!(
                            "[WorkBuddyAutoTravel] 领奖遇 token 失效，下轮重试: account_id={}",
                            acc.id
                        ));
                    } else {
                        mark_attempt_failure(&acc.id);
                        logger::log_warn(&format!(
                            "[WorkBuddyAutoTravel] 领取旅行奖励失败，1 小时后重试: account_id={}, error={}",
                            acc.id, err
                        ));
                    }
                }
            },
            "traveling" => {
                in_progress += 1;
                clear_attempt(&acc.id);
            }
            _ => {
                // idle + 今日上限：今天不能再旅行
                finished += 1;
                clear_attempt(&acc.id);
            }
        }
    }

    if dispatched > 0 || claimed > 0 {
        logger::log_info(&format!(
            "[WorkBuddyAutoTravel] 本轮完成: 派发 {} 个 / 领奖 {} 个 / 旅行中 {} / 今日已结束 {}",
            dispatched, claimed, in_progress, finished
        ));
    }
    Ok(())
}

/// 查询旅行状态，token 失效时刷新一次账号后重试（acc 会被更新为最新账号）
async fn get_travel_status_fresh(
    acc: &mut WorkbuddyAccount,
) -> Result<codebuddy_cn_oauth::TravelStatusResponse, String> {
    let status = codebuddy_cn_oauth::get_travel_status(
        &acc.access_token,
        acc.uid.as_deref(),
        acc.enterprise_id.as_deref(),
        acc.domain.as_deref(),
    )
    .await;
    match status {
        Ok(status) => Ok(status),
        Err(err) => {
            if !codebuddy_cn_oauth::is_token_error(&err) {
                return Err(err);
            }
            let fresh = workbuddy_account::refresh_account_token(&acc.id).await?;
            *acc = fresh;
            codebuddy_cn_oauth::get_travel_status(
                &acc.access_token,
                acc.uid.as_deref(),
                acc.enterprise_id.as_deref(),
                acc.domain.as_deref(),
            )
            .await
        }
    }
}

/// 为账号派发旅行：取地点列表并逐个尝试，错误分类处理（对齐 wb-switch-app）
async fn depart_for_account(acc: &WorkbuddyAccount) -> bool {
    let locations = match codebuddy_cn_oauth::get_travel_config(
        &acc.access_token,
        acc.uid.as_deref(),
        acc.enterprise_id.as_deref(),
        acc.domain.as_deref(),
    )
    .await
    {
        Ok(locations) => locations,
        Err(err) => {
            mark_attempt_failure(&acc.id);
            logger::log_warn(&format!(
                "[WorkBuddyAutoTravel] 读取旅行配置失败: account_id={}, error={}",
                acc.id, err
            ));
            return false;
        }
    };
    if locations.is_empty() {
        // 无可用地点（活动未开或无 Buddy），退避后重查
        mark_attempt_failure(&acc.id);
        logger::log_warn(&format!(
            "[WorkBuddyAutoTravel] 无可用旅行地点，稍后重试: account_id={}",
            acc.id
        ));
        return false;
    }

    for location in &locations {
        match codebuddy_cn_oauth::depart_travel(
            &acc.access_token,
            acc.uid.as_deref(),
            acc.enterprise_id.as_deref(),
            acc.domain.as_deref(),
            &location.id,
        )
        .await
        {
            Ok(_) => {
                clear_attempt(&acc.id);
                logger::log_info(&format!(
                    "[WorkBuddyAutoTravel] 旅行已派发: account_id={}, location={}",
                    acc.id, location.name
                ));
                return true;
            }
            Err(err) => {
                let lower = err.to_lowercase();
                if lower.contains("already traveling") {
                    // 已在旅行中，等同成功
                    clear_attempt(&acc.id);
                    return true;
                }
                if lower.contains("daily limit") || lower.contains("daily_limit") {
                    // 今日已达上限
                    clear_attempt(&acc.id);
                    logger::log_info(&format!(
                        "[WorkBuddyAutoTravel] 今日旅行已达上限: account_id={}",
                        acc.id
                    ));
                    return true;
                }
                if lower.contains("location not available") {
                    continue; // 换下一个地点
                }
                // no active buddy / 其他错误：退避重试
                mark_attempt_failure(&acc.id);
                logger::log_warn(&format!(
                    "[WorkBuddyAutoTravel] 派发旅行失败: account_id={}, location={}, error={}",
                    acc.id, location.name, err
                ));
                return false;
            }
        }
    }
    // 所有地点都不可用
    mark_attempt_failure(&acc.id);
    logger::log_warn(&format!(
        "[WorkBuddyAutoTravel] 所有旅行地点均不可用: account_id={}",
        acc.id
    ));
    false
}

/// 领取旅行奖励，返回奖励积分（可能为 None）
async fn claim_for_account(
    acc: &WorkbuddyAccount,
    record_id: i64,
) -> Result<Option<serde_json::Value>, String> {
    codebuddy_cn_oauth::claim_travel(
        &acc.access_token,
        acc.uid.as_deref(),
        acc.enterprise_id.as_deref(),
        acc.domain.as_deref(),
        record_id,
    )
    .await
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
            chrono::Utc::now().timestamp() + TRAVEL_FAILURE_BACKOFF_SECONDS,
        );
    }
}
