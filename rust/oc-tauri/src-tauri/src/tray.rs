//! 动画托盘 — 复现 Electron tray.mjs 的 breathing 动画 + title/tooltip + 状态行图标。
//!
//! `desktop_tray_update` 由 UI 推送 live 状态 (sessions, approvals, dockBadgeCount),
//! 我们据此:
//! 1. compute icon state (busy > unseen > idle)
//! 2. busy → 启动 ping-pong breathing 动画 (16 帧, 75ms/帧)
//! 3. unseen → 静态 unseen 图标
//! 4. idle → 静态 idle 图标
//! 5. compute title (◆ N / ▲ N) + tooltip → set_title / set_tooltip
//! 6. 重建菜单 (含状态行图标)
//!
//! macOS: 所有图标 set_icon_as_template(true), 自动适配深/浅色。
//! Windows: 不动画 (breathIconPaths = [icon.ico] 单元素, < 2 帧 → 不启动动画)。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};

use serde_json::{json, Value};
use tauri::{
    image::Image,
    menu::{IconMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu},
    AppHandle, Emitter, Manager,
};

/// 动画帧间隔 (ms) — 复现 ANIM_INTERVAL_MS。
const ANIM_INTERVAL_MS: u64 = 75;

/// breathing 帧数 — 复现 TRAY_BREATH_FRAME_COUNT。
const BREATH_FRAME_COUNT: usize = 16;

// ============================================================================
// 图标资源 (编译时嵌入)
// ============================================================================

/// 从编译时嵌入的 PNG 字节构建 Tauri Image。
macro_rules! tray_icon {
    ($path:expr) => {
        Image::from_bytes(include_bytes!($path))
            .expect("failed to parse embedded tray icon PNG")
    };
}

/// 加载 idle 图标。
fn load_idle_icon() -> Image<'static> {
    tray_icon!("../icons/tray/trayTemplate-idle.png")
}

/// 加载 unseen 图标。
fn load_unseen_icon() -> Image<'static> {
    tray_icon!("../icons/tray/trayTemplate-unseen.png")
}

/// 加载 16 帧 breathing 动画。
fn load_breath_frames() -> Vec<Image<'static>> {
    (0..BREATH_FRAME_COUNT)
        .map(|i| {
            // include_bytes! 需要字面量路径, 用 match 逐帧索引
            match i {
                0 => tray_icon!("../icons/tray/trayTemplate-breath-00.png"),
                1 => tray_icon!("../icons/tray/trayTemplate-breath-01.png"),
                2 => tray_icon!("../icons/tray/trayTemplate-breath-02.png"),
                3 => tray_icon!("../icons/tray/trayTemplate-breath-03.png"),
                4 => tray_icon!("../icons/tray/trayTemplate-breath-04.png"),
                5 => tray_icon!("../icons/tray/trayTemplate-breath-05.png"),
                6 => tray_icon!("../icons/tray/trayTemplate-breath-06.png"),
                7 => tray_icon!("../icons/tray/trayTemplate-breath-07.png"),
                8 => tray_icon!("../icons/tray/trayTemplate-breath-08.png"),
                9 => tray_icon!("../icons/tray/trayTemplate-breath-09.png"),
                10 => tray_icon!("../icons/tray/trayTemplate-breath-10.png"),
                11 => tray_icon!("../icons/tray/trayTemplate-breath-11.png"),
                12 => tray_icon!("../icons/tray/trayTemplate-breath-12.png"),
                13 => tray_icon!("../icons/tray/trayTemplate-breath-13.png"),
                14 => tray_icon!("../icons/tray/trayTemplate-breath-14.png"),
                15 => tray_icon!("../icons/tray/trayTemplate-breath-15.png"),
                _ => unreachable!(),
            }
        })
        .collect::<Vec<_>>()
}

/// 状态行图标集合 (busy / retry / error / unseen / blank)。
struct StatusIcons {
    busy: Image<'static>,
    retry: Image<'static>,
    error: Image<'static>,
    unseen: Image<'static>,
    blank: Image<'static>,
}

fn load_status_icons() -> StatusIcons {
    StatusIcons {
        busy: tray_icon!("../icons/tray/status/busy.png"),
        retry: tray_icon!("../icons/tray/status/retry.png"),
        error: tray_icon!("../icons/tray/status/error.png"),
        unseen: tray_icon!("../icons/tray/status/unseen.png"),
        blank: tray_icon!("../icons/tray/status/blank.png"),
    }
}

// ============================================================================
// 动画状态 (全局)
// ============================================================================

struct TrayAnimationState {
    /// 当前 icon state ("busy" / "unseen" / "idle" / null)。
    icon_state: Mutex<Option<String>>,
    /// 动画是否运行中。
    anim_running: AtomicBool,
    /// 动画是否已被销毁 (app quit)。
    destroyed: AtomicBool,
    /// 上次设置的 title (diff guard, 避免 per-tick native call)。
    last_title: Mutex<String>,
}

static TRAY_ANIM: LazyLock<TrayAnimationState> = LazyLock::new(|| TrayAnimationState {
    icon_state: Mutex::new(None),
    anim_running: AtomicBool::new(false),
    destroyed: AtomicBool::new(false),
    last_title: Mutex::new(String::new()),
});

/// 图标资源 (lazy init, 一次性加载)。
static IDLE_ICON: LazyLock<Image<'static>> = LazyLock::new(load_idle_icon);
static UNSEEN_ICON: LazyLock<Image<'static>> = LazyLock::new(load_unseen_icon);
static BREATH_FRAMES: LazyLock<Vec<Image<'static>>> = LazyLock::new(load_breath_frames);
static STATUS_ICONS: LazyLock<StatusIcons> = LazyLock::new(load_status_icons);

// ============================================================================
// IPC 入口
// ============================================================================

/// `desktop_tray_update` — args: TraySnapshot `{ sessions, approvals, instanceName, usage, dockBadgeCount }`
///
/// 复现 tray.mjs update(): compute counts → applyIconState → setTitle → setTooltip → rebuild menu。
pub async fn handle_tray_update(args: &Value, app: &AppHandle) -> Result<Value, String> {
    let snapshot = args.clone();

    // dock badge count (macOS only)
    let badge_count = args
        .get("dockBadgeCount")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    #[cfg(target_os = "macos")]
    {
        if let Some(window) = app.get_webview_window("main") {
            let _ = window.set_badge_count(if badge_count > 0 {
                Some(badge_count)
            } else {
                None
            });
        }
    }

    // compute counts
    let (counts, session_count) = compute_counts(args);

    // apply icon state (启动/停止动画)
    let next_state = compute_icon_state(&counts);
    apply_icon_state(app, next_state);

    // title (diff-guarded)
    let title = compute_title(&counts);
    set_title_if_changed(app, &title);

    // tooltip
    let tooltip = compute_tooltip(&counts, session_count);
    if let Some(tray) = app.tray_by_id("main_tray") {
        let _ = tray.set_tooltip(Some(&tooltip));
    }

    // 重建菜单
    if let Err(e) = rebuild_tray_menu(app, &snapshot, &counts) {
        log::warn!("[tray] failed to rebuild menu: {}", e);
    }

    Ok(Value::Null)
}

// ============================================================================
// 计数 + 状态计算
// ============================================================================

/// 从 snapshot 计算 counts + session_count。
struct TrayCounts {
    busy: usize,
    error: usize,
    approvals: usize,
    unseen: usize,
}

fn compute_counts(snapshot: &Value) -> (TrayCounts, usize) {
    let sessions = snapshot.get("sessions").and_then(|v| v.as_array());
    let approvals = snapshot.get("approvals").and_then(|v| v.as_array());

    let approval_count = approvals.map(|a| a.len()).unwrap_or(0);

    let (busy, error, unseen, session_count) = if let Some(sessions) = sessions {
        let mut busy = 0usize;
        let mut error = 0usize;
        let mut unseen = 0usize;
        for s in sessions {
            let status = s.get("status").and_then(|v| v.as_str()).unwrap_or("");
            if status == "busy" || status == "retry" {
                busy += 1;
            }
            if s.get("hasError").and_then(|v| v.as_bool()).unwrap_or(false) {
                error += 1;
            }
            unseen += s
                .get("unseen")
                .and_then(|v| v.as_i64())
                .filter(|n| *n > 0)
                .map(|_| 1usize)
                .unwrap_or(0);
        }
        (busy, error, unseen, sessions.len())
    } else {
        (0, 0, 0, 0)
    };

    (
        TrayCounts {
            busy,
            error,
            approvals: approval_count,
            unseen,
        },
        session_count,
    )
}

/// busy > unseen > idle。
fn compute_icon_state(counts: &TrayCounts) -> &'static str {
    if counts.busy > 0 {
        "busy"
    } else if counts.unseen > 0 {
        "unseen"
    } else {
        "idle"
    }
}

/// ◆ N (approvals) / ▲ N (error) / 空。
fn compute_title(counts: &TrayCounts) -> String {
    if counts.approvals > 0 {
        return format!("◆ {}", counts.approvals);
    }
    if counts.error > 0 {
        return format!("▲ {}", counts.error);
    }
    String::new()
}

/// "OpenChamber — N session(s) · ..."。
fn compute_tooltip(counts: &TrayCounts, session_count: usize) -> String {
    if session_count == 0 {
        return "OpenChamber — no active sessions".to_string();
    }
    let mut bits = Vec::new();
    if counts.approvals > 0 {
        bits.push(format!("{} awaiting approval", counts.approvals));
    }
    if counts.error > 0 {
        bits.push(format!("{} with errors", counts.error));
    }
    if counts.busy > 0 {
        bits.push(format!("{} working", counts.busy));
    }
    if counts.unseen > 0 {
        bits.push(format!("{} unread", counts.unseen));
    }
    let suffix = if bits.is_empty() {
        " · idle".to_string()
    } else {
        format!(" · {}", bits.join(", "))
    };
    let plural = if session_count == 1 { "" } else { "s" };
    format!(
        "OpenChamber — {} session{}{}",
        session_count, plural, suffix
    )
}

// ============================================================================
// 图标状态应用 + 动画
// ============================================================================

/// 应用 icon state — 启动/停止动画, 设置静态图标。
fn apply_icon_state(app: &AppHandle, next_state: &str) {
    let mut current = TRAY_ANIM.icon_state.lock().unwrap();
    let prev = current.as_deref();
    if prev == Some(next_state) {
        return; // no-op, state 未变
    }
    *current = Some(next_state.to_string());
    drop(current);

    if TRAY_ANIM.destroyed.load(Ordering::Relaxed) {
        return;
    }

    match next_state {
        "busy" => {
            // Windows: breath frames = [icon.ico] 单元素 → 不动画, 设静态
            if BREATH_FRAMES.len() > 1 {
                start_animation(app);
            } else if let Some(tray) = app.tray_by_id("main_tray") {
                let frame = BREATH_FRAMES.first().unwrap_or(&*IDLE_ICON);
                let _ = tray.set_icon(Some(frame.clone()));
            }
        }
        "unseen" => {
            stop_animation();
            if let Some(tray) = app.tray_by_id("main_tray") {
                let _ = tray.set_icon(Some((*UNSEEN_ICON).clone()));
                #[cfg(target_os = "macos")]
                {
                    let _ = tray.set_icon_as_template(true);
                }
            }
        }
        _ => {
            // idle
            stop_animation();
            if let Some(tray) = app.tray_by_id("main_tray") {
                let _ = tray.set_icon(Some((*IDLE_ICON).clone()));
                #[cfg(target_os = "macos")]
                {
                    let _ = tray.set_icon_as_template(true);
                }
            }
        }
    }
}

/// 启动 breathing 动画 (ping-pong, 0→15→0)。
fn start_animation(app: &AppHandle) {
    // 已在运行 → no-op
    if TRAY_ANIM.anim_running.swap(true, Ordering::SeqCst) {
        return;
    }

    let app_handle = app.clone();
    let frame_count = BREATH_FRAMES.len();

    tauri::async_runtime::spawn(async move {
        let mut index: usize = 0;
        let mut dir: i32 = 1;

        loop {
            if TRAY_ANIM.destroyed.load(Ordering::Relaxed) || !TRAY_ANIM.anim_running.load(Ordering::Relaxed) {
                break;
            }

            // 设当前帧
            if let Some(tray) = app_handle.tray_by_id("main_tray") {
                let frame = BREATH_FRAMES.get(index).unwrap_or(&*IDLE_ICON);
                let _ = tray.set_icon(Some(frame.clone()));
                #[cfg(target_os = "macos")]
                {
                    let _ = tray.set_icon_as_template(true);
                }
            }

            // ping-pong advance
            if dir > 0 && index >= frame_count - 1 {
                index = frame_count - 1;
                dir = -1;
            } else if dir < 0 && index == 0 {
                index = 0;
                dir = 1;
            } else {
                index = (index as i32 + dir) as usize;
            }

            tokio::time::sleep(std::time::Duration::from_millis(ANIM_INTERVAL_MS)).await;
        }

        TRAY_ANIM.anim_running.store(false, Ordering::SeqCst);
    });
}

/// 停止动画。
fn stop_animation() {
    TRAY_ANIM.anim_running.store(false, Ordering::SeqCst);
}

/// 销毁动画 (app quit 时调用)。
pub fn destroy_tray_animation() {
    TRAY_ANIM.destroyed.store(true, Ordering::Relaxed);
    stop_animation();
}

/// 设置 title (diff-guarded)。
fn set_title_if_changed(app: &AppHandle, title: &str) {
    let mut last = TRAY_ANIM.last_title.lock().unwrap();
    if *last == title {
        return;
    }
    *last = title.to_string();
    drop(last);

    if let Some(tray) = app.tray_by_id("main_tray") {
        // macOS: set_title 设菜单栏图标旁的文字
        let _ = tray.set_title(Some(title));
    }
}

// ============================================================================
// 状态行图标
// ============================================================================

/// 根据 session 属性返回状态行图标 key。
fn status_icon_key(session: &Value) -> &'static str {
    let status = session.get("status").and_then(|v| v.as_str()).unwrap_or("");
    if status == "busy" {
        return "busy";
    }
    if status == "retry" {
        return "retry";
    }
    if session
        .get("hasError")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return "error";
    }
    if session.get("unseen").and_then(|v| v.as_i64()).unwrap_or(0) > 0 {
        return "unseen";
    }
    "blank"
}

/// 获取状态行图标 Image。
fn status_icon_for(key: &str) -> &'static Image<'static> {
    match key {
        "busy" => &STATUS_ICONS.busy,
        "retry" => &STATUS_ICONS.retry,
        "error" => &STATUS_ICONS.error,
        "unseen" => &STATUS_ICONS.unseen,
        _ => &STATUS_ICONS.blank,
    }
}

// ============================================================================
// 菜单重建
// ============================================================================

/// 重建托盘菜单 (含状态行图标)。
fn rebuild_tray_menu(
    app: &AppHandle,
    snapshot: &Value,
    _counts: &TrayCounts,
) -> Result<(), String> {
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
                let label = approval
                    .get("title")
                    .or_else(|| approval.get("label"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("Approval")
                    .to_string();

                if kind == "permission" {
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

    // 会话列表 (max 8) — 带状态行图标
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
                let title = session
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Session");
                let label = format!("{}. {}", i + 1, title);

                // 状态行图标
                let icon_key = status_icon_key(session);
                let icon = status_icon_for(icon_key);

                let session_item = IconMenuItem::with_id(
                    app,
                    format!("session_{}_{}", i, session_id),
                    &label,
                    true,
                    Some(icon.clone()),
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
        create_tray(app, menu)?;
    }

    Ok(())
}

/// 创建托盘图标 + 绑定菜单。
fn create_tray(app: &AppHandle, menu: Menu<tauri::Wry>) -> Result<(), String> {
    let icon = (*IDLE_ICON).clone();

    let _tray = tauri::tray::TrayIconBuilder::with_id("main_tray")
        .icon(icon)
        .menu(&menu)
        .tooltip("OpenChamber")
        .on_menu_event(|app, event| {
            handle_tray_menu_click(app, &event.id().0);
        })
        .on_tray_icon_event(|tray, event| {
            if let tauri::tray::TrayIconEvent::Click {
                button: tauri::tray::MouseButton::Left,
                button_state: tauri::tray::MouseButtonState::Up,
                ..
            } = event
            {
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

    // macOS: 首次创建后设 template
    #[cfg(target_os = "macos")]
    {
        if let Some(tray) = app.tray_by_id("main_tray") {
            let _ = tray.set_icon_as_template(true);
        }
    }

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
            destroy_tray_animation();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_state_priority_busy_over_unseen() {
        let counts = TrayCounts {
            busy: 1,
            error: 0,
            approvals: 0,
            unseen: 5,
        };
        assert_eq!(compute_icon_state(&counts), "busy");
    }

    #[test]
    fn icon_state_unseen_over_idle() {
        let counts = TrayCounts {
            busy: 0,
            error: 0,
            approvals: 0,
            unseen: 3,
        };
        assert_eq!(compute_icon_state(&counts), "unseen");
    }

    #[test]
    fn icon_state_idle_when_empty() {
        let counts = TrayCounts {
            busy: 0,
            error: 0,
            approvals: 0,
            unseen: 0,
        };
        assert_eq!(compute_icon_state(&counts), "idle");
    }

    #[test]
    fn title_shows_approvals_diamond() {
        let counts = TrayCounts {
            busy: 0,
            error: 2,
            approvals: 3,
            unseen: 0,
        };
        assert_eq!(compute_title(&counts), "◆ 3");
    }

    #[test]
    fn title_shows_errors_triangle() {
        let counts = TrayCounts {
            busy: 0,
            error: 1,
            approvals: 0,
            unseen: 0,
        };
        assert_eq!(compute_title(&counts), "▲ 1");
    }

    #[test]
    fn title_empty_when_nothing_notable() {
        let counts = TrayCounts {
            busy: 5,
            error: 0,
            approvals: 0,
            unseen: 10,
        };
        assert_eq!(compute_title(&counts), "");
    }

    #[test]
    fn tooltip_no_sessions() {
        let counts = TrayCounts {
            busy: 0,
            error: 0,
            approvals: 0,
            unseen: 0,
        };
        assert_eq!(
            compute_tooltip(&counts, 0),
            "OpenChamber — no active sessions"
        );
    }

    #[test]
    fn tooltip_single_session_idle() {
        let counts = TrayCounts {
            busy: 0,
            error: 0,
            approvals: 0,
            unseen: 0,
        };
        assert_eq!(
            compute_tooltip(&counts, 1),
            "OpenChamber — 1 session · idle"
        );
    }

    #[test]
    fn tooltip_multiple_sessions_working() {
        let counts = TrayCounts {
            busy: 2,
            error: 0,
            approvals: 1,
            unseen: 0,
        };
        assert_eq!(
            compute_tooltip(&counts, 5),
            "OpenChamber — 5 sessions · 1 awaiting approval, 2 working"
        );
    }

    #[test]
    fn status_icon_key_busy_overrides() {
        let session = json!({ "status": "busy", "unseen": 5, "hasError": true });
        assert_eq!(status_icon_key(&session), "busy");
    }

    #[test]
    fn status_icon_key_retry() {
        let session = json!({ "status": "retry" });
        assert_eq!(status_icon_key(&session), "retry");
    }

    #[test]
    fn status_icon_key_error() {
        let session = json!({ "status": "idle", "hasError": true });
        assert_eq!(status_icon_key(&session), "error");
    }

    #[test]
    fn status_icon_key_unseen() {
        let session = json!({ "status": "idle", "unseen": 3 });
        assert_eq!(status_icon_key(&session), "unseen");
    }

    #[test]
    fn status_icon_key_blank() {
        let session = json!({ "status": "idle" });
        assert_eq!(status_icon_key(&session), "blank");
    }
}
