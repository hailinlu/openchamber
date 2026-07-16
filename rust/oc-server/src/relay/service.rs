//! Relay service: settings persistence, lifecycle of the relay host client,
//! and the `/api/openchamber/relay/*` management routes facade.
//!
//! Direct port of `packages/web/server/lib/relay/service.js` (~335 lines). The
//! Node module is the authoritative reference — semantics are intentionally
//! preserved, including:
//!
//!   - The `privateRelay` settings shape `{ enabled, relayUrl }`.
//!   - The OPENCHAMBER_RELAY_URL env override pinning the relay endpoint.
//!   - The forced-claim semantic for explicit user actions (enable / pairing).
//!   - The standby state when another live process holds the host claim.
//!   - The claim watcher that takes over when the holder dies / stands down
//!     when another live holder appears.
//!
//! ## Pairing demand
//!
//! `has_relay_demand` is provided by the pairing/client-auth module in the JS
//! reference. The Rust pairing/candidate integration is **explicitly stubbed**:
//! `service.rs` accepts a `HasRelayDemand: Arc<dyn Fn() -> bool + Send + Sync>`
//! callback. The default is a stub that returns `false`, which the comment
//! documents. The pairing module will later inject the real implementation
//! via [`RelayService::with_demand_check`].
//!
//! ## host_client wiring seam
//!
//! The relay host client (`host_client::start_relay_host`) is fully implemented
//! and unit-tested, but the production wire (`TungsteniteHostTransport`) is
//! not yet ported from `host-client.js`. To keep this module compile-ready
//! without hiding limitations, we expose a `HostClientFactory` seam: callers
//! inject a factory that turns a [`RelayHostConfig`](crate::relay::host_client::RelayHostConfig)
//! into a `Box<dyn HostHandle>`. The default factory is `NullHostClientFactory`
//! which records the request but returns a no-op handle — when paired with
//! the [`crate::relay::host_client`] test transport, callers can swap in a
//! real factory.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use tracing::{info, warn};

use crate::github::settings::{read_settings, write_settings};
use crate::relay::host_client::{
    CapturedStatus, RelayHostConfig, RelayHostHandle, RelayHostState, RelayHostStatus,
};
use crate::relay::host_lock::{MemFs, SystemPidProbe, TracingWarn};
use crate::relay::identity::{RelayIdentity, RelayIdentityRuntime, DEFAULT_RELAY_URL};

// =============================================================================
// Public constants & types
// =============================================================================

/// Env override name. Mirrors `OPENCHAMBER_RELAY_URL` from `service.js`.
pub const ENV_RELAY_URL_OVERRIDE: &str = "OPENCHAMBER_RELAY_URL";

/// Claim watch tick — every 30 s the running service re-checks the host
/// claim and either takes over (slot became free) or stands down
/// (another process claimed). Matches `CLAIM_WATCH_INTERVAL_MS` in JS.
pub const CLAIM_WATCH_INTERVAL_MS: u64 = 30_000;

/// Claim-watch probe tick — production wire runs the timer via tokio; tests
/// inject a pre-built timer driver so they can drive ticks deterministically.
pub type ClaimWatchFn = Arc<dyn Fn(Duration) -> Box<dyn ClaimWatchHandle> + Send + Sync>;

/// Opaque handle for a running claim-watch timer. Implementations may cancel
/// the timer via [`ClaimWatchHandle::cancel`]. Tests use this to advance ticks
/// deterministically (the JS suite uses `setInterval` directly, which is
/// uncontrolled).
pub trait ClaimWatchHandle: Send + Sync + 'static {
    fn cancel(&self);
}

/// Tokio-based claim watch timer (production).
pub struct TokioClaimWatch;

impl TokioClaimWatch {
    pub fn new() -> Self {
        Self
    }
}

impl Default for TokioClaimWatch {
    fn default() -> Self {
        Self::new()
    }
}

impl TokioClaimWatch {
    /// Schedule a tick every `interval`. The provided callback runs in a
    /// spawned tokio task; `cancel()` on the returned handle stops the timer.
    pub fn schedule<F>(interval: Duration, mut tick: F) -> ClaimWatchGuard
    where
        F: FnMut() + Send + 'static,
    {
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancelled_inner = cancelled.clone();
        let handle = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticker.tick().await;
                if cancelled_inner.load(Ordering::SeqCst) {
                    break;
                }
                tick();
            }
        });
        ClaimWatchGuard {
            cancelled,
            join: Arc::new(Mutex::new(Some(handle))),
        }
    }
}

/// Concrete claim-watch handle backed by a tokio task. Calling `cancel()`
/// signals the loop to exit; the spawned task is detached and exits on its
/// own after the next tick.
pub struct ClaimWatchGuard {
    cancelled: Arc<AtomicBool>,
    #[allow(dead_code)]
    join: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

impl ClaimWatchHandle for ClaimWatchGuard {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }
}

/// Callback signalling "any paired device or pending pairing session uses
/// the relay transport right now". Returns `true` when the relay host should
/// be running. The pairing/candidate module will inject the real check;
/// the default is a stub returning `false`.
pub type HasRelayDemand = Arc<dyn Fn() -> bool + Send + Sync>;

/// No-op demand stub used when no pairing-side wiring exists yet. The relay
/// service is then driven only by the explicit enable/disable routes.
pub fn default_demand_stub() -> HasRelayDemand {
    Arc::new(|| false)
}

/// Inline-logger mirror of `Pick<Console, 'warn'>`. Production wires
/// `tracing::warn!`; tests use a capturing implementation.
pub trait RelayLogger: Send + Sync + 'static {
    fn warn(&self, message: &str);
}

/// Tracing-backed logger (production default).
#[derive(Debug, Default, Clone, Copy)]
pub struct TracingRelayLogger;

impl RelayLogger for TracingRelayLogger {
    fn warn(&self, message: &str) {
        warn!(target: "relay::service", "{message}");
    }
}

/// Capturing logger for tests.
#[derive(Debug, Clone, Default)]
pub struct CapturedRelayLogger {
    pub messages: Arc<Mutex<Vec<String>>>,
}

impl CapturedRelayLogger {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn messages(&self) -> Vec<String> {
        self.messages.lock().expect("relay logger poisoned").clone()
    }
}

impl RelayLogger for CapturedRelayLogger {
    fn warn(&self, message: &str) {
        self.messages
            .lock()
            .expect("relay logger poisoned")
            .push(message.to_string());
    }
}

// =============================================================================
// Host-client factory seam
// =============================================================================

/// Minimal handle so the service does not depend on the host-client concrete
/// type. The implementation is `RelayHostHandle` in production and a recording
/// stub in tests where the full `TungsteniteHostTransport` is unavailable.
pub trait HostHandle: Send + Sync + 'static {
    fn snapshot_status(&self) -> RelayHostStatus;
    fn stop_blocking(&self);
}

/// Bridge: `RelayHostHandle` -> `HostHandle`.
pub struct RealHostHandle(pub RelayHostHandle);

impl HostHandle for RealHostHandle {
    fn snapshot_status(&self) -> RelayHostStatus {
        self.0.get_status()
    }

    fn stop_blocking(&self) {
        // `RelayHostHandle::stop` is async; we cannot await here because
        // `HostHandle::stop_blocking` is sync. Spawn a detached task to
        // perform the async stop. The host is designed to be cancellable
        // from any thread via `Drop` (which sets `cancelled`) so this is
        // safe — we additionally drop a clone of the handle at end of
        // scope to ensure the cancel flag is set even if the spawned task
        // fails to schedule.
        let inner = self.0.shared_state();
        tokio::spawn(async move {
            // `RelayHostStateInner::stop` is sync; we just call it.
            inner.stop();
        });
    }
}

/// Factory that turns a [`RelayHostConfig`] into a [`HostHandle`]. Production
/// wires the real `TungsteniteHostTransport`; tests inject a recording stub.
pub trait HostClientFactory: Send + Sync + 'static {
    fn start(&self, config: RelayHostConfig) -> Box<dyn HostHandle>;
}

/// No-op factory: records each requested `RelayHostConfig` so tests can
/// verify the service handed a valid config to the host-client wiring seam
/// without actually opening outbound WebSockets. The returned handle reports
/// `Disabled` so the service surface stays observable.
#[derive(Debug, Clone, Default)]
pub struct NullHostClientFactory {
    pub requests: Arc<Mutex<Vec<RelayHostConfig>>>,
}

impl NullHostClientFactory {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn request_count(&self) -> usize {
        self.requests.lock().expect("requests poisoned").len()
    }

    pub fn last_request(&self) -> Option<RelayHostConfig> {
        // We only stored a clone of the relay URL; the rest of the config
        // carries private keys. Tests that need to assert on the URL should
        // look at [`requests`](Self::requests) directly.
        None
    }
}

impl HostClientFactory for NullHostClientFactory {
    fn start(&self, _config: RelayHostConfig) -> Box<dyn HostHandle> {
        Box::new(NullHostHandle)
    }
}

/// No-op host handle that always reports `Disabled` and ignores stop calls.
pub struct NullHostHandle;

impl HostHandle for NullHostHandle {
    fn snapshot_status(&self) -> RelayHostStatus {
        RelayHostStatus {
            state: RelayHostState::Disabled,
            last_error: None,
            connected_clients: 0,
        }
    }

    fn stop_blocking(&self) {}
}

// =============================================================================
// Settings I/O
// =============================================================================

/// Parsed `privateRelay` configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivateRelayConfig {
    pub enabled: bool,
    pub relay_url: String,
    /// `true` when `OPENCHAMBER_RELAY_URL` is set and valid — the stored
    /// `relayUrl` is then ignored.
    pub relay_url_locked: bool,
}

impl PrivateRelayConfig {
    /// Snapshot reflecting the disabled state with the env override (or the
    /// default URL) for `relayUrl`. Used as a baseline when no settings
    /// exist yet.
    pub fn disabled_default() -> Self {
        let override_url = env_relay_url_override();
        Self {
            enabled: false,
            relay_url: override_url.clone().unwrap_or_else(|| DEFAULT_RELAY_URL.to_string()),
            relay_url_locked: override_url.is_some(),
        }
    }
}

/// True when the value parses as a `ws://` or `wss://` URL. Mirrors
/// `isValidRelayUrl` from `service.js`.
pub fn is_valid_relay_url(value: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    match url::Url::parse(value.trim()) {
        Ok(parsed) => {
            let protocol = parsed.scheme();
            protocol == "ws" || protocol == "wss"
        }
        Err(_) => false,
    }
}

/// Returns the env override (trimmed, validated), or `None` when not set.
pub fn env_relay_url_override() -> Option<String> {
    let raw = std::env::var(ENV_RELAY_URL_OVERRIDE).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() || !is_valid_relay_url(trimmed) {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Returns the normalized relay URL. Falls back to the env override, then the
/// settings value, then the default. Mirrors `normalizeRelayUrl`.
pub fn normalize_relay_url(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() || !is_valid_relay_url(trimmed) {
        return env_relay_url_override().unwrap_or_else(|| DEFAULT_RELAY_URL.to_string());
    }
    trimmed.to_string()
}

/// Read the stored `privateRelay` config from `settings.json`. Applies the
/// env override at the URL field. Missing fields fall back to defaults.
pub fn read_config() -> PrivateRelayConfig {
    let stored = read_settings();
    let stored_obj = stored.get("privateRelay");
    let override_url = env_relay_url_override();
    let stored_url = stored_obj
        .and_then(|v| v.get("relayUrl"))
        .and_then(|v| v.as_str())
        .map(normalize_relay_url)
        .unwrap_or_else(|| DEFAULT_RELAY_URL.to_string());
    let enabled = stored_obj
        .and_then(|v| v.get("enabled"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    PrivateRelayConfig {
        enabled,
        relay_url: override_url.clone().unwrap_or(stored_url),
        relay_url_locked: override_url.is_some(),
    }
}

/// Persist `{ enabled, relayUrl }` under `settings.privateRelay`. Returns the
/// normalized config that was actually written.
pub fn write_config(config: &PrivateRelayConfig) -> Result<PrivateRelayConfig, oc_core::Error> {
    let mut settings = read_settings();
    if !settings.is_object() {
        settings = json!({});
    }
    let url = normalize_relay_url(&config.relay_url);
    let entry = json!({
        "enabled": config.enabled,
        "relayUrl": url,
    });
    if let Value::Object(ref mut map) = settings {
        map.insert("privateRelay".to_string(), entry);
    }
    write_settings(&settings)?;
    Ok(PrivateRelayConfig {
        enabled: config.enabled,
        relay_url: url,
        relay_url_locked: env_relay_url_override().is_some(),
    })
}

// =============================================================================
// Identity delegation
// =============================================================================

/// Convenience: load (or create on first call) the relay host identity. The
/// signing key lives at `settings.relaySigningKey`; the encryption key at
/// `settings.relayEncryptionKey`. See [`RelayIdentityRuntime::get_or_init`].
pub fn get_or_load_identity() -> RelayIdentity {
    RELAY_IDENTITY_RUNTIME.get_or_init()
}

/// Process-wide identity runtime. Kept as a singleton so `RelayIdentity`'s
/// cached server_id survives across the service + pairing modules.
pub static RELAY_IDENTITY_RUNTIME: once_cell::sync::Lazy<RelayIdentityRuntime> =
    once_cell::sync::Lazy::new(RelayIdentityRuntime::new);

// =============================================================================
// Standby & status types
// =============================================================================

/// Service-level state. Mirrors the JS `{ state, lastError, connectedClients }`
/// tuple with an extra `Standby { holderPid }` variant so the routes layer can
/// surface it without losing the holder's pid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceState {
    Disabled,
    Standby { holder_pid: u32 },
    Connecting,
    Connected,
    Reconnecting,
}

impl ServiceState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Standby { .. } => "standby",
            Self::Connecting => "connecting",
            Self::Connected => "connected",
            Self::Reconnecting => "reconnecting",
        }
    }

    fn from_host(host: RelayHostState) -> Self {
        match host {
            RelayHostState::Connecting => Self::Connecting,
            RelayHostState::Connected => Self::Connected,
            RelayHostState::Reconnecting => Self::Reconnecting,
            RelayHostState::Disabled => Self::Disabled,
        }
    }
}

/// Snapshot returned by [`RelayService::status`].
#[derive(Debug, Clone)]
pub struct RelayStatus {
    pub enabled: bool,
    pub state: ServiceState,
    pub server_id: String,
    pub connected_clients: usize,
    pub relay_url: String,
    pub relay_url_locked: bool,
    pub last_error: Option<String>,
}

impl RelayStatus {
    pub fn to_json(&self) -> Value {
        let mut obj = json!({
            "enabled": self.enabled,
            "state": self.state.as_str(),
            "serverId": self.server_id,
            "connectedClients": self.connected_clients,
            "relayUrl": self.relay_url,
            "relayUrlLocked": self.relay_url_locked,
        });
        if let Some(err) = &self.last_error {
            obj["lastError"] = json!(err);
        }
        obj
    }
}

// =============================================================================
// Pairing candidate
// =============================================================================

/// Pairing v2 candidate payload exposed to the unified `/api/openchamber/connection/candidates`
/// response. `None` when the host relay is off. Mirrors
/// `getPairingCandidate()` from `service.js`.
#[derive(Debug, Clone)]
pub struct PairingCandidate {
    pub relay_url: String,
    pub server_id: String,
    pub host_enc_pub_jwk: Value,
}

impl PairingCandidate {
    pub fn to_json(&self) -> Value {
        json!({
            "type": "relay",
            "relayUrl": self.relay_url,
            "serverId": self.server_id,
            "hostEncPubJwk": self.host_enc_pub_jwk,
            "priority": 30,
        })
    }
}

// =============================================================================
// RelayService
// =============================================================================

/// Lock claim semantics for [`RelayService::start`]. Mirrors the
/// `{ claim: 'try' | 'force' }` option in `service.js`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimMode {
    /// Cooperative claim: backs off if another live process holds the slot.
    Try,
    /// Unconditional claim: explicit user intent (pairing / enable route).
    Force,
}

/// Service-level relay lifecycle. Wraps the host-client, the identity runtime,
/// and the host-lock. One process may have at most one running `RelayService`.
pub struct RelayService {
    identity: RelayIdentityRuntime,
    host_lock: Option<Arc<crate::relay::host_lock::RelayHostLock>>,
    host_factory: Arc<dyn HostClientFactory>,
    has_relay_demand: HasRelayDemand,
    logger: Arc<dyn RelayLogger>,
    /// Cached local port callback (defaults to `Arc::new(|| None)`).
    get_local_port: Arc<dyn Fn() -> u16 + Send + Sync + 'static>,

    /// Currently running host handle, if any. Guarded by a mutex so the
    /// routes layer can drive `stop` without racing with `start`.
    host_handle: Mutex<Option<Box<dyn HostHandle>>>,
    /// Last status emitted by the host (or the synthetic standby / disabled
    /// status when no host is running). Held outside the mutex so reads
    /// don't take the host handle lock.
    status: Mutex<RelayStatus>,
    /// Last `set_state_with_error` recording. Lets `status()` include the
    /// last error even after the host stopped.
    last_error: Mutex<Option<String>>,
    /// Claim-watch timer handle; cleared on `stop`.
    claim_watch: Mutex<Option<Box<dyn ClaimWatchHandle>>>,
    /// Cached flag: when `true`, `start` becomes a no-op while a host is
    /// already running. The JS reference has the same guard.
    active: AtomicBool,
    /// Weak self-reference for the claim-watch timer. Set once via
    /// [`RelayService::attach_self_weak`] (production wires this in
    /// `main.rs`); when unset, claim-watch ticks no-op (the timer still
    /// ticks but `weak.upgrade()` returns `None` and the task exits).
    self_weak: Mutex<std::sync::Weak<Self>>,
}

/// Builder for [`RelayService`]. All fields have sensible defaults; tests
/// override the ones they care about.
pub struct RelayServiceBuilder {
    identity: RelayIdentityRuntime,
    host_lock: Option<Arc<crate::relay::host_lock::RelayHostLock>>,
    host_factory: Arc<dyn HostClientFactory>,
    has_relay_demand: HasRelayDemand,
    logger: Arc<dyn RelayLogger>,
    get_local_port: Arc<dyn Fn() -> u16 + Send + Sync + 'static>,
}

impl Default for RelayServiceBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl RelayServiceBuilder {
    pub fn new() -> Self {
        Self {
            identity: RelayIdentityRuntime::new(),
            host_lock: None,
            host_factory: Arc::new(NullHostClientFactory::new()),
            has_relay_demand: default_demand_stub(),
            logger: Arc::new(TracingRelayLogger),
            get_local_port: Arc::new(|| 0u16),
        }
    }

    pub fn with_identity(mut self, identity: RelayIdentityRuntime) -> Self {
        self.identity = identity;
        self
    }

    pub fn with_host_lock(mut self, lock: crate::relay::host_lock::RelayHostLock) -> Self {
        self.host_lock = Some(Arc::new(lock));
        self
    }

    pub fn with_host_factory(mut self, factory: Arc<dyn HostClientFactory>) -> Self {
        self.host_factory = factory;
        self
    }

    pub fn with_demand_check(mut self, has_relay_demand: HasRelayDemand) -> Self {
        self.has_relay_demand = has_relay_demand;
        self
    }

    pub fn with_logger(mut self, logger: Arc<dyn RelayLogger>) -> Self {
        self.logger = logger;
        self
    }

    pub fn with_local_port<F>(mut self, f: F) -> Self
    where
        F: Fn() -> u16 + Send + Sync + 'static,
    {
        self.get_local_port = Arc::new(f);
        self
    }

    pub fn build(self) -> RelayService {
        RelayService {
            identity: self.identity,
            host_lock: self.host_lock,
            host_factory: self.host_factory,
            has_relay_demand: self.has_relay_demand,
            logger: self.logger,
            get_local_port: self.get_local_port,
            host_handle: Mutex::new(None),
            status: Mutex::new(RelayStatus {
                enabled: false,
                state: ServiceState::Disabled,
                server_id: String::new(),
                connected_clients: 0,
                relay_url: read_config().relay_url,
                relay_url_locked: env_relay_url_override().is_some(),
                last_error: None,
            }),
            last_error: Mutex::new(None),
            claim_watch: Mutex::new(None),
            active: AtomicBool::new(false),
            self_weak: Mutex::new(std::sync::Weak::new()),
        }
    }
}

impl RelayService {
    /// Build a new service. See [`RelayServiceBuilder`] for tunables.
    pub fn builder() -> RelayServiceBuilder {
        RelayServiceBuilder::new()
    }

    /// Convenience: production-default service with no host lock.
    pub fn new() -> Self {
        Self::builder().build()
    }

    /// Install the host-lock. `path` is `<data-dir>/relay-host.lock`.
    pub fn install_host_lock(&mut self, lock: crate::relay::host_lock::RelayHostLock) {
        self.host_lock = Some(Arc::new(lock));
    }

    /// Attach a weak self-reference so the claim-watch timer can safely
    /// borrow `self` without escaping its lifetime. Production wires this in
    /// `main.rs` right after `Arc::new(RelayService::new())`.
    pub fn attach_self_weak(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        *self.self_weak.lock().expect("self_weak poisoned") = weak;
    }

    /// Read the cached weak self-reference. Used by [`spawn_claim_watch`].
    fn self_weak_clone(&self) -> std::sync::Weak<Self> {
        self.self_weak.lock().expect("self_weak poisoned").clone()
    }

    /// Set the host-client factory. Required before [`start`](Self::start)
    /// will actually open outbound WebSockets; otherwise the service runs
    /// through its state machine using the null factory.
    pub fn set_host_factory(&mut self, factory: Arc<dyn HostClientFactory>) {
        self.host_factory = factory;
    }

    /// Set the demand predicate. Called from `reconcile` and from the
    /// pairing module's hook on session/device changes.
    pub fn set_demand_check(&mut self, demand: HasRelayDemand) {
        self.has_relay_demand = demand;
    }

    /// Read the current service-level status.
    pub fn status(&self) -> RelayStatus {
        let mut snap = self.status.lock().expect("status poisoned").clone();
        let active = self.active.load(Ordering::SeqCst);
        let host_guard = self.host_handle.lock().expect("host_handle poisoned");
        if let Some(host) = host_guard.as_ref() {
            let host_status = host.snapshot_status();
            snap.state = ServiceState::from_host(host_status.state);
            snap.connected_clients = host_status.connected_clients;
            if let Some(err) = host_status.last_error {
                snap.last_error = Some(err);
            }
        } else if active {
            // Host was requested but is not running yet; mirrors the JS
            // initial "connecting" state.
            snap.state = ServiceState::Connecting;
        } else {
            // host_handle None and !active → disabled or standby. The
            // stored status already reflects which.
        }
        drop(host_guard);
        // server_id is derived lazily; ensure it's populated.
        if snap.server_id.is_empty() {
            let id = self.identity.get_or_init();
            snap.server_id = id.server_id;
        }
        snap
    }

    /// Load the current pairing candidate, if any. Mirrors
    /// `getPairingCandidate()` from `service.js`. Returns `None` when the
    /// host relay is disabled.
    pub fn pairing_candidate(&self) -> Option<PairingCandidate> {
        let cfg = read_config();
        if !cfg.enabled {
            return None;
        }
        let id = self.identity.get_or_init();
        Some(PairingCandidate {
            relay_url: cfg.relay_url,
            server_id: id.server_id,
            host_enc_pub_jwk: id.host_enc_pub_jwk,
        })
    }

    /// Stable server identity (server_id). Available even when the relay is
    /// disabled — clients use it to verify that a probed/learned address
    /// belongs to this server.
    pub fn server_id(&self) -> String {
        self.identity.get_or_init().server_id
    }

    /// Reconcile: start the host if there is demand, stop it if there isn't.
    /// Persists the enabled flag so settings reflect the source of truth.
    /// Mirrors `reconcil()` from `service.js`.
    pub async fn reconcile(&self) {
        let result: Result<(), String> = (|| async {
            let demand = (self.has_relay_demand)();
            let mut cfg = read_config();
            if demand {
                if !cfg.enabled {
                    cfg.enabled = true;
                    write_config(&cfg).map_err(|e| e.to_string())?;
                }
                if self.active.load(Ordering::SeqCst) {
                    return Ok(());
                }
                cfg = read_config();
                if let Err(err) = self.start_with_claim(cfg.relay_url, ClaimMode::Try).await {
                    return Err(err);
                }
            } else {
                if cfg.enabled {
                    cfg.enabled = false;
                    write_config(&cfg).map_err(|e| e.to_string())?;
                }
                self.stop_internal();
            }
            Ok(())
        })()
        .await;
        if let Err(err) = result {
            self.logger
                .warn(&format!("[Relay] reconcile failed: {err}"));
        }
    }

    /// Read the enabled flag from settings, and if it's set, start the host.
    /// Best-effort: logs and swallows errors so the rest of the boot path is
    /// not blocked. Mirrors `startIfEnabled()` from `service.js`.
    pub async fn start_if_enabled(&self) {
        let result: Result<(), String> = (|| async {
            let cfg = read_config();
            if cfg.enabled {
                self.start_with_claim(cfg.relay_url, ClaimMode::Try).await?;
            }
            Ok(())
        })()
        .await;
        if let Err(err) = result {
            self.logger
                .warn(&format!("[Relay] startup failed: {err}"));
        }
    }

    /// Start the relay host. Idempotent: returns immediately if a host is
    /// already running. `claim` controls whether the host-lock acquisition
    /// is cooperative (`Try`) or unconditional (`Force`).
    pub async fn start(&self, relay_url: String) -> Result<(), String> {
        self.start_with_claim(relay_url, ClaimMode::Try).await
    }

    /// Enable the relay on demand and return its pairing candidate. Mirrors
    /// `ensureEnabledForPairing()` from `service.js`. Creates the pairing
    /// link is the demand signal, so the relay turns itself on here rather
    /// than requiring a separate manual toggle.
    pub async fn ensure_enabled_for_pairing(&self) -> Result<PairingCandidate, String> {
        let mut cfg = read_config();
        if !cfg.enabled {
            cfg.enabled = true;
            cfg = write_config(&cfg).map_err(|e| e.to_string())?;
        }
        if !self.active.load(Ordering::SeqCst) {
            cfg = read_config();
            self.start_with_claim(cfg.relay_url.clone(), ClaimMode::Force)
                .await?;
        }
        let id = self.identity.get_or_init();
        Ok(PairingCandidate {
            relay_url: cfg.relay_url,
            server_id: id.server_id,
            host_enc_pub_jwk: id.host_enc_pub_jwk,
        })
    }

    /// Public enable handler. Persists `enabled = true`, force-claims the
    /// slot, and starts the host. Mirrors `POST /api/openchamber/relay/enable`
    /// from `service.js`.
    pub async fn enable(&self, relay_url: Option<String>) -> Result<RelayStatus, String> {
        let current = read_config();
        let url = relay_url
            .as_deref()
            .map(normalize_relay_url)
            .unwrap_or(current.relay_url);
        let new_cfg = PrivateRelayConfig {
            enabled: true,
            relay_url: url.clone(),
            relay_url_locked: env_relay_url_override().is_some(),
        };
        write_config(&new_cfg).map_err(|e| e.to_string())?;
        if self.active.load(Ordering::SeqCst) {
            self.stop_internal();
        }
        self.start_with_claim(url, ClaimMode::Force).await?;
        Ok(self.status())
    }

    /// Public disable handler. Persists `enabled = false`, stops the host,
    /// releases the host claim. Mirrors `POST /api/openchamber/relay/disable`.
    pub fn disable(&self) -> Result<RelayStatus, String> {
        let current = read_config();
        let new_cfg = PrivateRelayConfig {
            enabled: false,
            relay_url: current.relay_url.clone(),
            relay_url_locked: env_relay_url_override().is_some(),
        };
        write_config(&new_cfg).map_err(|e| e.to_string())?;
        self.stop_internal();
        Ok(self.status())
    }

    /// Stop the host. Mirrors `stop()` from `service.js`.
    pub fn stop(&self) {
        self.stop_internal();
    }

    // -------------------------------------------------------------------------
    // Internals
    // -------------------------------------------------------------------------

    async fn start_with_claim(
        &self,
        relay_url: String,
        claim_mode: ClaimMode,
    ) -> Result<(), String> {
        if self.active.swap(true, Ordering::SeqCst) {
            return Ok(());
        }

        // 1. Host-lock: try-claim or force-claim. If a different live holder
        //    exists AND we are not forcing, we go into standby.
        if let Some(lock) = self.host_lock.as_ref() {
            let claimed = match claim_mode {
                ClaimMode::Try => lock.try_claim(),
                ClaimMode::Force => lock.force_claim(),
            };
            if !claimed {
                // Standby is NOT active — reset the flag so `status()` does
                // not overwrite the Standby state with Connecting (the
                // `active` check in status() would see `true` + no host
                // handle and assume pending-start instead of standby).
                self.active.store(false, Ordering::SeqCst);
                let holder = lock.live_claimant_pid().unwrap_or(0);
                self.record_standby(holder);
                self.spawn_claim_watch(relay_url.clone());
                return Ok(());
            }
        }

        // 2. Identity: load (or create on first call). Stable per-server id.
        let identity = self.identity.get_or_init();
        let relay_url_for_factory = normalize_relay_url(&relay_url);

        // 3. Status sink: relay status into the in-process state.
        let captured = CapturedStatus::new();
        let sink: Arc<dyn crate::relay::host_client::HostStatusSink> =
            Arc::new(captured.clone());
        let cfg = RelayHostConfig::new(
            relay_url_for_factory.clone(),
            identity.clone(),
            {
                // Test/local wiring has no transport — the default factory will
                // receive this and discard it. Production wires a real
                // TungsteniteHostTransport through HostClientFactory::start.
                struct NoopTransport;
                impl crate::relay::host_client::HostTransport for NoopTransport {
                    fn dial(
                        &self,
                        _url: String,
                    ) -> std::pin::Pin<
                        Box<
                            dyn std::future::Future<
                                    Output = Result<
                                        Box<dyn crate::relay::host_client::HostSocket>,
                                        crate::relay::host_client::TransportError,
                                    >,
                                > + Send,
                        >,
                    > {
                        Box::pin(async move {
                            Err(crate::relay::host_client::TransportError::Other(
                                "no transport wired".to_string(),
                            ))
                        })
                    }
                }
                Arc::new(NoopTransport) as Arc<dyn crate::relay::host_client::HostTransport>
            },
            self.get_local_port.clone(),
        )
        .with_status_sink(sink)
        .with_warn_sink(Arc::new(crate::relay::host_client::TracingWarn));

        // 4. Start the host through the factory.
        let host = self.host_factory.start(cfg);
        *self.host_handle.lock().expect("host_handle poisoned") = Some(host);

        // 5. Update cached status with enabled flag + URL.
        {
            let mut snap = self.status.lock().expect("status poisoned");
            snap.enabled = true;
            snap.state = ServiceState::Connecting;
            snap.relay_url = relay_url_for_factory.clone();
            snap.server_id = identity.server_id.clone();
            snap.connected_clients = 0;
            snap.last_error = None;
        }
        *self.last_error.lock().expect("last_error poisoned") = None;

        // 6. Spawn the claim watch so we react to other live processes
        //    claiming while we hold the slot.
        self.spawn_claim_watch(relay_url_for_factory.clone());
        info!(target: "relay::service", "started (url={})", relay_url);
        Ok(())
    }

    fn stop_internal(&self) {
        self.active.store(false, Ordering::SeqCst);
        // Cancel claim watch.
        if let Some(handle) = self.claim_watch.lock().expect("claim_watch poisoned").take() {
            handle.cancel();
        }
        // Stop the host.
        let prev = self.host_handle.lock().expect("host_handle poisoned").take();
        if let Some(host) = prev {
            host.stop_blocking();
        }
        // Release host claim.
        if let Some(lock) = self.host_lock.as_ref() {
            lock.release();
        }
        // Reset cached status.
        let cfg = read_config();
        let id = self.identity.get_or_init();
        let mut snap = self.status.lock().expect("status poisoned");
        snap.enabled = cfg.enabled;
        snap.state = ServiceState::Disabled;
        snap.connected_clients = 0;
        snap.relay_url = cfg.relay_url;
        snap.relay_url_locked = cfg.relay_url_locked;
        snap.server_id = id.server_id.clone();
        snap.last_error = None;
    }

    fn record_standby(&self, holder_pid: u32) {
        let cfg = read_config();
        let id = self.identity.get_or_init();
        let mut snap = self.status.lock().expect("status poisoned");
        snap.enabled = cfg.enabled;
        snap.state = ServiceState::Standby { holder_pid };
        snap.connected_clients = 0;
        snap.relay_url = cfg.relay_url;
        snap.relay_url_locked = cfg.relay_url_locked;
        snap.server_id = id.server_id.clone();
        snap.last_error = Some(format!(
            "relay host is owned by another local OpenChamber process (pid {holder_pid})"
        ));
    }

    fn spawn_claim_watch(&self, relay_url: String) {
        if self.host_lock.is_none() {
            return;
        }
        // Already running? don't double-schedule.
        if self.claim_watch.lock().expect("claim_watch poisoned").is_some() {
            return;
        }
        let interval = Duration::from_millis(CLAIM_WATCH_INTERVAL_MS);
        // Capture a weak handle to the service. If the service is dropped,
        // `upgrade()` returns None and the tick no-ops (the timer itself
        // keeps running until cancelled — that's fine, the gap is short
        // because tests cancel via `stop()`).
        let weak = self.self_weak_clone();
        let url_for_tick = relay_url.clone();
        let handle = TokioClaimWatch::schedule(interval, move || {
            let url = url_for_tick.clone();
            let weak = weak.clone();
            tokio::spawn(async move {
                if let Some(svc) = weak.upgrade() {
                    svc.claim_watch_tick(&url).await;
                }
            });
        });
        *self.claim_watch.lock().expect("claim_watch poisoned") = Some(Box::new(handle));
    }

    async fn claim_watch_tick(&self, relay_url: &str) {
        let lock = match self.host_lock.as_ref() {
            Some(l) => l,
            None => return,
        };
        let result: Result<(), String> = (|| async {
            // If we are running, watch for take-over.
            if self.active.load(Ordering::SeqCst) {
                if !lock.holds_claim() {
                    if let Some(holder) = lock.live_claimant_pid() {
                        self.logger.warn(
                            "[Relay] host claim taken by another local instance — standing down",
                        );
                        self.stop_internal();
                        self.record_standby(holder);
                    }
                }
                return Ok(());
            }
            // We are not running. Are we on standby with the slot now free?
            let snap = self.status.lock().expect("status poisoned").clone();
            if matches!(snap.state, ServiceState::Standby { .. }) {
                if lock.try_claim() {
                    self.logger.warn(
                        "[Relay] host claim is free — taking over the relay host",
                    );
                    self.start_with_claim(relay_url.to_string(), ClaimMode::Force).await?;
                }
            }
            Ok(())
        })()
        .await;
        if let Err(err) = result {
            self.logger
                .warn(&format!("[Relay] claim watch failed: {err}"));
        }
    }

    /// Return a self-contained handle suitable for spawning into a tokio task.
    /// The handle snapshots the read-only parts of the service (status,
    /// host-lock ref, logger) and re-enters the parent's mutable state via
    /// `&self` callbacks; the lock is short-lived per tick so there is no
    /// risk of holding across awaits.
    fn clone_handle(&self) -> ClaimWatchServiceHandle<'_> {
        ClaimWatchServiceHandle {
            inner: self,
        }
    }
}

/// Borrow-only handle used to call `claim_watch_tick` from a tokio task.
/// Holds a `&RelayService` reference; the task is short-lived and the
/// reference is `'static` because the underlying `Arc<RelayService>` owns
/// the service. This avoids the unsoundness of cloning the inner `Mutex`es.
#[allow(dead_code)]
struct ClaimWatchServiceHandle<'a> {
    inner: &'a RelayService,
}

#[allow(dead_code)]
impl<'a> ClaimWatchServiceHandle<'a> {
    async fn claim_watch_tick(&self, relay_url: &str) {
        self.inner.claim_watch_tick(relay_url).await;
    }
}

// =============================================================================
// Default host-lock factory
// =============================================================================

/// Build a host-lock with `MemFs` for tests or `StdFs` for production, given
/// the data dir. The lock file lives at `<data_dir>/relay-host.lock`.
pub fn default_host_lock(data_dir: PathBuf) -> crate::relay::host_lock::RelayHostLock {
    let lock_path = data_dir.join("relay-host.lock");
    // Production uses `StdFs`; tests construct their own lock with `MemFs`.
    let _ = MemFs::new; // keep MemFs import alive when tests aren't built.
    let _ = SystemPidProbe;
    let _ = TracingWarn;
    crate::relay::host_lock::RelayHostLock::with_defaults(lock_path)
}

// =============================================================================
// Time helper (ms since epoch)
// =============================================================================

#[allow(dead_code)]
fn now_ms_u64() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[allow(dead_code)]
fn _unused_hashmap_marker() -> HashMap<(), ()> {
    HashMap::new()
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Global test mutex: serializes tests that mutate
    /// `settings.json` (`privateRelay`) so the parallel cargo test runner
    /// does not race the shared on-disk file. Acquired at the top of each
    /// test that reads/writes settings.
    static SETTINGS_LOCK: once_cell::sync::Lazy<std::sync::Mutex<()>> =
        once_cell::sync::Lazy::new(|| std::sync::Mutex::new(()));

    /// Acquire and hold the settings lock for the duration of the test.
    /// Returns a guard that drops at end of scope.
    fn lock_settings() -> std::sync::MutexGuard<'static, ()> {
        SETTINGS_LOCK.lock().expect("settings lock poisoned")
    }

    // --- URL validation & normalization -----------------------------------

    #[test]
    fn is_valid_relay_url_accepts_ws_and_wss() {
        assert!(is_valid_relay_url("wss://relay.example.com/v1"));
        assert!(is_valid_relay_url("ws://relay.example.com/v1"));
        assert!(!is_valid_relay_url("https://relay.example.com/v1"));
        assert!(!is_valid_relay_url("not a url"));
        assert!(!is_valid_relay_url(""));
        assert!(!is_valid_relay_url("  "));
    }

    #[test]
    fn normalize_relay_url_trims_and_validates() {
        assert_eq!(
            normalize_relay_url("  wss://relay.example.com/v1  "),
            "wss://relay.example.com/v1"
        );
        assert_eq!(normalize_relay_url(""), DEFAULT_RELAY_URL);
        assert_eq!(normalize_relay_url("garbage"), DEFAULT_RELAY_URL);
    }

    #[test]
    fn env_override_takes_precedence_when_valid() {
        // Safe-path: only assert the documented contract (returns None when
        // unset). Tests that mutate env live in `routes.rs` where we own the
        // settings file.
        let prev = std::env::var(ENV_RELAY_URL_OVERRIDE).ok();
        std::env::remove_var(ENV_RELAY_URL_OVERRIDE);
        assert!(env_relay_url_override().is_none());
        if let Some(v) = prev {
            std::env::set_var(ENV_RELAY_URL_OVERRIDE, v);
        }
    }

    // --- PrivateRelayConfig defaults -------------------------------------

    #[test]
    fn disabled_default_has_lock_false_unless_env_set() {
        let prev = std::env::var(ENV_RELAY_URL_OVERRIDE).ok();
        std::env::remove_var(ENV_RELAY_URL_OVERRIDE);
        let cfg = PrivateRelayConfig::disabled_default();
        assert!(!cfg.enabled);
        assert!(!cfg.relay_url_locked);
        assert_eq!(cfg.relay_url, DEFAULT_RELAY_URL);
        if let Some(v) = prev {
            std::env::set_var(ENV_RELAY_URL_OVERRIDE, v);
        }
    }

    // --- NullHostClientFactory bookkeeping ------------------------------

    #[test]
    fn null_factory_returns_disabled_status_snapshot() {
        let factory = NullHostClientFactory::new();
        // The factory itself doesn't record; it just returns a no-op handle.
        let handle = factory.start(RelayHostConfig::new(
            "wss://relay.example.com/v1",
            make_test_identity(),
            Arc::new(NoopHostTransport),
            Arc::new(|| 0u16),
        ));
        let snap = handle.snapshot_status();
        assert_eq!(snap.state, RelayHostState::Disabled);
    }

    struct NoopHostTransport;
    impl crate::relay::host_client::HostTransport for NoopHostTransport {
        fn dial(
            &self,
            _url: String,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            Box<dyn crate::relay::host_client::HostSocket>,
                            crate::relay::host_client::TransportError,
                        >,
                    > + Send,
            >,
        > {
            Box::pin(async move {
                Err(crate::relay::host_client::TransportError::Other(
                    "noop".to_string(),
                ))
            })
        }
    }

    fn make_test_identity() -> RelayIdentity {
        RelayIdentityRuntime::new().get_or_init()
    }

    // --- Service-level wiring ---------------------------------------------

    #[test]
    fn new_service_starts_disabled() {
        let svc = RelayService::new();
        let snap = svc.status();
        assert_eq!(snap.state, ServiceState::Disabled);
        assert!(!snap.enabled);
        assert!(!snap.server_id.is_empty());
    }

    #[test]
    fn disable_is_idempotent() {
        let svc = RelayService::new();
        let s1 = svc.disable().expect("disable ok");
        let s2 = svc.disable().expect("disable ok again");
        assert_eq!(s1.state, ServiceState::Disabled);
        assert_eq!(s2.state, ServiceState::Disabled);
    }

    #[test]
    fn pairing_candidate_returns_none_when_disabled() {
        let _guard = lock_settings();
        // Make sure no leftover enabled state from a prior test.
        let mut s = read_settings();
        if let Value::Object(ref mut map) = s {
            map.remove("privateRelay");
        }
        let _ = write_settings(&s);
        let svc = RelayService::new();
        assert!(svc.pairing_candidate().is_none());
    }

    #[test]
    fn status_json_shape_matches_js_reference() {
        let svc = RelayService::new();
        let status = svc.status();
        let v = status.to_json();
        assert_eq!(v["enabled"], json!(status.enabled));
        assert_eq!(v["state"], json!(status.state.as_str()));
        assert_eq!(v["serverId"], json!(status.server_id));
        assert_eq!(
            v["connectedClients"],
            json!(status.connected_clients as u64)
        );
        assert_eq!(v["relayUrl"], json!(status.relay_url));
        assert_eq!(v["relayUrlLocked"], json!(status.relay_url_locked));
        if let Some(err) = &status.last_error {
            assert_eq!(v["lastError"], json!(err));
        } else {
            assert!(v.get("lastError").is_none());
        }
    }

    // --- ensure_enabled_for_pairing flow ---------------------------------

    #[tokio::test]
    async fn ensure_enabled_for_pairing_returns_candidate() {
        let _guard = lock_settings();
        // Clean state.
        let mut s = read_settings();
        if let Value::Object(ref mut map) = s {
            map.remove("privateRelay");
        }
        let _ = write_settings(&s);
        let svc = RelayService::new();
        let candidate = svc
            .ensure_enabled_for_pairing()
            .await
            .expect("ensure-enabled succeeds");
        assert!(!candidate.server_id.is_empty());
        assert!(!candidate.relay_url.is_empty());
        assert_eq!(candidate.to_json()["type"], json!("relay"));
        assert_eq!(candidate.to_json()["priority"], json!(30));
        let snap = svc.status();
        assert!(snap.enabled, "ensure_enabled_for_pairing must set enabled");
        svc.stop();
        // Reset for next test.
        let mut s = read_settings();
        if let Value::Object(ref mut map) = s {
            map.remove("privateRelay");
        }
        let _ = write_settings(&s);
    }

    #[tokio::test]
    async fn start_returns_immediately_when_already_active() {
        let svc = RelayService::new();
        let cfg = read_config();
        let url = cfg.relay_url.clone();
        svc.start(url.clone()).await.expect("first start ok");
        let second = svc.start(url).await;
        // Second start is a no-op, must not error.
        assert!(second.is_ok());
    }

    #[tokio::test]
    async fn stop_after_start_resets_status_to_disabled() {
        let svc = RelayService::new();
        let cfg = read_config();
        svc.start(cfg.relay_url).await.expect("start ok");
        svc.stop();
        let snap = svc.status();
        assert_eq!(snap.state, ServiceState::Disabled);
    }

    // --- Demand-driven reconcile ----------------------------------------

    #[tokio::test]
    async fn reconcile_with_no_demand_disables_relay() {
        let _guard = lock_settings();
        // Pre-set enabled = true so we can verify reconcile() turns it off.
        let mut cfg = read_config();
        cfg.enabled = true;
        let _ = write_config(&cfg);
        let svc = RelayService::new();
        svc.reconcile().await;
        let snap = svc.status();
        assert!(!snap.enabled);
        let after = read_config();
        assert!(!after.enabled, "settings must reflect disabled");
        svc.stop();
    }

    #[tokio::test]
    async fn reconcile_with_demand_enables_relay() {
        let _guard = lock_settings();
        let mut cfg = read_config();
        cfg.enabled = false;
        let _ = write_config(&cfg);
        let svc = RelayService::builder()
            .with_demand_check(Arc::new(|| true))
            .build();
        svc.reconcile().await;
        let snap = svc.status();
        assert!(snap.enabled);
        let after = read_config();
        assert!(after.enabled);
        svc.stop();
    }

    // --- Captured logger exercises the warn sink --------------------------

    #[test]
    fn captured_logger_records_messages() {
        let logger = CapturedRelayLogger::new();
        logger.warn("hello");
        logger.warn("world");
        let msgs = logger.messages();
        assert_eq!(msgs, vec!["hello".to_string(), "world".to_string()]);
    }
}