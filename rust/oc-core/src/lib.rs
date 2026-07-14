//! `oc-core` — 跨 crate 共享的类型与错误定义。
//!
//! 当前为阶段 0 脚手架。后续阶段会按现有 `packages/web/server` 与
//! `@opencode-ai/sdk` 的契约逐步填入:
//!   - API 请求/响应 schema (与 `/api/*` 字节对齐)
//!   - OpenCode 会话/消息/Part/权限/工具调用类型
//!   - SSE 事件与 WebSocket 帧协议类型
//!   - 统一错误类型 (映射到 HTTP 状态码)

pub mod error;

pub use error::{Error, Result};
