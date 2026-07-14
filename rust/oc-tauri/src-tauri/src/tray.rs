//! 静态托盘 — 复现 Electron tray.mjs 的核心功能。
//!
//! `desktop_tray_update` 由 UI 推送 live 状态 (sessions, approvals, dockBadgeCount),
//! 我们据此重建托盘菜单。
//!
//! 不实现: 动画图标 (breathing 16 帧)、usage 子菜单、Linux 托盘。

use serde_json::{json, Value};
use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem, Submenu},
    AppHandle, Manager,
};

/// `desktop_tray_update` — args: TraySnapshot `{ sessions, approvals, instanceName, usage, dockBadgeCount }`
///
/// 复现 tray.mjs buildMenu: header / 审批 (Allow once/always/Deny) / 会话列表 / New / Show / Quit。
pub async fn handle_tray_update(args: &Value, app: &AppHandle) -> Result<Value, String> {
    let snapshot = args.clone();

    // dock badge count (macOS only)
    #[allow(unused_variables)]
    let badge_count = args
        .get("dockBadgeCount")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    #[cfg(target_os = "macos")]
    {
        if let Some(window) = app.get_webview_window("main") {
            let _ = window.set_badge_count(Some(badge_count));
        }
    }

    // 构建并设置托盘菜单 (异步在 spawn_blocking 或直接同步构建)
    let app_handle = app.clone();
    let snap = snapshot.clone();
    // Tauri menu 构建是同步的, 在 async 上下文里直接调用 (不阻塞太久)
    if let Err(e) = rebuild_tray_menu(&app_handle, &snap) {
        log::warn!("[tray] failed to rebuild menu: {}", e);
    }

    Ok(Value::Null)
}

/// 重建托盘菜单 (静态图标 + 动态菜单项)。
fn rebuild_tray_menu(app: &AppHandle, snapshot: &Value) -> Result<(), String> {
    let instance_name = snapshot
        .get("instanceName")
        .and_then(|v| v.as_str())
        .unwrap_or("OpenChamber")
        .to_string();

    // header
    let header_item = MenuItem::with_id(app, "tray_header", &instance_name, false, None::<&str>)
        .map_err(|e| e.to_string())?;
    let sep1 = PredefinedMenuItem::separator(app).map_err(|e| e.to_string())?;

    let mut items: Vec<Box<dyn tauri::menu::IsMenuItem<tauri::Wry>>> = Vec::new();
    items.push(Box::new(header_item));
    items.push(Box::new(sep1));

    // 审批列表 (max 10)
    let approvals = snapshot.get("approvals").and_then(|v| v.as_array());
    if let Some(approvals) = approvals {
        if !approvals.is_empty() {
            let attention_label =
                MenuItem::with_id(app, "tray_attention", "Needs your attention", false, None::<&str>)
                    .map_err(|e| e.to_string())?;
            items.push(Box::new(attention_label));

            for (i, approval) in approvals.iter().take(10).enumerate() {
                let kind = approval.get("kind").and_then(|v| v.as_str()).unwrap_or("");
                let session_id = approval
                    .get("sessionId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let _directory = approval
                    .get("directory")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let label = approval
                    .get("title")
                    .or_else(|| approval.get("label"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("Approval")
                    .to_string();

                if kind == "permission" {
                    // permission → 子菜单 Allow once / Allow always / Deny / Open in app
                    let id_base = format!("approval_{}_{}", i, session_id);
                    let allow_once = MenuItem::with_id(
                        app,
                        format!("{}_once", id_base),
                        "Allow once",
                        true,
                        None::<&str>,
                    )
                    .map_err(|e| e.to_string())?;
                    let allow_always = MenuItem::with_id(
                        app,
                        format!("{}_always", id_base),
                        "Allow always",
                        true,
                        None::<&str>,
                    )
                    .map_err(|e| e.to_string())?;
                    let deny = MenuItem::with_id(
                        app,
                        format!("{}_reject", id_base),
                        "Deny",
                        true,
                        None::<&str>,
                    )
                    .map_err(|e| e.to_string())?;
                    let open_in = MenuItem::with_id(
                        app,
                        format!("{}_open", id_base),
                        "Open in app",
                        true,
                        None::<&str>,
                    )
                    .map_err(|e| e.to_string())?;

                    let submenu = Submenu::with_items(
                        app,
                        &label,
                        true,
                        &[&allow_once, &allow_always, &deny, &open_in],
                    )
                    .map_err(|e| e.to_string())?;
                    items.push(Box::new(submenu));
                } else {
                    // question/other → click 聚焦 session
                    let focus_item = MenuItem::with_id(
                        app,
                        format!("approval_focus_{}_{}", i, session_id),
                        &label,
                        true,
                        None::<&str>,
                    )
                    .map_err(|e| e.to_string())?;
                    items.push(Box::new(focus_item));
                }
            }

            let sep2 = PredefinedMenuItem::separator(app).map_err(|e| e.to_string())?;
            items.push(Box::new(sep2));
        }
    }

    // 会话列表 (max 8)
    let sessions = snapshot.get("sessions").and_then(|v| v.as_array());
    if let Some(sessions) = sessions {
        if !sessions.is_empty() {
            for (i, session) in sessions.iter().take(8).enumerate() {
                let session_id = session
                    .get("sessionId")
                    .or_else(|| session.get("id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let _directory = session
                    .get("directory")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let title = session
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Session");
                let label = format!("{}. {}", i + 1, title);

                let session_item = MenuItem::with_id(
                    app,
                    format!("session_{}_{}", i, session_id),
                    &label,
                    true,
                    None::<&str>,
                )
                .map_err(|e| e.to_string())?;
                items.push(Box::new(session_item));
            }
            let sep3 = PredefinedMenuItem::separator(app).map_err(|e| e.to_string())?;
            items.push(Box::new(sep3));
        }
    }

    // 快捷操作
    let new_session = MenuItem::with_id(app, "tray_new_session", "New Session", true, Some("CmdOrCtrl+N"))
        .map_err(|e| e.to_string())?;
    let show = MenuItem::with_id(app, "tray_show", "Show OpenChamber", true, None::<&str>)
        .map_err(|e| e.to_string())?;
    let quit = MenuItem::with_id(app, "tray_quit", "Quit OpenChamber", true, Some("CmdOrCtrl+Q"))
        .map_err(|e| e.to_string())?;

    items.push(Box::new(new_session));
    items.push(Box::new(show));
    items.push(Box::new(quit));

    // 构建 menu
    let item_refs: Vec<&dyn tauri::menu::IsMenuItem<tauri::Wry>> =
        items.iter().map(|b| b.as_ref()).collect();
    let menu = Menu::with_items(app, &item_refs).map_err(|e| e.to_string())?;

    // 设置到托盘 (如果托盘已存在则更新, 否则创建)
    if let Some(tray) = app.tray_by_id("main_tray") {
        tray.set_menu(Some(menu)).map_err(|e| e.to_string())?;
    } else {
        // 首次创建托盘
        create_tray(app, menu)?;
    }

    Ok(())
}

/// 创建托盘图标 + 绑定菜单。
fn create_tray(app: &AppHandle, menu: Menu<tauri::Wry>) -> Result<(), String> {
    // 托盘图标: 使用应用图标 (静态, 无动画)
    let icon = app
        .default_window_icon()
        .cloned()
        .ok_or_else(|| "No default window icon available for tray".to_string())?;

    let _tray = tauri::tray::TrayIconBuilder::with_id("main_tray")
        .icon(icon)
        .menu(&menu)
        .tooltip("OpenChamber")
        .on_menu_event(|app, event| {
            handle_tray_menu_click(app, &event.id().0);
        })
        .on_tray_icon_event(|tray, event| {
            if let tauri::tray::TrayIconEvent::Click { button: tauri::tray::MouseButton::Left, button_state: tauri::tray::MouseButtonState::Up, .. } = event {
                // 左键点击 → 显示主窗口
                let app = tray.app_handle();
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            }
        })
        .build(app)
        .map_err(|e| e.to_string())?;

    Ok(())
}

/// 托盘菜单点击处理: 根据 menu item id 发对应事件到 UI。
fn handle_tray_menu_click(app: &AppHandle, id: &str) {
    // 快捷操作
    match id {
        "tray_new_session" => {
            let _ = app.emit(
                "openchamber:emit",
                json!({ "event": "openchamber:open-draft-session", "detail": {} }),
            );
            return;
        }
        "tray_show" => {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.unminimize();
                let _ = window.set_focus();
            }
            return;
        }
        "tray_quit" => {
            app.exit(0);
            return;
        }
        _ => {}
    }

    // session_N_<id> → focus session
    if let Some(rest) = id.strip_prefix("session_") {
        // rest = "N_<sessionId>"
        if let Some(session_id) = rest.split('_').nth(1) {
            let _ = app.emit(
                "openchamber:emit",
                json!({
                    "event": "openchamber:open-session",
                    "detail": { "sessionId": session_id }
                }),
            );
        }
        return;
    }

    // approval_focus_N_<id> → focus session
    if let Some(rest) = id.strip_prefix("approval_focus_") {
        if let Some(session_id) = rest.split('_').nth(1) {
            let _ = app.emit(
                "openchamber:emit",
                json!({
                    "event": "openchamber:open-session",
                    "detail": { "sessionId": session_id }
                }),
            );
        }
        return;
    }

    // approval_N_<id>_<response> → respond-permission
    if let Some(rest) = id.strip_prefix("approval_") {
        // rest = "N_<sessionId>_<response>"
        let parts: Vec<&str> = rest.split('_').collect();
        if parts.len() >= 3 {
            let session_id = parts[1];
            let response = parts[2]; // once / always / reject
            let _ = app.emit(
                "openchamber:emit",
                json!({
                    "event": "openchamber:tray-action",
                    "detail": {
                        "type": "respond-permission",
                        "sessionId": session_id,
                        "response": response
                    }
                }),
            );
        }
        return;
    }

    log::debug!("[tray] unhandled menu click: {}", id);
}

use tauri::Emitter;
