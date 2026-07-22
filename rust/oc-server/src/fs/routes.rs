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
    /// 工作区目录候选 — 对应 Node 版本的 `req.query.directory`。
    /// 与 `x-opencode-directory` 头并列, 由 `resolve_project_directory`
    /// 优先取第一个存在的目录。
    #[serde(default)]
    pub directory: Option<String>,
}

#[derive(Deserialize)]
pub struct ListQuery {
    pub path: Option<String>,
    /// 同 `PathQuery::directory` — list 路由也接受工作区目录候选。
    #[serde(default)]
    pub directory: Option<String>,
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
    headers: HeaderMap,
    Json(body): Json<MkdirBody>,
) -> ApiResult<Json<Value>> {
    // Node 版本: allowOutsideWorkspace=true 始终返回 403 (需要 grant)
    if body.allow_outside {
        return Err(ApiError(oc_core::Error::BadRequest(
            "Operations outside the workspace require a grant".into(),
        )));
    }

    let base_dir = resolve_base_dir(&state, &headers, None).await;
    let user_config_root = user_config_root(&state);
    let resolved = workspace::resolve_workspace_path(&body.path, &base_dir, user_config_root.as_deref())?;

    Ok(Json(operations::mkdir(&resolved).await?))
}

/// `POST /api/fs/write`
pub async fn write(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<WriteBody>,
) -> ApiResult<Json<Value>> {
    let base_dir = resolve_base_dir(&state, &headers, None).await;
    let user_config_root = user_config_root(&state);
    let resolved = workspace::resolve_workspace_path(&body.path, &base_dir, user_config_root.as_deref())?;

    Ok(Json(operations::write(&resolved, &body.content).await?))
}

/// `POST /api/fs/delete`
pub async fn delete(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<DeleteBody>,
) -> ApiResult<Json<Value>> {
    let base_dir = resolve_base_dir(&state, &headers, None).await;
    let user_config_root = user_config_root(&state);
    let resolved = workspace::resolve_workspace_path(&body.path, &base_dir, user_config_root.as_deref())?;

    Ok(Json(operations::delete(&resolved).await?))
}

/// `POST /api/fs/rename`
pub async fn rename(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<RenameBody>,
) -> ApiResult<Json<Value>> {
    let base_dir = resolve_base_dir(&state, &headers, None).await;
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
    headers: HeaderMap,
    Json(body): Json<RevealBody>,
) -> ApiResult<Json<Value>> {
    let base_dir = resolve_base_dir(&state, &headers, None).await;
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
    headers: HeaderMap,
    Query(query): Query<PathQuery>,
) -> ApiResult<Json<Value>> {
    let path = query
        .path
        .as_deref()
        .ok_or_else(|| ApiError(oc_core::Error::BadRequest("Path is required".into())))?;

    let resolved = resolve_read_path(&state, &headers, path, "stat", &query).await?;

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
    headers: HeaderMap,
    Query(query): Query<PathQuery>,
) -> ApiResult<Response> {
    let path = query
        .path
        .as_deref()
        .ok_or_else(|| ApiError(oc_core::Error::BadRequest("Path is required".into())))?;

    let resolved = resolve_read_path(&state, &headers, path, "read", &query).await?;

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
    headers: HeaderMap,
    Query(query): Query<PathQuery>,
) -> ApiResult<Response> {
    let path = query
        .path
        .as_deref()
        .ok_or_else(|| ApiError(oc_core::Error::BadRequest("Path is required".into())))?;

    let resolved = resolve_read_path(&state, &headers, path, "raw", &query).await?;
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
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(rest): AxumPath<String>,
) -> ApiResult<Response> {
    // serve 路由拒绝 allowOutsideWorkspace (Node 版本始终 403)
    // path 从 `/<rest>` 解析 (绝对路径)
    let raw_path = format!("/{}", rest);
    // 补: 工作区边界校验 — 防止读取工作区外任意文件
    let base_dir = resolve_base_dir(&state, &headers, None).await;
    let user_root = user_config_root(&state);
    let resolved = workspace::resolve_workspace_path(
        &raw_path,
        &base_dir,
        user_root.as_deref(),
    )?;

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
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<Value>> {
    let path = query.path.as_deref().unwrap_or("~");
    let expanded = crate::project_dir::normalize_directory_path(path);

    // 如果展开后仍然为空, 使用 home
    let target_str = if expanded.is_empty() {
        std::env::var("HOME").unwrap_or_else(|_| "/".to_string())
    } else {
        expanded
    };

    // List 端点放宽边界 — 允许 home directory 内的任意子目录。
    // 理由: 列出 ~/Projects 用于"添加项目"对话框选目录是核心用例;
    // 仅暴露目录名不构成敏感读取 (read/write/delete 仍走严格 workspace check)。
    // resolve_workspace_path 接受 base_dir 内或 user_config_root 内的路径。
    let base_dir = resolve_base_dir(&state, &headers, query.directory.as_deref()).await;
    let user_root = user_config_root(&state);
    let home_root = std::env::var("HOME").ok().filter(|s| !s.is_empty()).map(PathBuf::from);
    let target = workspace::resolve_list_path(
        &target_str,
        &base_dir,
        user_root.as_deref(),
        home_root.as_deref(),
    )?;

    Ok(Json(operations::list(&target).await?))
}

// ---------------------------------------------------------------------------
// Exec handlers
// ---------------------------------------------------------------------------

/// `POST /api/fs/exec`
pub async fn exec(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
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
    let base_dir = resolve_base_dir(&state, &headers, None).await;
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
async fn resolve_base_dir_from_request(
    headers: &HeaderMap,
    query_directory: Option<&str>,
    settings_path: &std::path::Path,
) -> PathBuf {
    crate::project_dir::resolve_project_directory(headers, query_directory, settings_path)
        .await
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")))
}

async fn resolve_base_dir(state: &AppState, headers: &HeaderMap, query_directory: Option<&str>) -> PathBuf {
    resolve_base_dir_from_request(headers, query_directory, &state.settings_path).await
}

/// 用户配置目录 (~/.config/gridforge)。
fn user_config_root(state: &AppState) -> Option<PathBuf> {
    state.settings_path.parent().map(|p| p.to_path_buf())
}

/// 解析 read-path (支持 outside workspace grant)。
async fn resolve_read_path(
    state: &AppState,
    headers: &HeaderMap,
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
        let base_dir = resolve_base_dir(state, headers, query.directory.as_deref()).await;
        let user_config_root = user_config_root(state);
        workspace::resolve_workspace_path(path, &base_dir, user_config_root.as_deref())
            .map_err(ApiError::from)
    }
}

/// 命令执行超时 (环境变量覆盖)。
fn exec_timeout_secs() -> u64 {
    std::env::var("GRIDFORGE_FS_EXEC_TIMEOUT_MS")
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
        std::env::remove_var("GRIDFORGE_FS_EXEC_TIMEOUT_MS");
        assert_eq!(exec_timeout_secs(), super::super::DEFAULT_EXEC_TIMEOUT_SECS);
    }

    #[test]
    fn exec_timeout_env_override() {
        std::env::set_var("GRIDFORGE_FS_EXEC_TIMEOUT_MS", "120000");
        assert_eq!(exec_timeout_secs(), 120);
        std::env::remove_var("GRIDFORGE_FS_EXEC_TIMEOUT_MS");
    }

    /// axum-level 回归: 真实 `HeaderMap` 经 axum 传入 handler,
    /// `/api/fs/list` 必须按 `x-opencode-directory` 解析工作区。
    ///
    /// 这正是原始复现 `Path is outside of active workspace` 400 的失败点:
    /// 旧实现 `resolve_base_dir` 使用空 `HeaderMap::new()`,
    /// 退回到 settings → `cwd`, 而非请求上下文。
    ///
    /// `GRIDFORGE_DATA_DIR` 在构造 AppState 前指向临时目录,
    /// 避免触碰真实 `~/.config/gridforge/settings.json`。
    /// 子模块 `axum_tests` 内的所有测试共享一个 `Mutex` 串行化,
    /// 防止并行用例互相污染 env var。
    mod axum_tests {
        use std::path::PathBuf;
        use std::sync::{Mutex, OnceLock};

        use axum::body::Body;
        use axum::extract::Request;
        use axum::http::{header, StatusCode};
        use axum::routing::get;
        use axum::Router;
        use serde_json::Value;
        use tower::ServiceExt;

        use crate::state::AppState;

        /// 全模块共享串行锁 — 任何 `GRIDFORGE_DATA_DIR` 设置在测试结束前必须释放。
        static ENV_GUARD: OnceLock<Mutex<()>> = OnceLock::new();

        fn env_guard() -> std::sync::MutexGuard<'static, ()> {
            ENV_GUARD
                .get_or_init(|| Mutex::new(()))
                .lock()
                .unwrap_or_else(|e| e.into_inner())
        }

        /// 创建带临时 GRIDFORGE_DATA_DIR 的 AppState, 同时返回 data_dir 用于清理。
        fn build_test_state() -> (std::sync::Arc<AppState>, PathBuf) {
            let mut tmp = std::env::temp_dir();
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            tmp.push(format!("oc-server-fs-routes-{}-{}", std::process::id(), nanos));
            std::fs::create_dir_all(&tmp).expect("create temp data dir");
            std::env::set_var("GRIDFORGE_DATA_DIR", &tmp);
            let state = AppState::new_for_tests();
            (std::sync::Arc::new(state), tmp)
        }

        fn cleanup(dir: &PathBuf) {
            std::env::remove_var("GRIDFORGE_DATA_DIR");
            let _ = std::fs::remove_dir_all(dir);
        }

        fn make_workspace() -> PathBuf {
            let mut tmp = std::env::temp_dir();
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            tmp.push(format!(
                "oc-server-fs-workspace-{}-{}",
                std::process::id(),
                nanos
            ));
            std::fs::create_dir_all(&tmp).expect("create temp workspace");
            tmp
        }

        fn router(state: std::sync::Arc<AppState>) -> Router {
            Router::new()
                .route("/api/fs/list", get(super::list))
                .with_state(state)
        }

        #[tokio::test]
        async fn list_uses_request_header_as_workspace_root() {
            let _g = env_guard();
            let (state, data_dir) = build_test_state();
            let workspace = make_workspace();
            // 临时工作区内放一个可识别的占位文件, 用于断言 list 真的看见它。
            std::fs::write(workspace.join("marker.txt"), "hello").expect("write marker");

            let app = router(state);

            // 关键 header: x-opencode-directory = 临时工作区。
            // 旧实现会忽略, 退到 settings/cwd → 返回的 entries 来自 ~/.config/gridforge
            // 或 cwd 列表, 不会包含 marker.txt。
            let req = Request::builder()
                .method("GET")
                .uri(format!(
                    "/api/fs/list?path={}",
                    url_encoded_path(&workspace)
                ))
                .header(header::HeaderName::from_static("x-opencode-directory"), workspace.to_string_lossy().to_string())
                .body(Body::empty())
                .expect("build request");

            let response = app.oneshot(req).await.expect("oneshot");

            assert_eq!(
                response.status(),
                StatusCode::OK,
                "expected 200 once request header is honored, got {}",
                response.status()
            );

            let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
                .await
                .expect("read body");
            let json: Value = serde_json::from_slice(&body).expect("parse json");
            let entries = json
                .get("entries")
                .and_then(|v| v.as_array())
                .expect("entries array");
            let names: Vec<&str> = entries
                .iter()
                .filter_map(|e| e.get("name").and_then(|n| n.as_str()))
                .collect();
            assert!(
                names.iter().any(|n| *n == "marker.txt"),
                "entries {:?} should include marker.txt when x-opencode-directory points to the temp workspace",
                names
            );

            std::fs::remove_dir_all(&workspace).ok();
            cleanup(&data_dir);
        }

        #[tokio::test]
        async fn list_without_header_falls_back_to_home_without_panic() {
            let _g = env_guard();
            let (state, data_dir) = build_test_state();
            let app = router(state);

            // 不带 x-opencode-directory; list 端点放宽到 HOME 列表, 不应 panic / 5xx。
            let req = Request::builder()
                .method("GET")
                .uri("/api/fs/list?path=~")
                .body(Body::empty())
                .expect("build request");

            let response = app.oneshot(req).await.expect("oneshot");

            // 200 (HOME 可读) 或 400/403 (HOME 不可读) 都算合规; 5xx 即视为 bug。
            let status = response.status();
            assert!(
                status.is_success() || status == StatusCode::BAD_REQUEST || status == StatusCode::FORBIDDEN,
                "list without header should not 5xx, got {}",
                status
            );

            cleanup(&data_dir);
        }

        /// 回归测试: `?directory=` query 参数必须被识别为工作区候选,
        /// FilesView.tsx 在 `files.readFile` 不可用时的回退路径
        /// (lines 1563-1565) 就是把根目录放到 `?directory=…` 而非 header。
        #[tokio::test]
        async fn list_uses_query_directory_as_workspace_root() {
            let _g = env_guard();
            let (state, data_dir) = build_test_state();
            let workspace = make_workspace();
            std::fs::write(workspace.join("marker.txt"), "hello").expect("write marker");

            let app = router(state);

            // 不带 `x-opencode-directory` 头, 只带 `?directory=<workspace>`。
            // 旧实现没有从 query 取 directory, 退到 settings → 找不到 marker.txt;
            // 修复后 PathQuery/ListQuery 持有 directory, 经 resolve_project_directory
            // 取这个存在的目录作为 base_dir, list 应能看见 marker.txt。
            let req = Request::builder()
                .method("GET")
                .uri(format!(
                    "/api/fs/list?path={}&directory={}",
                    url_encoded_path(&workspace),
                    url_encoded_path(&workspace),
                ))
                .body(Body::empty())
                .expect("build request");

            let response = app.oneshot(req).await.expect("oneshot");

            assert_eq!(
                response.status(),
                StatusCode::OK,
                "expected 200 once query directory is honored, got {}",
                response.status()
            );

            let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
                .await
                .expect("read body");
            let json: Value = serde_json::from_slice(&body).expect("parse json");
            let entries = json
                .get("entries")
                .and_then(|v| v.as_array())
                .expect("entries array");
            let names: Vec<&str> = entries
                .iter()
                .filter_map(|e| e.get("name").and_then(|n| n.as_str()))
                .collect();
            assert!(
                names.iter().any(|n| *n == "marker.txt"),
                "entries {:?} should include marker.txt when ?directory= points to the temp workspace",
                names
            );

            std::fs::remove_dir_all(&workspace).ok();
            cleanup(&data_dir);
        }

        /// 极简 percent-encode, 仅用于构造测试 URI 的 path 参数。
        fn url_encoded_path(path: &PathBuf) -> String {
            let s = path.to_string_lossy().to_string();
            let mut out = String::with_capacity(s.len());
            for byte in s.bytes() {
                match byte {
                    b'A'..=b'Z'
                    | b'a'..=b'z'
                    | b'0'..=b'9'
                    | b'-'
                    | b'_'
                    | b'.'
                    | b'~'
                    | b'/'
                    | b':' => out.push(byte as char),
                    b'\\' => out.push('/'),
                    _ => out.push_str(&format!("%{:02X}", byte)),
                }
            }
            out
        }
    }
}
