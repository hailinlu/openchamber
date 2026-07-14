//! `/api/fs/*` 路由 handler。
//!
//! 对应 `packages/web/server/lib/fs/routes.js` 的 15 个路由。
//!
//! 所有 handler 返回 `ApiResult<T>` (= `Result<T, ApiError>`)。
//! 路由注册为具体路由, 优先于 `/api/*` catch-all proxy。

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

use super::operations;
use super::serve as serve_mod;
use super::workspace;

// ---------------------------------------------------------------------------
// 请求体类型
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct GrantBody {
    pub path: String,
    #[serde(default)]
    pub scopes: Option<Vec<String>>,
}

#[derive(Deserialize)]
pub struct MkdirBody {
    pub path: String,
    #[serde(default, rename = "allowOutsideWorkspace")]
    pub allow_outside: bool,
}

#[derive(Deserialize)]
pub struct WriteBody {
    pub path: String,
    pub content: String,
}

#[derive(Deserialize)]
pub struct DeleteBody {
    pub path: String,
}

#[derive(Deserialize)]
pub struct RenameBody {
    #[serde(rename = "oldPath")]
    pub old_path: String,
    #[serde(rename = "newPath")]
    pub new_path: String,
}

#[derive(Deserialize)]
pub struct RevealBody {
    pub path: String,
}

#[derive(Deserialize)]
pub struct ExecBody {
    pub commands: Vec<String>,
    pub cwd: String,
    #[serde(default)]
    pub background: bool,
}

#[derive(Deserialize)]
pub struct CloneBody {
    #[serde(rename = "remoteUrl")]
    pub remote_url: String,
    #[serde(rename = "destinationPath")]
    pub destination_path: String,
    #[serde(default, rename = "gitIdentityId")]
    #[allow(dead_code)]
    pub git_identity_id: Option<String>,
}

// Query 参数

#[derive(Deserialize)]
pub struct PathQuery {
    pub path: Option<String>,
    pub optional: Option<String>,
    #[serde(default, rename = "allowOutsideWorkspace")]
    pub allow_outside: Option<String>,
    #[serde(rename = "outsideFileGrant")]
    pub outside_file_grant: Option<String>,
    pub download: Option<String>,
}

#[derive(Deserialize)]
pub struct ListQuery {
    pub path: Option<String>,
}

// ---------------------------------------------------------------------------
// Grant handler
// ---------------------------------------------------------------------------

/// `POST /api/fs/grant` — mint outside-workspace grant。
pub async fn grant(
    State(state): State<Arc<AppState>>,
    Json(body): Json<GrantBody>,
) -> ApiResult<Json<Value>> {
    let scopes = body.scopes.unwrap_or_else(|| {
        vec!["stat".to_string(), "read".to_string(), "raw".to_string()]
    });
    let minted = state.grant_store.mint(&body.path, scopes).await?;
    Ok(Json(json!(minted)))
}

// ---------------------------------------------------------------------------
// Simple handlers
// ---------------------------------------------------------------------------

/// `GET /api/fs/home`
pub async fn home() -> ApiResult<Json<Value>> {
    Ok(Json(operations::home_dir()))
}

/// `POST /api/fs/mkdir`
pub async fn mkdir(
    State(state): State<Arc<AppState>>,
    Json(body): Json<MkdirBody>,
) -> ApiResult<Json<Value>> {
    // Node 版本: allowOutsideWorkspace=true 始终返回 403 (需要 grant)
    if body.allow_outside {
        return Err(ApiError(oc_core::Error::BadRequest(
            "Operations outside the workspace require a grant".into(),
        )));
    }

    let base_dir = resolve_base_dir(&state).await;
    let user_config_root = user_config_root(&state);
    let resolved = workspace::resolve_workspace_path(&body.path, &base_dir, user_config_root.as_deref())?;

    Ok(Json(operations::mkdir(&resolved).await?))
}

/// `POST /api/fs/write`
pub async fn write(
    State(state): State<Arc<AppState>>,
    Json(body): Json<WriteBody>,
) -> ApiResult<Json<Value>> {
    let base_dir = resolve_base_dir(&state).await;
    let user_config_root = user_config_root(&state);
    let resolved = workspace::resolve_workspace_path(&body.path, &base_dir, user_config_root.as_deref())?;

    Ok(Json(operations::write(&resolved, &body.content).await?))
}

/// `POST /api/fs/delete`
pub async fn delete(
    State(state): State<Arc<AppState>>,
    Json(body): Json<DeleteBody>,
) -> ApiResult<Json<Value>> {
    let base_dir = resolve_base_dir(&state).await;
    let user_config_root = user_config_root(&state);
    let resolved = workspace::resolve_workspace_path(&body.path, &base_dir, user_config_root.as_deref())?;

    Ok(Json(operations::delete(&resolved).await?))
}

/// `POST /api/fs/rename`
pub async fn rename(
    State(state): State<Arc<AppState>>,
    Json(body): Json<RenameBody>,
) -> ApiResult<Json<Value>> {
    let base_dir = resolve_base_dir(&state).await;
    let user_config_root = user_config_root(&state);

    let old_resolved =
        workspace::resolve_workspace_path(&body.old_path, &base_dir, user_config_root.as_deref())?;
    let new_resolved =
        workspace::resolve_workspace_path(&body.new_path, &base_dir, user_config_root.as_deref())?;

    Ok(Json(operations::rename(&old_resolved, &new_resolved).await?))
}

/// `POST /api/fs/reveal`
pub async fn reveal(
    State(state): State<Arc<AppState>>,
    Json(body): Json<RevealBody>,
) -> ApiResult<Json<Value>> {
    let base_dir = resolve_base_dir(&state).await;
    let user_config_root = user_config_root(&state);
    let resolved = workspace::resolve_workspace_path(&body.path, &base_dir, user_config_root.as_deref())?;

    Ok(Json(operations::reveal(&resolved).await?))
}

// ---------------------------------------------------------------------------
// Read-path handlers (stat / read / raw) — 支持 outside workspace grant
// ---------------------------------------------------------------------------

/// `GET /api/fs/stat`
pub async fn stat(
    State(state): State<Arc<AppState>>,
    Query(query): Query<PathQuery>,
) -> ApiResult<Json<Value>> {
    let path = query
        .path
        .as_deref()
        .ok_or_else(|| ApiError(oc_core::Error::BadRequest("Path is required".into())))?;

    let resolved = resolve_read_path(&state, path, "stat", &query).await?;

    // optional 模式: 文件不存在时返回 { exists: false } 而不是 404
    let is_optional = query.optional.as_deref() == Some("true");

    match operations::stat(&resolved).await {
        Ok(value) => Ok(Json(value)),
        Err(oc_core::Error::NotFound(_)) if is_optional => Ok(Json(json!({
            "path": resolved.to_string_lossy(),
            "exists": false,
        }))),
        Err(e) => Err(ApiError(e)),
    }
}

/// `GET /api/fs/read`
pub async fn read(
    State(state): State<Arc<AppState>>,
    Query(query): Query<PathQuery>,
) -> ApiResult<Response> {
    let path = query
        .path
        .as_deref()
        .ok_or_else(|| ApiError(oc_core::Error::BadRequest("Path is required".into())))?;

    let resolved = resolve_read_path(&state, path, "read", &query).await?;

    let is_optional = query.optional.as_deref() == Some("true");

    match operations::read(&resolved).await {
        Ok(content) => Ok((
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            content,
        )
            .into_response()),
        Err(oc_core::Error::NotFound(_)) if is_optional => Ok((
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            String::new(),
        )
            .into_response()),
        Err(e) => Err(ApiError(e)),
    }
}

/// `GET /api/fs/raw`
pub async fn raw(
    State(state): State<Arc<AppState>>,
    Query(query): Query<PathQuery>,
) -> ApiResult<Response> {
    let path = query
        .path
        .as_deref()
        .ok_or_else(|| ApiError(oc_core::Error::BadRequest("Path is required".into())))?;

    let resolved = resolve_read_path(&state, path, "raw", &query).await?;
    let download = query.download.as_deref() == Some("true");

    let data = serve_mod::serve_raw(&resolved, download).await?;

    let mut response = Response::new(Body::from(data.content));
    *response.status_mut() = StatusCode::OK;
    let headers = response.headers_mut();
    set_header(headers, axum::http::header::CONTENT_TYPE, &data.content_type);
    set_header(headers, axum::http::header::CACHE_CONTROL, "no-store");

    if let Some(ref cd) = data.content_disposition {
        set_header(headers, axum::http::header::CONTENT_DISPOSITION, cd);
    }

    // grant-served → Referrer-Policy: no-referrer
    if query.outside_file_grant.is_some() {
        set_header(headers, axum::http::header::REFERRER_POLICY, "no-referrer");
    }

    Ok(response)
}

/// `GET /api/fs/serve/<rest>` — serve 模式 (拒绝 allowOutsideWorkspace)。
pub async fn serve(
    AxumPath(rest): AxumPath<String>,
) -> ApiResult<Response> {
    // serve 路由拒绝 allowOutsideWorkspace (Node 版本始终 403)
    // path 从 `/<rest>` 解析 (绝对路径)
    let resolved = PathBuf::from(format!("/{}", rest));

    let data = serve_mod::serve_file(&resolved).await?;

    let mut response = Response::new(Body::from(data.content));
    *response.status_mut() = StatusCode::OK;
    let headers = response.headers_mut();
    set_header(headers, axum::http::header::CONTENT_TYPE, &data.content_type);
    set_header(headers, axum::http::header::CACHE_CONTROL, "no-store");
    set_header(
        headers,
        axum::http::header::HeaderName::from_static("x-content-type-options"),
        "nosniff",
    );

    Ok(response)
}

// ---------------------------------------------------------------------------
// List handler
// ---------------------------------------------------------------------------

/// `GET /api/fs/list`
pub async fn list(
    State(_state): State<Arc<AppState>>,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<Value>> {
    let path = query.path.as_deref().unwrap_or("~");
    let expanded = crate::project_dir::normalize_directory_path(path);

    // 如果展开后仍然为空, 使用 home
    let target = if expanded.is_empty() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
        PathBuf::from(home)
    } else {
        PathBuf::from(expanded)
    };

    Ok(Json(operations::list(&target).await?))
}

// ---------------------------------------------------------------------------
// Exec handlers
// ---------------------------------------------------------------------------

/// `POST /api/fs/exec`
pub async fn exec(
    State(state): State<Arc<AppState>>,
    Json(body): Json<ExecBody>,
) -> ApiResult<Json<Value>> {
    // background=true 始终拒绝 (Node 版本也是)
    if body.background {
        return Err(ApiError(oc_core::Error::BadRequest(
            "Background execution is not supported".into(),
        )));
    }

    if body.commands.is_empty() {
        return Err(ApiError(oc_core::Error::BadRequest("Commands are required".into())));
    }

    if body.cwd.is_empty() {
        return Err(ApiError(oc_core::Error::BadRequest(
            "Working directory is required".into(),
        )));
    }

    // 验证 cwd 在工作区内
    let base_dir = resolve_base_dir(&state).await;
    let user_config_root = user_config_root(&state);
    let resolved_cwd =
        workspace::resolve_workspace_path(&body.cwd, &base_dir, user_config_root.as_deref())?;

    let timeout = exec_timeout_secs();
    let result = state
        .exec_job_store
        .execute(body.commands, &resolved_cwd, Some(timeout))
        .await?;

    Ok(Json(result))
}

/// `GET /api/fs/exec/:jobId`
pub async fn exec_status(
    State(state): State<Arc<AppState>>,
    AxumPath(job_id): AxumPath<String>,
) -> ApiResult<Json<Value>> {
    match state.exec_job_store.get(&job_id) {
        Some(job) => Ok(Json(json!(job))),
        None => Err(ApiError(oc_core::Error::NotFound(format!(
            "Exec job not found: {}",
            job_id
        )))),
    }
}

// ---------------------------------------------------------------------------
// Clone handler
// ---------------------------------------------------------------------------

/// `POST /api/fs/clone` — git clone
pub async fn clone(
    Json(body): Json<CloneBody>,
) -> ApiResult<Json<Value>> {
    if body.remote_url.is_empty() {
        return Err(ApiError(oc_core::Error::BadRequest("Remote URL is required".into())));
    }
    if body.destination_path.is_empty() {
        return Err(ApiError(oc_core::Error::BadRequest(
            "Destination path is required".into(),
        )));
    }

    // 检查目标是否已存在
    if tokio::fs::metadata(&body.destination_path).await.is_ok() {
        return Err(ApiError(oc_core::Error::BadRequest(format!(
            "Destination already exists: {}",
            body.destination_path
        ))));
    }

    // 执行 git clone
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("clone")
        .arg(&body.remote_url)
        .arg(&body.destination_path);

    // Windows 隐藏窗口
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let output = cmd
        .output()
        .await
        .map_err(|e| ApiError(oc_core::Error::Internal(format!("Failed to run git clone: {}", e))))?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if !output.status.success() {
        return Err(ApiError(oc_core::Error::BadRequest(format!(
            "git clone failed: {}",
            if stderr.is_empty() { &stdout } else { &stderr }
        ))));
    }

    Ok(Json(json!({
        "success": true,
        "path": body.destination_path,
        "output": stdout,
    })))
}

// ---------------------------------------------------------------------------
// 辅助函数
// ---------------------------------------------------------------------------

/// 解析工作区基础目录 (从请求上下文)。
async fn resolve_base_dir(state: &AppState) -> PathBuf {
    // 读 settings.json 获取项目目录
    match crate::project_dir::resolve_project_directory(
        &HeaderMap::new(),
        None,
        &state.settings_path,
    )
    .await
    {
        Some(dir) => dir,
        None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
    }
}

/// 用户配置目录 (~/.config/openchamber)。
fn user_config_root(state: &AppState) -> Option<PathBuf> {
    state.settings_path.parent().map(|p| p.to_path_buf())
}

/// 解析 read-path (支持 outside workspace grant)。
async fn resolve_read_path(
    state: &AppState,
    path: &str,
    scope: &str,
    query: &PathQuery,
) -> ApiResult<PathBuf> {
    let allow_outside = query.allow_outside.as_deref() == Some("true");

    if allow_outside {
        // grant 模式
        let grant_token = query.outside_file_grant.as_deref().ok_or_else(|| {
            ApiError(oc_core::Error::BadRequest("Outside file grant is required".into()))
        })?;

        let resolved = state.grant_store.resolve(grant_token, path, scope).await?;
        Ok(resolved.resolved)
    } else {
        // 正常工作区模式
        let base_dir = resolve_base_dir(state).await;
        let user_config_root = user_config_root(state);
        workspace::resolve_workspace_path(path, &base_dir, user_config_root.as_deref())
            .map_err(ApiError::from)
    }
}

/// 命令执行超时 (环境变量覆盖)。
fn exec_timeout_secs() -> u64 {
    std::env::var("OPENCHAMBER_FS_EXEC_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&v| v > 0)
        .map(|ms| ms / 1000)
        .unwrap_or(super::DEFAULT_EXEC_TIMEOUT_SECS)
}

/// 安全设置 HTTP 头 (解析失败时跳过)。
fn set_header(
    headers: &mut axum::http::HeaderMap,
    name: axum::http::HeaderName,
    value: &str,
) {
    if let Ok(hv) = axum::http::HeaderValue::from_str(value) {
        headers.insert(name, hv);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_timeout_default() {
        std::env::remove_var("OPENCHAMBER_FS_EXEC_TIMEOUT_MS");
        assert_eq!(exec_timeout_secs(), super::super::DEFAULT_EXEC_TIMEOUT_SECS);
    }

    #[test]
    fn exec_timeout_env_override() {
        std::env::set_var("OPENCHAMBER_FS_EXEC_TIMEOUT_MS", "120000");
        assert_eq!(exec_timeout_secs(), 120);
        std::env::remove_var("OPENCHAMBER_FS_EXEC_TIMEOUT_MS");
    }
}
