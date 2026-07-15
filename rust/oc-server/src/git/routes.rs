//! Git 路由 handlers — 68 个端点。
//!
//! 移植自 `packages/web/server/lib/git/routes.js`。
//! 所有 handler 返回 `ApiResult<Json<Value>>`, 通过 axum 路由注册。

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::header::HeaderName;
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

// ============================================================
// Helper functions
// ============================================================

/// 从 query params 提取 directory, 缺失返回 400。
fn require_directory(params: &HashMap<String, String>) -> ApiResult<String> {
    params
        .get("directory")
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            ApiError(oc_core::Error::BadRequest(
                "directory parameter is required".to_string(),
            ))
        })
}

/// 从 query params 提取可选 string。
fn opt_query<'a>(params: &'a HashMap<String, String>, key: &str) -> Option<&'a str> {
    params.get(key).map(|s| s.as_str()).filter(|s| !s.is_empty())
}

/// 验证 commit hash 格式。
fn validate_hash(hash: &str) -> ApiResult<()> {
    if !crate::git::paths::is_valid_commit_hash(hash) {
        return Err(ApiError(oc_core::Error::BadRequest(
            "Invalid commit hash".to_string(),
        )));
    }
    Ok(())
}

// ============================================================
// Identity Profiles CRUD
// ============================================================

/// GET /api/git/identities
pub async fn list_identities() -> ApiResult<Json<Value>> {
    let profiles = crate::git::identity::get_profiles();
    Ok(Json(json!(profiles)))
}

/// POST /api/git/identities
pub async fn create_identity(Json(body): Json<Value>) -> ApiResult<Json<Value>> {
    let profile = crate::git::identity::create_profile(&body).map_err(ApiError::from)?;
    Ok(Json(profile))
}

/// PUT /api/git/identities/{id}
pub async fn update_identity(
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let profile = crate::git::identity::update_profile(&id, &body).map_err(ApiError::from)?;
    Ok(Json(profile))
}

/// DELETE /api/git/identities/{id}
pub async fn delete_identity(Path(id): Path<String>) -> ApiResult<Json<Value>> {
    crate::git::identity::delete_profile(&id).map_err(ApiError::from)?;
    Ok(Json(json!({ "success": true })))
}

// ============================================================
// Identity / Credentials Discovery
// ============================================================

/// GET /api/git/global-identity
pub async fn global_identity() -> ApiResult<Json<Value>> {
    let identity = crate::git::identity::get_global_identity().await?;
    Ok(Json(identity))
}

/// GET /api/git/discover-credentials
pub async fn discover_credentials() -> ApiResult<Json<Value>> {
    let credentials = crate::git::identity::discover_git_credentials();
    Ok(Json(json!(credentials)))
}

/// GET /api/git/current-identity
pub async fn current_identity(
    Query(params): Query<HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    let directory = require_directory(&params)?;
    let identity = crate::git::identity::get_current_identity(&directory).await?;
    Ok(Json(identity))
}

/// GET /api/git/has-local-identity
pub async fn has_local_identity(
    Query(params): Query<HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    let directory = require_directory(&params)?;
    let has_local = crate::git::identity::has_local_identity(&directory).await?;
    Ok(Json(json!({ "hasLocalIdentity": has_local })))
}

/// POST /api/git/set-identity
pub async fn set_identity(
    Query(params): Query<HashMap<String, String>>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let directory = require_directory(&params)?;
    let profile_id = body
        .get("profileId")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if profile_id.is_empty() {
        return Err(ApiError(oc_core::Error::BadRequest(
            "profileId is required".to_string(),
        )));
    }

    let profile = if profile_id == "global" {
        let global = crate::git::identity::get_global_identity().await?;
        let user_name = global.get("userName").and_then(|v| v.as_str()).unwrap_or("");
        let user_email = global.get("userEmail").and_then(|v| v.as_str()).unwrap_or("");

        if user_name.is_empty() || user_email.is_empty() {
            return Err(ApiError(oc_core::Error::NotFound(
                "Global identity is not configured".to_string(),
            )));
        }

        let ssh_command = global.get("sshCommand").and_then(|v| v.as_str()).unwrap_or("");
        let ssh_key = if !ssh_command.is_empty() {
            ssh_command.replace("ssh -i ", "")
        } else {
            String::new()
        };

        json!({
            "id": "global",
            "name": "Global Identity",
            "userName": user_name,
            "userEmail": user_email,
            "sshKey": if ssh_key.is_empty() { Value::Null } else { json!(ssh_key) },
        })
    } else {
        
        crate::git::identity::get_profile(profile_id).ok_or_else(|| {
            ApiError(oc_core::Error::NotFound("Profile not found".to_string()))
        })?
    };

    let result = crate::git::identity::set_local_identity(&directory, &profile).await?;
    Ok(Json(result))
}

// ============================================================
// Repository Introspection
// ============================================================

/// GET /api/git/check
pub async fn check(
    Query(params): Query<HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    let directory = require_directory(&params)?;
    let is_repo = crate::git::status::is_git_repository(&directory).await;
    Ok(Json(json!({ "isGitRepository": is_repo })))
}

/// GET /api/git/remote-url
pub async fn remote_url(
    Query(params): Query<HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    let directory = require_directory(&params)?;
    let remote = opt_query(&params, "remote").unwrap_or("origin");
    let url = crate::git::remote::get_remote_url(&directory, remote).await?;
    Ok(Json(json!({ "url": url })))
}

/// GET /api/git/primary-root
pub async fn primary_root(
    Query(params): Query<HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    let directory = require_directory(&params)?;
    let result = crate::git::worktree::resolve_primary_worktree_root(&directory).await?;
    Ok(Json(result))
}

/// GET /api/git/toplevel
pub async fn toplevel(
    Query(params): Query<HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    let directory = require_directory(&params)?;
    let result = crate::git::worktree::resolve_worktree_top_level(&directory).await?;
    Ok(Json(result))
}

// ============================================================
// Status & Diff
// ============================================================

/// GET /api/git/status
pub async fn status(
    Query(params): Query<HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    let directory = require_directory(&params)?;

    // 先检查是否是 git repo
    let is_repo = crate::git::status::is_git_repository(&directory).await;
    if !is_repo {
        return Ok(Json(json!({
            "isGitRepository": false,
            "files": [],
            "branch": null,
            "ahead": 0,
            "behind": 0,
        })));
    }

    let mode = opt_query(&params, "mode").map(|m| m.to_string());
    let status = crate::git::status::get_status(
        &directory,
        crate::git::status::StatusOptions { mode },
    )
    .await?;
    Ok(Json(status))
}

/// GET /api/git/diff
pub async fn diff(
    Query(params): Query<HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    let directory = require_directory(&params)?;
    let file_path = opt_query(&params, "path")
        .ok_or_else(|| {
            ApiError(oc_core::Error::BadRequest(
                "path parameter is required".to_string(),
            ))
        })?
        .to_string();
    let staged = opt_query(&params, "staged") == Some("true");
    let context_lines = opt_query(&params, "context")
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(3);

    let diff = crate::git::diff::get_diff(&directory, Some(&file_path), staged, context_lines).await?;
    Ok(Json(json!({ "diff": diff })))
}

/// GET /api/git/file-diff
pub async fn file_diff(
    Query(params): Query<HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    let directory = require_directory(&params)?;
    let file_path = opt_query(&params, "path")
        .ok_or_else(|| {
            ApiError(oc_core::Error::BadRequest(
                "path parameter is required".to_string(),
            ))
        })?
        .to_string();
    let staged = opt_query(&params, "staged") == Some("true");

    let result = crate::git::diff::get_file_diff(&directory, &file_path, staged).await?;
    Ok(Json(result))
}

// ============================================================
// Working-Tree Mutations
// ============================================================

/// POST /api/git/revert
pub async fn revert_file(
    Query(params): Query<HashMap<String, String>>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let directory = require_directory(&params)?;
    let file_path = body
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            ApiError(oc_core::Error::BadRequest(
                "path parameter is required".to_string(),
            ))
        })?;
    let scope = body
        .get("scope")
        .and_then(|v| v.as_str())
        .unwrap_or("all");

    crate::git::commit::revert_file(&directory, file_path, scope).await?;
    Ok(Json(json!({ "success": true })))
}

/// POST /api/git/stage
pub async fn stage(
    Query(params): Query<HashMap<String, String>>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let directory = require_directory(&params)?;

    let paths: Vec<String> = if let Some(paths_arr) = body.get("paths").and_then(|v| v.as_array()) {
        paths_arr
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect()
    } else if let Some(path) = body.get("path").and_then(|v| v.as_str()) {
        vec![path.to_string()]
    } else {
        return Err(ApiError(oc_core::Error::BadRequest(
            "path parameter is required".to_string(),
        )));
    };

    if !paths.iter().any(|s| !s.trim().is_empty()) {
        return Err(ApiError(oc_core::Error::BadRequest(
            "path parameter is required".to_string(),
        )));
    }

    crate::git::commit::stage_files(&directory, &paths).await?;
    Ok(Json(json!({ "success": true })))
}

/// POST /api/git/unstage
pub async fn unstage(
    Query(params): Query<HashMap<String, String>>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let directory = require_directory(&params)?;

    let paths: Vec<String> = if let Some(paths_arr) = body.get("paths").and_then(|v| v.as_array()) {
        paths_arr
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect()
    } else if let Some(path) = body.get("path").and_then(|v| v.as_str()) {
        vec![path.to_string()]
    } else {
        return Err(ApiError(oc_core::Error::BadRequest(
            "path parameter is required".to_string(),
        )));
    };

    if !paths.iter().any(|s| !s.trim().is_empty()) {
        return Err(ApiError(oc_core::Error::BadRequest(
            "path parameter is required".to_string(),
        )));
    }

    crate::git::commit::unstage_files(&directory, &paths).await?;
    Ok(Json(json!({ "success": true })))
}

/// POST /api/git/apply-hunk
pub async fn apply_hunk(
    Query(params): Query<HashMap<String, String>>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let directory = require_directory(&params)?;
    let file_path = body
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            ApiError(oc_core::Error::BadRequest(
                "path parameter is required".to_string(),
            ))
        })?;
    let patch = body
        .get("patch")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ApiError(oc_core::Error::BadRequest("patch is required".to_string())))?;
    let action = body
        .get("action")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if !matches!(action, "stage" | "unstage" | "discard") {
        return Err(ApiError(oc_core::Error::BadRequest(
            "action must be stage, unstage, or discard".to_string(),
        )));
    }

    crate::git::commit::apply_hunk(&directory, file_path, patch, action).await?;
    Ok(Json(json!({ "success": true })))
}

// ============================================================
// Commits & Log
// ============================================================

/// POST /api/git/commit
pub async fn commit(
    Query(params): Query<HashMap<String, String>>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let directory = require_directory(&params)?;
    let message = body
        .get("message")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            ApiError(oc_core::Error::BadRequest(
                "message is required".to_string(),
            ))
        })?;
    let add_all = body.get("addAll").and_then(|v| v.as_bool()).unwrap_or(false);
    let files: Option<Vec<String>> = body
        .get("files")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect());

    let result = crate::git::commit::commit(&directory, message, add_all, files.as_deref()).await?;
    Ok(Json(result))
}

/// POST /api/git/commit-summaries
pub async fn commit_summaries(
    Query(params): Query<HashMap<String, String>>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let directory = require_directory(&params)?;
    let shas: Vec<String> = body
        .get("shas")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default();

    let result = crate::git::log::get_commit_summaries(&directory, &shas).await?;
    Ok(Json(result))
}

/// GET /api/git/log
pub async fn log(
    Query(params): Query<HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    let directory = require_directory(&params)?;
    let max_count = opt_query(&params, "maxCount").and_then(|s| s.parse::<u32>().ok());
    let from = opt_query(&params, "from").map(|s| s.to_string());
    let to = opt_query(&params, "to").map(|s| s.to_string());
    let file = opt_query(&params, "file").map(|s| s.to_string());
    let all = opt_query(&params, "all") == Some("true");

    let result = crate::git::log::get_log(
        &directory,
        crate::git::log::LogOptions {
            max_count,
            from,
            to,
            file,
            all,
        },
    )
    .await?;
    Ok(Json(result))
}

/// GET /api/git/commit-files
pub async fn commit_files(
    Query(params): Query<HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    let directory = require_directory(&params)?;
    let hash = opt_query(&params, "hash")
        .ok_or_else(|| {
            ApiError(oc_core::Error::BadRequest(
                "hash parameter is required".to_string(),
            ))
        })?
        .to_string();

    let result = crate::git::log::get_commit_files(&directory, &hash).await?;
    Ok(Json(result))
}

/// GET /api/git/commit-file-diff
pub async fn commit_file_diff(
    Query(params): Query<HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    let directory = require_directory(&params)?;
    let hash = opt_query(&params, "hash")
        .ok_or_else(|| {
            ApiError(oc_core::Error::BadRequest(
                "hash parameter is required".to_string(),
            ))
        })?
        .to_string();
    validate_hash(&hash)?;

    let file_path = opt_query(&params, "path")
        .ok_or_else(|| {
            ApiError(oc_core::Error::BadRequest(
                "path parameter is required".to_string(),
            ))
        })?
        .to_string();
    let is_binary = opt_query(&params, "binary") == Some("true");

    let result = crate::git::diff::get_commit_file_diff(&directory, &hash, &file_path, is_binary).await?;
    Ok(Json(result))
}

// ============================================================
// Commit-target Operations
// ============================================================

/// POST /api/git/checkout-commit
pub async fn checkout_commit(
    Query(params): Query<HashMap<String, String>>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let directory = require_directory(&params)?;
    let hash = body
        .get("hash")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            ApiError(oc_core::Error::BadRequest("Invalid commit hash".to_string()))
        })?;
    validate_hash(hash)?;

    let result = crate::git::branch::checkout_commit(&directory, hash).await?;
    Ok(Json(result))
}

/// POST /api/git/cherry-pick
pub async fn cherry_pick(
    Query(params): Query<HashMap<String, String>>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let directory = require_directory(&params)?;
    let hash = body
        .get("hash")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            ApiError(oc_core::Error::BadRequest("Invalid commit hash".to_string()))
        })?;
    validate_hash(hash)?;

    let result = crate::git::commit::cherry_pick(&directory, hash).await?;
    Ok(Json(result))
}

/// POST /api/git/revert-commit
pub async fn revert_commit(
    Query(params): Query<HashMap<String, String>>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let directory = require_directory(&params)?;
    let hash = body
        .get("hash")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            ApiError(oc_core::Error::BadRequest("Invalid commit hash".to_string()))
        })?;
    validate_hash(hash)?;

    let result = crate::git::commit::revert_commit(&directory, hash).await?;
    Ok(Json(result))
}

/// POST /api/git/reset-to-commit
pub async fn reset_to_commit(
    Query(params): Query<HashMap<String, String>>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let directory = require_directory(&params)?;
    let hash = body
        .get("hash")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            ApiError(oc_core::Error::BadRequest("Invalid commit hash".to_string()))
        })?;
    validate_hash(hash)?;

    let mode = body.get("mode").and_then(|v| v.as_str()).unwrap_or("");
    if !matches!(mode, "soft" | "mixed" | "hard") {
        return Err(ApiError(oc_core::Error::BadRequest(
            "mode must be soft, mixed, or hard".to_string(),
        )));
    }

    let force = body.get("force").and_then(|v| v.as_bool()).unwrap_or(false);
    let result = crate::git::commit::reset_to_commit(&directory, hash, mode, force).await?;
    Ok(Json(result))
}

// ============================================================
// Branches
// ============================================================

/// GET /api/git/branches
pub async fn list_branches(
    Query(params): Query<HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    let directory = require_directory(&params)?;
    let result = crate::git::branch::get_branches(&directory).await?;
    Ok(Json(result))
}

/// POST /api/git/branches
pub async fn create_branch(
    Query(params): Query<HashMap<String, String>>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let directory = require_directory(&params)?;
    let name = body
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            ApiError(oc_core::Error::BadRequest("name is required".to_string()))
        })?;
    let start_point = body.get("startPoint").and_then(|v| v.as_str());

    let result = crate::git::branch::create_branch(&directory, name, start_point).await?;
    Ok(Json(result))
}

/// DELETE /api/git/branches
pub async fn delete_branch(
    Query(params): Query<HashMap<String, String>>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let directory = require_directory(&params)?;
    let branch = body
        .get("branch")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            ApiError(oc_core::Error::BadRequest(
                "branch is required".to_string(),
            ))
        })?;
    let force = body.get("force").and_then(|v| v.as_bool()).unwrap_or(false);

    let result = crate::git::branch::delete_branch(&directory, branch, force).await?;
    Ok(Json(result))
}

/// PUT /api/git/branches/rename
pub async fn rename_branch(
    Query(params): Query<HashMap<String, String>>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let directory = require_directory(&params)?;
    let old_name = body
        .get("oldName")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            ApiError(oc_core::Error::BadRequest(
                "oldName is required".to_string(),
            ))
        })?;
    let new_name = body
        .get("newName")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            ApiError(oc_core::Error::BadRequest(
                "newName is required".to_string(),
            ))
        })?;

    let result = crate::git::branch::rename_branch(&directory, old_name, new_name).await?;
    Ok(Json(result))
}

/// DELETE /api/git/remote-branches
pub async fn delete_remote_branch(
    Query(params): Query<HashMap<String, String>>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let directory = require_directory(&params)?;
    let branch = body
        .get("branch")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            ApiError(oc_core::Error::BadRequest(
                "branch is required".to_string(),
            ))
        })?;
    let remote = body.get("remote").and_then(|v| v.as_str());

    let result = crate::git::remote::delete_remote_branch(&directory, branch, remote).await?;
    Ok(Json(result))
}

/// POST /api/git/checkout
pub async fn checkout(
    Query(params): Query<HashMap<String, String>>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let directory = require_directory(&params)?;
    let branch = body
        .get("branch")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            ApiError(oc_core::Error::BadRequest(
                "branch is required".to_string(),
            ))
        })?;

    let result = crate::git::branch::checkout_branch(&directory, branch).await?;
    Ok(Json(result))
}

// ============================================================
// Remotes / Sync
// ============================================================

/// POST /api/git/pull
pub async fn pull(
    Query(params): Query<HashMap<String, String>>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let directory = require_directory(&params)?;
    let result = crate::git::remote::pull(&directory, &body).await?;
    Ok(Json(result))
}

/// POST /api/git/push
pub async fn push(
    Query(params): Query<HashMap<String, String>>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let directory = require_directory(&params)?;
    let result = crate::git::remote::push(&directory, &body).await?;
    Ok(Json(result))
}

/// POST /api/git/fetch
pub async fn fetch(
    Query(params): Query<HashMap<String, String>>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let directory = require_directory(&params)?;
    let result = crate::git::remote::fetch(&directory, &body).await?;
    Ok(Json(result))
}

/// GET /api/git/remotes
pub async fn list_remotes(
    Query(params): Query<HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    let directory = require_directory(&params)?;
    let result = crate::git::remote::get_remotes(&directory).await?;
    Ok(Json(result))
}

/// DELETE /api/git/remotes
pub async fn remove_remote(
    Query(params): Query<HashMap<String, String>>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let directory = require_directory(&params)?;
    let remote = body
        .get("remote")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if remote.is_empty() {
        return Err(ApiError(oc_core::Error::BadRequest(
            "remote is required".to_string(),
        )));
    }

    let result = crate::git::remote::remove_remote(&directory, &remote).await?;
    Ok(Json(result))
}

// ============================================================
// Merge / Rebase / Conflicts
// ============================================================

/// POST /api/git/merge
pub async fn merge(
    Query(params): Query<HashMap<String, String>>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let directory = require_directory(&params)?;
    let branch = body
        .get("branch")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            ApiError(oc_core::Error::BadRequest(
                "branch is required".to_string(),
            ))
        })?;

    let result = crate::git::merge_rebase::merge(&directory, branch).await?;
    Ok(Json(result))
}

/// POST /api/git/merge/abort
pub async fn abort_merge(
    Query(params): Query<HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    let directory = require_directory(&params)?;
    let result = crate::git::merge_rebase::abort_merge(&directory).await?;
    Ok(Json(result))
}

/// POST /api/git/merge/continue
pub async fn continue_merge(
    Query(params): Query<HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    let directory = require_directory(&params)?;
    let result = crate::git::merge_rebase::continue_merge(&directory).await?;
    Ok(Json(result))
}

/// POST /api/git/rebase
pub async fn rebase(
    Query(params): Query<HashMap<String, String>>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let directory = require_directory(&params)?;
    let onto = body
        .get("onto")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            ApiError(oc_core::Error::BadRequest("onto is required".to_string()))
        })?;

    let result = crate::git::merge_rebase::rebase(&directory, onto).await?;
    Ok(Json(result))
}

/// POST /api/git/rebase/abort
pub async fn abort_rebase(
    Query(params): Query<HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    let directory = require_directory(&params)?;
    let result = crate::git::merge_rebase::abort_rebase(&directory).await?;
    Ok(Json(result))
}

/// POST /api/git/rebase/continue
pub async fn continue_rebase(
    Query(params): Query<HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    let directory = require_directory(&params)?;
    let result = crate::git::merge_rebase::continue_rebase(&directory).await?;
    Ok(Json(result))
}

/// GET /api/git/conflict-details
pub async fn conflict_details(
    Query(params): Query<HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    let directory = require_directory(&params)?;
    let result = crate::git::merge_rebase::get_conflict_details(&directory).await?;
    Ok(Json(result))
}

// ============================================================
// Stashes
// ============================================================

/// GET /api/git/stashes
pub async fn list_stashes(
    Query(params): Query<HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    let directory = require_directory(&params)?;
    let result = crate::git::stash::list_stashes(&directory).await?;
    Ok(Json(result))
}

/// POST /api/git/stashes/file-counts
pub async fn stash_file_counts(
    Query(params): Query<HashMap<String, String>>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let directory = require_directory(&params)?;
    let refs: Vec<String> = body
        .get("refs")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default();

    let result = crate::git::stash::count_stash_files(&directory, &refs).await?;
    Ok(Json(result))
}

/// POST /api/git/stash
pub async fn stash_push(
    Query(params): Query<HashMap<String, String>>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let directory = require_directory(&params)?;
    let message = body.get("message").and_then(|v| v.as_str());

    let result = crate::git::stash::stash_push(&directory, message).await?;
    Ok(Json(result))
}

/// POST /api/git/stash/apply
pub async fn stash_apply(
    Query(params): Query<HashMap<String, String>>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let directory = require_directory(&params)?;
    let r#ref = body
        .get("ref")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            ApiError(oc_core::Error::BadRequest("ref is required".to_string()))
        })?;

    let result = crate::git::stash::stash_apply(&directory, r#ref).await?;
    Ok(Json(result))
}

/// POST /api/git/stash/pop
pub async fn stash_pop(
    Query(params): Query<HashMap<String, String>>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let directory = require_directory(&params)?;
    let r#ref = body
        .get("ref")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            ApiError(oc_core::Error::BadRequest("ref is required".to_string()))
        })?;

    let result = crate::git::stash::stash_pop(&directory, r#ref).await?;
    Ok(Json(result))
}

/// POST /api/git/stash/drop
pub async fn stash_drop(
    Query(params): Query<HashMap<String, String>>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let directory = require_directory(&params)?;
    let r#ref = body
        .get("ref")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            ApiError(oc_core::Error::BadRequest("ref is required".to_string()))
        })?;

    let result = crate::git::stash::stash_drop(&directory, r#ref).await?;
    Ok(Json(result))
}

// ============================================================
// Worktrees
// ============================================================

/// GET /api/git/worktrees — 失败返回 200 [] (不是 500), 带 warning header
pub async fn list_worktrees(
    Query(params): Query<HashMap<String, String>>,
) -> Result<Response, StatusCode> {
    let directory = match require_directory(&params) {
        Ok(d) => d,
        Err(_) => return Err(StatusCode::BAD_REQUEST),
    };

    // 不返回错误, 而是返回空数组
    let worktrees = crate::git::worktree::get_worktrees(&directory).await;

    let mut response = Json(json!(worktrees)).into_response();
    if worktrees.is_empty() {
        response.headers_mut().insert(
            HeaderName::from_static("x-openchamber-warning"),
            HeaderValue::from_static("git worktrees unavailable"),
        );
    }
    Ok(response)
}

/// POST /api/git/worktrees/validate
pub async fn validate_worktree(
    Query(params): Query<HashMap<String, String>>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let directory = require_directory(&params)?;
    let result = crate::git::worktree::validate_worktree_create(&directory, &body).await?;
    Ok(Json(result))
}

/// POST /api/git/worktrees/preview
pub async fn preview_worktree(
    Query(params): Query<HashMap<String, String>>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let directory = require_directory(&params)?;
    let result = crate::git::worktree::preview_worktree_create(&directory, &body).await?;
    Ok(Json(result))
}

/// POST /api/git/worktrees
pub async fn create_worktree(
    Query(params): Query<HashMap<String, String>>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let directory = require_directory(&params)?;
    let result = crate::git::worktree::create_worktree(&directory, &body).await?;
    Ok(Json(result))
}

/// GET /api/git/worktrees/bootstrap-status
pub async fn worktree_bootstrap_status(
    State(_state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    let _directory = require_directory(&params)?;
    // stub: worktree bootstrap state 返回 ready
    Ok(Json(json!({ "status": "ready" })))
}

/// DELETE /api/git/worktrees
pub async fn remove_worktree(
    Query(params): Query<HashMap<String, String>>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let directory = require_directory(&params)?;
    let target_dir = body
        .get("directory")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if target_dir.is_empty() {
        return Err(ApiError(oc_core::Error::BadRequest(
            "worktree directory is required".to_string(),
        )));
    }
    let delete_branch = body.get("deleteLocalBranch").and_then(|v| v.as_bool()).unwrap_or(false);

    let result = crate::git::worktree::remove_worktree(&directory, target_dir, delete_branch).await?;
    Ok(Json(result))
}

/// GET /api/git/worktree-type
pub async fn worktree_type(
    Query(params): Query<HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    let directory = require_directory(&params)?;
    let linked = crate::git::worktree::is_linked_worktree(&directory).await?;
    Ok(Json(json!({ "linked": linked })))
}

/// POST /api/git/validate-directory
pub async fn validate_directory(
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let directory = body
        .get("directory")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            ApiError(oc_core::Error::BadRequest("directory is required".to_string()))
        })?;
    let worktree_root = body
        .get("worktreeRoot")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            ApiError(oc_core::Error::BadRequest(
                "worktreeRoot is required".to_string(),
            ))
        })?;

    let result = crate::git::worktree::validate_worktree_directory(directory, worktree_root).await?;
    Ok(Json(result))
}

/// POST /api/git/canonicalize-worktree-state
pub async fn canonicalize_worktree_state(
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let directory = body
        .get("directory")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            ApiError(oc_core::Error::BadRequest("directory is required".to_string()))
        })?;

    let result = crate::git::worktree::canonicalize_worktree_state(directory).await?;
    Ok(Json(result))
}

// ============================================================
// Integrate Workflow
// ============================================================

/// POST /api/git/integrate/plan
pub async fn integrate_plan(Json(body): Json<Value>) -> ApiResult<Json<Value>> {
    let result = crate::git::integrate::compute_integrate_plan(&body).await?;
    Ok(Json(result))
}

/// POST /api/git/integrate/conflict-details
pub async fn integrate_conflict_details(Json(body): Json<Value>) -> ApiResult<Json<Value>> {
    let temp_path = body
        .get("tempWorktreePath")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let result = crate::git::integrate::get_integrate_conflict_details(temp_path).await?;
    Ok(Json(result))
}

/// POST /api/git/integrate/cherry-pick-status
pub async fn integrate_cherry_pick_status(Json(body): Json<Value>) -> ApiResult<Json<Value>> {
    let temp_path = body
        .get("tempWorktreePath")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let in_progress = crate::git::integrate::is_cherry_pick_in_progress(temp_path).await?;
    Ok(Json(json!({ "inProgress": in_progress })))
}

/// POST /api/git/integrate/run
pub async fn integrate_run(Json(body): Json<Value>) -> ApiResult<Json<Value>> {
    let plan = body.get("plan").cloned().unwrap_or(json!({}));
    let result = crate::git::integrate::integrate_worktree_commits(&plan).await?;
    Ok(Json(result))
}

/// POST /api/git/integrate/abort
pub async fn integrate_abort(Json(body): Json<Value>) -> ApiResult<Json<Value>> {
    let state = body.get("state").cloned().unwrap_or(json!({}));
    let result = crate::git::integrate::abort_integrate(&state).await?;
    Ok(Json(result))
}

/// POST /api/git/integrate/continue
pub async fn integrate_continue(Json(body): Json<Value>) -> ApiResult<Json<Value>> {
    let state = body.get("state").cloned().unwrap_or(json!({}));
    let result = crate::git::integrate::continue_integrate(&state).await?;
    Ok(Json(result))
}
