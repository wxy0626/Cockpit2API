//! 应用标识符由 com.jlcodes.cockpit-tools 改为 com.wxy0626.cockpit2api 后，
//! WebView2 缓存、WorkBuddy cookie 等按标识符存储的数据目录需要一次性搬迁。
//! 必须在 Tauri 创建任何应用目录之前调用（应用 bin 的 main 最前）。

use std::path::{Path, PathBuf};

const LEGACY_IDENTIFIER: &str = "com.jlcodes.cockpit-tools";
const NEW_IDENTIFIER: &str = "com.wxy0626.cockpit2api";
const LEGACY_DEV_IDENTIFIER: &str = "com.jlcodes.cockpit-tools.dev";
const NEW_DEV_IDENTIFIER: &str = "com.wxy0626.cockpit2api.dev";

/// 可能存放按标识符命名目录的 base（config/data 在部分平台相同，重复检测无害）。
fn migration_bases() -> Vec<PathBuf> {
    let mut bases = Vec::new();
    if let Some(dir) = dirs::config_dir() {
        bases.push(dir);
    }
    if let Some(dir) = dirs::data_dir() {
        bases.push(dir);
    }
    if let Some(dir) = dirs::data_local_dir() {
        bases.push(dir);
    }
    #[cfg(target_os = "macos")]
    if let Some(home) = dirs::home_dir() {
        bases.push(home.join("Library/WebKit"));
    }
    bases
}

/// 旧目录存在且新目录不存在时整体改名；已迁移或双方并存时不做任何事。
fn migrate_pair(legacy: &Path, modern: &Path) -> bool {
    if !legacy.is_dir() || modern.exists() {
        return false;
    }
    std::fs::rename(legacy, modern).is_ok()
}

/// 迁移所有旧标识符目录，返回成功迁移的数量；失败不阻塞启动。
pub fn migrate_legacy_app_dirs() -> usize {
    let id_pairs = [
        (LEGACY_IDENTIFIER, NEW_IDENTIFIER),
        (LEGACY_DEV_IDENTIFIER, NEW_DEV_IDENTIFIER),
    ];
    let mut migrated = 0;
    for base in migration_bases() {
        for (legacy_id, modern_id) in id_pairs {
            if migrate_pair(&base.join(legacy_id), &base.join(modern_id)) {
                migrated += 1;
            }
        }
    }
    migrated
}

#[cfg(test)]
mod tests {
    use super::migrate_pair;

    fn unique_root(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "c2api-migration-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn moves_legacy_dir_into_place_when_target_missing() {
        let root = unique_root("move");
        let legacy = root.join("old-id");
        let modern = root.join("new-id");
        std::fs::create_dir_all(legacy.join("webviews/workbuddy")).unwrap();
        std::fs::write(legacy.join("webviews/workbuddy/cookies"), b"data").unwrap();

        assert!(migrate_pair(&legacy, &modern));
        assert!(modern.join("webviews/workbuddy/cookies").is_file());
        assert!(!legacy.exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn keeps_legacy_dir_when_target_already_exists() {
        let root = unique_root("keep");
        let legacy = root.join("old-id");
        let modern = root.join("new-id");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::create_dir_all(&modern).unwrap();

        assert!(!migrate_pair(&legacy, &modern));
        assert!(legacy.is_dir());
        let _ = std::fs::remove_dir_all(root);
    }
}
