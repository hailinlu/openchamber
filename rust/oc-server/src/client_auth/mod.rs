//! Client auth 模块 — trusted-device bearer token + pairing。
//!
//! 移植自 `packages/web/server/lib/client-auth/remote-clients.js` + `pairing.js`。
//! 与 Node 后端 HTTP API 契约字节对齐。

pub mod remote_clients;
pub mod pairing;
pub mod routes;

/// Remote client token 前缀。
pub const TOKEN_PREFIX: &str = "oc_client_";
/// Token 熵 (bytes)。
pub const TOKEN_BYTES: usize = 32;
/// Pairing ID 前缀。
pub const PAIRING_ID_PREFIX: &str = "pair_";
/// Pairing secret 熵 (bytes)。
pub const SECRET_BYTES: usize = 32;
/// 默认 pairing TTL: 10 分钟。
#[allow(dead_code)]
pub const DEFAULT_PAIRING_TTL_MS: i64 = 10 * 60 * 1000;
/// Label 最大长度。
pub const MAX_LABEL_LENGTH: usize = 80;
/// Store 版本。
pub const STORE_VERSION: i64 = 1;
