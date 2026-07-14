//! SSE 透传代理 — `/api/event`, `/api/global/event`。
//!
//! 对应 `packages/web/server/lib/opencode/proxy.js` 的 `forwardSseRequest`。
//!
//! 与 WS 桥不同: 这条路径不解析 SSE, 不追踪 event ID,
//! 只做纯 chunk 透传 + 边界感知心跳 + 背压写。
//!
//! 心跳格式: SSE comment `:heartbeat\n\n` (不是 event)。
//! 边界检查: 只在 buffer 尾部处于 `\n\n` 事件边界时才插入心跳,
//! 避免切割正在传输中的事件。

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::IntoResponse;
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::json;
use tokio::sync::mpsc;
use tokio::time;

use crate::state::AppState;

use super::{SSE_BOUNDARY_TAIL_LIMIT, SSE_HEARTBEAT_INTERVAL};

/// SSE 透传代理 handler。
///
/// 处理 `GET /api/event` 和 `GET /api/global/event`。
/// 从 OpenCode 上游流式读取 SSE chunks, 透传给客户端, 定期插入心跳。
pub async fn sse_proxy_handler(
    State(state): State<std::sync::Arc<AppState>>,
    req: Request<Body>,
) -> Response {
    let (parts, _body) = req.into_parts();

    // 1. 检查 OpenCode 就绪
    if !state
        .opencode_ready
        .load(std::sync::atomic::Ordering::Relaxed)
    {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(json!({ "error": "OpenCode is starting", "restarting": true })),
        )
            .into_response();
    }

    // 2. 构造上游 URL
    //    parts.uri.path() 是完整路径 (例如 /api/global/event)
    //    去掉 /api 前缀得到上游路径
    let full_path = parts.uri.path();
    let upstream_path = if let Some(rest) = full_path.strip_prefix("/api") {
        if rest.is_empty() {
            "/"
        } else {
            rest
        }
    } else {
        full_path
    };
    let base = state.opencode_base_url.trim_end_matches('/');
    let mut target_url = format!("{}{}", base, upstream_path);
    if let Some(query) = parts.uri.query() {
        target_url.push('?');
        target_url.push_str(query);
    }

    // 3. 构造请求头
    let mut req_headers = reqwest::header::HeaderMap::new();
    req_headers.insert(
        reqwest::header::ACCEPT,
        reqwest::header::HeaderValue::from_static("text/event-stream"),
    );
    req_headers.insert(
        reqwest::header::CACHE_CONTROL,
        reqwest::header::HeaderValue::from_static("no-cache"),
    );
    if let Ok(auth_val) = reqwest::header::HeaderValue::from_str(&state.opencode_auth_header) {
        req_headers.insert(reqwest::header::AUTHORIZATION, auth_val);
    }
    // 透传 x-opencode-directory 等目录相关头
    for (name, value) in parts.headers.iter() {
        let name_str = name.as_str();
        if name_str.starts_with("x-opencode-") {
            req_headers.append(name.clone(), value.clone());
        }
    }
    // 透传 Last-Event-ID (resume 支持)
    if let Some(last_id) = parts.headers.get("last-event-id") {
        req_headers.insert("last-event-id", last_id.clone());
    }

    // 4. 发起上游请求 (无超时 — SSE 是长连接)
    let upstream_resp = match state.http_client.get(&target_url).headers(req_headers).send().await {
        Ok(resp) => resp,
        Err(e) => {
            tracing::error!(error = %e, url = %target_url, "SSE upstream request failed");
            return (
                StatusCode::BAD_GATEWAY,
                axum::Json(json!({ "error": format!("upstream error: {}", e) })),
            )
                .into_response();
        }
    };

    let upstream_status = upstream_resp.status();
    let upstream_content_type = upstream_resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("text/event-stream")
        .to_string();

    // 非 2xx → 直接返回上游状态 + body
    if !upstream_status.is_success() {
        let body_text = upstream_resp.text().await.unwrap_or_default();
        return (
            StatusCode::from_u16(upstream_status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
            body_text,
        )
            .into_response();
    }

    // 非 SSE → 透传 body
    let is_event_stream = upstream_content_type
        .to_lowercase()
        .contains("text/event-stream");
    if !is_event_stream {
        let body_text = upstream_resp.text().await.unwrap_or_default();
        return (StatusCode::OK, body_text).into_response();
    }

    // 5. SSE 流: 构造响应头
    let mut resp_headers = HeaderMap::new();
    resp_headers.insert(
        "content-type",
        upstream_content_type
            .parse()
            .unwrap_or_else(|_| "text/event-stream".parse().unwrap()),
    );
    resp_headers.insert(
        "cache-control",
        "no-cache".parse().unwrap(),
    );
    resp_headers.insert(
        "x-accel-buffering",
        "no".parse().unwrap(),
    );

    // 6. 流式透传 + 心跳
    //    用 mpsc channel 作为 Body 的 stream source:
    //    - 数据 chunk 和心跳都推入 channel
    //    - 单消费者 (axum/hyper) 写
    //    - 客户端断开 → channel drop → 上游 stream drop
    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(64);

    // 上游 stream
    let mut upstream_stream = upstream_resp.bytes_stream();
    let abort_token = tokio_util::sync::CancellationToken::new();

    // 透传 task: 读上游 → 推入 channel + 心跳定时器
    let abort_child = abort_token.child_token();
    tokio::spawn(async move {
        let mut boundary = SseBoundaryTracker::new();
        let mut heartbeat_timer = time::interval(SSE_HEARTBEAT_INTERVAL);
        heartbeat_timer.tick().await; // 立即返回 (跳过第一次)

        loop {
            tokio::select! {
                _ = abort_child.cancelled() => break,
                chunk = upstream_stream.next() => {
                    match chunk {
                        Some(Ok(bytes)) => {
                            if !bytes.is_empty() {
                                boundary.observe(&bytes);
                                if tx.send(Ok(bytes)).await.is_err() {
                                    // 客户端断开
                                    break;
                                }
                            }
                        }
                        Some(Err(e)) => {
                            tracing::warn!(error = %e, "SSE upstream stream error");
                            break;
                        }
                        None => break, // 上游关闭
                    }
                }
                _ = heartbeat_timer.tick() => {
                    // 边界检查: 只在事件边界插入心跳
                    if boundary.is_at_boundary() {
                        let heartbeat = Bytes::from_static(b":heartbeat\n\n");
                        if tx.send(Ok(heartbeat)).await.is_err() {
                            break;
                        }
                    }
                    // 不在边界 → 跳过本次心跳, 等下一个 interval
                }
            }
        }
    });

    // 构造响应 Body: mpsc::Receiver → Stream → Body
    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    let body = Body::from_stream(stream);

    let mut response = axum::response::Response::new(body);
    *response.status_mut() = StatusCode::OK;
    *response.headers_mut() = resp_headers;
    response
}

type Response = axum::response::Response;

/// SSE 边界跟踪器 — 追踪 buffer 尾部是否处于 `\n\n` 事件边界。
///
/// 对应 `proxy.js` 的 `createSseBoundaryTracker`。
/// 只保留尾部最多 `SSE_BOUNDARY_TAIL_LIMIT` 字符, 避免内存增长。
struct SseBoundaryTracker {
    tail: String,
}

impl SseBoundaryTracker {
    fn new() -> Self {
        Self {
            tail: String::new(),
        }
    }

    /// 观察新 chunk, 追加到尾部 buffer。
    fn observe(&mut self, chunk: &[u8]) {
        let text = String::from_utf8_lossy(chunk);
        self.tail.push_str(&text);
        // 保留尾部
        if self.tail.len() > SSE_BOUNDARY_TAIL_LIMIT {
            let start = self.tail.len() - SSE_BOUNDARY_TAIL_LIMIT;
            self.tail = self.tail[start..].to_string();
        }
    }

    /// 当前是否处于事件边界 (buffer 以 `\n\n` 结尾或为空)。
    fn is_at_boundary(&self) -> bool {
        self.tail.is_empty() || self.tail.ends_with("\n\n")
    }
}

// =========================================================================
// 测试
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boundary_tracker_empty_is_at_boundary() {
        let tracker = SseBoundaryTracker::new();
        assert!(tracker.is_at_boundary());
    }

    #[test]
    fn boundary_tracker_ends_with_double_newline() {
        let mut tracker = SseBoundaryTracker::new();
        tracker.observe(b"data: {\"test\":1}\n\n");
        assert!(tracker.is_at_boundary());
    }

    #[test]
    fn boundary_tracker_partial_event_not_at_boundary() {
        let mut tracker = SseBoundaryTracker::new();
        tracker.observe(b"data: {\"test\":1}\n");
        assert!(!tracker.is_at_boundary());
    }

    #[test]
    fn boundary_tracker_multiple_chunks() {
        let mut tracker = SseBoundaryTracker::new();
        tracker.observe(b"data: {\"a\":1}\n\n");
        assert!(tracker.is_at_boundary());
        tracker.observe(b"data: {\"b\":2}");
        assert!(!tracker.is_at_boundary());
        tracker.observe(b"\n\n");
        assert!(tracker.is_at_boundary());
    }

    #[test]
    fn boundary_tracker_truncates_tail() {
        let mut tracker = SseBoundaryTracker::new();
        // 推入超过 SSE_BOUNDARY_TAIL_LIMIT 的数据
        let big = "x".repeat(SSE_BOUNDARY_TAIL_LIMIT + 1000);
        tracker.observe(big.as_bytes());
        // 尾部应该被截断
        assert_eq!(tracker.tail.len(), SSE_BOUNDARY_TAIL_LIMIT);
        // 被截断后不以 \n\n 结尾, 不在边界
        assert!(!tracker.is_at_boundary());
    }

    #[test]
    fn boundary_tracker_preserves_boundary_at_tail_limit() {
        let mut tracker = SseBoundaryTracker::new();
        // 填充到接近上限, 然后加 \n\n
        let fill = "x".repeat(SSE_BOUNDARY_TAIL_LIMIT - 2);
        tracker.observe(fill.as_bytes());
        tracker.observe(b"\n\n");
        assert!(tracker.is_at_boundary());
    }
}
