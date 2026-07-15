//! 应用全局状态。
//!
//! 通过 `Arc<AppState>` 在 axum handler 间共享。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::fs::exec::ExecJobStore;
use crate::fs::grants::GrantStore;
use crate::github::rate_limit::RateLimitState;
use crate::realtime::global_hub::GlobalHub;
use crate::tunnels::managed_config::ManagedConfigRuntime;
use crate::tunnels::service::{TunnelRuntimeState, TunnelService};
use crate::tunnels::tunnel_auth::TunnelAuth;

/// PR status 缓存最大条目数。
const PR_STATUS_CACHE_MAX: usize = 200;

/// PR status 缓存条目 (github 模块)。
pub struct PrStatusCacheEntry {
    pub data: serde_json::Value,
    pub fetched_at: Instant,
}

/// PR status 缓存 (github 模块)。
pub struct PrStatusCache {
    entries: std::sync::Mutex<HashMap<String, PrStatusCacheEntry>>,
    max_entries: usize,
}

impl PrStatusCache {
    pub fn new(max_entries: usize) -> Self {
        Self {
            entries: std::sync::Mutex::new(HashMap::new()),
            max_entries,
        }
    }

    pub fn get(&self, key: &str) -> Option<PrStatusCacheEntry> {
        let entries = self.entries.lock().unwrap();
        entries.get(key).cloned()
    }

    pub fn insert(&self, key: String, data: serde_json::Value) {
        let mut entries = self.entries.lock().unwrap();
        if entries.len() >= self.max_entries && !entries.contains_key(&key) {
            // evict 最旧
            if let Some((oldest_key, _)) = entries
                .iter()
                .min_by_key(|(_, entry)| entry.fetched_at)
                .map(|(k, v)| (k.clone(), v.fetched_at))
            {
                entries.remove(&oldest_key);
            }
        }
        entries.insert(
            key,
            PrStatusCacheEntry {
                data,
                fetched_at: Instant::now(),
            },
        );
    }
}

impl Clone for PrStatusCacheEntry {
    fn clone(&self) -> Self {
        Self {
            data: self.data.clone(),
            fetched_at: self.fetched_at,
        }
    }
}

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
    /// GitHub PR status 缓存。
    pub github_pr_status_cache: Arc<PrStatusCache>,
    /// GitHub rate-limit 状态。
    pub github_rate_limit: Arc<RateLimitState>,
    /// 隧道 auth/session 控制器 (全内存)。
    pub tunnel_auth: Arc<TunnelAuth>,
    /// 隧道运行时状态 (active controller + runtime hostname/token)。
    pub tunnel_runtime: Arc<TunnelRuntimeState>,
    /// 隧道 managed config 持久化。
    pub managed_config: Arc<ManagedConfigRuntime>,
    /// 隧道服务编排。
    pub tunnel_service: Arc<TunnelService>,
    /// 获取活动端口的回调 (供隧道启动用)。
    pub get_active_port: Arc<dyn Fn() -> Option<u16> + Send + Sync>,
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

        // 隧道 runtime state (共享: tunnel_runtime + tunnel_service)
        let tunnel_runtime = Arc::new(TunnelRuntimeState::new());

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
            github_pr_status_cache: Arc::new(PrStatusCache::new(PR_STATUS_CACHE_MAX)),
            github_rate_limit: Arc::new(RateLimitState::new()),
            tunnel_auth: Arc::new(TunnelAuth::new()),
            tunnel_runtime: tunnel_runtime.clone(),
            managed_config: Arc::new(ManagedConfigRuntime::new()),
            tunnel_service: Arc::new(TunnelService::new(tunnel_runtime)),
            get_active_port: Arc::new(|| None),
        }
    }

    /// 标记 OpenCode 就绪。
    pub fn set_opencode_ready(&self, ready: bool) {
        self.opencode_ready
            .store(ready, std::sync::atomic::Ordering::Relaxed);
    }
}
