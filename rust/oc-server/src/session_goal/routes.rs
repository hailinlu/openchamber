//! Session-goal HTTP routes — 3 个 handlers。
//!
//! 对应 Node `session-goal/routes.js`:
//! - `PUT  /api/goals/objective/:sessionId` body `{content}` → write_objective
//! - `GET  /api/goals/objective/:sessionId` → read_objective (404 if missing)
//! - `DELETE /api/goals/objective/:sessionId` → delete_objective (best-effort)
//!
//! File-backed objective (UI 在 goal 标记 `objectiveFile: true` 时调用):
//! UI 先 PUT 文件, 再 stamp goal metadata; 读时 GET 文件; 删除时 DELETE 文件。

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::{ApiError, ApiResult};
use crate::session_goal::objectives;
use crate::state::AppState;

/// PUT 请求 body。
#[derive(Debug, Deserialize, Default)]
pub struct PutObjectiveBody {
    #[serde(default)]
    pub content: String,
}

/// `PUT /api/goals/objective/{session_id}` body `{content}`。
///
/// 写入目标文件, UI 在 stamp goal metadata 前调用。
pub async fn put_objective(
    State(_state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    Json(body): Json<PutObjectiveBody>,
) -> ApiResult<Json<Value>> {
    let written = objectives::write_objective(&session_id, &body.content).await?;
    Ok(Json(json!({ "ok": true, "content": written })))
}

/// `GET /api/goals/objective/{session_id}`。
///
/// 缺失返回 404。
pub async fn get_objective(
    State(_state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
) -> ApiResult<Json<Value>> {
    match objectives::read_objective(&session_id).await {
        Some(content) => Ok(Json(json!({ "content": content }))),
        None => Err(ApiError(oc_core::Error::NotFound("objective not found".into()))),
    }
}

/// `DELETE /api/goals/objective/{session_id}`。
///
/// Best-effort 删除 — 缺失文件视为成功。
pub async fn delete_objective(
    State(_state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
) -> ApiResult<Json<Value>> {
    objectives::delete_objective(&session_id).await;
    Ok(Json(json!({ "ok": true })))
}

/// 保留状态码 404 显式调用点 (供测试断言)。
#[allow(dead_code)]
pub fn not_found_status() -> StatusCode {
    StatusCode::NOT_FOUND
}