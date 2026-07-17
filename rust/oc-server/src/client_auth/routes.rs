//! Client auth 路由 — 10 个 axum handler。
//!
//! 移植自 `core-routes.js` lines 739-942 (`registerAuthAndAccessRoutes` 的 client-auth 部分)。
//! Transport candidates (relay/LAN) 暂返回空/默认值 (relay+LAN 未迁移)。

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use serde_json::{json, Value};

use crate::client_auth::pairing::{CreateSessionParams, RedeemParams};
use crate::client_auth::remote_clients::CreateClientParams;
use crate::state::AppState;
use crate::ui_auth::types::{get_bearer_token, parse_cookies};
use crate::ui_auth::SESSION_COOKIE_NAME;

// ============================================================
// 辅助函数
// ============================================================

/// 解析 auth context (session 或 client)。
/// 返回 (type, client_id, client_kind) 其中 type = "session" | "client"。
async fn resolve_auth_context(
    state: &AppState,
    headers: &HeaderMap,
) -> Option<AuthContext> {
    // 1. session cookie
    let cookie_header = headers
        .get("cookie")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let cookies = parse_cookies(cookie_header.as_deref());
    if let Some(token) = cookies.get(SESSION_COOKIE_NAME) {
        if let Some(ref sm) = state.ui_auth.session_manager {
            if sm.is_session_valid(token) {
                return Some(AuthContext {
                    ctx_type: "session".to_string(),
                    client_id: None,
                    client_kind: None,
                    client: None,
                });
            }
        }
    }

    // 2. client bearer
    let auth_header = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    if let Some(token) = get_bearer_token(auth_header.as_deref()) {
        if let Some(ref client_runtime) = state.ui_auth.client_auth {
            if let Some(result) = client_runtime.authenticate_bearer_token(&token, false) {
                let kind = result
                    .client
                    .get("clientKind")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                return Some(AuthContext {
                    ctx_type: "client".to_string(),
                    client_id: Some(result.client_id.clone()),
                    client_kind: kind,
                    client: Some(result.client),
                });
            }
        }
    }

    None
}

struct AuthContext {
    ctx_type: String,
    client_id: Option<String>,
    client_kind: Option<String>,
    client: Option<Value>,
}

/// 检查是否有 session 或 client auth。
fn has_any_auth(state: &AppState, headers: &HeaderMap) -> bool {
    // session
    let cookie_header = headers
        .get("cookie")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let cookies = parse_cookies(cookie_header.as_deref());
    if let Some(token) = cookies.get(SESSION_COOKIE_NAME) {
        if let Some(ref sm) = state.ui_auth.session_manager {
            if sm.is_session_valid(token) {
                return true;
            }
        }
    }
    // client
    let auth_header = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    if let Some(token) = get_bearer_token(auth_header.as_deref()) {
        if let Some(ref client_runtime) = state.ui_auth.client_auth {
            if client_runtime
                .authenticate_bearer_token(&token, false)
                .is_some()
            {
                return true;
            }
        }
    }
    false
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "error": "Authentication required", "locked": true })),
    )
        .into_response()
}

// ============================================================
// Handlers
// ============================================================

/// GET /api/client-auth/clients — 列出 remote clients。
pub async fn list_clients(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    let ctx = match resolve_auth_context(&state, &headers).await {
        Some(c) => c,
        None => {
            if state.ui_auth.require_client_auth || state.ui_auth.enabled {
                return unauthorized();
            }
            // disabled: fall through with session context
            AuthContext {
                ctx_type: "session".to_string(),
                client_id: None,
                client_kind: None,
                client: None,
            }
        }
    };

    let client_runtime = match state.remote_client_auth.as_ref() {
        Some(rt) => rt,
        None => return Json(json!({ "clients": [] })).into_response(),
    };

    // client auth 非 desktop-local: 仅看自己
    if ctx.ctx_type == "client" && ctx.client_kind.as_deref() != Some("desktop-local") {
        let client = ctx.client.unwrap_or(Value::Null);
        let clients = if client.is_null() {
            vec![]
        } else {
            vec![client]
        };
        return Json(json!({ "clients": clients })).into_response();
    }

    Json(json!({ "clients": client_runtime.list_clients() })).into_response()
}

/// POST /api/client-auth/clients — 创建 remote client。
pub async fn create_client(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let ctx = match resolve_auth_context(&state, &headers).await {
        Some(c) => c,
        None => {
            if state.ui_auth.require_client_auth || state.ui_auth.enabled {
                return unauthorized();
            }
            AuthContext {
                ctx_type: "session".to_string(),
                client_id: None,
                client_kind: None,
                client: None,
            }
        }
    };

    // client auth 非 desktop-local: 403
    if ctx.ctx_type == "client" && ctx.client_kind.as_deref() != Some("desktop-local") {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "Client tokens cannot create remote clients" })),
        )
            .into_response();
    }

    let client_runtime = match state.remote_client_auth.as_ref() {
        Some(rt) => rt,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "Client auth not configured" })),
            )
                .into_response();
        }
    };

    let params = CreateClientParams {
        label: body.get("label").and_then(|v| v.as_str()).map(|s| s.to_string()),
        client_kind: body.get("clientKind").and_then(|v| v.as_str()).map(|s| s.to_string()),
        dedupe_key: body.get("dedupeKey").and_then(|v| v.as_str()).map(|s| s.to_string()),
        ..Default::default()
    };

    match client_runtime.create_client(params) {
        Ok((client, token)) => (
            StatusCode::CREATED,
            [("cache-control", "no-store")],
            Json(json!({ "client": client, "token": token })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// DELETE /api/client-auth/clients/:id — 撤销单个 client。
pub async fn revoke_client(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let ctx = match resolve_auth_context(&state, &headers).await {
        Some(c) => c,
        None => return unauthorized(),
    };

    let client_runtime = match state.remote_client_auth.as_ref() {
        Some(rt) => rt,
        None => return Json(json!({ "revoked": false })).into_response(),
    };

    // client auth 非 desktop-local: 仅能撤销自己
    if ctx.ctx_type == "client" && ctx.client_kind.as_deref() != Some("desktop-local") {
        let client_id = ctx.client_id.as_deref().unwrap_or("");
        if client_id != id {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({ "revoked": false, "error": "Client tokens can only revoke themselves" })),
            )
                .into_response();
        }
    }

    match client_runtime.revoke_client(&id) {
        Ok((revoked, client)) => Json(json!({ "revoked": revoked, "client": client })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// DELETE /api/client-auth/clients — 撤销所有 client。
pub async fn revoke_all_clients(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    let ctx = match resolve_auth_context(&state, &headers).await {
        Some(c) => c,
        None => return unauthorized(),
    };

    let client_runtime = match state.remote_client_auth.as_ref() {
        Some(rt) => rt,
        None => return Json(json!({ "revoked": 0 })).into_response(),
    };

    // client auth 非 desktop-local: 403
    if ctx.ctx_type == "client" && ctx.client_kind.as_deref() != Some("desktop-local") {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "Client tokens cannot revoke all clients" })),
        )
            .into_response();
    }

    match client_runtime.revoke_all_clients() {
        Ok(count) => Json(json!({ "revoked": count })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// POST /api/client-auth/pairing/sessions — 创建 pairing session。
pub async fn create_pairing_session(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let ctx = match resolve_auth_context(&state, &headers).await {
        Some(c) => c,
        None => return unauthorized(),
    };

    // client auth 非 desktop-local: 403
    if ctx.ctx_type == "client" && ctx.client_kind.as_deref() != Some("desktop-local") {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "Client tokens cannot create pairing sessions" })),
        )
            .into_response();
    }

    let pairing = match state.client_pairing.as_ref() {
        Some(p) => p,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "Pairing not configured" })),
            )
                .into_response();
        }
    };

    let allowed_kinds: Vec<String> = body
        .get("allowedClientKinds")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    let include_relay = body
        .get("includeRelay")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let params = CreateSessionParams {
        label: body.get("label").and_then(|v| v.as_str()).map(|s| s.to_string()),
        allowed_client_kinds: allowed_kinds,
        created_by_client_id: ctx.client_id.clone(),
        uses_relay: include_relay,
    };

    match pairing.create_session(params) {
        Ok((pairing_data, secret)) => {
            // candidates 暂返回空数组 (relay+LAN 未迁移)
            let candidates: Vec<Value> = vec![];
            (
                StatusCode::CREATED,
                [("cache-control", "no-store")],
                Json(json!({
                    "pairing": pairing_data,
                    "secret": secret,
                    "server": {
                        "label": "GridForge",
                        "candidates": candidates,
                    }
                })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// GET /api/client-auth/connection/candidates — 刷新 transports。
pub async fn connection_candidates(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    if !has_any_auth(&state, &headers) && (state.ui_auth.require_client_auth || state.ui_auth.enabled)
    {
        return unauthorized();
    }
    // candidates 暂返回空数组 (relay+LAN 未迁移)
    (
        [("cache-control", "no-store")],
        Json(json!({
            "label": "GridForge",
            "candidates": [],
        })),
    )
        .into_response()
}

/// GET /api/client-auth/pairing/transports — 直接 transports。
pub async fn pairing_transports(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    let ctx = match resolve_auth_context(&state, &headers).await {
        Some(c) => c,
        None => {
            if state.ui_auth.require_client_auth || state.ui_auth.enabled {
                return unauthorized();
            }
            AuthContext {
                ctx_type: "session".to_string(),
                client_id: None,
                client_kind: None,
                client: None,
            }
        }
    };

    // client auth 非 desktop-local: 403
    if ctx.ctx_type == "client" && ctx.client_kind.as_deref() != Some("desktop-local") {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "Client tokens cannot access pairing transports" })),
        )
            .into_response();
    }

    (
        [("cache-control", "no-store")],
        Json(json!({
            "local": null,
            "lan": null,
            "relayAvailable": false,
        })),
    )
        .into_response()
}

/// GET /api/client-auth/pairing/sessions — 列出 pending sessions。
pub async fn list_pairing_sessions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    let ctx = match resolve_auth_context(&state, &headers).await {
        Some(c) => c,
        None => return unauthorized(),
    };

    if ctx.ctx_type == "client" && ctx.client_kind.as_deref() != Some("desktop-local") {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "Client tokens cannot list pairing sessions" })),
        )
            .into_response();
    }

    let pairing = match state.client_pairing.as_ref() {
        Some(p) => p,
        None => return Json(json!({ "pending": [] })).into_response(),
    };

    let pending = pairing.list_pending();
    (
        [("cache-control", "no-store")],
        Json(json!({ "pending": pending })),
    )
        .into_response()
}

/// DELETE /api/client-auth/pairing/sessions/:id — 取消 pairing session。
pub async fn cancel_pairing_session(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let ctx = match resolve_auth_context(&state, &headers).await {
        Some(c) => c,
        None => return unauthorized(),
    };

    if ctx.ctx_type == "client" && ctx.client_kind.as_deref() != Some("desktop-local") {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "Client tokens cannot cancel pairing sessions" })),
        )
            .into_response();
    }

    let pairing = match state.client_pairing.as_ref() {
        Some(p) => p,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "cancelled": false, "error": "Pairing session not found" })),
            )
                .into_response();
        }
    };

    match pairing.cancel_session(&id) {
        Ok((cancelled, session)) => {
            if !cancelled {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({ "cancelled": false, "error": "Pairing session not found" })),
                )
                    .into_response();
            }
            Json(json!({ "cancelled": true, "pairing": session })).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// POST /api/client-auth/pairing/redeem — redeem pairing secret → client token。
/// 无认证, 有限速 (10/5min)。
pub async fn redeem_pairing(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let pairing = match state.client_pairing.as_ref() {
        Some(p) => p,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "Invalid or expired pairing session" })),
            )
                .into_response();
        }
    };

    let params = RedeemParams {
        pairing_id: body
            .get("pairingId")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        secret: body
            .get("secret")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        client_label: body
            .get("clientLabel")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        client_kind: body
            .get("clientKind")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        device_name: body
            .get("deviceName")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        device_platform: body
            .get("devicePlatform")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        device_model: body
            .get("deviceModel")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        app_version: body
            .get("appVersion")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        dedupe_key: body
            .get("dedupeKey")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
    };

    match pairing.redeem_session(params) {
        Ok((pairing_data, client, token)) => (
            [("cache-control", "no-store")],
            Json(json!({
                "ok": true,
                "server": {
                    "label": "GridForge",
                    "fingerprint": pairing_data.get("fingerprint").cloned().unwrap_or(Value::Null),
                },
                "client": client,
                "clientToken": token
            })),
        )
            .into_response(),
        Err(_) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "Invalid or expired pairing session" })),
        )
            .into_response(),
    }
}

use axum::response::Response;
