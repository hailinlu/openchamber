//! 应用全局状态。
//!
//! 通过 `Arc<AppState>` 在 axum handler 间共享。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::client_auth::pairing::ClientPairingRuntime;
use crate::client_auth::remote_clients::RemoteClientAuthRuntime;
use crate::fs::exec::ExecJobStore;
use crate::fs::grants::GrantStore;
use crate::github::rate_limit::RateLimitState;
use crate::notifications::apns_send::ApnsSendRuntime;
use crate::notifications::apns_store::ApnsStore;
use crate::notifications::emitter::NotificationEmitter;
use crate::notifications::push_send::PushSendRuntime;
use crate::notifications::push_store::PushStore;
use crate::notifications::session_state::SessionStateRuntime;
use crate::notifications::template::NotificationTemplateRuntime;
use crate::notifications::trigger::NotificationTrigger;
use crate::realtime::global_hub::GlobalHub;
use crate::tunnels::managed_config::ManagedConfigRuntime;
use crate::tunnels::service::{TunnelRuntimeState, TunnelService};
use crate::tunnels::tunnel_auth::TunnelAuth;
use crate::ui_auth::UiAuth;

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
    /// UI 认证控制器 (password session / JWT / rate-limit / URL-token / passkeys)。
    pub ui_auth: Arc<UiAuth>,
    /// Remote client 认证 (trusted-device bearer token 存储, 无密码模式为 None)。
    pub remote_client_auth: Option<Arc<RemoteClientAuthRuntime>>,
    /// Pairing session runtime (无密码模式为 None)。
    pub client_pairing: Option<Arc<ClientPairingRuntime>>,

    // -----------------------------------------------------------------------
    // Notifications 模块 (阶段 3b group 4)
    // -----------------------------------------------------------------------
    /// Web-push 订阅持久化 + 可见性 Map。
    pub push_store: Arc<PushStore>,
    /// APNs token 持久化。
    pub apns_store: Arc<ApnsStore>,
    /// SSE 通知 emitter (broadcast + desktop notify)。
    pub emitter: Arc<NotificationEmitter>,
    /// Session 状态运行时 (activity/status/attention)。
    pub session_state: Arc<SessionStateRuntime>,
    /// 通知模板运行时 (变量解析 + git branch)。
    pub notification_template: Arc<NotificationTemplateRuntime>,
    /// Web-push 发送运行时。
    pub push_send: Arc<PushSendRuntime>,
    /// APNs 发送运行时 (relay + direct)。
    pub apns_send: Arc<ApnsSendRuntime>,
    /// Notification trigger fanout (从 GlobalHub 订阅事件, 触发推送)。
    /// 延迟初始化: 需要 `Arc<AppState>` 构建后才能创建 trigger (trigger 的方法接收 `self: Arc<Self>`)。
    pub notification_trigger: Arc<tokio::sync::OnceCell<Arc<NotificationTrigger>>>,
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

        // UI auth + client auth
        let normalized_password = config
            .ui_password
            .as_deref()
            .map(crate::ui_auth::types::normalize_password)
            .unwrap_or("");
        let require_client_auth = config.require_client_auth;

        let (ui_auth, remote_client_auth, client_pairing) = if !normalized_password.is_empty() {
            // enabled controller
            let jwt_secret = crate::ui_auth::jwt_secret::get_or_create_jwt_secret();
            let remote_rt = Arc::new(RemoteClientAuthRuntime::new());
            let pairing_rt = Arc::new(ClientPairingRuntime::new(remote_rt.clone()));
            let ui = crate::ui_auth::UiAuth::new_enabled(
                normalized_password,
                jwt_secret,
                Some(remote_rt.clone()),
            );
            (
                Arc::new(ui),
                Some(remote_rt),
                Some(pairing_rt),
            )
        } else {
            // disabled controller
            let remote_rt = if require_client_auth {
                Some(Arc::new(RemoteClientAuthRuntime::new()))
            } else {
                None
            };
            let pairing_rt = remote_rt
                .as_ref()
                .map(|rt| Arc::new(ClientPairingRuntime::new(rt.clone())));
            let ui = crate::ui_auth::UiAuth::new_disabled(remote_rt.clone(), require_client_auth);
            (Arc::new(ui), remote_rt, pairing_rt)
        };

        // -----------------------------------------------------------------------
        // Notifications 模块 (阶段 3b group 4)
        // -----------------------------------------------------------------------
        let push_store = Arc::new(PushStore::new(data_dir.join("push-subscriptions.json")));
        let apns_store = Arc::new(ApnsStore::new(data_dir.join("apns-tokens.json")));
        let emitter = Arc::new(NotificationEmitter::new());
        let session_state = Arc::new(SessionStateRuntime::new());
        let notification_template =
            Arc::new(NotificationTemplateRuntime::new(http_client.clone()));
        let push_send = Arc::new(PushSendRuntime::new(push_store.clone(), http_client.clone()));
        let apns_send = Arc::new(ApnsSendRuntime::new(
            apns_store.clone(),
            http_client.clone(),
        ));

        Self {
            config,
            version: env!("CARGO_PKG_VERSION"),
            started_at: chrono::Utc::now().to_rfc3339(),
            opencode_base_url: opencode_base_url.clone(),
            opencode_auth_header: opencode_auth_header.clone(),
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
            ui_auth,
            remote_client_auth,
            client_pairing,
            push_store,
            apns_store,
            emitter,
            session_state,
            notification_template,
            push_send,
            apns_send,
            notification_trigger: Arc::new(tokio::sync::OnceCell::new()),
        }
    }

    /// 标记 OpenCode 就绪。
    pub fn set_opencode_ready(&self, ready: bool) {
        self.opencode_ready
            .store(ready, std::sync::atomic::Ordering::Relaxed);
    }

    /// 初始化 notification trigger fanout, 启动 GlobalHub 事件消费 task。
    ///
    /// 必须在 `Arc<AppState>` 构建后调用。创建 `NotificationTrigger` 并订阅
    /// GlobalHub 事件流, 对每个事件调用 `maybe_send_push_for_trigger` +
    /// `session_state.process_sse_payload`。
    ///
    /// 幂等: 重复调用安全。
    pub fn init_notification_trigger(self: &Arc<Self>) -> Arc<NotificationTrigger> {
        let trigger = Arc::new(NotificationTrigger::new(
            self.push_store.clone(),
            self.emitter.clone(),
            self.notification_template.clone(),
            self.push_send.clone(),
            self.apns_send.clone(),
            self.session_state.clone(),
            self.opencode_base_url.clone(),
            self.opencode_auth_header.clone(),
        ));

        // 注册到 OnceCell (首次调用设置; 后续调用忽略)
        let _ = self.notification_trigger.set(trigger.clone());

        // 启动后台 GlobalHub 消费 task
        let trigger_clone = trigger.clone();
        let session_state = self.session_state.clone();
        let mut rx = self.global_hub.subscribe_event();
        tokio::spawn(async move {
            tracing::info!("[notifications] trigger fanout consumer started");
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        // 更新 session state (session.status → activity phase + status map)
                        session_state.process_sse_payload(&event.payload);

                        // trigger fanout (clone Arc, fire-and-forget)
                        let trigger = trigger_clone.clone();
                        let payload = event.payload.clone();
                        tokio::spawn(async move {
                            trigger.maybe_send_push_for_trigger(payload).await;
                        });
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "[notifications] trigger consumer lagged");
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        tracing::info!("[notifications] trigger consumer stopped (hub closed)");
                        break;
                    }
                }
            }
        });

        trigger
    }
}
