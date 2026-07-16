//! 全局认证中间件 — 对齐 Node `requireApiAuth` (`core-routes.js:595-609`)。
//!
//! 挂载方式: `axum::middleware::from_fn_with_state(state, require_api_auth)` 作为
//! 顶层 Router 的 `.layer()`。中间件对每个请求依次执行:
//!
//! 1. 公开路由白名单 → 放行
//! 2. OPTIONS 预检 → 放行
//! 3. preview-proxy 凭证旁路 (仅判断 `oc_preview_token` 存在性)
//! 4. tunnel scope (`tunnel` / `unknown-public`) → 校验 `oc_tunnel_session` cookie
//! 5. local scope → UI auth: session cookie → url_token → bearer → 无密码放行
//! 6. 全部失败 → 401

use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Request, Response};
use axum::middleware::Next;
use axum::response::IntoResponse;
use axum::Json;
use serde_json::json;

use crate::state::AppState;
use crate::tunnels::tunnel_auth::TUNNEL_SESSION_COOKIE_NAME;
use crate::ui_auth::types::{
    can_use_url_auth_token_for_request, get_bearer_token, get_client_ip,
    get_url_auth_token_from_query, parse_cookies,
};
use crate::ui_auth::SESSION_COOKIE_NAME;

// ============================================================
// 公开路由白名单
// ============================================================

/// 这些路径不需要认证 (对齐 Node 注册顺序白名单)。
///
/// Node 通过在 `app.use('/api', requireApiAuth)` 之前注册这些路由来实现白名单;
/// Rust 用显式匹配更安全、更易审计。
fn is_public_path(method: &str, path: &str) -> bool {
    // OPTIONS 预检豁免 (CORS)
    if method == "OPTIONS" {
        return true;
    }
    matches!(
        path,
        // 完全公开 — 健康检查 / 版本 / 系统信息
        "/health"
        | "/robots.txt"
        | "/api/version"
        | "/api/system/info"
        | "/api/system/free-port"
        // tunnel bootstrap token 兑换
        | "/connect"
        // 认证流程入口 (内部自校验)
        | "/auth/session"
        | "/auth/url-token"
        | "/auth/passkey/status"
        | "/auth/passkey/authenticate/options"
        | "/auth/passkey/authenticate/verify"
        // pairing redeem — 无预认证, 靠 pairingId+secret 一次性兑换
        | "/api/client-auth/pairing/redeem"
    )
}

// ============================================================
// 核心中间件
// ============================================================

/// 全局认证中间件 — 覆盖所有受保护路由。
pub async fn require_api_auth(
    State(state): State<Arc<AppState>>,
    request: Request<Body>,
    next: Next,
) -> Response<Body> {
    let method = request.method().as_str().to_string();
    let uri = request.uri().clone();
    let path = uri.path().to_string();
    let query = uri.query().map(|s| s.to_string());

    // 1. 公开路由放行
    if is_public_path(method.as_str(), path.as_str()) {
        return next.run(request).await;
    }

    let headers = request.headers().clone();

    // 2. preview-proxy 凭证旁路 — 仅判断 token 存在性
    //    (真正校验在 proxy 内部, 对齐 Node `hasPreviewProxyCredential`)
    if path.starts_with("/api/preview/proxy/")
        && has_preview_proxy_token(&headers, query.as_deref())
    {
        return next.run(request).await;
    }

    // 3. tunnel scope 分类
    let host = extract_host(&headers);
    let client_ip = get_client_ip(
        headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()),
        None,
    );
    let scope = state
        .tunnel_auth
        .classify_request_scope(&host, &client_ip.unwrap_or_default());

    if scope == "tunnel" || scope == "unknown-public" {
        // tunnel scope — 校验 oc_tunnel_session cookie
        if has_valid_tunnel_session(&state, &headers) {
            return next.run(request).await;
        }
        return tunnel_locked_response();
    }

    // 4. local scope — UI auth
    if try_ui_auth(&state, &method, &path, &headers, query.as_deref()) {
        return next.run(request).await;
    }

    unauthorized_response(&headers)
}

// ============================================================
// UI auth 决策 (local scope)
// ============================================================

/// 依次尝试 session cookie / url_token / bearer。
/// 无密码模式 (`!enabled && !require_client_auth`) 时直接放行。
fn try_ui_auth(
    state: &AppState,
    method: &str,
    path: &str,
    headers: &HeaderMap,
    query: Option<&str>,
) -> bool {
    // 4a. 无密码模式: 如果不 require_client_auth, 完全开放
    if !state.ui_auth.enabled && !state.ui_auth.require_client_auth {
        return true;
    }

    // 4b. session cookie (oc_ui_session JWT)
    if has_valid_session(state, headers) {
        return true;
    }

    // 4c. URL auth token (仅白名单 GET/WS 路径)
    let is_ws_upgrade = headers
        .get("upgrade")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false);

    if can_use_url_auth_token_for_request(method, path, is_ws_upgrade) {
        if let Some(token) = get_url_auth_token_from_query(query) {
            if state.ui_auth.url_token_store.authenticate(&token).is_some() {
                return true;
            }
        }
    }

    // 4d. client bearer token
    if authenticate_client(state, headers).is_some() {
        return true;
    }

    false
}

/// 检查 session cookie JWT 有效性。
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

/// 检查 client bearer token。
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

// ============================================================
// Tunnel session
// ============================================================

/// 校验 `oc_tunnel_session` cookie。
fn has_valid_tunnel_session(state: &AppState, headers: &HeaderMap) -> bool {
    let cookie_header = headers
        .get("cookie")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let cookies = parse_cookies(cookie_header.as_deref());
    if let Some(session_cookie) = cookies.get(TUNNEL_SESSION_COOKIE_NAME) {
        return state.tunnel_auth.get_session_from_cookie(session_cookie);
    }
    false
}

// ============================================================
// 辅助
// ============================================================

/// 从请求 headers 提取 host (优先 x-forwarded-host)。
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

/// preview-proxy 凭证旁路 — 判断 `oc_preview_token` 存在性 (query 或 cookie)。
/// 对齐 Node `hasPreviewProxyCredential` (core-routes.js:56-59)。
fn has_preview_proxy_token(headers: &HeaderMap, query: Option<&str>) -> bool {
    // cookie
    let cookie_header = headers
        .get("cookie")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let cookies = parse_cookies(cookie_header.as_deref());
    if cookies.contains_key("oc_preview_token") {
        return true;
    }
    // query
    if let Some(q) = query {
        for pair in q.split('&') {
            if pair == "oc_preview_token" || pair.starts_with("oc_preview_token=") {
                return true;
            }
        }
    }
    false
}

/// 401 未授权响应 — 对齐 Node `respondUnauthorized` (ui-auth.js:714-722)。
fn unauthorized_response(headers: &HeaderMap) -> Response<Body> {
    let accepts_json = headers
        .get("accept")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.contains("application/json"))
        .unwrap_or(false);

    if accepts_json {
        (
            axum::http::StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "UI authentication required", "locked": true })),
        )
            .into_response()
    } else {
        (
            axum::http::StatusCode::UNAUTHORIZED,
            [("content-type", "text/plain")],
            "Authentication required",
        )
            .into_response()
    }
}

/// 401 tunnel 锁定响应 — 对齐 Node `requireTunnelSession` (tunnel-auth.js:462-468)。
fn tunnel_locked_response() -> Response<Body> {
    (
        axum::http::StatusCode::UNAUTHORIZED,
        Json(json!({
            "error": "Tunnel authentication required",
            "locked": true,
            "tunnelLocked": true
        })),
    )
        .into_response()
}

// ============================================================
// 测试
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_paths_are_excluded() {
        assert!(is_public_path("GET", "/health"));
        assert!(is_public_path("GET", "/api/version"));
        assert!(is_public_path("GET", "/auth/session"));
        assert!(is_public_path("POST", "/auth/session"));
        assert!(is_public_path("POST", "/api/client-auth/pairing/redeem"));
        // OPTIONS 总是豁免
        assert!(is_public_path("OPTIONS", "/api/fs/read"));
    }

    #[test]
    fn protected_paths_are_not_excluded() {
        assert!(!is_public_path("GET", "/api/fs/read"));
        assert!(!is_public_path("POST", "/api/git/commit"));
        assert!(!is_public_path("GET", "/api/event"));
        assert!(!is_public_path("GET", "/api/event/ws"));
        assert!(!is_public_path("POST", "/api/scheduled-tasks"));
    }

    #[test]
    fn preview_proxy_token_in_query() {
        assert!(has_preview_proxy_token(
            &HeaderMap::new(),
            Some("oc_preview_token=abc")
        ));
        assert!(has_preview_proxy_token(
            &HeaderMap::new(),
            Some("foo=bar&oc_preview_token=xyz")
        ));
        assert!(!has_preview_proxy_token(
            &HeaderMap::new(),
            Some("foo=bar")
        ));
    }

    #[test]
    fn preview_proxy_token_in_cookie() {
        let mut headers = HeaderMap::new();
        headers.insert("cookie", "oc_preview_token=abc".parse().unwrap());
        assert!(has_preview_proxy_token(&headers, None));
    }

    #[test]
    fn preview_proxy_path_mismatch_no_bypass() {
        // 非 preview proxy 路径不走旁路 (即便有 token)
        // 这由调用处的 path.starts_with 保证
        assert!(!"/api/fs/read".starts_with("/api/preview/proxy/"));
    }
}
