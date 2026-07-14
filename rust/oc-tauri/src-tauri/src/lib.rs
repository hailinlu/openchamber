//! OpenChamber 桌面壳 (Tauri)。
//!
//! 阶段 4A (sidecar 过渡态 + IPC 契约对等):
//! - Tauri 进程以子进程方式拉起 `@openchamber/web` CLI (`openchamber serve --foreground`)
//! - WebView 通过 loopback 加载 UI
//! - init_script 注入 `window.__OPENCHAMBER_DESKTOP__` 桥 + 标量全局变量 (UI 零改动)
//! - IPC 分发器复现 Electron handleInvoke + origin 门 + SAFE_FOR_REMOTE
//! - 静态托盘 + 应用菜单 + 深链注册
//!
//! 见 docs/plan/rust-migration-plan.md。

mod ipc;
mod menu;
mod sidecar;
mod tray;

use std::sync::Mutex;

use ipc::globals::{build_init_script, RuntimeContext};
use sidecar::{SidecarBuilder, SidecarHandle};
use tauri::Manager;

/// 全局 sidecar 句柄 + 它专属的 tokio 运行时。
struct SidecarState {
    handle: Option<SidecarHandle>,
    rt: Option<tokio::runtime::Runtime>,
}

static SIDECAR: Mutex<Option<SidecarState>> = Mutex::new(None);

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // --- 插件注册 ---
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(tauri_plugin_deep_link::init())
        .setup(|app| {
            if cfg!(debug_assertions) {
                app.handle().plugin(
                    tauri_plugin_log::Builder::default()
                        .level(log::LevelFilter::Info)
                        .build(),
                )?;
            }

            // --- 启动 sidecar (仅桌面端) ---
            #[cfg(desktop)]
            {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()?;
                let handle = rt.block_on(async {
                    SidecarBuilder::new()
                        .ready_timeout(std::time::Duration::from_secs(45))
                        .start()
                        .await
                });
                match handle {
                    Ok(h) => {
                        let port = h.port();
                        log::info!("sidecar ready on port {}", port);

                        // --- 注入 init_script (标量全局变量 + IPC 桥) ---
                        let ctx = RuntimeContext::from_sidecar_port(port);
                        let init_script = build_init_script(&ctx);
                        if let Some(window) = app.get_webview_window("main") {
                            if let Err(e) = window.eval(&init_script) {
                                log::error!("failed to inject init_script: {}", e);
                            }
                        }

                        *SIDECAR.lock().unwrap() = Some(SidecarState {
                            handle: Some(h),
                            rt: Some(rt),
                        });
                    }
                    Err(e) => {
                        log::error!("sidecar startup failed: {:#}", e);
                        drop(rt);
                    }
                }
            }

            // --- 应用菜单 ---
            if let Err(e) = menu::setup_menu(app.handle()) {
                log::warn!("failed to setup menu: {}", e);
            }

            Ok(())
        })
        // --- IPC invoke_handler 注册 ---
        .invoke_handler(tauri::generate_handler![
            ipc::openchamber_invoke,
            ipc::dialog_cmd::openchamber_dialog_open,
            ipc::dialog_cmd::openchamber_file_grant,
        ])
        // --- 菜单事件 ---
        .on_menu_event(|app, event| {
            menu::handle_menu_event(app, &event.id().0);
        })
        // --- 深链事件 ---
        .on_webview_event(|_window, _event| {
            // TODO: deep-link 事件处理 (tauri-plugin-deep-link)
        })
        .on_window_event(|window, event| {
            // 主窗口关闭时触发 sidecar 清理。
            if let tauri::WindowEvent::Destroyed = event {
                let app = window.app_handle();
                if app.webview_windows().is_empty() {
                    shutdown_sidecar();
                }
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|_app_handle, event| {
            if matches!(event, tauri::RunEvent::Exit) {
                shutdown_sidecar();
            }
        });
}

/// 同步清理 sidecar: 从全局状态取出句柄, 在它的运行时上 block_on kill。
fn shutdown_sidecar() {
    let mut guard = SIDECAR.lock().unwrap();
    if let Some(mut state) = guard.take() {
        if let (Some(rt), Some(mut handle)) = (state.rt.take(), state.handle.take()) {
            let _ = rt.block_on(async { handle.kill().await });
        }
        drop(state);
    }
}
