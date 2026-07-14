//! 文件系统路由模块。
//!
//! 对应现有: `packages/web/server/lib/fs/`。
//!
//! 15 个路由, 与 Node 侧 `/api/fs/*` 契约字节对齐:
//!   - grant (outside-workspace 授权)
//!   - home / stat / read / raw / serve
//!   - write / delete / rename / mkdir / list / reveal
//!   - clone (git clone)
//!   - exec / exec/:jobId (命令执行)

pub mod exec;
pub mod grants;
pub mod operations;
pub mod routes;
pub mod serve;
pub mod workspace;

/// Grant TTL: 10 分钟 (对应 Node `OUTSIDE_FILE_GRANT_TTL_MS`)。
pub const GRANT_TTL_SECS: u64 = 10 * 60;

/// Exec job TTL: 30 分钟 (对应 Node `EXEC_JOB_TTL_MS`)。
pub const EXEC_JOB_TTL_SECS: u64 = 30 * 60;

/// Serve 最大字节数: 100 MiB (对应 Node `MAX_SERVE_BYTES`)。
pub const MAX_SERVE_BYTES: usize = 100 * 1024 * 1024;

/// 默认命令执行超时: 5 分钟。
pub const DEFAULT_EXEC_TIMEOUT_SECS: u64 = 5 * 60;
