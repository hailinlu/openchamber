//! 实时传输层 — SSE 事件流 + WebSocket 桥。
//!
//! 对应 `packages/web/server/lib/event-stream/` 的全部模块。
//!
//! 架构:
//!   - SSE 透传代理 (`/api/event`, `/api/global/event`):
//!     纯 chunk 透传 + 边界感知心跳。对应 `proxy.js` 的 `forwardSseRequest`。
//!   - WS 全局桥 (`/api/global/event/ws`):
//!     共享上游 reader → broadcast → N 个浏览器 WS 客户端 + replay ring。
//!     对应 `global-hub.js` + `global-ws-bridge.js`。
//!   - WS 目录桥 (`/api/event/ws`):
//!     每连接独享上游 reader, 无 replay。对应 `directory-ws-bridge.js`。
//!   - 上游 SSE reader: stall 检测 + 无声重连 + Last-Event-ID 跨重连持久。
//!     对应 `upstream-reader.js`。

pub mod global_hub;
pub mod protocol;
pub mod sse_proxy;
pub mod upstream_reader;
pub mod ws_bridge;

use std::time::Duration;

// --- WS 常量 (对应 protocol.js) ---

/// 全局事件 WS 路径。
#[allow(dead_code)]
pub const GLOBAL_WS_PATH: &str = "/api/global/event/ws";
/// 目录事件 WS 路径。
#[allow(dead_code)]
pub const DIRECTORY_WS_PATH: &str = "/api/event/ws";
/// WS 心跳间隔 (ping + synthetic heartbeat)。
pub const WS_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);
/// WS 客户端缓冲硬上限 — 超过则 close(1013)。
pub const WS_MAX_BUFFERED_BYTES: usize = 16 * 1024 * 1024;
/// WS 背压警告阈值 — 超过则发一次 backpressure 帧。
pub const WS_BACKPRESSURE_WARN_BYTES: usize = 12 * 1024 * 1024;

// --- SSE 代理常量 (对应 proxy.js) ---

/// SSE 透传代理心跳间隔。
pub const SSE_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(20);
/// SSE 边界跟踪器尾部缓冲上限 (字符数)。
pub const SSE_BOUNDARY_TAIL_LIMIT: usize = 4096;

// --- 上游 reader 常量 (对应 upstream-reader.js) ---

/// 上游 stall 超时 (单会话)。
pub const UPSTREAM_STALL_TIMEOUT: Duration = Duration::from_secs(20);
/// 上游 stall 超时 (多并发会话)。
#[allow(dead_code)]
pub const UPSTREAM_STALL_TIMEOUT_CONCURRENT: Duration = Duration::from_secs(60);
/// 上游重连延迟。
pub const UPSTREAM_RECONNECT_DELAY: Duration = Duration::from_millis(250);

// --- 全局 hub 常量 (对应 global-hub.js) ---

/// 全局 replay ring buffer 容量。
pub const GLOBAL_REPLAY_LIMIT: usize = 2048;
/// broadcast channel 容量 (fan-out 到 N 个 WS 客户端)。
pub const HUB_BROADCAST_CAPACITY: usize = 4096;
