//! `oc-server` — OpenChamber Rust 后端入口 (阶段 0 脚手架)。
//!
//! 替换目标: `packages/web/server/index.js` (Express, ~1691 行)。
//!
//! 当前: 仅启动 axum 监听 + `/health` 端点, 验证依赖链编译。
//! 阶段 1 会加入:
//!   - CLI/env 解析 (clap, 对应 lib/opencode/cli-options.js)
//!   - 绑定地址安全检查 (lib/security/bind-host.js 等价)
//!   - OpenCode 进程生命周期 (spawn/restart/shutdown)
//!   - OpenCode HTTP/SSE 代理
//!   - 静态 dist 托管 + SPA fallback

mod config;

use axum::{routing::get, Router};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let config = config::Config::load()?;
    tracing::info!(port = config.port, host = %config.host, "oc-server 启动中");

    let app = Router::new().route("/health", get(health));

    let listener = tokio::net::TcpListener::bind((config.host, config.port)).await?;
    tracing::info!("监听 http://{}", listener.local_addr()?);
    axum::serve(listener, app).await?;

    Ok(())
}

/// 健康检查端点。对应现有 `/health`。
async fn health() -> &'static str {
    "ok"
}
