//! Tunnels 模块 — cloudflare/ngrok 隧道编排 + 隧道认证 (bootstrap token / session)。
//!
//! 移植自:
//!   - `packages/web/server/lib/tunnels/` (types/index/routes/registry/providers/managed-config/executable-search/install-help)
//!   - `packages/web/server/lib/cloudflare-tunnel.js` (cloudflared 子进程)
//!   - `packages/web/server/lib/ngrok-tunnel.js` (ngrok 子进程)
//!   - `packages/web/server/lib/opencode/tunnel-auth.js` (bootstrap token + session 控制器)
//!   - `/connect` 路由 (core-routes.js line 944)
//!
//! 策略: 子进程生命周期用 `tokio::process::Child` (长驻); tunnel auth 全内存 Mutex 保护,
//! SHA-256 哈希 bootstrap token (单次使用); cookie 手动解析/构建。

pub mod executable_search;
pub mod install_help;
pub mod managed_config;
pub mod providers;
pub mod routes;
pub mod service;
pub mod tunnel_auth;
pub mod types;

// TTL 常量 (移植自 index.js:115-120)
/// bootstrap TTL 默认值 (30min)。当前 Rust 端使用 settings 中配置的值,
/// 此常量保留用于 API 契约文档。
#[allow(dead_code)]
pub const TUNNEL_BOOTSTRAP_TTL_DEFAULT_MS: i64 = 30 * 60 * 1000; // 30min
pub const TUNNEL_BOOTSTRAP_TTL_MIN_MS: i64 = 60 * 1000; // 1min
pub const TUNNEL_BOOTSTRAP_TTL_MAX_MS: i64 = 24 * 60 * 60 * 1000; // 24h

pub const TUNNEL_SESSION_TTL_DEFAULT_MS: i64 = 8 * 60 * 60 * 1000; // 8h
pub const TUNNEL_SESSION_TTL_MIN_MS: i64 = 5 * 60 * 1000; // 5min
pub const TUNNEL_SESSION_TTL_MAX_MS: i64 = 30 * 24 * 60 * 60 * 1000; // 30d

pub const CLOUDFLARE_MANAGED_REMOTE_TUNNELS_VERSION: i64 = 1;

/// bootstrap TTL 归一化: null 透传, 否则 clamp 到 [min, max]。
/// (JS 端还检查 Number.isFinite, 但 Rust 的 i64 永远有限。)
pub fn normalize_tunnel_bootstrap_ttl_ms(value: Option<i64>) -> Option<i64> {
    value.map(|v| v.clamp(TUNNEL_BOOTSTRAP_TTL_MIN_MS, TUNNEL_BOOTSTRAP_TTL_MAX_MS))
}

/// session TTL 归一化: null → default, 否则 clamp 到 [min, max]。
pub fn normalize_tunnel_session_ttl_ms(value: Option<i64>) -> i64 {
    match value {
        None => TUNNEL_SESSION_TTL_DEFAULT_MS,
        Some(v) => v.clamp(TUNNEL_SESSION_TTL_MIN_MS, TUNNEL_SESSION_TTL_MAX_MS),
    }
}
