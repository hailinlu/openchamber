//! UI auth 类型 + 辅助函数 (cookie 解析/构建、URL-token 路径白名单、IP 解析等)。
//!
//! 移植自 `ui-auth.js` lines 212-361 辅助函数。纯函数, 无 I/O。

use std::collections::HashMap;

use once_cell::sync::Lazy;
use regex::Regex;

/// 两个 URL-token 白名单正则 (移植自 ui-auth.js:304-305)。
#[allow(dead_code)]
static TERMINAL_STREAM_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^/api/terminal/[^/]+/stream$").unwrap());
#[allow(dead_code)]
static PROJECT_ICON_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^/api/projects/[^/]+/icon$").unwrap());

/// Bearer token 提取正则 (移植自 ui-auth.js:252)。
static BEARER_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)^Bearer\s+(.+)$").unwrap());

/// 解析 Cookie header → HashMap。
///
/// 移植自 `parseCookies` (ui-auth.js:224-246)。`decodeURIComponent` 对纯 ASCII 是 no-op;
/// Rust 用 percent-decoding 处理。
pub fn parse_cookies(cookie_header: Option<&str>) -> HashMap<String, String> {
    let mut result = HashMap::new();
    let header = match cookie_header {
        Some(h) if !h.is_empty() => h,
        _ => return result,
    };
    for segment in header.split(';') {
        let mut parts = segment.splitn(2, '=');
        let name = match parts.next() {
            Some(n) => n.trim(),
            None => continue,
        };
        if name.is_empty() {
            continue;
        }
        let raw_value = parts.next().unwrap_or("").trim();
        let decoded = percent_decode(raw_value);
        result.insert(name.to_string(), decoded);
    }
    result
}

/// 构建 Set-Cookie header value。
///
/// 移植自 `buildCookie` (ui-auth.js:326-354)。
/// 格式: `name=value; Path=/; HttpOnly; SameSite=Strict; Max-Age=...; Expires=...; [Secure]`
pub fn build_cookie(name: &str, value: &str, max_age_seconds: i64, secure: bool) -> String {
    let mut attributes = vec![
        format!("{name}={value}"),
        "Path=/".to_string(),
        "HttpOnly".to_string(),
        "SameSite=Strict".to_string(),
    ];

    attributes.push(format!("Max-Age={}", std::cmp::max(0, max_age_seconds)));

    let expires = if max_age_seconds == 0 {
        "Thu, 01 Jan 1970 00:00:00 GMT".to_string()
    } else {
        // Expires = now + maxAge * 1000 ms → RFC 1123 格式
        let now_ms = chrono::Utc::now().timestamp_millis();
        let expires_ts = now_ms + max_age_seconds * 1000;
        format_http_date(expires_ts)
    };
    attributes.push(format!("Expires={expires}"));

    if secure {
        attributes.push("Secure".to_string());
    }

    attributes.join("; ")
}

/// 从 Authorization header 提取 Bearer token。
///
/// 移植自 `getBearerTokenFromRequest` (ui-auth.js:248-257)。
pub fn get_bearer_token(auth_header: Option<&str>) -> Option<String> {
    let value = auth_header?;
    let caps = BEARER_RE.captures(value)?;
    let token = caps.get(1)?.as_str().trim();
    if token.is_empty() {
        None
    } else {
        Some(token.to_string())
    }
}

/// 从 query string 提取 `oc_url_token`。
///
/// 移植自 `getUrlAuthTokenFromRequest` (ui-auth.js:259-270)。
pub fn get_url_auth_token_from_query(query: Option<&str>) -> Option<String> {
    let q = query?;
    // 简单解析 `key=value&key2=value2`
    for pair in q.split('&') {
        let mut parts = pair.splitn(2, '=');
        if parts.next() == Some("oc_url_token") {
            let val = parts.next().unwrap_or("").trim();
            if !val.is_empty() {
                return Some(percent_decode(val));
            }
        }
    }
    None
}

/// normalize password: trim。
///
/// Node 端调用 `candidate.normalize()` (NFC), 但 NFC 对 ASCII 输入是 no-op。
/// Rust 不引入 unicode-normalization crate, 仅做 trim。
pub fn normalize_password(candidate: &str) -> &str {
    candidate.trim()
}

/// 判断 trustDevice body 字段 (移植自 ui-auth.js:363)。
#[allow(dead_code)]
pub fn is_trusted_device_request(value: &serde_json::Value) -> bool {
    value.as_bool() == Some(true)
}

/// URL auth token 可读的 HTTP GET 路径白名单。
///
/// 移植自 `isUrlAuthReadableHttpPath` (ui-auth.js:294-306)。
pub fn is_url_auth_readable_http_path(pathname: &str) -> bool {
    pathname == "/api/event"
        || pathname == "/api/global/event"
        || pathname == "/api/openchamber/events"
        || pathname == "/api/openchamber/realtime-proxy/sse"
        || pathname == "/api/notifications/stream"
        || pathname == "/api/fs/raw"
        || pathname == "/api/fs/serve"
        || pathname.starts_with("/api/fs/serve/")
        || pathname.starts_with("/api/preview/proxy/")
        || TERMINAL_STREAM_RE.is_match(pathname)
        || PROJECT_ICON_RE.is_match(pathname)
}

/// URL auth token 可用的 WebSocket 路径白名单。
///
/// 移植自 `isUrlAuthWebSocketPath` (ui-auth.js:308-315)。
pub fn is_url_auth_websocket_path(pathname: &str) -> bool {
    pathname == "/api/event/ws"
        || pathname == "/api/global/event/ws"
        || pathname == "/api/openchamber/realtime-proxy/ws"
        || pathname == "/api/terminal/ws"
        || pathname == "/api/dictation/ws"
        || pathname.starts_with("/api/preview/proxy/")
}

/// 判断请求是否可以使用 URL auth token。
///
/// 移植自 `canUseUrlAuthTokenForRequest` (ui-auth.js:317-324)。
pub fn can_use_url_auth_token_for_request(
    method: &str,
    pathname: &str,
    is_websocket_upgrade: bool,
) -> bool {
    if is_websocket_upgrade {
        return is_url_auth_websocket_path(pathname);
    }
    method.eq_ignore_ascii_case("GET") && is_url_auth_readable_http_path(pathname)
}

/// 从请求 headers 解析客户端 IP。
///
/// 移植自 `getClientIp` (ui-auth.js:25-43)。
/// 优先 `x-forwarded-for` 第一项, 否则返回 None。
/// 去除 IPv4-mapped IPv6 前缀 `::ffff:`。
pub fn get_client_ip(
    x_forwarded_for: Option<&str>,
    remote_addr: Option<&str>,
) -> Option<String> {
    if let Some(forwarded) = x_forwarded_for {
        let ip = forwarded.split(',').next().unwrap_or("").trim();
        if !ip.is_empty() {
            return Some(strip_ipv4_mapped(ip));
        }
    }
    if let Some(addr) = remote_addr {
        if !addr.is_empty() {
            return Some(strip_ipv4_mapped(addr));
        }
    }
    None
}

/// 去除 `::ffff:` IPv4-mapped 前缀。
pub fn strip_ipv4_mapped(ip: &str) -> String {
    if let Some(stripped) = ip.strip_prefix("::ffff:") {
        stripped.to_string()
    } else {
        ip.to_string()
    }
}

/// 判断请求是否安全 (HTTPS)。
///
/// 移植自 `isSecureRequest` (ui-auth.js:212-222)。
pub fn is_secure_request(x_forwarded_proto: Option<&str>) -> bool {
    if let Some(proto) = x_forwarded_proto {
        let first = proto.split(',').next().unwrap_or("").trim().to_lowercase();
        return first == "https";
    }
    false
}

/// 获取限速 key (移植自 `getRateLimitKey`, ui-auth.js:45-49)。
pub fn get_rate_limit_key(client_ip: Option<&str>) -> String {
    match client_ip {
        Some(ip) if !ip.is_empty() => ip.to_string(),
        _ => "rate-limit:no-ip".to_string(),
    }
}

// ─── 辅助 ───────────────────────────────────────────

/// percent-decode (对应 `decodeURIComponent`)。
fn percent_decode(input: &str) -> String {
    let mut result = Vec::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                result.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        result.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&result).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// 将 epoch ms 格式化为 HTTP 日期 (RFC 1123, GMT)。
fn format_http_date(timestamp_ms: i64) -> String {
    use chrono::{DateTime, Utc};
    let dt: DateTime<Utc> = DateTime::<Utc>::from_timestamp_millis(timestamp_ms)
        .unwrap_or_else(Utc::now);
    dt.to_rfc2822()
        .replace("+0000", "GMT")
        .trim_end_matches(" GMT")
        .to_string()
        + " GMT"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_cookies_simple() {
        let cookies = parse_cookies(Some("oc_ui_session=abc123; other=val"));
        assert_eq!(cookies.get("oc_ui_session"), Some(&"abc123".to_string()));
        assert_eq!(cookies.get("other"), Some(&"val".to_string()));
    }

    #[test]
    fn test_parse_cookies_empty() {
        assert!(parse_cookies(None).is_empty());
        assert!(parse_cookies(Some("")).is_empty());
    }

    #[test]
    fn test_parse_cookies_with_spaces() {
        let cookies = parse_cookies(Some("  oc_ui_session  =  abc  ;  x  =  y  "));
        assert_eq!(cookies.get("oc_ui_session"), Some(&"abc".to_string()));
        assert_eq!(cookies.get("x"), Some(&"y".to_string()));
    }

    #[test]
    fn test_parse_cookies_percent_encoded() {
        let cookies = parse_cookies(Some("oc_ui_session=abc%20def%2B123"));
        assert_eq!(cookies.get("oc_ui_session"), Some(&"abc def+123".to_string()));
    }

    #[test]
    fn test_parse_cookies_value_with_equals() {
        let cookies = parse_cookies(Some("data=key=value"));
        assert_eq!(cookies.get("data"), Some(&"key=value".to_string()));
    }

    #[test]
    fn test_build_cookie_basic() {
        let cookie = build_cookie("oc_ui_session", "token123", 43200, false);
        assert!(cookie.starts_with("oc_ui_session=token123; Path=/; HttpOnly; SameSite=Strict; Max-Age=43200; Expires="));
        assert!(!cookie.contains("Secure"));
    }

    #[test]
    fn test_build_cookie_secure() {
        let cookie = build_cookie("oc_ui_session", "token123", 43200, true);
        assert!(cookie.ends_with("; Secure"));
    }

    #[test]
    fn test_build_cookie_clear() {
        let cookie = build_cookie("oc_ui_session", "", 0, false);
        assert!(cookie.contains("Max-Age=0"));
        assert!(cookie.contains("Expires=Thu, 01 Jan 1970 00:00:00 GMT"));
    }

    #[test]
    fn test_get_bearer_token() {
        assert_eq!(
            get_bearer_token(Some("Bearer oc_client_abc123")),
            Some("oc_client_abc123".to_string())
        );
        assert_eq!(
            get_bearer_token(Some("bearer oc_client_abc123")),
            Some("oc_client_abc123".to_string())
        );
        assert_eq!(get_bearer_token(Some("Basic abc")), None);
        assert_eq!(get_bearer_token(Some("Bearer ")), None);
        assert_eq!(get_bearer_token(None), None);
    }

    #[test]
    fn test_get_url_auth_token_from_query() {
        assert_eq!(
            get_url_auth_token_from_query(Some("oc_url_token=abc123&other=val")),
            Some("abc123".to_string())
        );
        assert_eq!(
            get_url_auth_token_from_query(Some("other=val&oc_url_token=xyz")),
            Some("xyz".to_string())
        );
        assert_eq!(get_url_auth_token_from_query(Some("other=val")), None);
        assert_eq!(get_url_auth_token_from_query(None), None);
    }

    #[test]
    fn test_normalize_password() {
        assert_eq!(normalize_password("  hello  "), "hello");
        assert_eq!(normalize_password("hello"), "hello");
        assert_eq!(normalize_password(""), "");
    }

    #[test]
    fn test_is_trusted_device_request() {
        assert!(is_trusted_device_request(&serde_json::json!(true)));
        assert!(!is_trusted_device_request(&serde_json::json!(false)));
        assert!(!is_trusted_device_request(&serde_json::json!("true")));
        assert!(!is_trusted_device_request(&serde_json::json!(1)));
    }

    #[test]
    fn test_is_url_auth_readable_http_path() {
        assert!(is_url_auth_readable_http_path("/api/event"));
        assert!(is_url_auth_readable_http_path("/api/global/event"));
        assert!(is_url_auth_readable_http_path("/api/openchamber/events"));
        assert!(is_url_auth_readable_http_path("/api/notifications/stream"));
        assert!(is_url_auth_readable_http_path("/api/fs/raw"));
        assert!(is_url_auth_readable_http_path("/api/fs/serve"));
        assert!(is_url_auth_readable_http_path("/api/fs/serve/file.txt"));
        assert!(is_url_auth_readable_http_path("/api/preview/proxy/abc"));
        assert!(is_url_auth_readable_http_path("/api/terminal/sess123/stream"));
        assert!(is_url_auth_readable_http_path("/api/projects/proj1/icon"));

        assert!(!is_url_auth_readable_http_path("/api/other"));
        assert!(!is_url_auth_readable_http_path("/api/terminal/sess123/messages"));
        assert!(!is_url_auth_readable_http_path("/api/projects/proj1/other"));
    }

    #[test]
    fn test_is_url_auth_websocket_path() {
        assert!(is_url_auth_websocket_path("/api/event/ws"));
        assert!(is_url_auth_websocket_path("/api/global/event/ws"));
        assert!(is_url_auth_websocket_path("/api/openchamber/realtime-proxy/ws"));
        assert!(is_url_auth_websocket_path("/api/terminal/ws"));
        assert!(is_url_auth_websocket_path("/api/dictation/ws"));
        assert!(is_url_auth_websocket_path("/api/preview/proxy/abc"));

        assert!(!is_url_auth_websocket_path("/api/event"));
    }

    #[test]
    fn test_can_use_url_auth_token() {
        assert!(can_use_url_auth_token_for_request("GET", "/api/event", false));
        assert!(can_use_url_auth_token_for_request("get", "/api/fs/raw", false));
        assert!(can_use_url_auth_token_for_request("GET", "/api/event/ws", true));
        assert!(!can_use_url_auth_token_for_request("POST", "/api/event", false));
        assert!(!can_use_url_auth_token_for_request("GET", "/api/other", false));
    }

    #[test]
    fn test_get_client_ip_forwarded() {
        assert_eq!(
            get_client_ip(Some("1.2.3.4, 5.6.7.8"), None),
            Some("1.2.3.4".to_string())
        );
        assert_eq!(
            get_client_ip(Some("::ffff:1.2.3.4"), None),
            Some("1.2.3.4".to_string())
        );
        assert_eq!(
            get_client_ip(None, Some("192.168.1.1")),
            Some("192.168.1.1".to_string())
        );
        assert_eq!(get_client_ip(None, None), None);
    }

    #[test]
    fn test_strip_ipv4_mapped() {
        assert_eq!(strip_ipv4_mapped("::ffff:1.2.3.4"), "1.2.3.4");
        assert_eq!(strip_ipv4_mapped("1.2.3.4"), "1.2.3.4");
    }

    #[test]
    fn test_is_secure_request() {
        assert!(is_secure_request(Some("https")));
        assert!(is_secure_request(Some("https, http")));
        assert!(!is_secure_request(Some("http")));
        assert!(!is_secure_request(None));
    }

    #[test]
    fn test_get_rate_limit_key() {
        assert_eq!(get_rate_limit_key(Some("1.2.3.4")), "1.2.3.4");
        assert_eq!(get_rate_limit_key(None), "rate-limit:no-ip");
        assert_eq!(get_rate_limit_key(Some("")), "rate-limit:no-ip");
    }
}
