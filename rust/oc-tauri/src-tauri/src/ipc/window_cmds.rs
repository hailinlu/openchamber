//! 窗口 chrome 命令 — 复现 Electron main.mjs handleInvoke 的窗口操作分支。
//!
//! 每个函数签名: `(args, window) -> Result<Value, String>`，
//! 由 `ipc/mod.rs::dispatch` 的 match 分支调用。

use serde_json::{json, Value};
use tauri::{AppHandle, WebviewWindow};

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
///
/// 共用 `crate::tray::restore_main_window` 的 unminimize → show → set_focus 顺序,
/// 与托盘左键点击 / `tray_show` 行为完全一致, 防止最小化窗口先 `show()` 后
/// 再 `unminimize()` 的二次恢复抖动。
///
/// 响应形状固定为 `{ "focused": bool }`, 与 Electron `main.mjs:4136,4147`
/// 始终返回 `{ focused: true }` 保持一致; 主窗口缺失时返回 `{ focused: false }`
/// 而不是 `null`, 以便消费方 (`MiniChatLayout.tsx:244-250`) 用
/// `result?.focused === true` 单值判断 `desktop_close_current_window` 的触发条件。
fn focus_main_window_response(focused: bool) -> Value {
    json!({ "focused": focused })
}

pub async fn focus_main_window(_args: &Value, app: &AppHandle) -> Result<Value, String> {
    let focused = crate::tray::restore_main_window(app);
    Ok(focus_main_window_response(focused))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focus_main_window_response_reports_success() {
        // Happy path: 主窗口存在 → `{ focused: true }`, 与 Electron main.mjs:4136,4147
        // 始终返回的形状一致。 MiniChatLayout.tsx:246 的 `result?.focused === true`
        // 触发 `desktop_close_current_window`。
        assert_eq!(focus_main_window_response(true), json!({ "focused": true }));
    }

    #[test]
    fn focus_main_window_response_reports_missing_window() {
        // 主窗口缺失 → `{ focused: false }` (而不是 `Value::Null` 或空对象),
        // 保证消费方的 `result?.focused === true` 单值判断可靠。
        assert_eq!(focus_main_window_response(false), json!({ "focused": false }));
    }

    #[test]
    fn focus_main_window_response_never_returns_null_or_empty_object() {
        // 防回归: 响应形状必须始终是 `{ focused: bool }`, 任何分支都不能
        // 退化为 `null` / `{}` (会破坏 MiniChatLayout 的 gated close 逻辑)。
        for focused in [true, false] {
            let response = focus_main_window_response(focused);
            assert!(
                response.is_object(),
                "response must be a JSON object, got {:?}",
                response
            );
            let obj = response.as_object().expect("object");
            assert!(
                obj.contains_key("focused"),
                "response object must have `focused` key, got {:?}",
                obj
            );
            assert_eq!(obj.len(), 1, "response object must have exactly one key");
            assert_eq!(
                obj["focused"].as_bool(),
                Some(focused),
                "focused value must match input bool"
            );
        }
    }
}
