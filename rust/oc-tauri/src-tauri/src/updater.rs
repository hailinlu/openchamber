//! Auto-Update — 复现 Electron `electron-updater` 行为。
//!
//! 关键行为:
//! - 仅打包后运行 (dev 直接返回 no-update)
//! - 手动下载 (不自动下载)
//! - 404/ENOTFOUND → 视为 "无更新" 而非错误 (复现 Electron MISSING_UPDATE_FEED_RE)
//! - 下载进度 → emit `openchamber:update-progress` + Windows taskbar progress
//! - restart: pending update → app.restart()；否则 app.relaunch() + exit(0)
//! - on_before_exit: kill sidecar
//!
//! 复现 Electron:
//! - `setupAutoUpdater` (main.mjs:2829-2875)
//! - `checkForDesktopUpdate` (updater-check.mjs)
//! - `desktop_check_for_updates` (main.mjs:3908-3930)
//! - `desktop_download_and_install_update` (main.mjs:3932-3975)
//! - `desktop_restart` (main.mjs:3977-4022)

use std::sync::Mutex;

use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};
use tauri_plugin_updater::UpdaterExt;

/// 暂存的待安装更新 (版本号 + downloaded 标志)。
#[allow(dead_code)]
struct PendingUpdate {
    version: String,
    downloaded: bool,
}

/// 全局 pending update 状态 (简化: 用 static Mutex 而非 Tauri State)。
static PENDING_UPDATE: Mutex<Option<PendingUpdate>> = Mutex::new(None);

/// `desktop_check_for_updates` — 返回 `{ available, currentVersion, version?, body?, date? }`
///
/// 复现 Electron main.mjs:3908-3930。
pub async fn check_for_updates(_args: &Value, app: &AppHandle) -> Result<Value, String> {
    // dev 环境: 直接返回无更新 (复现 Electron isPackaged 判断)
    if tauri::is_dev() {
        return Ok(json!({
            "available": false,
            "currentVersion": app.package_info().version.to_string(),
        }));
    }
    let current_version = app.package_info().version.to_string();

    // 尝试通过 tauri-plugin-updater 检查
    match try_check_update(app).await {
        Ok(Some(update_info)) => {
            // 暂存 pending update
            if let Ok(mut guard) = PENDING_UPDATE.lock() {
                *guard = Some(PendingUpdate {
                    version: update_info.version.clone(),
                    downloaded: false,
                });
            }
            Ok(json!({
                "available": true,
                "currentVersion": current_version,
                "version": update_info.version,
                "body": update_info.body,
                "date": update_info.date,
            }))
        }
        Ok(None) => {
            // 无更新
            Ok(json!({
                "available": false,
                "currentVersion": current_version,
            }))
        }
        Err(e) => {
            // 404/网络错误 → 视为无更新 (复现 Electron MISSING_UPDATE_FEED_RE 容错)
            let err_str = e.to_string();
            if is_missing_feed_error(&err_str) {
                log::info!("[updater] no update feed available: {}", err_str);
                Ok(json!({
                    "available": false,
                    "currentVersion": current_version,
                }))
            } else {
                // 真实错误也返回给 UI (但不是 hard error)
                log::warn!("[updater] check failed: {}", err_str);
                Ok(json!({
                    "available": false,
                    "currentVersion": current_version,
                    "error": err_str,
                }))
            }
        }
    }
}

/// `desktop_download_and_install_update` — 下载并安装更新。
///
/// 复现 Electron main.mjs:3932-3975:
/// - emit `Started` progress event
/// - download → emit progress chunks + Windows taskbar progress
/// - emit `Finished` progress event
pub async fn download_and_install(_args: &Value, app: &AppHandle) -> Result<Value, String> {
    let updater = app
        .updater()
        .map_err(|e| format!("updater plugin not available: {}", e))?;

    let update = updater
        .check()
        .await
        .map_err(|e| e.to_string())?
        .ok_or("no update available")?;

    // emit Started
    let _ = app.emit(
        "openchamber:emit",
        json!({ "event": "openchamber:update-progress", "detail": { "event": "Started", "data": {} } }),
    );

    // Windows: 设置 taskbar progress
    #[cfg(target_os = "windows")]
    {
        if let Some(window) = app.get_webview_window("main") {
            let _ = window.set_progress_bar(0.01);
        }
    }

    // 下载并安装
    let app_handle = app.clone();
    let result: Result<(), tauri_plugin_updater::Error> = update
        .download_and_install(
            move |chunk_length: usize, total_length: Option<u64>| {
                // 进度回调
                let downloaded = chunk_length;
                let total = total_length.unwrap_or(0);
                let _ = app_handle.emit(
                    "openchamber:emit",
                    json!({
                        "event": "openchamber:update-progress",
                        "detail": {
                            "event": "Progress",
                            "data": { "downloaded": downloaded, "total": total }
                        }
                    }),
                );

                #[cfg(target_os = "windows")]
                {
                    if let Some(window) = app_handle.get_webview_window("main") {
                        if total > 0 {
                            let progress = (downloaded as f64 / total as f64).clamp(0.0, 1.0);
                            let _ = window.set_progress_bar(progress);
                        }
                    }
                }
            },
            || {
                // on_before_exit: kill sidecar (与 Electron killSidecar 一致)
                log::info!("[updater] killing sidecar before install");
                crate::shutdown_backend_public();
            },
        )
        .await;

    // 清除 taskbar progress
    #[cfg(target_os = "windows")]
    {
        if let Some(window) = app.get_webview_window("main") {
            let _ = window.set_progress_bar(-1.0);
        }
    }

    match result {
        Ok(()) => {
            // 标记为已下载
            if let Ok(mut guard) = PENDING_UPDATE.lock() {
                if let Some(ref mut pending) = *guard {
                    pending.downloaded = true;
                }
            }

            let _ = app.emit(
                "openchamber:emit",
                json!({ "event": "openchamber:update-progress", "detail": { "event": "Finished", "data": {} } }),
            );

            Ok(json!({ "ok": true }))
        }
        Err(e) => {
            let err_str = e.to_string();
            #[cfg(target_os = "windows")]
            {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.set_progress_bar(-1.0);
                }
            }
            let _ = app.emit(
                "openchamber:emit",
                json!({ "event": "openchamber:update-progress", "detail": { "event": "Error", "data": { "error": err_str } } }),
            );
            Err(err_str)
        }
    }
}

/// `desktop_restart` — 重启应用。
///
/// 复现 Electron main.mjs:3977-4022:
/// - pending update downloaded → app.restart() (应用更新)
/// - 否则 → app.relaunch() + exit(0) (普通重启)
pub async fn restart(_args: &Value, app: &AppHandle) -> Result<Value, String> {
    let has_downloaded_update = PENDING_UPDATE
        .lock()
        .map(|guard| {
            guard
                .as_ref()
                .map(|p| p.downloaded)
                .unwrap_or(false)
        })
        .unwrap_or(false);

    if has_downloaded_update {
        // 应用更新: restart 会安装并重启
        // kill sidecar (on_before_exit 可能已经做了,但做两次无害)
        crate::shutdown_backend_public();
        app.restart();
    } else {
        // 普通重启: Tauri 的 restart() 会清理并重新拉起进程
        crate::shutdown_backend_public();
        app.restart();
    }
}

// --- 内部辅助 ---

struct UpdateInfo {
    version: String,
    body: Option<String>,
    date: Option<String>,
}

/// 通过 tauri-plugin-updater 检查更新。返回 None = 无更新。
async fn try_check_update(app: &AppHandle) -> Result<Option<UpdateInfo>, Box<dyn std::error::Error>> {
    let updater = app.updater()?;
    match updater.check().await? {
        Some(update) => {
            let version = update.version.clone();
            let body = update.body.clone();
            let date = update.date.map(|d| d.to_string());
            // 注意: 不在这里 download，仅返回信息
            // update 对象需要保留以供后续 download_and_install 使用
            // 但由于 tauri-plugin-updater 的 Update 不是 Clone/Send，
            // 我们在 download_and_install 中重新 check
            // (这是 OK 的: check 是幂等的)
            // 暂存 body 到全局
            *UPDATE_BODY.lock().unwrap() = body.clone();
            Ok(Some(UpdateInfo { version, body, date }))
        }
        None => Ok(None),
    }
}

/// 暂存最近一次 check 的 release body (供 download_and_install 使用)。
static UPDATE_BODY: Mutex<Option<String>> = Mutex::new(None);

/// 判断错误是否为 "更新源不存在" (404/ENOTFOUND 等)。
///
/// 复现 Electron MISSING_UPDATE_FEED_RE (updater-check.mjs:16-22):
/// 这些错误应被视为 "无更新" 而非 hard error。
fn is_missing_feed_error(err: &str) -> bool {
    let lower = err.to_lowercase();
    lower.contains("404")
        || lower.contains("not found")
        || lower.contains("enotfound")
        || lower.contains("cannot find")
        || lower.contains("latest")
        || lower.contains("no such host")
        || lower.contains("failed to get")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_feed_error_detection() {
        assert!(is_missing_feed_error("HTTP 404 Not Found"));
        assert!(is_missing_feed_error("getaddrinfo ENOTFOUND github.com"));
        assert!(is_missing_feed_error("Cannot find channel/latest"));
        assert!(!is_missing_feed_error("network timeout"));
        assert!(!is_missing_feed_error("invalid signature"));
    }
}
