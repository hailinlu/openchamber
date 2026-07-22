//! Relay admin axum handlers.
//!
//! Direct port of `packages/web/server/lib/relay/service.js#registerRoutes`.
//! Three handlers backed by [`RelayService`]:
//!
//!   - `GET  /api/gridforge/relay/status`   — current state + settings.
//!   - `POST /api/gridforge/relay/enable`   — persist `enabled = true` and
//!                                                force-claim the host slot.
//!   - `POST /api/gridforge/relay/disable`  — persist `enabled = false` and
//!                                                release the host slot.
//!
//! Errors follow the JS reference contract: a JSON body
//! `{ "error": "<message>" }` with HTTP 500. The status handler always returns
//! 200 — the underlying service may be in any state, but the snapshot itself
//! is not an error condition.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};
use std::sync::Arc;

use crate::state::AppState;

use super::service::{normalize_relay_url, RelayService, RelayStatus};

// =============================================================================
// Handlers
// =============================================================================

/// Resolve the active relay service from AppState, taking into account test
/// overrides. Production paths simply read `state.relay_service`; tests
/// additionally consult `state.relay_service_override` because they need to
/// inject a service after the AppState has been wrapped in Arc.
fn resolve_relay_service(state: &Arc<AppState>) -> Option<Arc<RelayService>> {
    #[cfg(test)]
    {
        if let Some(svc) = state
            .relay_service_override
            .lock()
            .expect("relay_service_override poisoned")
            .as_ref()
        {
            return Some(svc.clone());
        }
    }
    state.relay_service
        .lock()
        .expect("relay_service poisoned")
        .clone()
}

/// `GET /api/gridforge/relay/status`.
pub async fn get_status_handler(
    State(state): State<Arc<AppState>>,
) -> Response {
    match resolve_relay_service(&state).as_ref() {
        Some(svc) => {
            let snap = svc.status();
            Json(snap.to_json()).into_response()
        }
        None => fallback_status().into_response(),
    }
}

/// `POST /api/gridforge/relay/enable`.
///
/// Optional body: `{ "relayUrl": "wss://..." }`. When omitted, the current
/// stored URL is reused. The relay host is force-started and force-claims
/// the per-machine lock so the calling instance is the one paired devices
/// reach.
pub async fn post_enable_handler(
    State(state): State<Arc<AppState>>,
    body: Option<Json<Value>>,
) -> Response {
    let svc = match resolve_relay_service(&state).as_ref() {
        Some(s) => s.clone(),
        None => return service_unavailable_response(),
    };
    let relay_url = body
        .as_ref()
        .and_then(|b| b.get("relayUrl"))
        .and_then(|v| v.as_str())
        .map(|s| normalize_relay_url(s));
    match svc.enable(relay_url).await {
        Ok(snap) => snap.to_json_response(),
        Err(err) => internal_error_response(&err),
    }
}

/// `POST /api/gridforge/relay/disable`.
pub async fn post_disable_handler(
    State(state): State<Arc<AppState>>,
) -> Response {
    let svc = match resolve_relay_service(&state).as_ref() {
        Some(s) => s.clone(),
        None => return service_unavailable_response(),
    };
    match svc.disable() {
        Ok(snap) => snap.to_json_response(),
        Err(err) => internal_error_response(&err),
    }
}

// =============================================================================
// Helpers
// =============================================================================

/// Status payload returned when the relay service has not been wired into
/// the AppState yet (e.g. during very early boot or test harnesses that do
/// not build the full AppState).
fn fallback_status() -> Json<Value> {
    Json(json!({
        "enabled": false,
        "state": "disabled",
        "serverId": "",
        "connectedClients": 0,
        "relayUrl": crate::relay::DEFAULT_RELAY_URL,
        "relayUrlLocked": false,
    }))
}

fn service_unavailable_response() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({
            "error": "Relay service not initialized",
        })),
    )
        .into_response()
}

fn internal_error_response(message: &str) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({
            "error": message,
        })),
    )
        .into_response()
}

impl RelayStatus {
    /// Render the status snapshot as a JSON response. Mirrors the JS
    /// `res.json(await getStatus())` shape exactly.
    pub fn to_json_response(&self) -> Response {
        Json(self.to_json()).into_response()
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::settings::{read_settings, write_settings};
    use crate::relay::host_lock::{
        CapturedWarn, MemFs, RelayHostLock, WarnSink,
    };
    use crate::relay::service::{
        read_config, CapturedRelayLogger, RelayLogger, RelayService,
        RELAY_IDENTITY_RUNTIME,
    };
    use crate::relay::host_lock::PidProbe;
    use axum::body::to_bytes;
    use axum::http::Request;
    use serde_json::Value;
    use std::path::PathBuf;
    use tower::ServiceExt;

    /// Global test mutex: serializes tests that mutate
    /// `settings.json` (`privateRelay`) so the parallel cargo test runner
    /// does not race the shared on-disk file.
    static SETTINGS_LOCK: once_cell::sync::Lazy<std::sync::Mutex<()>> =
        once_cell::sync::Lazy::new(|| std::sync::Mutex::new(()));

    fn lock_settings() -> std::sync::MutexGuard<'static, ()> {
        SETTINGS_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Clean slate: drop `privateRelay` so the test starts from defaults.
    fn reset_private_relay_settings() {
        let mut s = read_settings();
        if let Value::Object(ref mut map) = s {
            map.remove("privateRelay");
        }
        let _ = write_settings(&s);
    }

    // -- helpers used in assertions ----------------------------------------

    async fn body_json(resp: axum::response::Response) -> Value {
        let bytes = to_bytes(resp.into_body(), 64 * 1024)
            .await
            .expect("body readable");
        serde_json::from_slice(&bytes).expect("json body")
    }

    fn make_state() -> Arc<AppState> {
        Arc::new(AppState::new_for_tests())
    }

    fn build_app(state: Arc<AppState>) -> axum::Router {
        axum::Router::new()
            .route(
                "/api/gridforge/relay/status",
                axum::routing::get(get_status_handler),
            )
            .route(
                "/api/gridforge/relay/enable",
                axum::routing::post(post_enable_handler),
            )
            .route(
                "/api/gridforge/relay/disable",
                axum::routing::post(post_disable_handler),
            )
            .with_state(state)
    }

    /// Build a router that injects a `RelayService` into the AppState before
    /// serving. Must be called BEFORE the state is wrapped in `Arc::new` is
    /// shared — `AppState::set_relay_service_for_tests` requires exclusive
    /// access.
    fn build_app_with_service(svc: Arc<RelayService>) -> axum::Router {
        let state = make_state();
        state.set_relay_service_for_tests(svc);
        build_app(state)
    }

    // -- route handlers against un-wired state ----------------------------

    #[tokio::test]
    async fn status_returns_disabled_when_service_uninitialized() {
        let app = build_app(make_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/gridforge/relay/status")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["state"], "disabled");
        assert_eq!(body["enabled"], false);
        assert!(body["relayUrl"].is_string());
        assert!(body["relayUrlLocked"].is_boolean());
    }

    #[tokio::test]
    async fn enable_returns_503_when_service_uninitialized() {
        let app = build_app(make_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/gridforge/relay/enable")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = body_json(resp).await;
        assert!(body["error"].is_string());
    }

    #[tokio::test]
    async fn disable_returns_503_when_service_uninitialized() {
        let app = build_app(make_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/gridforge/relay/disable")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    // -- route handlers against wired state -------------------------------

    #[tokio::test]
    async fn full_status_returns_service_snapshot() {
        let svc = Arc::new(RelayService::new());
        let app = build_app_with_service(svc);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/gridforge/relay/status")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert!(body["serverId"].is_string());
        assert!(body["relayUrl"].is_string());
    }

    #[tokio::test]
    async fn full_enable_persists_and_returns_status() {
        let _guard = lock_settings();
        reset_private_relay_settings();
        let svc = Arc::new(RelayService::new());
        let app = build_app_with_service(svc.clone());
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/gridforge/relay/enable")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        r#"{"relayUrl":"wss://example.test/v1"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["enabled"], true);
        assert_eq!(body["relayUrl"], "wss://example.test/v1");
        svc.stop();
        reset_private_relay_settings();
    }

    #[tokio::test]
    async fn full_disable_persists_enabled_false() {
        let _guard = lock_settings();
        reset_private_relay_settings();
        let svc = Arc::new(RelayService::new());
        // Pre-enable so disable has something to flip.
        let _ = svc
            .enable(Some("wss://example.test/v1".to_string()))
            .await;
        let app = build_app_with_service(svc.clone());
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/gridforge/relay/disable")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["enabled"], false);
        svc.stop();
        reset_private_relay_settings();
    }

    // -- direct service tests ---------------------------------------------

    #[tokio::test]
    async fn enable_persists_and_returns_status() {
        let _guard = lock_settings();
        reset_private_relay_settings();

        let svc = RelayService::new();
        let snap = svc
            .enable(Some("wss://relay.example.test/v1".to_string()))
            .await
            .expect("enable ok");
        assert!(snap.enabled);
        assert_eq!(snap.relay_url, "wss://relay.example.test/v1");
        let stored = read_settings();
        assert_eq!(
            stored["privateRelay"]["enabled"],
            Value::Bool(true),
            "settings must reflect enabled"
        );
        assert_eq!(
            stored["privateRelay"]["relayUrl"],
            Value::String("wss://relay.example.test/v1".to_string())
        );
        svc.stop();
        reset_private_relay_settings();
    }

    #[tokio::test]
    async fn disable_persists_enabled_false() {
        let _guard = lock_settings();
        reset_private_relay_settings();
        let svc = RelayService::new();
        let _ = svc
            .enable(Some("wss://relay.example.test/v1".to_string()))
            .await;
        let snap = svc.disable().expect("disable ok");
        assert!(!snap.enabled);
        let stored = read_settings();
        assert_eq!(stored["privateRelay"]["enabled"], Value::Bool(false));
        svc.stop();
        reset_private_relay_settings();
    }

    #[tokio::test]
    async fn enable_with_no_url_reuses_stored_url() {
        let _guard = lock_settings();
        reset_private_relay_settings();
        let svc = RelayService::new();
        let _ = svc
            .enable(Some("wss://relay.example.test/v1".to_string()))
            .await;
        svc.stop();
        let snap = svc.enable(None).await.expect("enable no-url");
        assert_eq!(snap.relay_url, "wss://relay.example.test/v1");
        svc.stop();
        reset_private_relay_settings();
    }

    #[tokio::test]
    async fn enable_with_invalid_url_falls_back_to_default() {
        let _guard = lock_settings();
        reset_private_relay_settings();
        let svc = RelayService::new();
        let snap = svc
            .enable(Some("not a url".to_string()))
            .await
            .expect("enable ok (falls back)");
        assert_eq!(snap.relay_url, crate::relay::DEFAULT_RELAY_URL);
        svc.stop();
        reset_private_relay_settings();
    }

    // -- standby path: lock blocks us --------------------------------------

    /// Two services share a MemFs claim file. PID_A claims first; PID_B
    /// starts with `Try` → must report standby with PID_A as holder.
    #[tokio::test]
    async fn service_with_lock_records_standby_when_other_holds() {
        let mem = Arc::new(MemFs::new());
        let lock_path = PathBuf::from("/tmp/oc-relay-standby-test.lock");
        let warn_a: Arc<dyn WarnSink> = Arc::new(CapturedWarn::new());
        let warn_b: Arc<dyn WarnSink> = Arc::new(CapturedWarn::new());

        struct ProbeAlive { alive: [u32; 2] }
        impl PidProbe for ProbeAlive {
            fn is_alive(&self, pid: u32) -> bool {
                self.alive.contains(&pid) || pid == std::process::id()
            }
        }
        let all_pids = Arc::new(ProbeAlive {
            alive: [1001, 1002],
        });

        let lock_a = RelayHostLock::new(
            lock_path.clone(),
            mem.clone() as Arc<dyn crate::relay::host_lock::FsOps>,
            all_pids.clone() as Arc<dyn PidProbe>,
            warn_a,
            Some(1001),
        );
        let lock_b = RelayHostLock::new(
            lock_path.clone(),
            mem.clone() as Arc<dyn crate::relay::host_lock::FsOps>,
            all_pids.clone() as Arc<dyn PidProbe>,
            warn_b,
            Some(1002),
        );

        // First, claim as PID_A. Then build a service with PID_B's lock and
        // ask it to start with Try → must report standby.
        assert!(lock_a.try_claim());

        let svc = RelayService::builder()
            .with_host_lock(lock_b)
            .build();
        // Note: `with_host_lock` already wraps in Arc; the builder takes the
        // concrete `RelayHostLock` for ergonomic test setup.
        svc.start("wss://relay.test/v1".to_string())
            .await
            .expect("start ok (standby)");
        let snap = svc.status();
        match snap.state {
            super::super::service::ServiceState::Standby { holder_pid } => {
                assert_eq!(holder_pid, 1001);
            }
            other => panic!("expected Standby, got {other:?}"),
        }
        assert!(snap.last_error.is_some(), "standby must record last_error");
        svc.stop();
    }

    // -- identity delegation ----------------------------------------------

    #[test]
    fn server_id_is_stable_across_service_instances() {
        let a = RelayService::new();
        let b = RelayService::new();
        assert_eq!(a.server_id(), b.server_id());
        assert!(!a.server_id().is_empty());
    }

    #[test]
    fn identity_runtime_singleton_returns_same_identity() {
        let id_a = RELAY_IDENTITY_RUNTIME.get_or_init();
        let id_b = RELAY_IDENTITY_RUNTIME.get_or_init();
        assert_eq!(id_a.server_id, id_b.server_id);
    }

    // -- captured logger / sink wiring ------------------------------------

    #[test]
    fn captured_logger_records_messages() {
        let logger = CapturedRelayLogger::new();
        logger.warn("hello");
        logger.warn("world");
        let msgs = logger.messages();
        assert_eq!(msgs, vec!["hello".to_string(), "world".to_string()]);
    }

    // -- helpers ----------------------------------------------------------

    #[test]
    fn fallback_status_shape_is_complete() {
        let body = fallback_status().0;
        assert_eq!(body["enabled"], false);
        assert_eq!(body["state"], "disabled");
        assert_eq!(body["connectedClients"], 0);
        assert!(body["relayUrl"].is_string());
        assert_eq!(body["relayUrlLocked"], false);
    }

    #[test]
    fn internal_error_response_shape() {
        let resp = internal_error_response("boom");
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn service_unavailable_response_shape() {
        let resp = service_unavailable_response();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    // -- settings round-trip ----------------------------------------------

    #[test]
    fn reading_settings_with_no_privateRelay_field_uses_defaults() {
        let _guard = lock_settings();
        let mut settings = read_settings();
        if let Value::Object(ref mut map) = settings {
            map.remove("privateRelay");
        }
        let _ = write_settings(&settings);
        let cfg = read_config();
        assert!(!cfg.enabled);
        assert_eq!(cfg.relay_url, crate::relay::DEFAULT_RELAY_URL);
        assert!(!cfg.relay_url_locked);
    }

    #[test]
    fn read_config_honors_stored_url_when_set() {
        let _guard = lock_settings();
        let mut settings = read_settings();
        // Start clean so the stored value is the only signal.
        if let Value::Object(ref mut map) = settings {
            map.remove("privateRelay");
        }
        settings["privateRelay"] = json!({
            "enabled": true,
            "relayUrl": "wss://stored.example/v1",
        });
        let _ = write_settings(&settings);
        let cfg = read_config();
        assert!(cfg.enabled);
        assert_eq!(cfg.relay_url, "wss://stored.example/v1");
        // Restore.
        if let Value::Object(ref mut map) = settings {
            map.remove("privateRelay");
        }
        let _ = write_settings(&settings);
    }
}