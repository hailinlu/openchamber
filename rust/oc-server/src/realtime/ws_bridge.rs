//! WebSocket 桥 — 全局事件桥 + 目录事件桥。
//!
//! 对应:
//!   - `global-ws-bridge.js` — 全局 hub fan-out + ready 握手 + replay
//!   - `directory-ws-bridge.js` — 每连接独享上游 reader + ready 握手
//!
//! WS 帧协议: JSON-over-text-frames, 4 种帧 (ready/event/error/backpressure)。
//! 背压: axum `send().await` 天然背压 + `max_write_buffer_size(16MB)` 硬上限。
//!
//! 路由:
//!   - `GET /api/global/event/ws` → global_ws_handler
//!   - `GET /api/event/ws`        → directory_ws_handler

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::Response;
use bytes::Bytes;
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::broadcast;
use tokio::time;

use crate::state::AppState;

use super::global_hub::{HubStatus, ReplayEntry};
use super::protocol::WsFrame;
use super::upstream_reader::{UpstreamEvent, UpstreamReaderConfig, UpstreamSseReader};
use super::{
    UPSTREAM_RECONNECT_DELAY, UPSTREAM_STALL_TIMEOUT, WS_BACKPRESSURE_WARN_BYTES,
    WS_HEARTBEAT_INTERVAL, WS_MAX_BUFFERED_BYTES,
};

/// WS 连接查询参数。
#[derive(Deserialize, Debug)]
pub struct WsParams {
    #[serde(rename = "lastEventId")]
    pub last_event_id: Option<String>,
    pub directory: Option<String>,
}

// =========================================================================
// 全局 WS 桥
// =========================================================================

/// `GET /api/global/event/ws` handler。
pub async fn global_ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    Query(params): Query<WsParams>,
) -> Response {
    ws.max_write_buffer_size(WS_MAX_BUFFERED_BYTES)
        .max_message_size(WS_MAX_BUFFERED_BYTES * 2)
        .on_upgrade(move |socket| run_global_bridge(socket, state, params))
}

/// 全局 WS 桥连接生命周期。
///
/// 对应 `global-ws-bridge.js` 的 `accept` + 事件循环。
async fn run_global_bridge(socket: WebSocket, state: Arc<AppState>, params: WsParams) {
    let (mut sender, mut receiver) = socket.split();
    let requested_last_event_id = params.last_event_id.unwrap_or_default();
    let ready = Arc::new(AtomicBool::new(false));
    let backpressure_warned = Arc::new(AtomicBool::new(false));
    let pending_bytes = Arc::new(AtomicUsize::new(0));

    // 1. 启动 hub (幂等)
    state.global_hub.start();

    // 2. 订阅事件 + 状态
    let mut event_rx = state.global_hub.subscribe_event();
    let mut status_rx = state.global_hub.subscribe_status();

    // 3. 如果 hub 已连接 → markReady
    if state.global_hub.is_connected()
        && !mark_ready(
            &mut sender,
            &ready,
            &backpressure_warned,
            &pending_bytes,
            &state,
            &requested_last_event_id,
        )
        .await
    {
        return; // send 失败, 连接已关闭
    }

    // 4. 心跳定时器
    let mut ping_interval = time::interval(WS_HEARTBEAT_INTERVAL);
    ping_interval.tick().await; // 跳过第一次
    let mut heartbeat_interval = time::interval(WS_HEARTBEAT_INTERVAL);
    heartbeat_interval.tick().await; // 跳过第一次

    // 5. 事件循环
    loop {
        tokio::select! {
            // hub 事件 → 转发给客户端
            event = event_rx.recv() => {
                match event {
                    Ok(hub_event) => {
                        if !ready.load(Ordering::SeqCst) {
                            continue;
                        }
                        if !send_event_frame(
                            &mut sender,
                            &backpressure_warned,
                            &pending_bytes,
                            &hub_event.payload,
                            hub_event.event_id.as_deref(),
                            Some(&hub_event.directory),
                        ).await {
                            return; // send 失败
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(lagged = n, "global WS client lagged, skipping events");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            // hub 状态变更
            status = status_rx.recv() => {
                match status {
                    Ok(HubStatus::Connect { was_ready }) => {
                        if !ready.load(Ordering::SeqCst) {
                            // 首次连接 → markReady
                            if !mark_ready(
                                &mut sender,
                                &ready,
                                &backpressure_warned,
                                &pending_bytes,
                                &state,
                                &requested_last_event_id,
                            ).await {
                                return;
                            }
                        } else if was_ready {
                            // 重连后恢复 → 重发 ready (让浏览器做 scoped repair)
                            let frame = WsFrame::Ready { scope: "global".into() };
                            if sender.send(Message::Text(frame.to_json().into())).await.is_err() {
                                return;
                            }
                        }
                    }
                    Ok(HubStatus::Disconnect { .. }) => {
                        // 静默 — 浏览器靠心跳超时检测
                    }
                    Ok(HubStatus::Error { kind, initial }) => {
                        if initial && !ready.load(Ordering::SeqCst) {
                            // 初始错误 → 关闭客户端
                            let msg = if kind == "upstream_unavailable" {
                                "OpenCode event stream unavailable".to_string()
                            } else {
                                "Failed to connect to OpenCode event stream".to_string()
                            };
                            let frame = WsFrame::Error { message: msg.clone() };
                            let _ = sender.send(Message::Text(frame.to_json().into())).await;
                            let _ = sender.send(Message::Close(Some(CloseFrame {
                                code: 1011,
                                reason: msg.into(),
                            }))).await;
                            return;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => {}
                }
            }
            // ping 定时器
            _ = ping_interval.tick() => {
                if sender.send(Message::Ping(Bytes::new())).await.is_err() {
                    return;
                }
            }
            // synthetic heartbeat 定时器 (仅 hub connected 时)
            _ = heartbeat_interval.tick() => {
                if !state.global_hub.is_connected() || !ready.load(Ordering::SeqCst) {
                    continue;
                }
                let heartbeat_payload = json!({
                    "type": "openchamber:heartbeat",
                    "timestamp": chrono::Utc::now().timestamp_millis(),
                });
                if !send_event_frame(
                    &mut sender,
                    &backpressure_warned,
                    &pending_bytes,
                    &heartbeat_payload,
                    None,
                    Some("global"),
                ).await {
                    return;
                }
            }
            // 客户端消息 (忽略, 但需要检测 close)
            msg = receiver.next() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => break,
                    _ => {}
                }
            }
        }
    }

    // 清理: 发送 close
    let _ = sender.close().await;
}

/// 标记客户端 ready: 发 ready 帧 + replay events。
///
/// 返回 false 表示 send 失败 (连接已断)。
async fn mark_ready(
    sender: &mut SplitSink<WebSocket, Message>,
    ready: &AtomicBool,
    backpressure_warned: &AtomicBool,
    pending_bytes: &AtomicUsize,
    state: &Arc<AppState>,
    requested_last_event_id: &str,
) -> bool {
    let frame = WsFrame::Ready {
        scope: "global".into(),
    };
    if sender
        .send(Message::Text(frame.to_json().into()))
        .await
        .is_err()
    {
        return false;
    }
    ready.store(true, Ordering::SeqCst);

    // Replay events after requestedLastEventId
    if !requested_last_event_id.is_empty() {
        let replay_entries = state.global_hub.replay_after(requested_last_event_id);
        for entry in replay_entries {
            if !send_replay_entry(
                sender,
                backpressure_warned,
                pending_bytes,
                &entry,
            )
            .await
            {
                return false;
            }
        }
    }

    true
}

/// 发送一个 replay entry。
async fn send_replay_entry(
    sender: &mut SplitSink<WebSocket, Message>,
    backpressure_warned: &AtomicBool,
    pending_bytes: &AtomicUsize,
    entry: &ReplayEntry,
) -> bool {
    send_event_frame(
        sender,
        backpressure_warned,
        pending_bytes,
        &entry.payload,
        entry.event_id.as_deref(),
        Some(&entry.directory),
    )
    .await
}

/// 发送一个 event 帧 + 背压检查。
///
/// 返回 false 表示 send 失败或客户端被关闭 (backpressure 超限)。
async fn send_event_frame(
    sender: &mut SplitSink<WebSocket, Message>,
    backpressure_warned: &AtomicBool,
    pending_bytes: &AtomicUsize,
    payload: &serde_json::Value,
    event_id: Option<&str>,
    directory: Option<&str>,
) -> bool {
    let frame = WsFrame::Event {
        payload: payload.clone(),
        event_id: event_id.map(|s| s.to_string()),
        directory: directory.map(|s| s.to_string()),
    };
    let json_str = frame.to_json();
    let msg_bytes = json_str.len();
    pending_bytes.fetch_add(msg_bytes, Ordering::Relaxed);

    // 检查硬上限
    let current = pending_bytes.load(Ordering::Relaxed);
    if current > WS_MAX_BUFFERED_BYTES {
        let _ = sender
            .send(Message::Close(Some(CloseFrame {
                code: 1013,
                reason: "Message stream client is too slow".into(),
            })))
            .await;
        return false;
    }

    // 检查软警告
    if current > WS_BACKPRESSURE_WARN_BYTES && !backpressure_warned.swap(true, Ordering::SeqCst) {
        let bp_frame = WsFrame::Backpressure {
            buffered_bytes: current,
            max_bytes: WS_MAX_BUFFERED_BYTES,
        };
        let _ = sender
            .send(Message::Text(bp_frame.to_json().into()))
            .await;
    } else if current <= WS_BACKPRESSURE_WARN_BYTES && backpressure_warned.load(Ordering::SeqCst) {
        // buffer 已降到阈值以下 → 重置 flag
        backpressure_warned.store(false, Ordering::SeqCst);
    }

    // 发送事件帧
    let result = sender.send(Message::Text(json_str.into())).await;
    pending_bytes.fetch_sub(msg_bytes, Ordering::Relaxed);
    result.is_ok()
}

// =========================================================================
// 目录 WS 桥
// =========================================================================

/// `GET /api/event/ws` handler。
pub async fn directory_ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    Query(params): Query<WsParams>,
) -> Response {
    ws.max_write_buffer_size(WS_MAX_BUFFERED_BYTES)
        .max_message_size(WS_MAX_BUFFERED_BYTES * 2)
        .on_upgrade(move |socket| run_directory_bridge(socket, state, params))
}

/// 从 `session.status` payload 提取合成事件所需的字段。
///
/// 对应 Node `index.js:818-836` (`processForwardedEventPayload` 的解析部分)。
/// 返回 `(session_id, status_type)` 或 `None` (非 session.status / 缺字段)。
fn extract_session_status_for_synthesis(payload: &Value) -> Option<(String, String)> {
    if payload.get("type").and_then(|v| v.as_str()) != Some("session.status") {
        return None;
    }

    let properties = payload.get("properties")?.as_object()?;
    let status = properties.get("status").and_then(|v| v.as_object());
    let info = properties.get("info").and_then(|v| v.as_object());

    let session_id = properties
        .get("sessionID")
        .and_then(|v| v.as_str())?
        .trim();
    if session_id.is_empty() {
        return None;
    }

    // status.type 优先, info.type 回退 (与 SessionStateRuntime 一致)
    let status_type = status
        .and_then(|s| s.get("type"))
        .and_then(|v| v.as_str())
        .or_else(|| info.and_then(|i| i.get("type")).and_then(|v| v.as_str()))?
        .trim()
        .to_string();

    if status_type.is_empty() {
        return None;
    }

    Some((session_id.to_string(), status_type))
}

/// 对 `session.status` 上游事件, 合成 `openchamber:session-status` 和
/// `openchamber:session-activity` 帧发给当前目录 WS 客户端。
///
/// 对应 Node `directory-ws-bridge.js:82` 的 `processForwardedEventPayload(payload, emitSyntheticEvent)`。
/// 非 session.status 事件直接返回 true (无操作)。
/// 返回 false 表示 send 失败 (连接已断)。
async fn emit_synthetic_session_events(
    sender: &mut SplitSink<WebSocket, Message>,
    backpressure_warned: &AtomicBool,
    pending_bytes: &AtomicUsize,
    payload: &Value,
) -> bool {
    let (session_id, status) = match extract_session_status_for_synthesis(payload) {
        Some(v) => v,
        None => return true, // 非 session.status — 无操作
    };

    // 合成 session-status (对应 Node index.js:841-860)
    let session_status_payload = json!({
        "type": "openchamber:session-status",
        "properties": {
            "sessionID": session_id,
            "status": status,
            "timestamp": chrono::Utc::now().timestamp_millis(),
            "metadata": {},
            "needsAttention": false,
        }
    });
    if !send_event_frame(
        sender,
        backpressure_warned,
        pending_bytes,
        &session_status_payload,
        None,
        Some("global"),
    )
    .await
    {
        return false;
    }

    // 合成 session-activity (对应 Node index.js:862-875)
    let phase = if status == "busy" || status == "retry" {
        "busy"
    } else {
        "idle"
    };
    let session_activity_payload = json!({
        "type": "openchamber:session-activity",
        "properties": {
            "sessionId": session_id,
            "phase": phase,
        }
    });
    if !send_event_frame(
        sender,
        backpressure_warned,
        pending_bytes,
        &session_activity_payload,
        None,
        Some("global"),
    )
    .await
    {
        return false;
    }

    true
}

/// 目录 WS 桥连接生命周期。
///
/// 对应 `directory-ws-bridge.js`。每连接独享上游 reader, 无 replay。
async fn run_directory_bridge(socket: WebSocket, state: Arc<AppState>, params: WsParams) {
    let (mut sender, mut receiver) = socket.split();
    let requested_last_event_id = params.last_event_id;
    let requested_directory = params.directory.unwrap_or_default();

    let backpressure_warned = Arc::new(AtomicBool::new(false));
    let pending_bytes = Arc::new(AtomicUsize::new(0));

    // 创建 per-connection 上游 reader
    let base_url = state.opencode_base_url.clone();
    let auth_header = state.opencode_auth_header.clone();
    let http_client = state.http_client.clone();
    let dir_for_url = requested_directory.clone();

    let config = UpstreamReaderConfig {
        build_url: Box::new(move || {
            let mut url = format!("{}/event", base_url.trim_end_matches('/'));
            if !dir_for_url.is_empty() {
                url.push_str("?directory=");
                url.push_str(&dir_for_url);
            }
            url
        }),
        auth_header,
        http_client,
        initial_last_event_id: requested_last_event_id.clone(),
        stall_timeout: UPSTREAM_STALL_TIMEOUT,
        reconnect_delay: UPSTREAM_RECONNECT_DELAY,
    };

    let (reader, mut event_rx) = UpstreamSseReader::new(config);
    let reader = Arc::new(reader);
    reader.start();

    let stream_ready = Arc::new(AtomicBool::new(false));
    let upstream_connected = Arc::new(AtomicBool::new(false));

    // 心跳定时器
    let mut ping_interval = time::interval(WS_HEARTBEAT_INTERVAL);
    ping_interval.tick().await;
    let mut heartbeat_interval = time::interval(WS_HEARTBEAT_INTERVAL);
    heartbeat_interval.tick().await;

    loop {
        tokio::select! {
            // 上游 reader 事件
            event = event_rx.recv() => {
                match event {
                    Some(UpstreamEvent::Connect { .. }) => {
                        upstream_connected.store(true, Ordering::SeqCst);
                        if !stream_ready.swap(true, Ordering::SeqCst) {
                            // 首次连接 → 发 ready
                            let frame = WsFrame::Ready { scope: "directory".into() };
                            if sender.send(Message::Text(frame.to_json().into())).await.is_err() {
                                break;
                            }
                        }
                    }
                    Some(UpstreamEvent::Event { envelope }) => {
                        let dir = if !requested_directory.is_empty() {
                            requested_directory.as_str()
                        } else {
                            envelope.directory.as_deref().unwrap_or("global")
                        };
                        if !send_event_frame(
                            &mut sender,
                            &backpressure_warned,
                            &pending_bytes,
                            &envelope.payload,
                            envelope.event_id.as_deref(),
                            Some(dir),
                        ).await {
                            break;
                        }
                        // 对 session.status 合成 session-status + session-activity 帧
                        // (对应 Node directory-ws-bridge.js:82 processForwardedEventPayload)。
                        // 目录桥用 per-connection 上游 reader, 不走全局 hub, 所以 Part 1 的
                        // broadcast 到不了这里 — 需本地合成。
                        if !emit_synthetic_session_events(
                            &mut sender,
                            &backpressure_warned,
                            &pending_bytes,
                            &envelope.payload,
                        ).await {
                            break;
                        }
                    }
                    Some(UpstreamEvent::Disconnect { .. }) => {
                        upstream_connected.store(false, Ordering::SeqCst);
                    }
                    Some(UpstreamEvent::Error { kind, status }) => {
                        if !stream_ready.load(Ordering::SeqCst) {
                            // 初始错误 → 关闭
                            let msg = if matches!(kind, super::upstream_reader::UpstreamErrorKind::UpstreamUnavailable) {
                                format!("OpenCode event stream unavailable ({})", status.unwrap_or(0))
                            } else {
                                "Failed to connect to OpenCode event stream".to_string()
                            };
                            let frame = WsFrame::Error { message: msg.clone() };
                            let _ = sender.send(Message::Text(frame.to_json().into())).await;
                            let _ = sender.send(Message::Close(Some(CloseFrame {
                                code: 1011,
                                reason: msg.into(),
                            }))).await;
                            break;
                        }
                    }
                    None => break, // reader stopped
                }
            }
            // ping 定时器
            _ = ping_interval.tick() => {
                if sender.send(Message::Ping(bytes::Bytes::new())).await.is_err() {
                    break;
                }
            }
            // synthetic heartbeat (仅 upstream connected 时)
            _ = heartbeat_interval.tick() => {
                if !upstream_connected.load(Ordering::SeqCst) || !stream_ready.load(Ordering::SeqCst) {
                    continue;
                }
                let heartbeat_payload = json!({
                    "type": "openchamber:heartbeat",
                    "timestamp": chrono::Utc::now().timestamp_millis(),
                });
                if !send_event_frame(
                    &mut sender,
                    &backpressure_warned,
                    &pending_bytes,
                    &heartbeat_payload,
                    None,
                    Some("global"),
                ).await {
                    break;
                }
            }
            // 客户端消息
            msg = receiver.next() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => break,
                    _ => {}
                }
            }
        }
    }

    // 清理: 停止上游 reader + 关闭 WS
    reader.stop().await;
    let _ = sender.close().await;
}

// =========================================================================
// 测试
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extract_status_valid_with_status_type() {
        let payload = json!({
            "type": "session.status",
            "properties": {
                "sessionID": "sess-1",
                "status": { "type": "busy", "attempt": 2 }
            }
        });
        let result = extract_session_status_for_synthesis(&payload).unwrap();
        assert_eq!(result.0, "sess-1");
        assert_eq!(result.1, "busy");
    }

    #[test]
    fn extract_status_uses_info_type_fallback() {
        let payload = json!({
            "type": "session.status",
            "properties": {
                "sessionID": "sess-2",
                "info": { "type": "retry" }
            }
        });
        let result = extract_session_status_for_synthesis(&payload).unwrap();
        assert_eq!(result.0, "sess-2");
        assert_eq!(result.1, "retry");
    }

    #[test]
    fn extract_status_non_session_status_returns_none() {
        let payload = json!({ "type": "message.updated" });
        assert!(extract_session_status_for_synthesis(&payload).is_none());
    }

    #[test]
    fn extract_status_missing_session_id_returns_none() {
        let payload = json!({
            "type": "session.status",
            "properties": { "status": { "type": "busy" } }
        });
        assert!(extract_session_status_for_synthesis(&payload).is_none());
    }

    #[test]
    fn extract_status_missing_type_returns_none() {
        let payload = json!({
            "type": "session.status",
            "properties": { "sessionID": "sess-3" }
        });
        assert!(extract_session_status_for_synthesis(&payload).is_none());
    }

    #[test]
    fn extract_status_empty_session_id_returns_none() {
        let payload = json!({
            "type": "session.status",
            "properties": {
                "sessionID": "  ",
                "status": { "type": "idle" }
            }
        });
        assert!(extract_session_status_for_synthesis(&payload).is_none());
    }
}
