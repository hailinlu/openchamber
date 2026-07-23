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
mod opencode_discovery;
mod power;
mod settings;
mod sidecar;
mod ssh;
mod tray;
mod updater;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use backend::BackendHandle;
use ipc::globals::{build_init_script, RuntimeContext};
use sidecar::SidecarBuilder;
use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

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

// =========================================================================
// 同源化 Tauri 生产 webview: oc-server 托管 UI dist
// =========================================================================
//
// 根因: `WebviewUrl::App("index.html")` 让 page origin = `tauri.localhost`
// (Win/Linux) 或 `tauri://localhost` (macOS), 与 oc-server 的
// `http://127.0.0.1:<port>` 不一致 → cross-origin → CORS 失败 / 渲染
// 异常 (AGENTS.md "Frequently-misdiagnosed runtime issues")。
//
// 修复: 生产构建走 `WebviewUrl::External(<loopback>)`, 让
// `location.origin === __GRIDFORGE_API_BASE_URL__`, 同源 fetch 直通。
// Dev 保持 `WebviewUrl::App` (Vite :5180 + proxy 处理跨源)。
//
// 相关 env: `GRIDFORGE_DIST_DIR` — oc-server 用来定位要托管的 ui-dist。
// 仅在调用方未设置 (env unset 或空) 且默认路径 <CARGO_MANIFEST_DIR>/../ui-dist
// 存在时, oc-tauri 才注入这个 env, 给 sidecar 走 `--dist-dir`, 给 in-process
// 走 `clap` 的 env (oc-server::Config::load 自动读)。

/// 主窗口 URL 决策结果 (dev/release 走不同路径)。
#[derive(Debug, Clone, PartialEq, Eq)]
enum MainUrlDecision {
    /// Dev: `WebviewUrl::App("index.html")` → Tauri 走 devUrl (Vite)。
    AppIndexHtml,
    /// Release: `WebviewUrl::External(<loopback URL>)` → oc-server 托管 UI。
    ExternalLoopback(String),
}

/// 纯函数: 根据 debug_assertions + 当前后端 port 决定主窗口 URL 策略。
///
/// - `debug = true`  → App 路径 (devUrl, Vite + proxy 处理 cross-origin)。
/// - `debug = false` + `port > 0` → External loopback URL, 携带真实
///   port + trailing slash, 确保 page origin === __GRIDFORGE_API_BASE_URL__。
/// - `debug = false` + `port == 0` → App 路径。后端启动失败 / 不可达时
///   [`ctx_suggested_port`] 返回 0 (无 api_base_url, local_origin 是占位);
///   这种情况下绝对不能合成 `External("http://127.0.0.1:0/")` ——
///   port 0 是保留值, Webview 拿到的是无效 origin + 让 bundled UI
///   去取一个不存在的资源。让 Tauri 走 frontendDist (`WebviewUrl::App`)
///   即可, page origin 变成 `tauri.localhost` 但恢复屏不依赖 API base
///   URL, 渲染逻辑走 `local_origin`-占位 + `boot_status=unreachable`
///   这条路。
fn decide_main_webview_url(debug_assertions: bool, port: u16) -> MainUrlDecision {
    if debug_assertions {
        return MainUrlDecision::AppIndexHtml;
    }
    if port == 0 {
        // 后端不可达 / 端口未知 — 用 App 让 bundled UI 渲染恢复屏,
        // 而不是用一个 External 127.0.0.1:0 (无效) 或 tauri:// External 兜底
        // (后者会让 `__GRIDFORGE_LOCAL_ORIGIN__` 与 page origin 错位)。
        MainUrlDecision::AppIndexHtml
    } else {
        // trailing slash 让 Webview 把 "/" 当成目录而非"host 边界", 浏览器
        // 解析出的 location.origin 是 "http://127.0.0.1:<port>" (无 path),
        // 与 __GRIDFORGE_API_BASE_URL__ 严格相等 → 同源。
        MainUrlDecision::ExternalLoopback(format!("http://127.0.0.1:{}/", port))
    }
}

/// dist-dir 决策结果。
///
/// 三态决策: env 已设置 (无论空与非空) 一律保留, 仅在 env 缺席时决定是否
/// 计算/注入默认值。
#[derive(Debug, Clone, PartialEq, Eq)]
enum DistDirDecision {
    /// env 已设置 (非空) → 保留现状, 调用方**不**调用 `set_var`。
    Use(std::path::PathBuf),
    /// env 已设置 (空字符串) → 保留现状 (不覆盖), 调用方**不**调用 `set_var`。
    /// 不管 candidate 路径是否存在, 都尊重用户的显式空设置。
    PreserveEmpty,
    /// env 缺席, candidate 存在 → 调用方应 `set_var("GRIDFORGE_DIST_DIR", candidate)`。
    SetDefault(std::path::PathBuf),
    /// env 缺席, candidate 不存在 → 调用方 log warn, 不动 env。
    Skip,
}

/// 纯函数: 决定 `GRIDFORGE_DIST_DIR` 应该如何被处理。
///
/// 合约:
/// - `env_override == Some(non-empty)` → `Use(env)` (尊重既有值, 不覆盖)。
/// - `env_override == Some(empty)` → `PreserveEmpty` (尊重显式空设置, 不覆盖;
///   与"env 未设置"是两种不同概念, 不应混淆)。
/// - `env_override == None, candidate 存在` → `SetDefault(candidate)`
///   (env 完全缺席时计算默认并注入)。
/// - `env_override == None, candidate 不存在` → `Skip`
///   (env 缺席 + 默认路径不存在 → 不动, 让失败可观察)。
///
/// `candidate_exists` 参数化让本函数可测试 (避免在单测里创建/删除临时目录)。
///
/// `env_override` 接受 `Option<&OsStr>`:
/// - `env::var_os(...).as_deref()` 直接喂入 (生产路径);
/// - 单元测试可以用 `OsString::from(...)` / `&str` / `OsString::new()` 等统一接入。
fn resolve_dist_dir_decision(
    env_override: Option<&std::ffi::OsStr>,
    candidate: &std::path::Path,
    candidate_exists: bool,
) -> DistDirDecision {
    // env 已设置 (Some, 不论空与非空): 优先尊重用户/外层已设置的值,
    // 不覆盖 — 包括用户显式把 GRIDFORGE_DIST_DIR 设成空字符串的情况。
    if let Some(raw) = env_override {
        if raw.is_empty() {
            return DistDirDecision::PreserveEmpty;
        }
        return DistDirDecision::Use(std::path::PathBuf::from(raw));
    }
    // env 完全缺席 (None): 才考虑用候选默认值。
    if candidate_exists {
        return DistDirDecision::SetDefault(candidate.to_path_buf());
    }
    DistDirDecision::Skip
}

/// 纯函数: 默认 ui-dist 路径, 锚定 `CARGO_MANIFEST_DIR` 以保证 cwd 无关。
fn default_ui_dist_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("ui-dist")
}

/// 纯函数: 给 sidecar 构造 `--dist-dir <path>` 参数对 (或空)。
///
/// `dist_dir` 为 None 时返回空 Vec, 调用方应直接跳过 arg 注入。
fn build_dist_dir_args(dist_dir: Option<&std::path::Path>) -> Vec<String> {
    match dist_dir {
        Some(p) => match p.to_str() {
            Some(s) => vec!["--dist-dir".to_string(), s.to_string()],
            None => {
                // 非 UTF-8 路径: 退化为空 — 调用方应记录诊断, 避免
                // 静默丢参。生产路径几乎不会触发 (Windows UTF-16 / Unix
                // bytes 路径), 仅在测试覆盖率外保留 fail-safe。
                Vec::new()
            }
        },
        None => Vec::new(),
    }
}

/// 注入 `GRIDFORGE_DIST_DIR` env (在 sidecar 与 in-process oc-server 启动之前)。
///
/// 规则:
/// - 已设置 (非空) → 尊重, 不动; log info 说明用的什么值。
/// - 已设置 (空字符串) → 尊重显式空设置, 不动; log info 提示用户显式置空。
/// - 完全未设置 + `<CARGO_MANIFEST_DIR>/../ui-dist` 存在 → 注入, log info。
/// - 完全未设置 + 候选路径不存在 → Skip, log warn (让失败可观察)。
///
/// 严格尊重"显式 env": 用户把 `GRIDFORGE_DIST_DIR` 设成什么就是什么,
/// 包括空串。**只**在 env 完全缺席 (var_os 返回 None) 时才计算/注入默认。
fn set_oc_server_dist_dir_env() {
    let env_value = std::env::var_os("GRIDFORGE_DIST_DIR");
    let candidate = default_ui_dist_path();
    let candidate_exists = candidate.is_dir();
    let decision = resolve_dist_dir_decision(env_value.as_deref(), &candidate, candidate_exists);
    match decision {
        DistDirDecision::Use(existing) => {
            // env 已设置 (非空): 尊重, 不动; log info 说明用的什么值。
            log::info!(
                "GRIDFORGE_DIST_DIR already set to {:?}, using override",
                existing
            );
        }
        DistDirDecision::PreserveEmpty => {
            // env 显式置空: 尊重, 不动 (与 env 缺席语义不同, 不要混淆)。
            // 不把空字符串当作"未设置"然后自动覆盖成 candidate — 那会破坏
            // 用户用 `GRIDFORGE_DIST_DIR="" ./gridforge` 显式禁用 ui 托管的用法。
            log::info!(
                "GRIDFORGE_DIST_DIR explicitly set to empty; preserving (not overwriting with default)"
            );
        }
        DistDirDecision::SetDefault(path) => {
            // env 完全缺席: 计算默认值并注入。
            let path_str = path.to_string_lossy().to_string();
            log::info!("setting GRIDFORGE_DIST_DIR={}", path_str);
            std::env::set_var("GRIDFORGE_DIST_DIR", &path_str);
        }
        DistDirDecision::Skip => {
            log::warn!(
                "GRIDFORGE_DIST_DIR not set and {} is not a directory; UI will fail to load in prod",
                candidate.display()
            );
        }
    }
}

/// 创建主窗口 (代码建窗, **必须**在后端就绪、`BackendPort` 注册后调用)。
///
/// 复刻 `mini_chat::create_mini_chat_window` 的注入模式: 拿到真实端口后,
/// 用 `build_init_script(ctx)` (静态全局 + 端口相关全局 + 桥) 作为
/// `initialization_script`。`initialization_script` 在页面 JS 执行前、
/// 每次导航前运行, 因此 `main.tsx` / `runtimeConfig.ts` 同步读取
/// `__GRIDFORGE_API_BASE_URL__` 时一定拿到正确值 —— 彻底消除 prod 的
/// 首次加载竞态 (旧实现窗口建在后端启动前, 首次加载时端口未知 → 读空 →
/// 回退到 `tauri.localhost` → 所有请求返回 HTML → `<!doctype` JSON 报错)。
///
/// `ctx` 由调用方在后端就绪后构造 (含真实端口 + client token + runtime headers)。
fn create_main_window(app: &tauri::AppHandle, ctx: &RuntimeContext, background_start: bool) {
    // 同源化 webview URL: dev 走 WebviewUrl::App (Vite devUrl + proxy),
    // prod 走 WebviewUrl::External(loopback), 让 page origin 一致 —
    // 消除 tauri.localhost / 127.0.0.1 跨源 CORS 失败。
    //
    // 注: 后端不可达时 (port == 0) 走 AppIndexHtml 让 bundled UI 渲染
    // 恢复屏, 而不是合出一个 `External("http://127.0.0.1:0/")`。
    // 同样地, 这里**不**再有 `tauri://localhost` External 兜底:
    // 那会让 page origin 与 `__GRIDFORGE_LOCAL_ORIGIN__` 错位 (后者是
    // `http://tauri.localhost`), 比纯 App 路径更糟。
    let url = match decide_main_webview_url(cfg!(debug_assertions), ctx_suggested_port(ctx)) {
        MainUrlDecision::AppIndexHtml => WebviewUrl::App("index.html".into()),
        MainUrlDecision::ExternalLoopback(s) => {
            // 此分支仅在 port > 0 时进入, URL 形如 `http://127.0.0.1:<port>/`,
            // 是绝对标准 ASCII URL, url::Url::parse 在生产路径不会失败。
            // 这里**不**再回落到 tauri:// External 兜底: 那会让 page origin
            // 与 `__GRIDFORGE_LOCAL_ORIGIN__` (= http://tauri.localhost)
            // 错位, 把恢复屏逻辑搞坏。spec review 已要求移除误导性兜底,
            // 改用 expect 在实测到 url crate 行为异常时给出明确失败位置。
            let parsed = url::Url::parse(&s).unwrap_or_else(|e| {
                // 旧 fallback (`tauri://localhost/`) 让 webview 把这个
                // URL 当 External 处理, 拿到的是 tauri://localhost origin,
                // 与 initial_script 注入的 __GRIDFORGE_LOCAL_ORIGIN__
                // (= http://tauri.localhost) 不一致, 反把恢复屏搞坏。
                // panic 在 unwrap_or_else 里走 div 路径, 等价于 expect。
                panic!(
                    "main-window URL {:?} failed to parse (tauri:// External \
                     fallback removed by spec review): {}",
                    s, e
                );
            });
            WebviewUrl::External(parsed)
        }
    };

    let mut builder = WebviewWindowBuilder::new(app, "main", url)
        .title("GridForge")
        .inner_size(1280.0, 800.0)
        .min_inner_size(720.0, 480.0)
        .resizable(true)
        // 后台启动时不显示窗口 (托盘激活后用 Show GridForge 恢复)。
        // 比"显示后隐藏"更干净, 消除背景启动闪窗。
        .visible(!background_start)
        // 完整 init script: 静态全局变量 + 端口相关全局变量 (含真实端口) + 桥。
        // initialization_script 每次导航前运行, 首次加载与 reload 都能拿到正确 base URL。
        .initialization_script(&build_init_script(ctx));

    // Windows/Linux 关闭原生 chrome (使用 web title bar + 自绘窗口控制)。
    // macOS 保持原生 frame。
    #[cfg(not(target_os = "macos"))]
    {
        builder = builder.decorations(false);
    }

    let window = match builder.build() {
        Ok(window) => window,
        Err(error) => {
            log::error!("failed to create main window: {}", error);
            return;
        }
    };

    // macOS vibrancy (可选, 默认开)。
    // `window` 仅在 macOS+vibrancy 下被引用, 其它平台保留绑定避免编译错误。
    #[cfg(all(target_os = "macos", feature = "vibrancy"))]
    {
        apply_vibrancy_if_enabled(&window);
    }
    #[cfg(not(all(target_os = "macos", feature = "vibrancy")))]
    {
        let _ = &window;
    }
}

/// 构造主窗口的 `RuntimeContext` (后端就绪后调用)。
///
/// 复刻 `mini_chat::create_mini_chat_window`: 基于真实端口构造 ctx,
/// 并从 `SettingsStore` 读 `desktopLocalClientToken` / `desktopRuntimeHeaders`
/// (UI 认证 HTTP API 请求需要), 使主窗口与 mini chat 窗口的注入对齐。
/// `home_directory` / `relay_host_id` 在主窗口暂不需要, 保持 None。
fn build_main_window_ctx(port: u16) -> RuntimeContext {
    let client_token = settings::SettingsStore::get("desktopLocalClientToken")
        .and_then(|v| v.as_str().map(|s| s.to_string()));
    let runtime_headers = settings::SettingsStore::get("desktopRuntimeHeaders")
        .and_then(|v| if v.is_object() { Some(v.clone()) } else { None });
    let mut ctx = RuntimeContext::from_sidecar_port(port);
    ctx.client_token = client_token;
    ctx.runtime_headers = runtime_headers;
    ctx
}

/// 从 `RuntimeContext` 提取建议的 loopback 端口。
///
/// - Ok 后端: `api_base_url` (或 `local_origin`) 形式为 `http://127.0.0.1:<port>`,
///   用 [`backend::parse_port`] 提取。
/// - Err 后端: `api_base_url` 为 None, `local_origin` 是 `tauri.localhost` 占位。
///   这种情况下没有真实端口, 退回到 0; `decide_main_webview_url` 看到 port=0
///   会主动选 `WebviewUrl::App("index.html")`, 让 bundled UI 渲染 unreachable
///   恢复屏, **不**再合成无意义的 `External("http://127.0.0.1:0/")` 或
///   误导性的 `External("tauri://localhost/")`。
fn ctx_suggested_port(ctx: &RuntimeContext) -> u16 {
    if let Some(ref url) = ctx.api_base_url {
        return backend::parse_port(url);
    }
    // 无 api_base_url 时尝试 local_origin (Ok 路径也是这个值; Err 路径是占位)。
    // parse_port 内部用 expect, Err 路径会 panic, 这里改用 try 版本避免炸。
    if let Some(pos) = ctx.local_origin.rfind(':') {
        let after = &ctx.local_origin[pos + 1..];
        if let Some(stripped) = after.strip_suffix('/') {
            if let Ok(p) = stripped.parse::<u16>() {
                return p;
            }
        }
        if let Ok(p) = after.parse::<u16>() {
            return p;
        }
    }
    0
}

/// 后端启动失败时, Err 恢复窗注入的 `local_origin` 占位值。
///
/// 生产 webview 的页面 origin 在 Windows/Linux 是 `http://tauri.localhost`,
/// macOS 是 `tauri://localhost`。Tauri 代码层拿不到 webview 的运行时 origin,
/// 这里按平台返回固定占位。`isDesktopLocalOriginActive` (`desktop.ts:508-510`)
/// 在"无 api_base_url + local_origin 非空 + bootOutcome.target==='local'"时
/// 返回 true —— 只要这个值非空, UI 就会渲染 local-unavailable 恢复屏
/// 而非 restart 循环。精确匹配 page origin 不是必需的。
fn unreachable_local_origin() -> String {
    if cfg!(target_os = "macos") {
        "tauri://localhost".to_string()
    } else {
        "http://tauri.localhost".to_string()
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
            // 文件日志: 生产也需启用。后端 spawn 失败的诊断信息唯一出口 ——
            // `log::error!("oc-server embed startup failed: {:#}", e)` 等若被
            // release 排除, 用户将无法定位后端为何起不来。
            // tauri-plugin-log 2.9.0 默认 targets = [Stdout, LogDir],
            // Windows 写到 C:\Users\{user}\AppData\Local\com.gridforge.desktop\logs。
            app.handle().plugin(
                tauri_plugin_log::Builder::default()
                    .level(log::LevelFilter::Info)
                    .build(),
            )?;

            // 后台启动标志: 托盘/窗口隐藏判断需要, 在后端启动前计算一次。
            // 主窗口本身推迟到后端就绪后创建 (见下方 sidecar / in-process 分支),
            // 这样 initialization_script 能内嵌真实端口, 消除首次加载竞态。
            let background_start = should_start_in_background(std::env::args());

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

            // --- 探测 opencode 二进制 (仅桌面端, 后端启动之前) ---
            // 打包运行时不继承 dev 启动器设的 OPENCODE_BINARY。Windows 上
            // `Command::new("opencode")` 不可靠地解析 .cmd shim, 导致 spawn
            // 失败 → "Local OpenCode Unavailable"。这里复刻 tauri-dev.mjs 的
            // resolveOpencodeBinary, 把探测到的真实路径写进 OPENCODE_BINARY。
            // 用户显式设置时完全尊重, 不覆盖。
            #[cfg(desktop)]
            {
                opencode_discovery::resolve_and_export();
            }

            // --- 启动后端 (仅桌面端) ---
            #[cfg(desktop)]
            {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()?;

                if backend::use_sidecar() {
                    // —— 回退路径: sidecar 子进程 ——
                    // 同源化生产构建: 让 oc-server 托管 ui-dist, 让 webview 访问
                    // `http://127.0.0.1:<port>/` (同源) 而非 tauri.localhost (跨源)。
                    // 必须在 builder.start() 之前注入 env, 否则 sidecar CLI 看不到。
                    set_oc_server_dist_dir_env();

                    let mut builder = SidecarBuilder::new()
                        .ready_timeout(std::time::Duration::from_secs(45));
                    // --dist-dir <path>: 让 sidecar 在生产时也能 serve UI 资源;
                    // 只在 env 既有可用的非空值时才加 (env 不存在 / 空 / NonUtf8
                    // 都跳过)。`env::var_os` 必须在 set_oc_server_dist_dir_env
                    // **之后**读取, 这样若该函数注入了默认路径, 此处也能拿到。
                    // 注意: set_var 后 OS env 已被更新 (Even if it was a SetDefault),
                    // 所以下面这一行就是实际传给 sidecar 的 dist-dir 值。
                    for arg in build_dist_dir_args(
                        std::env::var_os("GRIDFORGE_DIST_DIR")
                            .as_ref()
                            // 空字符串视为"未设置" (不传给 sidecar, 避免空参数)
                            // — 用户的"显式禁用 ui 托管"靠 env 缺席/空实现,
                            // 不需要 sidecar 看到路径。
                            .filter(|s| !s.is_empty())
                            .map(|s| std::path::PathBuf::from(s))
                            .as_deref(),
                    ) {
                        builder = builder.arg(arg);
                    }
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

                            // 后端就绪 → 创建主窗口。
                            // initialization_script 内嵌真实端口, 消除首次加载竞态。
                            let ctx = build_main_window_ctx(port);
                            create_main_window(app.handle(), &ctx, background_start);

                            *BACKEND.lock().unwrap() = Some(BackendState {
                                handle: Some(BackendHandle::Sidecar(h)),
                                rt: Some(rt),
                            });
                        }
                        Err(e) => {
                            log::error!("sidecar startup failed: {:#}", e);
                            drop(rt);
                            // 后端失败也要建窗: 注入 unreachable boot outcome,
                            // UI 据此渲染 local-unavailable 恢复屏 (而非只起托盘)。
                            // 不注入 api_base_url (无后端, 不谎报地址)。
                            // 把 anyhow 完整因果链作为 diagnostic 注入, 恢复屏
                            // 可展开显示, 让用户/开发者看到真实失败原因。
                            let ctx = RuntimeContext::for_unreachable_backend(
                                unreachable_local_origin(),
                                Some(format!("{:#}", e)),
                            );
                            create_main_window(app.handle(), &ctx, background_start);
                        }
                    }
                } else {
                    // —— 新路径: 进程内嵌入 oc-server ——
                    // 同源化: 在 oc-server 启动前注入 GRIDFORGE_DIST_DIR,
                    // 让进程内 oc-server 也能托管 UI (clap #[arg(env = ...)] 自动读)。
                    set_oc_server_dist_dir_env();

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

                            // 后端就绪 → 创建主窗口。
                            // initialization_script 内嵌真实端口, 消除首次加载竞态。
                            let ctx = build_main_window_ctx(port);
                            create_main_window(app.handle(), &ctx, background_start);

                            *BACKEND.lock().unwrap() = Some(BackendState {
                                handle: Some(BackendHandle::InProcess(server)),
                                rt: Some(rt),
                            });
                        }
                        Err(e) => {
                            log::error!("oc-server embed startup failed: {:#}", e);
                            drop(rt);
                            // 后端失败也要建窗: 注入 unreachable boot outcome,
                            // UI 据此渲染 local-unavailable 恢复屏 (而非只起托盘)。
                            // 不注入 api_base_url (无后端, 不谎报地址)。
                            // 把 anyhow 完整因果链作为 diagnostic 注入, 恢复屏
                            // 可展开显示, 让用户/开发者看到真实失败原因。
                            let ctx = RuntimeContext::for_unreachable_backend(
                                unreachable_local_origin(),
                                Some(format!("{:#}", e)),
                            );
                            create_main_window(app.handle(), &ctx, background_start);
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

    // ------------------------------------------------------------------
    // TDD RED phase: same-origin main-window URL decision.
    //
    // Production cross-origin root cause: `WebviewUrl::App("index.html")`
    // makes the page origin `tauri.localhost` (Win/Linux) or
    // `tauri://localhost` (macOS), incompatible with oc-server's
    // `http://127.0.0.1:<port>` origin. Dev profile keeps the App URL
    // so Vite :5180 + proxy still works (cross-origin only in prod).
    // ------------------------------------------------------------------
    #[test]
    fn main_url_decision_dev_returns_app_path() {
        // debug_assertions = true → Vite dev URL, not loopback.
        let decision = decide_main_webview_url(true, 12345);
        assert_eq!(decision, MainUrlDecision::AppIndexHtml);
    }

    #[test]
    fn main_url_decision_release_returns_loopback_with_trailing_slash() {
        // debug_assertions = false → external URL must be the loopback
        // origin with trailing slash, so WebView origin == api_base_url.
        let decision = decide_main_webview_url(false, 12345);
        match decision {
            MainUrlDecision::ExternalLoopback(url) => {
                assert_eq!(url, "http://127.0.0.1:12345/");
            }
            other => panic!("expected ExternalLoopback, got {:?}", other),
        }
    }

    #[test]
    fn main_url_decision_release_carries_real_port() {
        // The decision must reflect the OS-assigned port verbatim —
        // tauri.localhost is intentionally avoided.
        for port in [1u16, 80, 5180, 58980, 65535] {
            let decision = decide_main_webview_url(false, port);
            match decision {
                MainUrlDecision::ExternalLoopback(url) => {
                    assert!(
                        url.ends_with(&format!(":{}/", port)),
                        "url {:?} must end with :{}/",
                        url,
                        port
                    );
                    assert!(
                        url.starts_with("http://127.0.0.1:"),
                        "url {:?} must use loopback, not tauri.localhost",
                        url
                    );
                }
                other => panic!("expected ExternalLoopback, got {:?}", other),
            }
        }
    }

    #[test]
    fn main_url_decision_release_with_no_usable_port_returns_app_path() {
        // Backend failure / unreachable context: `ctx_suggested_port`
        // returns 0 because api_base_url is None and local_origin is a
        // placeholder. Production with port == 0 must NOT synthesize an
        // External "http://127.0.0.1:0/" loopback (port 0 is reserved,
        // and forcing the bundled UI through a phantom origin breaks the
        // recovery screen). Instead, fall back to WebviewUrl::App so
        // Tauri's frontendDist serves the bundled UI directly — the UI
        // then sees `tauri.localhost` origin (acceptable for the
        // local-unavailable recovery screen path which is local-driven,
        // not API-driven).
        let decision = decide_main_webview_url(false, 0);
        assert_eq!(decision, MainUrlDecision::AppIndexHtml);
    }

    #[test]
    fn main_url_decision_dev_with_no_port_still_returns_app_path() {
        // Dev profile already uses WebviewUrl::App unconditionally; the
        // port-0 fallback must not break that invariant.
        let decision = decide_main_webview_url(true, 0);
        assert_eq!(decision, MainUrlDecision::AppIndexHtml);
    }

    // ------------------------------------------------------------------
    // TDD RED phase: default ui-dist path + dist-dir env decision.
    //
    // The pure helper computes the candidate default ui-dist path
    // (CARGO_MANIFEST_DIR/../ui-dist). The pure decision helper picks
    // either an existing env override, or the candidate, or skips.
    // ------------------------------------------------------------------
    #[test]
    fn default_ui_dist_path_is_manifest_relative() {
        // The default is anchored at CARGO_MANIFEST_DIR so the path is
        // deterministic regardless of cwd, and the same in dev/release.
        let expected = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("ui-dist");
        assert_eq!(default_ui_dist_path(), expected);
    }

    #[test]
    fn dist_dir_decision_prefers_existing_env_override() {
        // When GRIDFORGE_DIST_DIR is already set (non-empty), the helper
        // must use that value and NOT overwrite it. This protects users
        // who intentionally redirect the bundle to a custom location.
        let env_value = std::ffi::OsString::from("/some/explicit/override");
        let candidate = std::path::PathBuf::from("/never/reached");
        let decision = resolve_dist_dir_decision(Some(&env_value), &candidate, true);
        assert_eq!(decision, DistDirDecision::Use(std::path::PathBuf::from(&env_value)));
    }

    #[test]
    fn dist_dir_decision_falls_back_to_candidate_when_dir_exists() {
        // No env override, candidate path exists → caller must inject the
        // candidate as the new env value. The variant is `SetDefault`
        // (not `Use`) to make the "user override" vs "computed default"
        // distinction explicit at the call site — `set_var` should only
        // be invoked on the SetDefault branch.
        let candidate = std::env::temp_dir(); // temp dir is always a directory
        let decision = resolve_dist_dir_decision(None, &candidate, true);
        assert_eq!(decision, DistDirDecision::SetDefault(candidate));
    }

    #[test]
    fn dist_dir_decision_skips_when_no_env_and_candidate_missing() {
        // No env, candidate missing → Skip (do not set env); caller
        // is expected to log a warning so the misconfiguration is
        // observable in the log instead of silently failing.
        let candidate = std::path::PathBuf::from("/this/path/does/not/exist/anywhere");
        let decision = resolve_dist_dir_decision(None, &candidate, false);
        assert!(matches!(decision, DistDirDecision::Skip));
    }

    #[test]
    fn dist_dir_decision_preserves_empty_env_string_does_not_overwrite() {
        // An explicitly-empty env (e.g. `GRIDFORGE_DIST_DIR=""`) is a
        // USER signal and must be preserved as-is — even if a candidate
        // default path exists. The previous "treat-empty-as-unset"
        // semantic silently swallowed the user's intent and let us
        // overwrite an empty string with the candidate; the new contract
        // respects the explicit value (none vs empty are distinct).
        let empty = std::ffi::OsString::new();
        let candidate = std::env::temp_dir(); // temp dir is always a directory
        let decision = resolve_dist_dir_decision(Some(&empty), &candidate, true);
        assert_eq!(decision, DistDirDecision::PreserveEmpty);
        // Sanity: even when candidate is missing, the explicit empty
        // value is preserved (it never falls through to Skip).
        let missing = std::path::PathBuf::from("/nope/never/here");
        let decision_missing =
            resolve_dist_dir_decision(Some(&empty), &missing, false);
        assert_eq!(decision_missing, DistDirDecision::PreserveEmpty);
    }

    #[test]
    fn dist_dir_decision_set_default_when_env_absent_and_candidate_exists() {
        // Some env (non-empty) and Some env (empty) take priority; only
        // an entirely-absent env (None) lets the candidate be set. This
        // test pins the canonical "no env, default applies" path after
        // the API moved from Use(env|candidate) → Use|SetDefault split.
        let candidate = std::env::temp_dir(); // temp dir is always a directory
        let decision = resolve_dist_dir_decision(None, &candidate, true);
        assert_eq!(decision, DistDirDecision::SetDefault(candidate));
    }

    #[test]
    fn dist_dir_decision_use_returns_env_path_verbatim() {
        // After the API split, `Use` carries the *env* path verbatim —
        // the caller still must NOT `set_var` (the value is already
        // there). This test pins that semantic.
        let env_value = std::ffi::OsString::from("/some/explicit/override");
        let candidate = std::path::PathBuf::from("/never/reached");
        let decision = resolve_dist_dir_decision(Some(&env_value), &candidate, true);
        match decision {
            DistDirDecision::Use(path) => {
                assert_eq!(path, std::path::PathBuf::from(&env_value));
            }
            other => panic!("expected Use, got {:?}", other),
        }
    }

    // ------------------------------------------------------------------
    // TDD RED phase: dist-dir CLI args builder.
    //
    // Sidecar builder currently accepts only impl Into<String>; the
    // production wiring passes an OsString-typed path through it, so
    // the helper either returns "--dist-dir <path>" tokens or [].
    // ------------------------------------------------------------------
    #[test]
    fn build_dist_dir_args_emits_pair_when_present() {
        let p = std::path::PathBuf::from("/var/data/ui-dist");
        let args = build_dist_dir_args(Some(&p));
        assert_eq!(args, vec!["--dist-dir", "/var/data/ui-dist"]);
    }

    #[test]
    fn build_dist_dir_args_empty_when_absent() {
        assert!(build_dist_dir_args(None).is_empty());
    }
}
