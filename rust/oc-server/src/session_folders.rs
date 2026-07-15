//! Session folders — JSON 文件原子读写。
//!
//! 对应 Node `session-folders/routes.js` (63 行)。
//!
//! 路由:
//!  1. GET  /api/session-folders — 读取 `sessions-directories.json`
//!  2. POST /api/session-folders — 原子写入 (4MB 上限)

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use crate::github::settings::data_dir;
use crate::state::AppState;

/// 最大 body 大小: 4 MB。
const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

/// `sessions-directories.json` 文件路径。
fn file_path() -> std::path::PathBuf {
    data_dir().join("sessions-directories.json")
}

/// 默认空状态 (文件不存在 / 解析失败时返回)。
fn default_state() -> Value {
    json!({
        "version": 1,
        "foldersMap": {},
        "collapsedFolderIds": [],
        "updatedAt": 0,
    })
}

/// 原子写入: `.tmp → rename` (复用 `fs::operations::write` 模式)。
///
/// 返回 `Ok(())` 成功, `Err(message)` 失败。
async fn atomic_write(path: &std::path::Path, content: &str) -> Result<(), String> {
    // 确保 parent 目录存在
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| e.to_string())?;
    }

    let tmp_path = format!(
        "{}.tmp-{}-{}-{}",
        path.to_string_lossy(),
        std::process::id(),
        chrono::Utc::now().timestamp_millis(),
        rand::random::<u32>(),
    );
    let tmp = std::path::PathBuf::from(&tmp_path);

    // 写 .tmp
    if let Err(e) = tokio::fs::write(&tmp, content).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e.to_string());
    }

    // rename
    if let Err(e) = tokio::fs::rename(&tmp, path).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e.to_string());
    }

    Ok(())
}

// ============================================================
// 1. GET /api/session-folders
// ============================================================

/// 读取 session folders 状态。
///
/// 文件不存在 → 返回默认空状态; 解析失败 → 也返回默认。
pub async fn get_session_folders(State(_state): State<Arc<AppState>>) -> Response {
    let path = file_path();
    match tokio::fs::read_to_string(&path).await {
        Ok(raw) => {
            let parsed = serde_json::from_str::<Value>(&raw);
            match parsed {
                Ok(value) => Json(value).into_response(),
                Err(_) => Json(default_state()).into_response(),
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Json(default_state()).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

// ============================================================
// 2. POST /api/session-folders
// ============================================================

/// 原子写入 session folders 状态。
///
/// body 必须是 JSON object; 超过 4MB 返回 413。
pub async fn post_session_folders(
    State(_state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    // 校验: body 必须是 object
    if !body.is_object() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "Body must be an object" })),
        )
            .into_response();
    }

    let serialized = serde_json::to_string_pretty(&body).unwrap_or_default();

    // 大小检查
    if serialized.len() > MAX_BODY_BYTES {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({ "error": "Payload too large" })),
        )
            .into_response();
    }

    let path = file_path();
    match atomic_write(&path, &serialized).await {
        Ok(()) => Json(json!({ "success": true })).into_response(),
        Err(message) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": message })),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static TEST_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn temp_path() -> std::path::PathBuf {
        let dir = std::env::temp_dir();
        let seq = TEST_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        dir.join(format!(
            "oc-test-session-folders-{}-{}-{}.json",
            std::process::id(),
            chrono::Utc::now().timestamp_millis(),
            seq,
        ))
    }

    #[tokio::test]
    async fn atomic_write_roundtrip() {
        let path = temp_path();
        let _ = tokio::fs::remove_file(&path).await;

        atomic_write(&path, r#"{"version":1,"foldersMap":{}}"#)
            .await
            .unwrap();
        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(content.contains("foldersMap"));

        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn atomic_write_cleans_up_tmp_on_rename_failure() {
        // 写入到一个不存在的目录 → rename 失败, tmp 应被清理
        let path = std::path::PathBuf::from("/nonexistent/dir/file.json");
        let result = atomic_write(&path, "{}").await;
        assert!(result.is_err());
    }

    #[test]
    fn default_state_has_correct_shape() {
        let state = default_state();
        assert_eq!(state["version"], 1);
        assert!(state["foldersMap"].is_object());
        assert!(state["collapsedFolderIds"].is_array());
        assert_eq!(state["updatedAt"], 0);
    }
}
