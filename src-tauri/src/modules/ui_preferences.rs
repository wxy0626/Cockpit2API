//! 跨版本保留的侧边栏布局。
//! 存在数据目录，不依赖 WebView localStorage。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::modules::account;
use crate::modules::atomic_write::write_string_atomic;

const UI_PREFERENCES_FILE: &str = "ui_preferences.json";
/// 平台布局在偏好文件中的键名（load 兜底迁移与修订号保护共用）
const PLATFORM_LAYOUT_KEY: &str = "agtools.platform_layout.v1";

static PREFERENCES_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UiPreferences {
    #[serde(default)]
    pub values: BTreeMap<String, String>,
}

fn preferences_path() -> Result<PathBuf, String> {
    Ok(account::get_data_dir()?.join(UI_PREFERENCES_FILE))
}

fn read_preferences_from_path(path: &PathBuf) -> Result<UiPreferences, String> {
    if !path.exists() {
        return Ok(UiPreferences::default());
    }
    let raw =
        std::fs::read_to_string(path).map_err(|error| format!("读取界面偏好失败: {error}"))?;
    if raw.trim().is_empty() {
        return Err("界面偏好文件为空，已保留原文件".into());
    }
    serde_json::from_str(&raw).map_err(|error| format!("界面偏好格式无效: {error}"))
}

fn write_preferences_to_path(path: &PathBuf, preferences: &UiPreferences) -> Result<(), String> {
    let raw = serde_json::to_string_pretty(preferences)
        .map_err(|error| format!("序列化界面偏好失败: {error}"))?;
    write_string_atomic(path, &raw)
}

pub fn load_ui_preferences() -> Result<UiPreferences, String> {
    let _guard = PREFERENCES_LOCK
        .try_lock()
        .map_err(|_| "界面偏好正在读写，请重试".to_string())?;
    let mut preferences = read_preferences_from_path(&preferences_path()?);
    if let Ok(prefs) = preferences.as_mut() {
        // 兼容迁移：老版本把平台布局存在 config.json 的 platform_layout_config，
        // 重构后布局改存 ui_preferences.json。老用户升级后偏好文件里还没有布局键，
        // 这里把旧数据原样透传给前端水合，避免用户定制布局被重置为默认列表。
        // 只读不落盘：前端水合确认后会按新修订号正式写入偏好文件。
        if !prefs.values.contains_key(PLATFORM_LAYOUT_KEY) {
            if let Some(legacy) = legacy_platform_layout_value() {
                prefs
                    .values
                    .insert(PLATFORM_LAYOUT_KEY.to_string(), legacy);
            }
        }
    }
    preferences
}

/// 从 config.json 读取旧版平台布局数据（重构前存储位置，现为兼容遗留键）。
/// 不存在、解析失败或内容为空时返回 None，均不视为错误。
fn legacy_platform_layout_value() -> Option<String> {
    let path = crate::modules::config::get_user_config_path().ok()?;
    let raw = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let legacy = value.get("platform_layout_config")?;
    match legacy {
        serde_json::Value::Null => None,
        serde_json::Value::Object(map) if map.is_empty() => None,
        _ => Some(legacy.to_string()),
    }
}

fn apply_values(preferences: &mut UiPreferences, values: BTreeMap<String, String>) -> bool {
    let mut changed = false;
    for (key, value) in values {
        if preferences.values.get(&key) != Some(&value) {
            preferences.values.insert(key, value);
            changed = true;
        }
    }
    changed
}

fn check_layout_revision(
    preferences: &UiPreferences,
    values: &BTreeMap<String, String>,
) -> Result<(), String> {
    const KEY: &str = PLATFORM_LAYOUT_KEY;
    let revision = |raw: Option<&String>| -> u64 {
        raw.and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
            .and_then(|value| {
                value
                    .get("_layoutUpdatedAt")
                    .and_then(|value| value.as_u64())
            })
            .unwrap_or(0)
    };
    if values.contains_key(KEY) && revision(values.get(KEY)) < revision(preferences.values.get(KEY))
    {
        return Err("平台布局已在其他窗口更新，已保留较新设置，请重新加载后重试".into());
    }
    Ok(())
}

pub fn save_ui_preferences(values: BTreeMap<String, String>) -> Result<UiPreferences, String> {
    let _guard = PREFERENCES_LOCK
        .try_lock()
        .map_err(|_| "界面偏好正在读写，请重试".to_string())?;
    let path = preferences_path()?;
    let mut preferences = read_preferences_from_path(&path)?;
    check_layout_revision(&preferences, &values)?;
    if apply_values(&mut preferences, values) {
        write_preferences_to_path(&path, &preferences)?;
    }
    Ok(preferences)
}

#[cfg(test)]
mod tests {
    use super::{read_preferences_from_path, write_preferences_to_path, UiPreferences};
    use std::collections::BTreeMap;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn stale_window_cannot_overwrite_newer_layout() {
        let key = "agtools.platform_layout.v1".to_string();
        let preferences = UiPreferences {
            values: BTreeMap::from([(key.clone(), r#"{"_layoutUpdatedAt":200}"#.into())]),
        };
        assert!(super::check_layout_revision(
            &preferences,
            &BTreeMap::from([(key.clone(), r#"{"_layoutUpdatedAt":100}"#.into())])
        )
        .is_err());
        assert!(super::check_layout_revision(
            &preferences,
            &BTreeMap::from([(key, r#"{"_layoutUpdatedAt":201}"#.into())])
        )
        .is_ok());
    }

    #[test]
    fn roundtrip_preference_values() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("cockpit-ui-preferences-{stamp}.json"));
        let _ = std::fs::remove_file(&path);

        let empty = read_preferences_from_path(&path).expect("empty preferences");
        assert!(empty.values.is_empty());

        let mut preferences = UiPreferences::default();
        preferences.values.insert(
            "agtools.platform_layout.v1".to_string(),
            "{\"sidebarEntryIds\":[\"group:codex-suite\"]}".to_string(),
        );
        write_preferences_to_path(&path, &preferences).expect("write");

        let loaded = read_preferences_from_path(&path).expect("reload");
        assert!(loaded
            .values
            .get("agtools.platform_layout.v1")
            .unwrap()
            .contains("group:codex-suite"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn empty_or_invalid_file_errors_without_overwriting_data() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("cockpit-ui-preferences-bad-{stamp}.json"));
        std::fs::write(&path, "   ").expect("write empty");
        assert!(read_preferences_from_path(&path).is_err());

        std::fs::write(&path, "{not-json").expect("write invalid");
        assert!(read_preferences_from_path(&path).is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_merges_without_dropping_other_keys() {
        let mut preferences = UiPreferences::default();
        preferences
            .values
            .insert("agtools.platform_layout.v1".to_string(), "old".to_string());
        preferences
            .values
            .insert("keep".to_string(), "yes".to_string());

        assert!(super::apply_values(
            &mut preferences,
            BTreeMap::from([("agtools.platform_layout.v1".to_string(), "next".to_string())]),
        ));
        assert_eq!(
            preferences.values.get("agtools.platform_layout.v1"),
            Some(&"next".to_string())
        );
        assert_eq!(preferences.values.get("keep"), Some(&"yes".to_string()));
        assert!(!super::apply_values(
            &mut preferences,
            BTreeMap::from([("agtools.platform_layout.v1".to_string(), "next".to_string())]),
        ));
    }
}
