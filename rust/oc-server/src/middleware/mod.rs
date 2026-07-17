//! axum 中间件层。
//!
//! 当前提供全局认证中间件 `auth::require_api_auth`,对齐 Node `requireApiAuth`
//! (`core-routes.js:595-609`),覆盖所有受保护路由。
//!
//! 以及 CORS 中间件 `cors::cors_layer`,对齐 Node CORS 头逻辑
//! (`index.js:1328-1344`),允许本地开发 (localhost/127.0.0.1:any-port) 与
//! packaged client origins 跨域访问。

pub mod auth;
pub mod cors;
