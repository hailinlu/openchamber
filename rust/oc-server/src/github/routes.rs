//! GitHub 路由 — 18 个 axum handler。
//!
//! 移植自 `packages/web/server/lib/github/routes.js` (1797 行)。
//!
//! 所有 handler 返回 `ApiResult<Json<Value>>`。
//! JSON 响应字段与 Node 逐字段对齐 (camelCase)。

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Query, State};
use axum::Json;
use serde_json::{json, Value};
use tokio::time::timeout;

use crate::error::{ApiError, ApiResult};
use crate::github::client::{GitHubApiError, GitHubClient};
use crate::github::pr_status::{resolve_github_pr_status, PrStatusResult};
use crate::github::{auth, device_flow, fork_detection, settings};
use crate::state::AppState;

/// PR status 缓存 TTL。
const PR_STATUS_CACHE_TTL: Duration = Duration::from_secs(90);
/// PR status resolve 超时。
const PR_STATUS_RESOLVE_TIMEOUT: Duration = Duration::from_secs(12);

// ============================================================
// Helper functions
// ============================================================

/// 从 query map 中提取并 trim 字符串。
fn opt_query(params: &std::collections::HashMap<String, String>, key: &str) -> String {
    params.get(key).map(|s| s.trim().to_string()).unwrap_or_default()
}

/// 从 query map 中提取 boolean (接受 "true"/"1")。
fn opt_query_bool(params: &std::collections::HashMap<String, String>, key: &str) -> bool {
    matches!(
        params.get(key).map(|s| s.as_str()),
        Some("true") | Some("1")
    )
}

/// 构建 PR summary JSON (对应 Node routes.js 的 PR 响应构建器)。
fn build_pr_summary(pr: &Value, check_merged: bool) -> Value {
    let state = if check_merged {
        let is_merged = pr.get("merged").and_then(|v| v.as_bool()).unwrap_or(false)
            || pr.get("merged_at").as_ref().filter(|v| !v.is_null()).is_some();
        if is_merged {
            "merged".to_string()
        } else {
            pr.get("state").and_then(|v| v.as_str()).unwrap_or("open").to_string()
        }
    } else {
        // pr/create: 不检查 merged
        match pr.get("state").and_then(|v| v.as_str()) {
            Some("closed") => "closed".to_string(),
            _ => "open".to_string(),
        }
    };

    json!({
        "number": pr.get("number").cloned().unwrap_or(Value::Null),
        "title": pr.get("title").cloned().unwrap_or(Value::Null),
        "body": pr.get("body").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        "url": pr.get("html_url").cloned().unwrap_or(Value::Null),
        "state": state,
        "draft": pr.get("draft").and_then(|v| v.as_bool()).unwrap_or(false),
        "base": pr.get("base").and_then(|b| b.get("ref")).cloned().unwrap_or(Value::Null),
        "head": pr.get("head").and_then(|h| h.get("ref")).cloned().unwrap_or(Value::Null),
        "headSha": pr.get("head").and_then(|h| h.get("sha")).cloned().unwrap_or(Value::Null),
        "mergeable": pr.get("mergeable").cloned().unwrap_or(Value::Null),
        "mergeableState": pr.get("mergeable_state").cloned().unwrap_or(Value::Null),
    })
}

/// 获取 user summary (login + emails fallback)。
async fn get_user_summary(client: &GitHubClient) -> Result<Value, GitHubApiError> {
    let me = client.users_get_authenticated().await?;
    let login = me.get("login").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let id = me.get("id").and_then(|v| v.as_i64());
    let avatar_url = me.get("avatar_url").and_then(|v| v.as_str()).map(String::from);
    let name = me.get("name").and_then(|v| v.as_str()).map(String::from);

    let mut email = me.get("email").and_then(|v| v.as_str()).map(String::from);
    if email.as_deref().map(|s| s.is_empty()).unwrap_or(true) {
        if let Ok(emails) = client.users_list_emails().await {
            let primary_verified = emails.iter().find(|e| {
                e.get("primary").and_then(|v| v.as_bool()).unwrap_or(false)
                    && e.get("verified").and_then(|v| v.as_bool()).unwrap_or(false)
                    && e.get("email").and_then(|v| v.as_str()).is_some()
            });
            let email_val = if let Some(e) = primary_verified {
                e.get("email").and_then(|v| v.as_str()).map(String::from)
            } else {
                emails.iter().find(|e| {
                    e.get("verified").and_then(|v| v.as_bool()).unwrap_or(false)
                        && e.get("email").and_then(|v| v.as_str()).is_some()
                }).and_then(|e| e.get("email").and_then(|v| v.as_str()).map(String::from))
            };
            email = email_val;
        }
    }

    Ok(json!({
        "login": login,
        "id": id,
        "avatarUrl": avatar_url,
        "name": name,
        "email": email,
    }))
}

/// 构建 gh-CLI 状态 JSON。
fn build_gh_cli_status() -> Value {
    let disabled = settings::is_gh_cli_disabled();
    let active = settings::is_gh_cli_active();
    let available = !disabled && auth::get_gh_cli_token().is_some();

    let mut gh_cli = json!({
        "available": available,
        "disabled": disabled,
        "active": active,
    });

    // 如果 gh-CLI active 且有 token, 尝试获取 user 信息
    if active && available {
        // 注意: 这里不调用 API 获取 user (避免同步阻塞), 留给 auth_status handler
        gh_cli["source"] = json!("gh-cli");
    }

    gh_cli
}

// ============================================================
// Auth routes
// ============================================================

/// `GET /api/github/auth/status` — 连接状态 + accounts + gh-CLI。
pub async fn auth_status(
    State(state): State<Arc<AppState>>,
) -> ApiResult<Json<Value>> {
    let rate_limit = &state.github_rate_limit;

    // 获取当前 auth
    let current_auth = auth::get_github_auth();
    let accounts = auth::get_github_auth_accounts();

    // 尝试创建 client
    let client = GitHubClient::from_current_auth();

    let gh_cli = build_gh_cli_status();

    if let Some(client) = &client {
        // 尝试获取 user summary
        match get_user_summary(client).await {
            Ok(user) => {
                let scope = current_auth
                    .as_ref()
                    .map(|e| e.scope.clone())
                    .unwrap_or_default();
                return Ok(Json(json!({
                    "connected": true,
                    "user": user,
                    "scope": scope,
                    "accounts": accounts,
                    "ghCli": gh_cli,
                })));
            }
            Err(error) => {
                rate_limit.note_if_rate_limit(&error);
                if error.status == 401 || error.status == 403 {
                    // auth 无效, 清除
                    auth::clear_github_auth();
                }
            }
        }
    }

    Ok(Json(json!({
        "connected": false,
        "accounts": accounts,
        "ghCli": gh_cli,
    })))
}

/// `POST /api/github/auth/gh-cli` — 启用/禁用 gh-CLI。
pub async fn auth_gh_cli(
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let disabled = body
        .get("disabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    settings::set_gh_cli_disabled(disabled).map_err(ApiError::from)?;
    auth::clear_gh_cli_token_cache();

    Ok(Json(json!({ "disabled": disabled })))
}

/// `POST /api/github/auth/start` — 启动 device flow。
pub async fn auth_start() -> ApiResult<Json<Value>> {
    let client_id = settings::get_github_client_id();
    let scope = settings::get_github_scopes();

    if client_id.is_empty() {
        return Err(ApiError(oc_core::Error::BadRequest(
            "GitHub client ID is required".to_string(),
        )));
    }

    let payload = device_flow::start_device_flow(&client_id, &scope)
        .await
        .map_err(|e| ApiError(oc_core::Error::Internal(e.0)))?;

    Ok(Json(payload))
}

/// `POST /api/github/auth/complete` — 交换 device code。
pub async fn auth_complete(
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let device_code = body
        .get("deviceCode")
        .or_else(|| body.get("device_code"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    if device_code.is_empty() {
        return Err(ApiError(oc_core::Error::BadRequest(
            "deviceCode is required".to_string(),
        )));
    }

    let client_id = settings::get_github_client_id();
    let payload = device_flow::exchange_device_code(&client_id, &device_code)
        .await
        .map_err(|e| ApiError(oc_core::Error::Internal(e.0)))?;

    // 检查是否有 error (authorization_pending 等)
    if let Some(error) = payload.get("error").and_then(|v| v.as_str()) {
        let error_description = payload
            .get("error_description")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        return Ok(Json(json!({
            "connected": false,
            "status": error,
            "error": error_description,
        })));
    }

    // 成功 → 获取 access_token, 持久化, 获取 user
    let access_token = payload
        .get("access_token")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let scope = payload
        .get("scope")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if access_token.is_empty() {
        return Ok(Json(json!({
            "connected": false,
            "status": "no_access_token",
            "error": "No access token received",
        })));
    }

    // 持久化 auth (先存储, 再获取 user)
    auth::set_github_auth(access_token, scope, Some("bearer"), None, None);

    // 获取 user summary
    let client = GitHubClient::new(access_token.to_string());
    match get_user_summary(&client).await {
        Ok(user) => {
            let login = user.get("login").and_then(|v| v.as_str()).unwrap_or("");
            let id = user.get("id").and_then(|v| v.as_i64());
            let avatar_url = user.get("avatarUrl").and_then(|v| v.as_str()).map(String::from);
            let name = user.get("name").and_then(|v| v.as_str()).map(String::from);
            let email = user.get("email").and_then(|v| v.as_str()).map(String::from);

            let user_struct = auth::GitHubUser {
                login: login.to_string(),
                avatar_url,
                id,
                name,
                email,
            };

            auth::set_github_auth(
                access_token,
                scope,
                Some("bearer"),
                Some(user_struct.clone()),
                None,
            );

            let accounts = auth::get_github_auth_accounts();

            Ok(Json(json!({
                "connected": true,
                "user": user,
                "scope": scope,
                "accounts": accounts,
            })))
        }
        Err(_) => {
            // 获取 user 失败, 但 auth 已存储
            let accounts = auth::get_github_auth_accounts();
            Ok(Json(json!({
                "connected": true,
                "user": null,
                "scope": scope,
                "accounts": accounts,
            })))
        }
    }
}

/// `POST /api/github/auth/activate` — 激活指定 account。
pub async fn auth_activate(
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let account_id = body
        .get("accountId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    if account_id.is_empty() {
        return Err(ApiError(oc_core::Error::BadRequest(
            "accountId is required".to_string(),
        )));
    }

    // gh-CLI 特殊处理
    if account_id == crate::github::GH_CLI_ACCOUNT_ID {
        settings::set_gh_cli_active(true).map_err(ApiError::from)?;
        let gh_cli = build_gh_cli_status();
        let client = GitHubClient::from_current_auth();
        if let Some(client) = &client {
            if let Ok(user) = get_user_summary(client).await {
                return Ok(Json(json!({
                    "connected": true,
                    "user": user,
                    "ghCli": gh_cli,
                })));
            }
        }
        return Ok(Json(json!({
            "connected": true,
            "user": null,
            "ghCli": gh_cli,
        })));
    }

    let success = auth::activate_github_auth(&account_id);
    if !success {
        return Err(ApiError(oc_core::Error::NotFound(format!(
            "Account '{}' not found",
            account_id
        ))));
    }

    let client = GitHubClient::from_current_auth();
    let accounts = auth::get_github_auth_accounts();
    let gh_cli = build_gh_cli_status();

    if let Some(client) = &client {
        if let Ok(user) = get_user_summary(client).await {
            let scope = auth::get_github_auth().map(|e| e.scope).unwrap_or_default();
            return Ok(Json(json!({
                "connected": true,
                "user": user,
                "scope": scope,
                "accounts": accounts,
                "ghCli": gh_cli,
            })));
        }
    }

    Ok(Json(json!({
        "connected": true,
        "accounts": accounts,
        "ghCli": gh_cli,
    })))
}

/// `DELETE /api/github/auth` — 清除当前 auth。
pub async fn auth_delete() -> ApiResult<Json<Value>> {
    let removed = auth::clear_github_auth();
    Ok(Json(json!({ "success": true, "removed": removed })))
}

/// `GET /api/github/me` — 当前用户信息。
pub async fn me() -> ApiResult<Json<Value>> {
    let Some(client) = GitHubClient::from_current_auth() else {
        return Err(ApiError(oc_core::Error::Unauthorized(
            "GitHub not connected".to_string(),
        )));
    };

    match get_user_summary(&client).await {
        Ok(user) => Ok(Json(user)),
        Err(error) => {
            if error.status == 401 {
                auth::clear_github_auth();
                return Err(ApiError(oc_core::Error::Unauthorized(
                    "GitHub authentication expired".to_string(),
                )));
            }
            Err(ApiError(oc_core::Error::Internal(error.message)))
        }
    }
}

// ============================================================
// PR routes
// ============================================================

/// `GET /api/github/pr/status` — PR 状态 (带缓存, 12s 超时)。
pub async fn pr_status(
    State(state): State<Arc<AppState>>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    let directory = opt_query(&params, "directory");
    let branch = opt_query(&params, "branch");
    let remote = {
        let r = opt_query(&params, "remote");
        if r.is_empty() {
            "origin".to_string()
        } else {
            r
        }
    };
    let force = opt_query_bool(&params, "force");

    if directory.is_empty() || branch.is_empty() {
        return Err(ApiError(oc_core::Error::BadRequest(
            "directory and branch are required".to_string(),
        )));
    }

    let cache_key = format!("{}::{}::{}", directory, branch, remote);
    let rate_limit = &state.github_rate_limit;

    // 检查缓存
    if !force {
        if let Some(cached) = state.github_pr_status_cache.get(&cache_key) {
            if Instant::now().duration_since(cached.fetched_at) < PR_STATUS_CACHE_TTL {
                return Ok(Json(cached.data.clone()));
            }
        }
    }

    // rate-limit 检查
    if rate_limit.is_rate_limited() {
        if let Some(cached) = state.github_pr_status_cache.get(&cache_key) {
            return Ok(Json(cached.data.clone()));
        }
        return Err(ApiError(oc_core::Error::ServiceUnavailable(
            "GitHub rate limited".to_string(),
        )));
    }

    // 获取 client
    let Some(client) = GitHubClient::from_current_auth() else {
        return Ok(Json(json!({ "connected": false })));
    };

    // 12s 超时 resolve
    let resolve_result = timeout(
        PR_STATUS_RESOLVE_TIMEOUT,
        resolve_github_pr_status(&client, rate_limit, &directory, &branch, &remote),
    )
    .await;

    let resolved: PrStatusResult = match resolve_result {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => {
            rate_limit.note_if_rate_limit(&error);
            // transient failure → serve cache or 503
            if let Some(cached) = state.github_pr_status_cache.get(&cache_key) {
                return Ok(Json(cached.data.clone()));
            }
            return Err(ApiError(oc_core::Error::ServiceUnavailable(
                error.message,
            )));
        }
        Err(_) => {
            // timeout
            if let Some(cached) = state.github_pr_status_cache.get(&cache_key) {
                return Ok(Json(cached.data.clone()));
            }
            return Err(ApiError(oc_core::Error::ServiceUnavailable(
                "PR status resolve timed out".to_string(),
            )));
        }
    };

    let Some(search_repo) = &resolved.repo else {
        return Ok(Json(json!({
            "connected": true,
            "repo": null,
            "branch": branch,
            "pr": null,
            "checks": null,
            "canMerge": false,
            "defaultBranch": null,
            "resolvedRemoteName": null,
        })));
    };

    let Some(first_pr) = &resolved.pr else {
        let response = json!({
            "connected": true,
            "repo": fork_detection::repo_to_json(search_repo),
            "branch": branch,
            "pr": null,
            "checks": null,
            "canMerge": false,
            "defaultBranch": resolved.default_branch,
            "resolvedRemoteName": resolved.resolved_remote_name,
        });

        // 缓存 connected:true 响应
        state.github_pr_status_cache.insert(cache_key, response.clone());

        return Ok(Json(response));
    };

    // 获取完整 PR
    let pr_full = match client
        .pulls_get(&search_repo.owner, &search_repo.repo, first_pr.get("number").and_then(|v| v.as_u64()).unwrap_or(0))
        .await
    {
        Ok(pr) => pr,
        Err(error) => {
            rate_limit.note_if_rate_limit(&error);
            if error.status == 401 {
                auth::clear_github_auth();
                return Ok(Json(json!({ "connected": false })));
            }
            let response = json!({
                "connected": true,
                "repo": fork_detection::repo_to_json(search_repo),
                "branch": branch,
                "pr": null,
                "checks": null,
                "canMerge": false,
            });
            return Ok(Json(response));
        }
    };

    let sha = pr_full.get("head").and_then(|h| h.get("sha")).and_then(|v| v.as_str());

    // Checks summary
    let checks = if let Some(sha) = sha {
        resolve_checks_summary(&client, &search_repo.owner, &search_repo.repo, sha).await
    } else {
        Value::Null
    };

    // Permission check
    let can_merge = resolve_can_merge(&client, &search_repo.owner, &search_repo.repo).await;

    let response = json!({
        "connected": true,
        "repo": fork_detection::repo_to_json(search_repo),
        "branch": branch,
        "pr": build_pr_summary(&pr_full, true),
        "checks": checks,
        "canMerge": can_merge,
        "defaultBranch": resolved.default_branch,
        "resolvedRemoteName": resolved.resolved_remote_name,
    });

    // 缓存 connected:true
    state.github_pr_status_cache.insert(cache_key, response.clone());

    Ok(Json(response))
}

/// 解析 checks summary (check-runs 优先, fallback classic statuses)。
async fn resolve_checks_summary(
    client: &GitHubClient,
    owner: &str,
    repo: &str,
    sha: &str,
) -> Value {
    // 1. check-runs
    if let Ok(runs_response) = client.checks_list_for_ref(owner, repo, sha).await {
        let check_runs = runs_response
            .get("check_runs")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        if !check_runs.is_empty() {
            let mut success = 0u32;
            let mut failure = 0u32;
            let mut pending = 0u32;

            for run in &check_runs {
                let status = run.get("status").and_then(|v| v.as_str()).unwrap_or("");
                let conclusion = run.get("conclusion").and_then(|v| v.as_str());

                if status == "queued" || status == "in_progress" {
                    pending += 1;
                    continue;
                }
                match conclusion {
                    None => pending += 1,
                    Some("success") | Some("neutral") | Some("skipped") => success += 1,
                    Some(_) => failure += 1,
                }
            }

            let total = success + failure + pending;
            let state = if failure > 0 {
                "failure"
            } else if pending > 0 {
                "pending"
            } else if total > 0 {
                "success"
            } else {
                "unknown"
            };

            return json!({
                "state": state,
                "total": total,
                "success": success,
                "failure": failure,
                "pending": pending,
            });
        }
    }

    // 2. fallback: classic statuses
    if let Ok(combined) = client.repos_get_combined_status_for_ref(owner, repo, sha).await {
        let statuses = combined
            .get("statuses")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        if !statuses.is_empty() {
            let mut success = 0u32;
            let mut failure = 0u32;
            let mut pending = 0u32;

            for s in &statuses {
                match s.get("state").and_then(|v| v.as_str()) {
                    Some("success") => success += 1,
                    Some("failure") | Some("error") => failure += 1,
                    Some("pending") => pending += 1,
                    _ => {}
                }
            }

            let total = success + failure + pending;
            let state = if failure > 0 {
                "failure"
            } else if pending > 0 {
                "pending"
            } else if total > 0 {
                "success"
            } else {
                "unknown"
            };

            return json!({
                "state": state,
                "total": total,
                "success": success,
                "failure": failure,
                "pending": pending,
            });
        }
    }

    Value::Null
}

/// 解析 canMerge (collaborator permission)。
async fn resolve_can_merge(client: &GitHubClient, owner: &str, repo: &str) -> bool {
    let username = auth::get_github_auth().and_then(|e| {
        e.user.as_ref().map(|u| u.login.clone())
    });
    let Some(username) = username else {
        return false;
    };

    match client
        .repos_get_collaborator_permission(owner, repo, &username)
        .await
    {
        Ok(perm) => {
            let level = perm.get("permission").and_then(|v| v.as_str()).unwrap_or("");
            level == "admin" || level == "maintain" || level == "write"
        }
        Err(_) => false,
    }
}

/// `POST /api/github/pr/create` — 创建 PR。
pub async fn pr_create(
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let directory = body.get("directory").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    let title = body.get("title").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    let head = body.get("head").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    let requested_base = body.get("base").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    let pr_body = body.get("body").and_then(|v| v.as_str()).map(String::from);
    let draft = body.get("draft").and_then(|v| v.as_bool());
    let remote = body.get("remote").and_then(|v| v.as_str()).unwrap_or("origin").trim().to_string();
    let head_remote = body.get("headRemote").and_then(|v| v.as_str()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let target_repo = body.get("targetRepo").and_then(|v| {
        if v.is_object() {
            let owner = v.get("owner")?.as_str()?.to_string();
            let repo = v.get("repo")?.as_str()?.to_string();
            Some((owner, repo))
        } else {
            None
        }
    });

    if directory.is_empty() || title.is_empty() || head.is_empty() || requested_base.is_empty() {
        return Err(ApiError(oc_core::Error::BadRequest(
            "directory, title, head, base are required".to_string(),
        )));
    }

    let Some(client) = GitHubClient::from_current_auth() else {
        return Err(ApiError(oc_core::Error::Unauthorized(
            "GitHub not connected".to_string(),
        )));
    };

    // 解析 target repo
    let repo = if let Some((owner, repo_name)) = target_repo {
        let url = format!("https://github.com/{}/{}", owner, repo_name);
        crate::github::repo::GitHubRepo {
            owner,
            repo: repo_name,
            url,
        }
    } else {
        let (resolved, _) = crate::github::repo::resolve_repo_from_directory(&directory, &remote).await;
        let Some(resolved) = resolved else {
            return Err(ApiError(oc_core::Error::BadRequest(
                "Unable to resolve GitHub repo from git remote".to_string(),
            )));
        };
        resolved
    };

    // 解析 source remote (fork-aware)
    let mut source_remote = head_remote;
    if source_remote.is_none() {
        // 从 tracking info 获取
        if let Ok(status) = crate::git::status::get_status(&directory, Default::default()).await {
            if let Some(tracking) = status.get("tracking").and_then(|v| v.as_str()) {
                if let Some(idx) = tracking.find('/') {
                    if idx > 0 {
                        source_remote = Some(tracking[..idx].to_string());
                    }
                }
            }
        }
    }
    // fallback: origin if targeting non-origin
    if source_remote.is_none() && remote != "origin" {
        source_remote = Some("origin".to_string());
    }

    let mut head_ref = head.clone();
    let mut head_repo: Option<crate::github::repo::GitHubRepo> = None;

    if let Some(ref src_remote) = source_remote {
        let (resolved, _) = crate::github::repo::resolve_repo_from_directory(&directory, src_remote).await;
        head_repo = resolved;
        let Some(ref hr) = head_repo else {
            return Err(ApiError(oc_core::Error::BadRequest(format!(
                "Cannot resolve GitHub repo for remote \"{}\". Check that the remote URL is a valid GitHub repository.",
                src_remote
            ))));
        };
        // cross-repo PR: owner:branch
        if hr.owner != repo.owner || hr.repo != repo.repo {
            head_ref = format!("{}:{}", hr.owner, head);
        }
    }

    // 验证 cross-repo head branch
    if head_ref.contains(':') {
        let head_owner = head_ref.split(':').next().unwrap_or("");
        let head_repo_name = head_repo.as_ref().map(|r| r.repo.as_str()).unwrap_or(&repo.repo);
        if !head_repo_name.is_empty() {
            if let Err(error) = client.repos_get_branch(head_owner, head_repo_name, &head).await {
                if error.status == 404 {
                    return Err(ApiError(oc_core::Error::BadRequest(format!(
                        "Branch \"{}\" not found on {}/{}. Please push your branch first: git push {} {}",
                        head, head_owner, head_repo_name,
                        source_remote.as_deref().unwrap_or("origin"), head
                    ))));
                }
                // 其他错误继续, 让 PR create 处理
            }
        }
    }

    // 创建 PR
    let mut create_body = json!({
        "title": title,
        "head": head_ref,
        "base": requested_base,
    });
    if let Some(ref b) = pr_body {
        create_body["body"] = json!(b);
    }
    if let Some(d) = draft {
        create_body["draft"] = json!(d);
    }

    match client.pulls_create(&repo.owner, &repo.repo, &create_body).await {
        Ok(pr) => Ok(Json(build_pr_summary(&pr, false))),
        Err(error) => {
            let msg = &error.message;
            // head validation error
            if msg.contains("Validation Failed") && msg.contains("\"field\":\"head\"") && msg.contains("\"code\":\"invalid\"") {
                return Err(ApiError(oc_core::Error::BadRequest(
                    "Unable to create PR: You must have write access to the source repository. Make sure you have pushed your branch to a repository you own (your fork), and that the branch exists on the remote.".to_string(),
                )));
            }
            Err(ApiError(oc_core::Error::Internal(msg.clone())))
        }
    }
}

/// `POST /api/github/pr/update` — 更新 PR title/body。
pub async fn pr_update(
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let directory = body.get("directory").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    let number = body.get("number").and_then(|v| v.as_u64());
    let title = body.get("title").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    let pr_body = body.get("body").and_then(|v| v.as_str()).map(String::from);

    if directory.is_empty() || number.is_none() || title.is_empty() {
        return Err(ApiError(oc_core::Error::BadRequest(
            "directory, number, title are required".to_string(),
        )));
    }
    let number = number.unwrap();

    let Some(client) = GitHubClient::from_current_auth() else {
        return Err(ApiError(oc_core::Error::Unauthorized(
            "GitHub not connected".to_string(),
        )));
    };

    let (repo, _) = crate::github::repo::resolve_repo_from_directory(&directory, "origin").await;
    let Some(repo) = repo else {
        return Err(ApiError(oc_core::Error::BadRequest(
            "Unable to resolve GitHub repo from git remote".to_string(),
        )));
    };

    let mut update_body = json!({ "title": title });
    if let Some(ref b) = pr_body {
        update_body["body"] = json!(b);
    }

    match client.pulls_update(&repo.owner, &repo.repo, number, &update_body).await {
        Ok(pr) => Ok(Json(build_pr_summary(&pr, true))),
        Err(error) => {
            match error.status {
                401 => Err(ApiError(oc_core::Error::Unauthorized("GitHub not connected".to_string()))),
                403 => Err(ApiError(oc_core::Error::Forbidden("Not authorized to edit this PR".to_string()))),
                404 => Err(ApiError(oc_core::Error::NotFound("PR not found in this repository".to_string()))),
                422 => Err(ApiError(oc_core::Error::BadRequest(error.message))),
                _ => Err(ApiError(oc_core::Error::Internal(error.message))),
            }
        }
    }
}

/// `POST /api/github/pr/merge` — 合并 PR。
pub async fn pr_merge(
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let directory = body.get("directory").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    let number = body.get("number").and_then(|v| v.as_u64());
    let method = body.get("method").and_then(|v| v.as_str()).unwrap_or("merge").to_string();

    if directory.is_empty() || number.is_none() {
        return Err(ApiError(oc_core::Error::BadRequest(
            "directory and number are required".to_string(),
        )));
    }
    let number = number.unwrap();

    let Some(client) = GitHubClient::from_current_auth() else {
        return Err(ApiError(oc_core::Error::Unauthorized(
            "GitHub not connected".to_string(),
        )));
    };

    let (repo, _) = crate::github::repo::resolve_repo_from_directory(&directory, "origin").await;
    let Some(repo) = repo else {
        return Err(ApiError(oc_core::Error::BadRequest(
            "Unable to resolve GitHub repo from git remote".to_string(),
        )));
    };

    let merge_body = json!({ "merge_method": method });
    match client.pulls_merge(&repo.owner, &repo.repo, number, &merge_body).await {
        Ok(_) => Ok(Json(json!({ "merged": true, "message": "Pull request merged" }))),
        Err(error) => {
            match error.status {
                405 | 409 => Ok(Json(json!({ "merged": false, "message": error.message }))),
                _ => Err(ApiError(oc_core::Error::Internal(error.message))),
            }
        }
    }
}

/// `POST /api/github/pr/ready` — 标记 ready for review (GraphQL)。
pub async fn pr_ready(
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let directory = body.get("directory").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    let number = body.get("number").and_then(|v| v.as_u64());

    if directory.is_empty() || number.is_none() {
        return Err(ApiError(oc_core::Error::BadRequest(
            "directory and number are required".to_string(),
        )));
    }
    let number = number.unwrap();

    let Some(client) = GitHubClient::from_current_auth() else {
        return Err(ApiError(oc_core::Error::Unauthorized(
            "GitHub not connected".to_string(),
        )));
    };

    let (repo, _) = crate::github::repo::resolve_repo_from_directory(&directory, "origin").await;
    let Some(repo) = repo else {
        return Err(ApiError(oc_core::Error::BadRequest(
            "Unable to resolve GitHub repo from git remote".to_string(),
        )));
    };

    // 先获取 PR 的 node_id
    let pr = client.pulls_get(&repo.owner, &repo.repo, number).await
        .map_err(|e| ApiError(oc_core::Error::Internal(e.message)))?;
    let node_id = pr.get("node_id").and_then(|v| v.as_str()).unwrap_or("");

    if node_id.is_empty() {
        // 可能已经不是 draft
        return Ok(Json(json!({ "ready": true })));
    }

    let query = "mutation MarkPullRequestReadyForReview($input: MarkPullRequestReadyForReviewInput!) { markPullRequestReadyForReview(input: $input) { pullRequest { id isDraft } } }";
    let variables = json!({
        "input": { "pullRequestId": node_id }
    });

    match client.graphql(query, variables).await {
        Ok(_) => Ok(Json(json!({ "ready": true }))),
        Err(error) => {
            // 如果已经不是 draft, 视为成功
            if error.status == 403 || error.message.contains("not a draft") {
                return Ok(Json(json!({ "ready": true })));
            }
            Err(ApiError(oc_core::Error::Internal(error.message)))
        }
    }
}

// ============================================================
// Repo routes
// ============================================================

/// `GET /api/github/repo/upstream` — fork 检测 + upstream 信息。
pub async fn repo_upstream(
    State(state): State<Arc<AppState>>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    let directory = opt_query(&params, "directory");
    if directory.is_empty() {
        return Err(ApiError(oc_core::Error::BadRequest(
            "directory is required".to_string(),
        )));
    }

    let Some(client) = GitHubClient::from_current_auth() else {
        return Err(ApiError(oc_core::Error::Unauthorized(
            "GitHub not connected".to_string(),
        )));
    };

    // 检查 rate-limit
    if state.github_rate_limit.is_rate_limited() {
        return Ok(Json(json!({ "connected": true, "isFork": false, "upstream": null })));
    }

    let (repo, _) = crate::github::repo::resolve_repo_from_directory(&directory, "origin").await;
    let Some(repo) = repo else {
        return Ok(Json(json!({ "connected": true, "isFork": false, "upstream": null })));
    };

    let metadata = fork_detection::get_repo_metadata(&client, &repo)
        .await
        .map_err(|e| ApiError(oc_core::Error::Internal(e.message)))?;

    let Some(metadata) = metadata else {
        return Ok(Json(json!({ "connected": true, "isFork": false, "upstream": null })));
    };

    let is_fork = metadata.get("fork").and_then(|v| v.as_bool()).unwrap_or(false);

    let upstream = if let Some(parent) = metadata.get("parent").filter(|v| v.is_object()) {
        let owner = parent.get("owner").and_then(|o| o.get("login")).and_then(|v| v.as_str()).unwrap_or("");
        let name = parent.get("name").and_then(|v| v.as_str()).unwrap_or("");
        if !owner.is_empty() && !name.is_empty() {
            // 获取 default branch sha
            let default_branch = parent.get("default_branch").and_then(|v| v.as_str()).unwrap_or("");
            let default_branch_sha = if !default_branch.is_empty() {
                client.git_get_ref(owner, name, &format!("heads/{}", default_branch))
                    .await
                    .ok()
                    .and_then(|ref_data| {
                        ref_data.get("object").and_then(|o| o.get("sha")).and_then(|v| v.as_str()).map(String::from)
                    })
            } else {
                None
            };

            Some(json!({
                "owner": owner,
                "repo": name,
                "url": format!("https://github.com/{}/{}", owner, name),
                "defaultBranch": default_branch,
                "defaultBranchSha": default_branch_sha,
                "remoteName": "upstream",
            }))
        } else {
            None
        }
    } else {
        None
    };

    Ok(Json(json!({
        "connected": true,
        "isFork": is_fork,
        "upstream": upstream,
    })))
}

/// `GET /api/github/repo/branches` — 分支列表 (分页)。
pub async fn repo_branches(
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    let owner = opt_query(&params, "owner");
    let repo = opt_query(&params, "repo");

    if owner.is_empty() || repo.is_empty() {
        return Err(ApiError(oc_core::Error::BadRequest(
            "owner and repo are required".to_string(),
        )));
    }

    let Some(client) = GitHubClient::from_current_auth() else {
        return Err(ApiError(oc_core::Error::Unauthorized(
            "GitHub not connected".to_string(),
        )));
    };

    let mut all_branches = Vec::new();
    for page in 1..=10 {
        match client.repos_list_branches(&owner, &repo, page).await {
            Ok(branches) => {
                if branches.is_empty() {
                    break;
                }
                for b in &branches {
                    if let Some(name) = b.get("name").and_then(|v| v.as_str()) {
                        all_branches.push(name.to_string());
                    }
                }
                if branches.len() < 100 {
                    break;
                }
            }
            Err(error) => {
                return Err(ApiError(oc_core::Error::Internal(error.message)));
            }
        }
    }

    Ok(Json(json!({ "branches": all_branches })))
}

// ============================================================
// Issue routes
// ============================================================

/// `GET /api/github/issues/list` — issue 列表 (跨 repo network 查询)。
pub async fn issues_list(
    State(state): State<Arc<AppState>>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    let directory = opt_query(&params, "directory");
    let page = params.get("page").and_then(|v| v.parse::<u32>().ok()).unwrap_or(1);
    let search_query = opt_query(&params, "query");

    if directory.is_empty() {
        return Err(ApiError(oc_core::Error::BadRequest(
            "directory is required".to_string(),
        )));
    }

    let effective_page = if page > 0 { page } else { 1 };

    let Some(client) = GitHubClient::from_current_auth() else {
        return Ok(Json(json!({ "connected": false })));
    };

    // resolve repo network
    let network = fork_detection::resolve_repo_network(&client, &directory, "origin")
        .await
        .map_err(|e| ApiError(oc_core::Error::Internal(e.message)))?;

    let (origin_repo, _) = crate::github::repo::resolve_repo_from_directory(&directory, "origin").await;

    let repos_to_query: Vec<fork_detection::RepoNetworkEntry> = match network {
        Some(net) => net,
        None => {
            // 不是 fork → 只查 origin
            origin_repo
                .as_ref()
                .map(|r| vec![fork_detection::RepoNetworkEntry {
                    owner: r.owner.clone(),
                    repo: r.repo.clone(),
                    url: r.url.clone(),
                    source: "origin".to_string(),
                }])
                .unwrap_or_default()
        }
    };

    // repo JSON (对应 Node 端的 resolveGitHubRepoFromDirectory 结果)
    let repo_json = origin_repo
        .as_ref()
        .map(|r| json!({ "owner": r.owner, "repo": r.repo, "url": r.url }))
        .unwrap_or(Value::Null);

    let map_issue_summary = |item: &Value, entry: &fork_detection::RepoNetworkEntry| -> Value {
        let labels = item.get("labels").and_then(|v| v.as_array()).map(|arr| {
            arr.iter()
                .filter_map(|label| {
                    // 字符串 label 跳过 (Node 端也跳过)
                    if label.is_string() {
                        return None;
                    }
                    let name = label.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    if name.is_empty() {
                        return None;
                    }
                    let color = label.get("color").and_then(|v| v.as_str());
                    Some(json!({ "name": name, "color": color }))
                })
                .collect::<Vec<_>>()
        }).unwrap_or_default();

        json!({
            "number": item.get("number").cloned().unwrap_or(Value::Null),
            "title": item.get("title").cloned().unwrap_or(Value::Null),
            "url": item.get("html_url").cloned().unwrap_or(Value::Null),
            "state": if item.get("state").and_then(|v| v.as_str()) == Some("closed") { "closed" } else { "open" },
            "author": item.get("user").map(|u| json!({
                "login": u.get("login").cloned().unwrap_or(Value::Null),
                "id": u.get("id").cloned().unwrap_or(Value::Null),
                "avatarUrl": u.get("avatar_url").cloned().unwrap_or(Value::Null),
            })).unwrap_or(Value::Null),
            "labels": labels,
            "sourceRepo": {
                "owner": entry.owner,
                "repo": entry.repo,
                "source": entry.source,
            },
        })
    };

    // 有 search query → 走 search API
    if !search_query.is_empty() {
        let repo_qualifiers = repos_to_query
            .iter()
            .map(|r| format!("repo:{}/{}", r.owner, r.repo))
            .collect::<Vec<_>>()
            .join(" ");
        let q = format!("{} {} type:issue state:open", repo_qualifiers, search_query);

        match client.search_issues(&q, 50, effective_page).await {
            Ok(result) => {
                let total_count = result.get("total_count").and_then(|v| v.as_u64()).unwrap_or(0);
                let items = result.get("items").and_then(|v| v.as_array()).cloned().unwrap_or_default();
                let issues: Vec<Value> = items
                    .iter()
                    .filter(|item| item.get("pull_request").is_none())
                    .map(|item| {
                        // 匹配 sourceRepo
                        let repo_full_name = item
                            .get("repository_url")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .replace("https://api.github.com/repos/", "");
                        let matched = repos_to_query
                            .iter()
                            .find(|r| format!("{}/{}", r.owner, r.repo) == repo_full_name);
                        let entry = matched.unwrap_or(&repos_to_query[0]);
                        map_issue_summary(item, entry)
                    })
                    .collect();
                let fetched_count = (effective_page as u64 - 1) * 50 + items.len() as u64;
                let has_more = fetched_count < total_count;
                return Ok(Json(json!({
                    "connected": true,
                    "repo": repo_json,
                    "issues": issues,
                    "page": effective_page,
                    "hasMore": has_more,
                })));
            }
            Err(_) => {
                return Ok(Json(json!({
                    "connected": true,
                    "repo": repo_json,
                    "issues": [],
                    "page": effective_page,
                    "hasMore": false,
                })));
            }
        }
    }

    // 无 search query → 逐 repo listForRepo
    let mut all_issues = Vec::new();
    let mut has_more = false;

    for entry in &repos_to_query {
        match client.issues_list_for_repo(&entry.owner, &entry.repo, "open", 50, effective_page).await {
            Ok(issues) => {
                // per_page=50, 满页说明可能有更多
                if issues.len() >= 50 {
                    has_more = true;
                }
                for issue in &issues {
                    // 过滤掉 PR (item.pull_request 存在)
                    if issue.get("pull_request").is_some() {
                        continue;
                    }
                    all_issues.push(map_issue_summary(issue, entry));
                }
            }
            Err(error) => {
                state.github_rate_limit.note_if_rate_limit(&error);
                if error.status != 404 && error.status != 403 {
                    return Err(ApiError(oc_core::Error::Internal(error.message)));
                }
            }
        }
    }

    Ok(Json(json!({
        "connected": true,
        "repo": repo_json,
        "issues": all_issues,
        "page": effective_page,
        "hasMore": has_more,
    })))
}

/// `GET /api/github/issues/get` — 单个 issue。
pub async fn issues_get(
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    let directory = opt_query(&params, "directory");
    let number = params.get("number").and_then(|v| v.parse::<u64>().ok());
    let owner_override = opt_query(&params, "owner");
    let repo_override = opt_query(&params, "repo");

    if directory.is_empty() || number.is_none() {
        return Err(ApiError(oc_core::Error::BadRequest(
            "directory and number are required".to_string(),
        )));
    }
    let number = number.unwrap();

    let Some(client) = GitHubClient::from_current_auth() else {
        return Err(ApiError(oc_core::Error::Unauthorized(
            "GitHub not connected".to_string(),
        )));
    };

    let (owner, repo_name) = if !owner_override.is_empty() && !repo_override.is_empty() {
        (owner_override, repo_override)
    } else {
        let (repo, _) = crate::github::repo::resolve_repo_from_directory(&directory, "origin").await;
        let Some(repo) = repo else {
            return Err(ApiError(oc_core::Error::BadRequest(
                "Unable to resolve GitHub repo from git remote".to_string(),
            )));
        };
        (repo.owner, repo.repo)
    };

    let issue = client.issues_get(&owner, &repo_name, number).await
        .map_err(|e| ApiError(oc_core::Error::Internal(e.message)))?;

    // 如果是 PR, 返回 400
    if issue.get("pull_request").is_some() {
        return Err(ApiError(oc_core::Error::BadRequest(
            "#{} is a pull request, not an issue".to_string(),
        )));
    }

    Ok(Json(json!({
        "connected": true,
        "repo": { "owner": owner, "repo": repo_name },
        "issue": {
            "number": issue.get("number").cloned().unwrap_or(Value::Null),
            "title": issue.get("title").cloned().unwrap_or(Value::Null),
            "url": issue.get("html_url").cloned().unwrap_or(Value::Null),
            "state": issue.get("state").cloned().unwrap_or(Value::Null),
            "body": issue.get("body").cloned().unwrap_or(Value::Null),
            "createdAt": issue.get("created_at").cloned().unwrap_or(Value::Null),
            "updatedAt": issue.get("updated_at").cloned().unwrap_or(Value::Null),
            "author": {
                "login": issue.get("user").and_then(|u| u.get("login")).cloned().unwrap_or(Value::Null),
                "id": issue.get("user").and_then(|u| u.get("id")).cloned().unwrap_or(Value::Null),
                "avatarUrl": issue.get("user").and_then(|u| u.get("avatar_url")).cloned().unwrap_or(Value::Null),
            },
            "assignees": issue.get("assignees").cloned().unwrap_or(json!([])),
            "labels": issue.get("labels").cloned().unwrap_or(json!([])),
        },
    })))
}

/// `GET /api/github/issues/comments` — issue 评论。
pub async fn issues_comments(
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    let directory = opt_query(&params, "directory");
    let number = params.get("number").and_then(|v| v.parse::<u64>().ok());
    let owner_override = opt_query(&params, "owner");
    let repo_override = opt_query(&params, "repo");

    if directory.is_empty() || number.is_none() {
        return Err(ApiError(oc_core::Error::BadRequest(
            "directory and number are required".to_string(),
        )));
    }
    let number = number.unwrap();

    let Some(client) = GitHubClient::from_current_auth() else {
        return Err(ApiError(oc_core::Error::Unauthorized(
            "GitHub not connected".to_string(),
        )));
    };

    let (owner, repo_name) = if !owner_override.is_empty() && !repo_override.is_empty() {
        (owner_override, repo_override)
    } else {
        let (repo, _) = crate::github::repo::resolve_repo_from_directory(&directory, "origin").await;
        let Some(repo) = repo else {
            return Err(ApiError(oc_core::Error::BadRequest(
                "Unable to resolve GitHub repo from git remote".to_string(),
            )));
        };
        (repo.owner, repo.repo)
    };

    let comments = client.issues_list_comments(&owner, &repo_name, number).await
        .map_err(|e| ApiError(oc_core::Error::Internal(e.message)))?;

    let mapped: Vec<Value> = comments.iter().map(|c| {
        json!({
            "id": c.get("id").cloned().unwrap_or(Value::Null),
            "url": c.get("html_url").cloned().unwrap_or(Value::Null),
            "body": c.get("body").cloned().unwrap_or(Value::Null),
            "createdAt": c.get("created_at").cloned().unwrap_or(Value::Null),
            "updatedAt": c.get("updated_at").cloned().unwrap_or(Value::Null),
            "author": {
                "login": c.get("user").and_then(|u| u.get("login")).cloned().unwrap_or(Value::Null),
                "id": c.get("user").and_then(|u| u.get("id")).cloned().unwrap_or(Value::Null),
                "avatarUrl": c.get("user").and_then(|u| u.get("avatar_url")).cloned().unwrap_or(Value::Null),
            },
        })
    }).collect();

    Ok(Json(json!({
        "connected": true,
        "repo": { "owner": owner, "repo": repo_name },
        "comments": mapped,
    })))
}

// ============================================================
// Pull context routes
// ============================================================

/// `GET /api/github/pulls/list` — PR 列表。
pub async fn pulls_list(
    State(state): State<Arc<AppState>>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    let directory = opt_query(&params, "directory");
    let page = params.get("page").and_then(|v| v.parse::<u32>().ok()).unwrap_or(1);

    if directory.is_empty() {
        return Err(ApiError(oc_core::Error::BadRequest(
            "directory is required".to_string(),
        )));
    }

    let Some(client) = GitHubClient::from_current_auth() else {
        return Err(ApiError(oc_core::Error::Unauthorized(
            "GitHub not connected".to_string(),
        )));
    };

    let (repo, _) = crate::github::repo::resolve_repo_from_directory(&directory, "origin").await;
    let Some(repo) = repo else {
        return Err(ApiError(oc_core::Error::BadRequest(
            "Unable to resolve GitHub repo from git remote".to_string(),
        )));
    };

    let prs = client.pulls_list(&repo.owner, &repo.repo, "open", None).await
        .map_err(|e| {
            state.github_rate_limit.note_if_rate_limit(&e);
            ApiError(oc_core::Error::Internal(e.message))
        })?;

    let has_more = prs.len() >= 100;

    let mapped: Vec<Value> = prs.iter().map(|pr| {
        json!({
            "number": pr.get("number").cloned().unwrap_or(Value::Null),
            "title": pr.get("title").cloned().unwrap_or(Value::Null),
            "url": pr.get("html_url").cloned().unwrap_or(Value::Null),
            "state": pr.get("state").cloned().unwrap_or(Value::Null),
            "draft": pr.get("draft").and_then(|v| v.as_bool()).unwrap_or(false),
            "base": pr.get("base").and_then(|b| b.get("ref")).cloned().unwrap_or(Value::Null),
            "head": pr.get("head").and_then(|h| h.get("ref")).cloned().unwrap_or(Value::Null),
            "headSha": pr.get("head").and_then(|h| h.get("sha")).cloned().unwrap_or(Value::Null),
            "mergeable": pr.get("mergeable").cloned().unwrap_or(Value::Null),
            "mergeableState": pr.get("mergeable_state").cloned().unwrap_or(Value::Null),
            "author": {
                "login": pr.get("user").and_then(|u| u.get("login")).cloned().unwrap_or(Value::Null),
                "id": pr.get("user").and_then(|u| u.get("id")).cloned().unwrap_or(Value::Null),
                "avatarUrl": pr.get("user").and_then(|u| u.get("avatar_url")).cloned().unwrap_or(Value::Null),
            },
            "headLabel": pr.get("head").and_then(|h| h.get("label")).cloned().unwrap_or(Value::Null),
            "headRepo": pr.get("head").and_then(|h| h.get("repo")).and_then(|r| {
                if r.is_null() { None } else {
                    Some(json!({
                        "owner": r.get("owner").and_then(|o| o.get("login")).cloned().unwrap_or(Value::Null),
                        "repo": r.get("name").cloned().unwrap_or(Value::Null),
                        "url": r.get("html_url").cloned().unwrap_or(Value::Null),
                        "cloneUrl": r.get("clone_url").cloned().unwrap_or(Value::Null),
                        "sshUrl": r.get("ssh_url").cloned().unwrap_or(Value::Null),
                    }))
                }
            }).unwrap_or(Value::Null),
            "sourceRepo": {
                "owner": repo.owner,
                "repo": repo.repo,
                "source": "origin",
            },
        })
    }).collect();

    Ok(Json(json!({
        "connected": true,
        "repo": fork_detection::repo_to_json(&repo),
        "prs": mapped,
        "page": page,
        "hasMore": has_more,
    })))
}

/// `GET /api/github/pulls/context` — PR 完整上下文。
pub async fn pulls_context(
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    let directory = opt_query(&params, "directory");
    let number = params.get("number").and_then(|v| v.parse::<u64>().ok());
    let want_diff = opt_query_bool(&params, "diff");
    let want_check_details = opt_query_bool(&params, "checkDetails");
    let owner_override = opt_query(&params, "owner");
    let repo_override = opt_query(&params, "repo");

    if directory.is_empty() || number.is_none() {
        return Err(ApiError(oc_core::Error::BadRequest(
            "directory and number are required".to_string(),
        )));
    }
    let number = number.unwrap();

    let Some(client) = GitHubClient::from_current_auth() else {
        return Err(ApiError(oc_core::Error::Unauthorized(
            "GitHub not connected".to_string(),
        )));
    };

    let (owner, repo_name) = if !owner_override.is_empty() && !repo_override.is_empty() {
        (owner_override, repo_override)
    } else {
        let (repo, _) = crate::github::repo::resolve_repo_from_directory(&directory, "origin").await;
        let Some(repo) = repo else {
            return Err(ApiError(oc_core::Error::BadRequest(
                "Unable to resolve GitHub repo from git remote".to_string(),
            )));
        };
        (repo.owner, repo.repo)
    };

    let pr = client.pulls_get(&owner, &repo_name, number).await
        .map_err(|e| ApiError(oc_core::Error::Internal(e.message)))?;

    // Issue comments + review comments + files (并发)
    let (issue_comments, review_comments, files) = tokio::join!(
        client.issues_list_comments(&owner, &repo_name, number),
        client.pulls_list_review_comments(&owner, &repo_name, number),
        client.pulls_list_files(&owner, &repo_name, number),
    );

    let issue_comments = issue_comments.unwrap_or_default();
    let review_comments = review_comments.unwrap_or_default();
    let files = files.unwrap_or_default();

    // diff (可选)
    let diff = if want_diff {
        Some(client.pulls_get_diff(&owner, &repo_name, number).await.unwrap_or_default())
    } else {
        None
    };

    // checks
    let sha = pr.get("head").and_then(|h| h.get("sha")).and_then(|v| v.as_str()).unwrap_or("");
    let checks = if !sha.is_empty() {
        resolve_checks_summary(&client, &owner, &repo_name, sha).await
    } else {
        Value::Null
    };

    // check runs detail (可选)
    let check_runs = if want_check_details && !sha.is_empty() {
        resolve_check_runs_detail(&client, &owner, &repo_name, sha).await
    } else {
        Value::Null
    };

    Ok(Json(json!({
        "connected": true,
        "repo": { "owner": owner, "repo": repo_name, "url": format!("https://github.com/{}/{}", owner, repo_name) },
        "pr": build_pr_summary(&pr, true),
        "issueComments": issue_comments,
        "reviewComments": review_comments,
        "files": files,
        "diff": diff,
        "checks": checks,
        "checkRuns": check_runs,
    })))
}

/// 解析 check runs 详情 (jobs + annotations)。
async fn resolve_check_runs_detail(
    client: &GitHubClient,
    owner: &str,
    repo: &str,
    sha: &str,
) -> Value {
    let Ok(runs_response) = client.checks_list_for_ref(owner, repo, sha).await else {
        return Value::Null;
    };
    let check_runs = runs_response
        .get("check_runs")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let mut detailed = Vec::new();
    for run in &check_runs {
        let run_id = run.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
        let conclusion = run.get("conclusion").and_then(|v| v.as_str());

        // 只获取失败 run 的 annotations
        let annotations = if conclusion == Some("failure") || conclusion == Some("error") {
            let mut all_annotations = Vec::new();
            for page in 1..=3 {
                match client.checks_list_annotations(owner, repo, run_id, page).await {
                    Ok(anns) => {
                        if anns.is_empty() {
                            break;
                        }
                        let len = anns.len();
                        all_annotations.extend(anns);
                        if len < 100 {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            Value::Array(all_annotations)
        } else {
            Value::Array(Vec::new())
        };

        // jobs (Actions runs only)
        let jobs = if run.get("details_url").and_then(|v| v.as_str()).map(|s| s.contains("actions")).unwrap_or(false) {
            // 从 details_url 提取 run_id (简化: 用 check run id 直接查)
            client.actions_list_jobs(owner, repo, run_id).await
                .ok()
                .and_then(|j| j.get("jobs").cloned())
                .unwrap_or(Value::Array(Vec::new()))
        } else {
            Value::Array(Vec::new())
        };

        detailed.push(json!({
            "id": run.get("id").cloned().unwrap_or(Value::Null),
            "name": run.get("name").cloned().unwrap_or(Value::Null),
            "status": run.get("status").cloned().unwrap_or(Value::Null),
            "conclusion": run.get("conclusion").cloned().unwrap_or(Value::Null),
            "htmlUrl": run.get("html_url").cloned().unwrap_or(Value::Null),
            "annotations": annotations,
            "jobs": jobs,
        }));
    }

    Value::Array(detailed)
}
