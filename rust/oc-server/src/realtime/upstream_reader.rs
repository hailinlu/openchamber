//! 上游 SSE reader — 可重用的 OpenCode SSE 消费者。
//!
//! 对应 `packages/web/server/lib/event-stream/upstream-reader.js`。
//!
//! 核心不变量:
//! - **stall 检测**: 每收到 chunk 重置 stall timer; 超时 → abort 当前 fetch + 无声重连
//!   (stall 不调 `on_error`, 是恢复路径, 不是故障)
//! - **Last-Event-ID 跨重连持久**: `last_event_id` 在 reader 生命周期内保持,
//!   每次重连都带 `Last-Event-ID` 请求头
//! - **幂等 start**: 重复调用 `start()` 不创建新 task
//! - **可中断重连等待**: `reconnect_delay` 可被 `stop()` 中断

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use futures_util::StreamExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::protocol::{parse_sse_block, SseEnvelope};

/// 上游 reader 发出的事件 (通过 mpsc channel 传递给调用者)。
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum UpstreamEvent {
    /// 上游 SSE 连接成功建立。
    Connect {
        last_event_id: Option<String>,
    },
    /// 上游连接断开 (stall / closed / stopped)。
    Disconnect {
        reason: String,
    },
    /// 解析到一个 SSE 事件。
    Event {
        envelope: SseEnvelope,
    },
    /// 上游错误 (非 stall)。
    Error {
        kind: UpstreamErrorKind,
        status: Option<u16>,
    },
}

#[derive(Debug, Clone)]
pub enum UpstreamErrorKind {
    /// 上游不可用 (非 2xx 响应)。
    UpstreamUnavailable,
    /// 流读取错误 (网络中断等)。
    StreamError,
}

/// 上游 SSE reader 配置。
pub struct UpstreamReaderConfig {
    /// 构建 SSE 请求 URL (每次重连调用, 可能返回不同 URL)。
    pub build_url: Box<dyn Fn() -> String + Send + Sync>,
    /// Authorization header value (例如 `Basic <base64>`)。
    pub auth_header: String,
    /// HTTP client (复用连接池)。
    pub http_client: reqwest::Client,
    /// 初始 Last-Event-ID (用于 resume)。
    pub initial_last_event_id: Option<String>,
    /// stall 超时 (收到 chunk 后多久没新数据则判定 stall)。
    pub stall_timeout: Duration,
    /// 重连延迟。
    pub reconnect_delay: Duration,
}

/// 上游 SSE reader。
///
/// 使用 `start()` 启动后台 task, 通过 `subscribe()` 获取事件流。
/// `stop()` 取消所有 task 并等待退出。
pub struct UpstreamSseReader {
    config: Arc<UpstreamReaderConfig>,
    last_event_id: Mutex<Option<String>>,
    cancel_token: CancellationToken,
    task: Mutex<Option<JoinHandle<()>>>,
    event_tx: mpsc::UnboundedSender<UpstreamEvent>,
}

impl UpstreamSseReader {
    /// 创建 reader (尚未启动)。
    pub fn new(config: UpstreamReaderConfig) -> (Self, mpsc::UnboundedReceiver<UpstreamEvent>) {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let last_event_id = config.initial_last_event_id.clone();

        let reader = Self {
            config: Arc::new(config),
            last_event_id: Mutex::new(last_event_id),
            cancel_token: CancellationToken::new(),
            task: Mutex::new(None),
            event_tx,
        };

        (reader, event_rx)
    }

    /// 启动 reader (幂等: 已启动则返回)。
    pub fn start(self: &Arc<Self>) {
        let mut task_slot = self.task.lock().unwrap();
        if task_slot.is_some() {
            return;
        }

        let this = self.clone();
        let handle = tokio::spawn(async move {
            this.run_loop().await;
        });
        *task_slot = Some(handle);
    }

    /// 停止 reader 并等待后台 task 退出。
    pub async fn stop(&self) {
        self.cancel_token.cancel();

        let handle = { self.task.lock().unwrap().take() };
        if let Some(handle) = handle {
            let _ = handle.await;
        }
    }

    /// 获取当前 Last-Event-ID。
    pub fn last_event_id(&self) -> Option<String> {
        self.last_event_id.lock().unwrap().clone()
    }

    /// 主循环: 连接 → 流式读取 → 断开 → 延迟 → 重连。
    async fn run_loop(&self) {
        while !self.cancel_token.is_cancelled() {
            let result = self.connect_and_stream().await;

            match result {
                StreamOutcome::Stopped => break,
                StreamOutcome::Stalled => {
                    // stall 是无声恢复路径: 不发 Error, 只发 Disconnect
                    let _ = self
                        .event_tx
                        .send(UpstreamEvent::Disconnect {
                            reason: "upstream_stalled".into(),
                        });
                }
                StreamOutcome::Closed => {
                    let _ = self
                        .event_tx
                        .send(UpstreamEvent::Disconnect {
                            reason: "closed".into(),
                        });
                }
                StreamOutcome::Error(kind, status) => {
                    let _ = self.event_tx.send(UpstreamEvent::Error { kind, status });
                    let _ = self
                        .event_tx
                        .send(UpstreamEvent::Disconnect {
                            reason: "error".into(),
                        });
                }
            }

            // 等待重连延迟 (可被 stop 中断)
            if !self.cancel_token.is_cancelled() {
                tokio::select! {
                    _ = tokio::time::sleep(self.config.reconnect_delay) => {}
                    _ = self.cancel_token.cancelled() => {
                        break;
                    }
                }
            }
        }
    }

    /// 单次连接 + 流式读取。
    async fn connect_and_stream(&self) -> StreamOutcome {
        let url = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            (self.config.build_url)()
        })) {
            Ok(url) => url,
            Err(_) => {
                return StreamOutcome::Error(UpstreamErrorKind::StreamError, None);
            }
        };

        // 构造请求头
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::ACCEPT,
            reqwest::header::HeaderValue::from_static("text/event-stream"),
        );
        headers.insert(
            reqwest::header::CACHE_CONTROL,
            reqwest::header::HeaderValue::from_static("no-cache"),
        );
        if let Ok(auth_val) = reqwest::header::HeaderValue::from_str(&self.config.auth_header) {
            headers.insert(reqwest::header::AUTHORIZATION, auth_val);
        }
        if let Some(ref eid) = self.last_event_id() {
            if let Ok(val) = reqwest::header::HeaderValue::from_str(eid) {
                headers.insert("last-event-id", val);
            }
        }

        // 每次尝试用自己的 cancel token (外层 stop 也会取消它)
        let attempt_token = self.cancel_token.child_token();

        // 发起请求 (无全局 timeout — SSE 是长连接)
        let req = self.config.http_client.get(&url).headers(headers);
        let resp = tokio::select! {
            r = req.send() => match r {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(error = %e, url = %url, "upstream SSE request failed");
                    return StreamOutcome::Error(UpstreamErrorKind::StreamError, None);
                }
            },
            _ = attempt_token.cancelled() => return StreamOutcome::Stopped,
        };

        let status = resp.status();
        if !status.is_success() {
            tracing::warn!(status = %status, url = %url, "upstream SSE non-2xx");
            return StreamOutcome::Error(UpstreamErrorKind::UpstreamUnavailable, Some(status.as_u16()));
        }

        // 发送 Connect 事件
        let eid = self.last_event_id();
        let _ = self.event_tx.send(UpstreamEvent::Connect {
            last_event_id: eid.clone(),
        });

        // 流式读取
        let mut stream = resp.bytes_stream();
        let mut buffer = String::new();
        let stall_timer = tokio::time::sleep(self.config.stall_timeout);
        tokio::pin!(stall_timer);

        loop {
            tokio::select! {
                _ = &mut stall_timer => {
                    // stall: abort 当前读取, 无声重连
                    tracing::debug!("upstream SSE stalled, reconnecting");
                    return StreamOutcome::Stalled;
                }
                chunk = stream.next() => {
                    match chunk {
                        Some(Ok(bytes)) => {
                            // 重置 stall timer
                            stall_timer.as_mut().reset(tokio::time::Instant::now() + self.config.stall_timeout);

                            // 追加到 buffer, 按 \n\n 分割
                            let text = String::from_utf8_lossy(&bytes).replace("\r\n", "\n");
                            buffer.push_str(&text);

                            // 处理所有完整事件块
                            while let Some(pos) = buffer.find("\n\n") {
                                let block: String = buffer[..pos].to_string();
                                buffer = buffer[pos + 2..].to_string();

                                if let Some(envelope) = parse_sse_block(&block) {
                                    // 更新 last_event_id
                                    if let Some(ref eid) = envelope.event_id {
                                        if !eid.is_empty() {
                                            *self.last_event_id.lock().unwrap() = Some(eid.clone());
                                        }
                                    }
                                    let _ = self.event_tx.send(UpstreamEvent::Event {
                                        envelope,
                                    });
                                }
                            }
                        }
                        Some(Err(e)) => {
                            tracing::warn!(error = %e, "upstream SSE stream error");
                            return StreamOutcome::Error(UpstreamErrorKind::StreamError, None);
                        }
                        None => {
                            // 流结束 — flush 残余 buffer
                            let remaining = buffer.trim();
                            if !remaining.is_empty() {
                                if let Some(envelope) = parse_sse_block(remaining) {
                                    if let Some(ref eid) = envelope.event_id {
                                        if !eid.is_empty() {
                                            *self.last_event_id.lock().unwrap() = Some(eid.clone());
                                        }
                                    }
                                    let _ = self.event_tx.send(UpstreamEvent::Event {
                                        envelope,
                                    });
                                }
                            }
                            return StreamOutcome::Closed;
                        }
                    }
                }
                _ = attempt_token.cancelled() => return StreamOutcome::Stopped,
            }
        }
    }
}

/// 单次连接的流式读取结果。
enum StreamOutcome {
    /// 被 stop() 取消。
    Stopped,
    /// stall 超时 (无声重连)。
    Stalled,
    /// 上游正常关闭流。
    Closed,
    /// 上游错误 (非 stall)。
    Error(UpstreamErrorKind, Option<u16>),
}

// =========================================================================
// 测试
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reader_starts_with_initial_last_event_id() {
        let (reader, _rx) = UpstreamSseReader::new(UpstreamReaderConfig {
            build_url: Box::new(|| "http://localhost/test".into()),
            auth_header: "Basic test".into(),
            http_client: reqwest::Client::new(),
            initial_last_event_id: Some("evt-initial".into()),
            stall_timeout: Duration::from_secs(20),
            reconnect_delay: Duration::from_millis(250),
        });
        assert_eq!(reader.last_event_id(), Some("evt-initial".into()));
    }

    #[test]
    fn reader_starts_with_none_last_event_id() {
        let (reader, _rx) = UpstreamSseReader::new(UpstreamReaderConfig {
            build_url: Box::new(|| "http://localhost/test".into()),
            auth_header: "Basic test".into(),
            http_client: reqwest::Client::new(),
            initial_last_event_id: None,
            stall_timeout: Duration::from_secs(20),
            reconnect_delay: Duration::from_millis(250),
        });
        assert!(reader.last_event_id().is_none());
    }

    #[tokio::test]
    async fn stop_cancels_reader() {
        let (reader, _rx) = UpstreamSseReader::new(UpstreamReaderConfig {
            build_url: Box::new(|| "http://127.0.0.1:1/nonexistent".into()),
            auth_header: "Basic test".into(),
            http_client: reqwest::Client::builder()
                .timeout(Duration::from_millis(100))
                .build()
                .unwrap(),
            initial_last_event_id: None,
            stall_timeout: Duration::from_secs(20),
            reconnect_delay: Duration::from_secs(60),
        });
        let reader = Arc::new(reader);
        reader.start();
        // 给一点时间让 task 启动
        tokio::time::sleep(Duration::from_millis(50)).await;
        // stop 应该在合理时间内返回
        let stop_result = tokio::time::timeout(Duration::from_secs(5), reader.stop()).await;
        assert!(stop_result.is_ok(), "stop should complete within timeout");
    }
}
