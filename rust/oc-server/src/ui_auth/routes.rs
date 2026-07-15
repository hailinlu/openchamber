//! UI auth 路由 — 11 个 axum handler。
//!
//! 移植自 `core-routes.js` lines 611-737 (`registerAuthAndAccessRoutes` 的 auth 部分)。
//! 每个路由包含 tunnel scope check (tunnel/unknown-public → 403)。
//! requireSessionAuth 检查 session cookie JWT。

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use base64::Engine;
use serde_json::{json, Value};

use crate::state::AppState;
use crate::ui_auth::session::resolve_session_ttl_ms;
use crate::ui_auth::types::{
    get_bearer_token, get_client_ip, is_secure_request, parse_cookies,
};
use crate::ui_auth::SESSION_COOKIE_NAME;

// ============================================================
// 辅助函数
// ============================================================

/// 从请求 headers 提取 host。
fn extract_host(headers: &HeaderMap) -> String {
    if let Some(host) = headers.get("x-forwarded-host") {
        if let Ok(s) = host.to_str() {
            return s.split(',').next().unwrap_or("").trim().to_string();
        }
    }
    if let Some(host) = headers.get("host") {
        if let Ok(s) = host.to_str() {
            return s.trim().to_string();
        }
    }
    String::new()
}

/// 从请求 headers 提取 origin (`protocol://host`)。
fn extract_origin(headers: &HeaderMap) -> String {
    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(',').next().unwrap_or("").trim().to_lowercase())
        .unwrap_or_else(|| "http".to_string());
    let host = extract_host(headers);
    if host.is_empty() {
        String::new()
    } else {
        format!("{proto}://{host}")
    }
}

/// 从请求 headers 提取 rp_id (host 去端口)。
fn extract_rp_id(headers: &HeaderMap) -> String {
    let host = extract_host(headers);
    normalize_host(&host)
}

/// host 去端口, 转小写。
fn normalize_host(host: &str) -> String {
    let trimmed = host.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    // IPv6 literal
    if trimmed.starts_with('[') {
        if let Some(end) = trimmed.find(']') {
            return trimmed[1..end].to_lowercase();
        }
        return trimmed.to_lowercase();
    }
    // 去端口
    match trimmed.find(':') {
        Some(pos) => trimmed[..pos].to_lowercase(),
        None => trimmed.to_lowercase(),
    }
}

/// 判断请求是否在 tunnel/unknown-public scope。
fn is_tunnel_scope(state: &AppState, headers: &HeaderMap) -> bool {
    let host = extract_host(headers);
    let client_ip = get_client_ip(
        headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()),
        None,
    );
    let scope = state.tunnel_auth.classify_request_scope(&host, &client_ip.unwrap_or_default());
    scope == "tunnel" || scope == "unknown-public"
}

/// 检查 session auth (requireSessionAuth 等效)。
/// 返回 true 如果有有效 session。
fn has_valid_session(state: &AppState, headers: &HeaderMap) -> bool {
    let cookie_header = headers
        .get("cookie")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let cookies = parse_cookies(cookie_header.as_deref());
    if let Some(token) = cookies.get(SESSION_COOKIE_NAME) {
        if let Some(ref sm) = state.ui_auth.session_manager {
            return sm.is_session_valid(token);
        }
    }
    false
}

/// 检查 client auth (bearer token)。
fn authenticate_client(state: &AppState, headers: &HeaderMap) -> Option<String> {
    let auth_header = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let token = get_bearer_token(auth_header.as_deref())?;
    let client_auth = state.ui_auth.client_auth.as_ref()?;
    let result = client_auth.authenticate_bearer_token(&token, false)?;
    Some(result.client_id)
}

/// 构建未授权响应 (移植自 `respondUnauthorized`, ui-auth.js:714-722)。
fn unauthorized_response(headers: &HeaderMap) -> Response {
    let accepts_json = headers
        .get("accept")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.contains("application/json"))
        .unwrap_or(false);
    if accepts_json {
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "UI authentication required", "locked": true })),
        )
            .into_response()
    } else {
        (
            StatusCode::UNAUTHORIZED,
            [("content-type", "text/plain")],
            "Authentication required",
        )
            .into_response()
    }
}

/// tunnel scope 拒绝 JSON。
fn tunnel_locked_response(error: &str) -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(json!({ "error": error, "tunnelLocked": true })),
    )
        .into_response()
}

// ============================================================
// Handlers
// ============================================================

/// GET /auth/session — session 状态检查。
pub async fn auth_session_status(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    if is_tunnel_scope(&state, &headers) {
        // tunnel scope: 返回 tunnel session 状态
        let cookie_header = headers
            .get("cookie")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let cookies = parse_cookies(cookie_header.as_deref());
        if cookies.contains_key(crate::tunnels::tunnel_auth::TUNNEL_SESSION_COOKIE_NAME) {
            return Json(json!({ "authenticated": true, "scope": "tunnel" })).into_response();
        }
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({
                "authenticated": false,
                "locked": true,
                "tunnelLocked": true
            })),
        )
            .into_response();
    }

    // 检查 session cookie
    if has_valid_session(&state, &headers) {
        return Json(json!({ "authenticated": true })).into_response();
    }

    // 检查 client auth
    if authenticate_client(&state, &headers).is_some() {
        return Json(json!({ "authenticated": true, "scope": "client" })).into_response();
    }

    (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "authenticated": false, "locked": true })),
    )
        .into_response()
}

/// POST /auth/session — 密码登录。
pub async fn auth_session_create(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    // tunnel scope → 403
    if is_tunnel_scope(&state, &headers) {
        return tunnel_locked_response("Password login is disabled for tunnel scope");
    }

    // disabled controller (无密码)
    if !state.ui_auth.enabled {
        if state.ui_auth.require_client_auth {
            if authenticate_client(&state, &headers).is_some() {
                return Json(json!({ "authenticated": true, "disabled": true, "scope": "client" }))
                    .into_response();
            }
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({
                    "authenticated": false,
                    "locked": true,
                    "clientAuthRequired": true
                })),
            )
                .into_response();
        }
        return Json(json!({ "authenticated": true, "disabled": true })).into_response();
    }

    // enabled controller
    let client_ip = get_client_ip(
        headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()),
        None,
    );

    // rate limit check
    let rl_result = state.ui_auth.rate_limiter.check(client_ip.as_deref());

    let build_rate_limit_response = |status: StatusCode, body: Value, rl: &crate::ui_auth::rate_limit::RateLimitResult| -> Response {
        let mut resp = (status, Json(body)).into_response();
        let headers = resp.headers_mut();
        if let Ok(v) = rl.limit.to_string().parse() { headers.insert("x-ratelimit-limit", v); }
        if let Ok(v) = rl.remaining.to_string().parse() { headers.insert("x-ratelimit-remaining", v); }
        if let Ok(v) = rl.reset.to_string().parse() { headers.insert("x-ratelimit-reset", v); }
        if let Some(retry) = rl.retry_after {
            if let Ok(val) = retry.to_string().parse() {
                headers.insert("retry-after", val);
            }
        }
        resp
    };

    if !rl_result.allowed {
        return build_rate_limit_response(
            StatusCode::TOO_MANY_REQUESTS,
            json!({
                "error": "Too many login attempts, please try again later",
                "retryAfter": rl_result.retry_after
            }),
            &rl_result,
        );
    }

    // verify password
    let candidate = body.get("password").and_then(|v| v.as_str()).unwrap_or("");
    let hasher = state.ui_auth.password_hasher.as_ref().unwrap();
    if !hasher.verify(candidate) {
        state.ui_auth.rate_limiter.record_failure(client_ip.as_deref());
        return build_rate_limit_response(
            StatusCode::UNAUTHORIZED,
            json!({ "error": "Invalid credentials" }),
            &rl_result,
        );
    }

    // 成功: 清除 rate limit
    state.ui_auth.rate_limiter.clear(client_ip.as_deref());

    let trust_device = body
        .get("trustDevice")
        .map(|v| v.as_bool() == Some(true))
        .unwrap_or(false);
    let ttl_ms = resolve_session_ttl_ms(trust_device);
    let secure = is_secure_request(headers.get("x-forwarded-proto").and_then(|v| v.to_str().ok()));

    let sm = state.ui_auth.session_manager.as_ref().unwrap();
    let session_token = match sm.issue_session(trust_device) {
        Ok(t) => t,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e })),
            )
                .into_response();
        }
    };
    let cookie = sm.build_session_cookie(&session_token, ttl_ms, secure);
    let mut result = json!({ "authenticated": true });

    // 可选: 签发 client token
    if body.get("issueClientToken").and_then(|v| v.as_bool()) == Some(true) {
        if let Some(ref client_runtime) = state.ui_auth.client_auth {
            let now_ms = chrono::Utc::now().timestamp_millis();
            let expires_iso = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(now_ms + ttl_ms)
                .map(|dt| dt.to_rfc3339())
                .unwrap_or_default();
            let params = crate::client_auth::remote_clients::CreateClientParams {
                label: body.get("clientLabel").and_then(|v| v.as_str()).map(|s| s.to_string()),
                expires_at: Some(expires_iso),
                client_kind: body.get("clientKind").and_then(|v| v.as_str()).map(|s| s.to_string()),
                dedupe_key: body.get("dedupeKey").and_then(|v| v.as_str()).map(|s| s.to_string()),
                auth_method: Some("password".to_string()),
                device_name: body.get("deviceName").and_then(|v| v.as_str()).map(|s| s.to_string()),
                device_platform: body.get("devicePlatform").and_then(|v| v.as_str()).map(|s| s.to_string()),
                device_model: body.get("deviceModel").and_then(|v| v.as_str()).map(|s| s.to_string()),
                app_version: body.get("appVersion").and_then(|v| v.as_str()).map(|s| s.to_string()),
                uses_relay: false,
                ..Default::default()
            };
            if let Ok((client, token)) = client_runtime.create_client(params) {
                result["clientToken"] = Value::from(token);
                result["client"] = client;
            }
        }
    }

    (
        [
            ("set-cookie", cookie.as_str()),
            ("cache-control", "no-store"),
            ("x-ratelimit-limit", &rl_result.limit.to_string()),
            ("x-ratelimit-remaining", &rl_result.remaining.to_string()),
            ("x-ratelimit-reset", &rl_result.reset.to_string()),
        ],
        Json(result),
    )
        .into_response()
}

/// POST /auth/url-token — 签发短生命周期 URL auth token。
pub async fn auth_url_token(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    // 需要先验证 session 或 client auth
    let session_token = if has_valid_session(&state, &headers) {
        let cookie_header = headers
            .get("cookie")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let cookies = parse_cookies(cookie_header.as_deref());
        cookies.get(SESSION_COOKIE_NAME).cloned()
    } else {
        authenticate_client(&state, &headers).map(|id| format!("client:{id}"))
    };

    let token = match session_token {
        Some(t) => t,
        None => {
            // disabled controller: 可能无需认证
            if !state.ui_auth.enabled && !state.ui_auth.require_client_auth {
                // 生成 anonymous session token
                let bytes = {
                    use rand::RngCore;
                    let mut b = [0u8; 32];
                    rand::thread_rng().fill_bytes(&mut b);
                    b
                };
                let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
                let (url_token, expires_at) = state.ui_auth.url_token_store.issue(&token);
                return (
                    [("cache-control", "no-store")],
                    Json(json!({ "token": url_token, "expiresAt": expires_at })),
                )
                    .into_response();
            }
            return unauthorized_response(&headers);
        }
    };

    let (url_token, expires_at) = state.ui_auth.url_token_store.issue(&token);
    (
        [("cache-control", "no-store")],
        Json(json!({ "token": url_token, "expiresAt": expires_at })),
    )
        .into_response()
}

/// GET /auth/passkey/status — passkey 状态。
pub async fn passkey_status(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    if is_tunnel_scope(&state, &headers) {
        return Json(json!({
            "enabled": false,
            "hasPasskeys": false,
            "passkeyCount": 0,
            "rpID": null,
            "tunnelLocked": true
        }))
        .into_response();
    }
    let passkeys = match state.ui_auth.passkeys.as_ref() {
        Some(pk) => pk,
        None => {
            return Json(json!({
                "enabled": false,
                "hasPasskeys": false,
                "passkeyCount": 0,
                "rpID": null
            }))
            .into_response();
        }
    };
    let rp_id = extract_rp_id(&headers);
    Json(passkeys.get_status(&rp_id)).into_response()
}

/// POST /auth/passkey/authenticate/options — WebAuthn 认证 challenge。
pub async fn passkey_auth_options(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    if is_tunnel_scope(&state, &headers) {
        return tunnel_locked_response("Passkey login is disabled for tunnel scope");
    }
    let passkeys = match state.ui_auth.passkeys.as_ref() {
        Some(pk) => pk,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "UI password not configured" })),
            )
                .into_response();
        }
    };
    let origin = extract_origin(&headers);
    let rp_id = extract_rp_id(&headers);
    match passkeys.begin_authentication(&origin, &rp_id) {
        Ok(result) => Json(result).into_response(),
        Err(e) => (
            StatusCode::from_u16(e.status_code()).unwrap_or(StatusCode::BAD_REQUEST),
            Json(json!({ "error": e.message() })),
        )
            .into_response(),
    }
}

/// POST /auth/passkey/authenticate/verify — 验证 WebAuthn 认证 + 签发 session。
pub async fn passkey_auth_verify(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if is_tunnel_scope(&state, &headers) {
        return tunnel_locked_response("Passkey login is disabled for tunnel scope");
    }
    let passkeys = match state.ui_auth.passkeys.as_ref() {
        Some(pk) => pk,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "UI password not configured" })),
            )
                .into_response();
        }
    };

    let request_id = body
        .get("requestId")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let response = body.get("response").cloned().unwrap_or(Value::Null);

    if let Err(e) = passkeys.finish_authentication(request_id, &response) {
        return (
            StatusCode::from_u16(e.status_code()).unwrap_or(StatusCode::BAD_REQUEST),
            Json(json!({ "error": e.message() })),
        )
            .into_response();
    }

    // 签发 session
    let trust_device = body
        .get("trustDevice")
        .map(|v| v.as_bool() == Some(true))
        .unwrap_or(false);
    let ttl_ms = resolve_session_ttl_ms(trust_device);
    let secure = is_secure_request(headers.get("x-forwarded-proto").and_then(|v| v.to_str().ok()));

    let sm = state.ui_auth.session_manager.as_ref().unwrap();
    let session_token = match sm.issue_session(trust_device) {
        Ok(t) => t,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e })),
            )
                .into_response();
        }
    };
    let cookie = sm.build_session_cookie(&session_token, ttl_ms, secure);
    let mut result = json!({ "authenticated": true });

    // 可选: 签发 client token
    if body.get("issueClientToken").and_then(|v| v.as_bool()) == Some(true) {
        if let Some(ref client_runtime) = state.ui_auth.client_auth {
            let now_ms = chrono::Utc::now().timestamp_millis();
            let expires_iso = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(now_ms + ttl_ms)
                .map(|dt| dt.to_rfc3339())
                .unwrap_or_default();
            let params = crate::client_auth::remote_clients::CreateClientParams {
                label: body.get("clientLabel").and_then(|v| v.as_str()).map(|s| s.to_string()),
                expires_at: Some(expires_iso),
                client_kind: body.get("clientKind").and_then(|v| v.as_str()).map(|s| s.to_string()),
                dedupe_key: body.get("dedupeKey").and_then(|v| v.as_str()).map(|s| s.to_string()),
                auth_method: Some("passkey".to_string()),
                device_name: body.get("deviceName").and_then(|v| v.as_str()).map(|s| s.to_string()),
                device_platform: body.get("devicePlatform").and_then(|v| v.as_str()).map(|s| s.to_string()),
                device_model: body.get("deviceModel").and_then(|v| v.as_str()).map(|s| s.to_string()),
                app_version: body.get("appVersion").and_then(|v| v.as_str()).map(|s| s.to_string()),
                uses_relay: false,
                ..Default::default()
            };
            if let Ok((client, token)) = client_runtime.create_client(params) {
                result["clientToken"] = Value::from(token);
                result["client"] = client;
            }
        }
    }

    (
        [("set-cookie", cookie.as_str())],
        Json(result),
    )
        .into_response()
}

/// POST /auth/passkey/register/options — WebAuthn 注册 challenge (需 session auth)。
pub async fn passkey_register_options(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if is_tunnel_scope(&state, &headers) {
        return tunnel_locked_response("Passkey setup is disabled for tunnel scope");
    }
    if !has_valid_session(&state, &headers) {
        return unauthorized_response(&headers);
    }
    let passkeys = match state.ui_auth.passkeys.as_ref() {
        Some(pk) => pk,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "UI password not configured" })),
            )
                .into_response();
        }
    };
    let origin = extract_origin(&headers);
    let rp_id = extract_rp_id(&headers);
    let label = body.get("label").and_then(|v| v.as_str()).unwrap_or("");
    match passkeys.begin_registration(&origin, &rp_id, label) {
        Ok(result) => Json(result).into_response(),
        Err(e) => (
            StatusCode::from_u16(e.status_code()).unwrap_or(StatusCode::BAD_REQUEST),
            Json(json!({ "error": e.message() })),
        )
            .into_response(),
    }
}

/// POST /auth/passkey/register/verify — 验证 WebAuthn 注册 (需 session auth)。
pub async fn passkey_register_verify(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if is_tunnel_scope(&state, &headers) {
        return tunnel_locked_response("Passkey setup is disabled for tunnel scope");
    }
    if !has_valid_session(&state, &headers) {
        return unauthorized_response(&headers);
    }
    let passkeys = match state.ui_auth.passkeys.as_ref() {
        Some(pk) => pk,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "UI password not configured" })),
            )
                .into_response();
        }
    };
    let request_id = body
        .get("requestId")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let response = body.get("response").cloned().unwrap_or(Value::Null);
    match passkeys.finish_registration(request_id, &response) {
        Ok(result) => Json(result).into_response(),
        Err(e) => (
            StatusCode::from_u16(e.status_code()).unwrap_or(StatusCode::BAD_REQUEST),
            Json(json!({ "error": e.message() })),
        )
            .into_response(),
    }
}

/// GET /api/passkeys — 列出 passkeys (需 session auth)。
pub async fn passkey_list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    if is_tunnel_scope(&state, &headers) {
        return tunnel_locked_response("Passkey management is disabled for tunnel scope");
    }
    if !has_valid_session(&state, &headers) {
        return unauthorized_response(&headers);
    }
    let passkeys = match state.ui_auth.passkeys.as_ref() {
        Some(pk) => pk,
        None => {
            return Json(json!({ "passkeys": [] })).into_response();
        }
    };
    let rp_id = extract_rp_id(&headers);
    Json(json!({ "passkeys": passkeys.list_passkeys(&rp_id) })).into_response()
}

/// DELETE /api/passkeys/:id — 撤销 passkey (需 session auth)。
pub async fn passkey_revoke(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if is_tunnel_scope(&state, &headers) {
        return tunnel_locked_response("Passkey management is disabled for tunnel scope");
    }
    if !has_valid_session(&state, &headers) {
        return unauthorized_response(&headers);
    }
    let passkeys = match state.ui_auth.passkeys.as_ref() {
        Some(pk) => pk,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "UI password not configured" })),
            )
                .into_response();
        }
    };
    let rp_id = extract_rp_id(&headers);
    match passkeys.revoke_passkey(&rp_id, &id) {
        Ok(result) => Json(result).into_response(),
        Err(e) => (
            StatusCode::from_u16(e.status_code()).unwrap_or(StatusCode::BAD_REQUEST),
            Json(json!({ "error": e.message() })),
        )
            .into_response(),
    }
}

/// POST /api/auth/reset — 全局登出 (需 session auth)。
pub async fn auth_reset(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    if is_tunnel_scope(&state, &headers) {
        return tunnel_locked_response("Global sign-out is disabled for tunnel scope");
    }
    if !has_valid_session(&state, &headers) {
        return unauthorized_response(&headers);
    }
    match state.ui_auth.reset_auth().await {
        Ok(result) => {
            let sm = state.ui_auth.session_manager.as_ref().unwrap();
            let secure = is_secure_request(headers.get("x-forwarded-proto").and_then(|v| v.to_str().ok()));
            let cookie = sm.build_clear_cookie(secure);
            (
                [("set-cookie", cookie.as_str())],
                Json(json!({
                    "cleared": true,
                    "clearedPasskeys": result.cleared_passkeys,
                    "signedOutEverywhere": true
                })),
            )
                .into_response()
        }
        Err(crate::ui_auth::ResetAuthError::EnvSecretFixed) => (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "Global sign-out is unavailable while OPENCODE_JWT_SECRET is set"
            })),
        )
            .into_response(),
        Err(crate::ui_auth::ResetAuthError::Io(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

// 需要 IntoResponse trait
use axum::response::Response;
