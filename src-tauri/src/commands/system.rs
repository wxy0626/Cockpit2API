// System commands 统一入口。
// 按配置模型、备份/WebDAV、网络/通用设置和应用命令职责拆分，
// 通过 include! 保持 Tauri command 注册路径与调用行为不变。
include!("system_config_types.rs");
include!("system_backup_webdav.rs");
include!("system_network_general.rs");
include!("system_app_commands.rs");

/// 通过 Rust 请求本机 WorkBuddy 网关，绕过 WebView 的跨域限制。
#[tauri::command]
pub async fn workbuddy_gateway_chat(
    model: String,
    message: String,
    api_key: String,
) -> Result<String, String> {
    let client = reqwest::Client::new();
    let response = client
        .post("http://127.0.0.1:7863/v1/chat/completions")
        .bearer_auth(api_key.trim())
        .json(&serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": message}],
            "stream": false
        }))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = response.status();
    let value: serde_json::Value = response.json().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(value.to_string());
    }
    Ok(value["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("调用成功")
        .to_string())
}

#[tauri::command]
pub async fn load_ui_preferences() -> Result<modules::ui_preferences::UiPreferences, String> {
    tauri::async_runtime::spawn_blocking(modules::ui_preferences::load_ui_preferences)
        .await
        .map_err(|error| format!("读取界面偏好任务失败: {}", error))?
}

#[tauri::command]
pub async fn save_ui_preferences(
    values: std::collections::BTreeMap<String, String>,
) -> Result<modules::ui_preferences::UiPreferences, String> {
    tauri::async_runtime::spawn_blocking(move || {
        modules::ui_preferences::save_ui_preferences(values)
    })
    .await
    .map_err(|error| format!("保存界面偏好任务失败: {}", error))?
}

#[cfg(test)]
mod tests {
    include!("system_tests.rs");
}
