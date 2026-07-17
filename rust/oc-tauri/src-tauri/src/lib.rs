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

            // --- SIGTERM/SIGINT 处理 (Unix) ---
            // 覆盖路径 A: OS 发 SIGTERM/SIGINT 时, 内核默认处置 = 立即终止进程,
            // 不展开栈、不跑 Drop、不触发任何 RunEvent。子进程会 reparent 到 init → 孤儿。
            // 这里装一个显式 handler: 在专用 current-thread rt 上 recv 信号,
            // 收到后在【独立线程】调 shutdown_backend_public() 完成 cleanup。
            //
            // 为什么不能在 rt.block_on 里直接调 shutdown_backend_public():
            //   shutdown_backend 内部 backend_rt.block_on(...) 会嵌套 runtime
            //   → "Cannot start a runtime from within a runtime" panic。
            //   所以 cleanup 必须脱离 signal-rt 上下文, 在裸线程上执行。
            //
            // shutdown_backend 幂等: 若 ExitRequested/Destroyed 已清理, 此处 no-op。
            #[cfg(unix)]
            {
                let app_handle = app.handle().clone();
                std::thread::spawn(move || {
                    // current_thread rt 只用于 signal recv (轻量)。
                    let rt = match tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                    {
                        Ok(rt) => rt,
                        Err(e) => {
                            log::error!("failed to build signal-handler runtime: {}", e);
                            return;
                        }
                    };
                    // 用 LocalSet 驱动 signal stream。
                    let local = tokio::task::LocalSet::new();
                    local.block_on(&rt, async {
                        let sigterm = tokio::signal::unix::signal(
                            tokio::signal::unix::SignalKind::terminate(),
                        );
                        let sigint = tokio::signal::unix::signal(
                            tokio::signal::unix::SignalKind::interrupt(),
                        );
                        let (mut sigterm, mut sigint) = match (sigterm, sigint) {
                            (Ok(t), Ok(i)) => (t, i),
                            (Err(e), _) | (_, Err(e)) => {
                                log::error!("failed to install signal handler: {}", e);
                                return;
                            }
                        };
                        tokio::select! {
                            _ = sigterm.recv() => log::info!("received SIGTERM, shutting down backend"),
                            _ = sigint.recv() => log::info!("received SIGINT, shutting down backend"),
                        }
                    });
                    // rt 已停止 (block_on 返回)。在【裸线程上下文】调 cleanup,
                    // 避免 backend_rt.block_on 嵌套 panic。
                    shutdown_backend_public();
                    // 走 Tauri 正常退出 (触发 ExitRequested → tray/ssh 清理)。
                    app_handle.exit(0);
                });
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
        .run(|app_handle, event| {
            // 在 ExitRequested (而非 Exit) 做清理: ExitRequested 在进程真正退出前同步触发,
            // wry 事件循环会等本回调返回后再决定是否退出 (tauri-runtime-wry lib.rs:4316-4322),
            // 因此 rt.block_on(handle.shutdown()) 能完整跑完。
            // 而 RunEvent::Exit 在平台 runtime 调 std::process::exit 前一刻触发, async 清理会被截断。
            if let tauri::RunEvent::ExitRequested { .. } = event {
                // 停止托盘动画 (清理 tokio 任务)
                tray::destroy_tray_animation();
                // 清理 SSH 会话
                #[cfg(desktop)]
                {
                    ssh::shutdown_all(app_handle);
                }
                // shutdown_backend 幂等: 若 WindowEvent::Destroyed 或 signal handler 已清理, 此处 no-op。
                shutdown_backend();
                // 不调 api.prevent_exit() — 清理已完成, 放行正常退出。
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
///
/// 幂等: `guard.take()` 保证多次调用安全 (第二次拿不到 state → no-op)。
/// 这对 signal handler + ExitRequested + WindowEvent::Destroyed 三处都调本函数
/// 至关重要 — 先到的完成清理, 后到的 no-op。
///
/// poison 容忍: 用 `unwrap_or_else(into_inner)` 而非 `unwrap()`, 避免
/// 持锁线程 panic 后 signal handler 再调时二次 panic (signal handler panic 是 UB)。
fn shutdown_backend() {
    let mut guard = BACKEND.lock().unwrap_or_else(|e| e.into_inner());
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `shutdown_backend` 幂等性: BACKEND 为 None (未设置或已清理) 时调用不 panic。
    ///
    /// 这对三处调用点的安全至关重要:
    /// - WindowEvent::Destroyed (最后一个窗口销毁)
    /// - RunEvent::ExitRequested (退出前)
    /// - SIGTERM/SIGINT handler (信号到达)
    /// 先到的完成清理 (take 走 state), 后到的进入此分支 no-op。
    #[test]
    fn shutdown_backend_noop_when_none() {
        // 确保 BACKEND 为空 (测试间隔离)。
        {
            let mut guard = BACKEND.lock().unwrap();
            *guard = None;
        }
        // 多次调用都应安全。
        shutdown_backend();
        shutdown_backend();
        shutdown_backend();
    }

    /// poison 容忍: 即使 Mutex 被 poison (模拟持锁线程 panic),
    /// shutdown_backend 仍能通过 into_inner 取到数据不二次 panic。
    #[test]
    fn shutdown_backend_tolerates_poisoned_mutex() {
        // 注入一个 poison: lock 后主动 panic 让它 poison。
        // 用单独线程, panic 被 catch_unwind 吞掉, 但 poison 已注入。
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _guard = BACKEND.lock().unwrap();
            // 先重置为干净状态 (None), 这样后续 into_inner 拿到的也是 None。
            // guard drop 时会 poison (因为线程 panic)。
            tx.send(()).unwrap();
            panic!("intentional poison for test");
        })
        .join()
        .ok();
        let _ = rx.recv();
        // 此时 Mutex 已 poison。shutdown_backend 必须 tolerate。
        shutdown_backend(); // 不 panic 即通过
    }
}
