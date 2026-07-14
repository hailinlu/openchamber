//! `oc-server` — OpenChamber Rust 后端入口。
//!
//! 替换目标: `packages/web/server/index.js` (Express, ~1691 行)。
//!
//! 阶段 1 实现的垂直切片:
//!   - CLI/env 解析 (clap, 对应 cli-options.js)
//!   - 绑定地址安全检查 (bind-host.js 等价)
//!   - OpenCode 进程生命周期 (spawn + 就绪门 + 优雅关闭)
//!   - OpenCode HTTP 代理 (/api/* catch-all, 流式转发)
//!   - 静态 dist 托管 + SPA fallback
//!   - /health, /api/version, /api/system/info, /robots.txt
//!
//! 后续阶段:
//!   - 阶段 2: SSE/WS 实时层
//!   - 阶段 3: 功能模块路由 (fs/git/github/terminal/...)

mod bind_host;
mod config;
mod opencode;
mod proxy;
mod routes;
mod state;
mod static_files;

use std::sync::Arc;

use anyhow::Context;
use axum::routing::{any, get};
use axum::Router;
use tracing_subscriber::EnvFilter;

use crate::config::Config;
use crate::state::AppState;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let config = Config::load()?;

    // 1. 绑定安全检查 (拒绝未认证 LAN)
    bind_host::enforce(&config)?;

    // 2. 启动 OpenCode (managed spawn 或 external attach)
    let (oc_base_url, oc_auth, mut oc_handle) = opencode::start(&config)
        .await
        .context("failed to start opencode")?;

    // 3. 构建 AppState
    let state = Arc::new(AppState::new(config.clone(), oc_base_url, oc_auth));
    state.set_opencode_ready(true);

    // 4. 构建路由
    let app = build_router(state.clone(), &config);

    // 5. 绑定 + 优雅关闭
    let listener = tokio::net::TcpListener::bind((config.host, config.port)).await?;
    let local_addr = listener.local_addr()?;
    tracing::info!(
        addr = %local_addr,
        "oc-server listening (runtime=rust, version={})",
        state.version
    );

    // 就绪行 (供 sidecar/Tauri 解析, 同 Node 的 `openchamber:ready` IPC)
    println!("openchamber server listening on http://{}", local_addr);

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    // 6. 关闭 OpenCode 子进程
    tracing::info!("shutting down opencode process");
    oc_handle.shutdown().await;
    tracing::info!("oc-server stopped");

    Ok(())
}

/// 构建完整路由树。
fn build_router(state: Arc<AppState>, config: &Config) -> Router {
    // 具体路由优先于 catch-all。
    // /api/version 和 /api/system/info 是具体路由, axum 会优先匹配。
    // 其余 /api/* 走 proxy catch-all。
    let mut router = Router::new()
        // 状态端点
        .route("/health", get(routes::health))
        .route("/api/version", get(routes::version))
        .route("/api/system/info", get(routes::system_info))
        .route("/robots.txt", get(routes::robots_txt))
        // OpenCode 反向代理 (/api/* catch-all)
        // nest 会剥离 /api 前缀, proxy_handler 收到的 path 是去掉 /api 后的部分。
        // 具体路由 (/api/version, /api/system/info) 已在上面注册, axum 优先匹配。
        .nest("/api", Router::new().fallback(any(proxy::proxy_handler)));

    // 静态 dist 托管 + SPA fallback
    if config.api_only {
        router = router.fallback(static_files::headless_fallback);
    } else if let Some(dist_dir) = config.resolve_dist_dir() {
        if dist_dir.exists() && dist_dir.is_dir() {
            // ServeDir 处理静态文件; 不存在时 fallback 到 SPA service。
            let serve_dir = static_files::build_dist_service(&dist_dir);
            if let Some(serve_dir) = serve_dir {
                let spa = static_files::SpaFallback::new(&dist_dir);
                router = router.fallback_service(serve_dir.fallback(spa));
            } else {
                router = router.fallback(static_files::headless_fallback);
            }
        } else {
            router = router.fallback(static_files::headless_fallback);
        }
    } else {
        // 无 dist_dir → headless fallback
        router = router.fallback(static_files::headless_fallback);
    }

    router.with_state(state)
}

/// 等待 SIGINT/SIGTERM, 触发优雅关闭。
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {
            tracing::info!("received SIGINT, starting graceful shutdown");
        }
        _ = terminate => {
            tracing::info!("received SIGTERM, starting graceful shutdown");
        }
    }
}
