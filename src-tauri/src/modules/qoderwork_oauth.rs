use crate::models::qoder::{QoderOAuthStartResponse, QoderWorkAccount};
use crate::modules::provider_current_state;
use crate::modules::qoder_oauth;
use crate::modules::qoderwork_account;

/// 发起 QoderWork 登录；复用现有 Qoder device flow，仅替换客户端参数。
pub async fn start_login() -> Result<QoderOAuthStartResponse, String> {
    qoder_oauth::start_login_for_product(Some("qoderwork")).await
}

/// 等待 QoderWork 授权完成，并写入独立的 qoderwork 账号库。
pub async fn complete_login(login_id: &str) -> Result<QoderWorkAccount, String> {
    let account = qoder_oauth::complete_login(login_id).await?;
    let account = qoderwork_account::upsert_account_from_snapshot(
        account
            .auth_user_info_raw
            .clone()
            .ok_or_else(|| "QoderWork 登录结果缺少 user info".to_string())?,
        account.auth_user_plan_raw.clone(),
        account.auth_credit_usage_raw.clone(),
    )?;
    provider_current_state::set_current_account_id("qoderwork_intl", Some(account.id.as_str()))?;
    crate::modules::qoderwork_gateway::clear_cached_token();
    Ok(account)
}

/// 取消当前 QoderWork 登录会话；现有 OAuth 是单槽位，直接复用取消逻辑。
pub fn cancel_login(login_id: Option<&str>) -> Result<(), String> {
    qoder_oauth::cancel_login(login_id)
}

/// 按 qoderwork 独立账号库刷新账号配额。
pub async fn refresh_account(account_id: &str) -> Result<QoderWorkAccount, String> {
    let target = qoderwork_account::load_account(account_id)
        .ok_or_else(|| format!("QoderWork 账号不存在: {}", account_id))?;
    let account = qoder_oauth::refresh_account_from_openapi(&target.id).await?;
    let refreshed = qoderwork_account::upsert_account_from_snapshot(
        account
            .auth_user_info_raw
            .clone()
            .ok_or_else(|| "QoderWork 刷新结果缺少 user info".to_string())?,
        account.auth_user_plan_raw,
        account.auth_credit_usage_raw,
    )?;
    if refreshed.id != target.id {
        return Err(format!(
            "QoderWork 刷新结果账号不一致: target_id={}, actual_id={}",
            target.id, refreshed.id
        ));
    }
    crate::modules::qoderwork_gateway::clear_cached_token();
    Ok(refreshed)
}

/// 批量刷新 QoderWork 账号；单个失败不阻断其余账号。
pub async fn refresh_all_accounts() -> Result<i32, String> {
    let accounts = qoderwork_account::list_accounts();
    if accounts.is_empty() {
        return Ok(0);
    }
    let mut success_count = 0;
    for account in accounts {
        if refresh_account(&account.id).await.is_ok() {
            success_count += 1;
        } else {
            crate::modules::logger::log_warn(&format!(
                "[QoderWork Refresh] 刷新失败: account_id={}, email={}",
                account.id, account.email
            ));
        }
    }
    Ok(success_count)
}
