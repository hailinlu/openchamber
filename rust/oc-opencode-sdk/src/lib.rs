//! `oc-opencode-sdk` — OpenCode 服务端客户端。
//!
//! 替换 `@opencode-ai/sdk` 在服务端的用法 (162 个文件中引用的类型与调用)。
//!
//! 当前为阶段 0 脚手架。后续阶段将逐步实现:
//!   - `OpencodeClient` (typed HTTP 客户端, 封装 reqwest)
//!   - 会话/消息/Part/权限/工具调用/MCP/agent/command 的类型与方法
//!   - SSE 事件流读取 (与 packages/ui/src/sync/event-pipeline.ts 对齐)
//!   - 认证头注入 (managed-password bearer)
//!
//! 契约来源: `@opencode-ai/sdk@1.17.18` (pinned)。

pub mod client;

pub use client::OpencodeClient;
