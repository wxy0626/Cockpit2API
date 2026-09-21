use std::time::Instant;
use tauri::AppHandle;

use crate::models::qoder::{QoderOAuthStartResponse, QoderWorkAccount};
use crate::modules::{
    logger, provider_current_state, qoderwork_account, qoderwork_gateway, qoderwork_oauth,
    qoderwork_oauth_window,
};

#[tauri::command]
pub fn list_qoderwork_accounts() -> Result<Vec<QoderWorkAccount>, String> {
    qoderwork_account::list_accounts_checked()
}

#[tauri::command]
pub fn delete_qoderwork_account(account_id: String) -> Result<(), String> {
    qoderwork_account::remove_account(&account_id)
}

#[tauri::command]
pub fn delete_qoderwork_accounts(account_ids: Vec<String>) -> Result<(), String> {
    qoderwork_account::remove_accounts(&account_ids)
}

#[tauri::command]
pub fn import_qoderwork_from_json(json_content: String) -> Result<Vec<QoderWorkAccount>, String> {
    qoderwork_account::import_from_json(&json_content)
}

#[tauri::command]
pub fn export_qoderwork_accounts(account_ids: Vec<String>) -> Result<String, String> {
    qoderwork_account::export_accounts(&account_ids)
}

#[tauri::command]
pub async fn qoderwork_oauth_login_start() -> Result<QoderOAuthStartResponse, String> {
    let started_at = Instant::now();
    logger::log_info("[QoderWork OAuth] start 命令触发");
    let result = qoderwork_oauth::start_login().await;
    match &result {
        Ok(response) => logger::log_info(&format!(
            "[QoderWork OAuth] start 命令完成: login_id={}, elapsed={}ms",
            response.login_id,
            started_at.elapsed().as_millis()
        )),
        Err(err) => logger::log_warn(&format!(
            "[QoderWork OAuth] start 命令失败: elapsed={}ms, error={}",
            started_at.elapsed().as_millis(),
            err
        )),
    }
    result
}

#[tauri::command]
pub async fn qoderwork_oauth_login_complete(login_id: String) -> Result<QoderWorkAccount, String> {
    let started_at = Instant::now();
    logger::log_info(&format!(
        "[QoderWork OAuth] complete 命令触发: login_id={}",
        login_id
    ));
    let result = qoderwork_oauth::complete_login(&login_id).await;
    match &result {
        Ok(account) => logger::log_info(&format!(
            "[QoderWork OAuth] complete 命令完成: login_id={}, account_id={}, elapsed={}ms",
            login_id,
            account.id,
            started_at.elapsed().as_millis()
        )),
        Err(err) => logger::log_warn(&format!(
            "[QoderWork OAuth] complete 命令失败: login_id={}, elapsed={}ms, error={}",
            login_id,
            started_at.elapsed().as_millis(),
            err
        )),
    }
    result
}

#[tauri::command]
pub fn qoderwork_oauth_login_cancel(login_id: Option<String>) -> Result<(), String> {
    qoderwork_oauth::cancel_login(login_id.as_deref())
}

#[tauri::command]
pub async fn qoderwork_oauth_open_window(app: AppHandle, auth_url: String) -> Result<(), String> {
    qoderwork_oauth_window::open_oauth_window(&app, &auth_url).await
}

#[tauri::command]
pub fn qoderwork_oauth_close_window(app: AppHandle) -> Result<(), String> {
    qoderwork_oauth_window::close_oauth_window(&app)
}

#[tauri::command]
pub async fn refresh_qoderwork_token(account_id: String) -> Result<QoderWorkAccount, String> {
    let account = qoderwork_oauth::refresh_account(&account_id).await?;
    provider_current_state::set_current_account_id("qoderwork_intl", Some(account_id.as_str()))?;
    qoderwork_gateway::clear_cached_token();
    Ok(account)
}

#[tauri::command]
pub async fn refresh_all_qoderwork_tokens() -> Result<i32, String> {
    let refreshed = qoderwork_oauth::refresh_all_accounts().await?;
    crate::modules::qoderwork_gateway::clear_cached_token();
    Ok(refreshed)
}

#[tauri::command]
pub fn update_qoderwork_account_tags(
    account_id: String,
    tags: Vec<String>,
) -> Result<QoderWorkAccount, String> {
    qoderwork_account::update_account_tags(&account_id, tags)
}

#[tauri::command]
pub fn get_qoderwork_accounts_index_path() -> Result<String, String> {
    qoderwork_account::accounts_index_path_string()
}

/// 国际版切换当前账号；QoderWork 是客户端账号池，切换后清空网关 token 缓存。
#[tauri::command]
pub fn switch_qoderwork_intl_account(account_id: String) -> Result<(), String> {
    if qoderwork_account::load_account(&account_id).is_none() {
        return Err(format!("QoderWork 国际版账号不存在: {}", account_id));
    }
    provider_current_state::set_current_account_id("qoderwork_intl", Some(account_id.as_str()))?;
    qoderwork_gateway::clear_cached_token();
    Ok(())
}
