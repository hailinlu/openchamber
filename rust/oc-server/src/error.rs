//! axum 错误桥: 将 `oc_core::Error` 转换为 HTTP 响应。
//!
//! 由于 orphan rule, 不能直接 `impl IntoResponse for oc_core::Error`。
//! 使用 newtype wrapper 在 oc-server crate 内实现 trait 转换。
//!
//! 所有 Phase 3 handler 返回 `Result<T, ApiError>`, 其中 `ApiError` 包装 `oc_core::Error`。
//! 通过 `?` 和 `From` 转换自动传播。输出 wire 格式 `{ "error": "<message>" }`。

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;

/// API 错误 wrapper (newtype 绕过 orphan rule)。
pub struct ApiError(pub oc_core::Error);

impl From<oc_core::Error> for ApiError {
    fn from(e: oc_core::Error) -> Self {
        ApiError(e)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.0.http_status())
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        (status, Json(self.0.to_json())).into_response()
    }
}

/// 所有 handler 的统一 Result 类型。
pub type ApiResult<T> = Result<T, ApiError>;
