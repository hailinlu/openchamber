//! Dictation 路由 — 1 WS handler + 4 HTTP handler。
//!
//! 对应 Node `dictation/runtime.js` (278 LOC)。
//!
//! WS handler 镜像 `terminal::routes::terminal_ws_handler` 模式:
//! `WebSocketUpgrade` → `on_upgrade(run_dictation_bridge)`。
//!
//! Auth: `/api/dictation/ws` 已在 `ui_auth/types.rs:149` WS 白名单, 注册路由
//! 即自动获得全局 auth 中间件覆盖 (UI session token 或 oc_url_token + origin
//! 校验)。4 个 HTTP 路由走标准 UI auth。

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::state::AppState;

use super::service::SynthesizeResult;
use super::stream_manager::{
    CreateSttOutcome, DictationStreamManager, ManagerOutput, StartOptions,
};

// =========================================================================
// 1. GET /api/dictation/status
// =========================================================================

#[derive(Deserialize, Default)]
pub struct StatusQuery {
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default, rename = "localModel")]
    pub local_model: Option<String>,
}

pub async fn get_status_handler(
    State(state): State<Arc<AppState>>,
    Query(q): Query<StatusQuery>,
) -> Response {
    let svc = &state.dictation_service;
    match svc.get_status(q.provider.as_deref(), q.local_model.as_deref()).await {
        Value::Null => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "Failed to read dictation status" })),
        )
            .into_response(),
        v => Json(v).into_response(),
    }
}

// =========================================================================
// 2. POST /api/dictation/tts/speak
// =========================================================================

#[derive(Deserialize)]
pub struct TtsSpeakBody {
    pub text: String,
    /// 以下字段由客户端发送, 但本轮 local TTS 不支持, 暂不使用。
    #[serde(default)]
    #[allow(dead_code)]
    pub model: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub speaker_id: Option<i64>,
    #[serde(default)]
    #[allow(dead_code)]
    pub speed: Option<f32>,
}

pub async fn post_tts_speak_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<TtsSpeakBody>,
) -> Response {
    let text = body.text.trim().to_string();
    if text.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "Text is required" })),
        )
            .into_response();
    }
    // 本轮 local TTS 不支持; service.synthesize_speech 恒返回 Error
    match state.dictation_service.synthesize_speech().await {
        SynthesizeResult::Audio { audio, format } => {
            let mut headers = axum::http::HeaderMap::new();
            headers.insert(
                axum::http::header::CONTENT_TYPE,
                axum::http::HeaderValue::from_str(&format).unwrap_or_else(|_| {
                    axum::http::HeaderValue::from_static("audio/wav")
                }),
            );
            (StatusCode::OK, headers, audio).into_response()
        }
        SynthesizeResult::Error {
            error,
            retryable,
            reason_code,
        } => {
            let mut v = json!({ "error": error, "retryable": retryable });
            if let Some(rc) = reason_code {
                v["reasonCode"] = json!(rc);
            }
            (StatusCode::SERVICE_UNAVAILABLE, Json(v)).into_response()
        }
    }
}

// =========================================================================
// 3. POST /api/dictation/models/{model_id}/download
// =========================================================================

pub async fn post_model_download_handler(
    State(state): State<Arc<AppState>>,
    Path(model_id): Path<String>,
) -> Response {
    let result = state
        .dictation_service
        .request_model_download(&model_id)
        .await;
    if !result.ok {
        return (StatusCode::BAD_REQUEST, Json(result.to_json())).into_response();
    }
    Json(result.to_json()).into_response()
}

// =========================================================================
// 4. DELETE /api/dictation/models/{model_id}
// =========================================================================

pub async fn delete_model_handler(
    State(state): State<Arc<AppState>>,
    Path(model_id): Path<String>,
) -> Response {
    let result = state
        .dictation_service
        .clone()
        .delete_model(&model_id)
        .await;
    if !result.ok {
        return (
            StatusCode::BAD_REQUEST,
            Json(result.to_json()),
        )
            .into_response();
    }
    Json(result.to_json()).into_response()
}

// =========================================================================
// 5. WS /api/dictation/ws
// =========================================================================

/// WS 升级 handler (对应 Node `wsServer.on('connection')`)。
pub async fn dictation_ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> Response {
    ws.max_message_size(super::DICTATION_WS_MAX_PAYLOAD_BYTES * 2)
        .max_write_buffer_size(super::DICTATION_WS_MAX_PAYLOAD_BYTES)
        .on_upgrade(move |socket| run_dictation_bridge(socket, state))
}

/// WS 连接生命周期 (对应 Node runtime.js:124-223)。
///
/// 双向: WS 接收 → manager 方法; manager 输出 channel → WS 发送。
/// finalize 超时由 manager 的 `earliest_finalize_deadline()` 驱动。
async fn run_dictation_bridge(socket: WebSocket, state: Arc<AppState>) {
    let (mut sender, mut receiver) = socket.split();

    // manager 输出通道
    let (emit_tx, mut emit_rx) = mpsc::unbounded_channel::<ManagerOutput>();

    // 构造 create_stt_session 工厂 (闭包捕获 DictationService clone)
    let svc = state.dictation_service.clone();
    let factory = move |opts: StartOptions| {
        let svc = svc.clone();
        Box::pin(async move { svc.create_stt_session(opts).await })
            as std::pin::Pin<Box<dyn std::future::Future<Output = CreateSttOutcome> + Send>>
    };

    let mut manager = DictationStreamManager::new(
        emit_tx,
        factory,
        super::DEFAULT_FINAL_TIMEOUT_MS,
        super::DEFAULT_AUTO_COMMIT_SECONDS,
    );

    // 握手: 发 {type:"ready"}
    if sender
        .send(Message::Text(json!({ "type": "ready" }).to_string().into()))
        .await
        .is_err()
    {
        return;
    }

    let mut heartbeat = tokio::time::interval(Duration::from_millis(
        super::DICTATION_WS_HEARTBEAT_INTERVAL_MS,
    ));
    heartbeat.tick().await; // 跳过首次立即触发

    // finalize 超时检查间隔 (较短, 以便及时触发)
    let mut finalize_check = tokio::time::interval(Duration::from_millis(500));

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
                    Message::Text(text) => {
                        if !handle_ws_text(text.as_str(), &mut manager).await {
                            break;
                        }
                    }
                    Message::Binary(_) => {
                        // 二进制帧忽略 (对齐 Node runtime.js:155)
                        continue;
                    }
                    Message::Ping(p) => {
                        let _ = sender.send(Message::Pong(p)).await;
                    }
                    Message::Pong(_) => {}
                    Message::Close(_) => break,
                }
            }
            // manager 输出 → WS 发送
            Some(output) = emit_rx.recv() => {
                let json_str = serialize_manager_output(&output);
                if sender.send(Message::Text(json_str.into())).await.is_err() {
                    break;
                }
            }
            // 心跳 (WS-level ping)
            _ = heartbeat.tick() => {
                if sender.send(Message::Ping(vec![].into())).await.is_err() {
                    break;
                }
            }
            // finalize 超时检查
            _ = finalize_check.tick() => {
                check_finalize_timeouts(&mut manager).await;
            }
        }
    }

    // 清理所有流
    manager.cleanup_all();
    // 排空剩余输出 (尽力发送)
    while let Ok(output) = emit_rx.try_recv() {
        let json_str = serialize_manager_output(&output);
        let _ = sender.send(Message::Text(json_str.into())).await;
    }
    let _ = sender.send(Message::Close(None)).await;
}

/// 处理一条 WS 文本消息 (JSON)。返回 false 表示应断开连接。
async fn handle_ws_text<F>(
    text: &str,
    manager: &mut DictationStreamManager<F>,
) -> bool
where
    F: Fn(StartOptions) -> std::pin::Pin<Box<dyn std::future::Future<Output = CreateSttOutcome> + Send>>
        + Send
        + Sync,
{
    let msg: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return true, // 无效 JSON → 忽略 (对齐 Node)
    };
    let msg_type = msg.get("type").and_then(|v| v.as_str()).unwrap_or("");
    match msg_type {
        "start" => {
            let dictation_id = match msg.get("dictationId").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => return true,
            };
            let format = match msg.get("format").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => return true,
            };
            let options = parse_start_options(&msg);
            manager.handle_start(dictation_id, &format, options).await;
            true
        }
        "chunk" => {
            let dictation_id = match msg.get("dictationId").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => return true,
            };
            let seq = match msg.get("seq").and_then(|v| v.as_i64()) {
                Some(n) if n >= 0 => n as u32,
                _ => return true,
            };
            let audio = match msg.get("audio").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => return true,
            };
            manager.handle_chunk(&dictation_id, seq, &audio);
            true
        }
        "finish" => {
            let dictation_id = match msg.get("dictationId").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => return true,
            };
            let final_seq = match msg.get("finalSeq").and_then(|v| v.as_i64()) {
                Some(n) => n,
                None => return true,
            };
            manager.handle_finish(&dictation_id, final_seq);
            true
        }
        "cancel" => {
            let dictation_id = match msg.get("dictationId").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => return true,
            };
            manager.handle_cancel(&dictation_id);
            true
        }
        "ping" => {
            manager.handle_ping();
            true
        }
        _ => true,
    }
}

/// 从 `start` 消息解析 options (对应 Node message.options)。
fn parse_start_options(msg: &Value) -> StartOptions {
    let options = msg.get("options").unwrap_or(&Value::Null);
    let provider = options
        .get("provider")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let language = options
        .get("language")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let local_model = options
        .get("localModel")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let openai_compatible = options
        .get("openaiCompatible")
        .and_then(|v| v.as_object())
        .map(|o| super::stream_manager::OpenAiCompatibleConfig {
            base_url: o
                .get("baseUrl")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            model: o
                .get("model")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            api_key: o
                .get("apiKey")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
        });
    StartOptions {
        provider,
        language,
        local_model,
        openai_compatible,
    }
}

/// 检查并触发已到期的 finalize 超时。
async fn check_finalize_timeouts<F>(manager: &mut DictationStreamManager<F>)
where
    F: Fn(StartOptions) -> std::pin::Pin<Box<dyn std::future::Future<Output = CreateSttOutcome> + Send>>
        + Send
        + Sync,
{
    // 收集已超期的 dictation_id (避免 borrow 冲突)
    let now = std::time::Instant::now();
    let expired: Vec<String> = {
        let deadline = manager.earliest_finalize_deadline();
        match deadline {
            Some((id, d)) if d <= now => vec![id.to_string()],
            _ => Vec::new(),
        }
    };
    for id in expired {
        manager.on_finalize_timeout(&id);
    }
}

/// 序列化 manager 输出为 WS 文本帧 JSON (对应 server→client 消息)。
fn serialize_manager_output(output: &ManagerOutput) -> String {
    match output {
        ManagerOutput::Ack {
            dictation_id,
            ack_seq,
        } => json!({
            "type": "ack",
            "dictationId": dictation_id,
            "ackSeq": ack_seq,
        }),
        ManagerOutput::Partial { dictation_id, text } => json!({
            "type": "partial",
            "dictationId": dictation_id,
            "text": text,
        }),
        ManagerOutput::FinishAccepted {
            dictation_id,
            timeout_ms,
        } => json!({
            "type": "finish_accepted",
            "dictationId": dictation_id,
            "timeoutMs": timeout_ms,
        }),
        ManagerOutput::Final { dictation_id, text } => json!({
            "type": "final",
            "dictationId": dictation_id,
            "text": text,
        }),
        ManagerOutput::Error {
            dictation_id,
            error,
            retryable,
            reason_code,
        } => {
            let mut v = json!({
                "type": "error",
                "dictationId": dictation_id,
                "error": error,
                "retryable": retryable,
            });
            if let Some(rc) = reason_code {
                v["reasonCode"] = json!(rc);
            }
            v
        }
        ManagerOutput::Pong => json!({ "type": "pong" }),
    }
    .to_string()
}
