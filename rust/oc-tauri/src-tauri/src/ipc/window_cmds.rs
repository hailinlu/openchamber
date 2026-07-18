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

/// `desktop_show_app_menu` — macOS 原生应用菜单弹出。
///
/// Tauri 2 在 macOS 上使用原生菜单栏，不需要手动弹出菜单。
/// 此命令保留作为兼容桩，在当前窗口上下文返回 OK。
pub async fn show_app_menu(_args: &Value, _window: &WebviewWindow) -> Result<Value, String> {
    // macOS: 菜单栏已由系统渲染。无需额外操作。
    // Win/Linux: 无框架窗口的应用菜单由 UI 侧控制。
    Ok(json!({}))
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

/// `desktop_new_window_at_url` — args: `{ url, clientToken?, requestHeaders? }`
///
/// 创建新窗口加载指定 URL。复现 Electron main.mjs `createNewWindow`。
/// 新窗口不注入 init_script (远程实例, 加载自己的桥)。
pub async fn new_window_at_url(args: &Value, app: &AppHandle) -> Result<Value, String> {
    let url = args
        .get("url")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "url is required".to_string())?;

    let label = format!("remote-{}", std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0));

    let parsed_url = url::Url::parse(url).map_err(|e| format!("invalid URL: {}", e))?;

    let _window = tauri::WebviewWindowBuilder::new(app, &label, tauri::WebviewUrl::External(parsed_url))
        .title("GridForge")
        .inner_size(1200.0, 800.0)
        .resizable(true)
        .visible(true)
        .build()
        .map_err(|e| e.to_string())?;

    Ok(json!({ "label": label }))
}

/// `desktop_new_window_for_host` — args: `{ hostId }`
///
/// 从 `desktopHosts` 查找 host, 用其 URL 创建新窗口。
/// 复现 Electron `openNewWindowForHost` (main.mjs)。
pub async fn new_window_for_host(args: &Value, app: &AppHandle) -> Result<Value, String> {
    let host_id = args
        .get("hostId")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "hostId is required".to_string())?;

    // 读 hosts 列表
    let root = crate::settings::SettingsStore::read();
    let hosts = root.get("desktopHosts").and_then(|v| v.as_array()).cloned().unwrap_or_default();

    // 查找匹配的 host
    let host = hosts.iter().find(|h| {
        h.get("id").and_then(|v| v.as_str()) == Some(host_id)
            || h.get("apiUrl").and_then(|v| v.as_str()).map(|u| u.contains(host_id)).unwrap_or(false)
    }).cloned();

    let host_url = match host {
        Some(ref h) => {
            h.get("apiUrl")
                .or_else(|| h.get("url"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        }
        None => return Err(format!("Host '{}' not found", host_id)),
    };

    if host_url.is_empty() {
        return Err("Host has no URL".to_string());
    }

    let parsed_url = url::Url::parse(&host_url).map_err(|e| format!("invalid URL: {}", e))?;

    let label = format!("host-{}-{}", host_id, std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0));

    let _window = tauri::WebviewWindowBuilder::new(app, &label, tauri::WebviewUrl::External(parsed_url))
        .title("GridForge")
        .inner_size(1200.0, 800.0)
        .resizable(true)
        .visible(true)
        .build()
        .map_err(|e| e.to_string())?;

    Ok(json!({ "label": label }))
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
