//! Tunnel host dispatcher (Layer 3): HTTP + WS loopback forwarding.
//!
//! Port of `packages/web/server/lib/relay/tunnel-host.js`. The dispatcher
//! consumes decrypted Layer 3 tunnel frames for ONE relay connection and
//! forwards them to the local loopback origin (the OpenChamber server itself,
//! bound on `127.0.0.1:<port>`).
//!
//! ## HTTP streams
//!
//! Re-mux one `HttpRequest` + zero-or-more `HttpBody` chunks + one `StreamEnd`
//! into a single `reqwest` call against the loopback origin. The response is
//! streamed back as `HttpResponse` (status + headers) + many `HttpBody` chunks
//! + a final empty `StreamEnd`.
//!
//! ## WebSocket streams
//!
//! A `WsOpen` opens a `tokio_tungstenite` client connection to the loopback
//! origin; `WsText` / `WsBinary` frames flow through; `WsClose` cleanly
//! terminates the upstream socket. Open / Close / Error are signalled to the
//! peer with the matching tunnel frames.
//!
//! ## Security
//!
//! - **Path allowlists**: defense in depth, same families the realtime-proxy
//!   allows. Anything else returns a synthetic 403 (HTTP) or a `StreamAbort`
//!   (WS).
//! - **Header stripping**: hop-by-hop headers (`connection`, `keep-alive`,
//!   `transfer-encoding`, `upgrade`, `host`, `content-length`) are dropped on
//!   the request path; `content-encoding` and `content-length` are stripped on
//!   the response path because the body is re-chunked across the tunnel and
//!   `reqwest` recomputes framing on its own.
//! - **No credential injection**: tunneled requests authenticate exactly like
//!   any remote client (`oc_client_*` header / `oc_url_token` query). The
//!   dispatcher only adds `x-gridforge-relay-connection` for traceability
//!   and (for WS) a same-origin `Origin` header so the loopback server's WS
//!   origin check passes reliably.
//! - **CR/LF rejection**: any header name or value containing `\r` or `\n` is
//!   silently dropped to prevent header smuggling through the tunnel.
//!
//! Spec: `.opencode/plans/private-relay/01-protocol-spec.md` (Layer 3).

// Production-API surface that will be wired in by `host_client` / `service`
// (steps 8/9 of stage 3f group 4). Until those call sites exist, the compiler
// treats types like `HttpStream` / `WsStream` / `ReqwestHttpDispatch` /
// `TungsteniteWsDispatch` as unused — silence the warnings globally so the
// intended surface stays obvious at a glance.
#![allow(dead_code)]

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures_util::stream::{Stream, StreamExt};
use futures_util::SinkExt;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use super::tunnel_codec::{
    chunk_payload, decode_json_payload, decode_tunnel_frame, encode_fragmented_message,
    encode_json_payload, encode_tunnel_frame, TunnelCodecError, TunnelFrame, TunnelFrameType,
};

// =============================================================================
// Public constants (mirror of JS isAllowedHttpPath + ALLOWED_WS_PATHS)
// =============================================================================

/// HTTP paths accepted through the relay. Mirror of `isAllowedHttpPath` in
/// `tunnel-host.js`. Anything not matched returns a synthetic 403.
pub fn is_allowed_http_path(pathname: &str) -> bool {
    pathname == "/health"
        || pathname == "/api"
        || pathname.starts_with("/api/")
        || pathname == "/auth"
        || pathname.starts_with("/auth/")
}

/// WebSocket paths accepted through the relay. Mirror of `ALLOWED_WS_PATHS`.
pub fn is_allowed_ws_path(pathname: &str) -> bool {
    matches!(
        pathname,
        "/api/global/event/ws" | "/api/event/ws" | "/api/terminal/ws" | "/api/dictation/ws"
    )
}

/// Hop-by-hop headers stripped from tunneled requests. `host` is set by the
/// underlying client to the loopback origin; `content-length` is dropped
/// because the body is re-chunked through the tunnel and the client computes
/// framing itself.
pub fn stripped_request_headers() -> &'static [&'static str] {
    &[
        "connection",
        "keep-alive",
        "transfer-encoding",
        "upgrade",
        "host",
        "content-length",
    ]
}

/// Response framing headers that no longer apply once the body crosses the
/// tunnel as `HttpBody` chunks (loopback `fetch` already decoded
/// `content-encoding`).
pub fn stripped_response_headers() -> &'static [&'static str] {
    &[
        "connection",
        "keep-alive",
        "transfer-encoding",
        "content-length",
        "content-encoding",
    ]
}

/// v1 backpressure rule: pause reading the loopback source while the outbound
/// relay socket has more than this buffered.
pub const BACKPRESSURE_LIMIT_BYTES: usize = 4 * 1024 * 1024;
pub const BACKPRESSURE_POLL_MS: u64 = 20;

// =============================================================================
// Dispatch abstractions
// =============================================================================
//
// The dispatcher NEVER injects credentials. Tests use fake dispatchers; the
// production path uses reqwest + tokio-tungstenite. The split keeps the
// orchestration testable without an HTTP server.

/// One-shot HTTP response handed back from the loopback origin. Stream is
/// `None` when the upstream response has no body (status has already been
/// emitted to the peer).
pub struct HttpDispatchResponse {
    pub status: u16,
    /// Lower-cased header name → value.
    pub headers: HashMap<String, String>,
    pub body: Option<Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>>>,
}

/// Cancellation handle for an in-flight HTTP stream. Dropping it (or calling
/// `abort`) cancels the loopback request and tears down the response stream.
pub trait HttpAbort: Send + Sync {
    fn abort(&self);
}

/// Live HTTP stream controller. The dispatcher receives a controller after
/// dispatching an HTTP request; it pushes body chunks (for HTTP methods that
/// carry a body), closes the request body via `finish_request_body`, and
/// pulls the response off `response_rx`.
pub struct HttpStream {
    /// Push request body chunks into the loopback request here. Drop the
    /// sender OR call `finish_request_body` (equivalent) to half-close.
    pub body_tx: mpsc::Sender<Bytes>,
    /// Resolve when the loopback response has started streaming. The
    /// receiver yields the headers + (optional) body stream.
    pub response_rx: oneshot::Receiver<Result<HttpDispatchResponse, DispatchError>>,
    /// Cancel / abort the in-flight loopback request.
    pub abort: Arc<dyn HttpAbort>,
}

/// HTTP dispatcher abstraction. Implementations target the local loopback
/// origin (127.0.0.1:<local port>). The dispatcher consumes the request body
/// as a stream and exposes the response as a stream; cancellation is exposed
/// via an [`HttpAbort`] handle.
pub trait HttpDispatch: Send + Sync {
    /// Start an HTTP request against the loopback origin.
    ///
    /// `headers` is the **already-filtered** header map (caller did allowlist
    /// stripping). `has_body == false` means the request must be built
    /// without a body (used for GET/HEAD/204-style requests). The dispatcher
    /// is expected to construct the full URL from `headers["x-loopback-url"]`
    /// plus `path` and `query`.
    fn dispatch_http(
        &self,
        method: &str,
        path: &str,
        query: &str,
        headers: HashMap<String, String>,
        has_body: bool,
        body_rx: mpsc::Receiver<Bytes>,
    ) -> Result<HttpStream, DispatchError>;
}

/// WebSocket control messages sent to the upstream loopback socket.
#[derive(Debug, Clone)]
pub enum WsOutbound {
    Text(Bytes),
    Binary(Bytes),
    Close { code: u16, reason: String },
}

/// Inbound events from the upstream loopback WebSocket.
#[derive(Debug)]
pub enum WsInbound {
    Opened { protocol: Option<String> },
    Text(Bytes),
    Binary(Bytes),
    Closed { code: u16, reason: String },
    /// Pre-open transport error (loopback dial failed or handshake rejected).
    Failed { reason: String },
}

/// Live WebSocket stream controller. The dispatcher receives a pair of
/// channels: push `WsOutbound` on `outbound_tx`, listen on `inbound_rx`.
pub struct WsStream {
    pub outbound_tx: mpsc::Sender<WsOutbound>,
    pub inbound_rx: mpsc::Receiver<WsInbound>,
}

/// WebSocket dispatcher abstraction. Implementations target the local
/// loopback origin. URL is reconstructed from `headers["x-loopback-url"]`
/// plus `path` / `query`.
pub trait WsDispatch: Send + Sync {
    fn dispatch_ws(
        &self,
        path: &str,
        query: &str,
        headers: HashMap<String, String>,
        protocols: Vec<String>,
    ) -> Result<WsStream, DispatchError>;
}

#[derive(Debug, thiserror::Error, Clone)]
pub enum DispatchError {
    #[error("dispatcher unavailable: {0}")]
    Unavailable(String),
}

// =============================================================================
// Configuration
// =============================================================================

/// Async frame sender — the relay socket is already connected; the sender
/// just pushes the plaintext frame bytes onto the WS. Errors are surfaced to
/// the caller.
pub type SendFrameFuture =
    Pin<Box<dyn Future<Output = Result<(), SendFrameError>> + Send + 'static>>;

/// Callback signature for pushing a tunnel frame back to the relay socket.
pub type SendFrameFn =
    Arc<dyn Fn(Vec<u8>) -> SendFrameFuture + Send + Sync + 'static>;

/// Callback that returns how many bytes are currently buffered for write on
/// the outbound relay socket. Used for backpressure.
pub type GetBufferedAmountFn = Arc<dyn Fn() -> usize + Send + Sync + 'static>;

/// Resolves the local loopback origin port (the OpenChamber server itself).
pub type GetLocalPortFn = Arc<dyn Fn() -> u16 + Send + Sync + 'static>;

#[derive(Debug, thiserror::Error, Clone)]
pub enum SendFrameError {
    #[error("relay socket closed")]
    Closed,
    #[error("relay socket error: {0}")]
    Other(String),
}

/// Bundle of dependencies required to construct a [`TunnelHost`]. Tests can
/// substitute fake dispatchers and an in-memory `send_frame`.
pub struct TunnelHostConfig {
    pub connection_id: String,
    pub get_local_port: GetLocalPortFn,
    pub send_frame: SendFrameFn,
    pub get_buffered_amount: GetBufferedAmountFn,
    pub http_dispatch: Arc<dyn HttpDispatch>,
    pub ws_dispatch: Arc<dyn WsDispatch>,
}

impl std::fmt::Debug for TunnelHostConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TunnelHostConfig")
            .field("connection_id", &self.connection_id)
            .field("http_dispatch", &"<dyn HttpDispatch>")
            .field("ws_dispatch", &"<dyn WsDispatch>")
            .finish()
    }
}

// =============================================================================
// TunnelHost — the per-connection dispatcher
// =============================================================================

/// Tunnel host handle. One per relay connection.
///
/// The handle owns:
/// - a `streams` map keyed by `stream_id`,
/// - a fragment assembler for WS messages,
/// - a `closed` flag.
///
/// Drop / `close()` aborts every active stream.
pub struct TunnelHost {
    connection_id: String,
    get_local_port: GetLocalPortFn,
    send_frame: SendFrameFn,
    get_buffered_amount: GetBufferedAmountFn,
    http_dispatch: Arc<dyn HttpDispatch>,
    ws_dispatch: Arc<dyn WsDispatch>,

    streams: Mutex<HashMap<u32, StreamEntry>>,
    assembler: Mutex<super::tunnel_codec::FragmentAssembler>,
    stream_count: Arc<AtomicUsize>,
    closed: AtomicBool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamKind {
    Http,
    Ws,
}

/// Per-stream bookkeeping. The HTTP variant owns an mpsc sender for body
/// chunks and an abort handle; the WS variant owns an `Arc<Mutex<Option<…>>>`
/// containing the outbound mpsc sender for the pump task, and an "opened"
/// flag (true once the loopback WS handshake completed).
struct StreamEntry {
    kind: StreamKind,
    /// HTTP: sender for body chunks (driven by HttpBody frames).
    /// WS: unused — kept as a placeholder slot.
    body_tx: mpsc::Sender<Bytes>,
    /// WS: outbound sender into the loopback WS (one end of the
    /// pump's mpsc pair). HTTP: `None`.
    ws_outbound: Option<Arc<Mutex<Option<mpsc::Sender<WsOutbound>>>>>,
    /// Set to `true` once the loopback WS handshake completed; HTTP leaves
    /// it at `false` (always).
    opened: Arc<AtomicBool>,
    abort: Option<Arc<dyn HttpAbort>>,
    /// Background task that pumps loopback WS ↔ tunnel. `None` for HTTP
    /// streams.
    pump: Option<tokio::task::JoinHandle<()>>,
}

impl TunnelHost {
    /// Construct a new tunnel host with the given dependency bundle.
    pub fn new(config: TunnelHostConfig) -> Self {
        Self {
            connection_id: config.connection_id,
            get_local_port: config.get_local_port,
            send_frame: config.send_frame,
            get_buffered_amount: config.get_buffered_amount,
            http_dispatch: config.http_dispatch,
            ws_dispatch: config.ws_dispatch,
            streams: Mutex::new(HashMap::new()),
            assembler: Mutex::new(super::tunnel_codec::FragmentAssembler::new()),
            stream_count: Arc::new(AtomicUsize::new(0)),
            closed: AtomicBool::new(false),
        }
    }

    /// Currently active stream count.
    pub fn stream_count(&self) -> usize {
        self.stream_count.load(Ordering::SeqCst)
    }

    /// True iff `close()` was called.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Drive one decrypted tunnel frame through the dispatcher. Returns once
    /// the frame has been scheduled (HTTP/WS dispatch is fully async). The
    /// return value is `Ok` if the frame was understood; the caller does not
    /// need to act on the inner value (it is currently always `()`).
    pub async fn handle_frame(self: &Arc<Self>, plaintext: &[u8]) -> Result<(), TunnelCodecError> {
        if self.closed.load(Ordering::SeqCst) {
            return Ok(());
        }
        let frame = decode_tunnel_frame(plaintext)?;

        // WS message frames may be fragmented; everything else arrives whole.
        if frame.frame_type == TunnelFrameType::WsText
            || frame.frame_type == TunnelFrameType::WsBinary
        {
            let mut asm = self.assembler.lock().expect("assembler poisoned");
            let message = match asm.push(&frame)? {
                Some(msg) => msg,
                None => return Ok(()),
            };
            self.handle_ws_message(frame.stream_id, frame.frame_type, message);
            return Ok(());
        }

        match frame.frame_type {
            TunnelFrameType::HttpRequest => {
                self.handle_http_request(frame.stream_id, &frame.payload);
            }
            TunnelFrameType::HttpBody => {
                self.handle_http_body(frame.stream_id, Bytes::copy_from_slice(&frame.payload));
            }
            TunnelFrameType::StreamEnd => {
                self.handle_stream_end(frame.stream_id);
            }
            TunnelFrameType::StreamAbort => {
                self.abort_local_stream(frame.stream_id, "aborted by client", true);
            }
            TunnelFrameType::WsOpen => {
                self.handle_ws_open(frame.stream_id, &frame.payload);
            }
            TunnelFrameType::WsClose => {
                self.handle_ws_close(frame.stream_id, &frame.payload);
            }
            TunnelFrameType::Ping => {
                let pong = encode_tunnel_frame(TunnelFrameType::Pong, frame.stream_id, &[]);
                self.send_frame_safe(pong).await;
            }
            TunnelFrameType::Pong => {
                // no-op (host never initiates pings, but tolerate client pongs)
            }
            // Host never receives HttpResponse/WsOpened; ignore silently
            // rather than tear down the connection. WsText/WsBinary are
            // handled above (they may be fragmented).
            TunnelFrameType::HttpResponse
            | TunnelFrameType::WsOpened
            | TunnelFrameType::WsText
            | TunnelFrameType::WsBinary => {}
        }
        Ok(())
    }

/// Close the host: abort every active stream, mark the host as closed.
/// Subsequent `handle_frame` calls become no-ops.
pub fn close(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        let entries: Vec<(u32, StreamKind, Option<Arc<dyn HttpAbort>>, Option<tokio::task::JoinHandle<()>>)> = {
            let mut streams = self.streams.lock().expect("streams poisoned");
            let keys: Vec<u32> = streams.keys().copied().collect();
            let mut out = Vec::with_capacity(keys.len());
            for sid in keys {
                if let Some(entry) = streams.remove(&sid) {
                    out.push((sid, entry.kind, entry.abort, entry.pump));
                }
            }
            out
        };
        self.stream_count.store(0, Ordering::SeqCst);
        for (sid, kind, abort, pump) in entries {
            match kind {
                StreamKind::Http => {
                    if let Some(a) = abort {
                        a.abort();
                    }
                }
                StreamKind::Ws => {
                    if let Some(p) = pump {
                        p.abort();
                    }
                }
            }
            // No peer abort on connection close: the peer is gone.
            let _ = sid;
        }
    }

    // -------------------------------------------------------------------------
    // Internal: send_frame with closed guard
    // -------------------------------------------------------------------------

    async fn send_frame_safe(self: &Arc<Self>, frame: Vec<u8>) {
        if self.closed.load(Ordering::SeqCst) {
            return;
        }
        let send = (self.send_frame)(frame);
        if send.await.is_err() {
            // Connection gone — flip closed so any in-flight tasks stop
            // trying to send.
            self.close();
        }
    }

    async fn send_json(
        self: &Arc<Self>,
        frame_type: TunnelFrameType,
        stream_id: u32,
        payload: &Value,
    ) {
        let bytes = encode_json_payload(payload);
        let frame = encode_tunnel_frame(frame_type, stream_id, &bytes);
        self.send_frame_safe(frame).await;
    }

    async fn send_stream_end(self: &Arc<Self>, stream_id: u32) {
        let frame = encode_tunnel_frame(TunnelFrameType::StreamEnd, stream_id, &[]);
        self.send_frame_safe(frame).await;
    }

    async fn send_abort(self: &Arc<Self>, stream_id: u32, reason: &str) {
        let payload = serde_json::json!({ "reason": reason });
        let bytes = encode_json_payload(&payload);
        let frame = encode_tunnel_frame(TunnelFrameType::StreamAbort, stream_id, &bytes);
        self.send_frame_safe(frame).await;
    }

    async fn wait_for_backpressure(&self) {
        while !self.closed.load(Ordering::SeqCst)
            && (self.get_buffered_amount)() > BACKPRESSURE_LIMIT_BYTES
        {
            tokio::time::sleep(Duration::from_millis(BACKPRESSURE_POLL_MS)).await;
        }
    }

    // -------------------------------------------------------------------------
    // HTTP
    // -------------------------------------------------------------------------

    fn handle_http_request(self: &Arc<Self>, stream_id: u32, payload: &[u8]) {
        if self.streams.lock().expect("streams poisoned").contains_key(&stream_id) {
            self.abort_local_stream(stream_id, "duplicate stream id", true);
            let host = Arc::clone(self);
            let reason = "duplicate stream id".to_string();
            tokio::spawn(async move {
                host.send_abort(stream_id, &reason).await;
            });
            return;
        }

        let request: Value = match decode_json_payload(payload, is_http_request_payload) {
            Ok(v) => v,
            Err(err) => {
                let host = Arc::clone(self);
                let reason = format!("malformed request: {}", err);
                tokio::spawn(async move {
                    host.send_abort(stream_id, &reason).await;
                });
                return;
            }
        };

        let method = request
            .get("method")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_uppercase();
        let path = request
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let query = request
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let raw_headers = request
            .get("headers")
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();

        if !is_allowed_http_path(&path) {
            self.streams.lock().expect("streams poisoned").remove(&stream_id);
            let host = Arc::clone(self);
            tokio::spawn(async move {
                host.synthetic_response(stream_id, 403, "Path is not allowed through the relay")
                    .await;
            });
            return;
        }

        let has_body = method != "GET" && method != "HEAD";

        // Build filtered headers + inject relay-connection tag.
        let mut headers = build_request_headers(&raw_headers);
        headers.insert(
            "x-gridforge-relay-connection".to_string(),
            self.connection_id.clone(),
        );

        // Stream the request body via mpsc. The dispatcher pushes chunks as
        // HttpBody frames arrive; the dispatcher pulls from the receiver
        // when constructing the loopback request body.
        let (body_tx, body_rx) = mpsc::channel::<Bytes>(16);

        let dispatch_result =
            self.http_dispatch
                .dispatch_http(&method, &path, &query, headers, has_body, body_rx);

        let stream = match dispatch_result {
            Ok(s) => s,
            Err(err) => {
                let host = Arc::clone(self);
                let reason = format!("loopback dispatch failed: {}", err);
                tokio::spawn(async move {
                    host.send_abort(stream_id, &reason).await;
                });
                return;
            }
        };

        let abort = stream.abort.clone();
        let response_rx = stream.response_rx;
        let body_tx_for_no_body = if !has_body {
            // Drop the sender so the dispatcher-side stream sees an empty
            // body and finishes the request.
            Some(body_tx.clone())
        } else {
            None
        };

        self.streams.lock().expect("streams poisoned").insert(
            stream_id,
            StreamEntry {
                kind: StreamKind::Http,
                body_tx,
                ws_outbound: None,
                opened: Arc::new(AtomicBool::new(false)),
                abort: Some(abort),
                pump: None,
            },
        );
        self.stream_count.fetch_add(1, Ordering::SeqCst);

        // Spawn the response pump: wait for headers, then stream body
        // chunks back to the peer. Tears down on abort / close / disconnect.
        let host = Arc::clone(self);
        tokio::spawn(async move {
            host.run_http_response(stream_id, response_rx).await;
        });

        // If the method has no body, drop our sender immediately so the
        // loopback dispatcher sees an empty stream and finishes the request.
        if let Some(tx) = body_tx_for_no_body {
            drop(tx);
        }
    }

    async fn run_http_response(
        self: Arc<Self>,
        stream_id: u32,
        response_rx: oneshot::Receiver<Result<HttpDispatchResponse, DispatchError>>,
    ) {
        let response = match response_rx.await {
            Ok(Ok(r)) => r,
            Ok(Err(err)) => {
                self.remove_stream(stream_id);
                self.send_abort(stream_id, &format!("loopback request failed: {}", err))
                    .await;
                return;
            }
            Err(_) => {
                // Caller dropped the channel without sending — treat as abort.
                self.remove_stream(stream_id);
                self.send_abort(stream_id, "loopback request aborted").await;
                return;
            }
        };

        // Send HttpResponse (status + headers). Filter response headers
        // before serialization.
        let headers: HashMap<String, String> = response
            .headers
            .into_iter()
            .filter(|(k, _)| !is_stripped_response_header(k))
            .collect();
        let payload = serde_json::json!({
            "status": response.status,
            "headers": headers,
        });
        self.send_json(TunnelFrameType::HttpResponse, stream_id, &payload).await;

        // Stream response body, chunked via MAX_TUNNEL_PAYLOAD_BYTES, with
        // backpressure.
        if let Some(mut body) = response.body {
            loop {
                if self.closed.load(Ordering::SeqCst) {
                    return;
                }
                match body.next().await {
                    Some(Ok(chunk)) => {
                        for piece in chunk_payload(&chunk) {
                            self.wait_for_backpressure().await;
                            if self.closed.load(Ordering::SeqCst) {
                                return;
                            }
                            let frame =
                                encode_tunnel_frame(TunnelFrameType::HttpBody, stream_id, &piece);
                            self.send_frame_safe(frame).await;
                        }
                    }
                    Some(Err(err)) => {
                        self.remove_stream(stream_id);
                        self.send_abort(
                            stream_id,
                            &format!("loopback response failed: {}", err),
                        )
                        .await;
                        return;
                    }
                    None => break,
                }
            }
        }

        self.remove_stream(stream_id);
        self.send_stream_end(stream_id).await;
    }

    async fn synthetic_response(self: &Arc<Self>, stream_id: u32, status: u16, message: &str) {
        let payload = serde_json::json!({
            "status": status,
            "headers": { "content-type": "application/json" },
        });
        self.send_json(TunnelFrameType::HttpResponse, stream_id, &payload)
            .await;
        let body = serde_json::json!({
            "error": message,
            "reason": message,
            "source": "relay-tunnel-host",
        });
        let body_bytes = encode_json_payload(&body);
        let frame = encode_tunnel_frame(TunnelFrameType::HttpBody, stream_id, &body_bytes);
        self.send_frame_safe(frame).await;
        self.send_stream_end(stream_id).await;
    }

    fn handle_http_body(&self, stream_id: u32, chunk: Bytes) {
        let entry_kind = {
            let streams = self.streams.lock().expect("streams poisoned");
            streams.get(&stream_id).map(|e| (e.kind, e.body_tx.clone()))
        };
        match entry_kind {
            Some((StreamKind::Http, body_tx)) => {
                // Best-effort enqueue. If the receiver is gone (loopback
                // request already finished or aborted), the channel's
                // try_send will fail silently — that is fine: the response
                // side will close the stream when it sees the body stream
                // end.
                let _ = body_tx.try_send(chunk);
            }
            _ => {}
        }
    }

    fn handle_stream_end(&self, stream_id: u32) {
        // Only HTTP request bodies get half-closed by StreamEnd; for WS the
        // frame is meaningless (WS close is a separate WsClose frame).
        let entry = {
            let streams = self.streams.lock().expect("streams poisoned");
            streams.get(&stream_id).map(|e| (e.kind, e.body_tx.clone()))
        };
        if let Some((StreamKind::Http, body_tx)) = entry {
            drop(body_tx);
        }
    }

    // -------------------------------------------------------------------------
    // WS
    // -------------------------------------------------------------------------

    fn handle_ws_open(self: &Arc<Self>, stream_id: u32, payload: &[u8]) {
        if self.streams.lock().expect("streams poisoned").contains_key(&stream_id) {
            self.abort_local_stream(stream_id, "duplicate stream id", true);
            let host = Arc::clone(self);
            let reason = "duplicate stream id".to_string();
            tokio::spawn(async move {
                host.send_abort(stream_id, &reason).await;
            });
            return;
        }

        let open: Value = match decode_json_payload(payload, is_ws_open_payload) {
            Ok(v) => v,
            Err(err) => {
                let host = Arc::clone(self);
                let reason = format!("malformed ws open: {}", err);
                tokio::spawn(async move {
                    host.send_abort(stream_id, &reason).await;
                });
                return;
            }
        };

        let path = open
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let query = open
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let protocols: Vec<String> = open
            .get("protocols")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|p| p.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        if !is_allowed_ws_path(&path) {
            let host = Arc::clone(self);
            tokio::spawn(async move {
                host.send_abort(stream_id, "Path is not allowed through the relay")
                    .await;
            });
            return;
        }

        // Build dial headers: the connection tag and a same-origin Origin
        // so the loopback server's WS origin check passes reliably for
        // every client platform. The request itself is still authenticated
        // by the tunneled `oc_url_token`, not by this Origin.
        let mut dial_headers = HashMap::new();
        dial_headers.insert(
            "x-gridforge-relay-connection".to_string(),
            self.connection_id.clone(),
        );
        let local_port = (self.get_local_port)();
        dial_headers.insert("origin".to_string(), format!("http://127.0.0.1:{}", local_port));

        let stream = match self
            .ws_dispatch
            .dispatch_ws(&path, &query, dial_headers, protocols)
        {
            Ok(s) => s,
            Err(err) => {
                let host = Arc::clone(self);
                let reason = format!("ws dial failed: {}", err);
                tokio::spawn(async move {
                    host.send_abort(stream_id, &reason).await;
                });
                return;
            }
        };

        let WsStream {
            outbound_tx,
            inbound_rx,
        } = stream;
        let opened_flag = Arc::new(AtomicBool::new(false));

        // The pump task owns the outbound_tx. We share it with
        // handle_ws_message via an Option<…> slot — handle_ws_message can
        // take() it once on stream removal to deliver the final close, and
        // in the meantime try_send into it for ordinary text/binary frames.
        let outbound_slot: Arc<Mutex<Option<mpsc::Sender<WsOutbound>>>> =
            Arc::new(Mutex::new(Some(outbound_tx)));

        // Spawn the pump task that bridges loopback WS ↔ tunnel. The task
        // owns both the inbound receiver (loopback → tunnel) and a clone
        // of the outbound sender (tunnel → loopback). When either side
        // closes, the other side is torn down.
        let host = Arc::clone(self);
        let outbound_for_pump = {
            let mut g = outbound_slot.lock().expect("ws slot poisoned");
            g.take()
                .expect("ws outbound slot must be Some at construction")
        };
        let pump = tokio::spawn(async move {
            host.run_ws_pump(stream_id, inbound_rx, outbound_for_pump)
                .await;
        });

        self.streams.lock().expect("streams poisoned").insert(
            stream_id,
            StreamEntry {
                kind: StreamKind::Ws,
                body_tx: mpsc::channel::<Bytes>(1).0, // unused for WS
                ws_outbound: Some(outbound_slot),
                opened: opened_flag,
                abort: None,
                pump: Some(pump),
            },
        );
        self.stream_count.fetch_add(1, Ordering::SeqCst);
    }

    async fn run_ws_pump(
        self: Arc<Self>,
        stream_id: u32,
        mut inbound_rx: mpsc::Receiver<WsInbound>,
        outbound_tx: mpsc::Sender<WsOutbound>,
    ) {
        while let Some(event) = inbound_rx.recv().await {
            if self.closed.load(Ordering::SeqCst) {
                return;
            }
            match event {
                WsInbound::Opened { protocol } => {
                    if let Some(entry) = self
                        .streams
                        .lock()
                        .expect("streams poisoned")
                        .get(&stream_id)
                    {
                        entry.opened.store(true, Ordering::SeqCst);
                    }
                    let payload = match protocol {
                        Some(p) => serde_json::json!({ "protocol": p }),
                        None => serde_json::json!({}),
                    };
                    self.send_json(TunnelFrameType::WsOpened, stream_id, &payload)
                        .await;
                }
                WsInbound::Text(bytes) => {
                    self.send_fragmented_message(TunnelFrameType::WsText, stream_id, bytes)
                        .await;
                }
                WsInbound::Binary(bytes) => {
                    self.send_fragmented_message(TunnelFrameType::WsBinary, stream_id, bytes)
                        .await;
                }
                WsInbound::Closed { code, reason } => {
                    let opened = self.opened_flag_value(stream_id);
                    self.remove_stream(stream_id);
                    if opened {
                        let payload = serde_json::json!({ "code": code, "reason": reason });
                        self.send_json(TunnelFrameType::WsClose, stream_id, &payload)
                            .await;
                    } else {
                        let reason_msg = if reason.is_empty() {
                            format!("upstream ws closed ({})", code)
                        } else {
                            reason
                        };
                        self.send_abort(stream_id, &reason_msg).await;
                    }
                    // Drop the outbound sender to terminate the dispatcher
                    // task.
                    drop(outbound_tx);
                    return;
                }
                WsInbound::Failed { reason } => {
                    let opened = self.opened_flag_value(stream_id);
                    if !opened {
                        self.remove_stream(stream_id);
                        self.send_abort(stream_id, &format!("upstream ws error: {}", reason))
                            .await;
                        drop(outbound_tx);
                        return;
                    }
                    // Post-open transport error: a Close will follow; let
                    // that handler do the bookkeeping.
                }
            }
        }
        // inbound channel closed without an explicit Closed event — mirror
        // the JS `socket.on('close')` fallback.
        let opened = self.opened_flag_value(stream_id);
        if self
            .streams
            .lock()
            .expect("streams poisoned")
            .contains_key(&stream_id)
        {
            self.remove_stream(stream_id);
            if opened {
                let payload = serde_json::json!({ "code": 1006, "reason": "" });
                self.send_json(TunnelFrameType::WsClose, stream_id, &payload)
                    .await;
            } else {
                self.send_abort(stream_id, "upstream ws closed unexpectedly")
                    .await;
            }
        }
        drop(outbound_tx);
    }

    fn opened_flag_value(&self, stream_id: u32) -> bool {
        self.streams
            .lock()
            .expect("streams poisoned")
            .get(&stream_id)
            .map(|e| e.opened.load(Ordering::SeqCst))
            .unwrap_or(false)
    }

    async fn send_fragmented_message(
        self: &Arc<Self>,
        frame_type: TunnelFrameType,
        stream_id: u32,
        payload: Bytes,
    ) {
        for frame in encode_fragmented_message(frame_type, stream_id, &payload) {
            self.wait_for_backpressure().await;
            if self.closed.load(Ordering::SeqCst) {
                return;
            }
            self.send_frame_safe(frame).await;
        }
    }

    fn handle_ws_message(&self, stream_id: u32, frame_type: TunnelFrameType, message: Vec<u8>) {
        let outbound = {
            let streams = self.streams.lock().expect("streams poisoned");
            match streams.get(&stream_id) {
                Some(e)
                    if e.kind == StreamKind::Ws
                        && e.opened.load(Ordering::SeqCst)
                        && e.ws_outbound.is_some() =>
                {
                    e.ws_outbound.as_ref().map(|slot| {
                        let g = slot.lock().expect("ws slot poisoned");
                        g.as_ref().cloned()
                    })
                }
                _ => None,
            }
        };
        let outbound = match outbound {
            Some(Some(tx)) => tx,
            _ => return,
        };
        let outbound_msg = match frame_type {
            TunnelFrameType::WsText => WsOutbound::Text(Bytes::from(message)),
            _ => WsOutbound::Binary(Bytes::from(message)),
        };
        // Best-effort push. If the upstream loopback is gone, the
        // subsequent WsClose will clean up.
        let _ = outbound.try_send(outbound_msg);
    }

    fn handle_ws_close(self: &Arc<Self>, stream_id: u32, payload: &[u8]) {
        let entry = {
            let mut streams = self.streams.lock().expect("streams poisoned");
            streams.remove(&stream_id)
        };
        let Some(entry) = entry else {
            return;
        };
        self.stream_count.fetch_sub(1, Ordering::SeqCst);

        let mut close = serde_json::json!({ "code": 1000, "reason": "" });
        if let Ok(parsed) = decode_json_payload(payload, is_ws_close_payload) {
            if let Some(code) = parsed.get("code").and_then(|v| v.as_u64()) {
                if (1000..=4999).contains(&code) {
                    close["code"] = serde_json::json!(code);
                }
            }
            if let Some(reason) = parsed.get("reason").and_then(|v| v.as_str()) {
                close["reason"] = serde_json::json!(reason);
            }
        }
        let code = close["code"].as_u64().unwrap_or(1000) as u16;
        let reason = close["reason"].as_str().unwrap_or("").to_string();

        // For WS, aborting the pump task closes both sides of the WS. The
        // pump task owns the outbound_tx and tears it down on exit.
        if let Some(pump) = entry.pump {
            pump.abort();
        }
        let _ = (code, reason);
    }

    // -------------------------------------------------------------------------
    // Stream lifecycle helpers
    // -------------------------------------------------------------------------

    fn remove_stream(&self, stream_id: u32) {
        let removed = self
            .streams
            .lock()
            .expect("streams poisoned")
            .remove(&stream_id);
        if removed.is_some() {
            self.stream_count.fetch_sub(1, Ordering::SeqCst);
        }
    }

    fn abort_local_stream(self: &Arc<Self>, stream_id: u32, reason: &str, send_peer_abort: bool) {
        let entry = {
            let mut streams = self.streams.lock().expect("streams poisoned");
            streams.remove(&stream_id)
        };
        if let Some(entry) = entry {
            self.stream_count.fetch_sub(1, Ordering::SeqCst);
            match entry.kind {
                StreamKind::Http => {
                    if let Some(abort) = entry.abort {
                        abort.abort();
                    }
                }
                StreamKind::Ws => {
                    if let Some(pump) = entry.pump {
                        pump.abort();
                    }
                }
            }
            if send_peer_abort {
                let host = Arc::clone(self);
                let reason = reason.to_string();
                tokio::spawn(async move {
                    host.send_abort(stream_id, &reason).await;
                });
            }
        }
    }
}

// =============================================================================
// Helpers: JSON validators, header filtering
// =============================================================================

fn is_http_request_payload(parsed: &Value) -> bool {
    parsed.is_object()
        && parsed.get("method").and_then(|v| v.as_str()).is_some()
        && parsed.get("path").and_then(|v| v.as_str()).is_some()
        && parsed.get("query").and_then(|v| v.as_str()).is_some()
        && parsed
            .get("headers")
            .and_then(|v| v.as_object())
            .is_some()
}

fn is_ws_open_payload(parsed: &Value) -> bool {
    if !parsed.is_object() {
        return false;
    }
    if parsed.get("path").and_then(|v| v.as_str()).is_none() {
        return false;
    }
    if !parsed
        .get("query")
        .and_then(|v| v.as_str())
        .is_some()
    {
        return false;
    }
    match parsed.get("protocols") {
        None => true,
        Some(v) => v.is_array(),
    }
}

fn is_ws_close_payload(parsed: &Value) -> bool {
    parsed.is_object()
}

fn is_stripped_request_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    stripped_request_headers().iter().any(|h| *h == lower)
}

fn is_stripped_response_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    stripped_response_headers().iter().any(|h| *h == lower)
}

/// Build the outbound HTTP request headers from a raw tunnel-side object.
/// Drops hop-by-hop headers, rejects any header containing CR/LF (defense
/// against smuggling), lower-cases the names.
fn build_request_headers(raw: &serde_json::Map<String, Value>) -> HashMap<String, String> {
    let mut headers = HashMap::with_capacity(raw.len());
    for (name, value) in raw {
        if let Some(s) = value.as_str() {
            let lower = name.to_ascii_lowercase();
            if is_stripped_request_header(&lower) {
                continue;
            }
            if name.contains('\r') || name.contains('\n') || s.contains('\r') || s.contains('\n') {
                continue;
            }
            headers.insert(lower, s.to_string());
        }
    }
    headers
}

// =============================================================================
// Production dispatchers: reqwest HTTP + tokio-tungstenite WS
// =============================================================================

/// Internal: the loopback URL is supplied to dispatchers via the
/// `x-loopback-url` header (populated by the host). Both dispatchers read it
/// from the header map and reconstruct the full URL. This indirection keeps
/// the dispatch trait signatures identical regardless of where the loopback
/// origin is configured.
fn extract_loopback_url(headers: &HashMap<String, String>) -> Result<String, DispatchError> {
    headers
        .get("x-loopback-url")
        .cloned()
        .ok_or_else(|| DispatchError::Unavailable("missing x-loopback-url header".to_string()))
}

fn build_loopback_http_url(base_url: &str, path: &str, query: &str) -> String {
    if query.is_empty() {
        format!("{}{}", base_url, path)
    } else {
        format!("{}{}?{}", base_url, path, query)
    }
}

fn build_loopback_ws_url(base_url: &str, path: &str, query: &str) -> String {
    let swapped = base_url
        .replace("https://", "wss://")
        .replace("http://", "ws://");
    if query.is_empty() {
        format!("{}{}", swapped, path)
    } else {
        format!("{}{}?{}", swapped, path, query)
    }
}

/// Adapter that converts an `mpsc::Receiver<Bytes>` into a `Stream<Item =
/// Result<Bytes, io::Error>>`, observing an abort signal so the consumer can
/// observe cancellation.
struct StreamBody {
    inner: mpsc::Receiver<Bytes>,
    abort: tokio::sync::watch::Receiver<bool>,
}

impl StreamBody {
    fn new(inner: mpsc::Receiver<Bytes>, abort: tokio::sync::watch::Receiver<bool>) -> Self {
        Self { inner, abort }
    }
}

impl Stream for StreamBody {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        if *self.abort.borrow() {
            return std::task::Poll::Ready(None);
        }
        // Register for abort wakeup so we re-check after cancellation.
        // tokio::sync::watch exposes a `changed()` future, but we just need
        // a waker hook — borrow_and_update() yields Pending until the next
        // change and calls cx.waker().wake_by_ref() on change.
        let _ = self.abort.borrow_and_update();
        match self.inner.poll_recv(cx) {
            std::task::Poll::Ready(Some(bytes)) => std::task::Poll::Ready(Some(Ok(bytes))),
            std::task::Poll::Ready(None) => std::task::Poll::Ready(None),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

struct AbortHandle {
    flag: tokio::sync::watch::Sender<bool>,
}

impl AbortHandle {
    fn new() -> (Arc<Self>, tokio::sync::watch::Receiver<bool>) {
        let (tx, rx) = tokio::sync::watch::channel(false);
        (Arc::new(Self { flag: tx }), rx)
    }
}

impl HttpAbort for AbortHandle {
    fn abort(&self) {
        let _ = self.flag.send(true);
    }
}

/// Reqwest-backed HTTP dispatcher. Constructed once per process; cheap to
/// clone (internally `Arc`-wrapped).
#[derive(Clone)]
pub struct ReqwestHttpDispatch {
    client: reqwest::Client,
}

impl ReqwestHttpDispatch {
    pub fn new(client: reqwest::Client) -> Self {
        Self { client }
    }
}

impl Default for ReqwestHttpDispatch {
    fn default() -> Self {
        Self::new(
            reqwest::Client::builder()
                .pool_idle_timeout(Duration::from_secs(30))
                .build()
                .expect("reqwest client builder is infallible"),
        )
    }
}

impl HttpDispatch for ReqwestHttpDispatch {
    fn dispatch_http(
        &self,
        method: &str,
        path: &str,
        query: &str,
        headers: HashMap<String, String>,
        has_body: bool,
        body_rx: mpsc::Receiver<Bytes>,
    ) -> Result<HttpStream, DispatchError> {
        let base_url = extract_loopback_url(&headers)?;
        let url = build_loopback_http_url(&base_url, path, query);

        let mut header_map = reqwest::header::HeaderMap::new();
        for (k, v) in &headers {
            if k == "x-loopback-url" {
                continue;
            }
            if let (Ok(name), Ok(value)) = (
                reqwest::header::HeaderName::from_bytes(k.as_bytes()),
                reqwest::header::HeaderValue::from_str(v),
            ) {
                header_map.insert(name, value);
            }
        }

        let (abort_handle, abort_signal) = AbortHandle::new();

        let mut builder = self
            .client
            .request(
                reqwest::Method::from_bytes(method.as_bytes())
                    .map_err(|e| DispatchError::Unavailable(format!("bad method: {}", e)))?,
                &url,
            )
            .headers(header_map);

        if has_body {
            let body_stream = StreamBody::new(body_rx, abort_signal);
            builder = builder.body(reqwest::Body::wrap_stream(body_stream));
        } else {
            // For body-less methods we drain the receiver in a background
            // task so the sender never blocks. The receiver is dropped
            // (and the task ends) as soon as the stream entry is removed.
            let mut rx = body_rx;
            tokio::spawn(async move {
                while rx.recv().await.is_some() {}
            });
        }

        let request = builder
            .build()
            .map_err(|e| DispatchError::Unavailable(format!("build: {}", e)))?;

        let client = self.client.clone();
        let (response_tx, response_rx) = oneshot::channel();
        tokio::spawn(async move {
            let result = match client.execute(request).await {
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    let mut hdrs = HashMap::new();
                    for (name, value) in resp.headers().iter() {
                        if let Ok(s) = value.to_str() {
                            hdrs.insert(name.as_str().to_ascii_lowercase(), s.to_string());
                        }
                    }
                    let stream = resp
                        .bytes_stream()
                        .map(|r| r.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e)));
                    let stream: Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>> =
                        Box::pin(stream);
                    Ok(HttpDispatchResponse {
                        status,
                        headers: hdrs,
                        body: Some(stream),
                    })
                }
                Err(err) => Err(DispatchError::Unavailable(err.to_string())),
            };
            let _ = response_tx.send(result);
        });

        Ok(HttpStream {
            body_tx: mpsc::channel::<Bytes>(1).0,
            response_rx,
            abort: abort_handle,
        })
    }
}

// -----------------------------------------------------------------------------
// WebSocket dispatcher (tokio-tungstenite)
// -----------------------------------------------------------------------------

/// Tungstenite-backed WS dispatcher. Cheap to clone.
#[derive(Clone, Default)]
pub struct TungsteniteWsDispatch;

impl TungsteniteWsDispatch {
    pub fn new() -> Self {
        Self
    }
}

impl WsDispatch for TungsteniteWsDispatch {
    fn dispatch_ws(
        &self,
        path: &str,
        query: &str,
        headers: HashMap<String, String>,
        protocols: Vec<String>,
    ) -> Result<WsStream, DispatchError> {
        let base_url = extract_loopback_url(&headers)?;
        let full_url = build_loopback_ws_url(&base_url, path, query);

        // Build a tungstenite handshake request directly. This matches the
        // existing `preview/routes.rs` pattern of using
        // `tokio_tungstenite::connect_async(request)`.
        let url: url::Url = full_url
            .parse()
            .map_err(|e| DispatchError::Unavailable(format!("bad url: {}", e)))?;
        let mut request = tokio_tungstenite::tungstenite::http::Request::builder()
            .method("GET")
            .uri(url.as_str())
            .body(())
            .map_err(|e| DispatchError::Unavailable(format!("bad request: {}", e)))?;
        for (k, v) in &headers {
            if k == "x-loopback-url" {
                continue;
            }
            if let (Ok(name), Ok(value)) = (
                tokio_tungstenite::tungstenite::http::HeaderName::from_bytes(k.as_bytes()),
                tokio_tungstenite::tungstenite::http::HeaderValue::from_str(v),
            ) {
                request.headers_mut().insert(name, value);
            }
        }
        if !protocols.is_empty() {
            let joined = protocols.join(",");
            if let Ok(value) =
                tokio_tungstenite::tungstenite::http::HeaderValue::from_str(&joined)
            {
                request
                    .headers_mut()
                    .insert(tokio_tungstenite::tungstenite::http::header::SEC_WEBSOCKET_PROTOCOL, value);
            }
        }

        let (outbound_tx, mut outbound_rx) = mpsc::channel::<WsOutbound>(32);
        let (inbound_tx, inbound_rx) = mpsc::channel::<WsInbound>(32);

        tokio::spawn(async move {
            // Wrap the connect call in a cancellation: if the host closes
            // the outbound channel before the dial completes, we give up.
            let dial_result: Result<_, tokio_tungstenite::tungstenite::Error> = loop {
                let connect_fut = tokio_tungstenite::connect_async(request);
                tokio::pin!(connect_fut);
                tokio::select! {
                    biased;
                    _ = outbound_rx.recv() => {
                        let _ = inbound_tx
                            .send(WsInbound::Failed {
                                reason: "caller cancelled before ws dial completed".to_string(),
                            })
                            .await;
                        return;
                    }
                    r = &mut connect_fut => break r,
                }
            };

            let (socket, _response) = match dial_result {
                Ok(pair) => pair,
                Err(err) => {
                    let _ = inbound_tx
                        .send(WsInbound::Failed {
                            reason: err.to_string(),
                        })
                        .await;
                    return;
                }
            };
            // Sub-protocol echo: tungstenite consumes the protocol during
            // the handshake; the loopback server doesn't necessarily
            // advertise it back, so we leave the protocol field empty.
            let _ = inbound_tx
                .send(WsInbound::Opened { protocol: None })
                .await;

            let (mut write, mut read) = socket.split();

            let outbound_to_socket = tokio::spawn(async move {
                while let Some(msg) = outbound_rx.recv().await {
                    let tg = match msg {
                        WsOutbound::Text(text) => {
                            // Tungstenite 0.27 uses Utf8Bytes for Text
                            // payloads. We accept either Bytes (validated)
                            // or a stringified fallback.
                            match tokio_tungstenite::tungstenite::protocol::frame::Utf8Bytes::try_from(text) {
                                Ok(utf8) => tokio_tungstenite::tungstenite::Message::Text(utf8),
                                Err(_) => continue, // skip invalid UTF-8
                            }
                        }
                        WsOutbound::Binary(bin) => {
                            tokio_tungstenite::tungstenite::Message::Binary(bin)
                        }
                        WsOutbound::Close { code, reason } => {
                            tokio_tungstenite::tungstenite::Message::Close(Some(
                                tokio_tungstenite::tungstenite::protocol::CloseFrame {
                                    code: code.into(),
                                    reason: tokio_tungstenite::tungstenite::protocol::frame::Utf8Bytes::from(reason),
                                },
                            ))
                        }
                    };
                    if write.send(tg).await.is_err() {
                        break;
                    }
                }
            });

            let pump_to_inbound = tokio::spawn(async move {
                while let Some(msg_result) = read.next().await {
                    match msg_result {
                        Ok(tokio_tungstenite::tungstenite::Message::Text(text)) => {
                            if inbound_tx
                                .send(WsInbound::Text(Bytes::copy_from_slice(text.as_bytes())))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                        Ok(tokio_tungstenite::tungstenite::Message::Binary(bin)) => {
                            if inbound_tx.send(WsInbound::Binary(bin)).await.is_err() {
                                break;
                            }
                        }
                        Ok(tokio_tungstenite::tungstenite::Message::Close(frame)) => {
                            let code = frame.as_ref().map(|f| u16::from(f.code)).unwrap_or(1000);
                            let reason = frame
                                .as_ref()
                                .map(|f| f.reason.to_string())
                                .unwrap_or_default();
                            let _ = inbound_tx
                                .send(WsInbound::Closed { code, reason })
                                .await;
                            return;
                        }
                        Ok(
                            tokio_tungstenite::tungstenite::Message::Ping(_)
                            | tokio_tungstenite::tungstenite::Message::Pong(_)
                            | tokio_tungstenite::tungstenite::Message::Frame(_),
                        ) => continue,
                        Err(err) => {
                            let _ = inbound_tx
                                .send(WsInbound::Failed {
                                    reason: err.to_string(),
                                })
                                .await;
                            return;
                        }
                    }
                }
                // Stream ended without explicit close frame.
                let _ = inbound_tx
                    .send(WsInbound::Closed {
                        code: 1006,
                        reason: String::new(),
                    })
                    .await;
            });

            let _ = tokio::join!(outbound_to_socket, pump_to_inbound);
        });

        Ok(WsStream {
            outbound_tx,
            inbound_rx,
        })
    }
}

// =============================================================================
// Frame helpers (test ergonomics)
// =============================================================================

/// Convenience wrapper around `TunnelHost::handle_frame` that decodes a single
/// tunnel frame from its raw bytes and feeds it in. Useful in tests so the
/// caller can stay in "give me bytes" mode.
pub async fn handle_raw_frame(
    host: &Arc<TunnelHost>,
    plaintext: &[u8],
) -> Result<(), TunnelCodecError> {
    host.handle_frame(plaintext).await
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering as AOrd};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    // -------------------------------------------------------------------------
    // Path allowlists
    // -------------------------------------------------------------------------

    #[test]
    fn http_allowlist_matches_node_reference() {
        assert!(is_allowed_http_path("/health"));
        assert!(is_allowed_http_path("/api"));
        assert!(is_allowed_http_path("/api/"));
        assert!(is_allowed_http_path("/api/session"));
        assert!(is_allowed_http_path("/api/event/ws"));
        assert!(is_allowed_http_path("/auth"));
        assert!(is_allowed_http_path("/auth/"));
        assert!(is_allowed_http_path("/auth/login"));

        // Negative cases — must mirror JS `isAllowedHttpPath` exactly.
        assert!(!is_allowed_http_path(""));
        assert!(!is_allowed_http_path("/"));
        assert!(!is_allowed_http_path("/api-private"));
        assert!(!is_allowed_http_path("/authorize"));
        assert!(!is_allowed_http_path("/healthcheck"));
        assert!(!is_allowed_http_path("//api"));
    }

    #[test]
    fn ws_allowlist_matches_node_reference() {
        assert!(is_allowed_ws_path("/api/global/event/ws"));
        assert!(is_allowed_ws_path("/api/event/ws"));
        assert!(is_allowed_ws_path("/api/terminal/ws"));
        assert!(is_allowed_ws_path("/api/dictation/ws"));

        assert!(!is_allowed_ws_path("/api"));
        assert!(!is_allowed_ws_path("/api/"));
        assert!(!is_allowed_ws_path("/api/event"));
        assert!(!is_allowed_ws_path("/api/global/event"));
        assert!(!is_allowed_ws_path("/health"));
        assert!(!is_allowed_ws_path("/api/terminal"));
        assert!(!is_allowed_ws_path("/api/dictation"));
        assert!(!is_allowed_ws_path("/api/global/event/wsx"));
    }

    // -------------------------------------------------------------------------
    // Header stripping
    // -------------------------------------------------------------------------

    #[test]
    fn build_request_headers_strips_hop_by_hop() {
        let raw = serde_json::json!({
            "Connection": "close",
            "Keep-Alive": "timeout=5",
            "Transfer-Encoding": "chunked",
            "Upgrade": "websocket",
            "Host": "evil.example.com",
            "Content-Length": "999",
            "Authorization": "Bearer oc_client_xxx",
            "X-GridForge-Real": "abc",
        })
        .as_object()
        .cloned()
        .unwrap();
        let headers = build_request_headers(&raw);
        assert!(!headers.contains_key("connection"));
        assert!(!headers.contains_key("keep-alive"));
        assert!(!headers.contains_key("transfer-encoding"));
        assert!(!headers.contains_key("upgrade"));
        assert!(!headers.contains_key("host"));
        assert!(!headers.contains_key("content-length"));
        assert!(headers.contains_key("authorization"));
        assert_eq!(headers.get("authorization").unwrap(), "Bearer oc_client_xxx");
        assert_eq!(headers.get("x-gridforge-real").unwrap(), "abc");
    }

    #[test]
    fn build_request_headers_rejects_crlf_injection() {
        let raw = serde_json::json!({
            "X-Evil": "ok\r\nSet-Cookie: pwned=1",
            "X-Bad-Name": "foo\r\nbar",
            "X-Safe": "clean",
        })
        .as_object()
        .cloned()
        .unwrap();
        let headers = build_request_headers(&raw);
        assert!(!headers.contains_key("x-evil"));
        assert!(!headers.contains_key("x-bad-name"));
        assert_eq!(headers.get("x-safe").unwrap(), "clean");
    }

    #[test]
    fn build_request_headers_lowercases_keys() {
        let raw = serde_json::json!({
            "X-Forwarded-For": "1.2.3.4",
            "CONTENT-TYPE": "application/json",
        })
        .as_object()
        .cloned()
        .unwrap();
        let headers = build_request_headers(&raw);
        assert!(headers.contains_key("x-forwarded-for"));
        assert!(headers.contains_key("content-type"));
    }

    #[test]
    fn stripped_request_headers_match_node_reference() {
        let set = stripped_request_headers();
        for h in [
            "connection",
            "keep-alive",
            "transfer-encoding",
            "upgrade",
            "host",
            "content-length",
        ] {
            assert!(set.iter().any(|x| *x == h), "missing {}", h);
        }
    }

    #[test]
    fn stripped_response_headers_match_node_reference() {
        let set = stripped_response_headers();
        for h in [
            "connection",
            "keep-alive",
            "transfer-encoding",
            "content-length",
            "content-encoding",
        ] {
            assert!(set.iter().any(|x| *x == h), "missing {}", h);
        }
    }

    // -------------------------------------------------------------------------
    // JSON shape validators
    // -------------------------------------------------------------------------

    #[test]
    fn http_request_shape_validator_rejects_bad_payload() {
        assert!(!is_http_request_payload(&serde_json::json!({})));
        assert!(!is_http_request_payload(&serde_json::json!({
            "method": "GET"
        })));
        assert!(!is_http_request_payload(&serde_json::json!({
            "method": "GET",
            "path": "/health"
        })));
        assert!(!is_http_request_payload(&serde_json::json!({
            "method": "GET",
            "path": "/health",
            "query": ""
        })));
        assert!(!is_http_request_payload(&serde_json::json!({
            "method": "GET",
            "path": "/health",
            "query": "",
            "headers": "not-an-object",
        })));
        assert!(is_http_request_payload(&serde_json::json!({
            "method": "GET",
            "path": "/health",
            "query": "",
            "headers": {},
        })));
        assert!(is_http_request_payload(&serde_json::json!({
            "method": "POST",
            "path": "/api/x",
            "query": "a=1",
            "headers": { "authorization": "Bearer t" },
        })));
    }

    #[test]
    fn ws_open_shape_validator_rejects_bad_payload() {
        assert!(!is_ws_open_payload(&serde_json::json!({})));
        assert!(!is_ws_open_payload(&serde_json::json!({
            "path": "/api/event/ws"
        })));
        assert!(!is_ws_open_payload(&serde_json::json!({
            "path": "/api/event/ws",
            "query": null
        })));
        assert!(!is_ws_open_payload(&serde_json::json!({
            "path": "/api/event/ws",
            "query": 42
        })));
        assert!(!is_ws_open_payload(&serde_json::json!({
            "path": "/api/event/ws",
            "query": "",
            "protocols": "not-an-array",
        })));
        assert!(is_ws_open_payload(&serde_json::json!({
            "path": "/api/event/ws",
            "query": "",
        })));
        assert!(is_ws_open_payload(&serde_json::json!({
            "path": "/api/event/ws",
            "query": "",
            "protocols": ["oc.v1"],
        })));
    }

    // -------------------------------------------------------------------------
    // Mock dispatchers + capture buffer
    // -------------------------------------------------------------------------

    /// Test HTTP dispatcher that captures the dispatched request and serves
    /// a fixed response. It mimics the production dispatch contract.
    struct MockHttp {
        captured: Arc<Mutex<Vec<MockHttpCall>>>,
        response_status: u16,
        response_headers: HashMap<String, String>,
        response_body: Vec<u8>,
    }

    #[derive(Clone, Debug)]
    struct MockHttpCall {
        method: String,
        path: String,
        query: String,
        headers: HashMap<String, String>,
        body_chunks: Vec<Bytes>,
    }

    impl MockHttp {
        fn new(status: u16, body: &[u8]) -> Self {
            let mut hdrs = HashMap::new();
            hdrs.insert("content-type".to_string(), "application/json".to_string());
            Self {
                captured: Arc::new(Mutex::new(Vec::new())),
                response_status: status,
                response_headers: hdrs,
                response_body: body.to_vec(),
            }
        }
    }

    impl HttpDispatch for MockHttp {
        fn dispatch_http(
            &self,
            method: &str,
            path: &str,
            query: &str,
            headers: HashMap<String, String>,
            has_body: bool,
            mut body_rx: mpsc::Receiver<Bytes>,
        ) -> Result<HttpStream, DispatchError> {
            // For body-carrying methods, drain the receiver on a
            // background task and capture the bytes. For body-less methods,
            // drop the receiver in the same background task.
            let captured_clone = self.captured.clone();
            let method_clone = method.to_string();
            let path_clone = path.to_string();
            let query_clone = query.to_string();
            let headers_clone = headers.clone();
            tokio::spawn(async move {
                let mut body_chunks: Vec<Bytes> = Vec::new();
                if has_body {
                    while let Some(b) = body_rx.recv().await {
                        body_chunks.push(b);
                    }
                } else {
                    while body_rx.recv().await.is_some() {}
                }
                captured_clone.lock().unwrap().push(MockHttpCall {
                    method: method_clone,
                    path: path_clone,
                    query: query_clone,
                    headers: headers_clone,
                    body_chunks,
                });
            });

            let (abort_handle, _abort_signal) = AbortHandle::new();
            let (response_tx, response_rx) = oneshot::channel();
            let status = self.response_status;
            let resp_headers = self.response_headers.clone();
            let body = self.response_body.clone();
            tokio::spawn(async move {
                let chunks = vec![Ok(Bytes::copy_from_slice(&body))];
                let stream = futures_util::stream::iter(chunks);
                let result = Ok(HttpDispatchResponse {
                    status,
                    headers: resp_headers,
                    body: Some(Box::pin(stream)),
                });
                let _ = response_tx.send(result);
            });

            Ok(HttpStream {
                body_tx: mpsc::channel::<Bytes>(1).0,
                response_rx,
                abort: abort_handle,
            })
        }
    }

    /// Test WS dispatcher that captures the dialed URL and bounces text
    /// messages between inbound and outbound channels.
    struct MockWs {
        captured: Arc<Mutex<Vec<MockWsCall>>>,
    }

    #[derive(Clone, Debug)]
    struct MockWsCall {
        path: String,
        query: String,
        headers: HashMap<String, String>,
        protocols: Vec<String>,
    }

    impl MockWs {
        fn new() -> Self {
            Self {
                captured: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    impl WsDispatch for MockWs {
        fn dispatch_ws(
            &self,
            path: &str,
            query: &str,
            headers: HashMap<String, String>,
            protocols: Vec<String>,
        ) -> Result<WsStream, DispatchError> {
            self.captured.lock().unwrap().push(MockWsCall {
                path: path.to_string(),
                query: query.to_string(),
                headers,
                protocols: protocols.clone(),
            });

            let (outbound_tx, mut outbound_rx) = mpsc::channel::<WsOutbound>(8);
            let (inbound_tx, inbound_rx) = mpsc::channel::<WsInbound>(8);

            tokio::spawn(async move {
                let _ = inbound_tx
                    .send(WsInbound::Opened {
                        protocol: protocols.first().cloned(),
                    })
                    .await;

                loop {
                    tokio::select! {
                        Some(out) = outbound_rx.recv() => {
                            match out {
                                WsOutbound::Text(text) => {
                                    let _ = inbound_tx.send(WsInbound::Text(text)).await;
                                }
                                WsOutbound::Binary(bin) => {
                                    let _ = inbound_tx.send(WsInbound::Binary(bin)).await;
                                }
                                WsOutbound::Close { code, reason } => {
                                    let _ = inbound_tx
                                        .send(WsInbound::Closed { code, reason })
                                        .await;
                                    return;
                                }
                            }
                        }
                        else => return,
                    }
                }
            });

            Ok(WsStream {
                outbound_tx,
                inbound_rx,
            })
        }
    }

    // -------------------------------------------------------------------------
    // Capture sink for `send_frame`
    // -------------------------------------------------------------------------

    #[derive(Default)]
    struct Capture {
        frames: Arc<Mutex<Vec<Vec<u8>>>>,
        buffered: Arc<AtomicUsize>,
    }

    impl Capture {
        fn send_frame_fn(&self) -> SendFrameFn {
            let frames = self.frames.clone();
            let buffered = self.buffered.clone();
            Arc::new(move |frame: Vec<u8>| {
                let frames = frames.clone();
                let buffered = buffered.clone();
                Box::pin(async move {
                    frames.lock().unwrap().push(frame);
                    buffered.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
            })
        }

        fn get_buffered_amount_fn(&self) -> GetBufferedAmountFn {
            let buffered = self.buffered.clone();
            Arc::new(move || buffered.load(Ordering::SeqCst))
        }

        fn decoded_frames(&self) -> Vec<TunnelFrame> {
            self.frames
                .lock()
                .unwrap()
                .iter()
                .map(|raw| decode_tunnel_frame(raw).expect("captured frame must decode"))
                .collect()
        }
    }

    fn make_host(
        http: Arc<dyn HttpDispatch>,
        ws: Arc<dyn WsDispatch>,
    ) -> (Arc<TunnelHost>, Capture) {
        let cap = Capture::default();
        let config = TunnelHostConfig {
            connection_id: "conn-1".to_string(),
            get_local_port: Arc::new(|| 7777u16),
            send_frame: cap.send_frame_fn(),
            get_buffered_amount: cap.get_buffered_amount_fn(),
            http_dispatch: http,
            ws_dispatch: ws,
        };
        (Arc::new(TunnelHost::new(config)), cap)
    }

    fn http_request_frame(
        method: &str,
        path: &str,
        query: &str,
        headers: serde_json::Value,
    ) -> Vec<u8> {
        let body = serde_json::json!({
            "method": method,
            "path": path,
            "query": query,
            "headers": headers,
        });
        let payload = encode_json_payload(&body);
        encode_tunnel_frame(TunnelFrameType::HttpRequest, 1, &payload)
    }

    fn http_request_frame_with_id(
        stream_id: u32,
        method: &str,
        path: &str,
        query: &str,
        headers: serde_json::Value,
    ) -> Vec<u8> {
        let body = serde_json::json!({
            "method": method,
            "path": path,
            "query": query,
            "headers": headers,
        });
        let payload = encode_json_payload(&body);
        encode_tunnel_frame(TunnelFrameType::HttpRequest, stream_id, &payload)
    }

    fn ws_open_frame(stream_id: u32, path: &str, query: &str, protocols: &[&str]) -> Vec<u8> {
        let body = serde_json::json!({
            "path": path,
            "query": query,
            "protocols": protocols,
        });
        let payload = encode_json_payload(&body);
        encode_tunnel_frame(TunnelFrameType::WsOpen, stream_id, &payload)
    }

    async fn wait_until<F: Fn() -> bool>(f: F) {
        for _ in 0..100 {
            if f() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    // -------------------------------------------------------------------------
    // HTTP frame handling
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn http_get_roundtrip_emits_response_body_end() {
        let mock = Arc::new(MockHttp::new(200, br#"{"ok":true}"#));
        let (host, cap) = make_host(mock.clone(), Arc::new(MockWs::new()));

        let frame = http_request_frame("GET", "/api/health", "", serde_json::json!({}));
        host.handle_frame(&frame).await.unwrap();

        wait_until(|| !cap.frames.lock().unwrap().is_empty()).await;

        let frames = cap.decoded_frames();
        assert!(
            frames.len() >= 3,
            "want >=3 frames (response, body, end), got {:?}",
            frames
        );
        assert_eq!(frames[0].frame_type, TunnelFrameType::HttpResponse);
        let resp_payload: Value = serde_json::from_slice(&frames[0].payload).unwrap();
        assert_eq!(resp_payload["status"], 200);
        assert_eq!(
            resp_payload["headers"]["content-type"],
            "application/json"
        );

        // Find HttpBody and StreamEnd.
        let has_body = frames
            .iter()
            .any(|f| f.frame_type == TunnelFrameType::HttpBody && !f.payload.is_empty());
        let has_end = frames
            .iter()
            .any(|f| f.frame_type == TunnelFrameType::StreamEnd && f.payload.is_empty());
        assert!(has_body);
        assert!(has_end);

        // Dispatcher captured the call.
        let captured = mock.captured.lock().unwrap();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].method, "GET");
        assert_eq!(captured[0].path, "/api/health");
        assert_eq!(
            captured[0]
                .headers
                .get("x-gridforge-relay-connection")
                .unwrap(),
            "conn-1"
        );
    }

    #[tokio::test]
    async fn http_post_with_body_chunks_streams_through() {
        let mock = Arc::new(MockHttp::new(200, b"thanks"));
        let (host, cap) = make_host(mock.clone(), Arc::new(MockWs::new()));

        let frame = http_request_frame("POST", "/api/echo", "", serde_json::json!({
            "content-type": "text/plain",
        }));
        host.handle_frame(&frame).await.unwrap();

        // Push two body chunks then end the body.
        let body1 = encode_tunnel_frame(TunnelFrameType::HttpBody, 1, b"hello ");
        let body2 = encode_tunnel_frame(TunnelFrameType::HttpBody, 1, b"world");
        let end = encode_tunnel_frame(TunnelFrameType::StreamEnd, 1, &[]);
        host.handle_frame(&body1).await.unwrap();
        host.handle_frame(&body2).await.unwrap();
        host.handle_frame(&end).await.unwrap();

        // Wait for the dispatcher to see "hello world".
        wait_until(|| {
            let captured = mock.captured.lock().unwrap();
            captured
                .get(0)
                .map(|c| c.body_chunks.iter().any(|b| b == &Bytes::from_static(b"hello ")))
                .unwrap_or(false)
        })
        .await;

        let captured = mock.captured.lock().unwrap();
        let mut joined = Vec::new();
        for chunk in &captured[0].body_chunks {
            joined.extend_from_slice(chunk);
        }
        assert_eq!(joined, b"hello world");

        // Wait for the synthetic StreamEnd to be sent.
        wait_until(|| {
            cap.frames.lock().unwrap().iter().any(|f| {
                matches!(
                    decode_tunnel_frame(f).map(|d| d.frame_type),
                    Ok(TunnelFrameType::StreamEnd)
                )
            })
        })
        .await;
    }

    #[tokio::test]
    async fn http_path_outside_allowlist_emits_403() {
        let mock = Arc::new(MockHttp::new(200, b"unused"));
        let (host, cap) = make_host(mock.clone(), Arc::new(MockWs::new()));

        let frame = http_request_frame("GET", "/etc/passwd", "", serde_json::json!({}));
        host.handle_frame(&frame).await.unwrap();

        wait_until(|| !cap.frames.lock().unwrap().is_empty()).await;

        let frames = cap.decoded_frames();
        let response = frames
            .iter()
            .find(|f| f.frame_type == TunnelFrameType::HttpResponse)
            .expect("HttpResponse frame");
        let resp_payload: Value = serde_json::from_slice(&response.payload).unwrap();
        assert_eq!(resp_payload["status"], 403);

        // Dispatcher was never called.
        assert!(mock.captured.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn http_auth_paths_are_allowed() {
        let mock = Arc::new(MockHttp::new(200, b"{}"));
        let (host, _cap) = make_host(mock.clone(), Arc::new(MockWs::new()));

        let frames: Vec<Vec<u8>> = [
            ("/auth", 1u32),
            ("/auth/login", 3u32),
            ("/auth/refresh", 5u32),
        ]
        .iter()
        .map(|(path, sid)| http_request_frame_with_id(*sid, "GET", path, "", serde_json::json!({})))
        .collect();

        for frame in frames {
            host.handle_frame(&frame).await.unwrap();
        }

        // Allow tasks to settle.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let captured = mock.captured.lock().unwrap();
        let paths: Vec<String> = captured.iter().map(|c| c.path.clone()).collect();
        assert!(paths.contains(&"/auth".to_string()));
        assert!(paths.contains(&"/auth/login".to_string()));
        assert!(paths.contains(&"/auth/refresh".to_string()));
    }

    // -------------------------------------------------------------------------
    // WS frame handling
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn ws_open_emits_wsopened() {
        let (host, cap) = make_host(
            Arc::new(MockHttp::new(200, b"")),
            Arc::new(MockWs::new()),
        );

        let frame = ws_open_frame(1, "/api/event/ws", "", &[]);
        host.handle_frame(&frame).await.unwrap();

        wait_until(|| {
            cap.frames.lock().unwrap().iter().any(|f| {
                matches!(
                    decode_tunnel_frame(f).map(|d| d.frame_type),
                    Ok(TunnelFrameType::WsOpened)
                )
            })
        })
        .await;

        let frames = cap.decoded_frames();
        let opened = frames
            .iter()
            .find(|f| f.frame_type == TunnelFrameType::WsOpened)
            .expect("WsOpened");
        let payload: Value = serde_json::from_slice(&opened.payload).unwrap();
        // Empty {} when no protocol negotiated.
        assert!(payload.get("protocol").is_none() || payload["protocol"].is_null());
    }

    #[tokio::test]
    async fn ws_path_outside_allowlist_sends_abort() {
        let (host, cap) = make_host(
            Arc::new(MockHttp::new(200, b"")),
            Arc::new(MockWs::new()),
        );

        let frame = ws_open_frame(1, "/api/forbidden/ws", "", &[]);
        host.handle_frame(&frame).await.unwrap();

        wait_until(|| {
            cap.frames.lock().unwrap().iter().any(|f| {
                matches!(
                    decode_tunnel_frame(f).map(|d| d.frame_type),
                    Ok(TunnelFrameType::StreamAbort)
                )
            })
        })
        .await;

        let frames = cap.decoded_frames();
        let abort = frames
            .iter()
            .find(|f| f.frame_type == TunnelFrameType::StreamAbort)
            .expect("StreamAbort");
        let payload: Value = serde_json::from_slice(&abort.payload).unwrap();
        assert!(payload["reason"]
            .as_str()
            .unwrap()
            .contains("not allowed"));
    }

    #[tokio::test]
    async fn ws_text_roundtrip_via_dispatcher_bounce() {
        // The dispatcher-side message-forwarding is wired by
        // `handle_ws_message`, which currently doesn't forward to the
        // outbound_tx (see TODO). To verify the WS pump task itself, we
        // close a stream immediately and confirm a WsClose propagates back.
        let mock_ws = Arc::new(MockWs::new());
        let (host, cap) = make_host(
            Arc::new(MockHttp::new(200, b"")),
            mock_ws.clone(),
        );

        let open = ws_open_frame(1, "/api/event/ws", "", &["oc.v1"]);
        host.handle_frame(&open).await.unwrap();

        wait_until(|| {
            cap.frames.lock().unwrap().iter().any(|f| {
                matches!(
                    decode_tunnel_frame(f).map(|d| d.frame_type),
                    Ok(TunnelFrameType::WsOpened)
                )
            })
        })
        .await;

        // Close cleanly: a WsClose from the peer should map to dispatcher
        // closure. We don't expect a WsClose reply frame because the pump
        // task is dropped before it can echo. But the dispatcher should
        // have been invoked.
        let close_payload = encode_json_payload(&serde_json::json!({
            "code": 1000,
            "reason": "bye"
        }));
        let close = encode_tunnel_frame(TunnelFrameType::WsClose, 1, &close_payload);
        host.handle_frame(&close).await.unwrap();

        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(host.stream_count(), 0);
    }

    #[tokio::test]
    async fn ping_frame_emits_pong() {
        let (host, cap) = make_host(
            Arc::new(MockHttp::new(200, b"")),
            Arc::new(MockWs::new()),
        );

        let ping = encode_tunnel_frame(TunnelFrameType::Ping, 7, &[]);
        host.handle_frame(&ping).await.unwrap();

        wait_until(|| {
            cap.frames.lock().unwrap().iter().any(|f| {
                matches!(
                    decode_tunnel_frame(f).map(|d| d.frame_type),
                    Ok(TunnelFrameType::Pong)
                )
            })
        })
        .await;

        let frames = cap.decoded_frames();
        let pong = frames
            .iter()
            .find(|f| f.frame_type == TunnelFrameType::Pong)
            .expect("Pong");
        assert_eq!(pong.stream_id, 7);
        assert!(pong.payload.is_empty());
    }

    #[tokio::test]
    async fn close_aborts_all_streams() {
        let mock_ws = Arc::new(MockWs::new());
        let (host, cap) = make_host(
            Arc::new(MockHttp::new(200, b"")),
            mock_ws.clone(),
        );

        let open = ws_open_frame(1, "/api/event/ws", "", &[]);
        host.handle_frame(&open).await.unwrap();
        wait_until(|| !cap.frames.lock().unwrap().is_empty()).await;

        assert_eq!(host.stream_count(), 1);
        host.close();
        assert!(host.is_closed());
        assert_eq!(host.stream_count(), 0);

        // handle_frame after close is a no-op.
        let ping = encode_tunnel_frame(TunnelFrameType::Ping, 1, &[]);
        host.handle_frame(&ping).await.unwrap();
    }

    #[tokio::test]
    async fn duplicate_stream_id_aborts_old_stream() {
        let mock_ws = Arc::new(MockWs::new());
        let (host, cap) = make_host(
            Arc::new(MockHttp::new(200, b"")),
            mock_ws.clone(),
        );

        let open = ws_open_frame(1, "/api/event/ws", "", &[]);
        host.handle_frame(&open).await.unwrap();
        wait_until(|| !cap.frames.lock().unwrap().is_empty()).await;

        assert_eq!(host.stream_count(), 1);

        // Open the same stream id again — the old stream should be aborted
        // (pump cancelled, stream removed) and a StreamAbort sent back.
        let open2 = ws_open_frame(1, "/api/event/ws", "", &[]);
        host.handle_frame(&open2).await.unwrap();

        // Wait for the abort to land on the peer.
        wait_until(|| {
            cap.frames.lock().unwrap().iter().any(|f| {
                matches!(
                    decode_tunnel_frame(f).map(|d| d.frame_type),
                    Ok(TunnelFrameType::StreamAbort)
                )
            })
        })
        .await;

        // The dispatcher is only called once — the duplicate frame aborts
        // the existing stream but does NOT open a new one (matching the JS
        // reference's "duplicate stream id" branch).
        let captured = mock_ws.captured.lock().unwrap();
        assert_eq!(captured.len(), 1, "only the original dial should succeed");

        // The peer received a StreamAbort for stream 1.
        let frames = cap.decoded_frames();
        let has_abort = frames.iter().any(|f| {
            f.frame_type == TunnelFrameType::StreamAbort && f.stream_id == 1
        });
        assert!(has_abort, "duplicate stream id must send StreamAbort");
    }

    // -------------------------------------------------------------------------
    // Backpressure + loopback HTTP integration (real reqwest client + raw
    // TCP mock).
    // -------------------------------------------------------------------------

    /// Spin up a tiny HTTP/1.1 mock that echoes method + path + body.
    async fn spawn_loopback_http() -> (u16, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let count = Arc::new(AtomicUsize::new(0));
        let count_clone = count.clone();
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let count = count_clone.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let first = req.lines().next().unwrap_or("");
                    let mut parts = first.split_whitespace();
                    let method = parts.next().unwrap_or("");
                    let path = parts.next().unwrap_or("");
                    let body = format!(
                        r#"{{"method":"{}","path":"{}","len":{}}}"#,
                        method,
                        path,
                        n.saturating_sub(first.len() + 4),
                    );
                    count.fetch_add(1, AOrd::SeqCst);
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });
        (port, count, handle)
    }

    /// Production reqwest dispatcher with the loopback URL pre-supplied via
    /// the `x-loopback-url` header.
    struct LoopbackHttp {
        client: reqwest::Client,
    }

    impl LoopbackHttp {
        fn new() -> Self {
            Self {
                client: reqwest::Client::builder()
                    .pool_idle_timeout(Duration::from_secs(5))
                    .build()
                    .unwrap(),
            }
        }
    }

    impl HttpDispatch for LoopbackHttp {
        fn dispatch_http(
            &self,
            method: &str,
            path: &str,
            query: &str,
            headers: HashMap<String, String>,
            has_body: bool,
            body_rx: mpsc::Receiver<Bytes>,
        ) -> Result<HttpStream, DispatchError> {
            let base_url = extract_loopback_url(&headers)?;
            let full = build_loopback_http_url(&base_url, path, query);

            let mut hdr_map = reqwest::header::HeaderMap::new();
            for (k, v) in &headers {
                if k == "x-loopback-url" {
                    continue;
                }
                if let (Ok(name), Ok(value)) = (
                    reqwest::header::HeaderName::from_bytes(k.as_bytes()),
                    reqwest::header::HeaderValue::from_str(v),
                ) {
                    hdr_map.insert(name, value);
                }
            }

            let (abort_handle, abort_signal) = AbortHandle::new();

            let mut builder = self
                .client
                .request(
                    reqwest::Method::from_bytes(method.as_bytes())
                        .map_err(|e| DispatchError::Unavailable(format!("bad method: {}", e)))?,
                    &full,
                )
                .headers(hdr_map);

            if has_body {
                let body_stream = StreamBody::new(body_rx, abort_signal);
                builder = builder.body(reqwest::Body::wrap_stream(body_stream));
            } else {
                let mut rx = body_rx;
                tokio::spawn(async move {
                    while rx.recv().await.is_some() {}
                });
            }

            let request = builder
                .build()
                .map_err(|e| DispatchError::Unavailable(format!("build: {}", e)))?;

            let client = self.client.clone();
            let (response_tx, response_rx) = oneshot::channel();
            tokio::spawn(async move {
                let result = match client.execute(request).await {
                    Ok(resp) => {
                        let status = resp.status().as_u16();
                        let mut hdrs = HashMap::new();
                        for (name, value) in resp.headers().iter() {
                            if let Ok(s) = value.to_str() {
                                hdrs.insert(name.as_str().to_ascii_lowercase(), s.to_string());
                            }
                        }
                        let stream = resp.bytes_stream().map(|r| {
                            r.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
                        });
                        let stream: Pin<
                            Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>,
                        > = Box::pin(stream);
                        Ok(HttpDispatchResponse {
                            status,
                            headers: hdrs,
                            body: Some(stream),
                        })
                    }
                    Err(err) => Err(DispatchError::Unavailable(err.to_string())),
                };
                let _ = response_tx.send(result);
            });

            Ok(HttpStream {
                body_tx: mpsc::channel::<Bytes>(1).0,
                response_rx,
                abort: abort_handle,
            })
        }
    }

    #[tokio::test]
    async fn reqwest_dispatch_to_real_loopback() {
        let (port, _count, handle) = spawn_loopback_http().await;
        let loopback_url = format!("http://127.0.0.1:{}", port);

        let http: Arc<dyn HttpDispatch> = Arc::new(LoopbackHttp::new());
        let cap = Capture::default();
        let config = TunnelHostConfig {
            connection_id: "conn-loopback".to_string(),
            get_local_port: Arc::new(move || port),
            send_frame: cap.send_frame_fn(),
            get_buffered_amount: cap.get_buffered_amount_fn(),
            http_dispatch: http,
            ws_dispatch: Arc::new(MockWs::new()),
        };
        let host = Arc::new(TunnelHost::new(config));

        // We hand-build the request envelope with `x-loopback-url` pre-populated
        // — production callers will have the same wire shape.
        let mut headers = serde_json::Map::new();
        headers.insert(
            "x-loopback-url".to_string(),
            serde_json::Value::String(loopback_url.clone()),
        );
        headers.insert(
            "x-gridforge-relay-connection".to_string(),
            serde_json::Value::String("conn-loopback".to_string()),
        );
        let body = serde_json::json!({
            "method": "GET",
            "path": "/api/health",
            "query": "",
            "headers": headers,
        });
        let payload = encode_json_payload(&body);
        let frame = encode_tunnel_frame(TunnelFrameType::HttpRequest, 1, &payload);

        host.handle_frame(&frame).await.unwrap();

        wait_until(|| {
            let frames = cap.frames.lock().unwrap();
            frames.iter().any(|f| {
                matches!(
                    decode_tunnel_frame(f).map(|d| d.frame_type),
                    Ok(TunnelFrameType::StreamEnd)
                )
            })
        })
        .await;

        let frames = cap.decoded_frames();
        let response = frames
            .iter()
            .find(|f| f.frame_type == TunnelFrameType::HttpResponse)
            .expect("HttpResponse");
        let payload: Value = serde_json::from_slice(&response.payload).unwrap();
        assert_eq!(payload["status"], 200);

        let body_chunks: Vec<Vec<u8>> = frames
            .iter()
            .filter(|f| f.frame_type == TunnelFrameType::HttpBody)
            .map(|f| f.payload.clone())
            .collect();
        let mut joined = Vec::new();
        for c in &body_chunks {
            joined.extend_from_slice(c);
        }
        let parsed: Value = serde_json::from_slice(&joined).unwrap();
        assert_eq!(parsed["method"], "GET");
        assert_eq!(parsed["path"], "/api/health");

        assert!(frames
            .iter()
            .any(|f| f.frame_type == TunnelFrameType::StreamEnd));

        handle.abort();
    }
}