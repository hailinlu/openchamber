//! axum 中间件层。
//!
//! 当前提供全局认证中间件 `auth::require_api_auth`,对齐 Node `requireApiAuth`
//! (`core-routes.js:595-609`),覆盖所有受保护路由。

pub mod auth;
