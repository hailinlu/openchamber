//! `POST /api/text/summarize` handler。
//!
//! 对应现有: `packages/web/server/lib/tts/routes.js` line 106-129。
//!
//! 契约:
//!   - 请求: `{ text: string, threshold?: number, maxLength?: number, mode?: string }`
//!   - 成功: 200, summarize_text() 的返回
//!   - 空文本: 400 `{ error: "Text is required" }`
//!   - 异常: 200 `{ summary, summarized: false, reason }` (不是错误码!)

use axum::extract::Json;
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{json, Value};

use super::summarization::{sanitize_for_note, sanitize_for_tts, summarize_text, SummarizeResult};

/// 请求体 (对应 JS 的 `req.body` 解构)。
#[derive(Deserialize, Debug)]
pub struct SummarizeRequest {
    pub text: Option<String>,
    #[serde(default = "default_threshold")]
    pub threshold: usize,
    #[serde(default = "default_max_length", rename = "maxLength")]
    pub max_length: usize,
    pub mode: Option<String>,
}

fn default_threshold() -> usize {
    200
}

fn default_max_length() -> usize {
    500
}

/// `POST /api/text/summarize`
pub async fn summarize(Json(body): Json<SummarizeRequest>) -> impl IntoResponse {
    let text = body.text.as_deref().unwrap_or("");
    let mode = body.mode.as_deref().unwrap_or("tts");

    // 验证: text 非空
    if text.trim().is_empty() {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({ "error": "Text is required" })),
        )
            .into_response();
    }

    // summarize_text 是纯函数, 不会 panic
    let result: SummarizeResult = summarize_text(text, body.threshold, body.max_length, None, mode);
    Json(json!(result)).into_response()
}

/// 构建错误路径的降级响应。
///
/// 对应 JS catch 块:
/// ```js
/// const sanitized = mode === 'note' ? sanitizeForNote(text) : sanitizeForTTS(text);
/// return res.json({ summary: sanitized, summarized: false, reason: error.message });
/// ```
#[allow(dead_code)]
pub fn error_fallback(text: &str, mode: &str, error_message: &str) -> Value {
    let sanitized = if mode == "note" {
        sanitize_for_note(text)
    } else {
        sanitize_for_tts(text)
    };
    json!({
        "summary": sanitized,
        "summarized": false,
        "reason": error_message,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    async fn call_handler(body: Value) -> (axum::http::StatusCode, Value) {
        use axum::routing::post;
        let app = axum::Router::new().route("/summarize", post(summarize));

        let req = Request::builder()
            .method("POST")
            .uri("/summarize")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let body_bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body_bytes).unwrap_or(Value::Null);
        (status, json)
    }

    #[tokio::test]
    async fn empty_text_returns_400() {
        let (status, body) = call_handler(json!({ "text": "" })).await;
        assert_eq!(status, 400);
        assert_eq!(body["error"], "Text is required");
    }

    #[tokio::test]
    async fn missing_text_returns_400() {
        let (status, _body) = call_handler(json!({})).await;
        assert_eq!(status, 400);
    }

    #[tokio::test]
    async fn valid_text_returns_summary() {
        let (status, body) = call_handler(json!({ "text": "hello", "mode": "tts" })).await;
        assert_eq!(status, 200);
        assert_eq!(body["summary"], "hello");
        assert_eq!(body["summarized"], false);
        assert_eq!(body["reason"], "Text under threshold");
        // originalLength / summaryLength 在阈值以下不出现
        assert!(body.get("originalLength").is_none());
        assert!(body.get("summaryLength").is_none());
    }
}
