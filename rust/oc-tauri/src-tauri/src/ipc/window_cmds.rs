//! 窗口 chrome 命令 — 复现 Electron main.mjs handleInvoke 的窗口操作分支。
//!
//! 每个函数签名: `(args, window) -> Result<Value, String>`，
//! 由 `ipc/mod.rs::dispatch` 的 match 分支调用。

use serde_json::{json, Value};
use tauri::{AppHandle, Manager, WebviewWindow};

use crate::settings::SettingsStore;

/// `desktop_start_window_drag` — 启动无边框窗口拖拽。
pub async fn start_window_drag(_args: &Value, window: &WebviewWindow) -> Result<Value, String> {
    window.start_dragging().map_err(|e| e.to_string())?;
    Ok(Value::Null)
}

/// `desktop_is_window_fullscreen`
pub async fn is_window_fullscreen(_args: &Value, window: &WebviewWindow) -> Result<Value, String> {
    Ok(json!(window.is_fullscreen().unwrap_or(false)))
}

/// `desktop_set_window_title` — args: `{ title: string }`
pub async fn set_window_title(args: &Value, window: &WebviewWindow) -> Result<Value, String> {
    if let Some(title) = args.get("title").and_then(|v| v.as_str()) {
        window.set_title(title).map_err(|e| e.to_string())?;
    }
    Ok(Value::Null)
}

/// `desktop_get_current_window_state` — → `{ maximized: bool }`
pub async fn get_current_window_state(
    _args: &Value,
    window: &WebviewWindow,
) -> Result<Value, String> {
    let maximized = window.is_maximized().unwrap_or(false);
    Ok(json!({ "maximized": maximized }))
}

/// `desktop_minimize_current_window`
pub async fn minimize_current_window(
    _args: &Value,
    window: &WebviewWindow,
) -> Result<Value, String> {
    window.minimize().map_err(|e| e.to_string())?;
    Ok(Value::Null)
}

/// `desktop_toggle_current_window_maximized` — → `{ maximized: bool }`
pub async fn toggle_current_window_maximized(
    _args: &Value,
    window: &WebviewWindow,
) -> Result<Value, String> {
    let is_max = window.is_maximized().unwrap_or(false);
    if is_max {
        window.unmaximize().map_err(|e| e.to_string())?;
    } else {
        window.maximize().map_err(|e| e.to_string())?;
    }
    Ok(json!({ "maximized": !is_max }))
}

/// `desktop_close_current_window`
pub async fn close_current_window(
    _args: &Value,
    window: &WebviewWindow,
) -> Result<Value, String> {
    window.close().map_err(|e| e.to_string())?;
    Ok(Value::Null)
}

/// `desktop_set_window_theme` — args: `{ themeMode, themeVariant }`
///
/// Tauri 没有等价 Electron nativeTheme 的系统级主题切换;
/// 暂时仅设置窗口主题 (影响 title bar 颜色), 完整 nativeTheme 等价后移。
pub async fn set_window_theme(args: &Value, window: &WebviewWindow) -> Result<Value, String> {
    let theme_mode = args
        .get("themeMode")
        .and_then(|v| v.as_str())
        .unwrap_or("system");
    let theme = match theme_mode {
        "light" => Some(tauri::Theme::Light),
        "dark" => Some(tauri::Theme::Dark),
        _ => None, // system — 不强制设置
    };
    if let Some(t) = theme {
        window.set_theme(Some(t)).map_err(|e| e.to_string())?;
    } else {
        window.set_theme(None).map_err(|e| e.to_string())?;
    }
    Ok(Value::Null)
}

/// `desktop_set_vibrancy` — args: `{ enabled: boolean }`
///
/// 复现 Electron main.mjs:3888-3906:
/// 持久化 `desktopVibrancy` 到 settings.json，返回 `{ enabled, requiresRestart: true }`。
/// vibrancy 是窗口创建时决定的材质，切换需重启 (与 Electron 一致)。
pub async fn set_vibrancy(args: &Value, _app: &AppHandle) -> Result<Value, String> {
    #[cfg(target_os = "macos")]
    {
        let enabled = args.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
        let store = SettingsStore::new();
        store.set("desktopVibrancy", json!(enabled))?;
        Ok(json!({ "enabled": enabled, "requiresRestart": true }))
    }
    #[cfg(not(target_os = "macos"))]
    {
        // 非 macOS: 不支持 vibrancy
        Ok(json!({ "enabled": false, "requiresRestart": false }))
    }
}

/// `desktop_focus_main_window` — 聚焦 (show + unminimize + focus) 主窗口。
pub async fn focus_main_window(_args: &Value, app: &AppHandle) -> Result<Value, String> {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
    Ok(Value::Null)
}
