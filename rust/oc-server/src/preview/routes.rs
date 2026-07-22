//! Preview 模块路由: HTTP 反向代理 + WebSocket 升级代理 + 目标创建。
//!
//! 对应 `proxy-runtime.js` 的 `attach()` 方法 (proxy-runtime.js:1306-1593)。

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, Request, Response, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio_tungstenite::tungstenite::Message as TungsteniteMessage;

use super::cookies::build_cookie;
use super::normalize::normalize_proxy_target_url;
use super::rewrite::{
    inject_preview_bridge, rewrite_preview_body, rewrite_preview_csp_header,
    rewrite_preview_redirect_location, rewrite_vite_client_hmr, RewriteKind, RewriteParams,
};
use super::targets::{build_upstream_url, http_origin_to_ws, ResolvedTarget};
use super::{
    PREVIEW_PASSTHROUGH_REQUEST_HEADERS, PREVIEW_PASSTHROUGH_RESPONSE_HEADERS,
    PREVIEW_PROXY_MAX_BODY_BYTES, PREVIEW_PROXY_TIMEOUT_SECS, TOKEN_COOKIE_NAME,
};
use crate::state::AppState;

/// 随机 nonce (base64, 16 字节)。
fn random_nonce() -> String {
    let bytes: [u8; 16] = rand::random();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

// ============================================================
// 1. POST /api/preview/targets
// ============================================================

#[derive(serde::Deserialize)]
pub struct CreateTargetBody {
    pub url: String,
    #[serde(default)]
    pub ttl_ms: Option<u64>,
    #[serde(default)]
    pub allow_external: Option<bool>,
}

/// `POST /api/preview/targets` — 创建短命代理目标。
///
/// 对应 proxy-runtime.js:1351-1400。
pub async fn post_targets_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateTargetBody>,
) -> Response<Body> {
    let raw_url = body.url.trim().to_string();
    if raw_url.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "url is required");
    }
    let allow_external = body.allow_external.unwrap_or(false);
    let normalized = match normalize_proxy_target_url(&raw_url, allow_external) {
        Ok(n) => n,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, &e),
    };

    let ttl_ms = body.ttl_ms.unwrap_or(0);
    let (id, token, expires_ms) = state
        .preview_targets
        .create_target(normalized.origin.clone(), ttl_ms)
        .await;
    let cookie_path = format!("/api/preview/proxy/{}", id);
    let secure = false; // preview 仅 loopback, 非 TLS
    let cookie = build_cookie(
        TOKEN_COOKIE_NAME,
        &token,
        Some(&cookie_path),
        Some(expires_ms / 1000),
        secure,
    );

    let mut resp = Json(json!({
        "id": id,
        "proxyBasePath": cookie_path,
        "previewToken": token,
        "expiresAt": chrono::Utc::now().timestamp_millis() as u64 + expires_ms,
    }))
    .into_response();
    resp.headers_mut().insert(
        "set-cookie",
        HeaderValue::from_str(&cookie).unwrap_or_else(|_| HeaderValue::from_static("")),
    );
    resp
}

// ============================================================
// 2. HTTP 反向代理 + WebSocket 升级代理 (统一入口)
// ============================================================

/// `ANY /api/preview/proxy/:id/*` — HTTP 代理或 WS 升级代理的统一入口。
///
/// Node 侧 HTTP 代理和 WS 升级共用同一路径 (`server.on('upgrade')` vs `app.use`)。
/// axum 通过 `Result<WebSocketUpgrade, WebSocketUpgradeRejection>` 区分:
/// WS 升级请求提取 `Ok`, 普通请求提取 `Err`。
pub async fn proxy_or_ws_handler(
    ws_upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
    State(state): State<Arc<AppState>>,
    Path((id, rest)): Path<(String, String)>,
    Query(query_map): Query<std::collections::HashMap<String, String>>,
    method: Method,
    req: Request<Body>,
) -> Response<Body> {
    let full_path = format!("/api/preview/proxy/{}/{}", id, rest);
    let query_string = reconstruct_query(&query_map);

    match ws_upgrade {
        Ok(ws) => preview_ws_upgrade(ws, state, full_path, query_string).await,
        Err(_) => proxy_http(state, full_path, query_string, method, req).await,
    }
}

/// WS 升级分支。
async fn preview_ws_upgrade(
    ws: WebSocketUpgrade,
    state: Arc<AppState>,
    full_path: String,
    query_string: String,
) -> Response<Body> {
    let resolved = match state
        .preview_targets
        .resolve_target_from_request(&full_path, Some(&query_string), None)
        .await
    {
        Ok(r) => r,
        Err(_) => return StatusCode::FORBIDDEN.into_response(),
    };
    let ws_origin = http_origin_to_ws(&resolved.target.origin);
    let upstream_ws_url =
        build_upstream_url(&ws_origin, &resolved.stripped_path, Some(&query_string));
    ws.on_upgrade(move |socket| run_preview_ws_bridge(socket, upstream_ws_url))
}

/// HTTP 代理分支。
///
/// 对应 proxy-runtime.js:1551-1557 + proxy 中间件 (1402-1548)。
async fn proxy_http(
    state: Arc<AppState>,
    full_path: String,
    query_string: String,
    method: Method,
    req: Request<Body>,
) -> Response<Body> {
    let (parts, body) = req.into_parts();

    // 解析 target
    let resolved = match state
        .preview_targets
        .resolve_target_from_request(
            &full_path,
            Some(&query_string),
            parts.headers.get("cookie").and_then(|v| v.to_str().ok()),
        )
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return error_response(
                StatusCode::from_u16(e.status).unwrap_or(StatusCode::NOT_FOUND),
                &e.error,
            )
        }
    };

    // 构造上游 URL
    let upstream_url = build_upstream_url(
        &resolved.target.origin,
        &resolved.stripped_path,
        Some(&query_string),
    );

    // 构造请求头: 过滤凭证 + passthrough Inertia + identity encoding
    let mut req_headers = reqwest::header::HeaderMap::new();
    for (name, value) in parts.headers.iter() {
        let name_str = name.as_str();
        if name_str == "cookie"
            || name_str == "authorization"
            || name_str == "x-gridforge-ui-session"
        {
            continue;
        }
        req_headers.append(name.clone(), value.clone());
    }
    for passthrough in PREVIEW_PASSTHROUGH_REQUEST_HEADERS {
        if let Some(value) = parts.headers.get(*passthrough) {
            if let Ok(name) = HeaderName::from_bytes(passthrough.as_bytes()) {
                req_headers.insert(name, value.clone());
            }
        }
    }
    req_headers.insert("accept-encoding", HeaderValue::from_static("identity"));

    // 读取请求 body
    let body_bytes = axum::body::to_bytes(body, PREVIEW_PROXY_MAX_BODY_BYTES)
        .await
        .unwrap_or_default();

    // 发送 (带超时)
    let client = &state.http_client;
    let upstream_req = match client
        .request(method, &upstream_url)
        .headers(req_headers)
        .body(body_bytes)
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "preview proxy: failed to build upstream request");
            return error_response(StatusCode::BAD_GATEWAY, "Preview proxy build error");
        }
    };

    let upstream_resp = match tokio::time::timeout(
        Duration::from_secs(PREVIEW_PROXY_TIMEOUT_SECS),
        client.execute(upstream_req),
    )
    .await
    {
        Ok(Ok(resp)) => resp,
        Ok(Err(e)) => {
            tracing::error!(error = %e, "preview proxy: upstream request failed");
            return error_response(StatusCode::BAD_GATEWAY, "Preview proxy error");
        }
        Err(_) => {
            tracing::warn!("preview proxy: upstream timed out");
            return error_response(StatusCode::GATEWAY_TIMEOUT, "Preview proxy timeout");
        }
    };

    build_proxy_response(upstream_resp, &resolved).await
}

/// 构造代理响应: 重写 headers + 缓冲 body + body 重写 + bridge 注入。
async fn build_proxy_response(
    upstream_resp: reqwest::Response,
    resolved: &ResolvedTarget,
) -> Response<Body> {
    let status = upstream_resp.status();
    let upstream_headers = upstream_resp.headers().clone();

    // 生成 per-response nonce
    let bridge_nonce = random_nonce();

    // 缓冲 body
    let body_bytes = match axum::body::to_bytes(
        Body::from_stream(upstream_resp.bytes_stream()),
        PREVIEW_PROXY_MAX_BODY_BYTES,
    )
    .await
    {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(error = %e, "preview proxy: failed to buffer upstream body");
            return error_response(StatusCode::BAD_GATEWAY, "Preview proxy body error");
        }
    };

    // 构造响应头
    let mut resp_headers = HeaderMap::new();
    let content_type = upstream_headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_lowercase();
    let is_html = content_type.contains("text/html");
    let is_css = content_type.contains("text/css");
    let is_js = content_type.contains("javascript") || content_type.contains("ecmascript");
    let proxy_base_path = format!("/api/preview/proxy/{}", resolved.target.id);

    for (name, value) in upstream_headers.iter() {
        let name_str = name.as_str();
        let lower = name_str.to_lowercase();

        // 跳过 frame-busting
        if lower == "x-frame-options" {
            continue;
        }
        // CSP 重写
        if lower == "content-security-policy" || lower == "content-security-policy-report-only" {
            if let Ok(csp_val) = value.to_str() {
                if let Some(rewritten) = rewrite_preview_csp_header(csp_val, &bridge_nonce) {
                    resp_headers.insert(
                        name,
                        HeaderValue::from_str(&rewritten).unwrap_or_else(|_| value.clone()),
                    );
                }
            }
            continue;
        }
        // Location 重写
        if lower == "location" {
            if let Ok(loc) = value.to_str() {
                let rewritten = rewrite_preview_redirect_location(
                    loc,
                    &proxy_base_path,
                    &resolved.target.origin,
                    &resolved.target.token,
                    &resolved.url_auth_token,
                );
                resp_headers.insert(
                    name,
                    HeaderValue::from_str(&rewritten).unwrap_or_else(|_| value.clone()),
                );
                continue;
            }
        }
        // 过滤 hop-by-hop / 缓存相关 (后续会重设)
        if lower == "content-length"
            || lower == "transfer-encoding"
            || lower == "connection"
            || lower == "etag"
            || lower == "last-modified"
            || lower == "cache-control"
            || lower == "pragma"
            || lower == "expires"
        {
            continue;
        }
        resp_headers.append(name.clone(), value.clone());
    }

    // Inertia 响应透传
    for passthrough in PREVIEW_PASSTHROUGH_RESPONSE_HEADERS {
        if let Some(value) = upstream_headers.get(*passthrough) {
            if let Ok(name) = HeaderName::from_bytes(passthrough.as_bytes()) {
                resp_headers.insert(name, value.clone());
            }
        }
    }

    // body 重写 (html/css/javascript)
    let final_body: Vec<u8> = if body_bytes.is_empty() {
        body_bytes.to_vec()
    } else if is_html || is_css || is_js {
        let body_text = match std::str::from_utf8(&body_bytes) {
            Ok(s) => s,
            Err(_) => {
                let mut response = Response::new(Body::from(body_bytes));
                *response.status_mut() = status;
                *response.headers_mut() = resp_headers;
                return response;
            }
        };
        let kind = if is_html {
            RewriteKind::Html
        } else if is_css {
            RewriteKind::Css
        } else {
            RewriteKind::JavaScript
        };
        // Vite client 特殊处理
        let rewritten = if is_js && resolved.stripped_path == "/@vite/client" {
            let hmr_patched = rewrite_vite_client_hmr(body_text, &proxy_base_path);
            rewrite_preview_body(&RewriteParams {
                body_text: &hmr_patched,
                proxy_base_path: &proxy_base_path,
                target_origin: &resolved.target.origin,
                kind,
                preview_token: &resolved.target.token,
                url_auth_token: &resolved.url_auth_token,
            })
        } else {
            rewrite_preview_body(&RewriteParams {
                body_text,
                proxy_base_path: &proxy_base_path,
                target_origin: &resolved.target.origin,
                kind,
                preview_token: &resolved.target.token,
                url_auth_token: &resolved.url_auth_token,
            })
        };
        if is_html {
            inject_preview_bridge(&rewritten, &resolved.target.origin, &bridge_nonce).into_bytes()
        } else {
            rewritten.into_bytes()
        }
    } else {
        body_bytes.to_vec()
    };

    // HTML/CSS/JS: 设置 no-cache headers
    if is_html || is_css || is_js {
        resp_headers.insert(
            "cache-control",
            HeaderValue::from_static(
                "no-store, no-cache, must-revalidate, proxy-revalidate",
            ),
        );
        resp_headers.insert("pragma", HeaderValue::from_static("no-cache"));
        resp_headers.insert("expires", HeaderValue::from_static("0"));
    }

    let mut response = Response::new(Body::from(final_body));
    *response.status_mut() = status;
    *response.headers_mut() = resp_headers;
    response
}

// ============================================================
// 3. WebSocket 双向桥
// ============================================================

/// WS 双向桥: 浏览器 ↔ 上游 dev server。
///
/// 对应 `proxy.upgrade` (proxy-runtime.js:1586)。
async fn run_preview_ws_bridge(browser_socket: WebSocket, upstream_url: String) {
    let upstream_result = tokio_tungstenite::connect_async(&upstream_url).await;
    let upstream_socket = match upstream_result {
        Ok((s, _)) => s,
        Err(e) => {
            tracing::warn!(error = %e, url = %upstream_url, "preview WS: failed to connect upstream");
            return;
        }
    };

    let (mut browser_tx, mut browser_rx) = browser_socket.split();
    let (mut upstream_tx, mut upstream_rx) = upstream_socket.split();

    // 浏览器 → 上游
    let browser_to_upstream = tokio::spawn(async move {
        while let Some(Ok(msg)) = browser_rx.next().await {
            let tungstenite_msg = match msg {
                Message::Text(t) => {
                    TungsteniteMessage::Text(t.to_string().into())
                }
                Message::Binary(b) => {
                    TungsteniteMessage::Binary(bytes::Bytes::from(b.to_vec()))
                }
                Message::Ping(v) => {
                    TungsteniteMessage::Ping(bytes::Bytes::from(v.to_vec()))
                }
                Message::Pong(v) => {
                    TungsteniteMessage::Pong(bytes::Bytes::from(v.to_vec()))
                }
                Message::Close(_) => {
                    let _ = upstream_tx.close().await;
                    break;
                }
            };
            if upstream_tx.send(tungstenite_msg).await.is_err() {
                break;
            }
        }
    });

    // 上游 → 浏览器
    let upstream_to_browser = tokio::spawn(async move {
        while let Some(Ok(msg)) = upstream_rx.next().await {
            let axum_msg = match msg {
                TungsteniteMessage::Text(t) => Message::Text(t.to_string().into()),
                TungsteniteMessage::Binary(b) => Message::Binary(bytes::Bytes::from_owner(
                    b.to_vec(),
                )),
                TungsteniteMessage::Ping(v) => {
                    Message::Ping(bytes::Bytes::from(v.to_vec()))
                }
                TungsteniteMessage::Pong(v) => {
                    Message::Pong(bytes::Bytes::from(v.to_vec()))
                }
                TungsteniteMessage::Close(_) => {
                    let _ = browser_tx.close().await;
                    break;
                }
                TungsteniteMessage::Frame(_) => continue,
            };
            if browser_tx.send(axum_msg).await.is_err() {
                break;
            }
        }
    });

    let _ = tokio::join!(browser_to_upstream, upstream_to_browser);
}

// ============================================================
// 辅助
// ============================================================

/// 将 query HashMap 还原为 query string。
fn reconstruct_query(map: &std::collections::HashMap<String, String>) -> String {
    if map.is_empty() {
        return String::new();
    }
    let pairs: Vec<String> = map.iter().map(|(k, v)| format!("{}={}", k, v)).collect();
    format!("?{}", pairs.join("&"))
}

/// JSON 错误响应。
fn error_response(status: StatusCode, message: &str) -> Response<Body> {
    (status, Json(json!({ "error": message }))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconstruct_query_builds_string() {
        let mut map = std::collections::HashMap::new();
        map.insert("a".to_string(), "1".to_string());
        map.insert("b".to_string(), "2".to_string());
        let q = reconstruct_query(&map);
        assert!(q.starts_with('?'));
        assert!(q.contains("a=1"));
        assert!(q.contains("b=2"));
    }

    #[test]
    fn reconstruct_query_empty() {
        let map = std::collections::HashMap::new();
        assert_eq!(reconstruct_query(&map), "");
    }

    #[test]
    fn random_nonce_is_base64() {
        let nonce = random_nonce();
        assert_eq!(nonce.len(), 24);
        assert!(base64::engine::general_purpose::STANDARD.decode(&nonce).is_ok());
    }
}
