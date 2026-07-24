//! IPC 分发器: 复现 Electron main.mjs 的 `handleInvoke` switch + origin 门。
//!
//! 桥入口是 `gridforge_invoke` Tauri command, 接收 `{ cmd, args }` 并 dispatch
//! 到对应模块。origin 门复现 `COMMANDS_SAFE_FOR_REMOTE` 允许列表。

pub mod dialog_cmd;
pub mod globals;
pub mod shell_cmds;
pub mod system_cmds;
pub mod window_cmds;

use std::collections::HashSet;
use std::sync::LazyLock;

use serde_json::Value;
use tauri::{AppHandle, WebviewWindow};

/// 复现 main.mjs:4514-4532 COMMANDS_SAFE_FOR_REMOTE。
/// 这些命令可从 non-local origin (远程 OpenChamber 服务器渲染的页面) 调用。
/// 其余命令仅 local-origin 可用。
static COMMANDS_SAFE_FOR_REMOTE: LazyLock<HashSet<&str>> = LazyLock::new(|| {
    HashSet::from([
        "desktop_hosts_get",
        "desktop_host_probe",
        "desktop_new_window",
        "desktop_new_window_at_url",
        "desktop_new_window_for_host",
        "desktop_set_window_title",
        "desktop_set_window_theme",
        "desktop_is_window_fullscreen",
        "desktop_start_window_drag",
        "desktop_minimize_current_window",
        "desktop_toggle_current_window_maximized",
        "desktop_close_current_window",
        "desktop_get_current_window_state",
        "desktop_get_app_version",
        "desktop_get_lan_address",
        "desktop_capture_page_rect",
        "desktop_tray_update",
    ])
});

/// `gridforge_invoke` — 桥的 invoke 方法的 Rust 侧入口。
///
/// args 结构: `{ cmd: string, args: object }`
/// 返回: `Result<Value, String>` (error 为字符串, 与 Electron throw Error 一致)。
#[tauri::command]
pub async fn gridforge_invoke(
    cmd: String,
    args: Value,
    window: WebviewWindow,
    app: AppHandle,
) -> Result<Value, String> {
    // origin 门: 非 local 且不在 SAFE_FOR_REMOTE → 拒绝
    let is_local = is_local_origin(&window);
    if !is_local && !COMMANDS_SAFE_FOR_REMOTE.contains(cmd.as_str()) {
        let origin = window.url().map(|u| u.to_string()).unwrap_or_default();
        log::warn!("[ipc] rejected {} from non-local origin: {}", cmd, origin);
        return Err("IPC not available for this origin".into());
    }

    dispatch(&cmd, &args, &window, &app).await
}

/// 命令分发器: 复现 handleInvoke switch。
///
/// 已实现的命令走对应模块; 未实现的返回明确 error (不 crash)。
async fn dispatch(
    cmd: &str,
    args: &Value,
    window: &WebviewWindow,
    app: &AppHandle,
) -> Result<Value, String> {
    match cmd {
        // --- 窗口 chrome ---
        "desktop_start_window_drag" => window_cmds::start_window_drag(args, window).await,
        "desktop_is_window_fullscreen" => window_cmds::is_window_fullscreen(args, window).await,
        "desktop_set_window_title" => window_cmds::set_window_title(args, window).await,
        "desktop_get_current_window_state" => {
            window_cmds::get_current_window_state(args, window).await
        }
        "desktop_minimize_current_window" => window_cmds::minimize_current_window(args, window).await,
        "desktop_toggle_current_window_maximized" => {
            window_cmds::toggle_current_window_maximized(args, window).await
        }
        "desktop_close_current_window" => window_cmds::close_current_window(args, window).await,
        "desktop_set_window_theme" => window_cmds::set_window_theme(args, window).await,
        "desktop_set_vibrancy" => window_cmds::set_vibrancy(args, app).await,
        "desktop_focus_main_window" => window_cmds::focus_main_window(args, app).await,
        "desktop_show_app_menu" => window_cmds::show_app_menu(args, window).await,
        "desktop_new_window_at_url" => window_cmds::new_window_at_url(args, app).await,
        "desktop_new_window_for_host" => window_cmds::new_window_for_host(args, app).await,

        // --- mini-chat ---
        "desktop_open_session_mini_chat_window" => {
            crate::mini_chat::open_session_mini_chat(args, app).await
        }
        "desktop_open_draft_mini_chat_window" => {
            crate::mini_chat::open_draft_mini_chat(args, app).await
        }
        "desktop_set_window_pinned" => crate::mini_chat::set_window_pinned(args, window).await,
        "desktop_get_window_pinned" => crate::mini_chat::get_window_pinned(args, window).await,

        // --- shell / 文件 ---
        "desktop_open_external_url" => shell_cmds::open_external_url(args, window).await,
        "desktop_open_path" => shell_cmds::open_path(args, window).await,
        "desktop_reveal_path" => shell_cmds::reveal_path(args, window).await,
        "desktop_save_markdown_file" => shell_cmds::save_markdown_file(args, app).await,
        "desktop_clear_cache" => shell_cmds::clear_cache(args, app).await,
        "desktop_get_app_version" => shell_cmds::get_app_version(args, app).await,
        "desktop_open_in_app" => shell_cmds::open_in_app(args, window).await,

        // --- 系统 ---
        "desktop_get_lan_address" => system_cmds::get_lan_address(args, app).await,
        "desktop_get_launch_at_login" => system_cmds::get_launch_at_login(args, app).await,
        "desktop_set_launch_at_login" => system_cmds::set_launch_at_login(args, app).await,
        "desktop_notify" => system_cmds::notify(args, app).await,
        "desktop_get_minimize_to_tray" => system_cmds::get_minimize_to_tray(args, app).await,
        "desktop_set_minimize_to_tray" => system_cmds::set_minimize_to_tray(args, app).await,
        "desktop_get_keep_awake" => system_cmds::get_keep_awake(args, app).await,
        "desktop_set_keep_awake" => system_cmds::set_keep_awake(args, app).await,

        // --- hosts / discovery ---
        "desktop_hosts_get" => crate::discovery::hosts_get(args, app).await,
        "desktop_hosts_set" => crate::discovery::hosts_set(args, app).await,
        "desktop_host_probe" => crate::discovery::host_probe(args, app).await,
        "desktop_install_id_get" => crate::discovery::install_id_get(args, app).await,
        "desktop_local_client_token_get" => {
            crate::discovery::local_client_token_get(args, app).await
        }
        "desktop_remote_password_login" => crate::discovery::remote_password_login(args, app).await,

        // --- updater ---
        "desktop_check_for_updates" => crate::updater::check_for_updates(args, app).await,
        "desktop_download_and_install_update" => {
            crate::updater::download_and_install(args, app).await
        }
        "desktop_restart" => crate::updater::restart(args, app).await,

        // --- SSH ---
        "desktop_ssh_instances_get" => crate::ssh::instances_get(args, app).await,
        "desktop_ssh_instances_set" => crate::ssh::instances_set(args, app).await,
        "desktop_ssh_import_hosts" => crate::ssh::import_hosts(args, app).await,
        "desktop_ssh_connect" => crate::ssh::connect(args, app).await,
        "desktop_ssh_disconnect" => crate::ssh::disconnect(args, app).await,
        "desktop_ssh_status" => crate::ssh::status(args, app).await,
        "desktop_ssh_logs" => crate::ssh::logs(args, app).await,
        "desktop_ssh_logs_clear" => crate::ssh::logs_clear(args, app).await,

        // --- 托盘 (tray.rs 实现, 通过 tray state 路由) ---
        "desktop_tray_update" => crate::tray::handle_tray_update(args, app).await,

        // --- 未实现的命令: 明确 error ---
        "desktop_open_file_in_app"
        | "desktop_read_file"
        | "desktop_filter_installed_apps"
        | "desktop_fetch_app_icons"
        | "desktop_get_installed_apps"
        | "desktop_capture_page_rect"
        | "desktop_browser_capture_page"
        | "desktop_new_window" => {
            Err(format!("Command '{}' not yet implemented in Tauri shell", cmd))
        }

        _ => Err(format!("Unknown desktop command '{}'", cmd)),
    }
}

/// 判断窗口 origin 是否 local。
///
/// local =
/// 1. `gridforge-ui://` 自定义协议 (packaged UI, 历史方案);
/// 2. `http(s)://127.0.0.1|* localhost:*` (loopback, dev 时 Vite / oc-server 同源);
/// 3. Tauri 2.x App 模式默认 origin:
///    - `http://tauri.localhost` (Windows / Linux);
///    - `https://tauri.localhost` (Linux 上 wry 偶尔用 https);
///    - `tauri://localhost` (macOS)。
///
/// 第三类是 commit `8f6ccd0e revert main webview to WebviewUrl::App` 之后
/// 实际渲染出来的 origin (代码注释见 `lib.rs:96-117`)。漏掉它会让所有
/// desktop IPC (host probe / 窗口控制 / reveal_path / open_path / mini-chat)
/// 走 `IPC not available for this origin` 分支 —— 表现为 UI 里 "open in
/// Finder" / mini chat 完全不可用, 与本次用户报告一致。修法是把
/// `tauri.localhost` host + `tauri://localhost` scheme 都纳入 local 集合。
/// 注意: 这个 host 是 Tauri 在 webview URL 解析阶段固定的字符串, 不可被
/// 远程页面伪造, 所以放宽到 `tauri.localhost` 不构成新攻击面。
pub(crate) fn is_local_origin(window: &WebviewWindow) -> bool {
    let raw = match window.url() {
        Ok(u) => u.to_string(),
        Err(_) => return false,
    };
    is_local_url(&raw)
}

/// 纯函数: 判定一个 URL 字符串是否指向 local origin。
///
/// 拆出来是为了让单元测试直接喂 URL 字符串 (避免构造 `WebviewWindow`),
/// 同时把 policy 与 Tauri API 副作用解耦。调用方 (`is_local_origin`)
/// 负责把 `WebviewWindow::url()` 翻译成字符串后转过来。
///
/// 接受的 origin 集合 (与上方 doc 同步):
/// - `gridforge-ui://...`
/// - `tauri://localhost` (macOS App 模式)
/// - `http(s)://127.0.0.1` / `http(s)://localhost` (loopback, dev 时 Vite / oc-server)
/// - `http(s)://tauri.localhost` (Win/Linux App 模式)
pub(crate) fn is_local_url(raw: &str) -> bool {
    let url = match url::Url::parse(raw) {
        Ok(u) => u,
        Err(_) => return false,
    };
    let scheme = url.scheme();
    let host = url.host_str().unwrap_or("");
    match scheme {
        "gridforge-ui" => true,
        "tauri" => host == "localhost",
        "http" | "https" => host == "127.0.0.1" || host == "localhost" || host == "tauri.localhost",
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- is_local_url pure helper ---

    #[test]
    fn is_local_url_accepts_tauri_app_mode_windows_linux() {
        // Win/Linux App 模式渲染时 webview 的 origin —— commit 8f6ccd0e 之后
        // 真实使用的 origin, 漏掉它会让所有 desktop IPC 被拒绝。
        assert!(is_local_url("http://tauri.localhost/"));
        assert!(is_local_url("http://tauri.localhost/?session=ses_xxx"));
        // wry 在某些 Linux 发行版会走 https, 同样要接受。
        assert!(is_local_url("https://tauri.localhost/"));
    }

    #[test]
    fn is_local_url_accepts_tauri_app_mode_macos() {
        // macOS App 模式默认 scheme 是 tauri://localhost (https 需显式开启)。
        assert!(is_local_url("tauri://localhost/"));
    }

    #[test]
    fn is_local_url_accepts_loopback_dev_origin() {
        // Dev: Vite 5180 / oc-server 端口都是 loopback, 也算 local。
        assert!(is_local_url("http://127.0.0.1:5180/"));
        assert!(is_local_url("http://localhost:3001/"));
        assert!(is_local_url("https://127.0.0.1/"));
    }

    #[test]
    fn is_local_url_accepts_gridforge_ui_protocol() {
        // 历史方案 / 自定义 packaged UI 协议, 仍要接受以兼容旧路径。
        assert!(is_local_url("gridforge-ui://app/index.html"));
    }

    #[test]
    fn is_local_url_rejects_remote_origins() {
        // 真·远程 origin 必须拒绝 (remote URL 经 oc-server 渲染时, 这些
        // 页面触发的 IPC 应受 SAFE_FOR_REMOTE 列表约束, 不能全放行)。
        assert!(!is_local_url("http://192.168.1.10/"));
        assert!(!is_local_url("https://example.com/"));
        assert!(!is_local_url("http://localhost.evil.com/"));
    }

    #[test]
    fn is_local_url_rejects_tauri_localhost_host_spoofs() {
        // 段名前缀攻击: `nottauri.localhost` 不是 `tauri.localhost`, 必须拒。
        assert!(!is_local_url("http://nottauri.localhost/"));
        // 段后追加攻击: `tauri.localhost.evil.com` 必须拒。
        assert!(!is_local_url("http://tauri.localhost.evil.com/"));
    }

    #[test]
    fn is_local_url_rejects_unknown_schemes() {
        // 不在 allowlist 的 scheme 一律拒绝 (file://, data://, blob:// 等)。
        assert!(!is_local_url("file:///c:/windows/system32"));
        assert!(!is_local_url("data:text/html,<script>"));
    }

    #[test]
    fn is_local_url_rejects_unparseable_strings() {
        // 解析失败的字符串 (空 / 乱七八糟) 必须保守拒绝。
        assert!(!is_local_url(""));
        assert!(!is_local_url("not a url"));
        assert!(!is_local_url("tauri://"));
    }

    #[test]
    fn safe_for_remote_contains_expected_commands() {
        assert!(COMMANDS_SAFE_FOR_REMOTE.contains("desktop_get_app_version"));
        assert!(COMMANDS_SAFE_FOR_REMOTE.contains("desktop_start_window_drag"));
        assert!(COMMANDS_SAFE_FOR_REMOTE.contains("desktop_tray_update"));
        assert!(COMMANDS_SAFE_FOR_REMOTE.contains("desktop_hosts_get"));
    }

    #[test]
    fn safe_for_remote_excludes_privileged_commands() {
        assert!(!COMMANDS_SAFE_FOR_REMOTE.contains("desktop_open_path"));
        assert!(!COMMANDS_SAFE_FOR_REMOTE.contains("desktop_notify"));
        assert!(!COMMANDS_SAFE_FOR_REMOTE.contains("desktop_ssh_connect"));
        assert!(!COMMANDS_SAFE_FOR_REMOTE.contains("desktop_save_markdown_file"));
    }

    #[test]
    fn safe_for_remote_count_matches_electron() {
        // Electron main.mjs:4514-4532 有 17 个命令
        assert_eq!(COMMANDS_SAFE_FOR_REMOTE.len(), 17);
    }
}
