//! Terminal 路由 — 7 REST handler + 1 WS handler + SSE stream。
//!
//! 对应 Node `runtime.js` 的路由注册 + WS server + SSE fallback。

use std::sync::Arc;

use axum::body::Body;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use bytes::Bytes;
use futures_util::stream;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::time;

use crate::state::AppState;

use super::pty::{build_pty_env, KillMode, PtyExit, PtyOutput, TerminalPty};
use super::session::{generate_session_id, TerminalSession};
use super::protocol::{
    create_control_frame, is_rebind_rate_limited, prune_rebind_timestamps, read_control_frame,
};
use super::{
    MAX_TERMINAL_SESSIONS, TERMINAL_OUTPUT_REPLAY_MAX_BYTES, TERMINAL_SSE_HEARTBEAT_INTERVAL_MS,
    TERMINAL_WS_HEARTBEAT_INTERVAL_MS, TERMINAL_WS_MAX_INVALID_FRAMES, TERMINAL_WS_MAX_PAYLOAD_BYTES,
    TERMINAL_WS_MAX_REBINDS_PER_WINDOW, TERMINAL_WS_REBIND_WINDOW_MS,
};

/// 传输能力声明 (对齐 Node `terminalTransportCapabilities`)。
fn transport_capabilities() -> Value {
    json!({
        "input": {
            "preferred": "ws",
            "transports": ["http", "ws"],
            "ws": { "path": super::TERMINAL_WS_PATH, "v": 2, "enc": "text+json-bin-control" }
        },
        "stream": {
            "preferred": "ws",
            "transports": ["sse", "ws"],
            "ws": { "path": super::TERMINAL_WS_PATH, "v": 2, "enc": "text+json-bin-control" }
        }
    })
}

/// 运行时名 (对齐 Node `terminalRuntimeName`)。
const RUNTIME_NAME: &str = "rust";

// =========================================================================
// 1. POST /api/terminal/create
// =========================================================================

#[derive(Deserialize)]
pub struct CreateBody {
    pub cwd: String,
    #[serde(default)]
    pub cols: Option<u16>,
    #[serde(default)]
    pub rows: Option<u16>,
}

/// 创建终端会话。
pub async fn create(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateBody>,
) -> Response {
    let store = &state.terminal_sessions;
    if store.len().await >= MAX_TERMINAL_SESSIONS {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({ "error": "Maximum terminal sessions reached" })),
        )
            .into_response();
    }

    let cwd = std::path::PathBuf::from(&body.cwd);
    if !cwd.is_dir() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "Invalid working directory" })),
        )
            .into_response();
    }

    let cols = body.cols.unwrap_or(80);
    let rows = body.rows.unwrap_or(24);

    let env = build_pty_env(cols, rows);
    let pty = match TerminalPty::spawn(&cwd, cols, rows, &env) {
        Ok(p) => p,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    let session = Arc::new(TerminalSession::new(pty, cwd.clone()));
    let session_id = generate_session_id();
    tracing::info!("Created terminal session: {}", session_id);
    store.insert(session_id.clone(), session).await;

    Json(json!({
        "sessionId": session_id,
        "cols": cols,
        "rows": rows,
        "capabilities": transport_capabilities(),
    }))
    .into_response()
}

// =========================================================================
// 2. GET /api/terminal/{sessionId}/stream (SSE)
// =========================================================================

/// SSE 输出流回退。
pub async fn stream(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
) -> Response {
    let session = match state.terminal_sessions.get(&session_id).await {
        Some(s) => s,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Terminal session not found" })),
            )
                .into_response();
        }
    };

    session.touch().await;
    let pty_backend = session.pty_backend.clone();

    let mut output_rx = session.pty.subscribe_output();
    let mut exit_rx = session.pty.subscribe_exit();

    let (tx, stream_rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(64);

    // 连接事件
    let connected = Bytes::from(format!(
        "data: {}\n\n",
        json!({ "type": "connected", "runtime": RUNTIME_NAME, "ptyBackend": pty_backend })
    ));
    let _ = tx.send(Ok(connected)).await;

    // 转发 task: PTY 输出 → SSE data 帧
    let tx2 = tx.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                output = output_rx.recv() => {
                    match output {
                        Ok(PtyOutput { data }) => {
                            let frame = Bytes::from(format!(
                                "data: {}\n\n",
                                json!({ "type": "data", "data": data })
                            ));
                            if tx2.send(Ok(frame)).await.is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                exit = exit_rx.recv() => {
                    if let Ok(PtyExit { exit_code, signal }) = exit {
                        let frame = Bytes::from(format!(
                            "data: {}\n\n",
                            json!({ "type": "exit", "exitCode": exit_code, "signal": signal })
                        ));
                        let _ = tx2.send(Ok(frame)).await;
                        break;
                    }
                }
            }
        }
    });

    // 心跳 task
    let (heartbeat_tx, heartbeat_rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(4);
    tokio::spawn(async move {
        let mut interval = time::interval(std::time::Duration::from_millis(
            TERMINAL_SSE_HEARTBEAT_INTERVAL_MS,
        ));
        interval.tick().await;
        loop {
            interval.tick().await;
            let hb = Bytes::from_static(b": heartbeat\n\n");
            if heartbeat_tx.send(Ok(hb)).await.is_err() {
                break;
            }
        }
    });

    let output_stream = tokio_stream::wrappers::ReceiverStream::new(stream_rx);
    let heartbeat_stream = tokio_stream::wrappers::ReceiverStream::new(heartbeat_rx);
    let merged = stream::select(output_stream, heartbeat_stream);
    let body = Body::from_stream(merged);

    let mut response = Response::new(body);
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        "content-type",
        "text/event-stream; charset=utf-8".parse().unwrap(),
    );
    response
        .headers_mut()
        .insert("cache-control", "no-cache".parse().unwrap());
    response
        .headers_mut()
        .insert("connection", "keep-alive".parse().unwrap());
    response
        .headers_mut()
        .insert("x-accel-buffering", "no".parse().unwrap());
    response
}

// =========================================================================
// 3. POST /api/terminal/{sessionId}/input
// =========================================================================

/// HTTP 输入写入。body 为纯文本 (`Content-Type: */*`)。
pub async fn input(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    body: String,
) -> Response {
    let session = match state.terminal_sessions.get(&session_id).await {
        Some(s) => s,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Terminal session not found" })),
            )
                .into_response();
        }
    };

    match session.pty.write(&body).await {
        Ok(()) => {
            session.touch().await;
            Json(json!({ "success": true })).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

// =========================================================================
// 4. POST /api/terminal/{sessionId}/resize
// =========================================================================

#[derive(Deserialize)]
pub struct ResizeBody {
    pub cols: u16,
    pub rows: u16,
}

/// 调整窗口大小。
pub async fn resize(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    Json(body): Json<ResizeBody>,
) -> Response {
    let session = match state.terminal_sessions.get(&session_id).await {
        Some(s) => s,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Terminal session not found" })),
            )
                .into_response();
        }
    };

    match session.pty.resize(body.cols, body.rows).await {
        Ok(()) => {
            session.touch().await;
            Json(json!({ "success": true, "cols": body.cols, "rows": body.rows })).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

// =========================================================================
// 5. DELETE /api/terminal/{sessionId}
// =========================================================================

/// 关闭会话 (SIGTERM 进程组)。
pub async fn delete(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
) -> Response {
    let session = match state.terminal_sessions.remove(&session_id).await {
        Some(s) => s,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Terminal session not found" })),
            )
                .into_response();
        }
    };

    session.pty.kill_process_group(KillMode::Term);
    tracing::info!("Closed terminal session: {}", session_id);
    Json(json!({ "success": true })).into_response()
}

// =========================================================================
// 6. POST /api/terminal/{sessionId}/restart
// =========================================================================

#[derive(Deserialize)]
pub struct RestartBody {
    pub cwd: String,
    #[serde(default)]
    pub cols: Option<u16>,
    #[serde(default)]
    pub rows: Option<u16>,
}

/// 重启会话: 杀旧建新。
pub async fn restart(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    Json(body): Json<RestartBody>,
) -> Response {
    // 杀旧会话 (忽略不存在)
    if let Some(old) = state.terminal_sessions.remove(&session_id).await {
        old.pty.kill_process_group(KillMode::Term);
    }

    let cwd = std::path::PathBuf::from(&body.cwd);
    if !cwd.is_dir() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "Invalid working directory: not accessible" })),
        )
            .into_response();
    }

    let cols = body.cols.unwrap_or(80);
    let rows = body.rows.unwrap_or(24);
    let env = build_pty_env(cols, rows);
    let pty = match TerminalPty::spawn(&cwd, cols, rows, &env) {
        Ok(p) => p,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    let session = Arc::new(TerminalSession::new(pty, cwd));
    let new_session_id = generate_session_id();
    state
        .terminal_sessions
        .insert(new_session_id.clone(), session)
        .await;

    Json(json!({
        "sessionId": new_session_id,
        "cols": cols,
        "rows": rows,
        "capabilities": transport_capabilities(),
    }))
    .into_response()
}

// =========================================================================
// 7. POST /api/terminal/force-kill
// =========================================================================

#[derive(Deserialize)]
pub struct ForceKillBody {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
}

/// 批量杀会话 (by sessionId / by cwd / all)。
pub async fn force_kill(
    State(state): State<Arc<AppState>>,
    Json(body): Json<ForceKillBody>,
) -> Response {
    let killed = if let Some(ref session_id) = body.session_id {
        if let Some(session) = state.terminal_sessions.remove(session_id).await {
            session.pty.kill_process_group(KillMode::Kill);
            1
        } else {
            0
        }
    } else if let Some(ref cwd) = body.cwd {
        let mut count = 0;
        let mut to_remove = Vec::new();
        {
            let sessions = state.terminal_sessions.sessions_for_sweep().await;
            for (id, session) in &sessions {
                if session.cwd.as_path() == std::path::Path::new(cwd.as_str()) {
                    session.pty.kill_process_group(KillMode::Kill);
                    to_remove.push(id.clone());
                    count += 1;
                }
            }
        }
        for id in &to_remove {
            state.terminal_sessions.remove(id).await;
        }
        count
    } else {
        let sessions = state.terminal_sessions.sessions_for_sweep().await;
        let count = sessions.len();
        for session in sessions.values() {
            session.pty.kill_process_group(KillMode::Kill);
        }
        state.terminal_sessions.clear().await;
        count
    };

    tracing::info!("Force killed {} terminal session(s)", killed);
    Json(json!({ "success": true, "killedCount": killed })).into_response()
}

// =========================================================================
// 8. WS /api/terminal/ws
// =========================================================================

/// WS 升级 handler。
pub async fn terminal_ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> Response {
    ws.max_message_size(TERMINAL_WS_MAX_PAYLOAD_BYTES * 2)
        .write_buffer_size(TERMINAL_WS_MAX_PAYLOAD_BYTES)
        .max_write_buffer_size(TERMINAL_WS_MAX_PAYLOAD_BYTES * 2)
        .on_upgrade(move |socket| run_terminal_bridge(socket, state))
}

/// WS 连接生命周期: 双向 I/O + 控制帧 + 回放。
///
/// 对应 Node `runtime.js` 的 `terminalInputWsServer.on('connection')` 事件循环。
async fn run_terminal_bridge(socket: WebSocket, state: Arc<AppState>) {
    let (mut sender, mut receiver) = socket.split();

    // 连接状态 (对齐 Node `connectionState`)
    let mut bound_session_id: Option<String> = None;
    let mut invalid_frames: u32 = 0;
    let mut rebind_timestamps: Vec<u128> = Vec::new();
    let mut replay_cursor_by_session: std::collections::HashMap<String, u64> =
        std::collections::HashMap::new();

    // 握手帧 {t:"ok",v:2}
    let ok_frame = create_control_frame(&json!({"t": "ok", "v": 2}));
    if sender.send(Message::Binary(ok_frame.into())).await.is_err() {
        return;
    }

    let mut heartbeat = time::interval(std::time::Duration::from_millis(
        TERMINAL_WS_HEARTBEAT_INTERVAL_MS,
    ));
    heartbeat.tick().await; // 跳过第一次

    // 当前绑定的 session + 输出/退出 receiver (bind 时设置)
    let mut output_rx: Option<tokio::sync::broadcast::Receiver<PtyOutput>> = None;
    let mut exit_rx: Option<tokio::sync::broadcast::Receiver<PtyExit>> = None;
    let mut bound_session: Option<Arc<TerminalSession>> = None;

    loop {
        tokio::select! {
            // WS 接收
            msg = receiver.next() => {
                let Some(msg) = msg else { break; };
                let msg = match msg {
                    Ok(m) => m,
                    Err(_) => break,
                };

                match msg {
                    Message::Binary(bin) => {
                        let now_ms = now_millis();
                        let control = read_control_frame(&bin);
                        let Some(control) = control else {
                            invalid_frames += 1;
                            let _ = send_control(&mut sender, &json!({
                                "t": "e", "c": "BAD_FRAME",
                                "f": invalid_frames >= TERMINAL_WS_MAX_INVALID_FRAMES,
                            })).await;
                            if invalid_frames >= TERMINAL_WS_MAX_INVALID_FRAMES {
                                let _ = sender.send(Message::Close(Some(CloseFrame {
                                    code: 1008,
                                    reason: "protocol violation".into(),
                                }))).await;
                                break;
                            }
                            continue;
                        };

                        let t = control.get("t").and_then(|v| v.as_str()).unwrap_or("");
                        match t {
                            // ping → pong
                            "p" => {
                                let _ = send_control(&mut sender, &json!({"t": "po", "v": 2})).await;
                            }
                            // bind session
                            "b" => {
                                let session_id = control.get("s").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
                                if session_id.is_empty() {
                                    invalid_frames += 1;
                                    let _ = send_control(&mut sender, &json!({
                                        "t": "e", "c": "BAD_FRAME", "f": false,
                                    })).await;
                                    continue;
                                }

                                // 速率限制
                                rebind_timestamps = prune_rebind_timestamps(
                                    &rebind_timestamps, now_ms, TERMINAL_WS_REBIND_WINDOW_MS,
                                );
                                if is_rebind_rate_limited(
                                    rebind_timestamps.len(), TERMINAL_WS_MAX_REBINDS_PER_WINDOW,
                                ) {
                                    let _ = send_control(&mut sender, &json!({
                                        "t": "e", "c": "RATE_LIMIT", "f": false,
                                    })).await;
                                    continue;
                                }

                                // 查找 session
                                let Some(session) = state.terminal_sessions.get(&session_id).await else {
                                    bound_session_id = None;
                                    bound_session = None;
                                    output_rx = None;
                                    exit_rx = None;
                                    let _ = send_control(&mut sender, &json!({
                                        "t": "e", "c": "SESSION_NOT_FOUND", "f": false,
                                    })).await;
                                    continue;
                                };

                                let replay_since_raw = control.get("r").and_then(|v| v.as_u64()).unwrap_or(0);
                                let remembered = replay_cursor_by_session.get(&session_id).copied().unwrap_or(0);
                                let replay_since = replay_since_raw.max(remembered);

                                rebind_timestamps.push(now_ms);
                                bound_session_id = Some(session_id.clone());
                                bound_session = Some(session.clone());
                                output_rx = Some(session.pty.subscribe_output());
                                exit_rx = Some(session.pty.subscribe_exit());

                                let _ = send_control(&mut sender, &json!({
                                    "t": "bok", "v": 2, "s": session_id,
                                    "runtime": RUNTIME_NAME,
                                    "ptyBackend": session.pty_backend,
                                })).await;

                                // 回放
                                let chunks = session.replay_buffer.list_since(replay_since);
                                for chunk in chunks {
                                    let frame = create_control_frame(&json!({
                                        "t": "d", "s": bound_session_id.clone().unwrap_or_default(),
                                        "i": chunk.id, "d": chunk.data,
                                    }));
                                    if sender.send(Message::Binary(frame.into())).await.is_err() {
                                        break;
                                    }
                                    replay_cursor_by_session.insert(session_id.clone(), chunk.id);
                                }

                                let _ = session.touch().await;
                            }
                            _ => {
                                invalid_frames += 1;
                                let _ = send_control(&mut sender, &json!({
                                    "t": "e", "c": "BAD_FRAME", "f": false,
                                })).await;
                            }
                        }
                    }
                    Message::Text(text) => {
                        // 终端输入
                        if text.is_empty() { continue; }
                        let Some(ref session) = bound_session else {
                            let _ = send_control(&mut sender, &json!({
                                "t": "e", "c": "NOT_BOUND", "f": false,
                            })).await;
                            continue;
                        };
                        if session.pty.write(&text).await.is_err() {
                            let _ = send_control(&mut sender, &json!({
                                "t": "e", "c": "WRITE_FAIL", "f": false,
                            })).await;
                        }
                        let _ = session.touch().await;
                    }
                    Message::Close(_) => break,
                    Message::Ping(_) | Message::Pong(_) => {}
                }
            }
            // PTY 输出 → 发 d 帧
            output = async {
                if let Some(ref mut rx) = output_rx {
                    rx.recv().await
                } else {
                    std::future::pending::<Result<PtyOutput, _>>().await
                }
            } => {
                let session_id = match &bound_session_id {
                    Some(s) => s.clone(),
                    None => continue,
                };
                match output {
                    Ok(PtyOutput { data }) => {
                        let session = match &bound_session {
                            Some(s) => s.clone(),
                            None => continue,
                        };
                        let chunk = session.replay_buffer.append(
                            &data, TERMINAL_OUTPUT_REPLAY_MAX_BYTES,
                        );
                        if let Some(ref chunk) = chunk {
                            let frame = create_control_frame(&json!({
                                "t": "d", "s": session_id, "i": chunk.id, "d": data,
                            }));
                            if sender.send(Message::Binary(frame.into())).await.is_err() {
                                break;
                            }
                            replay_cursor_by_session.insert(session_id.clone(), chunk.id);
                        } else {
                            // 空 chunk (data 为空或超大 trim 后空), 仍发不带 id 的 d 帧
                            let frame = create_control_frame(&json!({
                                "t": "d", "s": session_id, "d": data,
                            }));
                            if sender.send(Message::Binary(frame.into())).await.is_err() {
                                break;
                            }
                        }
                    }
                    Err(_) => { /* reader 关闭, 不再收输出 */ }
                }
            }
            // PTY 退出 → 发 x 帧
            exit = async {
                if let Some(ref mut rx) = exit_rx {
                    rx.recv().await
                } else {
                    std::future::pending::<Result<PtyExit, _>>().await
                }
            } => {
                let session_id = bound_session_id.clone();
                if let Ok(PtyExit { exit_code, signal }) = exit {
                    let frame = create_control_frame(&json!({
                        "t": "x", "v": 2, "s": session_id,
                        "exitCode": exit_code, "signal": signal,
                    }));
                    let _ = sender.send(Message::Binary(frame.into())).await;
                    // 清除绑定 (session 可能已被 store 删除)
                    bound_session_id = None;
                    bound_session = None;
                    output_rx = None;
                    exit_rx = None;
                }
            }
            // 心跳
            _ = heartbeat.tick() => {
                let _ = sender.send(Message::Ping(Bytes::new())).await;
            }
        }
    }

    // 清理
    let _ = sender.close().await;
}

/// 发送控制帧辅助。
async fn send_control(
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    payload: &Value,
) -> Result<(), axum::Error> {
    let frame = create_control_frame(payload);
    sender.send(Message::Binary(frame.into())).await
}

/// 当前时间毫秒。
fn now_millis() -> u128 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}
