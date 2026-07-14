//! 统一错误类型。
//!
//! 设计目标 (与现有 Node 后端行为对齐):
//!   - `Error::Internal`     → HTTP 500
//!   - `Error::NotFound`     → HTTP 404
//!   - `Error::BadRequest`   → HTTP 400
//!   - `Error::Upstream`     → OpenCode 代理上游错误 (透传状态码)
//!   - `Error::Io`           → 文件系统/进程错误 (通常 500)

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("internal error: {0}")]
    Internal(String),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("bad request: {0}")]
    BadRequest(String),

    #[error("upstream (opencode) error: {status} {body}")]
    Upstream { status: u16, body: String },

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("serde error: {0}")]
    Serde(#[from] serde_json::Error),
}
