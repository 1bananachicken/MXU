//! 更新安装相关命令
//!
//! 提供解压、增量/全量更新、文件移动等功能

use std::path::Path;
use std::sync::Arc;

use log::{error, info, warn};
use tauri::State;

use super::file_ops::get_exe_dir;
use super::maa_agent::quiesce_all_for_update;
use super::types::ChangesJson;
use super::types::MaaState;
use super::update_transaction::PreparedUpdate;

/// 校验更新计划、停止全部 Maa 运行时并以单个文件事务安装更新。
#[tauri::command]
pub async fn install_prepared_update(
    state: State<'_, Arc<MaaState>>,
    extract_dir: String,
    target_dir: String,
) -> Result<(), String> {
    info!(
        "install_prepared_update called: extract_dir={}, target_dir={}",
        extract_dir, target_dir
    );

    let prepared = tauri::async_runtime::spawn_blocking(move || {
        PreparedUpdate::prepare(Path::new(&extract_dir), Path::new(&target_dir))
    })
    .await
    .map_err(|e| e.to_string())??;

    let mut update_permit = state.update_coordinator.clone().begin_update().await?;
    let maa_state = state.inner().clone();

    let result = tauri::async_runtime::spawn_blocking(move || {
        quiesce_all_for_update(&maa_state)?;
        prepared.install()
    })
    .await
    .map_err(|e| e.to_string())?;

    match result {
        Ok(()) => {
            update_permit.keep_runtime_closed();
            info!("install_prepared_update success; runtime remains closed until restart");
            Ok(())
        }
        Err(error) => {
            error!("install_prepared_update failed: {}", error);
            Err(error)
        }
    }
}

/// 解压压缩文件到指定目录，支持 zip 和 tar.gz/tgz 格式
#[tauri::command]
pub fn extract_zip(zip_path: String, dest_dir: String) -> Result<(), String> {
    info!("extract_zip called: {} -> {}", zip_path, dest_dir);

    let path_lower = zip_path.to_lowercase();

    // 根据文件扩展名判断格式
    if path_lower.ends_with(".tar.gz") || path_lower.ends_with(".tgz") {
        extract_tar_gz(&zip_path, &dest_dir)
    } else {
        extract_zip_file(&zip_path, &dest_dir)
    }
}

/// 解压 ZIP 文件
fn extract_zip_file(zip_path: &str, dest_dir: &str) -> Result<(), String> {
    let file = std::fs::File::open(zip_path)
        .map_err(|e| format!("无法打开 ZIP 文件 [{}]: {}", zip_path, e))?;

    let mut archive =
        zip::ZipArchive::new(file).map_err(|e| format!("无法解析 ZIP 文件: {}", e))?;

    // 确保目标目录存在
    std::fs::create_dir_all(dest_dir).map_err(|e| format!("无法创建目录 [{}]: {}", dest_dir, e))?;

    for i in 0..archive.len() {
        let mut file = archive
            .by_index(i)
            .map_err(|e| format!("无法读取 ZIP 条目 {}: {}", i, e))?;

        let outpath = match file.enclosed_name() {
            Some(path) => std::path::Path::new(dest_dir).join(path),
            None => continue,
        };

        if file.name().ends_with('/') {
            // 目录
            std::fs::create_dir_all(&outpath)
                .map_err(|e| format!("无法创建目录 [{}]: {}", outpath.display(), e))?;
        } else {
            // 文件
            if let Some(p) = outpath.parent() {
                if !p.exists() {
                    std::fs::create_dir_all(p)
                        .map_err(|e| format!("无法创建父目录 [{}]: {}", p.display(), e))?;
                }
            }
            let mut outfile = std::fs::File::create(&outpath)
                .map_err(|e| format!("无法创建文件 [{}]: {}", outpath.display(), e))?;
            std::io::copy(&mut file, &mut outfile)
                .map_err(|e| format!("无法写入文件 [{}]: {}", outpath.display(), e))?;
        }
    }

    info!("extract_zip success");
    Ok(())
}

/// 解压 tar.gz/tgz 文件
fn extract_tar_gz(tar_path: &str, dest_dir: &str) -> Result<(), String> {
    use flate2::read::GzDecoder;
    use tar::Archive;

    let file = std::fs::File::open(tar_path)
        .map_err(|e| format!("无法打开 tar.gz 文件 [{}]: {}", tar_path, e))?;

    let gz = GzDecoder::new(file);
    let mut archive = Archive::new(gz);

    // 确保目标目录存在
    std::fs::create_dir_all(dest_dir).map_err(|e| format!("无法创建目录 [{}]: {}", dest_dir, e))?;

    archive
        .unpack(dest_dir)
        .map_err(|e| format!("解压 tar.gz 失败: {}", e))?;

    info!("extract_tar_gz success");
    Ok(())
}

/// 检查解压目录中是否存在 changes.json（增量包标识）
#[tauri::command]
pub fn check_changes_json(extract_dir: String) -> Result<Option<ChangesJson>, String> {
    let changes_path = std::path::Path::new(&extract_dir).join("changes.json");

    if !changes_path.exists() {
        return Ok(None);
    }

    let content = std::fs::read_to_string(&changes_path)
        .map_err(|e| format!("无法读取 changes.json: {}", e))?;

    let changes: ChangesJson =
        serde_json::from_str(&content).map_err(|e| format!("无法解析 changes.json: {}", e))?;

    Ok(Some(changes))
}

/// 递归清理目录内容，逐个删除文件和空目录，返回 (成功数, 失败数)
pub fn cleanup_dir_contents(dir: &std::path::Path) -> (usize, usize) {
    let mut deleted = 0;
    let mut failed = 0;

    match std::fs::read_dir(dir) {
        Ok(entries) => {
            for entry in entries {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) => {
                        log_cleanup_error("read_dir_entry", dir, &error);
                        failed += 1;
                        continue;
                    }
                };
                let path = entry.path();
                if path.is_dir() {
                    // 递归清理子目录
                    let (d, f) = cleanup_dir_contents(&path);
                    deleted += d;
                    failed += f;
                    // 尝试删除空目录
                    match std::fs::remove_dir(&path) {
                        Ok(()) => deleted += 1,
                        Err(error) if path.exists() => {
                            log_cleanup_error("remove_dir", &path, &error);
                            failed += 1;
                        }
                        Err(_) => {}
                    }
                } else {
                    // 删除文件
                    match std::fs::remove_file(&path) {
                        Ok(()) => deleted += 1,
                        Err(error) => {
                            log_cleanup_error("remove_file", &path, &error);
                            failed += 1;
                        }
                    }
                }
            }
        }
        Err(error) => {
            log_cleanup_error("read_dir", dir, &error);
            failed += 1;
        }
    }

    // 尝试删除根目录本身
    if let Err(error) = std::fs::remove_dir(dir) {
        if dir.exists() {
            log_cleanup_error("remove_dir", dir, &error);
            failed += 1;
        }
    }

    (deleted, failed)
}

fn log_cleanup_error(operation: &str, path: &std::path::Path, error: &std::io::Error) {
    warn!(
        "update cleanup failed: op={} path={} error={} raw_os_error={:?}",
        operation,
        path.display(),
        error,
        error.raw_os_error()
    );
}

/// 将文件或目录移动到程序目录下的 cache/old 文件夹，处理重名冲突
/// 供前端调用，统一文件移动逻辑
#[tauri::command]
pub fn move_file_to_old(file_path: String) -> Result<(), String> {
    let path = std::path::Path::new(&file_path);
    move_to_old_folder(path)
}

/// 将文件或目录移动到程序目录下的 cache/old 文件夹，处理重名冲突（内部函数）
pub fn move_to_old_folder(source: &std::path::Path) -> Result<(), String> {
    if !source.exists() {
        return Ok(());
    }

    // 统一移动到 exe_dir/cache/old
    let exe_dir = get_exe_dir()?;
    let old_dir = std::path::Path::new(&exe_dir).join("cache").join("old");

    // 在移动前先尝试清理 old 目录，避免同名文件冲突
    if old_dir.exists() {
        // 1. 尝试删除整个目录
        if std::fs::remove_dir_all(&old_dir).is_err() {
            // 2. 如果失败，遍历删除里面每个文件/子目录
            let (deleted, failed) = cleanup_dir_contents(&old_dir);
            if deleted > 0 || failed > 0 {
                info!(
                    "Cleanup cache/old before move: {} deleted, {} failed",
                    deleted, failed
                );
            }
        }
    }

    // 确保目录存在（刚删掉的话需要重新创建）
    std::fs::create_dir_all(&old_dir)
        .map_err(|e| format!("无法创建 old 目录 [{}]: {}", old_dir.display(), e))?;

    let file_name = source
        .file_name()
        .ok_or_else(|| format!("无法获取文件名: {}", source.display()))?;

    let mut dest = old_dir.join(file_name);

    // 如果目标仍然存在（清理没删掉），添加 .bak001 等后缀
    if dest.exists() {
        let base_name = file_name.to_string_lossy();
        for i in 1..=999 {
            let new_name = format!("{}.bak{:03}", base_name, i);
            dest = old_dir.join(&new_name);
            if !dest.exists() {
                break;
            }
        }
        // 如果 999 个备份都存在，覆盖最后的
    }

    // 执行移动（重命名）
    std::fs::rename(source, &dest).map_err(|e| {
        format!(
            "无法移动 [{}] -> [{}]: {}",
            source.display(),
            dest.display(),
            e
        )
    })?;

    info!("Moved to old: {} -> {}", source.display(), dest.display());
    Ok(())
}

/// 复制单个文件。
fn copy_file(src: &std::path::Path, dst: &std::path::Path) -> Result<(), String> {
    std::fs::copy(src, dst).map_err(|e| {
        format!(
            "无法复制文件 [{}] -> [{}]: {}",
            src.display(),
            dst.display(),
            e
        )
    })?;

    Ok(())
}

/// 递归复制目录内容（不包含根目录本身）
fn copy_dir_contents(src: &str, dst: &str, skip_files: Option<&[&str]>) -> Result<(), String> {
    let src_path = std::path::Path::new(src);
    let dst_path = std::path::Path::new(dst);

    // 确保目标目录存在
    std::fs::create_dir_all(dst_path).map_err(|e| format!("无法创建目录 [{}]: {}", dst, e))?;

    for entry in
        std::fs::read_dir(src_path).map_err(|e| format!("无法读取目录 [{}]: {}", src, e))?
    {
        let entry = entry.map_err(|e| format!("无法读取目录条目: {}", e))?;
        let file_name = entry.file_name();
        let file_name_str = file_name.to_string_lossy();

        // 检查是否需要跳过
        if let Some(skip) = skip_files {
            if skip.iter().any(|s| *s == file_name_str) {
                continue;
            }
        }

        let src_item = entry.path();
        let dst_item = dst_path.join(&file_name);

        if src_item.is_dir() {
            copy_dir_recursive(&src_item, &dst_item)?;
        } else {
            copy_file(&src_item, &dst_item)?;
        }
    }

    Ok(())
}

/// 递归复制整个目录
fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> Result<(), String> {
    std::fs::create_dir_all(dst).map_err(|e| format!("无法创建目录 [{}]: {}", dst.display(), e))?;

    for entry in
        std::fs::read_dir(src).map_err(|e| format!("无法读取目录 [{}]: {}", src.display(), e))?
    {
        let entry = entry.map_err(|e| format!("无法读取目录条目: {}", e))?;
        let src_item = entry.path();
        let dst_item = dst.join(entry.file_name());

        if src_item.is_dir() {
            copy_dir_recursive(&src_item, &dst_item)?;
        } else {
            copy_file(&src_item, &dst_item)?;
        }
    }

    Ok(())
}

/// 更新完成后清理残留产物：
/// 1. 删除 target_dir/changes.json（增量包标识，更新后无需保留）
/// 2. 删除 cache_dir 下所有 *.downloading 临时文件
#[tauri::command]
pub fn cleanup_update_artifacts(target_dir: String, cache_dir: String) -> Result<(), String> {
    // 删除 target_dir/changes.json
    let changes_path = std::path::Path::new(&target_dir).join("changes.json");
    if changes_path.exists() {
        match std::fs::remove_file(&changes_path) {
            Ok(()) => info!("已删除 changes.json: {}", changes_path.display()),
            Err(e) => warn!("删除 changes.json 失败（忽略）: {}", e),
        }
    }

    // 删除 cache_dir 下所有 *.downloading 文件
    let cache_path = std::path::Path::new(&cache_dir);
    if cache_path.exists() {
        if let Ok(entries) = std::fs::read_dir(cache_path) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file() {
                    let name = path.file_name().unwrap_or_default().to_string_lossy();
                    if name.ends_with(".downloading") {
                        match std::fs::remove_file(&path) {
                            Ok(()) => info!("已删除临时下载文件: {}", path.display()),
                            Err(e) => warn!("删除临时下载文件失败（忽略）: {}", e),
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

/// 清理临时解压目录
#[tauri::command]
pub fn cleanup_extract_dir(extract_dir: String) -> Result<(), String> {
    info!("cleanup_extract_dir: {}", extract_dir);

    let path = std::path::Path::new(&extract_dir);
    if path.exists() {
        std::fs::remove_dir_all(path)
            .map_err(|e| format!("无法清理目录 [{}]: {}", extract_dir, e))?;
    }

    Ok(())
}

/// 兜底更新：当正常更新失败时，将新文件解压到 v版本号 文件夹
/// 并复制 config 文件夹，让用户可以临时使用新版本
#[tauri::command]
pub fn fallback_update(
    extract_dir: String,
    target_dir: String,
    new_version: String,
) -> Result<String, String> {
    info!(
        "fallback_update called: extract_dir={}, target_dir={}, new_version={}",
        extract_dir, target_dir, new_version
    );

    let target_path = std::path::Path::new(&target_dir);

    // 创建 v版本号 文件夹（如 v1.2.3）
    let version_folder_name = format!("v{}", new_version.trim_start_matches('v'));
    let fallback_dir = target_path.join(&version_folder_name);

    // 如果已存在同名文件夹，加后缀
    let mut final_fallback_dir = fallback_dir.clone();
    let mut suffix = 0;
    while final_fallback_dir.exists() {
        suffix += 1;
        final_fallback_dir = target_path.join(format!("{}-{}", version_folder_name, suffix));
    }

    info!("创建兜底目录: {}", final_fallback_dir.display());

    // 创建兜底目录
    std::fs::create_dir_all(&final_fallback_dir).map_err(|e| format!("无法创建兜底目录: {}", e))?;

    // 复制解压的新文件到兜底目录
    copy_dir_contents(
        &extract_dir,
        final_fallback_dir.to_str().unwrap_or(""),
        Some(&["changes.json"]),
    )?;

    // 复制 config 文件夹（如果存在）
    let config_src = target_path.join("config");
    if config_src.exists() {
        let config_dst = final_fallback_dir.join("config");
        if let Err(e) = copy_dir_recursive(&config_src, &config_dst) {
            warn!("复制 config 文件夹失败: {}", e);
        } else {
            info!("已复制 config 文件夹到兜底目录");
        }
    }

    let result_path = final_fallback_dir.to_str().unwrap_or("").to_string();
    info!("fallback_update success: {}", result_path);

    Ok(result_path)
}
