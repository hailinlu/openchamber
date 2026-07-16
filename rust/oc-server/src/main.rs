//! `oc-server` — OpenChamber Rust 后端二进制入口。
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
    println!("openchamber server listening on {}", server.base_url());

    // 等待 Ctrl-C / SIGTERM
    tokio::signal::ctrl_c()
        .await
        .expect("failed to install Ctrl+C handler");

    server.shutdown().await;
    Ok(())
}
