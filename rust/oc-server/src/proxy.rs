//! `/api/*` 反向代理到 OpenCode 后端。
//!
//! 对应现有: `packages/web/server/lib/opencode/proxy.js` 的 catch-all `apiProxy`。
//!
//! 职责:
//!   1. 去掉 `/api` 前缀 (pathRewrite: `^/api` → ``)
//!   2. 过滤 hop-by-hop 请求头 + stripping client `authorization`
//!   3. 注入 managed Basic auth
//!   4. 设置 `accept-encoding: identity`
//!   5. 流式转发请求/响应 body (不缓冲)
//!   6. 4 分钟超时 → 504
//!   7. OpenCode 未就绪 → 503
//!
//! 对应 `packages/web/server/proxy-headers.js` 的头过滤逻辑。

use std::collections::HashSet;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Method, Request, Response, StatusCode};
use axum::response::IntoResponse;
use once_cell::sync::Lazy;
use serde_json::json;
use tokio::time;

use crate::state::AppState;

/// 代理请求超时 (对应 Node 侧 `LONG_REQUEST_TIMEOUT_MS = 4 * 60 * 1000`)。
const PROXY_TIMEOUT: Duration = Duration::from_secs(4 * 60);

/// 请求头过滤名单 (对应 `proxy-headers.js` 的 `filteredRequestHeaders`)。
///
/// 注意: `authorization` 被过滤 (客户端 UI token 不能到达上游),
/// 代理会重新注入 managed Basic auth。
static FILTERED_REQUEST_HEADERS: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    HashSet::from([
        "authorization",
        "host",
        "connection",
        "content-length",
        "transfer-encoding",
        "keep-alive",
        "te",
        "trailer",
        "upgrade",
        "accept-encoding",
    ])
});

/// 响应头过滤名单 (对应 `proxy-headers.js` 的 `filteredResponseHeaders`)。
static FILTERED_RESPONSE_HEADERS: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    HashSet::from([
        "connection",
        "content-length",
        "transfer-encoding",
        "keep-alive",
        "te",
        "trailer",
        "upgrade",
        "www-authenticate",
        "content-encoding",
    ])
});

/// `/api/*` catch-all 代理 handler。
///
/// `path` 是 `/api` 之后的剩余路径 (例如 `session`, `global/event`)。
pub async fn proxy_handler(
    State(state): State<std::sync::Arc<AppState>>,
    method: Method,
    Path(path): Path<String>,
    req: Request<Body>,
) -> Response<Body> {
    let (parts, body) = req.into_parts();

    // 1. 检查 OpenCode 就绪
    if !state.opencode_ready.load(std::sync::atomic::Ordering::Relaxed) {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(json!({ "error": "OpenCode is starting", "restarting": true })),
        )
            .into_response();
    }

    // 2. 构造目标 URL: {base_url}/{path}[?{query}]
    //    path 已去掉 /api 前缀 (axum nest 自动剥离)。
    //    query 直接拼到 URL, 避开 reqwest `.query(&str)` 的 serde_urlencoded
    //    序列化陷阱 (会把 "archived=true&limit=500" 错误编码为单个 value)。
    let base = state.opencode_base_url.trim_end_matches('/');
    let target_path = if path.starts_with('/') {
        path.as_str()
    } else {
        &format!("/{}", path)
    };
    let query_part = parts.uri.query().map(|q| format!("?{}", q)).unwrap_or_default();
    let target_url = format!("{}{}{}", base, target_path, query_part);

    // 3. 构造请求头: 过滤 + 注入 auth + identity encoding
    let mut req_headers = reqwest::header::HeaderMap::new();
    for (name, value) in parts.headers.iter() {
        let name_str = name.as_str();
        if FILTERED_REQUEST_HEADERS.contains(name_str) {
            continue;
        }
        req_headers.append(name.clone(), value.clone());
    }
    // 注入 managed Basic auth
    if let Ok(auth_val) = reqwest::header::HeaderValue::from_str(&state.opencode_auth_header) {
        req_headers.insert(reqwest::header::AUTHORIZATION, auth_val);
    }
    // 防御性: identity encoding (不压缩上游响应)
    req_headers.insert(
        reqwest::header::ACCEPT_ENCODING,
        reqwest::header::HeaderValue::from_static("identity"),
    );

    // 4. 读取请求 body (阶段 1: 缓冲; SSE 是响应流, 请求体通常不大)
    let client = &state.http_client;
    let body_bytes = axum::body::to_bytes(body, 10 * 1024 * 1024) // 10MB max
        .await
        .unwrap_or_default();
    let req_builder = client.request(method, &target_url).headers(req_headers);

    let upstream_req = match req_builder.body(body_bytes).build() {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "failed to build upstream request");
            return (
                StatusCode::BAD_GATEWAY,
                axum::Json(json!({ "error": format!("proxy build error: {}", e) })),
            )
                .into_response();
        }
    };

    // 5. 发送 (带超时)
    let upstream_resp: reqwest::Response =
        match time::timeout(PROXY_TIMEOUT, client.execute(upstream_req)).await {
            Ok(Ok(resp)) => resp,
            Ok(Err(e)) => {
                tracing::error!(error = %e, "upstream request failed");
                return (
                    StatusCode::BAD_GATEWAY,
                    axum::Json(json!({ "error": format!("upstream error: {}", e) })),
                )
                    .into_response();
            }
            Err(_) => {
                tracing::warn!("upstream request timed out after {:?}", PROXY_TIMEOUT);
                return (
                    StatusCode::GATEWAY_TIMEOUT,
                    axum::Json(json!({ "error": "upstream timeout" })),
                )
                    .into_response();
            }
        };

    // 6. 构造响应: 过滤头 + 流式 body
    let status = upstream_resp.status();
    let mut resp_headers = HeaderMap::new();
    for (name, value) in upstream_resp.headers().iter() {
        let name_str = name.as_str();
        if FILTERED_RESPONSE_HEADERS.contains(name_str) {
            continue;
        }
        resp_headers.append(name.clone(), value.clone());
    }

    let resp_body = Body::from_stream(upstream_resp.bytes_stream());

    let mut response = Response::new(resp_body);
    *response.status_mut() = status;
    *response.headers_mut() = resp_headers;
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filtered_request_headers_includes_authorization() {
        assert!(FILTERED_REQUEST_HEADERS.contains("authorization"));
        assert!(FILTERED_REQUEST_HEADERS.contains("host"));
        assert!(FILTERED_REQUEST_HEADERS.contains("accept-encoding"));
    }

    #[test]
    fn filtered_response_headers_includes_content_encoding() {
        assert!(FILTERED_RESPONSE_HEADERS.contains("content-encoding"));
        assert!(FILTERED_RESPONSE_HEADERS.contains("www-authenticate"));
        assert!(FILTERED_RESPONSE_HEADERS.contains("transfer-encoding"));
    }

    #[test]
    fn filtered_request_headers_excludes_content_type() {
        assert!(!FILTERED_REQUEST_HEADERS.contains("content-type"));
        assert!(!FILTERED_REQUEST_HEADERS.contains("x-opencode-directory"));
    }
}
