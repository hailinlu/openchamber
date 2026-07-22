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
use crate::permission_auto_accept::PermissionAutoAcceptRuntime;
use crate::realtime::global_hub::GlobalHub;
use crate::session_assist::SessionAssistRuntime;
use crate::session_goal::SessionGoalRuntime;
use crate::small_model::SmallModelService;
use crate::tts::capability_runtime::{detect_say_tts_capability, SayTtsCapability};
use crate::tts::service::TtsService;
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

    // -----------------------------------------------------------------------
    // Permission auto-accept 模块 (阶段 3c group 1)
    // -----------------------------------------------------------------------
    /// Permission auto-accept 运行时 (策略持久化 + session lineage + auto-reply)。
    pub permission_auto_accept: Arc<PermissionAutoAcceptRuntime>,

    // -----------------------------------------------------------------------
    // Small-model 模块 (阶段 3c group 2)
    // -----------------------------------------------------------------------
    /// Small-model 服务占位 (stateless, 供路由调用 resolve/index/call 模块)。
    /// 当前未被直接读取 — 用 unit struct + Arc 为后续 group 留 per-session 缓存空间。
    #[allow(dead_code)]
    pub small_model_service: Arc<SmallModelService>,

    // -----------------------------------------------------------------------
    // Session-assist + Session-goal 模块 (阶段 3c group 3)
    // -----------------------------------------------------------------------
    /// Session-assist 运行时 (busy→idle 后 60s 静默期生成 recap + suggestion)。
    pub session_assist: Arc<SessionAssistRuntime>,
    /// Session-goal 运行时 (持久化目标 + audit + auto-continuation)。
    pub session_goal: Arc<SessionGoalRuntime>,

    // -----------------------------------------------------------------------
    // Scheduled-tasks 模块 (阶段 3c group 4)
    // -----------------------------------------------------------------------
    /// Scheduled-tasks 配置 runtime (per-project JSON 持久化)。
    pub scheduled_tasks_config: Arc<crate::scheduled_tasks::ProjectConfigRuntime>,
    /// Scheduled-tasks 状态机 (timer 队列 + 并发限制)。
    pub scheduled_tasks_runtime: Arc<crate::scheduled_tasks::ScheduledTasksRuntime>,
    /// SSE 客户端池 (`/api/gridforge/events` 注册)。
    pub gridforge_event_clients: Arc<crate::scheduled_tasks::routes::GridForgeEventClients>,

    // -----------------------------------------------------------------------
    // TTS 模块 (Text-to-Speech / Speech-to-Text)
    // -----------------------------------------------------------------------
    /// TTS 服务单例 (OpenAI TTS, stateless wrapper)。
    pub tts_service: Arc<TtsService>,
    /// macOS `say` 命令能力缓存 (startup 时探测一次, 路由 GET 返回该值)。
    /// 默认值为 `SayTtsCapability::not_initialized()`, 经探测后覆盖。
    pub say_tts_capability: Arc<tokio::sync::RwLock<SayTtsCapability>>,

    // -----------------------------------------------------------------------
    // Terminal 模块 (PTY 会话 + WS 桥)
    // -----------------------------------------------------------------------
    /// 终端会话存储 (PTY session lifecycle + idle sweep)。
    pub terminal_sessions: Arc<crate::terminal::session::TerminalSessionStore>,

    // -----------------------------------------------------------------------
    // Preview 模块 (dev server 反向代理 + WS 升级代理)
    // -----------------------------------------------------------------------
    /// Preview 目标存储 (TTL sweeper)。
    pub preview_targets: Arc<crate::preview::targets::PreviewTargetStore>,

    // -----------------------------------------------------------------------
    // Dictation 模块 (流式 STT + 本地 TTS)
    // -----------------------------------------------------------------------
    /// Dictation 服务。本轮仅 openai-compatible 提供方; local 返回桩错误。
    pub dictation_service: Arc<crate::dictation::service::DictationService>,

    // -----------------------------------------------------------------------
    // Relay 模块 (阶段 3f Group 4 step 9) — private relay host.
    // -----------------------------------------------------------------------
    /// Relay service (settings persistence + lifecycle + routes facade).
    /// `None` when the relay has not been initialized; the routes layer
    /// degrades gracefully to a "disabled" snapshot in that case. The
    /// production wire installs this in `main.rs` after building AppState.
    #[allow(dead_code)]
    pub relay_service: std::sync::Mutex<Option<Arc<crate::relay::service::RelayService>>>,
    /// 测试覆盖: 测试可以在 AppState 已共享后注入 relay_service; 路由层
    /// 优先读取这个覆盖. 生产路径忽略此字段.
    #[cfg(test)]
    pub relay_service_override: std::sync::Mutex<Option<Arc<crate::relay::service::RelayService>>>,

    // -----------------------------------------------------------------------
    // MCP auth 模块 — OAuth 状态暂存
    // -----------------------------------------------------------------------
    /// MCP OAuth pending auth context store (in-memory HashMap with TTL).
    #[allow(dead_code)]
    pub mcp_auth: Arc<crate::mcp_auth::McpAuthStore>,
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

        // settings.json 路径: $GRIDFORGE_DATA_DIR/settings.json 或 ~/.config/gridforge/settings.json
        let data_dir = std::env::var("GRIDFORGE_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
                PathBuf::from(home).join(".config").join("gridforge")
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
            permission_auto_accept: Arc::new(PermissionAutoAcceptRuntime::new()),
            small_model_service: Arc::new(SmallModelService::new()),
            session_assist: Arc::new(SessionAssistRuntime::new()),
            session_goal: Arc::new(SessionGoalRuntime::new()),

            // Scheduled-tasks 模块 (阶段 3c group 4) — config + runtime.
            // 用 user_config_root (与 Node GRIDFORGE_USER_CONFIG_ROOT 对齐, 不读 GRIDFORGE_DATA_DIR)。
            scheduled_tasks_config: Arc::new(crate::scheduled_tasks::ProjectConfigRuntime::new(
                crate::github::settings::user_config_root().join("projects"),
            )),
            scheduled_tasks_runtime: crate::scheduled_tasks::build_default_runtime(),
            gridforge_event_clients: Arc::new(
                crate::scheduled_tasks::routes::GridForgeEventClients::new(),
            ),

            // TTS 模块 — 初始化为 default; `init_say_tts_capability` 在 startup 后探测真实能力
            tts_service: Arc::new(TtsService::new()),
            say_tts_capability: Arc::new(tokio::sync::RwLock::new(
                SayTtsCapability::not_initialized(),
            )),

            // Terminal 模块 — 会话存储 (idle sweep 由 main.rs 启动)
            terminal_sessions: Arc::new(
                crate::terminal::session::TerminalSessionStore::new(),
            ),

            // Preview 模块 — 目标存储 (TTL sweeper 由 main.rs 启动)
            preview_targets: Arc::new(crate::preview::targets::PreviewTargetStore::new()),

            // Dictation 模块 — 服务 (models_dir 对齐 Node speech-models;
            // 本轮无 worker/无下载, 仅 openai-compatible 提供方)
            dictation_service: crate::dictation::service::DictationService::new(
                crate::github::settings::user_config_root().join("speech-models"),
            ),

            // MCP auth store — in-memory pending auth contexts
            mcp_auth: Arc::new(crate::mcp_auth::McpAuthStore::new()),

            // Relay 模块 — 由 main.rs 在启动时通过 `install_relay_service`
            // 注入一个带 host-lock + host_factory 的实例。当前为 None;
            // 路由层在没有 service 时返回 disabled 的 snapshot。
            relay_service: std::sync::Mutex::new(None),
            #[cfg(test)]
            relay_service_override: std::sync::Mutex::new(None),
        }
    }

    /// 安装 relay service (生产 wire 由 main.rs 在 build_router 之前调用)。
    pub fn install_relay_service(self: &Arc<Self>, svc: Arc<crate::relay::service::RelayService>) {
        self.relay_service
            .lock()
            .expect("relay_service poisoned")
            .replace(svc);
    }

    /// 测试钩子: 在 `AppState` 构造后注入一个 `RelayService`, 使 axum 路由
    /// 测试能驱动真实的状态机。仅 `#[cfg(test)]` 时暴露。
    #[cfg(test)]
    pub fn set_relay_service_for_tests(
        self: &Arc<Self>,
        svc: Arc<crate::relay::service::RelayService>,
    ) {
        // 测试路径要求 state 在 set 之后才会被 `with_state` 共享出去. 我们
        // 通过原子 store + Mutex 让 setter 即使在已共享场景下也能工作,
        // 避免测试必须控制 Arc 的唯一性.
        self.relay_service_override
            .lock()
            .expect("relay_service_override poisoned")
            .replace(svc);
    }

    /// 读取测试注入的 relay_service 覆盖; 优先于字段值返回.
    #[cfg(test)]
    fn relay_service_for_request(&self) -> Option<Arc<crate::relay::service::RelayService>> {
        if let Some(svc) = self
            .relay_service_override
            .lock()
            .expect("relay_service_override poisoned")
            .as_ref()
        {
            return Some(svc.clone());
        }
        self.relay_service
            .lock()
            .expect("relay_service poisoned")
            .clone()
    }

    /// 测试专用: 直接构造一个带 relay_service 的 AppState.
    #[cfg(test)]
    pub fn new_with_relay_for_tests(
        config: crate::config::Config,
        relay: Arc<crate::relay::service::RelayService>,
    ) -> Arc<Self> {
        let state = Arc::new(Self::new(
            config,
            "http://127.0.0.1:1".to_string(),
            String::new(),
        ));
        state.set_relay_service_for_tests(relay);
        state
    }

    /// 测试专用 AppState 构造路径, 避免在路由测试中触发完整的 OpenCode /
    /// 通知 trigger 初始化。
    #[cfg(test)]
    pub fn new_for_tests() -> Self {
        Self::new(
            crate::config::Config::for_tests(),
            "http://127.0.0.1:1".to_string(),
            String::new(),
        )
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

        // SessionStateRuntime 合成事件 fanout: session-status/session-activity →
        // SSE 通知流 + 全局 WS 桥。
        //
        // SessionStateRuntime 从上游 session.status (经上面的 GlobalHub consumer 喂入)
        // 派生合成事件。这里订阅它的 event_tx, 把合成事件扇出到:
        // 1. SSE 通知流 (emitter.write_sse_event) — 对应 Node broadcastGlobalUiEvent→sseClients
        // 2. 全局 hub WS 桥 (global_hub.broadcast_synthetic) — 对应 Node broadcastGlobalUiEvent→wsClients
        //
        // 合成事件无 event_id, 不进 replay ring, 不可 resume。
        let emitter = self.emitter.clone();
        let global_hub = self.global_hub.clone();
        let mut synthetic_rx = self.session_state.subscribe_events();
        tokio::spawn(async move {
            tracing::info!("[session-state] synthetic event fanout started");
            loop {
                match synthetic_rx.recv().await {
                    Ok(payload) => {
                        emitter.write_sse_event(&payload);
                        global_hub.broadcast_synthetic(payload);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "[session-state] synthetic fanout lagged");
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        tracing::info!("[session-state] synthetic fanout stopped");
                        break;
                    }
                }
            }
        });

        trigger
    }

    /// 初始化 permission-auto-accept 运行时, 启动 GlobalHub 事件/状态消费 task。
    ///
    /// 对应 Node `permissionAutoAcceptRuntime.start()`。
    /// 在 `set_opencode_ready(true)` 后调用。
    pub fn init_permission_auto_accept(self: &Arc<Self>) {
        self.permission_auto_accept
            .clone()
            .start(
                &self.global_hub,
                &self.opencode_base_url,
                &self.opencode_auth_header,
            );
    }

    /// 初始化 session-assist 运行时, 启动 GlobalHub 事件消费 task。
    ///
    /// 对应 Node `createSessionAssistRuntime` 的隐式启动 (Node 端由 index.js 在
    /// `start()` 后注入)。session-assist 监听 session.status (idle → 60s 静默期)
    /// + message.updated user (tail-moved-on 检查)。
    pub fn init_session_assist(self: &Arc<Self>) {
        self.session_assist.clone().start(self.clone());
    }

    /// 初始化 session-goal 运行时, 启动 GlobalHub 事件消费 task。
    ///
    /// 对应 Node `createSessionGoalRuntime` 的隐式启动。session-goal 监听
    /// session.status (idle → 15s tick) + message.updated assistant (abort pause)
    /// + session.updated (kickoff path)。
    pub fn init_session_goal(self: &Arc<Self>) {
        self.session_goal.clone().start(self.clone());
    }

    /// 初始化 scheduled-tasks 运行时 — 立即 fire-and-forget background task,
    /// runtime.start() 内部触发 `sync_all_projects()`。
    ///
    /// 对应 Node `scheduled-tasks/index.js` 在 `start()` 后注入 runtime。
    /// 应在 `set_opencode_ready(true)` 之后调用。
    pub fn init_scheduled_tasks(self: &Arc<Self>) {
        self.scheduled_tasks_runtime.clone().start(self.clone());
    }

    /// 探测 macOS `say` 能力, 写入 `say_tts_capability` 缓存。
    ///
    /// 对应 Node `index.js` 启动时调用 `detectSayTtsCapability(process)` 的结果
    /// 通过 `registerTtsRoutes(app, { sayTTSCapability })` 注入。Rust 版本在
    /// `set_opencode_ready(true)` 之后调用一次, 结果对 GET /api/tts/say/status
    /// 立即可见。
    pub async fn init_say_tts_capability(self: &Arc<Self>) {
        let capability = detect_say_tts_capability().await;
        tracing::info!(
            "[tts] say capability probed: available={} voices={}",
            capability.available,
            capability.voices.len()
        );
        let mut guard = self.say_tts_capability.write().await;
        *guard = capability;
    }
}
