//! `/api/mcp/auth/pending` 路由 — MCP OAuth 状态暂存。
//!
//! 对应 Node `packages/web/server/lib/opencode/routes.js` 的:
//! - `POST /api/mcp/auth/pending` (line 307)
//! - `GET /api/mcp/auth/pending` (line 341)
//! - `DELETE /api/mcp/auth/pending` (line 362)
//!
//! Node 端用 `Map<state, { name, directory, expiresAt }>` 存内存,
//! 30 分钟 TTL, 每次操作前剪枝过期项。
//! Rust 端用 `Mutex<HashMap>` 完全复现。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::SystemTime;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::state::AppState;

/// MCP OAuth 待认证上下文。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PendingMcpAuthEntry {
    name: String,
    directory: Option<String>,
    #[serde(rename = "expiresAt")]
    expires_at: u128,
}

/// 暂存管理器 (AppState 注入)。
pub struct McpAuthStore {
    pending: Mutex<HashMap<String, PendingMcpAuthEntry>>,
}

impl McpAuthStore {
    pub fn new() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
        }
    }

    fn prune_expired(&self) {
        let now = now_millis();
        if let Ok(mut guard) = self.pending.lock() {
            guard.retain(|_, entry| entry.expires_at > now);
        }
    }
}

impl Default for McpAuthStore {
    fn default() -> Self {
        Self::new()
    }
}

const PENDING_MCP_AUTH_TTL_MS: u128 = 30 * 60 * 1000;

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn normalize_string(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

// --- Query params for GET/DELETE ---

#[derive(Deserialize)]
pub struct StateQuery {
    state: Option<String>,
}

// --- POST body ---

#[derive(Deserialize)]
pub struct PostPendingBody {
    state: Option<String>,
    name: Option<String>,
    directory: Option<String>,
}

/// POST /api/mcp/auth/pending — 创建待认证上下文。
pub async fn post_mcp_auth_pending(
    State(state): State<std::sync::Arc<AppState>>,
    Json(body): Json<PostPendingBody>,
) -> impl IntoResponse {
    let state_val = match normalize_string(body.state.as_deref().unwrap_or("")) {
        Some(s) => s,
        None => {
            return (StatusCode::OK, Json(json!({ "success": true, "context": null }))).into_response();
        }
    };

    let name = match normalize_string(body.name.as_deref().unwrap_or("")) {
        Some(n) => n,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "MCP server name is required" })),
            )
                .into_response();
        }
    };

    let directory = normalize_string(body.directory.as_deref().unwrap_or(""));

    let entry = PendingMcpAuthEntry {
        name,
        directory,
        expires_at: now_millis() + PENDING_MCP_AUTH_TTL_MS,
    };

    if let Ok(mut guard) = state.mcp_auth.pending.lock() {
        guard.insert(state_val, entry.clone());
    }

    (
        StatusCode::OK,
        Json(json!({
            "success": true,
            "context": {
                "name": entry.name,
                "directory": entry.directory,
            },
        })),
    )
        .into_response()
}

/// GET /api/mcp/auth/pending — 读取待认证上下文。
pub async fn get_mcp_auth_pending(
    State(state): State<std::sync::Arc<AppState>>,
    Query(query): Query<StateQuery>,
) -> impl IntoResponse {
    state.mcp_auth.prune_expired();

    let state_val = match query.state.as_deref().and_then(normalize_string) {
        Some(s) => s,
        None => return (StatusCode::OK, Json(json!(null))).into_response(),
    };

    if let Ok(guard) = state.mcp_auth.pending.lock() {
        if let Some(entry) = guard.get(&state_val) {
            return (
                StatusCode::OK,
                Json(json!({
                    "name": entry.name,
                    "directory": entry.directory,
                    "expiresAt": entry.expires_at,
                })),
            )
                .into_response();
        }
    }

    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": "No pending MCP auth context" })),
    )
        .into_response()
}

/// DELETE /api/mcp/auth/pending — 删除待认证上下文。
pub async fn delete_mcp_auth_pending(
    State(state): State<std::sync::Arc<AppState>>,
    Query(query): Query<StateQuery>,
) -> impl IntoResponse {
    state.mcp_auth.prune_expired();

    let state_val = match query.state.as_deref().and_then(normalize_string) {
        Some(s) => s,
        None => return (StatusCode::OK, Json(json!({ "success": true }))).into_response(),
    };

    if let Ok(mut guard) = state.mcp_auth.pending.lock() {
        guard.remove(&state_val);
    }

    (StatusCode::OK, Json(json!({ "success": true }))).into_response()
}
