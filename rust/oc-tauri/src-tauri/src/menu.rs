//! 应用菜单 — 复现 Electron main.mjs buildMacMenu / buildAutoHiddenMenu。
//!
//! macOS: 完整菜单 (App/File/Edit/View/Window/Help + 自定义项)
//! Win/Linux: 简化菜单 (File/View/Help)
//!
//! 自定义项点击 → emit `openchamber:menu-action` 事件到 UI。

use serde_json::json;
use tauri::{
    menu::{AboutMetadata, Menu, MenuItem, PredefinedMenuItem, Submenu},
    AppHandle, Emitter, Manager,
};

/// 构建并设置应用菜单。
pub fn setup_menu(app: &AppHandle) -> Result<(), String> {
    let menu = build_menu(app).map_err(|e| e.to_string())?;

    app.set_menu(menu)
        .map_err(|e| format!("Failed to set menu: {}", e))?;

    Ok(())
}

fn build_menu(app: &AppHandle) -> tauri::Result<Menu<tauri::Wry>> {
    #[cfg(target_os = "macos")]
    {
        build_mac_menu(app)
    }
    #[cfg(not(target_os = "macos"))]
    {
        build_cross_platform_menu(app)
    }
}

#[cfg(target_os = "macos")]
fn build_mac_menu(app: &AppHandle) -> tauri::Result<Menu<tauri::Wry>> {
    // App 菜单 (macOS 标准)
    let app_name = "GridForge";
    let about_meta = AboutMetadata {
        name: Some(app_name.to_string()),
        version: Some(app.package_info().version.to_string()),
        ..Default::default()
    };

    let app_sub = Submenu::with_items(
        app,
        app_name,
        true,
        &[
            &PredefinedMenuItem::about(app, None, Some(about_meta))?,
            &PredefinedMenuItem::separator(app)?,
            &MenuItem::with_id(app, "menu_settings", "Settings...", true, Some("CmdOrCtrl+,"))?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::services(app, None)?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::hide(app, None)?,
            &PredefinedMenuItem::hide_others(app, None)?,
            &PredefinedMenuItem::separator(app)?,
            &MenuItem::with_id(app, "menu_quit", format!("Quit {}", app_name), true, Some("CmdOrCtrl+Q"))?,
        ],
    )?;

    // File 菜单
    let file_sub = Submenu::with_items(
        app,
        "File",
        true,
        &[
            &MenuItem::with_id(app, "menu_new_session", "New Session", true, Some("CmdOrCtrl+N"))?,
            &MenuItem::with_id(app, "menu_new_window", "New Window", true, Some("CmdOrCtrl+Shift+Alt+N"))?,
            &MenuItem::with_id(app, "menu_new_mini_chat", "New Mini Chat", true, Some("CmdOrCtrl+Alt+N"))?,
        ],
    )?;

    // View 菜单
    let view_sub = Submenu::with_items(
        app,
        "View",
        true,
        &[
            &MenuItem::with_id(app, "menu_command_palette", "Command Palette", true, Some("CmdOrCtrl+P"))?,
            &PredefinedMenuItem::separator(app)?,
            &MenuItem::with_id(app, "menu_toggle_session_sidebar", "Toggle Session Sidebar", true, Some("CmdOrCtrl+L"))?,
            &MenuItem::with_id(app, "menu_toggle_right_sidebar", "Toggle Right Sidebar", true, Some("CmdOrCtrl+B"))?,
            &PredefinedMenuItem::separator(app)?,
            &MenuItem::with_id(app, "menu_reload", "Reload", true, Some("CmdOrCtrl+R"))?,
            &MenuItem::with_id(app, "menu_devtools", "Toggle Developer Tools", true, Some("CmdOrCtrl+Alt+I"))?,
        ],
    )?;

    // Help 菜单
    let help_sub = Submenu::with_items(
        app,
        "Help",
        true,
        &[
            &MenuItem::with_id(app, "menu_keyboard_shortcuts", "Keyboard Shortcuts", true, Some("CmdOrCtrl+."))?,
            &MenuItem::with_id(app, "menu_check_updates", "Check for Updates", true, None::<&str>)?,
            &PredefinedMenuItem::separator(app)?,
            &MenuItem::with_id(app, "menu_report_bug", "Report Bug", true, None::<&str>)?,
            &MenuItem::with_id(app, "menu_request_feature", "Request Feature", true, None::<&str>)?,
        ],
    )?;

    Menu::with_items(app, &[&app_sub, &file_sub, &view_sub, &help_sub])
}

#[cfg(not(target_os = "macos"))]
fn build_cross_platform_menu(app: &AppHandle) -> tauri::Result<Menu<tauri::Wry>> {
    // File 菜单
    let file_sub = Submenu::with_items(
        app,
        "File",
        true,
        &[
            &MenuItem::with_id(app, "menu_new_session", "New Session", true, Some("CmdOrCtrl+N"))?,
            &MenuItem::with_id(app, "menu_new_window", "New Window", true, Some("CmdOrCtrl+Shift+Alt+N"))?,
            &PredefinedMenuItem::separator(app)?,
            &MenuItem::with_id(app, "menu_settings", "Settings...", true, Some("CmdOrCtrl+,"))?,
            &MenuItem::with_id(app, "menu_quit", "Quit", true, Some("CmdOrCtrl+Q"))?,
        ],
    )?;

    // View 菜单
    let view_sub = Submenu::with_items(
        app,
        "View",
        true,
        &[
            &MenuItem::with_id(app, "menu_command_palette", "Command Palette", true, Some("CmdOrCtrl+P"))?,
            &PredefinedMenuItem::separator(app)?,
            &MenuItem::with_id(app, "menu_toggle_session_sidebar", "Toggle Session Sidebar", true, Some("CmdOrCtrl+L"))?,
            &MenuItem::with_id(app, "menu_toggle_right_sidebar", "Toggle Right Sidebar", true, Some("CmdOrCtrl+B"))?,
            &PredefinedMenuItem::separator(app)?,
            &MenuItem::with_id(app, "menu_reload", "Reload", true, Some("CmdOrCtrl+R"))?,
            &MenuItem::with_id(app, "menu_devtools", "Toggle Developer Tools", true, Some("CmdOrCtrl+Alt+I"))?,
        ],
    )?;

    // Help 菜单
    let help_sub = Submenu::with_items(
        app,
        "Help",
        true,
        &[
            &MenuItem::with_id(app, "menu_keyboard_shortcuts", "Keyboard Shortcuts", true, Some("CmdOrCtrl+."))?,
            &MenuItem::with_id(app, "menu_check_updates", "Check for Updates", true, None::<&str>)?,
            &PredefinedMenuItem::separator(app)?,
            &MenuItem::with_id(app, "menu_report_bug", "Report Bug", true, None::<&str>)?,
            &MenuItem::with_id(app, "menu_request_feature", "Request Feature", true, None::<&str>)?,
        ],
    )?;

    Menu::with_items(app, &[&file_sub, &view_sub, &help_sub])
}

/// 菜单点击事件处理。
/// 自定义项 → emit `openchamber:menu-action` (action name)
/// 特殊项 → 特殊处理 (quit / reload / devtools / check_updates)
pub fn handle_menu_event(app: &AppHandle, id: &str) {
    match id {
        "menu_quit" => {
            crate::request_quit(app);
        }
        "menu_reload" => {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.eval("location.reload()");
            }
        }
        "menu_devtools" => {
            if let Some(window) = app.get_webview_window("main") {
                #[cfg(debug_assertions)]
                window.open_devtools();
            }
        }
        "menu_check_updates" => {
            let _ = app.emit(
                "openchamber:emit",
                json!({ "event": "openchamber:check-for-updates", "detail": null }),
            );
        }
        "menu_report_bug" => {
            let _ = app.emit(
                "openchamber:emit",
                json!({
                    "event": "openchamber:menu-action",
                    "detail": "report-bug"
                }),
            );
        }
        "menu_request_feature" => {
            let _ = app.emit(
                "openchamber:emit",
                json!({
                    "event": "openchamber:menu-action",
                    "detail": "request-feature"
                }),
            );
        }
        // 其余自定义项 → 统一 emit menu-action
        other => {
            // 去掉 menu_ 前缀作为 action name
            let action = other.strip_prefix("menu_").unwrap_or(other);
            let _ = app.emit(
                "openchamber:emit",
                json!({ "event": "openchamber:menu-action", "detail": action }),
            );
        }
    }
}
