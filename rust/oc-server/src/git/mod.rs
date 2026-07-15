//! Git 模块 — 迁移自 `packages/web/server/lib/git/`。
//!
//! 所有 git 操作通过 `tokio::process::Command` 调用 `git` CLI 二进制,
//! 不使用 `simple-git` 或 `git2` crate。
//!
//! 与 Node 后端契约字节对齐: 路径、JSON schema、HTTP 状态码、错误消息。
//!
//! 部分辅助函数/常量目前未被直接引用, 但为后续 worktree bootstrap、
//! range diff、credentials 等功能预留。在模块级别允许 dead_code。

#![allow(dead_code)]

pub mod branch;
pub mod commit;
pub mod context;
pub mod diff;
pub mod identity;
pub mod integrate;
pub mod log;
pub mod merge_rebase;
pub mod parsing;
pub mod paths;
pub mod remote;
pub mod routes;
pub mod runner;
pub mod stash;
pub mod status;
pub mod worktree;

/// 远程存在性缓存 TTL (与 Node REMOTE_EXISTENCE_CACHE_TTL_MS 一致)。
pub const REMOTE_EXISTENCE_CACHE_TTL_SECS: u64 = 30;

/// 新文件 diff stats 上限 (与 Node MAX_NEW_FILE_STATS 一致)。
pub const MAX_NEW_FILE_STATS: usize = 200;

/// 新文件 diff stats 最大文件大小 (与 Node MAX_NEW_FILE_STAT_SIZE 一致, 1MB)。
pub const MAX_NEW_FILE_STAT_SIZE: u64 = 1024 * 1024;

/// git 命令 stdout 最大缓冲 (与 Node maxBuffer 一致, 20MB)。
pub const MAX_GIT_STDOUT_BYTES: usize = 20 * 1024 * 1024;

/// Worktree bootstrap 状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootstrapStatus {
    Pending,
    Ready,
    Failed,
}

impl BootstrapStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            BootstrapStatus::Pending => "pending",
            BootstrapStatus::Ready => "ready",
            BootstrapStatus::Failed => "failed",
        }
    }
}

/// Worktree bootstrap 状态条目。
#[derive(Debug, Clone)]
pub struct BootstrapEntry {
    pub status: BootstrapStatus,
    pub error: Option<String>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}
