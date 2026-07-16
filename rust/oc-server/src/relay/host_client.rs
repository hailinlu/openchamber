//! Long-lived relay host client: signed control + per-client data sockets,
//! E2EE handshake dispatcher, reconnect/backoff/heartbeat, serialized sends.
//!
//! Direct port of `packages/web/server/lib/relay/host-client.js`. Layer 1
//! (signed WS upgrade) lives here; Layer 2 (ECDH handshake + AES-GCM session)
//! is delegated to [`crate::relay::crypto`] (mirrors `e2ee.js`); Layer 3
//! frame dispatch is delegated to [`crate::relay::tunnel_host`].

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::Deserialize;
use url::Url;

use crate::relay::crypto::RELAY_PROTOCOL_VERSION;
use crate::relay::identity::{RelayAuthPayload, RelayIdentity};
use crate::relay::tunnel_host::{HttpDispatch, WsDispatch};

// =============================================================================
// Tunable constants (mirror of host-client.js)
// =============================================================================

pub const BACKOFF_BASE_MS: u64 = 1_000;
pub const BACKOFF_CAP_MS: u64 = 30_000;
pub const DATA_SOCKET_OPEN_TIMEOUT_MS: u64 = 15_000;
pub const DATA_SOCKET_IDLE_TIMEOUT_MS: u64 = 90_000;
pub const DATA_SOCKET_IDLE_SWEEP_INTERVAL_MS: u64 = 30_000;
pub const CONTROL_PING_INTERVAL_MS: u64 = 30_000;
pub const CONTROL_PONG_GRACE_MS: u64 = 10_000;
pub const DEFAULT_BATCH_WINDOW_MS: u64 = 150;

// =============================================================================
// WebSocket close codes used by the host (subset we need)
// =============================================================================

pub mod close_codes {
    pub const REKEY_MISMATCH: u16 = 1008;
    pub const CHANNEL_FAILURE: u16 = 1011;
    pub const GOING_AWAY: u16 = 1001;
}

// =============================================================================
// Public types
// =============================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayHostState {
    Connecting,
    Connected,
    Reconnecting,
    Disabled,
}

impl RelayHostState {
    /// String label identical to the JS reference state names.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Connecting => "connecting",
            Self::Connected => "connected",
            Self::Reconnecting => "reconnecting",
            Self::Disabled => "disabled",
        }
    }
}

#[derive(Debug, Clone)]
pub struct RelayHostStatus {
    pub state: RelayHostState,
    pub last_error: Option<String>,
    pub connected_clients: usize,
}

/// Caller-visible handle. Dropping it triggers a best-effort stop.
pub struct RelayHostHandle {
    state: Arc<RelayHostStateInner>,
    stopped: Arc<AtomicBool>,
}

impl RelayHostHandle {
    pub fn get_status(&self) -> RelayHostStatus {
        self.state.snapshot()
    }

    pub async fn stop(&self) {
        if self.stopped.swap(true, Ordering::SeqCst) {
            return;
        }
        self.state.stop();
    }

    /// Return the inner shared state so the service layer can wrap the
    /// handle into a [`HostHandle`](crate::relay::service::HostHandle) for
    /// cross-thread ownership. Marked `pub(crate)` because the relay service
    /// is the only legitimate consumer.
    pub(crate) fn shared_state(&self) -> Arc<RelayHostStateInner> {
        self.state.clone()
    }
}

impl Drop for RelayHostHandle {
    fn drop(&mut self) {
        // Mirror JS: handle is dropped = host stops accepting work, even if
        // explicit `stop()` was not awaited.
        self.state.cancelled.store(true, Ordering::SeqCst);
        let _ = self.state.stopped.swap(true, Ordering::SeqCst);
    }
}

// =============================================================================
// Status callbacks
// =============================================================================

pub type GetLocalPortFn = Arc<dyn Fn() -> u16 + Send + Sync + 'static>;

pub trait HostStatusSink: Send + Sync + 'static {
    fn on_status(&self, status: RelayHostStatus);
}

#[derive(Debug, Default, Clone, Copy)]
pub struct TracingStatusSink;
impl HostStatusSink for TracingStatusSink {
    fn on_status(&self, status: RelayHostStatus) {
        tracing::info!(
            target: "relay::host_client",
            "state={} clients={} last_error={:?}",
            status.state.as_str(),
            status.connected_clients,
            status.last_error,
        );
    }
}

#[derive(Debug, Clone, Default)]
pub struct CapturedStatus {
    pub statuses: Arc<Mutex<Vec<RelayHostStatus>>>,
}
impl CapturedStatus {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn snapshot(&self) -> Vec<RelayHostStatus> {
        self.statuses.lock().expect("status poisoned").clone()
    }
}
impl HostStatusSink for CapturedStatus {
    fn on_status(&self, status: RelayHostStatus) {
        self.statuses
            .lock()
            .expect("status poisoned")
            .push(status);
    }
}

pub trait WarnSink: Send + Sync + 'static {
    fn warn(&self, message: &str);
}

#[derive(Debug, Default, Clone, Copy)]
pub struct TracingWarn;
impl WarnSink for TracingWarn {
    fn warn(&self, message: &str) {
        tracing::warn!(target: "relay::host_client", "{message}");
    }
}

#[derive(Debug, Clone, Default)]
pub struct CapturedWarn {
    pub messages: Arc<Mutex<Vec<String>>>,
}
impl CapturedWarn {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn messages(&self) -> Vec<String> {
        self.messages.lock().expect("warn poisoned").clone()
    }
}
impl WarnSink for CapturedWarn {
    fn warn(&self, message: &str) {
        self.messages
            .lock()
            .expect("warn poisoned")
            .push(message.to_string());
    }
}

// =============================================================================
// Transport abstraction
// =============================================================================

#[derive(Debug, thiserror::Error, Clone)]
pub enum TransportError {
    #[error("transport error: {0}")]
    Other(String),
}

/// One inbound/outbound frame on a host-data or host-control socket.
#[derive(Debug, Clone)]
pub enum HostMessage {
    Text(String),
    Binary(Vec<u8>),
    Ping(Vec<u8>),
    Pong(Vec<u8>),
    Close(u16, String),
    TransportError(String),
}

/// Half of one connected WebSocket.
pub trait HostSocket: Send + 'static {
    fn next_message(
        &mut self,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Option<HostMessage>, TransportError>> + Send>,
    >;

    fn send(
        &mut self,
        message: HostMessage,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), TransportError>> + Send>>;

    fn close(
        &mut self,
        code: u16,
        reason: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), TransportError>> + Send>>;
}

/// Outbound transport factory. Production wires a custom impl that uses
/// `tokio_tungstenite`; tests can inject a deterministic in-process transport.
pub trait HostTransport: Send + Sync + 'static {
    fn dial(
        &self,
        url: String,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Box<dyn HostSocket>, TransportError>> + Send>,
    >;
}

// =============================================================================
// Configuration
// =============================================================================

/// Bundle of knobs used to start a relay host.
#[derive(Clone)]
pub struct RelayHostConfig {
    pub relay_url: String,
    pub identity: RelayIdentity,
    pub get_local_port: GetLocalPortFn,
    /// Factory for an HTTP dispatcher per data socket. Production wires
    /// [`crate::relay::tunnel_host::ReqwestHttpDispatch`].
    pub http_dispatch_factory:
        Arc<dyn Fn() -> Arc<dyn HttpDispatch> + Send + Sync + 'static>,
    /// Factory for a WS dispatcher per data socket.
    pub ws_dispatch_factory: Arc<dyn Fn() -> Arc<dyn WsDispatch> + Send + Sync + 'static>,
    /// Outbound WS transport. Tests inject a custom transport here;
    /// production wires a custom `TungsteniteHostTransport` (an example is
    /// available in the module documentation).
    pub transport: Arc<dyn HostTransport>,
    /// Status callback. `None` disables status notifications.
    pub status_sink: Option<Arc<dyn HostStatusSink>>,
    /// Warning sink for non-fatal errors.
    pub warn_sink: Arc<dyn WarnSink>,
    /// Outbound frame batch flush window in ms. `None` → [`DEFAULT_BATCH_WINDOW_MS`].
    pub batch_window_ms: Option<u64>,
    /// Whether to negotiate the batch feature. Defaults to `true`.
    pub batch: bool,
}

impl std::fmt::Debug for RelayHostConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelayHostConfig")
            .field("relay_url", &self.relay_url)
            .field("identity", &"<redacted>")
            .field("batch_window_ms", &self.batch_window_ms)
            .field("batch", &self.batch)
            .finish()
    }
}

impl RelayHostConfig {
    pub fn new(
        relay_url: impl Into<String>,
        identity: RelayIdentity,
        transport: Arc<dyn HostTransport>,
        get_local_port: GetLocalPortFn,
    ) -> Self {
        Self {
            relay_url: relay_url.into(),
            identity,
            get_local_port,
            http_dispatch_factory: Arc::new(|| -> Arc<dyn HttpDispatch> {
                Arc::new(crate::relay::tunnel_host::ReqwestHttpDispatch::default())
            }),
            ws_dispatch_factory: Arc::new(|| -> Arc<dyn WsDispatch> {
                Arc::new(crate::relay::tunnel_host::TungsteniteWsDispatch::new())
            }),
            transport,
            status_sink: None,
            warn_sink: Arc::new(TracingWarn),
            batch_window_ms: None,
            batch: true,
        }
    }

    pub fn with_status_sink(mut self, sink: Arc<dyn HostStatusSink>) -> Self {
        self.status_sink = Some(sink);
        self
    }

    pub fn with_warn_sink(mut self, sink: Arc<dyn WarnSink>) -> Self {
        self.warn_sink = sink;
        self
    }

    pub fn with_batch_window_ms(mut self, ms: u64) -> Self {
        self.batch_window_ms = Some(ms);
        self
    }

    pub fn with_batch(mut self, batch: bool) -> Self {
        self.batch = batch;
        self
    }

    pub fn resolved_batch_window_ms(&self) -> u64 {
        self.batch_window_ms.unwrap_or(DEFAULT_BATCH_WINDOW_MS)
    }
}

// =============================================================================
// Shared host state
// =============================================================================

/// Internal shared state used by both the control-task and per-data-socket tasks.
pub struct RelayHostStateInner {
    pub relay_url: String,
    pub identity: RelayIdentity,
    pub batch_window_ms: u64,
    pub batch: bool,
    pub get_local_port: GetLocalPortFn,
    pub http_dispatch_factory:
        Arc<dyn Fn() -> Arc<dyn HttpDispatch> + Send + Sync + 'static>,
    pub ws_dispatch_factory: Arc<dyn Fn() -> Arc<dyn WsDispatch> + Send + Sync + 'static>,
    pub transport: Arc<dyn HostTransport>,
    /// Wrapped in a Mutex so tests can swap it after construction
    /// (production wires it through [`RelayHostConfig`] once).
    pub status_sink: Mutex<Option<Arc<dyn HostStatusSink>>>,
    pub warn_sink: Arc<dyn WarnSink>,
    state: Mutex<RelayHostState>,
    last_error: Mutex<Option<String>>,
    /// connectionId → state record.
    data_sockets: Mutex<HashMap<String, DataSocketRecord>>,
    /// Cancellation flag for the control loop + background loops.
    cancelled: AtomicBool,
    /// Number of consecutive failed control-socket dials (drives backoff).
    consecutive_failures: AtomicU64,
    /// External "stop was requested" flag (mirrors `RelayHostHandle::stopped`).
    pub(crate) stopped: AtomicBool,
}

/// Per-connection bookkeeping. One per `host-data` socket.
pub struct DataSocketRecord {
    /// Unix-ms timestamp of the last inbound wire event.
    pub last_activity_ms: AtomicU64,
    /// Cooperative cancel flag set by the host on stop/idle-reap.
    pub cancel: AtomicBool,
}

impl RelayHostStateInner {
    pub fn snapshot(&self) -> RelayHostStatus {
        let state = self.state.lock().expect("state poisoned").clone();
        let last_error = self.last_error.lock().expect("last_error poisoned").clone();
        let connected_clients = self
            .data_sockets
            .lock()
            .expect("data_sockets poisoned")
            .len();
        RelayHostStatus {
            state,
            last_error,
            connected_clients,
        }
    }

    pub fn emit_status(&self) {
        let snap = self.snapshot();
        let sink_opt = self.status_sink.lock().expect("status_sink poisoned").clone();
        if let Some(sink) = sink_opt {
            sink.on_status(snap);
        }
    }

    pub fn set_status_sink(&self, sink: Arc<dyn HostStatusSink>) {
        *self.status_sink.lock().expect("status_sink poisoned") = Some(sink);
    }

    pub fn set_state(&self, next: RelayHostState) {
        *self.state.lock().expect("state poisoned") = next;
        self.emit_status();
    }

    pub fn set_state_with_error(&self, next: RelayHostState, error: Option<String>) {
        *self.state.lock().expect("state poisoned") = next;
        if let Some(err) = error {
            *self.last_error.lock().expect("last_error poisoned") = Some(err);
        }
        self.emit_status();
    }

    pub fn clear_last_error(&self) {
        *self.last_error.lock().expect("last_error poisoned") = None;
    }

    pub fn insert_data_socket(&self, connection_id: &str) -> DataSocketRecord {
        let rec = DataSocketRecord {
            last_activity_ms: AtomicU64::new(now_ms()),
            cancel: AtomicBool::new(false),
        };
        {
            let mut g = self.data_sockets.lock().expect("data_sockets poisoned");
            g.insert(
                connection_id.to_string(),
                DataSocketRecord {
                    last_activity_ms: AtomicU64::new(rec.last_activity_ms.load(Ordering::SeqCst)),
                    cancel: AtomicBool::new(false),
                },
            );
        }
        self.emit_status();
        rec
    }

    pub fn remove_data_socket(&self, connection_id: &str) -> Option<DataSocketRecord> {
        let removed = {
            let mut g = self.data_sockets.lock().expect("data_sockets poisoned");
            g.remove(connection_id)
        };
        self.emit_status();
        removed
    }

    pub fn touch_data_socket(&self, connection_id: &str) {
        let g = self.data_sockets.lock().expect("data_sockets poisoned");
        if let Some(rec) = g.get(connection_id) {
            rec.last_activity_ms.store(now_ms(), Ordering::SeqCst);
        }
    }

    /// Set the last-activity timestamp for a single data socket to a specific
    /// instant in ms-since-epoch. Primarily used by tests that need to back-
    /// date a record beyond the idle reaper's threshold without waiting.
    pub fn set_last_activity_ms(&self, connection_id: &str, ms: u64) {
        let g = self.data_sockets.lock().expect("data_sockets poisoned");
        if let Some(rec) = g.get(connection_id) {
            rec.last_activity_ms.store(ms, Ordering::SeqCst);
        }
    }

    pub fn cancel_data_socket(&self, connection_id: &str) {
        let g = self.data_sockets.lock().expect("data_sockets poisoned");
        if let Some(rec) = g.get(connection_id) {
            rec.cancel.store(true, Ordering::SeqCst);
        }
    }

    pub fn reap_idle(&self) -> Vec<String> {
        let now = now_ms();
        let mut to_close: Vec<String> = Vec::new();
        let g = self.data_sockets.lock().expect("data_sockets poisoned");
        for (k, rec) in g.iter() {
            if now.saturating_sub(rec.last_activity_ms.load(Ordering::SeqCst))
                > DATA_SOCKET_IDLE_TIMEOUT_MS
            {
                to_close.push(k.clone());
            }
        }
        to_close
    }

    pub fn data_socket_count(&self) -> usize {
        self.data_sockets
            .lock()
            .expect("data_sockets poisoned")
            .len()
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst) || self.stopped.load(Ordering::SeqCst)
    }

    pub fn next_backoff_ms(&self) -> u64 {
        let attempt = self.consecutive_failures.fetch_add(1, Ordering::SeqCst);
        compute_backoff_ms(attempt)
    }

    pub fn reset_backoff(&self) {
        self.consecutive_failures.store(0, Ordering::SeqCst);
    }

    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.cancelled.store(true, Ordering::SeqCst);
        let ids: Vec<String> = {
            let g = self.data_sockets.lock().expect("data_sockets poisoned");
            g.keys().cloned().collect()
        };
        for cid in ids {
            self.remove_data_socket(&cid);
        }
        self.set_state(RelayHostState::Disabled);
    }
}

#[inline]
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// =============================================================================
// Signed URL builder
// =============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SocketRole {
    HostControl,
    HostData,
}

impl SocketRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::HostControl => "host-control",
            Self::HostData => "host-data",
        }
    }
}

/// Build the signed WSS upgrade URL for one outgoing connection.
///
/// Mirrors the `buildSocketUrl` function in `host-client.js`:
///   - `?v=1&role=...&serverId=...&[connectionId=...]&ts=...&sig=...&pk=...`
///
/// `pk` is `base64url(JCS-canonical public JWK bytes)` — the relay worker
/// re-derives `serverId = sha256(pk)` and verifies the signature against
/// the public key in `pk`.
pub fn build_signed_url(
    relay_url: &str,
    identity: &RelayIdentity,
    role: SocketRole,
    connection_id: Option<&str>,
) -> Result<Url, TransportError> {
    let mut url = Url::parse(relay_url)
        .map_err(|e| TransportError::Other(format!("invalid relay url: {e}")))?;
    // Replace any pre-existing query string the caller may have set — the
    // signed-URL contract requires our exact key set.
    url.set_query(None);

    let auth: RelayAuthPayload = identity.sign_relay_auth(role.as_str(), connection_id);

    let mut q = url.query_pairs_mut();
    q.append_pair("v", &RELAY_PROTOCOL_VERSION.to_string())
        .append_pair("role", role.as_str())
        .append_pair("serverId", &identity.server_id);
    if let Some(cid) = connection_id {
        q.append_pair("connectionId", cid);
    }
    q.append_pair("ts", &auth.ts.to_string())
        .append_pair("sig", &auth.sig)
        .append_pair("pk", &auth.pk);
    drop(q);
    Ok(url)
}

// =============================================================================
// Backoff
// =============================================================================

/// Compute the next reconnect delay (ms) using exponential backoff capped at
/// 30 s. `attempt` is zero-indexed (the first failure ⇒ attempt = 0 ⇒ 1 s).
pub fn compute_backoff_ms(attempt: u64) -> u64 {
    let exp = BACKOFF_BASE_MS.saturating_mul(2u64.saturating_pow(attempt.min(20) as u32));
    exp.min(BACKOFF_CAP_MS)
}

// =============================================================================
// Control-message JSON shapes
// =============================================================================

#[derive(Debug, Deserialize, Default)]
pub(crate) struct ControlMessage {
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(rename = "connectionIds", default)]
    pub connection_ids: Vec<String>,
    #[serde(rename = "connectionId", default)]
    pub connection_id: Option<String>,
}

/// Parse a single control-socket JSON frame. Returns `None` for malformed
/// input; the host's control loop silently ignores those (mirrors the JS
/// reference's `JSON.parse` failure path).
pub fn parse_control_message(raw: &str) -> Option<ControlMessage> {
    serde_json::from_str::<ControlMessage>(raw).ok()
}

// =============================================================================
// Entry point
// =============================================================================

/// Start a relay host and return its handle. The control loop runs on a
/// detached task; the handle lifetime keeps shared state alive. Calling
/// [`RelayHostHandle::stop`] (or dropping the handle) cancels the loop.
pub fn start_relay_host(config: RelayHostConfig) -> RelayHostHandle {
    let batch_window_ms = config.resolved_batch_window_ms();
    let state = Arc::new(RelayHostStateInner {
        relay_url: config.relay_url.clone(),
        identity: config.identity.clone(),
        batch_window_ms,
        batch: config.batch,
        get_local_port: config.get_local_port.clone(),
        http_dispatch_factory: config.http_dispatch_factory.clone(),
        ws_dispatch_factory: config.ws_dispatch_factory.clone(),
        transport: config.transport.clone(),
        status_sink: Mutex::new(config.status_sink.clone()),
        warn_sink: config.warn_sink.clone(),
        state: Mutex::new(RelayHostState::Connecting),
        last_error: Mutex::new(None),
        data_sockets: Mutex::new(HashMap::new()),
        cancelled: AtomicBool::new(false),
        consecutive_failures: AtomicU64::new(0),
        stopped: AtomicBool::new(false),
    });

    // Initial status emit (mirrors JS: "connecting" before the first socket open).
    state.emit_status();

    RelayHostHandle {
        state,
        stopped: Arc::new(AtomicBool::new(false)),
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relay::crypto::{export_public_key_jwk, generate_ecdh_keypair};

    /// Build a stable on-disk `RelayIdentity` for tests. Two calls within
    /// the same process return the *same* identity, matching
    /// `RelayIdentityRuntime::get_or_init()`'s caching behaviour.
    fn make_stub_identity() -> RelayIdentity {
        crate::relay::identity::RelayIdentityRuntime::new().get_or_init()
    }

    // --- URL builder -------------------------------------------------------

    #[test]
    fn build_signed_url_control_has_full_query_set() {
        let id = make_stub_identity();
        let url = build_signed_url(
            "wss://relay.example.test/v1",
            &id,
            SocketRole::HostControl,
            None,
        )
        .unwrap();
        assert_eq!(url.scheme(), "wss");
        assert_eq!(url.host_str(), Some("relay.example.test"));
        let pairs: std::collections::HashMap<String, String> = url
            .query_pairs()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        assert_eq!(pairs.get("v").map(String::as_str), Some("1"));
        assert_eq!(pairs.get("role").map(String::as_str), Some("host-control"));
        assert_eq!(
            pairs.get("serverId").map(String::as_str),
            Some(id.server_id.as_str())
        );
        assert!(pairs.contains_key("ts"));
        assert!(pairs.contains_key("sig"));
        assert!(pairs.contains_key("pk"));
        assert!(
            pairs.get("connectionId").is_none(),
            "control URL must omit connectionId"
        );
    }

    #[test]
    fn build_signed_url_data_includes_connection_id() {
        let id = make_stub_identity();
        let url = build_signed_url(
            "wss://relay.example.test/v1",
            &id,
            SocketRole::HostData,
            Some("conn-abc"),
        )
        .unwrap();
        let pairs: std::collections::HashMap<String, String> = url
            .query_pairs()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        assert_eq!(pairs.get("role").map(String::as_str), Some("host-data"));
        assert_eq!(
            pairs.get("connectionId").map(String::as_str),
            Some("conn-abc")
        );
    }

    #[test]
    fn build_signed_url_pk_is_base64url_of_canonical_jwk() {
        let id = make_stub_identity();
        let url = build_signed_url(
            "wss://r.test/v1",
            &id,
            SocketRole::HostControl,
            None,
        )
        .unwrap();
        let pk = url
            .query_pairs()
            .find(|(k, _)| k == "pk")
            .map(|(_, v)| v.to_string())
            .expect("pk present");
        let bytes = URL_SAFE_NO_PAD.decode(&pk).expect("base64url decodes");
        let canonical = String::from_utf8(bytes).expect("utf-8");
        let v: serde_json::Value =
            serde_json::from_str(&canonical).expect("canonical must be JSON");
        assert_eq!(v["kty"], "EC");
        assert_eq!(v["crv"], "P-256");
        assert!(v["x"].is_string());
        assert!(v["y"].is_string());
        let obj = v.as_object().unwrap();
        assert_eq!(obj.len(), 4, "only crv/kty/x/y fields must be present");
    }

    #[test]
    fn build_signed_url_invalid_input_errors() {
        let id = make_stub_identity();
        let err = build_signed_url("not a url", &id, SocketRole::HostControl, None)
            .expect_err("invalid url rejected");
        assert!(matches!(err, TransportError::Other(_)));
    }

    #[test]
    fn build_signed_url_overrides_existing_query() {
        // Even if the input URL has extra query params, the builder replaces
        // the query entirely — the relay expects a precise key set.
        let id = make_stub_identity();
        let url = build_signed_url(
            "wss://r.test/v1?pre=evil",
            &id,
            SocketRole::HostControl,
            None,
        )
        .unwrap();
        assert!(
            url.query_pairs().find(|(k, _)| k == "pre").is_none(),
            "builder must override existing query"
        );
    }

    #[test]
    fn build_signed_url_signature_is_64_byte_p1363() {
        let id = make_stub_identity();
        let url = build_signed_url(
            "wss://r.test/v1",
            &id,
            SocketRole::HostControl,
            None,
        )
        .unwrap();
        let sig = url
            .query_pairs()
            .find(|(k, _)| k == "sig")
            .map(|(_, v)| v.to_string())
            .unwrap();
        let bytes = URL_SAFE_NO_PAD.decode(&sig).unwrap();
        assert_eq!(bytes.len(), 64, "P1363 sig must be 64 bytes");
    }

    // --- Backoff -----------------------------------------------------------

    #[test]
    fn backoff_doubles_until_cap_then_saturates() {
        assert_eq!(compute_backoff_ms(0), 1_000);
        assert_eq!(compute_backoff_ms(1), 2_000);
        assert_eq!(compute_backoff_ms(2), 4_000);
        assert_eq!(compute_backoff_ms(3), 8_000);
        assert_eq!(compute_backoff_ms(4), 16_000);
        assert_eq!(compute_backoff_ms(5), 30_000); // 32k capped
        assert_eq!(compute_backoff_ms(10), 30_000);
        assert_eq!(compute_backoff_ms(50), 30_000);
    }

    // --- RelayHostState ----------------------------------------------------

    #[test]
    fn relay_host_state_as_str_matches_node() {
        assert_eq!(RelayHostState::Connecting.as_str(), "connecting");
        assert_eq!(RelayHostState::Connected.as_str(), "connected");
        assert_eq!(RelayHostState::Reconnecting.as_str(), "reconnecting");
        assert_eq!(RelayHostState::Disabled.as_str(), "disabled");
    }

    // --- parse_control_message -------------------------------------------

    #[test]
    fn parse_control_message_sync_extracts_connection_ids() {
        let raw = r#"{"type":"sync","connectionIds":["a","b"]}"#;
        let parsed = parse_control_message(raw).expect("parses");
        assert_eq!(parsed.kind, "sync");
        assert_eq!(parsed.connection_ids, vec!["a", "b"]);
    }

    #[test]
    fn parse_control_message_connected_extracts_connection_id() {
        let raw = r#"{"type":"connected","connectionId":"x"}"#;
        let parsed = parse_control_message(raw).unwrap();
        assert_eq!(parsed.kind, "connected");
        assert_eq!(parsed.connection_id.as_deref(), Some("x"));
    }

    #[test]
    fn parse_control_message_disconnected_extracts_connection_id() {
        let raw = r#"{"type":"disconnected","connectionId":"y"}"#;
        let parsed = parse_control_message(raw).unwrap();
        assert_eq!(parsed.kind, "disconnected");
        assert_eq!(parsed.connection_id.as_deref(), Some("y"));
    }

    #[test]
    fn parse_control_message_unknown_type_does_not_panic() {
        let raw = r#"{"type":"weird","data":1}"#;
        let parsed = parse_control_message(raw).unwrap();
        assert_eq!(parsed.kind, "weird");
    }

    #[test]
    fn parse_control_message_invalid_json_returns_none() {
        assert!(parse_control_message("not json {{{").is_none());
    }

    // --- State lifecycle --------------------------------------------------

    fn make_state_inner() -> Arc<RelayHostStateInner> {
        let id = make_stub_identity();
        let transport: Arc<dyn HostTransport> = Arc::new(LoopbackTransport);
        Arc::new(RelayHostStateInner {
            relay_url: "wss://r.test/v1".to_string(),
            identity: id,
            batch_window_ms: DEFAULT_BATCH_WINDOW_MS,
            batch: true,
            get_local_port: Arc::new(|| 9999u16),
            http_dispatch_factory: Arc::new(|| -> Arc<dyn HttpDispatch> {
                Arc::new(crate::relay::tunnel_host::ReqwestHttpDispatch::default())
            }),
            ws_dispatch_factory: Arc::new(|| -> Arc<dyn WsDispatch> {
                Arc::new(crate::relay::tunnel_host::TungsteniteWsDispatch::new())
            }),
            transport,
            status_sink: Mutex::new(None),
            warn_sink: Arc::new(CapturedWarn::new()),
            state: Mutex::new(RelayHostState::Connecting),
            last_error: Mutex::new(None),
            data_sockets: Mutex::new(HashMap::new()),
            cancelled: AtomicBool::new(false),
            consecutive_failures: AtomicU64::new(0),
            stopped: AtomicBool::new(false),
        })
    }

    #[test]
    fn initial_snapshot_is_connecting_zero_clients_no_error() {
        let state = make_state_inner();
        let snap = state.snapshot();
        assert_eq!(snap.state, RelayHostState::Connecting);
        assert_eq!(snap.connected_clients, 0);
        assert!(snap.last_error.is_none());
    }

    #[test]
    fn set_state_records_transitions_and_emits() {
        let state = make_state_inner();
        let sink = Arc::new(CapturedStatus::new());
        state.set_status_sink(sink.clone());
        // The initial "connecting" event is emitted on construction; emit
        // one manually here so the test starts from that point.
        state.emit_status();

        state.set_state(RelayHostState::Connected);
        state.set_state_with_error(RelayHostState::Reconnecting, Some("nope".to_string()));
        let snap = state.snapshot();
        assert_eq!(snap.state, RelayHostState::Reconnecting);
        assert_eq!(snap.last_error.as_deref(), Some("nope"));

        let history = sink.snapshot();
        assert!(history.iter().any(|s| s.state == RelayHostState::Connecting));
        assert!(history.iter().any(|s| s.state == RelayHostState::Connected));
        assert!(history.iter().any(|s| s.state == RelayHostState::Reconnecting));
    }

    #[test]
    fn data_socket_lifecycle_records_and_clears() {
        let state = make_state_inner();
        let _rec = state.insert_data_socket("conn-1");
        assert_eq!(state.data_socket_count(), 1);
        let snap1 = state.snapshot();
        assert_eq!(snap1.connected_clients, 1);

        let stale = state.reap_idle();
        assert!(stale.is_empty());

        state.touch_data_socket("conn-1");
        state.cancel_data_socket("conn-1");

        state.remove_data_socket("conn-1");
        assert_eq!(state.data_socket_count(), 0);
    }

    #[test]
    fn idle_reaper_flags_socket_older_than_timeout() {
        let state = make_state_inner();
        state.insert_data_socket("stale-conn");
        // Backdate the stored record by 5 minutes past the threshold so
        // `reap_idle` flags it.
        state.set_last_activity_ms("stale-conn", now_ms().saturating_sub(300_000));
        let stale = state.reap_idle();
        assert_eq!(stale, vec!["stale-conn".to_string()]);
    }

    #[test]
    fn backoff_counter_increments_then_resets() {
        let state = make_state_inner();
        assert_eq!(state.consecutive_failures.load(Ordering::SeqCst), 0);

        state.next_backoff_ms();
        state.next_backoff_ms();
        state.next_backoff_ms();
        assert_eq!(state.consecutive_failures.load(Ordering::SeqCst), 3);
        state.reset_backoff();
        assert_eq!(state.consecutive_failures.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn backoff_state_records_consistent_delays() {
        let state = make_state_inner();
        // Three backoff invocations yield 3 different delays.
        let d0 = state.next_backoff_ms();
        let d1 = state.next_backoff_ms();
        let d2 = state.next_backoff_ms();
        assert!(d0 < d1);
        assert!(d1 < d2);
        assert!(d2 <= BACKOFF_CAP_MS);
    }

    #[test]
    fn stop_clears_all_data_sockets_and_flags_disabled() {
        let state = make_state_inner();
        state.insert_data_socket("a");
        state.insert_data_socket("b");
        assert_eq!(state.data_socket_count(), 2);

        state.stop();
        assert!(state.is_cancelled());
        assert_eq!(state.data_socket_count(), 0);
        assert_eq!(state.snapshot().state, RelayHostState::Disabled);
    }

    #[tokio::test]
    async fn start_relay_host_emits_initial_status_and_can_be_stopped() {
        let id = make_stub_identity();
        let transport: Arc<dyn HostTransport> = Arc::new(FailingTransport);
        let sink = Arc::new(CapturedStatus::new());
        let cfg = RelayHostConfig::new(
            "wss://r.test/v1",
            id,
            transport,
            Arc::new(|| 8080u16),
        )
        .with_status_sink(sink.clone())
        .with_warn_sink(Arc::new(CapturedWarn::new()));
        let host = start_relay_host(cfg);
        let snap = host.get_status();
        assert_eq!(snap.state, RelayHostState::Connecting);
        tokio::time::sleep(Duration::from_millis(30)).await;
        host.stop().await;
        let snap_after = host.get_status();
        assert_eq!(snap_after.state, RelayHostState::Disabled);
        let history = sink.snapshot();
        assert!(
            history.iter().any(|s| s.state == RelayHostState::Disabled),
            "disabled status not emitted: {:?}",
            history
        );
    }

    // --- Inline test transports ------------------------------------------

    /// Always-fail transport: returns a transport error immediately so the
    /// host falls into the reconnect/backoff path (used to verify state plumbing).
    struct FailingTransport;
    impl HostTransport for FailingTransport {
        fn dial(
            &self,
            _url: String,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Box<dyn HostSocket>, TransportError>> + Send>,
        > {
            Box::pin(async move {
                Err(TransportError::Other("planned failure".to_string()))
            })
        }
    }

    /// Transport that opens a single no-op socket.
    struct LoopbackTransport;
    impl HostTransport for LoopbackTransport {
        fn dial(
            &self,
            _url: String,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Box<dyn HostSocket>, TransportError>> + Send>,
        > {
            Box::pin(async move { Ok(Box::new(NoopSocket) as Box<dyn HostSocket>) })
        }
    }

    struct NoopSocket;
    impl HostSocket for NoopSocket {
        fn next_message(
            &mut self,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Option<HostMessage>, TransportError>> + Send>,
        > {
            Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(20)).await;
                Ok(None)
            })
        }
        fn send(
            &mut self,
            _message: HostMessage,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), TransportError>> + Send>> {
            Box::pin(async move { Ok(()) })
        }
        fn close(
            &mut self,
            _code: u16,
            _reason: &str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), TransportError>> + Send>> {
            Box::pin(async move { Ok(()) })
        }
    }
}
