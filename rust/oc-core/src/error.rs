//! 统一错误类型。
//!
//! 设计目标 (与现有 Node 后端行为对齐):
//!   - `Error::BadRequest`         → HTTP 400
//!   - `Error::Unauthorized`       → HTTP 401
//!   - `Error::Forbidden`          → HTTP 403
//!   - `Error::NotFound`           → HTTP 404
//!   - `Error::ServiceUnavailable` → HTTP 503
//!   - `Error::Internal`           → HTTP 500
//!   - `Error::Upstream`           → OpenCode 代理上游错误 (透传状态码)
//!   - `Error::Io`                 → 文件系统/进程错误 (通常 500)
//!   - `Error::Serde`              → 序列化错误 (通常 500)

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

    #[error("unauthorized: {0}")]
    Unauthorized(String),

    #[error("forbidden: {0}")]
    Forbidden(String),

    #[error("service unavailable: {0}")]
    ServiceUnavailable(String),

    #[error("upstream (opencode) error: {status} {body}")]
    Upstream { status: u16, body: String },

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("serde error: {0}")]
    Serde(#[from] serde_json::Error),
}

impl Error {
    /// 返回该错误对应的 HTTP 状态码。
    pub fn http_status(&self) -> u16 {
        match self {
            Error::BadRequest(_) => 400,
            Error::Unauthorized(_) => 401,
            Error::Forbidden(_) => 403,
            Error::NotFound(_) => 404,
            Error::ServiceUnavailable(_) => 503,
            Error::Upstream { status, .. } => *status,
            Error::Internal(_) | Error::Io(_) | Error::Serde(_) => 500,
        }
    }

    /// 返回该错误的 JSON 错误体: `{ "error": "<message>" }`。
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({ "error": self.to_string() })
    }
}
