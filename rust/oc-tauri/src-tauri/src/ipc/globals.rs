//! init_script 生成器: 注入标量全局变量 + `window.__OPENCHAMBER_DESKTOP__` 桥。
//!
//! Tauri 没有 Electron 的 contextBridge 预加载机制。我们用 `withGlobalTauri: true`
//! 注入 `window.__TAURI__` 全局, 然后在此脚本里构建与 Electron preload.mjs
//! 完全等价的桥接口, 让 UI (`packages/ui/src/lib/desktop.ts`) 零改动。
//!
//! 注入方式: setup 里拿到 sidecar port 后调用 `build_init_script(port)`,
//! 通过 `window.eval()` 或 `WebviewWindowBuilder::initialization_script()` 执行。

use serde_json::json;

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
/// 1. 设置所有标量全局变量 (`__OPENCHAMBER_*`)
/// 2. 构建 `window.__OPENCHAMBER_DESKTOP__` 对象 (5 方法)
///
/// 桥的 invoke 走 `window.__TAURI__.core.invoke` (withGlobalTauri 注入),
/// 事件走 Tauri 的 `event.listen`, 复现 Electron 的 listen 双路径
/// (handler 回调 + DOM CustomEvent)。
pub fn build_init_script(ctx: &RuntimeContext) -> String {
    let mut globals = Vec::new();

    // --- 标量全局变量 (条件注入, 与 preload.mjs 一致) ---

    // __OPENCHAMBER_LOCAL_ORIGIN__ — Remote 页面也需要 (HostSwitcher 判断 Local 入口)
    globals.push(format_js_global(
        "__OPENCHAMBER_LOCAL_ORIGIN__",
        &json!(ctx.local_origin),
    ));

    if let Some(ref url) = ctx.api_base_url {
        globals.push(format_js_global("__OPENCHAMBER_API_BASE_URL__", &json!(url)));
    }

    // local-only 全局变量 (UI 会从 location.origin 判断是否 local page)
    // 在 Tauri loopback 模式下, 页面 origin == local_origin, 所以这些始终注入。
    if let Some(ref token) = ctx.client_token {
        globals.push(format_js_global(
            "__OPENCHAMBER_CLIENT_TOKEN__",
            &json!(token),
        ));
    }
    if let Some(ref home) = ctx.home_directory {
        globals.push(format_js_global("__OPENCHAMBER_HOME__", &json!(home)));
    }
    if let Some(ref id) = ctx.relay_host_id {
        globals.push(format_js_global(
            "__OPENCHAMBER_RELAY_HOST_ID__",
            &json!(id),
        ));
    }
    if let Some(ref headers) = ctx.runtime_headers {
        globals.push(format_js_global(
            "__OPENCHAMBER_RUNTIME_HEADERS__",
            headers,
        ));
    }
    if let Some(major) = ctx.macos_major {
        globals.push(format_js_global(
            "__OPENCHAMBER_MACOS_MAJOR__",
            &json!(major),
        ));
    }

    // __OPENCHAMBER_PLATFORM__ — UI 用来判断 frameless chrome / control 侧
    let platform = platform_string();
    globals.push(format_js_global(
        "__OPENCHAMBER_PLATFORM__",
        &json!(platform),
    ));

    // __OPENCHAMBER_ELECTRON__ — 壳身份标识。UI 的 isElectronShell() 检查它。
    // 我们保持同名但 runtime 标为 'tauri', macVibrancy 暂不支持。
    // 注意: UI 的 isElectronShell() 检查的是 runtime === 'electron'。
    // 为了让 UI 在 Tauri 下也走桌面壳分支, 我们仍标 runtime: 'electron'
    // (桥接口完全等价, UI 不需要区分)。macVibrancySupported 在非 mac 上为 false。
    let mac_vibrancy_supported = cfg!(target_os = "macos");
    globals.push(format!(
        "(function(){{window.__OPENCHAMBER_ELECTRON__={{runtime:'electron',macVibrancy:false,macVibrancySupported:{}}};}})();",
        mac_vibrancy_supported
    ));

    // __OPENCHAMBER_DESKTOP_BOOT_OUTCOME__ — 前端的桌面启动状态机依赖此值。
    // 复现 Electron main.mjs buildInitScript。
    // sidecar 已就绪(健康检查通过)后才设置, 此时 local 后端一定可达。
    globals.push(format_js_global(
        "__OPENCHAMBER_DESKTOP_BOOT_OUTCOME__",
        &serde_json::json!({"target": "local", "status": "ok"}),
    ));

    let globals_js = globals.join("\n");

    // --- __OPENCHAMBER_DESKTOP__ 桥对象 ---
    // 复现 preload.mjs:184-190 的 5 个方法。
    // invoke → __TAURI__.core.invoke('openchamber_invoke', {cmd, args})
    // openDialog → invoke('openchamber_dialog_open', {options})
    // grantFileAccess → invoke('openchamber_file_grant', {filePath})
    // openExternal → invoke('openchamber_invoke', {cmd:'desktop_open_external_url', args:{url}})
    // listen → 订阅 'openchamber:emit', 双路径分发 (handler + DOM CustomEvent)
    let bridge_js = r#"
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

  // 订阅 Tauri 事件 'openchamber:emit', 分发 {event, detail}
  // withGlobalTauri 注入 __TAURI__.event.listen
  var __ocEmitUnlisten = null;
  function __ocEnsureEmitListener() {
    if (__ocEmitUnlisten) return;
    if (!window.__TAURI__ || !window.__TAURI__.event || !window.__TAURI__.event.listen) {
      // withGlobalTauri 尚未注入; 延迟重试
      setTimeout(__ocEnsureEmitListener, 50);
      return;
    }
    window.__TAURI__.event.listen('openchamber:emit', function(evt) {
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
      console.error('[tauri:bridge] failed to subscribe openchamber:emit:', e);
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
    return window.__TAURI__.core.invoke('openchamber_invoke', { cmd: cmd, args: args || {} });
  }

  __ocEnsureEmitListener();

  window.__OPENCHAMBER_DESKTOP__ = {
    invoke: function(cmd, args) { return __ocInvoke(cmd, args); },
    openDialog: function(options) {
      if (!window.__TAURI__ || !window.__TAURI__.core || !window.__TAURI__.core.invoke) {
        return Promise.reject(new Error('Tauri core API not available'));
      }
      return window.__TAURI__.core.invoke('openchamber_dialog_open', { options: options || {} });
    },
    grantFileAccess: function(filePath) {
      if (!window.__TAURI__ || !window.__TAURI__.core || !window.__TAURI__.core.invoke) {
        return Promise.reject(new Error('Tauri core API not available'));
      }
      return window.__TAURI__.core.invoke('openchamber_file_grant', { filePath: filePath });
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

    format!("{}\n{}", globals_js, bridge_js)
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
        assert!(script.contains("__OPENCHAMBER_LOCAL_ORIGIN__"));
        assert!(script.contains("http://127.0.0.1:12345"));
        assert!(script.contains("__OPENCHAMBER_API_BASE_URL__"));
        assert!(script.contains("__OPENCHAMBER_PLATFORM__"));
        assert!(script.contains("__OPENCHAMBER_ELECTRON__"));
        assert!(script.contains("__OPENCHAMBER_DESKTOP__"));
    }

    #[test]
    fn build_init_script_has_bridge_methods() {
        let ctx = RuntimeContext::from_sidecar_port(0);
        let script = build_init_script(&ctx);

        assert!(script.contains("openchamber_invoke"));
        assert!(script.contains("openchamber_dialog_open"));
        assert!(script.contains("openchamber_file_grant"));
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
}
