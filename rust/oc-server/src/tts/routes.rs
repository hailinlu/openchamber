//! TTS 路由 — 6 个 axum handler。
//!
//! 对应 Node `tts/routes.js` (`registerTtsRoutes`, 265 行):
//!   1. POST /api/voice/token
//!   2. POST /api/tts/speak            (raw audio bytes)
//!   3. GET  /api/tts/status
//!   4. GET  /api/tts/say/status       (cached capability)
//!   5. POST /api/tts/say/speak        (raw audio bytes, macOS only)
//!   6. POST /api/stt/transcribe       (raw audio bytes → { transcript })
//!
//! 设计要点:
//!   - 所有 handler 接收 `State<Arc<AppState>>`
//!   - 二进制返回 (audio) 用 `axum::response::Response` 直接拼 headers + body
//!   - JSON 错误用 `ApiResult<Json<Value>>`

use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use bytes::Bytes;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

use super::capability_runtime::SayTtsCapability;
use super::service::{SpeechOptions, TtsService, TTS_VOICES};
use super::stt::{transcribe_audio, TranscribeOptions};

// =========================================================================
// 1. POST /api/voice/token
// =========================================================================

/// `POST /api/voice/token` — 报告 TTS 是否可用 (env `OPENAI_API_KEY` 检查)。
///
/// 对应 Node `routes.js` line 14-42。
pub async fn post_voice_token() -> ApiResult<Json<Value>> {
    let openai_api_key = std::env::var("OPENAI_API_KEY").ok();

    if openai_api_key
        .as_ref()
        .map(|s| !s.is_empty())
        .unwrap_or(false)
    {
        Ok(Json(json!({
            "allowed": true,
            "provider": "openai",
            "message": "OpenAI TTS is available",
        })))
    } else {
        // 503 + JSON 错误体 — 与 Node 一致
        let body = json!({
            "allowed": false,
            "error": "OpenAI voice service not configured. Set OPENAI_API_KEY environment variable."
        });
        Err(ApiError(oc_core::Error::Internal(
            serde_json::to_string(&body).unwrap_or_else(|_| "{}".to_string()),
        )))
    }
}

/// 变体: 不通过 ApiError, 而是直接返回带 503 的 Response (handler 内部决策 status)。
pub async fn post_voice_token_response() -> Response {
    let openai_api_key = std::env::var("OPENAI_API_KEY").ok();
    let has_key = openai_api_key
        .as_ref()
        .map(|s| !s.is_empty())
        .unwrap_or(false);

    if has_key {
        (StatusCode::OK, Json(json!({
            "allowed": true,
            "provider": "openai",
            "message": "OpenAI TTS is available",
        })))
            .into_response()
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "allowed": false,
                "error": "OpenAI voice service not configured. Set OPENAI_API_KEY environment variable."
            }),
        ))
            .into_response()
    }
}

// =========================================================================
// 2. POST /api/tts/speak
// =========================================================================

/// `POST /api/tts/speak` body。
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TtsSpeakBody {
    pub text: Option<String>,
    pub voice: Option<String>,
    pub model: Option<String>,
    pub speed: Option<f64>,
    pub instructions: Option<String>,
    pub provider_id: Option<String>,
    pub model_id: Option<String>,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
}

/// `POST /api/tts/speak` — 返回 MP3 (`audio/mpeg`) bytes。
///
/// 对应 Node `routes.js` line 45-104。
pub async fn post_tts_speak(
    State(state): State<Arc<AppState>>,
    Json(body): Json<TtsSpeakBody>,
) -> Result<Response, Response> {
    // baseURL 校验
    let normalized = match super::base_url::normalize_custom_openai_base_url(
        body.base_url.as_deref().unwrap_or(""),
    ) {
        Ok(v) => v,
        Err(e) => return Err(error_json(StatusCode::BAD_REQUEST, e)),
    };

    // text 校验
    let text = body.text.as_deref().unwrap_or("").trim();
    if text.is_empty() {
        return Err(error_json(StatusCode::BAD_REQUEST, "Text is required".to_string()));
    }

    // 可用性: server key, client key, 或 custom baseURL
    let has_server_key = state.tts_service.is_available();
    let has_client_key = body
        .api_key
        .as_deref()
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    let has_custom_base = normalized.is_some();

    if !has_server_key && !has_client_key && !has_custom_base {
        return Err(error_json(
            StatusCode::SERVICE_UNAVAILABLE,
            "TTS service not available. Please configure OpenAI in OpenCode, provide an API key, or set a custom server URL in settings.".to_string(),
        ));
    }

    // 客户端提供的 key/baseURL → trim
    let trimmed_client_key = if has_client_key {
        body.api_key.as_ref().map(|s| s.trim().to_string())
    } else {
        None
    };

    let speech_opts = SpeechOptions {
        text: text.to_string(),
        voice: body.voice.clone(),
        model: body.model.clone(),
        speed: body.speed,
        instructions: body.instructions.clone(),
        api_key: trimmed_client_key,
        base_url: normalized.clone(),
    };

    match state.tts_service.generate_speech_stream(speech_opts).await {
        Ok(result) => Ok(audio_response(
            result.content_type,
            result.buffer,
        )),
        Err(e) => {
            tracing::error!("[TTS] Error: {}", e);
            Err(error_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to generate speech: {}", e),
            ))
        }
    }
}

// =========================================================================
// 3. GET /api/tts/status
// =========================================================================

/// `GET /api/tts/status` — 报告 TTS 可用性 + voices 列表。
///
/// 对应 Node `routes.js` line 132-146。
pub async fn get_tts_status(State(state): State<Arc<AppState>>) -> ApiResult<Json<Value>> {
    Ok(Json(json!({
        "available": state.tts_service.is_available(),
        "voices": TTS_VOICES,
    })))
}

// =========================================================================
// 4. GET /api/tts/say/status
// =========================================================================

/// `GET /api/tts/say/status` — 返回 cached `say_tts_capability`。
///
/// 对应 Node `routes.js` line 148-151。
pub async fn get_tts_say_status(State(state): State<Arc<AppState>>) -> Json<Value> {
    let cap = state.say_tts_capability.read().await.clone();
    Json(serde_json::to_value(&cap).unwrap_or(Value::Null))
}

// =========================================================================
// 5. POST /api/tts/say/speak
// =========================================================================

/// `POST /api/tts/say/speak` body。
#[derive(Debug, Default, Deserialize)]
pub struct SaySpeakBody {
    pub text: Option<String>,
    pub voice: Option<String>,
    pub rate: Option<u32>,
}

/// `POST /api/tts/say/speak` — macOS `say` 命令, 返回 AAC (`audio/mp4`)。
///
/// 对应 Node `routes.js` line 153-206。
pub async fn post_tts_say_speak(
    Json(body): Json<SaySpeakBody>,
) -> Result<Response, Response> {
    let text = body.text.as_deref().unwrap_or("").trim();
    if text.is_empty() {
        return Err(error_json(StatusCode::BAD_REQUEST, "Text is required".to_string()));
    }

    if std::env::consts::OS != "darwin" {
        return Err(error_json(
            StatusCode::SERVICE_UNAVAILABLE,
            "macOS say command not available on this platform".to_string(),
        ));
    }

    let voice = body.voice.unwrap_or_else(|| "Samantha".to_string());
    let rate = body.rate.unwrap_or(200);

    // 生成 temp 文件: $TMPDIR/say-{ts}.m4a
    let temp_dir = std::env::temp_dir();
    let temp_file = temp_dir.join(format!("say-{}.m4a", chrono::Utc::now().timestamp_millis()));

    // 转义: shell 中单引号和双引号都处理
    // JS: text.replace(/'/g, "'\\''").replace(/"/g, '\\"')
    let escaped_text = text
        .replace('\'', "'\\''")
        .replace('"', "\\\"");

    let cmd_str = format!(
        "say -v \"{}\" -r {} -o \"{}\" --data-format=aac '{}'",
        voice,
        rate,
        temp_file.display(),
        escaped_text
    );

    tracing::info!(
        "[TTS-Say] Generating speech: textLength={}, voice={}, rate={}",
        text.len(),
        voice,
        rate
    );

    // 走 tokio::process::Command (windowsHide cross-platform safety, Unix no-op)
    let status = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(&cmd_str)
        .status()
        .await;

    match status {
        Ok(s) if s.success() => {
            // 读 + 清理
            let buffer = match std::fs::read(&temp_file) {
                Ok(b) => b,
                Err(e) => {
                    let _ = std::fs::remove_file(&temp_file);
                    return Err(error_json(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("Failed to read say output: {}", e),
                    ));
                }
            };
            let _ = std::fs::remove_file(&temp_file);
            Ok(audio_response("audio/mp4".to_string(), buffer))
        }
        Ok(s) => Err(error_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("say command exited with {}", s),
        )),
        Err(e) => Err(error_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("say command failed: {}", e),
        )),
    }
}

// =========================================================================
// 6. POST /api/stt/transcribe
// =========================================================================

/// `POST /api/stt/transcribe` — raw audio bytes → `{ transcript }`。
///
/// 对应 Node `routes.js` line 208-264。
pub async fn post_stt_transcribe(
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, Response> {
    let mime_type = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("audio/webm")
        .split(',')
        .next()
        .unwrap_or("audio/webm")
        .trim()
        .to_string();

    let base_url = headers
        .get("x-base-url")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();

    let model = headers
        .get("x-model")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "deepdml/faster-whisper-large-v3-turbo-ct2".to_string());

    let language = headers
        .get("x-language")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let api_key = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| {
            let trimmed = s.trim();
            trimmed.strip_prefix("Bearer ").map(|k| k.trim().to_string())
        });

    if body.is_empty() {
        return Err(error_json(
            StatusCode::BAD_REQUEST,
            "Audio data is required".to_string(),
        ));
    }

    if base_url.is_empty() {
        return Err(error_json(
            StatusCode::BAD_REQUEST,
            "X-Base-URL header is required".to_string(),
        ));
    }

    tracing::info!(
        "[STT] Transcribing audio: bytes={}, mime={}, model={}, baseURL={}, language={:?}, hasApiKey={}",
        body.len(),
        mime_type,
        model,
        base_url,
        language,
        api_key.is_some()
    );

    let result = transcribe_audio(TranscribeOptions {
        audio_buffer: body.to_vec(),
        mime_type,
        model,
        base_url,
        api_key,
        language,
    })
    .await;

    match result {
        Ok(transcript) => {
            tracing::info!("[STT] Transcript: {}", transcript.chars().take(120).collect::<String>());
            Ok(Json(json!({ "transcript": transcript })))
        }
        Err(e) => Err(error_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Transcription failed: {}", e),
        )),
    }
}

// =========================================================================
// 辅助
// =========================================================================

/// 拼一个 JSON 错误响应 (与 Node `res.status(...).json({ error })` 一致)。
fn error_json(status: StatusCode, error: impl Into<String>) -> Response {
    (
        status,
        Json(json!({ "error": error.into() })),
    )
        .into_response()
}

/// 拼一个 binary audio 响应 (Content-Type + Content-Length + body)。
fn audio_response(content_type: String, buffer: Vec<u8>) -> Response {
    let len = buffer.len();
    let mut resp = Response::new(Body::from(buffer));
    *resp.status_mut() = StatusCode::OK;
    let headers = resp.headers_mut();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        content_type.parse().unwrap_or_else(|_| "application/octet-stream".parse().unwrap()),
    );
    headers.insert(
        axum::http::header::CONTENT_LENGTH,
        len.to_string().parse().unwrap(),
    );
    resp
}

// =========================================================================
// 测试
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use axum::body::to_bytes;
    use axum::http::Request;
    use clap::Parser;
    use tower::ServiceExt;

    fn test_state() -> Arc<AppState> {
        let config = Config::try_parse_from(["oc-server"]).unwrap();
        Arc::new(AppState::new(
            config,
            "http://127.0.0.1:4096".to_string(),
            "Basic test".to_string(),
        ))
    }

    async fn body_json(resp: Response) -> (StatusCode, Value) {
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, json)
    }

    #[tokio::test]
    async fn post_tts_speak_empty_text_returns_400() {
        let state = test_state();
        let app = axum::Router::new()
            .route("/api/tts/speak", axum::routing::post(post_tts_speak))
            .with_state(state);

        let req = Request::builder()
            .method("POST")
            .uri("/api/tts/speak")
            .header("content-type", "application/json")
            .body(Body::from(json!({ "text": "" }).to_string()))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        let (status, body) = body_json(resp).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "Text is required");
    }

    #[tokio::test]
    async fn post_tts_speak_invalid_base_url_returns_400() {
        let state = test_state();
        let app = axum::Router::new()
            .route("/api/tts/speak", axum::routing::post(post_tts_speak))
            .with_state(state);

        let req = Request::builder()
            .method("POST")
            .uri("/api/tts/speak")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({
                    "text": "hello",
                    "baseUrl": "https://user:pass@example.com/v1"
                })
                .to_string(),
            ))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        let (status, body) = body_json(resp).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"]
            .as_str()
            .unwrap_or("")
            .contains("must not include credentials"));
    }

    #[tokio::test]
    async fn post_stt_transcribe_missing_base_url_returns_400() {
        let app = axum::Router::new()
            .route("/api/stt/transcribe", axum::routing::post(post_stt_transcribe));

        let req = Request::builder()
            .method("POST")
            .uri("/api/stt/transcribe")
            .header("content-type", "audio/webm")
            .body(Body::from(vec![0u8; 16]))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        let (status, body) = body_json(resp).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "X-Base-URL header is required");
    }

    #[tokio::test]
    async fn post_stt_transcribe_empty_body_returns_400() {
        let app = axum::Router::new()
            .route("/api/stt/transcribe", axum::routing::post(post_stt_transcribe));

        let req = Request::builder()
            .method("POST")
            .uri("/api/stt/transcribe")
            .header("content-type", "audio/webm")
            .header("x-base-url", "http://localhost:9999")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        let (status, body) = body_json(resp).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "Audio data is required");
    }

    #[tokio::test]
    async fn get_tts_status_returns_voices() {
        let state = test_state();
        let app = axum::Router::new()
            .route("/api/tts/status", axum::routing::get(get_tts_status))
            .with_state(state);

        let req = Request::builder()
            .method("GET")
            .uri("/api/tts/status")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        let (status, body) = body_json(resp).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["voices"].as_array().unwrap().len(), 13);
        assert!(body["voices"].as_array().unwrap().iter().any(|v| v == "coral"));
    }

    #[tokio::test]
    async fn get_tts_say_status_returns_default_when_uninitialized() {
        let state = test_state();
        let app = axum::Router::new()
            .route("/api/tts/say/status", axum::routing::get(get_tts_say_status))
            .with_state(state);

        let req = Request::builder()
            .method("GET")
            .uri("/api/tts/say/status")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        let (status, body) = body_json(resp).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["available"], false);
        assert_eq!(body["reason"], "Not initialized");
    }

    #[tokio::test]
    async fn post_tts_say_speak_on_non_macos_returns_503() {
        // CI 是 linux/macos 但 handler 走 std::env::consts::OS — 强行检查。
        // 不在 darwin 上跑时直接 503。
        if std::env::consts::OS == "darwin" {
            return;
        }
        let app = axum::Router::new()
            .route("/api/tts/say/speak", axum::routing::post(post_tts_say_speak));

        let req = Request::builder()
            .method("POST")
            .uri("/api/tts/say/speak")
            .header("content-type", "application/json")
            .body(Body::from(json!({ "text": "hello" }).to_string()))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        let (status, body) = body_json(resp).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(body["error"]
            .as_str()
            .unwrap_or("")
            .contains("not available on this platform"));
    }

    #[tokio::test]
    async fn post_voice_token_missing_key_returns_503() {
        // 清空 env
        let prev = std::env::var("OPENAI_API_KEY").ok();
        std::env::remove_var("OPENAI_API_KEY");

        let resp = post_voice_token_response().await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);

        let (_, body) = body_json(resp).await;
        assert_eq!(body["allowed"], false);
        assert!(body["error"].as_str().unwrap().contains("OPENAI_API_KEY"));

        // 还原
        match prev {
            Some(v) => std::env::set_var("OPENAI_API_KEY", v),
            None => {}
        }
    }

    #[tokio::test]
    async fn post_voice_token_with_key_returns_200() {
        let prev = std::env::var("OPENAI_API_KEY").ok();
        std::env::set_var("OPENAI_API_KEY", "sk-test");

        let resp = post_voice_token_response().await;
        assert_eq!(resp.status(), StatusCode::OK);

        let (_, body) = body_json(resp).await;
        assert_eq!(body["allowed"], true);
        assert_eq!(body["provider"], "openai");

        match prev {
            Some(v) => std::env::set_var("OPENAI_API_KEY", v),
            None => std::env::remove_var("OPENAI_API_KEY"),
        }
    }
}