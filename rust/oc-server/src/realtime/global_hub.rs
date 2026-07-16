//! 全局消息流 hub — 单共享上游 reader + replay ring + broadcast fan-out。
//!
//! 对应 `packages/web/server/lib/event-stream/global-hub.js`。
//!
//! 核心不变量:
//! - **单上游 reader**: 整个进程只有一个到 OpenCode `/global/event` 的 SSE 连接
//! - **replay ring**: 最近 2048 个带 eventId 的事件, 供新 WS 客户端 resume
//! - **broadcast fan-out**: 事件通过 broadcast channel 扇出到 N 个 WS 客户端
//! - **幂等 start**: 重复调用不创建新 reader
//! - **reconnect-after-ready**: 上游重连成功时发 `Connect { was_ready: true }`,
//!   WS 桥据此重发 ready 帧让浏览器做 scoped state repair

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::Mutex;

use serde_json::Value;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use super::protocol::SseEnvelope;
use super::upstream_reader::{UpstreamEvent, UpstreamReaderConfig, UpstreamSseReader};
use super::{GLOBAL_REPLAY_LIMIT, HUB_BROADCAST_CAPACITY, UPSTREAM_RECONNECT_DELAY, UPSTREAM_STALL_TIMEOUT};

/// hub 发出的事件 (broadcast 到 N 个 WS 客户端)。
#[derive(Debug, Clone)]
pub struct HubEvent {
    pub payload: Value,
    pub directory: String,
    pub event_id: Option<String>,
}

/// hub 发出的状态变更 (broadcast 到 WS 桥)。
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum HubStatus {
    /// 上游连接成功。`was_ready` 区分首次连接 vs 重连后恢复。
    Connect { was_ready: bool },
    /// 上游断开。
    Disconnect { reason: String },
    /// 上游错误。`initial` = true 表示首次连接前错误;
    /// `build_url_failed` = true 表示 URL 构建失败 (对应 Node `buildUrlFailed`)。
    Error {
        kind: String,
        initial: bool,
        build_url_failed: bool,
    },
}

/// replay ring 中的一条记录。
#[derive(Debug, Clone)]
pub struct ReplayEntry {
    pub payload: Value,
    pub directory: String,
    pub event_id: Option<String>,
}

/// 全局消息流 hub。
///
/// 拥有一个 `UpstreamSseReader`, 将事件广播到多个订阅者,
/// 并维护一个 bounded replay ring 供新客户端 resume。
pub struct GlobalHub {
    opencode_base_url: String,
    auth_header: String,
    http_client: reqwest::Client,
    replay: Arc<Mutex<VecDeque<ReplayEntry>>>,
    event_tx: broadcast::Sender<HubEvent>,
    status_tx: broadcast::Sender<HubStatus>,
    connected: Arc<AtomicBool>,
    ever_connected: Arc<AtomicBool>,
    // WS 桥客户端计数 (仅跟踪 WS 客户端, 不含后台消费者)。
    // 0→1 自动 start(), 1→0 自动 stop()。对应 Node `stopHubIfUnused`。
    ws_client_count: Arc<AtomicUsize>,
    // reader 生命周期管理
    reader: Mutex<Option<Arc<UpstreamSseReader>>>,
    reader_task: Mutex<Option<JoinHandle<()>>>,
}

impl GlobalHub {
    /// 创建 hub (尚未启动上游 reader)。
    pub fn new(
        opencode_base_url: String,
        auth_header: String,
        http_client: reqwest::Client,
    ) -> Self {
        let (event_tx, _) = broadcast::channel(HUB_BROADCAST_CAPACITY);
        let (status_tx, _) = broadcast::channel(64);

        Self {
            opencode_base_url,
            auth_header,
            http_client,
            replay: Arc::new(Mutex::new(VecDeque::with_capacity(GLOBAL_REPLAY_LIMIT))),
            event_tx,
            status_tx,
            connected: Arc::new(AtomicBool::new(false)),
            ever_connected: Arc::new(AtomicBool::new(false)),
            ws_client_count: Arc::new(AtomicUsize::new(0)),
            reader: Mutex::new(None),
            reader_task: Mutex::new(None),
        }
    }

    /// 启动上游 reader (幂等)。
    pub fn start(&self) {
        let mut reader_slot = self.reader.lock().unwrap();
        if reader_slot.is_some() {
            return;
        }

        let base_url = self.opencode_base_url.clone();
        let auth_header = self.auth_header.clone();
        let http_client = self.http_client.clone();

        let config = UpstreamReaderConfig {
            build_url: Box::new(move || {
                let raw = format!("{}/global/event", base_url.trim_end_matches('/'));
                url::Url::parse(&raw)
                    .map(|u| u.to_string())
                    .map_err(|_| ())
            }),
            auth_header,
            http_client,
            initial_last_event_id: None,
            stall_timeout: UPSTREAM_STALL_TIMEOUT,
            reconnect_delay: UPSTREAM_RECONNECT_DELAY,
        };

        let (reader, mut event_rx) = UpstreamSseReader::new(config);
        let reader = Arc::new(reader);
        reader.start();
        *reader_slot = Some(reader.clone());

        // 启动事件消费 task: reader → hub broadcast + replay
        let event_tx = self.event_tx.clone();
        let status_tx = self.status_tx.clone();
        let replay = self.replay.clone();
        let connected = self.connected.clone();
        let ever_connected = self.ever_connected.clone();

        let handle = tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                match event {
                    UpstreamEvent::Connect { .. } => {
                        let was_ready = ever_connected.swap(true, Ordering::SeqCst);
                        connected.store(true, Ordering::SeqCst);
                        let _ = status_tx.send(HubStatus::Connect { was_ready });
                    }
                    UpstreamEvent::Disconnect { reason } => {
                        connected.store(false, Ordering::SeqCst);
                        let _ = status_tx.send(HubStatus::Disconnect { reason });
                    }
                    UpstreamEvent::Event { envelope } => {
                        let normalized = normalize_event(&envelope);
                        // push to replay (如有 event_id)
                        if normalized.event_id.is_some() {
                            push_replay(&replay, normalized.clone());
                        }
                        // broadcast
                        let _ = event_tx.send(normalized);
                    }
                    UpstreamEvent::Error { kind, .. } => {
                        let initial = !ever_connected.load(Ordering::SeqCst);
                        let (kind_str, build_url_failed) = match kind {
                            super::upstream_reader::UpstreamErrorKind::UpstreamUnavailable => {
                                ("upstream_unavailable", false)
                            }
                            super::upstream_reader::UpstreamErrorKind::StreamError => {
                                ("stream_error", false)
                            }
                            super::upstream_reader::UpstreamErrorKind::BuildUrlFailed => {
                                ("stream_error", true)
                            }
                        };
                        let _ = status_tx.send(HubStatus::Error {
                            kind: kind_str.into(),
                            initial,
                            build_url_failed,
                        });
                    }
                }
            }
        });

        *self.reader_task.lock().unwrap() = Some(handle);
    }

    /// 停止上游 reader。
    pub async fn stop(&self) {
        let reader = { self.reader.lock().unwrap().take() };
        if let Some(reader) = reader {
            reader.stop().await;
        }
        // 等待消费 task 退出
        let task = { self.reader_task.lock().unwrap().take() };
        if let Some(task) = task {
            let _ = task.await;
        }
        self.connected.store(false, Ordering::SeqCst);
        self.ever_connected.store(false, Ordering::SeqCst);
    }

    /// 注册一个 WS 桥客户端。客户端数从 0→1 时自动 `start()`。
    /// 对应 Node `global-ws-bridge.js` 的 `accept()` → `globalHub.start()`。
    pub fn register_ws_client(&self) {
        let prev = self.ws_client_count.fetch_add(1, Ordering::SeqCst);
        if prev == 0 {
            self.start();
        }
    }

    /// 注销一个 WS 桥客户端。客户端数→0 时自动 `stop()`。
    /// 对应 Node `global-ws-bridge.js` 的 `stopHubIfUnused()`。
    pub async fn unregister_ws_client(&self) {
        // 防下溢: 只在计数 > 0 时递减
        let prev = if self.ws_client_count.load(Ordering::SeqCst) == 0 {
            return;
        } else {
            self.ws_client_count.fetch_sub(1, Ordering::SeqCst)
        };
        if prev == 1 {
            self.stop().await;
        }
    }

    /// 上游是否已连接。
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    /// 上游是否曾经连接过。
    #[allow(dead_code)]
    pub fn has_connected(&self) -> bool {
        self.ever_connected.load(Ordering::SeqCst)
    }

    /// 订阅事件流。
    pub fn subscribe_event(&self) -> broadcast::Receiver<HubEvent> {
        self.event_tx.subscribe()
    }

    /// 订阅状态流。
    pub fn subscribe_status(&self) -> broadcast::Receiver<HubStatus> {
        self.status_tx.subscribe()
    }

    /// 广播合成事件 (openchamber:session-status / openchamber:session-activity) 到所有
    /// WS 桥订阅者。
    ///
    /// 合成事件由 `SessionStateRuntime` 从上游 `session.status` 派生, 无 event_id,
    /// **不入 replay ring** (不可 resume)。对应 Node `broadcastGlobalUiEvent` 对 WS
    /// 客户端的扇出路径。
    pub fn broadcast_synthetic(&self, payload: Value) {
        let _ = self.event_tx.send(HubEvent {
            payload,
            directory: "global".to_string(),
            event_id: None,
        });
    }

    /// 获取指定 eventId 之后的所有 replay 事件。
    ///
    /// 返回空 Vec 表示: eventId 未找到 (客户端落后太多, 需全量同步) 或 eventId 为 None。
    pub fn replay_after(&self, event_id: &str) -> Vec<ReplayEntry> {
        let replay = self.replay.lock().unwrap();
        let pos = replay
            .iter()
            .position(|e| e.event_id.as_deref() == Some(event_id));
        match pos {
            Some(idx) => replay.iter().skip(idx + 1).cloned().collect(),
            None => Vec::new(),
        }
    }
}

/// 将 SSE envelope 规范化为 HubEvent (directory 默认 "global")。
fn normalize_event(envelope: &SseEnvelope) -> HubEvent {
    let directory = envelope
        .directory
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or("global")
        .to_string();
    HubEvent {
        payload: envelope.payload.clone(),
        directory,
        event_id: envelope.event_id.clone(),
    }
}

/// 将事件推入 replay ring, 保持 bounded。
fn push_replay(replay: &Arc<Mutex<VecDeque<ReplayEntry>>>, event: HubEvent) {
    let mut buf = replay.lock().unwrap();
    buf.push_back(ReplayEntry {
        payload: event.payload,
        directory: event.directory,
        event_id: event.event_id,
    });
    while buf.len() > GLOBAL_REPLAY_LIMIT {
        buf.pop_front();
    }
}

// =========================================================================
// 测试
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalize_event_defaults_directory_to_global() {
        let env = SseEnvelope {
            event_id: Some("evt-1".into()),
            directory: None,
            payload: json!({"type": "test"}),
        };
        let hub_event = normalize_event(&env);
        assert_eq!(hub_event.directory, "global");
        assert_eq!(hub_event.event_id.as_deref(), Some("evt-1"));
    }

    #[test]
    fn normalize_event_preserves_directory() {
        let env = SseEnvelope {
            event_id: None,
            directory: Some("/work/dir".into()),
            payload: json!({"type": "test"}),
        };
        let hub_event = normalize_event(&env);
        assert_eq!(hub_event.directory, "/work/dir");
    }

    #[test]
    fn normalize_event_empty_directory_becomes_global() {
        let env = SseEnvelope {
            event_id: None,
            directory: Some("".into()),
            payload: json!({}),
        };
        let hub_event = normalize_event(&env);
        assert_eq!(hub_event.directory, "global");
    }

    #[test]
    fn replay_after_finds_events() {
        let replay: Arc<Mutex<VecDeque<ReplayEntry>>> = Arc::new(Mutex::new(VecDeque::new()));
        for i in 0..5 {
            push_replay(
                &replay,
                HubEvent {
                    payload: json!({"i": i}),
                    directory: "global".into(),
                    event_id: Some(format!("evt-{}", i)),
                },
            );
        }
        // replay_after("evt-2") → evt-3, evt-4
        let result = replay_after(&replay, "evt-2");
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].event_id.as_deref(), Some("evt-3"));
        assert_eq!(result[1].event_id.as_deref(), Some("evt-4"));
    }

    #[test]
    fn replay_after_not_found_returns_empty() {
        let replay: Arc<Mutex<VecDeque<ReplayEntry>>> = Arc::new(Mutex::new(VecDeque::new()));
        push_replay(
            &replay,
            HubEvent {
                payload: json!({}),
                directory: "global".into(),
                event_id: Some("evt-1".into()),
            },
        );
        let result = replay_after(&replay, "nonexistent");
        assert!(result.is_empty());
    }

    #[test]
    fn replay_ring_evicts_oldest() {
        let replay: Arc<Mutex<VecDeque<ReplayEntry>>> = Arc::new(Mutex::new(VecDeque::new()));
        // Push GLOBAL_REPLAY_LIMIT + 10 events
        for i in 0..(GLOBAL_REPLAY_LIMIT + 10) {
            push_replay(
                &replay,
                HubEvent {
                    payload: json!({"i": i}),
                    directory: "global".into(),
                    event_id: Some(format!("evt-{}", i)),
                },
            );
        }
        let buf = replay.lock().unwrap();
        assert_eq!(buf.len(), GLOBAL_REPLAY_LIMIT);
        // 最旧的应该被驱逐
        assert_eq!(buf.front().unwrap().event_id.as_deref(), Some("evt-10"));
    }

    #[tokio::test]
    async fn broadcast_synthetic_reaches_subscribers() {
        let hub = GlobalHub::new(
            "http://127.0.0.1:1".into(),
            "Basic test".into(),
            reqwest::Client::new(),
        );
        let mut rx = hub.subscribe_event();

        let payload = json!({"type": "openchamber:session-activity"});
        hub.broadcast_synthetic(payload.clone());

        let event = rx.recv().await.unwrap();
        assert_eq!(event.payload, payload);
        assert_eq!(event.directory, "global");
        // 合成事件无 event_id (不入 replay ring)
        assert!(event.event_id.is_none());
    }

    #[tokio::test]
    async fn register_ws_client_starts_reader_on_zero_to_one() {
        // 0→1 转换应启动 reader (is_connected 在连接前为 false, 但 reader 槽位应被填充)
        let hub = GlobalHub::new(
            "http://127.0.0.1:1".into(), // 端口 1, 不会真正连接
            "Basic test".into(),
            reqwest::Client::new(),
        );
        assert_eq!(hub.ws_client_count.load(Ordering::SeqCst), 0);
        // register 应触发 start(): reader 槽位从 None 变为 Some
        assert!(hub.reader.lock().unwrap().is_none());
        hub.register_ws_client();
        assert!(hub.reader.lock().unwrap().is_some());
        assert_eq!(hub.ws_client_count.load(Ordering::SeqCst), 1);
        // 第二次 register 不重复 start (reader 槽位已有值)
        hub.register_ws_client();
        assert_eq!(hub.ws_client_count.load(Ordering::SeqCst), 2);
        // 清理
        hub.unregister_ws_client().await;
        hub.unregister_ws_client().await;
        assert_eq!(hub.ws_client_count.load(Ordering::SeqCst), 0);
        assert!(hub.reader.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn unregister_ws_client_stops_reader_at_zero() {
        let hub = GlobalHub::new(
            "http://127.0.0.1:1".into(),
            "Basic test".into(),
            reqwest::Client::new(),
        );
        hub.register_ws_client();
        assert!(hub.reader.lock().unwrap().is_some());
        // 1→0 转换应停止 reader
        hub.unregister_ws_client().await;
        assert_eq!(hub.ws_client_count.load(Ordering::SeqCst), 0);
        // reader 槽位被 take() 清空
        assert!(hub.reader.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn unregister_below_zero_is_noop() {
        // 防下溢: 在计数为 0 时调用 unregister 不应导致下溢
        let hub = GlobalHub::new(
            "http://127.0.0.1:1".into(),
            "Basic test".into(),
            reqwest::Client::new(),
        );
        hub.unregister_ws_client().await;
        assert_eq!(hub.ws_client_count.load(Ordering::SeqCst), 0);
    }

    fn replay_after(replay: &Arc<Mutex<VecDeque<ReplayEntry>>>, event_id: &str) -> Vec<ReplayEntry> {
        let buf = replay.lock().unwrap();
        let pos = buf
            .iter()
            .position(|e| e.event_id.as_deref() == Some(event_id));
        match pos {
            Some(idx) => buf.iter().skip(idx + 1).cloned().collect(),
            None => Vec::new(),
        }
    }
}
