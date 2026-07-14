//! 应用全局状态。
//!
//! 通过 `Arc<AppState>` 在 axum handler 间共享。

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use crate::config::Config;

/// 应用全局状态。
///
/// 在启动时构建, 通过 `Arc<AppState>` 注入到所有 axum handler。
pub struct AppState {
    /// CLI/env 配置。
    pub config: Config,
    /// 版本号 (来自 CARGO_PKG_VERSION)。
    pub version: &'static str,
    /// 启动时间 (ISO 8601)。
    pub started_at: String,
    /// OpenCode base URL (例如 `http://127.0.0.1:4096`)。
    pub opencode_base_url: String,
    /// OpenCode Basic auth header (例如 `Basic <base64>`)。
    pub opencode_auth_header: String,
    /// OpenCode 是否就绪 (原子标志, 供 proxy 检查)。
    pub opencode_ready: Arc<AtomicBool>,
    /// HTTP 客户端 (供 proxy 流式转发, 复用连接池)。
    pub http_client: reqwest::Client,
}

impl AppState {
    pub fn new(
        config: Config,
        opencode_base_url: String,
        opencode_auth_header: String,
    ) -> Self {
        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(4 * 60))
            .build()
            .expect("failed to build proxy http client");

        Self {
            config,
            version: env!("CARGO_PKG_VERSION"),
            started_at: chrono::Utc::now().to_rfc3339(),
            opencode_base_url,
            opencode_auth_header,
            opencode_ready: Arc::new(AtomicBool::new(false)),
            http_client,
        }
    }

    /// 标记 OpenCode 就绪。
    pub fn set_opencode_ready(&self, ready: bool) {
        self.opencode_ready
            .store(ready, std::sync::atomic::Ordering::Relaxed);
    }
}
