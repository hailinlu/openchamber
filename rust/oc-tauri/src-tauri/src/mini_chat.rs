//! Mini-Chat 窗口管理 — 复现 Electron main.mjs `createMiniChatWindow`。
//!
//! Mini-chat 是一个小型独立窗口 (520×760)，加载同一个 web app 的 `mini-chat.html`。
//! - `mode: 'session'` — 绑定特定 sessionId，按 (apiUrl, sessionId) 去重
//! - `mode: 'draft'` — 不去重，每次调用创建新窗口
//!
//! Pinning: `desktop_set_window_pinned` → always-on-top + visible-on-all-workspaces。
//!
//! 复现 Electron:
//! - `createMiniChatWindow` (main.mjs:2589-2715)
//! - `setMiniChatPinned` (main.mjs:2717-2735)
//! - IPC: `desktop_open_session_mini_chat_window` / `desktop_open_draft_mini_chat_window`

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::{json, Value};
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

use crate::ipc::globals::{build_init_script, RuntimeContext};

/// Mini-chat 窗口去重管理器。作为 Tauri State。
pub struct MiniChatManager {
    /// key = (api_base_url, session_id), value = window label。
    /// 仅 session 模式去重；draft 模式不登记。
    windows: Mutex<HashMap<(String, String), String>>,
}

impl MiniChatManager {
    pub fn new() -> Self {
        Self {
            windows: Mutex::new(HashMap::new()),
        }
    }

    /// 查找已存在的 session 窗口。
    fn find_session_window(&self, api_base_url: &str, session_id: &str) -> Option<String> {
        let guard = self.windows.lock().ok()?;
        guard
            .get(&(api_base_url.to_string(), session_id.to_string()))
            .cloned()
    }

    /// 登记窗口 label。
    fn register(&self, api_base_url: &str, session_id: &str, label: &str) {
        if let Ok(mut guard) = self.windows.lock() {
            guard.insert(
                (api_base_url.to_string(), session_id.to_string()),
                label.to_string(),
            );
        }
    }

    /// 移除窗口 label (窗口关闭时)。
    fn unregister(&self, label: &str) {
        if let Ok(mut guard) = self.windows.lock() {
            guard.retain(|_, v| v != label);
        }
    }
}

impl Default for MiniChatManager {
    fn default() -> Self {
        Self::new()
    }
}

const MINI_CHAT_WIDTH: f64 = 520.0;
const MINI_CHAT_HEIGHT: f64 = 760.0;
const MINI_CHAT_MIN_WIDTH: f64 = 360.0;
const MINI_CHAT_MIN_HEIGHT: f64 = 480.0;

/// `desktop_open_session_mini_chat_window` — args: `{ sessionId, directory, projectId?, ...runtimeConfig }`
///
/// Session 模式: 按 (apiUrl, sessionId) 去重。已存在则 show+focus。
pub async fn open_session_mini_chat(args: &Value, app: &AppHandle) -> Result<Value, String> {
    let session_id = args
        .get("sessionId")
        .and_then(|v| v.as_str())
        .ok_or("sessionId is required")?
        .to_string();
    let directory = args
        .get("directory")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let project_id = args
        .get("projectId")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    // 获取 origin (sidecar port → loopback URL)
    let origin = resolve_origin(app)?;

    // 去重检查
    let manager: tauri::State<MiniChatManager> = app.state();
    if let Some(existing_label) = manager.find_session_window(&origin, &session_id) {
        if let Some(window) = app.get_webview_window(&existing_label) {
            let _ = window.show();
            let _ = window.unminimize();
            let _ = window.set_focus();
            return Ok(json!({ "label": existing_label }));
        }
        // 窗口不存在了 (可能已关闭)，清理
        manager.unregister(&existing_label);
    }

    let label = format!("mini-chat-session-{}", session_id);
    let url = format!(
        "{}/mini-chat.html?mode=session&sessionId={}&directory={}&projectId={}",
        origin,
        url_encode(&session_id),
        url_encode(&directory),
        url_encode(project_id),
    );

    create_mini_chat_window(app, &label, &url)?;

    // 登记到去重 map
    manager.register(&origin, &session_id, &label);

    Ok(json!({ "label": label }))
}

/// `desktop_open_draft_mini_chat_window` — args: `{ directory, projectId?, ...runtimeConfig }`
///
/// Draft 模式: 不去重，每次创建新窗口。
pub async fn open_draft_mini_chat(args: &Value, app: &AppHandle) -> Result<Value, String> {
    let directory = args
        .get("directory")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let project_id = args
        .get("projectId")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let origin = resolve_origin(app)?;

    // Draft 不去重: 每次生成唯一 label
    let label = format!(
        "mini-chat-draft-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    );
    let url = format!(
        "{}/mini-chat.html?mode=draft&directory={}&projectId={}",
        origin,
        url_encode(&directory),
        url_encode(project_id),
    );

    create_mini_chat_window(app, &label, &url)?;

    Ok(json!({ "label": label }))
}

/// `desktop_set_window_pinned` — args: `{ pinned: boolean }`
///
/// 复现 Electron setMiniChatPinned (main.mjs:2717-2735)。
/// pinned → always-on-top ('floating' level) + visible-on-all-workspaces。
/// unpinned → 取消 always-on-top + 取消 all-workspaces。
pub async fn set_window_pinned(args: &Value, window: &WebviewWindow) -> Result<Value, String> {
    let pinned = args.get("pinned").and_then(|v| v.as_bool()).unwrap_or(false);

    if pinned {
        window
            .set_always_on_top(true)
            .map_err(|e| e.to_string())?;
        #[cfg(target_os = "macos")]
        {
            window
                .set_visible_on_all_workspaces(true)
                .map_err(|e| e.to_string())?;
        }
    } else {
        window
            .set_always_on_top(false)
            .map_err(|e| e.to_string())?;
        #[cfg(target_os = "macos")]
        {
            window
                .set_visible_on_all_workspaces(false)
                .map_err(|e| e.to_string())?;
        }
    }

    Ok(json!({ "pinned": pinned }))
}

/// `desktop_get_window_pinned` — 查询当前窗口的 pinning 状态。
pub async fn get_window_pinned(_args: &Value, window: &WebviewWindow) -> Result<Value, String> {
    let pinned = window.is_always_on_top().unwrap_or(false);
    Ok(json!({ "pinned": pinned }))
}

// --- 内部辅助 ---

/// 创建 mini-chat 窗口并注入 init_script。
fn create_mini_chat_window(app: &AppHandle, label: &str, url: &str) -> Result<(), String> {
    let parsed_url = url::Url::parse(url).map_err(|e| format!("invalid URL: {}", e))?;

    let mut builder = WebviewWindowBuilder::new(app, label, WebviewUrl::External(parsed_url))
        .title("OpenChamber Mini Chat")
        .inner_size(MINI_CHAT_WIDTH, MINI_CHAT_HEIGHT)
        .min_inner_size(MINI_CHAT_MIN_WIDTH, MINI_CHAT_MIN_HEIGHT)
        .resizable(true)
        .visible(true);

    // 平台 chrome (与主窗口一致):
    // - macOS: 原生 frame + hidden title bar + traffic lights at {16,17}
    // - Win/Linux: frameless (decorations=false)
    #[cfg(target_os = "macos")]
    {
        builder = builder
            .title_bar_style(tauri::TitleBarStyle::Overlay)
            .hidden_title(true);
    }
    #[cfg(not(target_os = "macos"))]
    {
        builder = builder.decorations(false);
    }

    let window = builder.build().map_err(|e| e.to_string())?;

    // 注入 init_script (与主窗口一致的桥)
    let ctx = RuntimeContext::from_sidecar_port(get_backend_port(app).unwrap_or(0));
    let init_script = build_init_script(&ctx);
    let _ = window.eval(&init_script);

    // 窗口关闭时从去重 map 清理
    let app_handle = app.clone();
    let label_owned = label.to_string();
    window.on_window_event(move |event| {
        if let tauri::WindowEvent::Destroyed = event {
            let app_handle = app_handle.clone();
            let label = label_owned.clone();
            // 在事件回调中延迟执行清理
            if let Some(manager) = app_handle.try_state::<MiniChatManager>() {
                manager.unregister(&label);
            }
        }
    });

    Ok(())
}

/// 从全局 backend port 获取 → 构造 origin。
fn resolve_origin(app: &AppHandle) -> Result<String, String> {
    let port = get_backend_port(app).ok_or("backend not ready")?;
    Ok(format!("http://127.0.0.1:{}", port))
}

/// 从全局 BACKEND_PORT static 获取 port。
fn get_backend_port(_app: &AppHandle) -> Option<u16> {
    // 从全局 BACKEND_PORT static 读取 (lib.rs setup 时写入, 进程内嵌和 sidecar 两路径共用)。
    let guard = BACKEND_PORT.lock().ok()?;
    guard.as_ref().copied()
}

/// 全局 backend port 存储 (lib.rs setup 时写入)。
/// 用单独的 static 而非直接访问 BACKEND, 因为后者包含非 Send 的 runtime。
static BACKEND_PORT: Mutex<Option<u16>> = Mutex::new(None);

/// 供 lib.rs 在 setup 时调用，注册 backend port。
pub fn set_backend_port(port: u16) {
    if let Ok(mut guard) = BACKEND_PORT.lock() {
        *guard = Some(port);
    }
}

/// 简易 URL 编码 (不依赖外部 crate)。
fn url_encode(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                result.push(byte as char);
            }
            _ => {
                result.push_str(&format!("%{:02X}", byte));
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_encode_basic() {
        assert_eq!(url_encode("hello world"), "hello%20world");
        assert_eq!(url_encode("/path/to/dir"), "%2Fpath%2Fto%2Fdir");
        assert_eq!(url_encode("abc123-_.~"), "abc123-_.~");
    }

    #[test]
    fn mini_chat_manager_dedup() {
        let manager = MiniChatManager::new();
        manager.register("http://127.0.0.1:12345", "sess-1", "mini-chat-session-sess-1");
        assert_eq!(
            manager.find_session_window("http://127.0.0.1:12345", "sess-1"),
            Some("mini-chat-session-sess-1".to_string())
        );
        assert_eq!(
            manager.find_session_window("http://127.0.0.1:12345", "sess-2"),
            None
        );
        manager.unregister("mini-chat-session-sess-1");
        assert_eq!(
            manager.find_session_window("http://127.0.0.1:12345", "sess-1"),
            None
        );
    }
}
