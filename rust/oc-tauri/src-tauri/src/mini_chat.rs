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

/// Tauri managed state: 后端端口号。
/// 替代旧的 `static BACKEND_PORT: Mutex` — Tauri state 不会被 poison,
/// 且 setup 阶段通过 `app.manage()` 注册后, 在任何 IPC 上下文中都能可靠读取。
pub struct BackendPort(pub u16);

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
///
/// init_script 会注入 `__GRIDFORGE_CLIENT_TOKEN__` (从 SettingsStore 读取的
/// `desktopLocalClientToken`), 使窗口内的 JS 能认证 HTTP API 请求。
fn create_mini_chat_window(app: &AppHandle, label: &str, url: &str) -> Result<(), String> {
    let parsed_url = url::Url::parse(url).map_err(|e| format!("invalid URL: {}", e))?;

    // 先读取 client token 和 runtime headers (SettingsStore 是同步的)
    let client_token = crate::settings::SettingsStore::get("desktopLocalClientToken")
        .and_then(|v| v.as_str().map(|s| s.to_string()));
    let runtime_headers = crate::settings::SettingsStore::get("desktopRuntimeHeaders")
        .and_then(|v| if v.is_object() { Some(v.clone()) } else { None });

    // 构建 init_script (与主窗口一致的桥 + client token + runtime headers)
    //
    // 当 BackendPort 不可用时 (managed OpenCode 启动失败等场景), 不设
    // __GRIDFORGE_API_BASE_URL__, UI 会自动退到同源 API 请求 (页面 origin
    // 即 Vite dev server / Node 后端)。这比硬编码 port 0 更健壮 —— port 0
    // 会导致所有 API 请求走到 `http://127.0.0.1:0` 而失败 ("无法连接服务器")。
    let (port, api_base_url) = match get_backend_port(app) {
        Some(p) => (p, Some(format!("http://127.0.0.1:{}", p))),
        None => (0, None),
    };
    let mut ctx = RuntimeContext::from_sidecar_port(port);
    ctx.api_base_url = api_base_url;
    ctx.client_token = client_token;
    ctx.runtime_headers = runtime_headers;
    let init_script = build_init_script(&ctx);

    // 用 initialization_script 注入 init script, 在页面 JS 执行前运行。
    // 这比 `window.eval()` (窗口创建后再注入) 更可靠:
    // eval 可能因页面未加载而失败, 或页面模块脚本跑在注入之前导致
    // `__GRIDFORGE_CLIENT_TOKEN__` 等全局变量未被 `createConfiguredWebAPIs` 读到。
    let mut builder = WebviewWindowBuilder::new(app, label, WebviewUrl::External(parsed_url))
        .title("GridForge Mini Chat")
        .inner_size(MINI_CHAT_WIDTH, MINI_CHAT_HEIGHT)
        .min_inner_size(MINI_CHAT_MIN_WIDTH, MINI_CHAT_MIN_HEIGHT)
        .resizable(true)
        .visible(true)
        .initialization_script(&init_script);

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

/// 从全局 backend port 或 HMR UI URL 获取 → 构造 origin。
///
/// 优先级:
///   1. `GRIDFORGE_HMR_UI_URL` 环境变量 (Tauri dev 模式, Vite HMR 地址)
///   2. BackendPort managed state (生产模式, oc-server 嵌入地址)
fn resolve_origin(app: &AppHandle) -> Result<String, String> {
    // Dev 模式: 从环境变量读取 Vite 地址 (tauri-dev.mjs 注入)
    if let Ok(hmr_url) = std::env::var("GRIDFORGE_HMR_UI_URL") {
        let trimmed = hmr_url.trim().trim_end_matches('/');
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }
    let port = get_backend_port(app).ok_or("backend not ready")?;
    Ok(format!("http://127.0.0.1:{}", port))
}

/// 从 Tauri managed state 获取后端 port。
///
/// 这是个进程级值, setup 时由 `set_backend_port` 写入 `BackendPort` state。
/// 用 Tauri state 而非 static Mutex 是为了避免 Mutex poison 导致静默失败
/// (static Mutex 被 poison 后 `lock()` 返回 `Err`, 而 `ok()?` 返回 None,
/// 使得 `unwrap_or(0)` 错误地给出端口 0)。
fn get_backend_port(app: &AppHandle) -> Option<u16> {
    app.try_state::<BackendPort>().map(|s| s.0)
}

/// 供 lib.rs 在 setup 时调用，注册 backend port 到 Tauri managed state。
///
/// 同时保留旧的 `set_backend_port_cb` 符号供现有调用方使用。
/// state 注册后, 在任意 IPC handler / 窗口创建函数中均可通过 `app.try_state::<BackendPort>()`
/// 可靠读取, 不会因 Mutex poison 而静默失败。
pub fn set_backend_port(app: &AppHandle, port: u16) {
    app.manage(BackendPort(port));
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
