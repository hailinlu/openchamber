//! Terminal 模块 — PTY 会话 + WebSocket I/O 桥 + REST 路由。
//!
//! 对应 Node `packages/web/server/lib/terminal/`:
//!   - `terminal-ws-protocol.js` → `protocol.rs`
//!   - `output-replay-buffer.js` → `replay_buffer.rs`
//!   - `runtime.js` (PTY + 路由 + WS) → `pty.rs` + `session.rs` + `routes.rs`
//!
//! 路由:
//!   - POST   `/api/terminal/create`              → 创建 PTY 会话
//!   - GET    `/api/terminal/{sessionId}/stream`  → SSE 输出流回退
//!   - POST   `/api/terminal/{sessionId}/input`   → HTTP 输入
//!   - POST   `/api/terminal/{sessionId}/resize`  → 调整大小
//!   - DELETE `/api/terminal/{sessionId}`         → 关闭会话
//!   - POST   `/api/terminal/{sessionId}/restart` → 重启
//!   - POST   `/api/terminal/force-kill`          → 批量杀
//!   - WS     `/api/terminal/ws`                  → 双向 I/O + 控制帧

pub mod pty;
pub mod protocol;
pub mod replay_buffer;
pub mod routes;
pub mod session;

/// 终端 WebSocket 端点路径 (对齐 Node `TERMINAL_WS_PATH`)。
pub const TERMINAL_WS_PATH: &str = "/api/terminal/ws";
/// WS 最大入站 payload (对齐 Node `TERMINAL_WS_MAX_PAYLOAD_BYTES = 64KB`)。
pub const TERMINAL_WS_MAX_PAYLOAD_BYTES: usize = 64 * 1024;
/// 输出重放缓冲最大字节数 (对齐 Node `TERMINAL_OUTPUT_REPLAY_MAX_BYTES = 64KB`)。
pub const TERMINAL_OUTPUT_REPLAY_MAX_BYTES: usize = 64 * 1024;
/// WS 心跳间隔 (对齐 Node `TERMINAL_INPUT_WS_HEARTBEAT_INTERVAL_MS`)。
pub const TERMINAL_WS_HEARTBEAT_INTERVAL_MS: u64 = 30_000;
/// 重连速率窗口 (对齐 Node `TERMINAL_INPUT_WS_REBIND_WINDOW_MS`)。
pub const TERMINAL_WS_REBIND_WINDOW_MS: u128 = 10_000;
/// 重连速率窗口内最大次数 (对齐 Node `TERMINAL_INPUT_WS_MAX_REBINDS_PER_WINDOW`)。
pub const TERMINAL_WS_MAX_REBINDS_PER_WINDOW: usize = 20;
/// 最大并发会话数 (对齐 Node `MAX_TERMINAL_SESSIONS = 20`)。
pub const MAX_TERMINAL_SESSIONS: usize = 20;
/// 控制帧无效上限 (达到后断开连接, 对齐 Node `invalidFrames >= 10`)。
pub const TERMINAL_WS_MAX_INVALID_FRAMES: u32 = 10;
/// SSE 输出流心跳间隔 (对齐 Node `15s`)。
pub const TERMINAL_SSE_HEARTBEAT_INTERVAL_MS: u64 = 15_000;
