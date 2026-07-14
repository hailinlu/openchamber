//! 应用全局状态。
//!
//! 通过 `Arc<AppState>` 在 axum handler 间共享。

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use crate::config::Config;
use crate::fs::exec::ExecJobStore;
use crate::fs::grants::GrantStore;
use crate::realtime::global_hub::GlobalHub;

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
    /// 全局消息流 hub (单上游 SSE reader + replay + broadcast)。
    pub global_hub: Arc<GlobalHub>,
    /// Outside-workspace 文件授权存储 (fs 模块)。
    pub grant_store: Arc<GrantStore>,
    /// 命令执行 job 存储 (fs 模块)。
    pub exec_job_store: Arc<ExecJobStore>,
    /// settings.json 路径 (工作区目录解析用)。
    pub settings_path: PathBuf,
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

        let global_hub = Arc::new(GlobalHub::new(
            opencode_base_url.clone(),
            opencode_auth_header.clone(),
            http_client.clone(),
        ));

        let grant_store = Arc::new(GrantStore::new(Duration::from_secs(
            crate::fs::GRANT_TTL_SECS,
        )));
        let exec_job_store = Arc::new(ExecJobStore::new(Duration::from_secs(
            crate::fs::EXEC_JOB_TTL_SECS,
        )));

        // settings.json 路径: $OPENCHAMBER_DATA_DIR/settings.json 或 ~/.config/openchamber/settings.json
        let data_dir = std::env::var("OPENCHAMBER_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
                PathBuf::from(home).join(".config").join("openchamber")
            });
        let settings_path = data_dir.join("settings.json");

        Self {
            config,
            version: env!("CARGO_PKG_VERSION"),
            started_at: chrono::Utc::now().to_rfc3339(),
            opencode_base_url,
            opencode_auth_header,
            opencode_ready: Arc::new(AtomicBool::new(false)),
            http_client,
            global_hub,
            grant_store,
            exec_job_store,
            settings_path,
        }
    }

    /// 标记 OpenCode 就绪。
    pub fn set_opencode_ready(&self, ready: bool) {
        self.opencode_ready
            .store(ready, std::sync::atomic::Ordering::Relaxed);
    }
}
