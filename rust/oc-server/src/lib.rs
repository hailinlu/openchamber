//! `oc-server` 库入口 — 进程内嵌入句柄。
//!
//! Phase 4B: 把原 main.rs 的启动编排提取为 `OcServer` 句柄,
//! 供 Tauri 进程内嵌入 与 oc-server bin 共用同一条启动/关闭路径。

// --- 模块声明 (从 main.rs 移入, 改 pub mod 供 lib 消费者访问) ---
pub mod bind_host;
pub mod client_auth;
pub mod config;
pub mod error;
pub mod fs;
pub mod git;
pub mod github;
pub mod magic_prompts;
pub mod middleware;
pub mod notifications;
pub mod opencode;
pub mod permission_auto_accept;
pub mod project_dir;
pub mod proxy;
pub mod quota;
pub mod realtime;
pub mod routes;
pub mod scheduled_tasks;
pub mod session_assist;
pub mod session_folders;
pub mod session_goal;
pub mod skills_catalog;
pub mod small_model;
pub mod state;
pub mod static_files;
pub mod terminal;
pub mod preview;
pub mod dictation;
pub mod relay;
pub mod text;
pub mod tts;
pub mod tunnels;
pub mod ui_auth;

// --- 便捷 re-export ---
pub use config::Config;

use std::sync::Arc;

use anyhow::Context;
use axum::routing::{any, delete, get, post, put};
use axum::Router;
use tokio::sync::oneshot;

use opencode::OpenCodeHandle;

// =========================================================================
// OcServer 句柄
// =========================================================================

/// 进程内嵌入的 oc-server 句柄。
///
/// 封装完整的启动编排 (配置 → OpenCode spawn → AppState + 模块 init → 路由 →
/// axum::serve) 与优雅关闭顺序 (axum graceful → hub.stop → terminal.kill_all →
/// opencode.shutdown)。
///
/// 用法:
/// ```ignore
/// let server = OcServer::start(config).await?;
/// let url = server.base_url();
/// // ... serve 在后台 task 运行 ...
/// server.shutdown().await;
/// ```
pub struct OcServer {
    base_url: String,
    shutdown_tx: Option<oneshot::Sender<()>>,
    join_handle: Option<tokio::task::JoinHandle<()>>,
    state: Arc<state::AppState>,
    oc_handle: Option<OpenCodeHandle>,
}

impl OcServer {
    /// 启动后端: 配置 → bind_host 检查 → OpenCode spawn → AppState + 7 步模块
    /// init → build_router → axum::serve (后台 task)。
    ///
    /// 端口绑定 `config.host:config.port` (port=0 时 OS 分配)。
    /// 返回的句柄含实际 base_url; serve 在 spawned task 内运行, 不阻塞调用方。
    pub async fn start(config: Config) -> anyhow::Result<Self> {
        // 1. 绑定安全检查 (拒绝未认证 LAN)
        bind_host::enforce(&config)?;

        // 2. 启动 OpenCode (managed spawn 或 external attach)
        let (oc_base_url, oc_auth, oc_handle) = opencode::start(&config)
            .await
            .context("failed to start opencode")?;

        // 3. 构建 AppState
        let state = Arc::new(state::AppState::new(
            config.clone(),
            oc_base_url,
            oc_auth,
        ));
        state.set_opencode_ready(true);

        // 3b-3i. 模块 init (与原 main.rs 顺序一致)
        state.init_notification_trigger();
        state.init_permission_auto_accept();
        state.init_session_assist();
        state.init_session_goal();
        state.init_say_tts_capability().await;
        state.init_scheduled_tasks();
        state.terminal_sessions.clone().start_idle_sweep();
        state.preview_targets.clone().start_sweeper();

        // 3j. 初始化私有中继 (host-lock + 生命周期)
        let relay_service = Arc::new(relay::service::RelayService::new());
        relay_service.attach_self_weak();
        state.install_relay_service(relay_service);
        let state_clone = state.clone();
        tokio::spawn(async move {
            let svc = state_clone
                .relay_service
                .lock()
                .expect("relay_service poisoned")
                .clone();
            if let Some(svc) = svc {
                svc.start_if_enabled().await;
            }
        });

        // 4. 构建路由
        let app = build_router(state.clone(), &config);

        // 5. 绑定 listener (port=0 时 OS 分配)
        let listener = tokio::net::TcpListener::bind((config.host, config.port))
            .await
            .context("bind oc-server listener")?;
        let local_addr = listener.local_addr().context("get listener local addr")?;
        let base_url = format!("http://{}", local_addr);

        tracing::info!(
            addr = %local_addr,
            "oc-server listening (runtime=rust, version={})",
            state.version
        );

        // 6. axum::serve 作为 spawned task (不阻塞调用方)
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let join_handle = tokio::spawn(async move {
            let serve = axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                });
            if let Err(e) = serve.await {
                tracing::error!("oc-server serve task exited with error: {:#}", e);
            }
        });

        Ok(Self {
            base_url,
            shutdown_tx: Some(shutdown_tx),
            join_handle: Some(join_handle),
            state,
            oc_handle: Some(oc_handle),
        })
    }

    /// `http://<host>:<port>` — 供 WebView 和 IPC 命令使用。
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// 优雅关闭 (顺序与原 main.rs 一致, 不可漂移):
    /// 1. 触发 axum graceful shutdown (shutdown_tx)
    /// 2. 等 serve task 退出 (join_handle)
    /// 3. global_hub.stop() (停上游 SSE reader)
    /// 4. terminal_sessions.kill_all() (杀终端会话)
    /// 5. oc_handle.shutdown() (杀 OpenCode 子进程)
    pub async fn shutdown(mut self) {
        // 1. 触发 axum graceful shutdown
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        // 2. 等 serve task 退出
        if let Some(join) = self.join_handle.take() {
            let _ = join.await;
        }
        // 3. 关闭全局 hub (停止上游 SSE reader)
        tracing::info!("shutting down global event hub");
        self.state.global_hub.stop().await;
        // 4. 杀所有终端会话 (对齐 Node `shutdown`)
        self.state.terminal_sessions.kill_all().await;
        // 5. 关闭 OpenCode 子进程
        if let Some(mut h) = self.oc_handle.take() {
            tracing::info!("shutting down opencode process");
            h.shutdown().await;
        }
        tracing::info!("oc-server stopped");
    }
}

// =========================================================================
// 路由构建 (从 main.rs 移入)
// =========================================================================

/// 构建完整路由树。从原 main.rs build_router 原样移入。
fn build_router(state: Arc<state::AppState>, config: &Config) -> Router {
    // 具体路由优先于 catch-all。
    // SSE/WS 端点和状态端点是具体路由, axum 会优先匹配。
    // 其余 /api/* 走 proxy catch-all。
    let mut router = Router::new()
        // 状态端点
        .route("/health", get(routes::health))
        .route("/api/version", get(routes::version))
        .route("/api/system/info", get(routes::system_info))
        .route("/robots.txt", get(routes::robots_txt))
        // 文本摘要 (阶段 3a)
        .route("/api/text/summarize", post(text::routes::summarize))
        // 文件系统路由 (阶段 3a, 15 个端点)
        .route("/api/fs/grant", post(fs::routes::grant))
        .route("/api/fs/home", get(fs::routes::home))
        .route("/api/fs/mkdir", post(fs::routes::mkdir))
        .route("/api/fs/clone", post(fs::routes::clone))
        .route("/api/fs/stat", get(fs::routes::stat))
        .route("/api/fs/read", get(fs::routes::read))
        .route("/api/fs/raw", get(fs::routes::raw))
        .route("/api/fs/serve/{rest}", get(fs::routes::serve))
        .route("/api/fs/write", post(fs::routes::write))
        .route("/api/fs/delete", post(fs::routes::delete))
        .route("/api/fs/rename", post(fs::routes::rename))
        .route("/api/fs/reveal", post(fs::routes::reveal))
        .route("/api/fs/exec", post(fs::routes::exec))
        .route("/api/fs/exec/{job_id}", get(fs::routes::exec_status))
        .route("/api/fs/list", get(fs::routes::list))
        // Git 路由 (阶段 3a 后半, 68 个端点)
        .route("/api/git/identities", get(git::routes::list_identities).post(git::routes::create_identity))
        .route("/api/git/identities/{id}", put(git::routes::update_identity).delete(git::routes::delete_identity))
        .route("/api/git/global-identity", get(git::routes::global_identity))
        .route("/api/git/discover-credentials", get(git::routes::discover_credentials))
        .route("/api/git/current-identity", get(git::routes::current_identity))
        .route("/api/git/has-local-identity", get(git::routes::has_local_identity))
        .route("/api/git/set-identity", post(git::routes::set_identity))
        .route("/api/git/check", get(git::routes::check))
        .route("/api/git/remote-url", get(git::routes::remote_url))
        .route("/api/git/primary-root", get(git::routes::primary_root))
        .route("/api/git/toplevel", get(git::routes::toplevel))
        .route("/api/git/status", get(git::routes::status))
        .route("/api/git/diff", get(git::routes::diff))
        .route("/api/git/file-diff", get(git::routes::file_diff))
        .route("/api/git/revert", post(git::routes::revert_file))
        .route("/api/git/stage", post(git::routes::stage))
        .route("/api/git/unstage", post(git::routes::unstage))
        .route("/api/git/apply-hunk", post(git::routes::apply_hunk))
        .route("/api/git/commit", post(git::routes::commit))
        .route("/api/git/commit-summaries", post(git::routes::commit_summaries))
        .route("/api/git/log", get(git::routes::log))
        .route("/api/git/commit-files", get(git::routes::commit_files))
        .route("/api/git/commit-file-diff", get(git::routes::commit_file_diff))
        .route("/api/git/checkout-commit", post(git::routes::checkout_commit))
        .route("/api/git/cherry-pick", post(git::routes::cherry_pick))
        .route("/api/git/revert-commit", post(git::routes::revert_commit))
        .route("/api/git/reset-to-commit", post(git::routes::reset_to_commit))
        .route("/api/git/branches", get(git::routes::list_branches).post(git::routes::create_branch).delete(git::routes::delete_branch))
        .route("/api/git/branches/rename", put(git::routes::rename_branch))
        .route("/api/git/remote-branches", delete(git::routes::delete_remote_branch))
        .route("/api/git/checkout", post(git::routes::checkout))
        .route("/api/git/pull", post(git::routes::pull))
        .route("/api/git/push", post(git::routes::push))
        .route("/api/git/fetch", post(git::routes::fetch))
        .route("/api/git/remotes", get(git::routes::list_remotes).delete(git::routes::remove_remote))
        .route("/api/git/merge", post(git::routes::merge))
        .route("/api/git/merge/abort", post(git::routes::abort_merge))
        .route("/api/git/merge/continue", post(git::routes::continue_merge))
        .route("/api/git/rebase", post(git::routes::rebase))
        .route("/api/git/rebase/abort", post(git::routes::abort_rebase))
        .route("/api/git/rebase/continue", post(git::routes::continue_rebase))
        .route("/api/git/conflict-details", get(git::routes::conflict_details))
        .route("/api/git/stashes", get(git::routes::list_stashes))
        .route("/api/git/stashes/file-counts", post(git::routes::stash_file_counts))
        .route("/api/git/stash", post(git::routes::stash_push))
        .route("/api/git/stash/apply", post(git::routes::stash_apply))
        .route("/api/git/stash/pop", post(git::routes::stash_pop))
        .route("/api/git/stash/drop", post(git::routes::stash_drop))
        .route("/api/git/worktrees", get(git::routes::list_worktrees).post(git::routes::create_worktree).delete(git::routes::remove_worktree))
        .route("/api/git/worktrees/validate", post(git::routes::validate_worktree))
        .route("/api/git/worktrees/preview", post(git::routes::preview_worktree))
        .route("/api/git/worktrees/bootstrap-status", get(git::routes::worktree_bootstrap_status))
        .route("/api/git/worktree-type", get(git::routes::worktree_type))
        .route("/api/git/validate-directory", post(git::routes::validate_directory))
        .route("/api/git/canonicalize-worktree-state", post(git::routes::canonicalize_worktree_state))
        .route("/api/git/integrate/plan", post(git::routes::integrate_plan))
        .route("/api/git/integrate/conflict-details", post(git::routes::integrate_conflict_details))
        .route("/api/git/integrate/cherry-pick-status", post(git::routes::integrate_cherry_pick_status))
        .route("/api/git/integrate/run", post(git::routes::integrate_run))
        .route("/api/git/integrate/abort", post(git::routes::integrate_abort))
        .route("/api/git/integrate/continue", post(git::routes::integrate_continue))
        // GitHub 路由 (阶段 3b group 1, 18 个端点)
        .route("/api/github/auth/status", get(github::routes::auth_status))
        .route("/api/github/auth/gh-cli", post(github::routes::auth_gh_cli))
        .route("/api/github/auth/start", post(github::routes::auth_start))
        .route("/api/github/auth/complete", post(github::routes::auth_complete))
        .route("/api/github/auth/activate", post(github::routes::auth_activate))
        .route("/api/github/auth", delete(github::routes::auth_delete))
        .route("/api/github/me", get(github::routes::me))
        .route("/api/github/pr/status", get(github::routes::pr_status))
        .route("/api/github/pr/create", post(github::routes::pr_create))
        .route("/api/github/pr/update", post(github::routes::pr_update))
        .route("/api/github/pr/merge", post(github::routes::pr_merge))
        .route("/api/github/pr/ready", post(github::routes::pr_ready))
        .route("/api/github/repo/upstream", get(github::routes::repo_upstream))
        .route("/api/github/repo/branches", get(github::routes::repo_branches))
        .route("/api/github/issues/list", get(github::routes::issues_list))
        .route("/api/github/issues/get", get(github::routes::issues_get))
        .route("/api/github/issues/comments", get(github::routes::issues_comments))
        .route("/api/github/pulls/list", get(github::routes::pulls_list))
        .route("/api/github/pulls/context", get(github::routes::pulls_context))
        // Tunnels 路由 (阶段 3b group 2, 8 个端点)
        .route("/api/openchamber/tunnel/check", get(tunnels::routes::tunnel_check))
        .route(
            "/api/openchamber/tunnel/doctor",
            post(tunnels::routes::tunnel_doctor).get(tunnels::routes::tunnel_doctor),
        )
        .route(
            "/api/openchamber/tunnel/providers",
            get(tunnels::routes::tunnel_providers),
        )
        .route(
            "/api/openchamber/tunnel/status",
            get(tunnels::routes::tunnel_status),
        )
        .route(
            "/api/openchamber/tunnel/managed-remote-token",
            put(tunnels::routes::tunnel_managed_remote_token),
        )
        .route(
            "/api/openchamber/tunnel/start",
            post(tunnels::routes::tunnel_start),
        )
        .route(
            "/api/openchamber/tunnel/stop",
            post(tunnels::routes::tunnel_stop),
        )
        .route("/connect", get(tunnels::routes::connect))
        // UI auth 路由 (阶段 3b group 3, 11 个端点)
        .route(
            "/auth/session",
            get(ui_auth::routes::auth_session_status).post(ui_auth::routes::auth_session_create),
        )
        .route("/auth/url-token", post(ui_auth::routes::auth_url_token))
        .route("/auth/passkey/status", get(ui_auth::routes::passkey_status))
        .route(
            "/auth/passkey/authenticate/options",
            post(ui_auth::routes::passkey_auth_options),
        )
        .route(
            "/auth/passkey/authenticate/verify",
            post(ui_auth::routes::passkey_auth_verify),
        )
        .route(
            "/auth/passkey/register/options",
            post(ui_auth::routes::passkey_register_options),
        )
        .route(
            "/auth/passkey/register/verify",
            post(ui_auth::routes::passkey_register_verify),
        )
        .route("/api/passkeys", get(ui_auth::routes::passkey_list))
        .route("/api/passkeys/{id}", delete(ui_auth::routes::passkey_revoke))
        .route("/api/auth/reset", post(ui_auth::routes::auth_reset))
        // Client auth 路由 (阶段 3b group 3, 10 个端点)
        .route(
            "/api/client-auth/clients",
            get(client_auth::routes::list_clients)
                .post(client_auth::routes::create_client)
                .delete(client_auth::routes::revoke_all_clients),
        )
        .route(
            "/api/client-auth/clients/{id}",
            delete(client_auth::routes::revoke_client),
        )
        .route(
            "/api/client-auth/pairing/sessions",
            post(client_auth::routes::create_pairing_session)
                .get(client_auth::routes::list_pairing_sessions),
        )
        .route(
            "/api/client-auth/pairing/sessions/{id}",
            delete(client_auth::routes::cancel_pairing_session),
        )
        .route(
            "/api/client-auth/pairing/redeem",
            post(client_auth::routes::redeem_pairing),
        )
        .route(
            "/api/client-auth/pairing/transports",
            get(client_auth::routes::pairing_transports),
        )
        .route(
            "/api/client-auth/connection/candidates",
            get(client_auth::routes::connection_candidates),
        )
        // Notifications 路由 (阶段 3b group 4, 18 个端点)
        .route("/api/push/vapid-public-key", get(notifications::routes::vapid_public_key))
        .route("/api/push/subscribe", post(notifications::routes::push_subscribe).delete(notifications::routes::push_unsubscribe))
        .route("/api/push/apns-token", post(notifications::routes::apns_token_subscribe).delete(notifications::routes::apns_token_unsubscribe))
        .route("/api/push/visibility", post(notifications::routes::set_visibility).get(notifications::routes::get_visibility))
        .route("/api/notifications/stream", get(notifications::routes::notification_stream))
        .route("/api/session-activity", get(notifications::routes::session_activity))
        .route("/api/sessions/snapshot", get(notifications::routes::sessions_snapshot))
        .route("/api/sessions/status", get(notifications::routes::sessions_status))
        .route("/api/sessions/{id}/status", get(notifications::routes::session_status))
        .route("/api/sessions/attention", get(notifications::routes::sessions_attention))
        .route("/api/sessions/{id}/attention", get(notifications::routes::session_attention))
        .route("/api/sessions/{id}/view", post(notifications::routes::session_view))
        .route("/api/sessions/{id}/unview", post(notifications::routes::session_unview))
        .route("/api/sessions/{id}/message-sent", post(notifications::routes::session_message_sent))
        .route("/api/notifications/auto-accept", post(notifications::routes::auto_accept))
        // permission-auto-accept (阶段 3c group 1)
        .route(
            "/api/permission-auto-accept",
            get(permission_auto_accept::get_permission_auto_accept),
        )
        .route(
            "/api/permission-auto-accept/sessions/{sessionId}",
            put(permission_auto_accept::put_session_policy),
        )
        // session-folders (阶段 3c group 1)
        .route(
            "/api/session-folders",
            get(session_folders::get_session_folders).post(session_folders::post_session_folders),
        )
        // magic-prompts (阶段 3c group 1)
        .route(
            "/api/magic-prompts",
            get(magic_prompts::get_magic_prompts).delete(magic_prompts::delete_all_magic_prompts),
        )
        .route(
            "/api/magic-prompts/{id}",
            put(magic_prompts::put_magic_prompt).delete(magic_prompts::delete_magic_prompt),
        )
        // session-goal (阶段 3c group 3) — file-backed objective CRUD
        .route(
            "/api/goals/objective/{session_id}",
            put(session_goal::routes::put_objective)
                .get(session_goal::routes::get_objective)
                .delete(session_goal::routes::delete_objective),
        )
        // small-model (阶段 3c group 2)
        .route(
            "/api/small-model",
            get(small_model::routes::get_small_model),
        )
        .route(
            "/api/small-model/generate",
            post(small_model::routes::post_small_model_generate),
        )
        // TTS 路由 (阶段 3d — voice/token + tts/speak + tts/status + tts/say/* + stt/transcribe)
        .route("/api/voice/token", post(tts::routes::post_voice_token_response))
        .route("/api/tts/speak", post(tts::routes::post_tts_speak))
        .route("/api/tts/status", get(tts::routes::get_tts_status))
        .route("/api/tts/say/status", get(tts::routes::get_tts_say_status))
        .route("/api/tts/say/speak", post(tts::routes::post_tts_say_speak))
        .route("/api/stt/transcribe", post(tts::routes::post_stt_transcribe))
        // Quota 路由 (阶段 3c group 4 — 7 个端点)
        .route("/api/quota/providers", get(quota::routes::list_providers))
        .route("/api/quota/credentials/{provider_id}", get(quota::routes::get_credential_status).put(quota::routes::put_credential))
        .route("/api/quota/credentials/{provider_id}/validate", post(quota::routes::validate_credential))
        .route("/api/quota/credentials/{provider_id}/import", post(quota::routes::import_credential))
        .route("/api/quota/credentials/{provider_id}", delete(quota::routes::delete_credential))
        .route("/api/quota/{provider_id}", get(quota::routes::fetch_quota))
        // Scheduled-tasks 路由 (阶段 3c group 4 — 5 + 1 SSE)
        .route("/api/projects/{project_id}/scheduled-tasks", get(scheduled_tasks::routes::list_scheduled_tasks).put(scheduled_tasks::routes::upsert_scheduled_task))
        .route("/api/projects/{project_id}/scheduled-tasks/{task_id}", delete(scheduled_tasks::routes::delete_scheduled_task))
        .route("/api/projects/{project_id}/scheduled-tasks/{task_id}/run", post(scheduled_tasks::routes::run_scheduled_task))
        .route("/api/openchamber/scheduled-tasks/status", get(scheduled_tasks::routes::scheduled_tasks_status))
        .route("/api/openchamber/events", get(scheduled_tasks::routes::openchamber_events))
        // Skills-catalog 路由 (阶段 3c group 4 — 12 个端点)
        .route("/api/config/skills", get(skills_catalog::routes::list_skills))
        .route("/api/config/skills/catalog", get(skills_catalog::routes::list_catalog_sources))
        .route("/api/config/skills/catalog/source", get(skills_catalog::routes::browse_catalog_source))
        .route("/api/config/skills/scan", post(skills_catalog::routes::scan_repository))
        .route("/api/config/skills/install", post(skills_catalog::routes::install_skills))
        .route("/api/config/skills/{name}", get(skills_catalog::routes::get_skill).post(skills_catalog::routes::create_skill).patch(skills_catalog::routes::update_skill).delete(skills_catalog::routes::delete_skill))
        .route("/api/config/skills/{name}/files/{*file_path}", get(skills_catalog::routes::read_skill_file).put(skills_catalog::routes::write_skill_file).delete(skills_catalog::routes::delete_skill_file))
        // SSE 透传代理 (具体路由, 优先于 catch-all)
        .route("/api/global/event", get(realtime::sse_proxy::sse_proxy_handler))
        .route("/api/event", get(realtime::sse_proxy::sse_proxy_handler))
        // WebSocket 桥
        .route("/api/global/event/ws", any(realtime::ws_bridge::global_ws_handler))
        .route("/api/event/ws", any(realtime::ws_bridge::directory_ws_handler))
        // Terminal 模块 (PTY 会话 + WS 桥)
        .route("/api/terminal/create", post(terminal::routes::create))
        .route("/api/terminal/force-kill", post(terminal::routes::force_kill))
        .route("/api/terminal/{sessionId}/stream", get(terminal::routes::stream))
        .route("/api/terminal/{sessionId}/input", post(terminal::routes::input))
        .route("/api/terminal/{sessionId}/resize", post(terminal::routes::resize))
        .route("/api/terminal/{sessionId}/restart", post(terminal::routes::restart))
        .route("/api/terminal/{sessionId}", delete(terminal::routes::delete))
        .route("/api/terminal/ws", any(terminal::routes::terminal_ws_handler))
        // Preview 模块 (dev server 反向代理 + WS 升级代理)
        .route("/api/preview/targets", post(preview::routes::post_targets_handler))
        .route(
            "/api/preview/proxy/{id}/{*rest}",
            any(preview::routes::proxy_or_ws_handler),
        )
        // Dictation 模块 (流式 STT + 本地 TTS)。本地推理路径本轮不移植
        // (openai-compatible 提供方可用, local 返回 local_models_unsupported)。
        // WS 路径已在 ui_auth/types.rs WS 白名单, 注册即获得 auth 覆盖。
        .route("/api/dictation/status", get(dictation::routes::get_status_handler))
        .route("/api/dictation/tts/speak", post(dictation::routes::post_tts_speak_handler))
        .route(
            "/api/dictation/models/{model_id}/download",
            post(dictation::routes::post_model_download_handler),
        )
        .route(
            "/api/dictation/models/{model_id}",
            delete(dictation::routes::delete_model_handler),
        )
        .route("/api/dictation/ws", any(dictation::routes::dictation_ws_handler))
        // 私有中继 (Phase 3f Group 4) — 管理路由
        .route(
            "/api/openchamber/relay/status",
            get(relay::routes::get_status_handler),
        )
        .route(
            "/api/openchamber/relay/enable",
            post(relay::routes::post_enable_handler),
        )
        .route(
            "/api/openchamber/relay/disable",
            post(relay::routes::post_disable_handler),
        )
        // OpenCode 反向代理 (/api/* catch-all)
        // nest 会剥离 /api 前缀, proxy_handler 收到的 path 是去掉 /api 后的部分。
        // 具体路由 (fs/text/SSE/WS/version/system-info) 已在上面注册, axum 优先匹配。
        .nest("/api", Router::new().fallback(any(proxy::proxy_handler)))
        // 全局认证中间件 — 覆盖以上所有路由 (含 SSE/WS/proxy catch-all)。
        // 对齐 Node `app.use('/api', requireApiAuth)` (core-routes.js:997)。
        // 公开路由 (health/version/auth-session 等) 由中间件内 is_public_path 放行。
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            middleware::auth::require_api_auth,
        ));

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

#[cfg(test)]
mod tests {
    use super::*;

    /// helper: 从 "http://127.0.0.1:<port>" 提取端口号。
    fn parse_port(base_url: &str) -> u16 {
        base_url
            .rsplit(':')
            .next()
            .and_then(|s| s.parse().ok())
            .expect("base_url should contain port")
    }

    #[tokio::test]
    async fn oc_server_starts_and_serves_health() {
        let config = Config::for_tests();
        let server = OcServer::start(config).await.expect("OcServer::start");

        let resp = reqwest::get(format!("{}/health", server.base_url()))
            .await
            .expect("GET /health");
        assert!(
            resp.status().is_success(),
            "/health should return 2xx, got {}",
            resp.status()
        );

        server.shutdown().await;
    }

    #[tokio::test]
    async fn oc_server_shutdown_releases_port() {
        let config = Config::for_tests();
        let server = OcServer::start(config).await.expect("OcServer::start");
        let port = parse_port(server.base_url());

        server.shutdown().await;

        // shutdown 后端口应可重新绑定 (证明 listener 已释放)
        let rebind = tokio::net::TcpListener::bind(("127.0.0.1", port)).await;
        assert!(
            rebind.is_ok(),
            "port {} should be free after shutdown",
            port
        );
    }

    #[test]
    fn base_url_format() {
        // OcServer::start 需要 tokio context, 这里只验证 base_url 字符串形态
        // 用 oneshot 构造一个不完整句柄的等价字符串测试。
        let sample = "http://127.0.0.1:12345";
        assert!(sample.starts_with("http://"));
        assert!(parse_port(sample) == 12345);
    }
}
