//! `oc-server` — OpenChamber Rust 后端入口。
//!
//! 替换目标: `packages/web/server/index.js` (Express, ~1691 行)。
//!
//! 阶段 1-3a 实现:
//!   - CLI/env 解析 (clap, 对应 cli-options.js)
//!   - 绑定地址安全检查 (bind-host.js 等价)
//!   - OpenCode 进程生命周期 (spawn + 就绪门 + 优雅关闭)
//!   - OpenCode HTTP 代理 (/api/* catch-all, 流式转发)
//!   - 静态 dist 托管 + SPA fallback
//!   - /health, /api/version, /api/system/info, /robots.txt
//!   - SSE 透传代理 (/api/event, /api/global/event) — 边界感知心跳
//!   - WS 全局事件桥 (/api/global/event/ws) — 共享 reader + replay
//!   - WS 目录事件桥 (/api/event/ws) — 每连接独享 reader
//!   - 文本摘要 (/api/text/summarize)
//!   - 文件系统路由 (/api/fs/*) — 15 个端点
//!   - Git 路由 (/api/git/*) — 68 个端点 (spawn `git` CLI)
//!
//! 后续阶段:
//!   - 阶段 3b: github / tunnels / ui-auth / terminal / ...

mod bind_host;
mod client_auth;
mod config;
mod error;
mod fs;
mod git;
mod github;
mod opencode;
mod project_dir;
mod proxy;
mod realtime;
mod routes;
mod state;
mod static_files;
mod text;
mod tunnels;
mod ui_auth;

use std::sync::Arc;

use anyhow::Context;
use axum::routing::{any, delete, get, post, put};
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

    // 6. 关闭全局 hub (停止上游 SSE reader)
    tracing::info!("shutting down global event hub");
    state.global_hub.stop().await;

    // 7. 关闭 OpenCode 子进程
    tracing::info!("shutting down opencode process");
    oc_handle.shutdown().await;
    tracing::info!("oc-server stopped");

    Ok(())
}

/// 构建完整路由树。
fn build_router(state: Arc<AppState>, config: &Config) -> Router {
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
        // SSE 透传代理 (具体路由, 优先于 catch-all)
        .route("/api/global/event", get(realtime::sse_proxy::sse_proxy_handler))
        .route("/api/event", get(realtime::sse_proxy::sse_proxy_handler))
        // WebSocket 桥
        .route("/api/global/event/ws", any(realtime::ws_bridge::global_ws_handler))
        .route("/api/event/ws", any(realtime::ws_bridge::directory_ws_handler))
        // OpenCode 反向代理 (/api/* catch-all)
        // nest 会剥离 /api 前缀, proxy_handler 收到的 path 是去掉 /api 后的部分。
        // 具体路由 (fs/text/SSE/WS/version/system-info) 已在上面注册, axum 优先匹配。
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
