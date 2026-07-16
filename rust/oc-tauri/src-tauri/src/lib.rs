//! OpenChamber 桌面壳 (Tauri)。
//!
//! 阶段 4A (sidecar 过渡态 + IPC 契约对等):
//! - Tauri 进程以子进程方式拉起 `@openchamber/web` CLI (`openchamber serve --foreground`)
//! - WebView 通过 loopback 加载 UI
//! - init_script 注入 `window.__OPENCHAMBER_DESKTOP__` 桥 + 标量全局变量 (UI 零改动)
//! - IPC 分发器复现 Electron handleInvoke + origin 门 + SAFE_FOR_REMOTE
//! - 静态托盘 + 应用菜单 + 深链注册
//! - settings.json 原子持久化 (与 Electron 共享)
//! - keep-awake (caffeinate/SetThreadExecutionState)
//!
//! 见 docs/plan/rust-migration-plan.md。

mod backend;
mod discovery;
mod ipc;
mod menu;
mod mini_chat;
mod power;
mod settings;
mod sidecar;
mod ssh;
mod tray;
mod updater;

use std::sync::Mutex;

use backend::BackendHandle;
use ipc::globals::{build_init_script, RuntimeContext};
use sidecar::SidecarBuilder;
use tauri::Manager;

/// 全局后端句柄 + 它专属的 tokio 运行时。
struct BackendState {
    handle: Option<BackendHandle>,
    rt: Option<tokio::runtime::Runtime>,
}

static BACKEND: Mutex<Option<BackendState>> = Mutex::new(None);

/// 获取后端 base_url (供 IPC 命令 HTTP 调用后端端点)。
///
/// 返回 `http://127.0.0.1:<port>`，后端未启动时返回 None。
/// 用例: `dialog_cmd::openchamber_file_grant` 调 `POST /api/fs/grant`。
pub fn backend_base_url() -> Option<String> {
    BACKEND
        .lock()
        .ok()?
        .as_ref()
        .and_then(|s| s.handle.as_ref())
        .map(|h| h.base_url())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // --- 插件注册 ---
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        // autostart: 传递 --background 参数 (复现 Electron openAsHidden + BACKGROUND_START_ARG)
        .plugin(
            tauri_plugin_autostart::Builder::new()
                .args(["--background"])
                .build(),
        )
        .plugin(tauri_plugin_deep_link::init())
        .manage(settings::SettingsStore::new())
        .manage(mini_chat::MiniChatManager::new())
        .manage(ssh::SshManager::new())
        .setup(|app| {
            if cfg!(debug_assertions) {
                app.handle().plugin(
                    tauri_plugin_log::Builder::default()
                        .level(log::LevelFilter::Info)
                        .build(),
                )?;
            }

            // --- 启动后端 (仅桌面端) ---
            #[cfg(desktop)]
            {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()?;

                if backend::use_sidecar() {
                    // —— 回退路径: sidecar 子进程 (现状不变) ——
                    let handle = rt.block_on(async {
                        SidecarBuilder::new()
                            .ready_timeout(std::time::Duration::from_secs(45))
                            .arg("--api-only")
                            .start()
                            .await
                    });
                    match handle {
                        Ok(h) => {
                            let port = h.port();
                            log::info!("sidecar ready on port {}", port);
                            mini_chat::set_backend_port(port);

                            let ctx = RuntimeContext::from_sidecar_port(port);
                            let init_script = build_init_script(&ctx);
                            if let Some(window) = app.get_webview_window("main") {
                                if let Err(e) = window.eval(&init_script) {
                                    log::error!("failed to inject init_script: {}", e);
                                }
                                #[cfg(all(target_os = "macos", feature = "vibrancy"))]
                                {
                                    apply_vibrancy_if_enabled(&window);
                                }
                            }

                            *BACKEND.lock().unwrap() = Some(BackendState {
                                handle: Some(BackendHandle::Sidecar(h)),
                                rt: Some(rt),
                            });
                        }
                        Err(e) => {
                            log::error!("sidecar startup failed: {:#}", e);
                            drop(rt);
                        }
                    }
                } else {
                    // —— 新路径: 进程内嵌入 oc-server ——
                    let server_result = rt.block_on(async {
                        let config = oc_server::Config::load()?;
                        oc_server::OcServer::start(config).await
                    });
                    match server_result {
                        Ok(server) => {
                            let base_url = server.base_url().to_string();
                            let port = backend::parse_port(&base_url);
                            log::info!("oc-server (in-process) ready on port {}", port);
                            mini_chat::set_backend_port(port);

                            let ctx = RuntimeContext::from_sidecar_port(port);
                            let init_script = build_init_script(&ctx);
                            if let Some(window) = app.get_webview_window("main") {
                                if let Err(e) = window.eval(&init_script) {
                                    log::error!("failed to inject init_script: {}", e);
                                }
                                #[cfg(all(target_os = "macos", feature = "vibrancy"))]
                                {
                                    apply_vibrancy_if_enabled(&window);
                                }
                            }

                            *BACKEND.lock().unwrap() = Some(BackendState {
                                handle: Some(BackendHandle::InProcess(server)),
                                rt: Some(rt),
                            });
                        }
                        Err(e) => {
                            log::error!("oc-server embed startup failed: {:#}", e);
                            drop(rt);
                        }
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
            let app = window.app_handle();

            match event {
                // 主窗口关闭时触发后端清理。
                tauri::WindowEvent::Destroyed => {
                    if app.webview_windows().is_empty() {
                        shutdown_backend();
                    }
                }

                // macOS vibrancy flash 防护:
                // minimize → emit { ready: false } (UI 隐藏 vibrancy 依赖元素)
                // restore (Focused/Resumed) → delay 160ms → emit { ready: true }
                #[cfg(all(target_os = "macos", feature = "vibrancy"))]
                tauri::WindowEvent::Focused(focused) => {
                    use tauri::Emitter;
                    if *focused {
                        // 窗口重新获得焦点: 延迟通知 UI 恢复 vibrancy 效果
                        let app_clone = app.clone();
                        std::thread::spawn(move || {
                            std::thread::sleep(std::time::Duration::from_millis(160));
                            let _ = app_clone.emit(
                                "openchamber:emit",
                                serde_json::json!({
                                    "event": "openchamber:vibrancy-ready",
                                    "detail": { "ready": true }
                                }),
                            );
                        });
                    }
                }

                _ => {}
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|_app_handle, event| {
            if matches!(event, tauri::RunEvent::Exit) {
                // 停止托盘动画 (清理 tokio 任务)
                tray::destroy_tray_animation();
                // 清理 SSH 会话
                #[cfg(desktop)]
                {
                    ssh::shutdown_all(_app_handle);
                }
                shutdown_backend();
            }
        });
}

/// macOS vibrancy: 读 settings 判断是否启用 (默认开), 启用则 apply。
#[cfg(all(target_os = "macos", feature = "vibrancy"))]
fn apply_vibrancy_if_enabled(window: &tauri::WebviewWindow) {
    let vibrancy_enabled = settings::SettingsStore::get_bool(
        "desktopVibrancy",
        true,
    );
    if !vibrancy_enabled {
        return;
    }
    use window_vibrancy::{apply_vibrancy, NSVisualEffectMaterial, NSVisualEffectState};
    match apply_vibrancy(
        window,
        NSVisualEffectMaterial::Sidebar,
        Some(NSVisualEffectState::Active),
        None,
    ) {
        Ok(()) => log::info!("[vibrancy] applied sidebar material"),
        Err(e) => log::warn!("[vibrancy] failed to apply: {}", e),
    }
}

/// 同步清理后端: 从全局状态取出句柄, 在它的运行时上 block_on shutdown。
fn shutdown_backend() {
    let mut guard = BACKEND.lock().unwrap();
    if let Some(mut state) = guard.take() {
        if let (Some(rt), Some(handle)) = (state.rt.take(), state.handle.take()) {
            let _ = rt.block_on(async { handle.shutdown().await; });
        }
        drop(state);
    }
}

/// 公开的后端清理入口 (供 updater 模块在 on_before_exit 中调用)。
pub fn shutdown_backend_public() {
    shutdown_backend();
}
