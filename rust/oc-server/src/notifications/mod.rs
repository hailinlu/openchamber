//! Notifications 模块 — 通知推送 + 会话状态 + SSE 通知流。
//!
//! 对应 Node `packages/web/server/lib/notifications/` 全部 8 个文件 (~2,600 行):
//! - `routes.js` (18 个 Express 路由) → `routes.rs`
//! - `runtime.js` (trigger fanout orchestrator) → `trigger.rs`
//! - `push-runtime.js` (web-push 订阅 + 可见性) → `push_store.rs` + `push_send.rs`
//! - `apns-runtime.js` (APNs relay + direct) → `apns_store.rs` + `apns_send.rs`
//! - `emitter-runtime.js` (SSE write + broadcast) → `emitter.rs`
//! - `template-runtime.js` (模板变量) → `template.rs`
//! - `message.js` (markdown→plain + truncate) → `message.rs`
//! - `relay/signing-key.js` (ECDSA P-256 签名) → `relay_key.rs`
//!
//! 会话状态机 (`session-runtime.js`) → `session_state.rs`。
//!
//! **设计**: 所有 push/APNs 发送是 fire-and-forget; trigger 调用发送但不 await,
//! 失败只 warn 不阻塞事件流。trigger 通过 `global_hub.subscribe_event()` 订阅
//! SSE 事件流, 在后台 task 中消费。

// 部分字段/方法当前未被 bin target 直接调用, 但属于 API 对等的一部分 (供 Tauri/Electron
// shell 注入或后续 wiring 使用), 抑制 dead_code 警告。
#![allow(dead_code)]

pub mod apns_send;
pub mod apns_store;
pub mod emitter;
pub mod message;
pub mod push_send;
pub mod push_store;
pub mod relay_key;
pub mod routes;
pub mod session_state;
pub mod template;
pub mod trigger;
pub mod types;

// =========================================================================
// 常量
// =========================================================================

/// SSE 通知流心跳间隔 (20s), 对应 Node `NOTIFICATION_SSE_HEARTBEAT_INTERVAL_MS`。
pub const NOTIFICATION_SSE_HEARTBEAT_INTERVAL_MS: u64 = 20_000;

/// 通知消息默认最大长度, 对应 Node `DEFAULT_NOTIFICATION_MESSAGE_MAX_LENGTH`。
pub const DEFAULT_NOTIFICATION_MESSAGE_MAX_LENGTH: usize = 250;

/// UI 可见性 TTL (30s), 超过此时间的可见性心跳视为过期。
pub const UI_VISIBILITY_TTL_MS: i64 = 30_000;

/// push 订阅文件版本。
pub const PUSH_SUBSCRIPTIONS_VERSION: u32 = 1;

/// APNs token 文件版本。
pub const APNS_TOKENS_VERSION: u32 = 1;

/// 每 UI session 最大订阅/token 数。
pub const MAX_SUBS_PER_SESSION: usize = 10;

/// APNs JWT TTL (50 min, APNs 拒绝超过 1h 的 token)。
pub const APNS_JWT_TTL_MS: i64 = 50 * 60 * 1000;

/// ready/error 通知冷却时间 (5s)。
pub const PUSH_READY_COOLDOWN_MS: i64 = 5000;

/// question 通知去抖时间 (500ms)。
pub const PUSH_QUESTION_DEBOUNCE_MS: u64 = 500;

/// permission 通知去抖时间 (500ms)。
pub const PUSH_PERMISSION_DEBOUNCE_MS: u64 = 500;

/// 默认 relay URL。
pub const DEFAULT_RELAY_URL: &str = "https://api.openchamber.dev/v1/push/send";

/// APNs 生产环境 host。
pub const APNS_HOST_PRODUCTION: &str = "https://api.push.apple.com";

/// APNs 沙箱环境 host。
pub const APNS_HOST_SANDBOX: &str = "https://api.sandbox.push.apple.com";

/// 默认 bundle ID。
pub const DEFAULT_BUNDLE_ID: &str = "com.openchamber.app";

/// session activity cooldown 持续时间 (2s)。
pub const SESSION_COOLDOWN_DURATION_MS: u64 = 2000;

/// session state 最大保留时间 (24h)。
pub const SESSION_STATE_MAX_AGE_MS: i64 = 24 * 60 * 60 * 1000;

/// session attention 最大保留时间 (24h)。
pub const SESSION_ATTENTION_MAX_AGE_MS: i64 = 24 * 60 * 60 * 1000;

/// session parent 缓存 TTL (60s)。
pub const SESSION_PARENT_CACHE_TTL_MS: i64 = 60 * 1000;

/// 通知 body 最大字符数 (template-runtime.js `NOTIFICATION_BODY_MAX_CHARS`)。
pub const NOTIFICATION_BODY_MAX_CHARS: usize = 1000;

/// Cookie 名称: UI session token。
pub const UI_SESSION_COOKIE_NAME: &str = "oc_ui_session";

/// 将 `i64` 毫秒时间戳从 `chrono::Utc::now()` 获取。
pub fn now_millis() -> i64 {
    chrono::Utc::now().timestamp_millis()
}
