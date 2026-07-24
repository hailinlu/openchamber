//! init_script 生成器: 注入标量全局变量 + `window.__GRIDFORGE_DESKTOP__` 桥。
//!
//! Tauri 没有 Electron 的 contextBridge 预加载机制。我们用 `withGlobalTauri: true`
//! 注入 `window.__TAURI__` 全局, 然后在此脚本里构建与 Electron preload.mjs
//! 完全等价的桥接口, 让 UI (`packages/ui/src/lib/desktop.ts`) 零改动。
//!
//! 注入方式: setup 里拿到 sidecar port 后调用 `build_init_script(port)`,
//! 通过 `window.eval()` 或 `WebviewWindowBuilder::initialization_script()` 执行。

use serde_json::json;

/// 后端启动结果, 驱动注入的 `__GRIDFORGE_DESKTOP_BOOT_OUTCOME__`。
///
/// - `Ok`: 后端就绪 (sidecar 健康检查通过 / oc-server 绑定成功) →
///   boot outcome `{target:'local', status:'ok'}`。
/// - `Unreachable`: 后端启动失败 (`OcServer::start` / sidecar 返回 Err) →
///   boot outcome `{target:'local', status:'unreachable'}`, UI 据此渲染
///   local-unavailable 恢复屏 (`desktopBoot.ts:185-187`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootStatus {
    Ok,
    Unreachable,
}

/// 运行时上下文: setup 阶段从 sidecar / CLI args 解析出的标量值。
/// None 的字段不注入 (与 preload.mjs 的条件暴露一致)。
pub struct RuntimeContext {
    pub local_origin: String,
    pub api_base_url: Option<String>,
    pub client_token: Option<String>,
    pub home_directory: Option<String>,
    pub relay_host_id: Option<String>,
    pub runtime_headers: Option<serde_json::Value>,
    pub macos_major: Option<i32>,
    pub boot_status: BootStatus,
    /// 后端启动失败时的诊断信息 (anyhow 完整因果链)。
    ///
    /// 仅在 `boot_status == Unreachable` 时有意义; 注入为
    /// `__GRIDFORGE_BOOT_DIAGNOSTIC__` (JSON 字符串), 供 UI 恢复屏展开显示,
    /// 让用户/开发者看到后端真实的失败原因 (而非笼统的 "could not be started")。
    pub diagnostic: Option<String>,
}

impl RuntimeContext {
    pub fn from_sidecar_port(port: u16) -> Self {
        let origin = format!("http://127.0.0.1:{}", port);
        Self {
            local_origin: origin.clone(),
            api_base_url: Some(origin),
            client_token: None,
            home_directory: None,
            relay_host_id: None,
            runtime_headers: None,
            macos_major: detect_macos_major(),
            boot_status: BootStatus::Ok,
            diagnostic: None,
        }
    }

    /// 后端启动失败时用的上下文: 无 api_base_url (不谎报后端地址),
    /// boot_status = Unreachable。`local_origin` 由调用方提供 (Err 窗
    /// 用 `unreachable_local_origin()` 占位, `isDesktopLocalOriginActive`
    /// 只要求非空即可让 UI 渲染恢复屏而非 restart 循环)。
    ///
    /// `diagnostic`: 后端失败原因 (anyhow `{:#}` 完整因果链)。非空时注入为
    /// `__GRIDFORGE_BOOT_DIAGNOSTIC__`, 恢复屏可展开显示, 取代笼统文案。
    pub fn for_unreachable_backend(
        local_origin: String,
        diagnostic: Option<String>,
    ) -> Self {
        Self {
            local_origin,
            api_base_url: None,
            client_token: None,
            home_directory: None,
            relay_host_id: None,
            runtime_headers: None,
            macos_major: detect_macos_major(),
            boot_status: BootStatus::Unreachable,
            diagnostic,
        }
    }
}

/// 检测 macOS 主版本号 (驱动 traffic light 偏移)。非 macOS 返回 None。
fn detect_macos_major() -> Option<i32> {
    #[cfg(target_os = "macos")]
    {
        use std::process::Command;
        let output = Command::new("sw_vers")
            .arg("-productVersion")
            .output()
            .ok()?;
        let version = String::from_utf8_lossy(&output.stdout);
        version
            .trim()
            .split('.')
            .next()
            .and_then(|major| major.parse::<i32>().ok())
            .filter(|&v| v > 0)
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

/// 平台标识字符串 (对应 Electron 的 process.platform)。
pub fn platform_string() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "darwin"
    }
    #[cfg(target_os = "windows")]
    {
        "win32"
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        "linux"
    }
}

/// 生成完整的 init_script 字符串。
///
/// 脚本做了两件事:
/// 1. 设置所有标量全局变量 (`__GRIDFORGE_*`)
/// 2. 构建 `window.__GRIDFORGE_DESKTOP__` 对象 (5 方法)
///
/// 桥的 invoke 走 `window.__TAURI__.core.invoke` (withGlobalTauri 注入),
/// 事件走 Tauri 的 `event.listen`, 复现 Electron 的 listen 双路径
/// (handler 回调 + DOM CustomEvent)。
pub fn build_init_script(ctx: &RuntimeContext) -> String {
    // 组合: 静态全局变量 (平台/壳身份) + 端口/运行时相关全局变量 + 桥。
    // 静态部分复用 build_static_globals_script; 端口/运行时部分复用
    // build_port_globals_script; 桥复用 BRIDGE_JS。
    let static_globals = build_static_globals_script();
    let port_globals = build_port_globals_script(ctx);
    format!("{}\n{}\n{}", static_globals, port_globals, BRIDGE_JS)
}

/// 生成仅含**依赖后端端口/运行时上下文**的全局变量脚本。
///
/// 这些全局变量 (`__GRIDFORGE_API_BASE_URL__`、`__GRIDFORGE_LOCAL_ORIGIN__`、
/// client token、home、relay host id、runtime headers、boot outcome) 只有在
/// 后端启动 (成功或失败) 后才能确定。主窗口/mini chat 窗口都在**后端启动后**
/// 建窗 (成功分支注入真实端口; 失败分支用 `for_unreachable_backend` 注入
/// unreachable boot outcome, 不注入 api_base_url), 因此这些值由 `build_init_script`
/// 组合进 `initialization_script` (在页面 JS 执行前、每次导航前运行),
/// 彻底消除首次加载竞态与 reload gap。
///
/// `__GRIDFORGE_MACOS_MAJOR__` 虽不依赖端口, 但来自 RuntimeContext,
/// 与其它运行时全局放在一起以保持来源一致。
pub fn build_port_globals_script(ctx: &RuntimeContext) -> String {
    let mut globals = Vec::new();

    // __GRIDFORGE_LOCAL_ORIGIN__ — Remote 页面也需要 (HostSwitcher 判断 Local 入口)
    globals.push(format_js_global(
        "__GRIDFORGE_LOCAL_ORIGIN__",
        &json!(ctx.local_origin),
    ));

    if let Some(ref url) = ctx.api_base_url {
        globals.push(format_js_global("__GRIDFORGE_API_BASE_URL__", &json!(url)));
    }

    // local-only 全局变量 (UI 会从 location.origin 判断是否 local page)
    //
    // 注意: 主窗口走 `WebviewUrl::App("index.html")` 后, page origin 与
    // `__GRIDFORGE_LOCAL_ORIGIN__` 不再相等:
    // - Dev: page origin = `http://127.0.0.1:5180` (Vite devUrl), local_origin
    //   = oc-server 的 `http://127.0.0.1:<port>` (Vite proxy 转发)。
    // - Prod: page origin = `tauri.localhost` (Win/Linux) 或 `tauri://localhost`
    //   (macOS) (Tauri frontendDist), local_origin = oc-server 的
    //   `http://127.0.0.1:<port>` (axum CORS 处理跨源)。
    //
    // `client_token` / `home_directory` / `relay_host_id` 这些是 desktop 本地
    // 元数据, 与 origin 无关, 始终注入 (mini_chat 也用同一份 init_script)。
    if let Some(ref token) = ctx.client_token {
        globals.push(format_js_global(
            "__GRIDFORGE_CLIENT_TOKEN__",
            &json!(token),
        ));
    }
    if let Some(ref home) = ctx.home_directory {
        globals.push(format_js_global("__GRIDFORGE_HOME__", &json!(home)));
    }
    if let Some(ref id) = ctx.relay_host_id {
        globals.push(format_js_global(
            "__GRIDFORGE_RELAY_HOST_ID__",
            &json!(id),
        ));
    }
    if let Some(ref headers) = ctx.runtime_headers {
        globals.push(format_js_global(
            "__GRIDFORGE_RUNTIME_HEADERS__",
            headers,
        ));
    }
    if let Some(major) = ctx.macos_major {
        globals.push(format_js_global(
            "__GRIDFORGE_MACOS_MAJOR__",
            &json!(major),
        ));
    }

    // __GRIDFORGE_DESKTOP_BOOT_OUTCOME__ — 前端的桌面启动状态机依赖此值
    // (desktopBoot.ts:185-187: unreachable → local-unavailable 恢复屏)。
    // 复现 Electron main.mjs buildInitScript, 按 boot_status 选择 ok/unreachable。
    let status_str = match ctx.boot_status {
        BootStatus::Ok => "ok",
        BootStatus::Unreachable => "unreachable",
    };
    globals.push(format_js_global(
        "__GRIDFORGE_DESKTOP_BOOT_OUTCOME__",
        &serde_json::json!({"target": "local", "status": status_str}),
    ));

    // __GRIDFORGE_BOOT_DIAGNOSTIC__ — 仅后端失败 (Unreachable) 且有诊断信息时注入。
    // 把后端 anyhow 完整因果链透传给 UI, 恢复屏可展开显示, 取代笼统文案,
    // 让用户/开发者看到后端真实的失败原因。正常路径不注入 (保持 undefined)。
    if ctx.boot_status == BootStatus::Unreachable {
        if let Some(ref diag) = ctx.diagnostic {
            let trimmed = diag.trim();
            if !trimmed.is_empty() {
                globals.push(format_js_global(
                    "__GRIDFORGE_BOOT_DIAGNOSTIC__",
                    &serde_json::json!(trimmed),
                ));
            }
        }
    }

    globals.join("\n")
}

/// `window.__GRIDFORGE_DESKTOP__` 桥对象 JS (复现 preload.mjs:184-190 的 5 个方法)。
///
/// 桥只依赖 `window.__TAURI__` (由 `withGlobalTauri` 注入), **不依赖后端端口**,
/// 因此可安全用于 `initialization_script` (在页面 JS 执行前、每次导航前运行)。
///
/// - invoke → __TAURI__.core.invoke('gridforge_invoke', {cmd, args})
/// - openDialog → invoke('gridforge_dialog_open', {options})
/// - grantFileAccess → invoke('gridforge_file_grant', {filePath})
/// - openExternal → invoke('gridforge_invoke', {cmd:'desktop_open_external_url', args:{url}})
/// - listen → 订阅 'gridforge:emit', 双路径分发 (handler + DOM CustomEvent)
const BRIDGE_JS: &str = r#"
(function() {
  // event listener 注册表 (复现 preload.mjs eventListeners Map)
  var __ocEventListeners = {};

  // 双路径事件分发: handler 回调 + DOM CustomEvent
  function __ocDispatchNativeEvent(event, detail) {
    var listeners = __ocEventListeners[event];
    if (listeners) {
      for (var i = 0; i < listeners.length; i++) {
        try { listeners[i]({ payload: detail }); }
        catch (e) { console.error('[tauri:bridge] listener failed for ' + event + ':', e); }
      }
    }
    try {
      var domEvent = (detail === undefined)
        ? new Event(event)
        : new CustomEvent(event, { detail: detail });
      window.dispatchEvent(domEvent);
    } catch (e) {
      console.error('[tauri:bridge] failed to dispatch DOM event ' + event + ':', e);
    }
  }

  // 订阅 Tauri 事件 'gridforge:emit', 分发 {event, detail}
  // withGlobalTauri 注入 __TAURI__.event.listen
  var __ocEmitUnlisten = null;
  function __ocEnsureEmitListener() {
    if (__ocEmitUnlisten) return;
    if (!window.__TAURI__ || !window.__TAURI__.event || !window.__TAURI__.event.listen) {
      // withGlobalTauri 尚未注入; 延迟重试
      setTimeout(__ocEnsureEmitListener, 50);
      return;
    }
    window.__TAURI__.event.listen('gridforge:emit', function(evt) {
      var payload = evt && evt.payload;
      if (!payload || typeof payload !== 'object') return;
      var event = typeof payload.event === 'string' ? payload.event : '';
      if (!event) return;
      var detail = payload.detail;
      // vibrancy-ready 特殊处理 (preload.mjs 等价; Tauri 暂不支持 vibrancy)
      __ocDispatchNativeEvent(event, detail);
    }).then(function(unlisten) {
      __ocEmitUnlisten = unlisten;
    }).catch(function(e) {
      console.error('[tauri:bridge] failed to subscribe gridforge:emit:', e);
    });
  }

  function __ocAddListener(event, handler) {
    if (!__ocEventListeners[event]) __ocEventListeners[event] = [];
    __ocEventListeners[event].push(handler);
    return function() {
      var arr = __ocEventListeners[event];
      if (!arr) return;
      var idx = arr.indexOf(handler);
      if (idx >= 0) arr.splice(idx, 1);
      if (arr.length === 0) delete __ocEventListeners[event];
    };
  }

  function __ocInvoke(cmd, args) {
    // withGlobalTauri: window.__TAURI__.core.invoke
    if (!window.__TAURI__ || !window.__TAURI__.core || !window.__TAURI__.core.invoke) {
      return Promise.reject(new Error('Tauri core API not available'));
    }
    return window.__TAURI__.core.invoke('gridforge_invoke', { cmd: cmd, args: args || {} });
  }

  __ocEnsureEmitListener();

  window.__GRIDFORGE_DESKTOP__ = {
    invoke: function(cmd, args) { return __ocInvoke(cmd, args); },
    openDialog: function(options) {
      if (!window.__TAURI__ || !window.__TAURI__.core || !window.__TAURI__.core.invoke) {
        return Promise.reject(new Error('Tauri core API not available'));
      }
      return window.__TAURI__.core.invoke('gridforge_dialog_open', { options: options || {} });
    },
    grantFileAccess: function(filePath) {
      if (!window.__TAURI__ || !window.__TAURI__.core || !window.__TAURI__.core.invoke) {
        return Promise.reject(new Error('Tauri core API not available'));
      }
      return window.__TAURI__.core.invoke('gridforge_file_grant', { filePath: filePath });
    },
    openExternal: function(url) {
      return __ocInvoke('desktop_open_external_url', { url: url });
    },
    listen: async function(event, handler) {
      return __ocAddListener(event, handler);
    }
  };
})();
"#;

/// 生成仅含"静态"全局变量的 init 脚本 (不依赖后端端口/运行时上下文)。
///
/// 这些变量在窗口创建时即可注入, 无需等后端启动完成:
/// - `__GRIDFORGE_ELECTRON__` — 壳身份标识 (`{ runtime: 'electron', … }`)
/// - `__GRIDFORGE_PLATFORM__` — 平台字符串 (darwin/win32/linux)
/// - `__GRIDFORGE_MACOS_MAJOR__` — macOS 主版本 (仅在 macOS 上)
///
/// 适用于 `configure_main_window_shell()` 中提前注入,
/// 确保 React 首次渲染前 `isElectronShell()` / `usesFramelessElectronChrome()` 能正确检测。
pub fn build_static_globals_script() -> String {
    let mut globals = Vec::new();

    // __GRIDFORGE_PLATFORM__
    let platform = platform_string();
    globals.push(format_js_global(
        "__GRIDFORGE_PLATFORM__",
        &json!(platform),
    ));

    // __GRIDFORGE_ELECTRON__ (与 build_init_script 保持一致)
    let mac_vibrancy_supported = cfg!(target_os = "macos");
    let mac_vibrancy = if mac_vibrancy_supported {
        crate::settings::SettingsStore::get_bool("desktopVibrancy", true)
    } else {
        false
    };
    globals.push(format!(
        "(function(){{window.__GRIDFORGE_ELECTRON__={{runtime:'electron',macVibrancy:{},macVibrancySupported:{}}};}})();",
        mac_vibrancy, mac_vibrancy_supported
    ));

    // __GRIDFORGE_MACOS_MAJOR__ (macOS 上检测)
    if let Some(major) = detect_macos_major() {
        globals.push(format_js_global(
            "__GRIDFORGE_MACOS_MAJOR__",
            &json!(major),
        ));
    }

    globals.join("\n")
}

/// 格式化单个全局变量注入语句: `window.__X__ = <json>;`
fn format_js_global(name: &str, value: &serde_json::Value) -> String {
    format!(
        "(function(){{try{{window.{}={};}}catch(e){{console.error('[tauri:bridge] failed to set {}',e);}}}})();",
        name, value, name
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_init_script_contains_all_globals() {
        let ctx = RuntimeContext::from_sidecar_port(12345);
        let script = build_init_script(&ctx);

        // 核心全局变量必须存在
        assert!(script.contains("__GRIDFORGE_LOCAL_ORIGIN__"));
        assert!(script.contains("http://127.0.0.1:12345"));
        assert!(script.contains("__GRIDFORGE_API_BASE_URL__"));
        assert!(script.contains("__GRIDFORGE_PLATFORM__"));
        assert!(script.contains("__GRIDFORGE_ELECTRON__"));
        assert!(script.contains("__GRIDFORGE_DESKTOP__"));
    }

    #[test]
    fn build_init_script_has_bridge_methods() {
        let ctx = RuntimeContext::from_sidecar_port(0);
        let script = build_init_script(&ctx);

        assert!(script.contains("gridforge_invoke"));
        assert!(script.contains("gridforge_dialog_open"));
        assert!(script.contains("gridforge_file_grant"));
        assert!(script.contains("desktop_open_external_url"));
        assert!(script.contains("__ocDispatchNativeEvent"));
    }

    #[test]
    fn build_init_script_injects_local_origin() {
        let ctx = RuntimeContext::from_sidecar_port(9999);
        let script = build_init_script(&ctx);
        assert!(script.contains("http://127.0.0.1:9999"));
    }

    #[test]
    fn platform_string_matches_electron_convention() {
        let p = platform_string();
        // Electron 用 darwin/win32/linux
        assert!(p == "darwin" || p == "win32" || p == "linux");
    }

    #[test]
    fn macos_major_is_none_off_macos() {
        #[cfg(not(target_os = "macos"))]
        {
            assert!(detect_macos_major().is_none());
        }
    }

    #[test]
    fn runtime_context_from_port_sets_origin_and_api() {
        let ctx = RuntimeContext::from_sidecar_port(8080);
        assert_eq!(ctx.local_origin, "http://127.0.0.1:8080");
        assert_eq!(ctx.api_base_url.as_deref(), Some("http://127.0.0.1:8080"));
    }

    #[test]
    fn build_static_globals_script_includes_platform_and_electron() {
        let script = build_static_globals_script();
        assert!(script.contains("__GRIDFORGE_PLATFORM__"));
        assert!(script.contains("__GRIDFORGE_ELECTRON__"));
        assert!(script.contains("runtime:'electron'"));
        assert!(
            script.contains("\"win32\"")
                || script.contains("\"darwin\"")
                || script.contains("\"linux\""),
            "platform must be one of win32/darwin/linux"
        );
    }

    #[test]
    fn build_static_globals_script_has_no_port_dependent_values() {
        let script = build_static_globals_script();
        // 静态脚本不应包含端口/URL 绑定变量
        assert!(!script.contains("__GRIDFORGE_API_BASE_URL__"));
        assert!(!script.contains("__GRIDFORGE_LOCAL_ORIGIN__"));
        assert!(!script.contains("__GRIDFORGE_DESKTOP__"));
        assert!(!script.contains("http://"));
    }

    #[test]
    fn build_init_script_includes_static_port_and_bridge() {
        // build_init_script 是主窗口/mini chat 的 initialization_script,
        // 必须同时含静态全局变量、端口相关全局变量 (含真实端口) 和桥。
        // 这是修复 prod 注入竞态的关键契约: 后端就绪后建窗, 真实端口在
        // initialization_script 里, 首次加载时 main.tsx 就能读到正确 base URL。
        let ctx = RuntimeContext::from_sidecar_port(12345);
        let script = build_init_script(&ctx);
        // 静态全局变量
        assert!(script.contains("__GRIDFORGE_PLATFORM__"));
        assert!(script.contains("__GRIDFORGE_ELECTRON__"));
        // 端口相关全局变量 (含真实端口)
        assert!(script.contains("__GRIDFORGE_API_BASE_URL__"));
        assert!(script.contains("__GRIDFORGE_LOCAL_ORIGIN__"));
        assert!(script.contains("http://127.0.0.1:12345"));
        assert!(script.contains("__GRIDFORGE_DESKTOP_BOOT_OUTCOME__"));
        // 桥
        assert!(script.contains("__GRIDFORGE_DESKTOP__"));
        assert!(script.contains("gridforge_invoke"));
    }

    #[test]
    fn build_port_globals_script_includes_port_and_runtime_values() {
        let ctx = RuntimeContext::from_sidecar_port(12345);
        let script = build_port_globals_script(&ctx);
        // 端口相关变量必须存在
        assert!(script.contains("__GRIDFORGE_API_BASE_URL__"));
        assert!(script.contains("__GRIDFORGE_LOCAL_ORIGIN__"));
        assert!(script.contains("http://127.0.0.1:12345"));
        assert!(script.contains("__GRIDFORGE_DESKTOP_BOOT_OUTCOME__"));
        // 端口脚本不应含桥 (桥在 initialization_script 中)
        assert!(!script.contains("__GRIDFORGE_DESKTOP__"));
    }

    #[test]
    fn build_port_globals_script_omits_api_base_url_when_none() {
        // 当 api_base_url 为 None 时不应注入该变量
        let mut ctx = RuntimeContext::from_sidecar_port(12345);
        ctx.api_base_url = None;
        let script = build_port_globals_script(&ctx);
        assert!(!script.contains("__GRIDFORGE_API_BASE_URL__"));
        // local_origin 仍应注入 (Remote 页面判断 Local 入口需要)
        assert!(script.contains("__GRIDFORGE_LOCAL_ORIGIN__"));
    }

    #[test]
    fn for_unreachable_backend_emits_unreachable_boot_outcome() {
        // 后端启动失败时的 Err 窗上下文: 不注入 api_base_url (不谎报地址),
        // boot outcome = unreachable, 驱动 UI 渲染 local-unavailable 恢复屏。
        let ctx =
            RuntimeContext::for_unreachable_backend("http://tauri.localhost".to_string(), None);
        assert_eq!(ctx.boot_status, BootStatus::Unreachable);
        assert_eq!(ctx.api_base_url, None);
        assert_eq!(ctx.local_origin, "http://tauri.localhost");

        let script = build_init_script(&ctx);
        // boot outcome 必须是 unreachable (不是 ok), 否则 UI 会误以为后端正常。
        assert!(script.contains("\"status\":\"unreachable\""));
        assert!(!script.contains("\"status\":\"ok\""));
        // 不注入 api_base_url: 无后端时不该谎报地址。
        assert!(!script.contains("__GRIDFORGE_API_BASE_URL__"));
        // local_origin 必须注入: isDesktopLocalOriginActive 需要非空值才能渲染恢复屏。
        assert!(script.contains("__GRIDFORGE_LOCAL_ORIGIN__"));
        assert!(script.contains("http://tauri.localhost"));
        // 静态全局 + 桥仍需注入 (壳身份标识 + IPC 桥, 不依赖后端)。
        assert!(script.contains("__GRIDFORGE_PLATFORM__"));
        assert!(script.contains("__GRIDFORGE_DESKTOP__"));
        // 无 diagnostic 时不注入该变量。
        assert!(!script.contains("__GRIDFORGE_BOOT_DIAGNOSTIC__"));
    }

    #[test]
    fn for_unreachable_backend_injects_diagnostic_when_present() {
        // 后端失败带诊断信息: 把 anyhow 因果链透传给 UI 恢复屏展开显示。
        let diag = "failed to spawn opencode binary `opencode`: program not found";
        let ctx = RuntimeContext::for_unreachable_backend(
            "http://tauri.localhost".to_string(),
            Some(diag.to_string()),
        );
        assert_eq!(ctx.boot_status, BootStatus::Unreachable);
        assert_eq!(ctx.diagnostic.as_deref(), Some(diag));

        let script = build_init_script(&ctx);
        // __GRIDFORGE_BOOT_DIAGNOSTIC__ 必须注入, 含完整诊断文本。
        assert!(script.contains("__GRIDFORGE_BOOT_DIAGNOSTIC__"));
        assert!(script.contains(diag));
        // boot outcome 仍是 unreachable (diagnostic 不影响 outcome 本身)。
        assert!(script.contains("\"status\":\"unreachable\""));
    }

    #[test]
    fn for_unreachable_backend_omits_diagnostic_when_empty() {
        // 空字符串 / 纯空白 diagnostic 不注入 (避免恢复屏显示空详情块)。
        let ctx = RuntimeContext::for_unreachable_backend(
            "http://tauri.localhost".to_string(),
            Some("   \n\t ".to_string()),
        );
        let script = build_init_script(&ctx);
        assert!(!script.contains("__GRIDFORGE_BOOT_DIAGNOSTIC__"));
        // outcome 仍正确。
        assert!(script.contains("\"status\":\"unreachable\""));
    }

    #[test]
    fn ok_path_never_injects_diagnostic() {
        // 正常路径 (后端就绪) 即使误设 diagnostic 也不注入
        // (diagnostic 仅对 Unreachable 有意义)。
        let mut ctx = RuntimeContext::from_sidecar_port(12345);
        ctx.diagnostic = Some("should not appear".to_string());
        let script = build_port_globals_script(&ctx);
        assert!(!script.contains("__GRIDFORGE_BOOT_DIAGNOSTIC__"));
        assert!(script.contains("\"status\":\"ok\""));
    }

    #[test]
    fn from_sidecar_port_defaults_boot_status_ok() {
        // 正常 (后端就绪) 路径: boot_status 默认 Ok, boot outcome = ok。
        let ctx = RuntimeContext::from_sidecar_port(12345);
        assert_eq!(ctx.boot_status, BootStatus::Ok);
        let script = build_port_globals_script(&ctx);
        assert!(script.contains("\"status\":\"ok\""));
        assert!(!script.contains("\"status\":\"unreachable\""));
    }
}
