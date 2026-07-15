//! Notification 路由 — 18 个 axum handler。
//!
//! 对应 Node `notifications/routes.js` (`registerNotificationRoutes`, 409 行)。
//!
//! 路由列表:
//!  1. GET    /api/push/vapid-public-key
//!  2. POST   /api/push/subscribe
//!  3. DELETE /api/push/subscribe
//!  4. POST   /api/push/apns-token
//!  5. DELETE /api/push/apns-token
//!  6. POST   /api/push/visibility
//!  7. GET    /api/push/visibility
//!  8. GET    /api/notifications/stream            (SSE)
//!  9. GET    /api/session-activity
//! 10. GET    /api/sessions/snapshot
//! 11. GET    /api/sessions/status
//! 12. GET    /api/sessions/{id}/status
//! 13. GET    /api/sessions/attention
//! 14. GET    /api/sessions/{id}/attention
//! 15. POST   /api/sessions/{id}/view
//! 16. POST   /api/sessions/{id}/unview
//! 17. POST   /api/sessions/{id}/message-sent
//! 18. POST   /api/notifications/auto-accept

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use bytes::Bytes;
use futures_util::stream;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::state::AppState;
use crate::ui_auth::types::parse_cookies;
use crate::ui_auth::SESSION_COOKIE_NAME;

use super::relay_key;
use super::types::{parse_push_subscribe_body, parse_push_unsubscribe_body};

// ============================================================
// 辅助函数
// ============================================================

/// 从请求 headers 提取 UI session token (`oc_ui_session` cookie)。
///
/// 对应 Node `getUiSessionTokenFromRequest`。
fn get_ui_session_token(headers: &HeaderMap) -> Option<String> {
    let cookie_header = headers.get("cookie").and_then(|v| v.to_str().ok())?;
    let cookies = parse_cookies(Some(cookie_header));
    cookies.get(SESSION_COOKIE_NAME).cloned()
}

/// 从请求 headers 提取 user-agent。
fn get_user_agent(headers: &HeaderMap) -> Option<String> {
    headers
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

/// 从请求 headers 提取 client ID (`x-client-id` 或 `x-forwarded-for` 或 `anonymous`)。
///
/// 对应 Node `req.headers['x-client-id'] || req.ip || 'anonymous'`。
fn get_client_id(headers: &HeaderMap) -> String {
    if let Some(cid) = headers.get("x-client-id").and_then(|v| v.to_str().ok()) {
        if !cid.trim().is_empty() {
            return cid.to_string();
        }
    }
    if let Some(xff) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        let first = xff.split(',').next().unwrap_or("").trim();
        if !first.is_empty() {
            return first.to_string();
        }
    }
    "anonymous".to_string()
}

// ============================================================
// 1. GET /api/push/vapid-public-key
// ============================================================

/// 返回 VAPID public key。
pub async fn vapid_public_key() -> Response {
    let (public_key, _private_key) = relay_key::get_or_create_vapid_keys();
    Json(json!({ "publicKey": public_key })).into_response()
}

// ============================================================
// 2. POST /api/push/subscribe
// ============================================================

/// 持久化 web-push 订阅。
///
/// 对应 Node `app.post('/api/push/subscribe')`。
pub async fn push_subscribe(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let ui_token = match get_ui_session_token(&headers) {
        Some(t) if !t.is_empty() => t,
        _ => return (StatusCode::UNAUTHORIZED, Json(json!({ "error": "UI session missing" }))).into_response(),
    };

    let (endpoint, p256dh, auth) = match parse_push_subscribe_body(&body) {
        Some(parsed) => parsed,
        None => return (StatusCode::BAD_REQUEST, Json(json!({ "error": "Invalid body" }))).into_response(),
    };

    // origin → publicOrigin (首次设置)
    let origin = body.get("origin").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    if origin.starts_with("http://") || origin.starts_with("https://") {
        let settings = crate::github::settings::read_settings();
        let has_public_origin = settings
            .get("publicOrigin")
            .and_then(|v| v.as_str())
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);
        if !has_public_origin {
            let mut next = settings;
            if let Value::Object(ref mut map) = next {
                map.insert("publicOrigin".to_string(), Value::String(origin));
            }
            let _ = crate::github::settings::write_settings(&next);
        }
    }

    let platform = body
        .get("platform")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let user_agent = get_user_agent(&headers);

    state.push_store.add_or_update_subscription(
        &ui_token,
        &endpoint,
        &p256dh,
        &auth,
        user_agent.as_deref(),
        platform.as_deref(),
    );

    Json(json!({ "ok": true })).into_response()
}

// ============================================================
// 3. DELETE /api/push/subscribe
// ============================================================

/// 删除 web-push 订阅。
pub async fn push_unsubscribe(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let ui_token = match get_ui_session_token(&headers) {
        Some(t) if !t.is_empty() => t,
        _ => return (StatusCode::UNAUTHORIZED, Json(json!({ "error": "UI session missing" }))).into_response(),
    };

    let endpoint = match parse_push_unsubscribe_body(&body) {
        Some(e) => e,
        None => return (StatusCode::BAD_REQUEST, Json(json!({ "error": "Invalid body" }))).into_response(),
    };

    state.push_store.remove_subscription(&ui_token, &endpoint);
    Json(json!({ "ok": true })).into_response()
}

// ============================================================
// 4. POST /api/push/apns-token
// ============================================================

/// 注册 APNs device token。
pub async fn apns_token_subscribe(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let ui_token = match get_ui_session_token(&headers) {
        Some(t) if !t.is_empty() => t,
        _ => return (StatusCode::UNAUTHORIZED, Json(json!({ "error": "UI session missing" }))).into_response(),
    };

    let device_token = body.get("token").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    if device_token.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "Invalid body" }))).into_response();
    }

    let platform = if body.get("platform").and_then(|v| v.as_str()) == Some("android") {
        "android"
    } else {
        "ios"
    };
    let user_agent = get_user_agent(&headers);

    state.apns_store.add_or_update_token(
        &ui_token,
        &device_token,
        user_agent.as_deref(),
        Some(platform),
    );

    // 异步注册到 relay (fire-and-forget)
    {
        let apns_send = state.apns_send.clone();
        let token = device_token.clone();
        let plat = platform.to_string();
        tokio::spawn(async move {
            apns_send.register_token_with_relay(&token, &plat).await;
        });
    }

    Json(json!({ "ok": true })).into_response()
}

// ============================================================
// 5. DELETE /api/push/apns-token
// ============================================================

/// 删除 APNs device token。
pub async fn apns_token_unsubscribe(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let ui_token = match get_ui_session_token(&headers) {
        Some(t) if !t.is_empty() => t,
        _ => return (StatusCode::UNAUTHORIZED, Json(json!({ "error": "UI session missing" }))).into_response(),
    };

    let device_token = body.get("token").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    if device_token.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "Invalid body" }))).into_response();
    }

    state.apns_store.remove_token(&ui_token, &device_token);
    Json(json!({ "ok": true })).into_response()
}

// ============================================================
// 6. POST /api/push/visibility
// ============================================================

/// 可见性心跳。
pub async fn set_visibility(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let ui_token = match get_ui_session_token(&headers) {
        Some(t) if !t.is_empty() => t,
        _ => return (StatusCode::UNAUTHORIZED, Json(json!({ "error": "UI session missing" }))).into_response(),
    };

    let visible = body.get("visible").and_then(|v| v.as_bool()) == Some(true);
    let platform = body
        .get("platform")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    state.push_store.update_visibility(&ui_token, visible, platform.as_deref());
    Json(json!({ "ok": true })).into_response()
}

// ============================================================
// 7. GET /api/push/visibility
// ============================================================

/// 查询可见性。
pub async fn get_visibility(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    let ui_token = match get_ui_session_token(&headers) {
        Some(t) if !t.is_empty() => t,
        _ => return (StatusCode::UNAUTHORIZED, Json(json!({ "error": "UI session missing" }))).into_response(),
    };

    let visible = state.push_store.is_ui_visible(&ui_token);
    Json(json!({ "ok": true, "visible": visible })).into_response()
}

// ============================================================
// 8. GET /api/notifications/stream (SSE)
// ============================================================

/// SSE 通知流 (20s 心跳)。
///
/// 对应 Node `app.get('/api/notifications/stream')`。
/// 与 SSE 透传代理不同, 这是 OpenChamber 自有的通知流。
pub async fn notification_stream(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    let ui_token = match get_ui_session_token(&headers) {
        Some(t) if !t.is_empty() => t,
        _ => return (StatusCode::UNAUTHORIZED, Json(json!({ "error": "UI session missing" }))).into_response(),
    };

    let mut rx = state.emitter.subscribe_sse();

    let (tx, stream_rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(64);

    // 启动转发 task: broadcast receiver → mpsc channel
    tokio::spawn(async move {
        // 发送 stream-ready 事件
        let ready = json!({
            "type": "openchamber:notification-stream-ready",
            "properties": { "uiToken": ui_token }
        });
        let ready_bytes = Bytes::from(format!("data: {}\n\n", ready));
        if tx.send(Ok(ready_bytes)).await.is_err() {
            return;
        }

        // 消费 broadcast, 推入 channel
        loop {
            match rx.recv().await {
                Ok(bytes) => {
                    if tx.send(Ok(bytes)).await.is_err() {
                        break; // 客户端断开
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    // 心跳 task
    let (heartbeat_tx, heartbeat_rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(4);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(
            super::NOTIFICATION_SSE_HEARTBEAT_INTERVAL_MS,
        ));
        interval.tick().await; // 跳过第一次
        loop {
            interval.tick().await;
            let heartbeat = Bytes::from_static(b":heartbeat\n\n");
            if heartbeat_tx.send(Ok(heartbeat)).await.is_err() {
                break;
            }
        }
    });

    // 合并通知流 + 心跳流
    let notification_stream = tokio_stream::wrappers::ReceiverStream::new(stream_rx);
    let heartbeat_stream = tokio_stream::wrappers::ReceiverStream::new(heartbeat_rx);
    let merged = stream::select(notification_stream, heartbeat_stream);
    let body = Body::from_stream(merged);

    let mut response = axum::response::Response::new(body);
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        "content-type",
        "text/event-stream; charset=utf-8".parse().unwrap(),
    );
    response.headers_mut().insert(
        "cache-control",
        "no-cache, no-transform".parse().unwrap(),
    );
    response.headers_mut().insert(
        "connection",
        "keep-alive".parse().unwrap(),
    );
    response.headers_mut().insert(
        "x-accel-buffering",
        "no".parse().unwrap(),
    );
    response
}

// ============================================================
// 9. GET /api/session-activity
// ============================================================

/// session activity 快照。
pub async fn session_activity(State(state): State<Arc<AppState>>) -> Response {
    let snapshot = state.session_state.get_activity_snapshot();
    Json(snapshot).into_response()
}

// ============================================================
// 10. GET /api/sessions/snapshot
// ============================================================

/// status + attention + serverTime 快照。
pub async fn sessions_snapshot(State(state): State<Arc<AppState>>) -> Response {
    let status = state.session_state.get_state_snapshot();
    let attention = state.session_state.get_attention_snapshot();
    let server_time = super::now_millis();
    Json(json!({
        "statusSessions": status,
        "attentionSessions": attention,
        "serverTime": server_time,
    }))
    .into_response()
}

// ============================================================
// 11. GET /api/sessions/status
// ============================================================

/// status 快照。
pub async fn sessions_status(State(state): State<Arc<AppState>>) -> Response {
    let snapshot = state.session_state.get_state_snapshot();
    let server_time = super::now_millis();
    Json(json!({
        "sessions": snapshot,
        "serverTime": server_time,
    }))
    .into_response()
}

// ============================================================
// 12. GET /api/sessions/{id}/status
// ============================================================

/// 单 session status。
pub async fn session_status(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
) -> Response {
    match state.session_state.get_session_state(&session_id) {
        Some(s) => {
            let mut result = serde_json::Map::new();
            result.insert("sessionId".to_string(), Value::String(session_id));
            if let Value::Object(obj) = s {
                result.extend(obj);
            }
            Json(Value::Object(result)).into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({
                "error": "Session not found or no state available",
                "sessionId": session_id
            })),
        )
            .into_response(),
    }
}

// ============================================================
// 13. GET /api/sessions/attention
// ============================================================

/// attention 快照。
pub async fn sessions_attention(State(state): State<Arc<AppState>>) -> Response {
    let snapshot = state.session_state.get_attention_snapshot();
    let server_time = super::now_millis();
    Json(json!({
        "sessions": snapshot,
        "serverTime": server_time,
    }))
    .into_response()
}

// ============================================================
// 14. GET /api/sessions/{id}/attention
// ============================================================

/// 单 session attention 状态。
pub async fn session_attention(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
) -> Response {
    match state.session_state.get_attention_state(&session_id) {
        Some(s) => {
            let mut result = serde_json::Map::new();
            result.insert("sessionId".to_string(), Value::String(session_id));
            if let Value::Object(obj) = s {
                result.extend(obj);
            }
            Json(Value::Object(result)).into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({
                "error": "Session not found or no attention state available",
                "sessionId": session_id
            })),
        )
            .into_response(),
    }
}

// ============================================================
// 15. POST /api/sessions/{id}/view
// ============================================================

/// mark viewed + clear badge。
pub async fn session_view(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
) -> Response {
    let client_id = get_client_id(&headers);
    state.session_state.mark_session_viewed(&session_id, &client_id);
    if let Some(trigger) = state.notification_trigger.get() {
        trigger.clear_pending_push_badge();
    }
    Json(json!({
        "success": true,
        "sessionId": session_id,
        "viewed": true,
    }))
    .into_response()
}

// ============================================================
// 16. POST /api/sessions/{id}/unview
// ============================================================

/// mark unviewed。
pub async fn session_unview(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
) -> Response {
    let client_id = get_client_id(&headers);
    state.session_state.mark_session_unviewed(&session_id, &client_id);
    Json(json!({
        "success": true,
        "sessionId": session_id,
        "viewed": false,
    }))
    .into_response()
}

// ============================================================
// 17. POST /api/sessions/{id}/message-sent
// ============================================================

/// mark message sent + clear badge。
pub async fn session_message_sent(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
) -> Response {
    state.session_state.mark_user_message_sent(&session_id);
    if let Some(trigger) = state.notification_trigger.get() {
        trigger.clear_pending_push_badge();
    }
    Json(json!({
        "success": true,
        "sessionId": session_id,
        "messageSent": true,
    }))
    .into_response()
}

// ============================================================
// 18. POST /api/notifications/auto-accept
// ============================================================

/// mirror auto-accept state。
pub async fn auto_accept(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let session_id = body
        .get("sessionId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let enabled = body.get("enabled").and_then(|v| v.as_bool()) == Some(true);

    if session_id.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "sessionId required" }))).into_response();
    }

    if let Some(trigger) = state.notification_trigger.get() {
        trigger.set_auto_accept_session(&session_id, enabled);
    }
    Json(json!({
        "success": true,
        "sessionId": session_id,
        "enabled": enabled,
    }))
    .into_response()
}

type Response = axum::response::Response;
