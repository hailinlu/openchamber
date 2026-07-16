//! Private relay module (host-side port of `packages/web/server/lib/relay/`).
//!
//! 阶段 3f Group 4 — 完整迁移 8 个 Node 文件 (~2,370 行) 到 Rust axum,
//! 同时保证 `crypto.rs` + `tunnel_codec.rs` 的字节输出与
//! `packages/ui/src/lib/relay/` 中的规范性 TS 实现**逐字节对齐** —
//! `tests/cross_compat_vectors.rs` 读 frozen JSON fixture 做 byte-equality。
//!
//! 协议分层:
//! - **Layer 1**: WS 路由 + ECDSA-P256 签名认证 (`identity.rs`)
//! - **Layer 2**: ECDH P-256 + HKDF + AES-256-GCM E2EE + host 握手 (`crypto.rs`)
//! - **Layer 3**: tunnel mux 帧 / 批量 / 分片 (`tunnel_codec.rs`)
//!
//! 持久化:
//! - `<data-dir>/settings.json` 中 `privateRelay` + `relayEncryptionKey`
//!   (签名 key 复用 `notifications::relay_key`, 与 push relay 共享 serverId)
//! - `<data-dir>/relay-host.lock` (host-lock.js 等价物)
//!
//! 模块结构:
//! - `crypto`       — `e2ee.js` 全文移植 (Layer 2)
//! - `tunnel_codec` — `tunnel-codec.js` 全文移植 (Layer 3)
//! - `identity`     — `signing-key.js` + `identity.js` 移植 (Layer 1)
//! - `host_lock`    — `host-lock.js` 移植 (cooperative claim)
//! - `host_client`  — `host-client.js` 移植 (出站 WS + 重连 + 握手)
//! - `tunnel_host`  — `tunnel-host.js` 移植 (loopback HTTP/WS 分发)
//! - `service`      — `service.js` 移植 (生命周期编排)
//! - `routes`       — 3 个 axum 管理路由

pub mod crypto;
pub mod host_client;
pub mod host_lock;
pub mod identity;
pub mod routes;
pub mod service;
pub mod tunnel_codec;
pub mod tunnel_host;

// 公开常量 (供测试 + 服务使用)
pub use crypto::{
    base64_url_to_bytes, create_host_handshake, derive_session_keys, export_public_key_jwk,
    generate_ecdh_keypair, import_ecdh_private_key, HandshakeAction, HostHandshake,
    RelayCryptoError, ENCRYPTED_FRAME_HEADER_BYTES, ENCRYPTED_FRAME_IV_BYTES,
    ENCRYPTED_FRAME_VERSION,
};
pub use identity::{RelayIdentity, RelayIdentityRuntime, DEFAULT_RELAY_URL};
pub use tunnel_codec::{
    decode_frame_batch, decode_tunnel_frame, encode_frame_batch, encode_tunnel_frame,
    DecodeFrameBatchError, TunnelCodecError, TunnelFrame, TunnelFrameType,
    DEFAULT_BATCH_MAX_BYTES, DEFAULT_BATCH_MAX_FRAMES, DEFAULT_BATCH_WINDOW_MS,
    MAX_PLAINTEXT_FRAME_BYTES, MAX_TUNNEL_PAYLOAD_BYTES, TUNNEL_FRAME_HEADER_BYTES,
    TUNNEL_FRAGMENT_FLAG,
};

#[cfg(test)]
mod tests;