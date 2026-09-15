//! 会话历史清洗：把第三方（DeepSeek 等）产生的 reasoning 项清成官方可接受的形状。
//!
//! 背景：官方 Codex 后端要求 reasoning 项的 `content` 必须是空数组。第三方提供商在历史里
//! 留下带可见思考文本的 reasoning 项（`content` 非空），同一会话切到官方账号后整段请求会被拒：
//! `[ArrayParam] [input[i].content] [array_above_max_length]`，普通回合与自动压缩都无法继续。
//!
//! 新的脏数据已由网关响应出口统一改写（见 sidecar `responses_reasoning_sanitize.go`），
//! 所以这里只在应用启动后做一次性后台迁移：把客户端历史库 `thread_history_*.sqlite` 里
//! `thread_items` 的这类项清空 `content`，使旧会话在官方账号下也能继续使用。
//!
//! 成本控制：查询先按官方客户端写入的 `item_type` 过滤，避免对历史库里的大字段（文件改动、
//! 长回复）做全表 `LIKE`；备份只记录被改动行的原始内容，不做整库快照——客户端历史库可能有
//! 数 GB，整库备份会占用同量级磁盘与 IO。操作幂等、可重复执行。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::modules;

const HISTORY_DB_STEM: &str = "thread_history";
const HISTORY_DB_EXTENSION: &str = "sqlite";
const SQLITE_DIR_NAME: &str = "sqlite";
const BACKUP_DIR_NAME: &str = "cockpit-history-sanitize-backup";
const MAX_BACKUP_FILES: usize = 3;
const BUSY_TIMEOUT_SECONDS: u64 = 5;
const ITEM_TABLES: [&str; 2] = ["thread_items", "thread_realtime_items"];
/// 一次性迁移标识：标识变化时会对所有 profile 目录重新执行一次。
const ONE_TIME_MIGRATION_ID: &str = "third_party_reasoning_content_v1";
const ONE_TIME_MIGRATION_STATE_FILE: &str = "codex_history_reasoning_sanitize.json";

static HISTORY_SANITIZE_LOCK: Mutex<()> = Mutex::new(());

/// 会话历史清洗结果，用于日志与上层判断是否需要提示。
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexSessionHistorySanitizeSummary {
    pub database_count: usize,
    pub updated_item_count: usize,
    pub changed_thread_count: usize,
}

impl CodexSessionHistorySanitizeSummary {
    pub fn changed_anything(&self) -> bool {
        self.updated_item_count > 0
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct OneTimeSanitizeState {
    #[serde(default)]
    migration: String,
    #[serde(default)]
    sanitized_dirs: std::collections::BTreeMap<String, CodexSessionHistorySanitizeSummary>,
    /// 会话日志（rollout）清洗状态：`pending_files` 为空表示该目录已处理完。
    #[serde(default)]
    rollout_dirs: std::collections::BTreeMap<String, CodexRolloutDirState>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct CodexRolloutDirState {
    #[serde(default)]
    pending_files: Vec<String>,
}

/// 一次性迁移的汇总结果，用于启动日志。
#[derive(Debug, Default, Clone)]
pub struct OneTimeSanitizeOutcome {
    pub history: CodexSessionHistorySanitizeSummary,
    pub rollout: super::codex_session_rollout_sanitize::CodexRolloutSanitizeSummary,
}

/// 应用启动后在后台执行的一次性迁移：为每个 Codex profile 目录各清洗一次历史库与会话日志。
///
/// 结果记录在应用数据目录的迁移状态文件里，记录过的目录会跳过，因此不会在每次切号或启动
/// 时重复扫描。目录已在配置里但未记录（例如后来新增/导入的实例）时会补做一次；
/// 上次因为仍被客户端写入而跳过的会话日志会在下次启动继续处理。
pub fn run_one_time_reasoning_history_sanitize() -> Result<OneTimeSanitizeOutcome, String> {
    run_one_time_sanitize_with(&one_time_sanitize_state_path(), codex_profile_dirs())
}

fn run_one_time_sanitize_with(
    state_path: &Path,
    profile_dirs: Vec<PathBuf>,
) -> Result<OneTimeSanitizeOutcome, String> {
    let mut state = load_one_time_sanitize_state(state_path);
    if state.migration != ONE_TIME_MIGRATION_ID {
        state.migration = ONE_TIME_MIGRATION_ID.to_string();
        state.sanitized_dirs.clear();
        state.rollout_dirs.clear();
    }

    let mut outcome = OneTimeSanitizeOutcome::default();
    let mut state_changed = false;
    for dir in profile_dirs {
        let key = dir.to_string_lossy().to_string();
        if !dir.is_dir() {
            continue;
        }

        let history_done = state.sanitized_dirs.contains_key(&key);
        let rollout_pending = match state.rollout_dirs.get(&key) {
            Some(existing) if existing.pending_files.is_empty() => None,
            Some(existing) => Some(existing.pending_files.clone()),
            // 首次遇到该目录：全量扫描会话日志。
            None => Some(Vec::new()),
        };
        if history_done && rollout_pending.is_none() {
            continue;
        }

        if !history_done {
            match sanitize_official_incompatible_reasoning_history(&dir) {
                Ok(result) => {
                    outcome.history.database_count += result.database_count;
                    outcome.history.updated_item_count += result.updated_item_count;
                    outcome.history.changed_thread_count += result.changed_thread_count;
                    state.sanitized_dirs.insert(key.clone(), result);
                    state_changed = true;
                }
                // 单个目录失败不记录，下次启动会重试其它未迁移目录，不影响应用其它功能。
                Err(error) => modules::logger::log_warn(&format!(
                    "[Codex History Sanitize] 一次性清理第三方推理历史失败: data_dir={}, error={}",
                    dir.display(),
                    error
                )),
            }
        }

        if let Some(pending) = rollout_pending {
            let retry_files = pending.iter().map(PathBuf::from).collect::<Vec<_>>();
            match super::codex_session_rollout_sanitize::sanitize_official_incompatible_rollout_signatures(
                &dir,
                &retry_files,
            ) {
                Ok((result, backups)) => {
                    outcome.rollout.files_scanned += result.files_scanned;
                    outcome.rollout.files_changed += result.files_changed;
                    outcome.rollout.removed_signatures += result.removed_signatures;
                    outcome.rollout.pending_files.extend(result.pending_files.clone());
                    if !backups.is_empty() {
                        if let Err(error) = write_rollout_signature_backup(&dir, &backups) {
                            modules::logger::log_warn(&format!(
                                "[Codex Rollout Sanitize] 记录签名回滚备份失败: data_dir={}, error={}",
                                dir.display(),
                                error
                            ));
                        }
                    }
                    state.rollout_dirs.insert(
                        key.clone(),
                        CodexRolloutDirState {
                            pending_files: result.pending_files,
                        },
                    );
                    state_changed = true;
                }
                Err(error) => modules::logger::log_warn(&format!(
                    "[Codex Rollout Sanitize] 一次性清理会话日志签名失败: data_dir={}, error={}",
                    dir.display(),
                    error
                )),
            }
        }
    }

    if state_changed {
        save_one_time_sanitize_state(state_path, &state)?;
    }
    Ok(outcome)
}

/// 需要迁移的 Codex profile 目录：默认实例 CODEX_HOME 与所有多开实例目录。
fn codex_profile_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![modules::codex_account::get_codex_home()];
    if let Ok(store) = modules::codex_instance::load_instance_store() {
        for instance in store.instances {
            let path = PathBuf::from(instance.user_data_dir.trim());
            if path.as_os_str().is_empty() {
                continue;
            }
            dirs.push(path);
        }
    }
    dirs.sort();
    dirs.dedup();
    dirs
}

fn one_time_sanitize_state_path() -> PathBuf {
    modules::account::get_data_dir()
        .unwrap_or_else(|_| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".antigravity_cockpit")
        })
        .join(ONE_TIME_MIGRATION_STATE_FILE)
}

fn load_one_time_sanitize_state(path: &Path) -> OneTimeSanitizeState {
    let Ok(content) = fs::read_to_string(path) else {
        return OneTimeSanitizeState::default();
    };
    serde_json::from_str(&content).unwrap_or_default()
}

/// 会话日志签名清洗的回滚备份（被删除的假签名清单）。
fn write_rollout_signature_backup(
    data_dir: &Path,
    backups: &[super::codex_session_rollout_sanitize::CodexRolloutSignatureBackup],
) -> Result<(), String> {
    let backup_root = data_dir.join(BACKUP_DIR_NAME);
    fs::create_dir_all(&backup_root).map_err(|error| {
        format!(
            "创建会话日志备份目录失败 ({}): {}",
            backup_root.display(),
            error
        )
    })?;
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default();
    let backup_file = backup_root.join(format!("rollout-signatures-{timestamp}.json"));
    let serialized = serde_json::to_vec_pretty(backups)
        .map_err(|error| format!("序列化会话日志签名备份失败: {}", error))?;
    let temp_path = backup_file.with_extension("json.tmp");
    fs::write(&temp_path, &serialized)
        .map_err(|error| format!("写入会话日志签名备份失败 ({}): {}", temp_path.display(), error))?;
    fs::rename(&temp_path, &backup_file).map_err(|error| {
        format!(
            "更新会话日志签名备份失败 ({}): {}",
            backup_file.display(),
            error
        )
    })?;
    prune_backup_files(&backup_root);
    modules::logger::log_info(&format!(
        "[Codex Rollout Sanitize] 已记录签名回滚备份: file={}, entries={}",
        backup_file.display(),
        backups.len()
    ));
    Ok(())
}

fn save_one_time_sanitize_state(path: &Path, state: &OneTimeSanitizeState) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            format!(
                "创建会话历史迁移状态目录失败 ({}): {}",
                parent.display(),
                error
            )
        })?;
    }
    let serialized = serde_json::to_vec_pretty(state)
        .map_err(|error| format!("序列化会话历史迁移状态失败: {}", error))?;
    let temp_path = path.with_extension("json.tmp");
    fs::write(&temp_path, serialized).map_err(|error| {
        format!(
            "写入会话历史迁移状态失败 ({}): {}",
            temp_path.display(),
            error
        )
    })?;
    fs::rename(&temp_path, path)
        .map_err(|error| format!("更新会话历史迁移状态失败 ({}): {}", path.display(), error))
}

/// 清理指定实例目录下所有历史库里的第三方 reasoning 项（幂等）。
pub fn sanitize_official_incompatible_reasoning_history(
    data_dir: &Path,
) -> Result<CodexSessionHistorySanitizeSummary, String> {
    let _guard = HISTORY_SANITIZE_LOCK
        .lock()
        .map_err(|_| "会话历史清洗锁已中毒".to_string())?;

    let databases = history_database_paths(data_dir);
    let mut summary = CodexSessionHistorySanitizeSummary {
        database_count: databases.len(),
        ..Default::default()
    };
    if databases.is_empty() {
        return Ok(summary);
    }

    let mut backups: Vec<HistoryBackupDatabase> = Vec::new();
    for database in databases {
        let (updated_items, changed_threads, backup) = sanitize_history_database(&database)?;
        if let Some(backup) = backup {
            backups.push(backup);
        }
        summary.updated_item_count += updated_items;
        summary.changed_thread_count += changed_threads;
    }
    if !backups.is_empty() {
        write_history_backup(data_dir, backups)?;
    }
    Ok(summary)
}

/// 历史库候选：实例根目录与 `sqlite/` 子目录下的 `thread_history_*.sqlite`。
fn history_database_paths(data_dir: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    collect_history_databases(data_dir, &mut paths);
    collect_history_databases(&data_dir.join(SQLITE_DIR_NAME), &mut paths);
    paths.sort();
    paths.dedup();
    paths
}

fn collect_history_databases(dir: &Path, paths: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let matches_stem = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .is_some_and(|stem| stem.starts_with(HISTORY_DB_STEM));
        let matches_extension = path
            .extension()
            .and_then(|extension| extension.to_str())
            == Some(HISTORY_DB_EXTENSION);
        if matches_stem && matches_extension {
            paths.push(path);
        }
    }
}

fn sanitize_history_database(
    database: &Path,
) -> Result<(usize, usize, Option<HistoryBackupDatabase>), String> {
    let connection = Connection::open(database)
        .map_err(|error| format!("打开会话历史库失败 ({}): {}", database.display(), error))?;
    connection
        .busy_timeout(Duration::from_secs(BUSY_TIMEOUT_SECONDS))
        .map_err(|error| {
            format!(
                "设置会话历史库 busy_timeout 失败 ({}): {}",
                database.display(),
                error
            )
        })?;

    let pending = collect_sanitize_updates(&connection, database)?;
    if pending.is_empty() {
        return Ok((0, 0, None));
    }

    let backup = HistoryBackupDatabase {
        path: database.to_string_lossy().to_string(),
        rows: pending.iter().map(HistoryBackupRow::from).collect(),
    };

    let transaction = connection
        .unchecked_transaction()
        .map_err(|error| format!("开启会话历史库事务失败 ({}): {}", database.display(), error))?;
    for update in &pending {
        transaction
            .execute(
                "UPDATE thread_items SET item_json = ?1 WHERE rowid = ?2",
                rusqlite::params![update.item_json, update.rowid],
            )
            .map_err(|error| {
                format!(
                    "写入会话历史项失败 ({} rowid={}): {}",
                    database.display(),
                    update.rowid,
                    error
                )
            })?;
    }
    transaction
        .commit()
        .map_err(|error| format!("提交会话历史库事务失败 ({}): {}", database.display(), error))?;

    let changed_threads = pending
        .iter()
        .map(|update| update.thread_id.as_str())
        .collect::<std::collections::HashSet<_>>()
        .len();
    modules::logger::log_info(&format!(
        "[Codex History Sanitize] 已清理第三方推理历史: database={}, updated_items={}, changed_threads={}",
        database.display(),
        pending.len(),
        changed_threads
    ));
    Ok((pending.len(), changed_threads, Some(backup)))
}

struct HistoryItemUpdate {
    rowid: i64,
    thread_id: String,
    turn_id: String,
    item_id: String,
    /// 改写前的原始内容，用于回滚。
    original_item_json: String,
    /// 改写后的内容，用于写库。
    item_json: String,
}

/// 被改动行的原始内容备份（用于手动回滚）。只记录被改动的 reasoning 行，
/// 避免对可能数 GB 的历史库做整库快照。
#[derive(Debug, Serialize)]
struct HistoryBackupDatabase {
    path: String,
    rows: Vec<HistoryBackupRow>,
}

#[derive(Debug, Serialize)]
struct HistoryBackupRow {
    rowid: i64,
    thread_id: String,
    turn_id: String,
    item_id: String,
    item_json: String,
}

impl From<&HistoryItemUpdate> for HistoryBackupRow {
    fn from(update: &HistoryItemUpdate) -> Self {
        Self {
            rowid: update.rowid,
            thread_id: update.thread_id.clone(),
            turn_id: update.turn_id.clone(),
            item_id: update.item_id.clone(),
            item_json: update.original_item_json.clone(),
        }
    }
}

#[derive(Debug, Serialize)]
struct HistoryBackupFile {
    migration: String,
    created_at_ms: u128,
    databases: Vec<HistoryBackupDatabase>,
}

/// 收集需要清理的行；已经是 `content: []` 的行会被跳过，保证幂等。
fn collect_sanitize_updates(
    connection: &Connection,
    database: &Path,
) -> Result<Vec<HistoryItemUpdate>, String> {
    let mut updates = Vec::new();
    for table in ITEM_TABLES {
        if !table_exists(connection, table)? {
            continue;
        }
        if table != "thread_items" {
            // 其它表结构与 thread_items 不同（无 thread_id 语义），当前只处理主历史表。
            continue;
        }
        // 官方客户端把条目类型写在 item_type 列；先按它过滤可以跳过文件改动、长回复等大字段，
        // 避免对整张表做 LIKE。列缺失或为空时仍回退到原来的全文匹配，保证不漏项。
        let filter = if table_has_column(connection, table, "item_type")? {
            "item_type = 'reasoning' OR (item_type = '' AND item_json LIKE '%\"reasoning\"%')"
        } else {
            "item_json LIKE '%\"reasoning\"%'"
        };
        let mut statement = connection
            .prepare(&format!(
                "SELECT rowid, thread_id, turn_id, item_id, item_json FROM {table} WHERE {filter}"
            ))
            .map_err(|error| {
                format!(
                    "查询会话历史项失败 ({} / {}): {}",
                    database.display(),
                    table,
                    error
                )
            })?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })
            .map_err(|error| {
                format!(
                    "读取会话历史项失败 ({} / {}): {}",
                    database.display(),
                    table,
                    error
                )
            })?;
        for row in rows {
            let (rowid, thread_id, turn_id, item_id, item_json) = row.map_err(|error| {
                format!(
                    "解析会话历史项失败 ({} / {}): {}",
                    database.display(),
                    table,
                    error
                )
            })?;
            let Some(sanitized) = sanitize_history_item_json(&item_json) else {
                continue;
            };
            updates.push(HistoryItemUpdate {
                rowid,
                thread_id,
                turn_id,
                item_id,
                original_item_json: item_json,
                item_json: sanitized,
            });
        }
    }
    Ok(updates)
}

/// 把第三方 reasoning 项的 `content` 清空；官方形状（空数组）原样返回 `None`。
fn sanitize_history_item_json(item_json: &str) -> Option<String> {
    let mut value: Value = serde_json::from_str(item_json).ok()?;
    let object = value.as_object_mut()?;
    if object.get("type").and_then(Value::as_str) != Some("reasoning") {
        return None;
    }
    let has_visible_content = object
        .get("content")
        .and_then(Value::as_array)
        .is_some_and(|content| !content.is_empty());
    if !has_visible_content {
        return None;
    }
    object.insert("content".to_string(), Value::Array(Vec::new()));
    serde_json::to_string(&value).ok()
}

fn table_exists(connection: &Connection, table: &str) -> Result<bool, String> {
    let count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table],
            |row| row.get(0),
        )
        .map_err(|error| format!("读取 SQLite 表结构失败 (table={}): {}", table, error))?;
    Ok(count > 0)
}

fn table_has_column(connection: &Connection, table: &str, column: &str) -> Result<bool, String> {
    let mut statement = connection
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|error| format!("读取 SQLite 表结构失败 (table={}): {}", table, error))?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|error| format!("读取 SQLite 表结构失败 (table={}): {}", table, error))?;
    for name in rows {
        let name =
            name.map_err(|error| format!("读取 SQLite 表结构失败 (table={}): {}", table, error))?;
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
}

fn write_history_backup(
    data_dir: &Path,
    databases: Vec<HistoryBackupDatabase>,
) -> Result<(), String> {
    let backup_root = data_dir.join(BACKUP_DIR_NAME);
    fs::create_dir_all(&backup_root).map_err(|error| {
        format!(
            "创建会话历史备份目录失败 ({}): {}",
            backup_root.display(),
            error
        )
    })?;
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default();
    let backup_file = backup_root.join(format!("sanitize-{timestamp}.json"));
    let payload = HistoryBackupFile {
        migration: ONE_TIME_MIGRATION_ID.to_string(),
        created_at_ms: timestamp,
        databases,
    };
    let serialized = serde_json::to_vec(&payload)
        .map_err(|error| format!("序列化会话历史备份失败: {}", error))?;
    let temp_path = backup_file.with_extension("json.tmp");
    fs::write(&temp_path, &serialized)
        .map_err(|error| format!("写入会话历史备份失败 ({}): {}", temp_path.display(), error))?;
    fs::rename(&temp_path, &backup_file).map_err(|error| {
        format!(
            "更新会话历史备份失败 ({}): {}",
            backup_file.display(),
            error
        )
    })?;
    prune_backup_files(&backup_root);
    modules::logger::log_info(&format!(
        "[Codex History Sanitize] 已记录可回滚备份: file={}, bytes={}",
        backup_file.display(),
        serialized.len()
    ));
    Ok(())
}

fn prune_backup_files(backup_root: &Path) {
    let Ok(entries) = fs::read_dir(backup_root) else {
        return;
    };
    let mut files = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();
    if files.len() <= MAX_BACKUP_FILES {
        return;
    }
    files.sort();
    let remove_count = files.len() - MAX_BACKUP_FILES;
    for path in files.into_iter().take(remove_count) {
        let _ = fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn make_temp_dir(prefix: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("{}-{}-{}", prefix, std::process::id(), unique));
        if dir.exists() {
            fs::remove_dir_all(&dir).expect("cleanup");
        }
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn create_history_db(dir: &Path) -> PathBuf {
        let path = dir.join("thread_history_1.sqlite");
        let connection = Connection::open(&path).expect("open db");
        connection
            .execute_batch(
                "CREATE TABLE thread_items (
                    thread_id TEXT NOT NULL,
                    turn_id TEXT NOT NULL,
                    item_id TEXT NOT NULL,
                    rollout_ordinal INTEGER NOT NULL,
                    created_at_ms INTEGER NOT NULL,
                    item_json TEXT NOT NULL,
                    item_type TEXT NOT NULL DEFAULT '',
                    updated_at_ordinal INTEGER NOT NULL DEFAULT 0,
                    PRIMARY KEY (thread_id, turn_id, item_id)
                );",
            )
            .expect("create table");
        path
    }

    fn insert_item(connection: &Connection, thread: &str, item_id: &str, item_json: &str) {
        insert_typed_item(connection, thread, item_id, item_json, "reasoning");
    }

    fn insert_typed_item(
        connection: &Connection,
        thread: &str,
        item_id: &str,
        item_json: &str,
        item_type: &str,
    ) {
        connection
            .execute(
                "INSERT INTO thread_items (thread_id, turn_id, item_id, rollout_ordinal, created_at_ms, item_json, item_type)
                 VALUES (?1, 'turn-1', ?2, 0, 0, ?3, ?4)",
                rusqlite::params![thread, item_id, item_json, item_type],
            )
            .expect("insert item");
    }

    fn backup_files(dir: &Path) -> Vec<PathBuf> {
        let mut files = fs::read_dir(dir.join(BACKUP_DIR_NAME))
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.path())
                    .filter(|path| path.is_file())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        files.sort();
        files
    }

    #[test]
    fn clears_third_party_reasoning_content_and_is_idempotent() {
        let dir = make_temp_dir("codex-history-sanitize-test");
        let db = create_history_db(&dir);
        let connection = Connection::open(&db).expect("open db");
        insert_item(
            &connection,
            "thread-1",
            "item-1",
            r#"{"type":"reasoning","id":"r1","summary":[],"content":["thinking text"]}"#,
        );
        insert_item(
            &connection,
            "thread-1",
            "item-2",
            r#"{"type":"reasoning","id":"r2","summary":["done"],"content":[]}"#,
        );
        insert_item(
            &connection,
            "thread-2",
            "item-3",
            r#"{"type":"agentMessage","id":"m1","text":"hello"}"#,
        );
        drop(connection);

        let first = sanitize_official_incompatible_reasoning_history(&dir).expect("sanitize");
        assert_eq!(first.database_count, 1);
        assert_eq!(first.updated_item_count, 1);
        assert_eq!(first.changed_thread_count, 1);

        let connection = Connection::open(&db).expect("reopen db");
        let updated: String = connection
            .query_row(
                "SELECT item_json FROM thread_items WHERE item_id = 'item-1'",
                [],
                |row| row.get(0),
            )
            .expect("read updated item");
        assert_eq!(
            updated,
            r#"{"content":[],"id":"r1","summary":[],"type":"reasoning"}"#
        );
        let untouched: String = connection
            .query_row(
                "SELECT item_json FROM thread_items WHERE item_id = 'item-3'",
                [],
                |row| row.get(0),
            )
            .expect("read untouched item");
        assert!(untouched.contains("agentMessage"));
        drop(connection);

        let second = sanitize_official_incompatible_reasoning_history(&dir).expect("sanitize again");
        assert_eq!(second.updated_item_count, 0);
        assert!(!second.changed_anything());

        let backups = backup_files(&dir);
        assert_eq!(backups.len(), 1, "应生成一份可回滚备份");
        let payload: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&backups[0]).expect("read backup"))
                .expect("parse backup");
        assert_eq!(
            payload["databases"][0]["rows"][0]["item_json"].as_str(),
            Some(r#"{"type":"reasoning","id":"r1","summary":[],"content":["thinking text"]}"#),
            "备份应保留被改动行的原始内容以便回滚"
        );
    }

    #[test]
    fn ignores_directories_without_history_databases() {
        let dir = make_temp_dir("codex-history-sanitize-empty");
        let summary = sanitize_official_incompatible_reasoning_history(&dir).expect("sanitize");
        assert_eq!(summary.database_count, 0);
        assert_eq!(summary.updated_item_count, 0);
        assert!(backup_files(&dir).is_empty(), "无改动时不应生成备份");
    }

    #[test]
    fn sanitizes_reasoning_rows_with_empty_item_type() {
        let dir = make_temp_dir("codex-history-sanitize-empty-type");
        let db = create_history_db(&dir);
        let connection = Connection::open(&db).expect("open db");
        insert_typed_item(
            &connection,
            "thread-1",
            "item-1",
            r#"{"type":"reasoning","id":"r1","summary":[],"content":["thinking text"]}"#,
            "",
        );
        insert_item(
            &connection,
            "thread-1",
            "item-2",
            r#"{"type":"agentMessage","id":"m1","text":"keep me"}"#,
        );
        drop(connection);

        let summary = sanitize_official_incompatible_reasoning_history(&dir).expect("sanitize");
        assert_eq!(
            summary.updated_item_count, 1,
            "item_type 为空时仍应按 JSON 命中"
        );
    }

    #[test]
    fn one_time_migration_runs_once_per_profile_dir() {
        let dir = make_temp_dir("codex-history-migration");
        let state_path = dir.join("state").join(ONE_TIME_MIGRATION_STATE_FILE);
        let default_dir = dir.join("default");
        fs::create_dir_all(&default_dir).expect("create default dir");
        let db = create_history_db(&default_dir);
        let connection = Connection::open(&db).expect("open db");
        insert_item(
            &connection,
            "thread-1",
            "item-1",
            r#"{"type":"reasoning","id":"r1","summary":[],"content":["thinking text"]}"#,
        );
        drop(connection);

        let first = run_one_time_sanitize_with(&state_path, vec![default_dir.clone()])
            .expect("first migration");
        assert_eq!(first.history.updated_item_count, 1);

        // 再次执行时该目录已记录，不再扫描历史库。
        let second = run_one_time_sanitize_with(&state_path, vec![default_dir.clone()])
            .expect("second migration");
        assert_eq!(second.history.database_count, 0);
        assert_eq!(second.history.updated_item_count, 0);

        // 新增目录（例如后来导入的实例）会补做一次。
        let instance_dir = dir.join("instance");
        fs::create_dir_all(&instance_dir).expect("create instance dir");
        create_history_db(&instance_dir);
        let connection =
            Connection::open(instance_dir.join("thread_history_1.sqlite")).expect("open db");
        insert_item(
            &connection,
            "thread-2",
            "item-2",
            r#"{"type":"reasoning","id":"r2","summary":[],"content":["other thinking"]}"#,
        );
        drop(connection);
        let third = run_one_time_sanitize_with(&state_path, vec![default_dir, instance_dir])
            .expect("third migration");
        assert_eq!(third.history.updated_item_count, 1);
    }

    #[test]
    fn one_time_migration_state_resets_when_migration_id_changes() {
        let dir = make_temp_dir("codex-history-migration-reset");
        let state_path = dir.join(ONE_TIME_MIGRATION_STATE_FILE);
        let profile_dir = dir.join("default");
        fs::create_dir_all(&profile_dir).expect("create profile dir");
        create_history_db(&profile_dir);
        let connection =
            Connection::open(profile_dir.join("thread_history_1.sqlite")).expect("open db");
        insert_item(
            &connection,
            "thread-1",
            "item-1",
            r#"{"type":"reasoning","id":"r1","summary":[],"content":["thinking text"]}"#,
        );
        drop(connection);

        run_one_time_sanitize_with(&state_path, vec![profile_dir.clone()])
            .expect("first migration");
        let mut state = load_one_time_sanitize_state(&state_path);
        state.migration = "legacy".to_string();
        save_one_time_sanitize_state(&state_path, &state).expect("save legacy state");

        let rerun = run_one_time_sanitize_with(&state_path, vec![profile_dir])
            .expect("migration after id change");
        assert_eq!(rerun.history.database_count, 1);
        assert_eq!(
            rerun.history.updated_item_count, 0,
            "历史已清洗，重复执行应幂等"
        );
    }

    #[test]
    fn one_time_migration_cleans_idle_rollouts_and_defers_active_ones() {
        let dir = make_temp_dir("codex-rollout-migration");
        let state_path = dir.join(ONE_TIME_MIGRATION_STATE_FILE);
        let profile_dir = dir.join("default");
        let session_dir = profile_dir
            .join("sessions")
            .join("2026")
            .join("09")
            .join("14");
        fs::create_dir_all(&session_dir).expect("create session dir");
        let idle_rollout = session_dir.join("rollout-idle.jsonl");
        fs::write(
            &idle_rollout,
            "{\"ordinal\":11,\"payload\":{\"type\":\"reasoning\",\"id\":\"r1\",\"encrypted_content\":\"bf0ff0e2-ffac-4bf0-a39a-80b340896e1c-0\"}}\n",
        )
        .expect("write rollout");
        let active_rollout = session_dir.join("rollout-active.jsonl");
        fs::write(
            &active_rollout,
            "{\"ordinal\":11,\"payload\":{\"type\":\"reasoning\",\"id\":\"r2\",\"encrypted_content\":\"73cbd22c-9f79-439f-b5a3-001d40bb4a80-0\"}}\n",
        )
        .expect("write rollout");
        let idle_modified_at = std::time::SystemTime::now() - Duration::from_secs(3600);
        crate::modules::codex_session_file_time::restore_modified_time(
            &idle_rollout,
            Some(idle_modified_at),
        )
        .expect("backdate idle rollout");

        let outcome =
            run_one_time_sanitize_with(&state_path, vec![profile_dir.clone()]).expect("migration");

        assert_eq!(outcome.rollout.files_changed, 1);
        assert_eq!(outcome.rollout.removed_signatures, 1);
        assert_eq!(outcome.rollout.pending_files.len(), 1);
        let cleaned = fs::read_to_string(&idle_rollout).expect("read cleaned rollout");
        assert!(!cleaned.contains("encrypted_content"));
        let deferred = fs::read_to_string(&active_rollout).expect("read active rollout");
        assert!(
            deferred.contains("encrypted_content"),
            "活跃会话日志应留到下次启动"
        );

        // 状态里记录待处理文件，且已清洗目录不会重复全量扫描。
        let state = load_one_time_sanitize_state(&state_path);
        let rollout_state = state
            .rollout_dirs
            .get(&profile_dir.to_string_lossy().to_string())
            .expect("rollout state");
        assert_eq!(rollout_state.pending_files.len(), 1);
        let second = run_one_time_sanitize_with(&state_path, vec![profile_dir]).expect("second");
        assert_eq!(second.rollout.files_scanned, 1, "只重试待处理文件");
    }

    #[test]
    fn sanitizes_items_inside_sqlite_subdirectory() {
        let dir = make_temp_dir("codex-history-sanitize-subdir");
        let sqlite_dir = dir.join(SQLITE_DIR_NAME);
        fs::create_dir_all(&sqlite_dir).expect("create sqlite dir");
        let db = sqlite_dir.join("thread_history_2.sqlite");
        let connection = Connection::open(&db).expect("open db");
        connection
            .execute_batch(
                "CREATE TABLE thread_items (
                    thread_id TEXT NOT NULL,
                    turn_id TEXT NOT NULL,
                    item_id TEXT NOT NULL,
                    rollout_ordinal INTEGER NOT NULL,
                    created_at_ms INTEGER NOT NULL,
                    item_json TEXT NOT NULL,
                    item_type TEXT NOT NULL DEFAULT '',
                    updated_at_ordinal INTEGER NOT NULL DEFAULT 0,
                    PRIMARY KEY (thread_id, turn_id, item_id)
                );",
            )
            .expect("create table");
        insert_item(
            &connection,
            "thread-9",
            "item-9",
            r#"{"type":"reasoning","id":"r9","summary":[],"content":[{"type":"reasoning_text","text":"x"}]}"#,
        );
        drop(connection);

        let summary = sanitize_official_incompatible_reasoning_history(&dir).expect("sanitize");
        assert_eq!(summary.updated_item_count, 1);
        let connection = Connection::open(&db).expect("reopen");
        let updated: String = connection
            .query_row(
                "SELECT item_json FROM thread_items WHERE item_id = 'item-9'",
                [],
                |row| row.get(0),
            )
            .expect("read item");
        assert!(updated.contains(r#""content":[]"#));
    }
}
