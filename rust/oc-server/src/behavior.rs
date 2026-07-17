//! `/api/config/settings` + `/api/behavior/agents-md` 路由。
//!
//! Tauri 模式下 `__OPENCHAMBER_API_BASE_URL__` 指向 Rust server,
//! 但 settings 相关的路由原本只在 Node 后端实现。
//! 此处提供简单的文件读写实现，覆盖 BehaviorPage 所需字段。
//!
//! 注意: 这是前端调用 settings 端点的最小实现，不包含 Node 端的迁移/
//! 校验逻辑。如需完整功能，后续可考虑 proxy 到 Node 后端或完善此模块。

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde_json::{json, Value};

use crate::state::AppState;

#[cfg(not(target_os = "windows"))]
fn default_settings_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".config/openchamber/settings.json")
}

#[cfg(target_os = "windows")]
fn default_settings_path() -> PathBuf {
    let home = std::env::var("USERPROFILE").unwrap_or_else(|_| "C:\\".to_string());
    PathBuf::from(home).join(".config/openchamber/settings.json")
}

fn resolve_settings_path() -> PathBuf {
    if let Ok(dir) = std::env::var("OPENCHAMBER_DATA_DIR") {
        PathBuf::from(dir).join("settings.json")
    } else {
        default_settings_path()
    }
}

#[cfg(not(target_os = "windows"))]
fn agents_md_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".config/opencode/AGENTS.md")
}

#[cfg(target_os = "windows")]
fn agents_md_path() -> PathBuf {
    let home = std::env::var("USERPROFILE").unwrap_or_else(|_| "C:\\".to_string());
    PathBuf::from(home).join(".config/opencode/AGENTS.md")
}

/// GET /api/config/settings
pub async fn get_settings(
    State(_state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let path = resolve_settings_path();
    match tokio::fs::read_to_string(&path).await {
        Ok(content) => match serde_json::from_str::<Value>(&content) {
            Ok(settings) => Json(settings).into_response(),
            Err(e) => {
                tracing::error!(path = %path.display(), error = %e, "failed to parse settings");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": "Failed to parse settings" })),
                )
                    .into_response()
            }
        },
        Err(_) => {
            // 文件不存在 → 返回空对象 (UI 用默认值)
            Json(json!({})).into_response()
        }
    }
}

/// PUT /api/config/settings
pub async fn put_settings(
    State(_state): State<Arc<AppState>>,
    Json(changes): Json<Value>,
) -> impl IntoResponse {
    let path = resolve_settings_path();

    // 读取已有 settings
    let existing = match tokio::fs::read_to_string(&path).await {
        Ok(content) => serde_json::from_str::<Value>(&content).unwrap_or(json!({})),
        Err(_) => json!({}),
    };

    // 合并: changes 的属性覆盖到 existing
    let merged = deep_merge(existing, changes);

    // 确保父目录存在
    if let Some(parent) = path.parent() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            tracing::error!(path = %parent.display(), error = %e, "failed to create settings dir");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Failed to create settings directory" })),
            )
                .into_response();
        }
    }

    // 以 tmp + rename 方式原子写入
    let tmp_path = {
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let suffix = format!("{}-{}", pid, nanos);
        path.with_extension(format!("json.tmp-{}", suffix))
    };

    let content = serde_json::to_string_pretty(&merged).unwrap_or_default();
    match tokio::fs::write(&tmp_path, &content).await {
        Ok(_) => {
            match tokio::fs::rename(&tmp_path, &path).await {
                Ok(_) => Json(merged).into_response(),
                Err(e) => {
                    tracing::error!(from = %tmp_path.display(), to = %path.display(), error = %e, "rename failed");
                    let _ = tokio::fs::remove_file(&tmp_path).await;
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({ "error": "Failed to write settings" })),
                    )
                        .into_response()
                }
            }
        }
        Err(e) => {
            tracing::error!(path = %tmp_path.display(), error = %e, "write failed");
            let _ = tokio::fs::remove_file(&tmp_path).await;
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Failed to write settings" })),
            )
                .into_response()
        }
    }
}

/// GET /api/behavior/agents-md
pub async fn get_agents_md(
    State(_state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let path = agents_md_path();
    match tokio::fs::read_to_string(&path).await {
        Ok(content) => Json(json!({
            "content": content,
            "exists": true,
        }))
        .into_response(),
        Err(_) => Json(json!({
            "content": "",
            "exists": false,
        }))
        .into_response(),
    }
}

/// PUT /api/behavior/agents-md
pub async fn put_agents_md(
    State(_state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    const MAX_SIZE: usize = 256 * 1024; // 256KB

    let content = body
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    if content.len() > MAX_SIZE {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({ "error": "Content exceeds maximum size" })),
        )
            .into_response();
    }

    let path = agents_md_path();

    // 确保父目录存在
    if let Some(parent) = path.parent() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            tracing::error!(path = %parent.display(), error = %e, "failed to create agents-md dir");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Failed to create directory" })),
            )
                .into_response();
        }
    }

    match tokio::fs::write(&path, &content).await {
        Ok(_) => Json(json!({ "success": true })).into_response(),
        Err(e) => {
            tracing::error!(path = %path.display(), error = %e, "failed to write AGENTS.md");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Failed to write AGENTS.md" })),
            )
                .into_response()
        }
    }
}

/// 深度合并: b 中的字段覆盖到 a。
/// 遇到嵌套对象时递归合并 (非对象字段直接覆盖)。
fn deep_merge(a: Value, b: Value) -> Value {
    match (a, b) {
        (Value::Object(mut a_map), Value::Object(b_map)) => {
            for (k, v) in b_map {
                if let Some(existing) = a_map.remove(&k) {
                    a_map.insert(k, deep_merge(existing, v));
                } else {
                    a_map.insert(k, v);
                }
            }
            Value::Object(a_map)
        }
        (_a, b) => b,
    }
}
