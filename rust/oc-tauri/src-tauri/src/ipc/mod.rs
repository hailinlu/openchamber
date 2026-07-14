//! IPC 分发器: 复现 Electron main.mjs 的 `handleInvoke` switch + origin 门。
//!
//! 桥入口是 `openchamber_invoke` Tauri command, 接收 `{ cmd, args }` 并 dispatch
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

/// `openchamber_invoke` — 桥的 invoke 方法的 Rust 侧入口。
///
/// args 结构: `{ cmd: string, args: object }`
/// 返回: `Result<Value, String>` (error 为字符串, 与 Electron throw Error 一致)。
#[tauri::command]
pub async fn openchamber_invoke(
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
        "desktop_show_app_menu"
        | "desktop_open_in_app"
        | "desktop_open_file_in_app"
        | "desktop_read_file"
        | "desktop_filter_installed_apps"
        | "desktop_fetch_app_icons"
        | "desktop_get_installed_apps"
        | "desktop_capture_page_rect"
        | "desktop_browser_capture_page"
        | "desktop_new_window"
        | "desktop_new_window_for_host"
        | "desktop_new_window_at_url" => {
            Err(format!("Command '{}' not yet implemented in Tauri shell", cmd))
        }

        _ => Err(format!("Unknown desktop command '{}'", cmd)),
    }
}

/// 判断窗口 origin 是否 local。
/// local = openchamber-ui:// 协议 (packaged UI) 或 http(s)://127.0.0.1|localhost:* (loopback)。
pub(crate) fn is_local_origin(window: &WebviewWindow) -> bool {
    let url = match window.url() {
        Ok(u) => u,
        Err(_) => return false,
    };
    let scheme = url.scheme();
    scheme == "openchamber-ui"
        || (scheme == "http" || scheme == "https")
            && url
                .host_str()
                .map(|h| h == "127.0.0.1" || h == "localhost")
                .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

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
