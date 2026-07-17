//! `oc-server` — GridForge Rust 后端二进制入口。
//!
//! Phase 4B: 瘦壳, 启动编排委托给 `oc_server::OcServer`。
//! 替换目标: `packages/web/server/index.js` (Express)。

use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config = oc_server::Config::load()?;
    let server = oc_server::OcServer::start(config).await?;

    // 就绪行 (供 sidecar/Tauri 解析, 同 Node 的 ready 行)
    println!("gridforge server listening on {}", server.base_url());

    // 等待 Ctrl-C (SIGINT) 或 SIGTERM, 任一到达即触发优雅关闭。
    // SIGTERM 对进程管理器 (systemd / launchd / kill <pid>) 很重要:
    // 缺失它会导致 opencode 子进程和 PTY 会话成为孤儿 (直到 OpenCodeHandle::Drop 的
    // SIGKILL 兜底, 但那会跳过 2.5s 的优雅窗口)。OcServer::shutdown 本身是
    // 调用方驱动的 (与信号无关), 所以这里只负责把 "信号到达" 翻译成 "调用 shutdown"。
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("received SIGINT, starting graceful shutdown"),
        _ = terminate => tracing::info!("received SIGTERM, starting graceful shutdown"),
    }

    server.shutdown().await;
    Ok(())
}
