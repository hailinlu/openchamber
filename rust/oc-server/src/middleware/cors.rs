//! CORS 中间件 — 对齐 Node `packages/web/server/index.js:1328-1344`。
//!
//! 允许 packaged client origins (gridforge-ui/capacitor) 和本地开发 origin
//! (localhost/127.0.0.1:any-port)。OPTIONS 预检返回 204 + CORS 头。
//!
//! 必要性: Tauri dev 模式下 UI (vite :5180) 与 API (oc-server :<port>) 跨域,
//! 浏览器对 `credentials: 'include'` 的请求强制预检; 无 CORS 头则请求被拦截,
//! 导致 UI 无法验证会话 (SessionAuthGate 显示「无法验证 UI 会话」错误页)。
//!
//! 挂载方式: 作为最外层 `.layer()`,在 `require_api_auth` 之前,使 OPTIONS 预检
//! 不进入认证中间件。

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderName, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::Response;

/// 判断 origin 是否为允许的 client origin (packaged 或本地开发)。
///
/// 对齐 Node:
/// - `packagedClientOrigins` (`index.js:1307-1312`): gridforge-ui / capacitor / localhost (无端口)
/// - `isLocalDevClientOrigin` (`index.js:1313`): `/^https?:\/\/(localhost|127\.0\.0\.1):\d+$/`
fn is_allowed_origin(origin: &str) -> bool {
    const PACKAGED: &[&str] = &[
        "gridforge-ui://app",
        "capacitor://localhost",
        "http://localhost",
        "https://localhost",
    ];
    if PACKAGED.contains(&origin) {
        return true;
    }
    // 本地开发: http(s)://(localhost|127.0.0.1):<port>
    // 对齐 Node 正则 /^https?:\/\/(localhost|127\.0\.0\.1):\d+$/
    let rest = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
        .unwrap_or("");
    is_local_host_with_port(rest, "localhost:") || is_local_host_with_port(rest, "127.0.0.1:")
}

/// `rest` 形如 `localhost:5180` 或 `127.0.0.1:5180`; 校验 prefix 后面是非空纯数字端口。
fn is_local_host_with_port(rest: &str, prefix: &str) -> bool {
    match rest.strip_prefix(prefix) {
        Some(port) => !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()),
        None => false,
    }
}

/// CORS 中间件入口。
pub async fn cors_layer(request: Request, next: Next) -> Response {
    let origin = request
        .headers()
        .get("origin")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    // 非 allowed origin: 不加 CORS 头, 直接放行 (浏览器会自行拒绝跨域读取)。
    if !is_allowed_origin(&origin) {
        return next.run(request).await;
    }

    // OPTIONS 预检: 直接回 204 + CORS 头 (不走后续 handler/认证)。
    if request.method() == Method::OPTIONS {
        let mut response = Response::new(Body::empty());
        *response.status_mut() = StatusCode::NO_CONTENT;
        set_cors_headers(response.headers_mut(), &origin);
        return response;
    }

    let mut response = next.run(request).await;
    set_cors_headers(response.headers_mut(), &origin);
    response
}

/// 写入 CORS 响应头 (对齐 Node `index.js:1329-1334`)。
fn set_cors_headers(headers: &mut axum::http::HeaderMap, origin: &str) {
    headers.insert(
        HeaderName::from_static("access-control-allow-origin"),
        // origin 已由 is_allowed_origin 校验, 不含非法字符。
        HeaderValue::from_str(origin).unwrap_or(HeaderValue::from_static("null")),
    );
    headers.insert(
        HeaderName::from_static("access-control-allow-credentials"),
        HeaderValue::from_static("true"),
    );
    headers.insert(
        HeaderName::from_static("access-control-allow-methods"),
        HeaderValue::from_static("GET,POST,PUT,PATCH,DELETE,OPTIONS"),
    );
    headers.insert(
        HeaderName::from_static("access-control-allow-headers"),
        HeaderValue::from_static(
            "Content-Type,Authorization,Accept,X-Requested-With,Cache-Control,X-OpenCode-Directory,X-OpenCode-Directory-Encoding",
        ),
    );
    headers.insert(
        HeaderName::from_static("access-control-expose-headers"),
        HeaderValue::from_static("x-next-cursor"),
    );
    headers.insert(
        HeaderName::from_static("vary"),
        HeaderValue::from_static("Origin"),
    );
}

// =========================================================================
// 测试
// =========================================================================
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_packaged_origins() {
        for origin in [
            "gridforge-ui://app",
            "capacitor://localhost",
            "http://localhost",
            "https://localhost",
        ] {
            assert!(is_allowed_origin(origin), "{} should be allowed", origin);
        }
    }

    #[test]
    fn allows_local_dev_origins() {
        for origin in [
            "http://localhost:5180",
            "http://127.0.0.1:5180",
            "https://localhost:3000",
            "https://127.0.0.1:8080",
            "http://127.0.0.1:0",
            "http://127.0.0.1:65535",
        ] {
            assert!(is_allowed_origin(origin), "{} should be allowed", origin);
        }
    }

    #[test]
    fn rejects_disallowed_origins() {
        for origin in [
            "https://evil.com",
            "http://example.com:5180",
            "http://192.168.1.1:5180",
            // 注意: "http://localhost" / "https://localhost" 在 PACKAGED 里, 是允许的 (对齐 Node)
            "http://127.0.0.1",
            "http://127.0.0.1:abc",
            "",
            "null",
            "file:///etc/passwd",
        ] {
            assert!(!is_allowed_origin(origin), "{} should be rejected", origin);
        }
    }

    #[test]
    fn rejects_malformed_schemes() {
        // 非 http(s) scheme、无 scheme、端口段含非数字
        for origin in [
            "ftp://localhost:5180",
            "localhost:5180",
            "http://localhost:5180.evil.com",
            "http://127.0.0.1:8080path",
        ] {
            assert!(!is_allowed_origin(origin), "{} should be rejected", origin);
        }
    }
}
