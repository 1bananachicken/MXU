//! 更新文件事务。

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::path::Component;
use std::path::{Path, PathBuf};

use super::types::ChangesJson;

const TRANSACTION_DIR: &str = ".update-tx";
const PRESERVED_ROOTS: &[&str] = &["cache", "config", "debug", "logs"];

struct UpdatePlan {
    backup: Vec<PathBuf>,
    apply: Vec<PathBuf>,
}

/// 已完成路径和清单校验、可以进入更新临界区的安装计划。
pub struct PreparedUpdate {
    staging: PathBuf,
    target: PathBuf,
    plan: UpdatePlan,
}

impl PreparedUpdate {
    pub fn prepare(staging: &Path, target: &Path) -> Result<Self, String> {
        if !staging.is_dir() {
            return Err(format!("更新暂存目录不存在: [{}]", staging.display()));
        }
        if !target.is_dir() {
            return Err(format!("更新目标目录不存在: [{}]", target.display()));
        }

        Ok(Self {
            staging: staging.to_path_buf(),
            target: target.to_path_buf(),
            plan: build_update_plan(staging, target)?,
        })
    }

    pub fn install(self) -> Result<(), String> {
        let staging = &self.staging;
        let target = &self.target;
        let plan = &self.plan;

        let transaction_root = create_transaction_root(target)?;
        let backup_root = transaction_root.join("backup");
        fs::create_dir_all(&backup_root)
            .map_err(|e| format!("无法创建更新备份目录 [{}]: {}", backup_root.display(), e))?;

        let mut backed_up = Vec::new();
        for relative in &plan.backup {
            let source = target.join(relative);
            if !source.exists() {
                continue;
            }

            let destination = backup_root.join(relative);
            if let Some(parent) = destination.parent() {
                if let Err(error) = fs::create_dir_all(parent) {
                    let rollback_error = restore_backups(target, &backup_root, &backed_up).err();
                    return Err(format_transaction_error(
                        format!("无法创建备份父目录 [{}]: {}", parent.display(), error),
                        rollback_error,
                    ));
                }
            }

            if let Err(error) = rename_with_retry(&source, &destination) {
                let rollback_error = restore_backups(target, &backup_root, &backed_up).err();
                return Err(format_transaction_error(
                    format!(
                        "更新备份失败: [{}] -> [{}], error={}, raw_os_error={:?}",
                        source.display(),
                        destination.display(),
                        error,
                        error.raw_os_error()
                    ),
                    rollback_error,
                ));
            }
            backed_up.push(relative.clone());
        }

        if let Err(apply_error) = copy_entries(staging, target, &plan.apply) {
            let cleanup_error = remove_entries(target, &plan.apply).err();
            let rollback_error = restore_backups(target, &backup_root, &backed_up).err();
            return Err(format_transaction_error(
                format!("应用更新失败: {}", apply_error),
                combine_errors(cleanup_error, rollback_error),
            ));
        }

        if let Err(error) = fs::write(transaction_root.join("COMMITTED"), b"committed") {
            log::warn!(
                "更新已应用，但无法写入事务完成标记: path={}, error={}, raw_os_error={:?}",
                transaction_root.display(),
                error,
                error.raw_os_error()
            );
        }
        Ok(())
    }
}

fn build_update_plan(staging: &Path, target: &Path) -> Result<UpdatePlan, String> {
    let changes_path = staging.join("changes.json");
    if !changes_path.exists() {
        let entries = full_update_entries(staging)?;
        return Ok(UpdatePlan {
            backup: entries.clone(),
            apply: entries,
        });
    }

    let content = fs::read_to_string(&changes_path)
        .map_err(|e| format!("无法读取 changes.json [{}]: {}", changes_path.display(), e))?;
    let changes: ChangesJson = serde_json::from_str(&content)
        .map_err(|e| format!("无法解析 changes.json [{}]: {}", changes_path.display(), e))?;
    build_incremental_plan(staging, target, changes)
}

fn build_incremental_plan(
    staging: &Path,
    target: &Path,
    changes: ChangesJson,
) -> Result<UpdatePlan, String> {
    let mut classifications = HashMap::<String, &'static str>::new();
    let added = normalize_paths("added", changes.added, &mut classifications)?;
    let modified = normalize_paths("modified", changes.modified, &mut classifications)?;
    let deleted = normalize_paths("deleted", changes.deleted, &mut classifications)?;
    validate_non_overlapping_paths(&classifications)?;

    for relative in added.iter().chain(modified.iter()) {
        let source = staging.join(relative);
        if !source.exists() {
            return Err(format!(
                "changes.json 声明的新文件不存在: [{}]",
                source.display()
            ));
        }
    }

    for relative in &added {
        let destination = target.join(relative);
        if destination.exists() {
            return Err(format!(
                "changes.json 将已存在路径声明为 added: [{}]",
                destination.display()
            ));
        }
    }

    for relative in &modified {
        let destination = target.join(relative);
        if !destination.exists() {
            return Err(format!(
                "changes.json 将不存在路径声明为 modified: [{}]",
                destination.display()
            ));
        }
    }

    let mut backup = modified.clone();
    backup.extend(
        deleted
            .into_iter()
            .filter(|relative| target.join(relative).exists()),
    );
    backup.sort();

    let mut apply = modified;
    apply.extend(added);
    apply.sort();

    Ok(UpdatePlan { backup, apply })
}

fn normalize_paths(
    classification: &'static str,
    raw_paths: Vec<String>,
    classifications: &mut HashMap<String, &'static str>,
) -> Result<Vec<PathBuf>, String> {
    let mut result = Vec::new();
    let mut local = HashSet::new();

    for raw in raw_paths {
        let relative = normalize_relative_path(&raw)?;
        let key = path_key(&relative);
        if !local.insert(key.clone()) {
            return Err(format!(
                "changes.json 的 {} 包含重复路径: [{}]",
                classification, raw
            ));
        }
        if let Some(previous) = classifications.insert(key, classification) {
            return Err(format!(
                "changes.json 路径同时属于 {} 和 {}: [{}]",
                previous, classification, raw
            ));
        }
        result.push(relative);
    }

    result.sort();
    Ok(result)
}

fn validate_non_overlapping_paths(
    classifications: &HashMap<String, &'static str>,
) -> Result<(), String> {
    let paths = classifications.iter().collect::<Vec<_>>();
    for (index, (left_path, left_classification)) in paths.iter().enumerate() {
        for (right_path, right_classification) in paths.iter().skip(index + 1) {
            let left_prefix = format!("{}/", left_path);
            let right_prefix = format!("{}/", right_path);
            if right_path.starts_with(&left_prefix) || left_path.starts_with(&right_prefix) {
                return Err(format!(
                    "changes.json 包含相互重叠的路径: {} [{}] 与 {} [{}]",
                    left_classification, left_path, right_classification, right_path
                ));
            }
        }
    }
    Ok(())
}

fn normalize_relative_path(raw: &str) -> Result<PathBuf, String> {
    let normalized_separators = raw.trim().replace('\\', "/");
    if normalized_separators.is_empty() || normalized_separators.starts_with('/') {
        return Err(format!("changes.json 包含非法相对路径: [{}]", raw));
    }

    let mut result = PathBuf::new();
    for component in Path::new(&normalized_separators).components() {
        match component {
            Component::Normal(part) => result.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(format!("changes.json 路径越过更新根目录: [{}]", raw));
            }
        }
    }

    if result.as_os_str().is_empty() || is_preserved_path(&result) {
        return Err(format!("changes.json 包含保留路径: [{}]", raw));
    }
    Ok(result)
}

fn is_preserved_path(path: &Path) -> bool {
    let Some(Component::Normal(root)) = path.components().next() else {
        return true;
    };
    name_matches(root, TRANSACTION_DIR)
        || PRESERVED_ROOTS.iter().any(|name| name_matches(root, name))
}

fn name_matches(actual: &OsStr, expected: &str) -> bool {
    if cfg!(windows) {
        actual.to_string_lossy().eq_ignore_ascii_case(expected)
    } else {
        actual == OsStr::new(expected)
    }
}

fn path_key(path: &Path) -> String {
    let key = path.to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        key.to_ascii_lowercase()
    } else {
        key
    }
}

fn full_update_entries(staging: &Path) -> Result<Vec<PathBuf>, String> {
    let mut entries = Vec::new();
    let read_dir = fs::read_dir(staging)
        .map_err(|e| format!("无法读取更新暂存目录 [{}]: {}", staging.display(), e))?;

    for entry in read_dir {
        let entry = entry.map_err(|e| format!("无法读取更新暂存目录条目: {}", e))?;
        let name = entry.file_name();
        if name_matches(&name, "changes.json")
            || name_matches(&name, TRANSACTION_DIR)
            || PRESERVED_ROOTS.iter().any(|root| name_matches(&name, root))
        {
            continue;
        }
        entries.push(PathBuf::from(name));
    }

    entries.sort();
    Ok(entries)
}

fn create_transaction_root(target: &Path) -> Result<PathBuf, String> {
    cleanup_previous_committed_transactions(target);

    let id = format!(
        "{}-{}",
        chrono::Utc::now().format("%Y%m%d-%H%M%S-%f"),
        std::process::id()
    );
    let root = target.join(TRANSACTION_DIR).join(id);
    fs::create_dir_all(&root)
        .map_err(|e| format!("无法创建更新事务目录 [{}]: {}", root.display(), e))?;
    Ok(root)
}

fn cleanup_previous_committed_transactions(target: &Path) {
    let transaction_base = target.join(TRANSACTION_DIR);
    let Ok(entries) = fs::read_dir(&transaction_base) else {
        return;
    };

    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        let path = entry.path();
        if !path.join("COMMITTED").is_file() {
            continue;
        }

        match fs::remove_dir_all(&path) {
            Ok(()) => log::info!("Removed previous committed update: {}", path.display()),
            Err(error) => log::warn!(
                "Failed to remove previous committed update: path={}, error={}, raw_os_error={:?}",
                path.display(),
                error,
                error.raw_os_error()
            ),
        }
    }
}

fn copy_entries(staging: &Path, target: &Path, entries: &[PathBuf]) -> Result<(), String> {
    for relative in entries {
        copy_entry(&staging.join(relative), &target.join(relative))?;
    }
    Ok(())
}

fn copy_entry(source: &Path, destination: &Path) -> Result<(), String> {
    if source.is_dir() {
        fs::create_dir_all(destination)
            .map_err(|e| format!("无法创建更新目录 [{}]: {}", destination.display(), e))?;
        for entry in fs::read_dir(source)
            .map_err(|e| format!("无法读取更新目录 [{}]: {}", source.display(), e))?
        {
            let entry = entry.map_err(|e| format!("无法读取更新目录条目: {}", e))?;
            copy_entry(&entry.path(), &destination.join(entry.file_name()))?;
        }
        return Ok(());
    }

    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("无法创建更新父目录 [{}]: {}", parent.display(), e))?;
    }
    fs::copy(source, destination).map_err(|e| {
        format!(
            "无法复制更新文件 [{}] -> [{}]: {}, raw_os_error={:?}",
            source.display(),
            destination.display(),
            e,
            e.raw_os_error()
        )
    })?;
    Ok(())
}

fn remove_entries(target: &Path, entries: &[PathBuf]) -> Result<(), String> {
    let mut errors = Vec::new();
    for relative in entries.iter().rev() {
        let path = target.join(relative);
        if let Err(error) = remove_path_if_exists(&path) {
            errors.push(format!("[{}]: {}", path.display(), error));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

fn restore_backups(target: &Path, backup: &Path, backed_up: &[PathBuf]) -> Result<(), String> {
    let mut errors = Vec::new();
    for relative in backed_up.iter().rev() {
        let source = backup.join(relative);
        let destination = target.join(relative);
        if let Err(error) = rename_with_retry(&source, &destination) {
            errors.push(format!(
                "[{}] -> [{}]: {}, raw_os_error={:?}",
                source.display(),
                destination.display(),
                error,
                error.raw_os_error()
            ));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

fn remove_path_if_exists(path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }
    let result = if path.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
    result.map_err(|e| format!("{}, raw_os_error={:?}", e, e.raw_os_error()))
}

fn rename_with_retry(source: &Path, destination: &Path) -> std::io::Result<()> {
    const RETRY_DELAYS_MS: &[u64] = &[50, 100, 200, 400];

    for (attempt, delay_ms) in RETRY_DELAYS_MS.iter().enumerate() {
        match fs::rename(source, destination) {
            Ok(()) => return Ok(()),
            Err(error) if is_retryable_lock_error(&error) => {
                log::warn!(
                    "update rename locked, retrying: attempt={}, source={}, destination={}, error={}, raw_os_error={:?}",
                    attempt + 1,
                    source.display(),
                    destination.display(),
                    error,
                    error.raw_os_error()
                );
                std::thread::sleep(std::time::Duration::from_millis(*delay_ms));
            }
            Err(error) => return Err(error),
        }
    }

    fs::rename(source, destination)
}

fn is_retryable_lock_error(error: &std::io::Error) -> bool {
    cfg!(windows) && matches!(error.raw_os_error(), Some(32 | 33))
}

fn combine_errors(first: Option<String>, second: Option<String>) -> Option<String> {
    match (first, second) {
        (None, None) => None,
        (Some(error), None) | (None, Some(error)) => Some(error),
        (Some(first), Some(second)) => Some(format!("{}; {}", first, second)),
    }
}

fn format_transaction_error(primary: String, rollback: Option<String>) -> String {
    match rollback {
        Some(rollback) => format!("{}; 回滚也失败: {}", primary, rollback),
        None => primary,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    #[cfg(windows)]
    use std::fs::OpenOptions;
    #[cfg(windows)]
    use std::os::windows::fs::OpenOptionsExt;
    #[cfg(windows)]
    use std::thread;
    #[cfg(windows)]
    use std::time::Duration;

    use tempfile::tempdir;

    use super::{PreparedUpdate, TRANSACTION_DIR};

    fn install_prepared_update(
        staging: &std::path::Path,
        target: &std::path::Path,
    ) -> Result<(), String> {
        PreparedUpdate::prepare(staging, target)?.install()
    }

    #[test]
    fn full_update_replaces_managed_roots_and_preserves_mutable_data() {
        let temp = tempdir().unwrap();
        let target = temp.path().join("MaaNTE");
        let staging = temp.path().join("staging");

        fs::create_dir_all(target.join("python")).unwrap();
        fs::create_dir_all(target.join("config")).unwrap();
        fs::create_dir_all(target.join("cache")).unwrap();
        fs::write(target.join("python/runtime.pyd"), b"old-runtime").unwrap();
        fs::write(target.join("config/user.json"), b"user-config").unwrap();
        fs::write(target.join("cache/download.zip"), b"download").unwrap();

        fs::create_dir_all(staging.join("python")).unwrap();
        fs::create_dir_all(staging.join("config")).unwrap();
        fs::create_dir_all(staging.join("cache")).unwrap();
        fs::write(staging.join("python/runtime.pyd"), b"new-runtime").unwrap();
        fs::write(staging.join("config/default.json"), b"default-config").unwrap();
        fs::write(staging.join("cache/packaged.tmp"), b"packaged-cache").unwrap();

        install_prepared_update(&staging, &target).unwrap();

        assert_eq!(
            fs::read(target.join("python/runtime.pyd")).unwrap(),
            b"new-runtime"
        );
        assert_eq!(
            fs::read(target.join("config/user.json")).unwrap(),
            b"user-config"
        );
        assert_eq!(
            fs::read(target.join("cache/download.zip")).unwrap(),
            b"download"
        );
        assert!(!target.join("config/default.json").exists());
        assert!(!target.join("cache/packaged.tmp").exists());
    }

    #[test]
    fn incremental_update_applies_added_modified_and_deleted_paths() {
        let temp = tempdir().unwrap();
        let target = temp.path().join("MaaNTE");
        let staging = temp.path().join("staging");

        fs::create_dir_all(target.join("python")).unwrap();
        fs::create_dir_all(staging.join("python")).unwrap();
        fs::write(target.join("python/modified.pyd"), b"old-modified").unwrap();
        fs::write(target.join("python/deleted.pyd"), b"old-deleted").unwrap();
        fs::write(target.join("untouched.txt"), b"untouched").unwrap();
        fs::write(staging.join("python/modified.pyd"), b"new-modified").unwrap();
        fs::write(staging.join("python/added.pyd"), b"new-added").unwrap();
        fs::write(staging.join("unlisted.txt"), b"must-not-be-copied").unwrap();
        fs::write(
            staging.join("changes.json"),
            br#"{
                "added": ["python/added.pyd"],
                "modified": ["python/modified.pyd"],
                "deleted": ["python/deleted.pyd"]
            }"#,
        )
        .unwrap();

        install_prepared_update(&staging, &target).unwrap();

        assert_eq!(
            fs::read(target.join("python/modified.pyd")).unwrap(),
            b"new-modified"
        );
        assert_eq!(
            fs::read(target.join("python/added.pyd")).unwrap(),
            b"new-added"
        );
        assert!(!target.join("python/deleted.pyd").exists());
        assert_eq!(
            fs::read(target.join("untouched.txt")).unwrap(),
            b"untouched"
        );
        assert!(!target.join("unlisted.txt").exists());
    }

    #[cfg(windows)]
    #[test]
    fn backup_retries_a_transient_windows_file_lock() {
        let temp = tempdir().unwrap();
        let target = temp.path().join("MaaNTE");
        let staging = temp.path().join("staging");
        fs::create_dir_all(&target).unwrap();
        fs::create_dir_all(&staging).unwrap();
        fs::write(target.join("runtime.pyd"), b"old-runtime").unwrap();
        fs::write(staging.join("runtime.pyd"), b"new-runtime").unwrap();

        let locked_file = OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(target.join("runtime.pyd"))
            .unwrap();
        let unlock = thread::spawn(move || {
            thread::sleep(Duration::from_millis(150));
            drop(locked_file);
        });

        install_prepared_update(&staging, &target).unwrap();
        unlock.join().unwrap();

        assert_eq!(
            fs::read(target.join("runtime.pyd")).unwrap(),
            b"new-runtime"
        );
    }

    #[test]
    fn a_new_update_cleans_the_previous_committed_transaction_once() {
        let temp = tempdir().unwrap();
        let target = temp.path().join("MaaNTE");
        let staging = temp.path().join("staging");
        fs::create_dir_all(&target).unwrap();
        fs::create_dir_all(&staging).unwrap();
        fs::write(target.join("runtime.pyd"), b"v1").unwrap();
        fs::write(staging.join("runtime.pyd"), b"v2").unwrap();

        install_prepared_update(&staging, &target).unwrap();
        fs::write(staging.join("runtime.pyd"), b"v3").unwrap();
        install_prepared_update(&staging, &target).unwrap();

        let transactions = fs::read_dir(target.join(TRANSACTION_DIR))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(transactions.len(), 1);
        assert!(transactions[0].path().join("COMMITTED").exists());
    }

    #[cfg(windows)]
    #[test]
    fn apply_failure_restores_every_backed_up_root() {
        let temp = tempdir().unwrap();
        let target = temp.path().join("MaaNTE");
        let staging = temp.path().join("staging");
        fs::create_dir_all(&target).unwrap();
        fs::create_dir_all(&staging).unwrap();
        fs::write(target.join("a-runtime.bin"), b"old-a").unwrap();
        fs::write(target.join("b-runtime.bin"), b"old-b").unwrap();
        fs::write(staging.join("a-runtime.bin"), b"new-a").unwrap();
        fs::write(staging.join("b-runtime.bin"), b"new-b").unwrap();

        let locked_staging_file = OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(staging.join("b-runtime.bin"))
            .unwrap();

        let result = install_prepared_update(&staging, &target);
        drop(locked_staging_file);

        assert!(result.is_err());
        assert_eq!(fs::read(target.join("a-runtime.bin")).unwrap(), b"old-a");
        assert_eq!(fs::read(target.join("b-runtime.bin")).unwrap(), b"old-b");
    }

    #[test]
    fn incremental_added_conflict_aborts_before_creating_a_transaction() {
        let temp = tempdir().unwrap();
        let target = temp.path().join("MaaNTE");
        let staging = temp.path().join("staging");
        fs::create_dir_all(&target).unwrap();
        fs::create_dir_all(&staging).unwrap();
        fs::write(target.join("existing.bin"), b"user-data").unwrap();
        fs::write(staging.join("existing.bin"), b"package-data").unwrap();
        fs::write(
            staging.join("changes.json"),
            br#"{"added":["existing.bin"],"modified":[],"deleted":[]}"#,
        )
        .unwrap();

        let result = install_prepared_update(&staging, &target);

        assert!(result.is_err());
        assert_eq!(fs::read(target.join("existing.bin")).unwrap(), b"user-data");
        assert!(!target.join(TRANSACTION_DIR).exists());
    }

    #[test]
    fn incremental_path_traversal_is_rejected_before_target_writes() {
        let temp = tempdir().unwrap();
        let target = temp.path().join("MaaNTE");
        let staging = temp.path().join("staging");
        fs::create_dir_all(&target).unwrap();
        fs::create_dir_all(&staging).unwrap();
        fs::write(
            staging.join("changes.json"),
            br#"{"added":[],"modified":[],"deleted":["../outside.bin"]}"#,
        )
        .unwrap();

        let result = install_prepared_update(&staging, &target);

        assert!(result.is_err());
        assert!(!target.join(TRANSACTION_DIR).exists());
    }

    #[test]
    fn incremental_parent_and_child_paths_are_rejected_before_target_writes() {
        let temp = tempdir().unwrap();
        let target = temp.path().join("MaaNTE");
        let staging = temp.path().join("staging");
        fs::create_dir_all(target.join("python")).unwrap();
        fs::create_dir_all(staging.join("python")).unwrap();
        fs::write(target.join("python/runtime.pyd"), b"old-runtime").unwrap();
        fs::write(staging.join("python/runtime.pyd"), b"new-runtime").unwrap();
        fs::write(
            staging.join("changes.json"),
            br#"{
                "added": [],
                "modified": ["python"],
                "deleted": ["python/runtime.pyd"]
            }"#,
        )
        .unwrap();

        let result = install_prepared_update(&staging, &target);

        assert!(result.is_err());
        assert_eq!(
            fs::read(target.join("python/runtime.pyd")).unwrap(),
            b"old-runtime"
        );
        assert!(!target.join(TRANSACTION_DIR).exists());
    }

    #[cfg(windows)]
    #[test]
    fn incremental_reserved_paths_are_case_insensitive_on_windows() {
        let temp = tempdir().unwrap();
        let target = temp.path().join("MaaNTE");
        let staging = temp.path().join("staging");
        fs::create_dir_all(&target).unwrap();
        fs::create_dir_all(staging.join("CACHE")).unwrap();
        fs::write(staging.join("CACHE/package.tmp"), b"package-data").unwrap();
        fs::write(
            staging.join("changes.json"),
            br#"{"added":["CACHE/package.tmp"],"modified":[],"deleted":[]}"#,
        )
        .unwrap();

        let result = install_prepared_update(&staging, &target);

        assert!(result.is_err());
        assert!(!target.join(TRANSACTION_DIR).exists());
    }
}
