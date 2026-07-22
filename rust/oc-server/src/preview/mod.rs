//! Preview 模块 — dev server 反向代理。
//!
//! 对应 `packages/web/server/lib/preview/proxy-runtime.js` (1,599 行)。
//!
//! 职责:
//!   1. `POST /api/preview/targets` — 创建短命代理目标 (loopback dev server)
//!   2. HTTP 反向代理 `/api/preview/proxy/:id/*` — 转发到 dev server,
//!      重写 HTML/CSS/JS body + CSP/redirect headers + 注入 preview bridge
//!   3. WebSocket 升级代理 `/api/preview/proxy/:id/*` — 转发 WS (Vite HMR 等)
//!
//! Auth 旁路 (`middleware/auth.rs:87`) 已预埋: `/api/preview/proxy/` + `oc_preview_token`
//! 存在性 → 放行到 handler, 真实 token 校验在 `targets.rs::resolve_target_from_request`。
//! UI auth 白名单 (`ui_auth/types.rs:136,150`) 已含 `/api/preview/proxy/`。

pub mod classify;
pub mod cookies;
pub mod normalize;
pub mod rewrite;
pub mod routes;
pub mod targets;

// ============================================================
// 常量 (对齐 proxy-runtime.js 顶部)
// ============================================================

/// 目标默认存活时间: 30 分钟 (对应 `DEFAULT_TARGET_TTL_MS`)。
pub const DEFAULT_TARGET_TTL_MS: u64 = 30 * 60 * 1000;

/// 目标最小存活时间: 15 秒 (低于此值会被 clamp)。
pub const MIN_TARGET_TTL_MS: u64 = 15_000;

/// TTL 扫描间隔: 30 秒。
pub const SWEEP_INTERVAL_MS: u64 = 30_000;

/// Preview token cookie 名称。
pub const TOKEN_COOKIE_NAME: &str = "oc_preview_token";

/// Preview token query 参数名。
pub const TOKEN_QUERY_PARAM: &str = "oc_preview_token";

/// 客户端 token query 参数名 (需要 strip)。
pub const CLIENT_TOKEN_QUERY_PARAM: &str = "oc_client_token";

/// URL auth token query 参数名 (需要 strip)。
pub const URL_AUTH_TOKEN_QUERY_PARAM: &str = "oc_url_token";

/// Vite reload 参数名 (需要 strip)。
pub const PREVIEW_RELOAD_PARAM: &str = "ocPreview";

/// 代理响应 body 缓冲上限: 10 MB。
pub const PREVIEW_PROXY_MAX_BODY_BYTES: usize = 10 * 1024 * 1024;

/// 代理请求超时 (对齐 `proxy.rs` 的 `PROXY_TIMEOUT`)。
pub const PREVIEW_PROXY_TIMEOUT_SECS: u64 = 4 * 60;

/// Inertia 请求透传 header。
pub const PREVIEW_PASSTHROUGH_REQUEST_HEADERS: &[&str] = &["x-inertia", "x-inertia-version"];

/// Inertia 响应透传 header。
pub const PREVIEW_PASSTHROUGH_RESPONSE_HEADERS: &[&str] = &["x-inertia", "x-inertia-location"];

/// Preview bridge 脚本的 DOM id。
pub const PREVIEW_BRIDGE_SCRIPT_ID: &str = "gridforge-preview-bridge";

/// Loopback 主机名集合 (仅这些允许非 allowExternal 代理)。
pub const LOOPBACK_HOSTS: &[&str] = &[
    "localhost",
    "127.0.0.1",
    "::1",
    "[::1]",
    "0.0.0.0",
];

/// Target id 正则: 16-64 位十六进制 (对齐 `[a-f0-9]{16,64}`)。
pub static TARGET_ID_RE: once_cell::sync::Lazy<regex::Regex> =
    once_cell::sync::Lazy::new(|| {
        regex::Regex::new(r"(?i)^/api/preview/proxy/([a-f0-9]{16,64})(?:/|$)")
            .expect("invalid TARGET_ID_RE")
    });

/// 从路径中提取 target id (路径形如 `/api/preview/proxy/<hex>/...`)。
pub fn extract_target_id(path: &str) -> Option<&str> {
    TARGET_ID_RE
        .captures(path)
        .and_then(|c| c.get(1).map(|m| m.as_str()))
}

/// 剥离代理前缀, 返回上游路径。
///
/// 对应 `stripProxyPrefix` (proxy-runtime.js:1256-1263)。
/// `/api/preview/proxy/<id>` → `/`, `/api/preview/proxy/<id>/foo` → `/foo`。
pub fn strip_proxy_prefix(pathname: &str, id: &str) -> String {
    let prefix = format!("/api/preview/proxy/{}", id);
    if !pathname.starts_with(&prefix) {
        return pathname.to_string();
    }
    let rest = &pathname[prefix.len()..];
    if rest.is_empty() {
        "/".to_string()
    } else {
        rest.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_target_id_matches_hex() {
        assert_eq!(
            extract_target_id("/api/preview/proxy/abc123def4567890/foo"),
            Some("abc123def4567890")
        );
    }

    #[test]
    fn extract_target_id_rejects_short() {
        assert_eq!(extract_target_id("/api/preview/proxy/short/foo"), None);
    }

    #[test]
    fn strip_proxy_prefix_root() {
        assert_eq!(strip_proxy_prefix("/api/preview/proxy/abc123", "abc123"), "/");
    }

    #[test]
    fn strip_proxy_prefix_subpath() {
        assert_eq!(
            strip_proxy_prefix("/api/preview/proxy/abc123/assets/app.js", "abc123"),
            "/assets/app.js"
        );
    }

    #[test]
    fn strip_proxy_prefix_preserves_non_match() {
        assert_eq!(strip_proxy_prefix("/other", "abc123"), "/other");
    }
}
