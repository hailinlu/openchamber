//! GridForge 桌面壳 (Tauri)。
//!
//! 阶段 4A (sidecar 过渡态 + IPC 契约对等):
//! - Tauri 进程以子进程方式拉起 `@openchamber/web` CLI (`gridforge serve --foreground`)
//! - WebView 通过 loopback 加载 UI
//! - init_script 注入 `window.__GRIDFORGE_DESKTOP__` 桥 + 标量全局变量 (UI 零改动)
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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use backend::BackendHandle;
use ipc::globals::{build_early_globals_script, build_init_script, RuntimeContext};
use sidecar::SidecarBuilder;
use tauri::Manager;

/// 全局后端句柄 + 它专属的 tokio 运行时。
struct BackendState {
    handle: Option<BackendHandle>,
    rt: Option<tokio::runtime::Runtime>,
}

static BACKEND: Mutex<Option<BackendState>> = Mutex::new(None);

const BACKGROUND_START_ARG: &str = "--background";
static QUIT_REQUESTED: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClosePolicy {
    AllowClose,
    HideToTray,
}

fn close_policy(
    is_main_window: bool,
    minimize_to_tray: bool,
    quit_requested: bool,
) -> ClosePolicy {
    if is_main_window && minimize_to_tray && !quit_requested {
        ClosePolicy::HideToTray
    } else {
        ClosePolicy::AllowClose
    }
}

fn should_start_in_background<I, S>(args: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    args.into_iter()
        .any(|arg| arg.as_ref() == BACKGROUND_START_ARG)
}

#[derive(Debug, Default)]
struct MaximizeState {
    last: Option<bool>,
}

impl MaximizeState {
    fn transition(&mut self, next: bool) -> Option<bool> {
        if self.last == Some(next) {
            return None;
        }

        self.last = Some(next);
        Some(next)
    }
}

pub(crate) fn request_quit(app: &tauri::AppHandle) {
    QUIT_REQUESTED.store(true, Ordering::SeqCst);
    app.exit(0);
}

/// 配置主窗口 shell (必须在后端启动之前调用)。
///
/// 负责 platform-native 的窗口骨架: 图标 + Windows 无边框 + 后台启动时隐藏。
/// 不依赖后端 base_url,因此可以在 backend 启动前/失败时安全执行。
fn configure_main_window_shell(app: &tauri::AppHandle, background_start: bool) {
    let Some(window) = app.get_webview_window("main") else {
        log::warn!("main window not found during shell setup");
        return;
    };

    // 1. 窗口图标 — 用 tauri.conf.json bundle 配置的图标 (dev/build 都生效)
    if let Some(icon) = app.default_window_icon().cloned() {
        if let Err(error) = window.set_icon(icon) {
            log::warn!("failed to set main window icon: {}", error);
        }
    }

    // 2. Windows 关闭原生 chrome (使用 web title bar + 自绘窗口控制)
    #[cfg(target_os = "windows")]
    if let Err(error) = window.set_decorations(false) {
        log::warn!("failed to disable Windows window decorations: {}", error);
    }

    // 3. 注入静态全局变量 (提前注入, 不依赖后端端口)
    //
    // 设 `window.__GRIDFORGE_ELECTRON__` / `__GRIDFORGE_PLATFORM__`。
    // 这些值在编译期即确定, 不等后端启动: 确保 React hydration 时
    // `isElectronShell()` / `usesFramelessElectronChrome()` 能正确检测。
    //
    // 同时注入 `__GRIDFORGE_API_BASE_URL__` 和 `__GRIDFORGE_LOCAL_ORIGIN__`
    // (基于 GRIDFORGE_PORT env), 以防页面加载快于后端启动导致 WS 请求
    // 走相对路径经 Vite proxy 转发时 ECONNRESET。
    // 后端启动后 `inject_main_window_runtime` 会用实际端口覆盖。
    //
    // `window.eval()` 在 Tauri 2 的 setup 阶段可能因页面未加载而失败,
    // 但 UI 侧有 `isTauriShell()` 回退 (`window.__TAURI__`), 此处为
    // belt-and-suspenders 方案。
    let early_script = build_early_globals_script();
    if let Err(error) = window.eval(&early_script) {
        log::warn!(
            "failed to inject early globals (will rely on UI fallback): {}",
            error,
        );
    }

    // 4. 后台启动 → 立即隐藏 (托盘激活后用 Show GridForge 恢复)
    if background_start {
        if let Err(error) = window.hide() {
            log::warn!("failed to hide main window for background launch: {}", error);
        }
    }
}

/// 向主窗口注入运行时桥 (后端就绪后调用)。
///
/// 仅负责依赖后端 base_url / 端口的运行时脚本注入,以及 macOS vibrancy 应用。
fn inject_main_window_runtime(app: &tauri::AppHandle, init_script: &str) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };

    // 1. 注入 init_script (后端 base_url / 桥全局变量)
    if let Err(error) = window.eval(init_script) {
        log::warn!("failed to inject main window runtime: {}", error);
    }

    // 2. macOS vibrancy (可选, 默认开)
    #[cfg(all(target_os = "macos", feature = "vibrancy"))]
    {
        apply_vibrancy_if_enabled(&window);
    }
}

/// 获取后端 base_url (供 IPC 命令 HTTP 调用后端端点)。
///
/// 返回 `http://127.0.0.1:<port>`，后端未启动时返回 None。
/// 用例: `dialog_cmd::gridforge_file_grant` 调 `POST /api/fs/grant`。
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

            // --- 配置主窗口 shell (在后端启动之前) ---
            // 不依赖后端 base_url/端口: 图标 + Windows 无边框 + 后台启动时立即隐藏。
            // 这样后台启动时窗口不会先闪一下再消失,且 Windows chrome 与后端就绪解耦。
            let background_start = should_start_in_background(std::env::args());
            configure_main_window_shell(app.handle(), background_start);

            // --- 创建系统托盘 (在后端启动之前) ---
            // 必须在 `SidecarBuilder::start()` / `OcServer::start()` 之前调用,
            // 这样后台启动或冷启动时托盘立即可见, 用户可在 UI hydration 完成前
            // 通过托盘菜单 (New Session / Show GridForge / Quit) 操作应用。
            if let Err(error) = tray::setup_tray(app.handle()) {
                log::error!("failed to create tray during setup: {}", error);
            }

            // --- 设置应用数据目录 ---
            // 仅 production 模式覆盖为 macOS 标准 userData 位置
            // (~/Library/Application Support/GridForge) 对齐 Electron。
            // Dev 模式 (cargo tauri dev) **不**覆盖, 让 Rust oc-server 走默认的
            // ~/.config/gridforge/ 路径 —— 与 Node 模式同位置, 用户已有的
            // projects 能直接被 Tauri dev 读到, 不会每次启动都弹"添加项目"对话框。
            // 用户仍可通过显式设置 GRIDFORGE_DATA_DIR 覆盖。
            if !cfg!(debug_assertions)
                && !std::env::var("GRIDFORGE_DATA_DIR").is_ok_and(|v| !v.trim().is_empty())
            {
                let dir_name = "GridForge";
                // 借用 Tauri 的 app_data_dir 父目录 (平台自适应:
                //   macOS:   ~/Library/Application Support
                //   Windows: %APPDATA%
                //   Linux:   ~/.local/share
                // ), 但用 GridForge 作为目录名而非 bundle identifier。
                let dir = app.path().app_data_dir()
                    .ok()
                    .and_then(|p| p.parent().map(|parent| parent.join(dir_name)))
                    .unwrap_or_else(|| {
                        // fallback: 不应发生, 仅兜底
                        let home = std::env::var("HOME")
                            .or_else(|_| std::env::var("USERPROFILE"))
                            .unwrap_or_else(|_| "/tmp".to_string());
                        std::path::PathBuf::from(home)
                            .join(if cfg!(target_os = "macos") {
                                "Library/Application Support"
                            } else if cfg!(target_os = "windows") {
                                "AppData/Roaming"
                            } else {
                                ".local/share"
                            })
                            .join(dir_name)
                    });
                if let Err(e) = std::fs::create_dir_all(&dir) {
                    log::error!("failed to create app data dir {:?}: {}", dir, e);
                } else {
                    let dir_str = dir.to_string_lossy().to_string();
                    log::info!("setting GRIDFORGE_DATA_DIR={}", dir_str);
                    std::env::set_var("GRIDFORGE_DATA_DIR", &dir_str);
                }
            }

            // --- 启动后端 (仅桌面端) ---
            #[cfg(desktop)]
            {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()?;

                if backend::use_sidecar() {
                    // —— 回退路径: sidecar 子进程 ——
                    let mut builder = SidecarBuilder::new()
                        .ready_timeout(std::time::Duration::from_secs(45))
                        .arg("--api-only");
                    // Dev 模式: 如果 GRIDFORGE_PORT 已设置，使用固定端口确保
                    // Vite early injection、proxy 和 sidecar 使用同一个端口。
                    if cfg!(debug_assertions) {
                        if let Ok(port_str) = std::env::var("GRIDFORGE_PORT") {
                            if let Ok(port) = port_str.parse::<u16>() {
                                builder = builder.port(port);
                            }
                        }
                    }
                    let handle = rt.block_on(async {
                        builder.start().await
                    });
                    match handle {
                        Ok(h) => {
                            let port = h.port();
                            log::info!("sidecar ready on port {}", port);
                            mini_chat::set_backend_port(app.handle(), port);

                            let ctx = RuntimeContext::from_sidecar_port(port);
                            let init_script = build_init_script(&ctx);
                            inject_main_window_runtime(app.handle(), &init_script);

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
                            mini_chat::set_backend_port(app.handle(), port);

                            let ctx = RuntimeContext::from_sidecar_port(port);
                            let init_script = build_init_script(&ctx);
                            inject_main_window_runtime(app.handle(), &init_script);

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
            ipc::gridforge_invoke,
            ipc::dialog_cmd::gridforge_dialog_open,
            ipc::dialog_cmd::gridforge_file_grant,
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
                tauri::WindowEvent::CloseRequested { api, .. } => {
                    let policy = close_policy(
                        window.label() == "main",
                        settings::SettingsStore::get_bool(
                            "desktopMinimizeToTrayEnabled",
                            false,
                        ),
                        QUIT_REQUESTED.load(Ordering::SeqCst),
                    );

                    if policy == ClosePolicy::HideToTray {
                        match window.hide() {
                            Ok(()) => api.prevent_close(),
                            Err(error) => log::error!(
                                "failed to hide main window during close-to-tray: {}",
                                error,
                            ),
                        }
                    }
                }

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
                                "gridforge:emit",
                                serde_json::json!({
                                    "event": "gridforge:vibrancy-ready",
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
                QUIT_REQUESTED.store(true, Ordering::SeqCst);
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

    #[test]
    fn main_window_close_hides_when_enabled() {
        assert_eq!(
            close_policy(true, true, false),
            ClosePolicy::HideToTray,
        );
    }

    #[test]
    fn disabled_minimize_to_tray_allows_close() {
        assert_eq!(
            close_policy(true, false, false),
            ClosePolicy::AllowClose,
        );
    }

    #[test]
    fn explicit_quit_bypasses_close_to_tray() {
        assert_eq!(
            close_policy(true, true, true),
            ClosePolicy::AllowClose,
        );
    }

    #[test]
    fn non_main_windows_are_not_intercepted() {
        assert_eq!(
            close_policy(false, true, false),
            ClosePolicy::AllowClose,
        );
    }

    #[test]
    fn detects_exact_background_argument() {
        assert!(should_start_in_background([
            "GridForge.exe",
            "--background",
        ]));
    }

    #[test]
    fn ignores_unrelated_or_prefixed_background_arguments() {
        assert!(!should_start_in_background([
            "GridForge.exe",
            "--some-other-flag",
        ]));
        assert!(!should_start_in_background([
            "GridForge.exe",
            "--background=true",
        ]));
    }

    #[test]
    fn maximize_state_emits_initial_and_changed_values_only() {
        let mut state = MaximizeState::default();

        assert_eq!(state.transition(false), Some(false));
        assert_eq!(state.transition(false), None);
        assert_eq!(state.transition(true), Some(true));
        assert_eq!(state.transition(true), None);
        assert_eq!(state.transition(false), Some(false));
    }
}
