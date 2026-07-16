//! Cookie 解析与构造。
//!
//! 对应 `parseCookieHeader` + `buildCookie` (proxy-runtime.js:906-944)。

use std::collections::HashMap;

/// 解析 Cookie header → HashMap<name, value>。
///
/// 对应 `parseCookieHeader` (proxy-runtime.js:906-926)。
pub fn parse_cookie_header(cookie_header: Option<&str>) -> HashMap<String, String> {
    let mut result = HashMap::new();
    let header = match cookie_header {
        Some(h) if !h.is_empty() => h,
        _ => return result,
    };
    for part in header.split(';') {
        let idx = match part.find('=') {
            Some(i) if i > 0 => i,
            _ => continue,
        };
        let key = part[..idx].trim();
        let value = part[idx + 1..].trim();
        if key.is_empty() {
            continue;
        }
        result.insert(key.to_string(), value.to_string());
    }
    result
}

/// 构建 Set-Cookie header 值。
///
/// 对应 `buildCookie` (proxy-runtime.js:928-944)。
pub fn build_cookie(
    name: &str,
    value: &str,
    path: Option<&str>,
    max_age_seconds: Option<u64>,
    secure: bool,
) -> String {
    let mut chunks = vec![format!("{}={}", name, value)];
    if let Some(p) = path {
        chunks.push(format!("Path={}", p));
    }
    if let Some(secs) = max_age_seconds {
        chunks.push(format!("Max-Age={}", secs));
    }
    chunks.push("HttpOnly".to_string());
    chunks.push("SameSite=Lax".to_string());
    if secure {
        chunks.push("Secure".to_string());
    }
    chunks.join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cookie_header_basic() {
        let cookies = parse_cookie_header(Some("oc_preview_token=abc; other=def"));
        assert_eq!(cookies.get("oc_preview_token").unwrap(), "abc");
        assert_eq!(cookies.get("other").unwrap(), "def");
    }

    #[test]
    fn parse_cookie_header_empty() {
        assert!(parse_cookie_header(None).is_empty());
        assert!(parse_cookie_header(Some("")).is_empty());
    }

    #[test]
    fn parse_cookie_header_skips_malformed() {
        let cookies = parse_cookie_header(Some("valid=1; badpair; =empty; good=2"));
        assert_eq!(cookies.len(), 2);
        assert_eq!(cookies.get("valid").unwrap(), "1");
        assert_eq!(cookies.get("good").unwrap(), "2");
    }

    #[test]
    fn build_cookie_full() {
        let cookie = build_cookie("oc_preview_token", "secret", Some("/api/preview/proxy/abc"), Some(3600), true);
        assert!(cookie.contains("oc_preview_token=secret"));
        assert!(cookie.contains("Path=/api/preview/proxy/abc"));
        assert!(cookie.contains("Max-Age=3600"));
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Lax"));
        assert!(cookie.contains("Secure"));
    }

    #[test]
    fn build_cookie_minimal() {
        let cookie = build_cookie("token", "val", None, None, false);
        assert_eq!(cookie, "token=val; HttpOnly; SameSite=Lax");
    }
}
