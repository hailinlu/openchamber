//! URL 规范化 + SSRF 防护。
//!
//! 对应 `normalizeProxyTargetUrl` + `isBlockedExternalHost` (proxy-runtime.js:946-1017)。

use url::Url;

/// Loopback 主机集合 (用于 `allowExternal=false` 路径校验)。
fn is_loopback_host(hostname: &str) -> bool {
    super::LOOPBACK_HOSTS.contains(&hostname)
}

/// SSRF 防护: 拒绝代理到私有/loopback/保留地址。
///
/// 对应 `isBlockedExternalHost` (proxy-runtime.js:953-981)。
/// 操作在 WHATWG 规范化后的 hostname 上 (十进制/八进制 IPv4 已是点分十进制)。
/// 注意: 仅拦截 IP 字面量 — 解析到私有 IP 的主机名 (DNS rebinding) 不在此处理。
fn is_blocked_external_host(hostname: &str) -> bool {
    if hostname.is_empty() {
        return true;
    }
    let mut host = hostname.to_lowercase();
    if host.starts_with('[') && host.ends_with(']') {
        host = host[1..host.len() - 1].to_string();
    }

    if host == "localhost" || host.ends_with(".localhost") || host.ends_with(".local") {
        return true;
    }

    // IPv4 点分十进制
    if let Some(v4) = parse_ipv4(&host) {
        let [a, b, _c, _d] = v4;
        if a == 0 || a == 127 || a == 10 {
            return true; // this-host / loopback / private
        }
        if a == 169 && b == 254 {
            return true; // link-local incl. cloud metadata
        }
        if a == 172 && (16..=31).contains(&b) {
            return true; // private
        }
        if a == 192 && b == 168 {
            return true; // private
        }
        if a == 100 && (64..=127).contains(&b) {
            return true; // carrier-grade NAT
        }
        return false;
    }

    // IPv6
    if host.contains(':') {
        if host == "::1" || host == "::" {
            return true; // loopback / unspecified
        }
        if host.starts_with("fe80") {
            return true; // link-local
        }
        if host.starts_with("fc") || host.starts_with("fd") {
            return true; // unique local fc00::/7
        }
        if host.contains("::ffff:") {
            return true; // IPv4-mapped
        }
        return false;
    }

    false
}

/// 解析点分十进制 IPv4 → [u8; 4], 每段 0-255。
fn parse_ipv4(host: &str) -> Option<[u8; 4]> {
    let parts: Vec<&str> = host.split('.').collect();
    if parts.len() != 4 {
        return None;
    }
    let mut octets = [0u8; 4];
    for (i, part) in parts.iter().enumerate() {
        // 每段 1-3 位数字
        if part.is_empty() || part.len() > 3 {
            return None;
        }
        if !part.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        let val: u32 = part.parse().ok()?;
        if val > 255 {
            return None;
        }
        octets[i] = val as u8;
    }
    Some(octets)
}

/// 规范化后的代理目标结果。
#[derive(Debug, Clone, PartialEq)]
pub struct NormalizedTarget {
    pub origin: String,
}

/// 规范化代理目标 URL。
///
/// 对应 `normalizeProxyTargetUrl` (proxy-runtime.js:983-1017)。
/// - `allowExternal=false`: 仅允许 loopback 主机
/// - `allowExternal=true`: 允许外部主机, 但 SSRF 防护拒绝私有/保留地址
/// - loopback 主机名规范化为 `127.0.0.1` (避免 localhost→::1 但 dev server 只绑 IPv4)
/// - 仅保留 origin (scheme://host:port), 路径由代理端保留
pub fn normalize_proxy_target_url(
    raw_url: &str,
    allow_external: bool,
) -> Result<NormalizedTarget, String> {
    let url = Url::parse(raw_url).map_err(|_| "Invalid URL".to_string())?;

    let scheme = url.scheme();
    if scheme != "http" && scheme != "https" {
        return Err("Only http(s) URLs are supported".to_string());
    }

    let hostname = url.host_str().unwrap_or("");
    if !allow_external {
        if !is_loopback_host(hostname) {
            return Err("Only loopback hosts are supported".to_string());
        }
    } else if is_blocked_external_host(hostname) {
        return Err("Refusing to proxy private or reserved addresses".to_string());
    }

    // 端口校验
    let port = url.port_or_known_default();
    match port {
        Some(p) if p > 0 => {}
        _ => return Err("Invalid port".to_string()),
    }

    // 规范化 loopback 主机名为 127.0.0.1
    let final_host = if is_loopback_host(hostname)
        && (hostname == "0.0.0.0"
            || hostname == "localhost"
            || hostname == "::1"
            || hostname == "[::1]")
    {
        "127.0.0.1"
    } else {
        hostname
    };

    // 重建 origin (scheme://host:port)
    let port_str = match url.port() {
        Some(p) => format!(":{}", p),
        None => match scheme {
            "https" => String::new(),
            _ => String::new(),
        },
    };
    // 对于 IPv6 主机, url crate 的 host_str 不含方括号, 需手动加
    let host_with_brackets = if final_host.contains(':') {
        format!("[{}]", final_host)
    } else {
        final_host.to_string()
    };
    let origin = format!("{}://{}{}", scheme, host_with_brackets, port_str);

    Ok(NormalizedTarget { origin })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ============================================================
    // SSRF guard: allowExternal=true 路径
    // ============================================================

    #[test]
    fn allows_ordinary_external_host() {
        let result = normalize_proxy_target_url("https://docs.gridforge.dev/security/", true).unwrap();
        assert_eq!(result.origin, "https://docs.gridforge.dev");
    }

    #[test]
    fn rejects_non_loopback_without_allow_external() {
        assert!(normalize_proxy_target_url("https://example.com/", false).is_err());
    }

    #[test]
    fn refuses_private_loopback_link_local_literals() {
        for url in [
            "http://127.0.0.1/",
            "http://10.0.0.5/",
            "http://172.16.9.9/",
            "http://192.168.1.1/",
            "http://169.254.169.254/latest/meta-data/",
            "http://100.64.0.1/",
            "http://localhost/",
            "http://service.local/",
            "http://[::1]/",
            "http://[fd00::1]/",
            "http://[fe80::1]/",
            // 十进制 127.0.0.1 — url crate 规范化为 127.0.0.1
            "http://2130706433/",
        ] {
            assert!(
                normalize_proxy_target_url(url, true).is_err(),
                "should block: {}",
                url
            );
        }
    }

    #[test]
    fn blocks_private_via_ipv4_mapped_ipv6() {
        assert!(normalize_proxy_target_url("http://[::ffff:127.0.0.1]/", true).is_err());
    }

    // ============================================================
    // loopback 路径 (allowExternal=false)
    // ============================================================

    #[test]
    fn allows_loopback_without_allow_external() {
        let result = normalize_proxy_target_url("http://localhost:3000/", false).unwrap();
        assert_eq!(result.origin, "http://127.0.0.1:3000");
    }

    #[test]
    fn normalizes_localhost_to_127001() {
        let result = normalize_proxy_target_url("http://localhost:5173/", false).unwrap();
        assert_eq!(result.origin, "http://127.0.0.1:5173");
    }

    #[test]
    fn normalizes_0000_to_127001() {
        let result = normalize_proxy_target_url("http://0.0.0.0:8080/", false).unwrap();
        assert_eq!(result.origin, "http://127.0.0.1:8080");
    }

    #[test]
    fn rejects_invalid_scheme() {
        assert!(normalize_proxy_target_url("file:///etc/passwd", false).is_err());
        assert!(normalize_proxy_target_url("ftp://localhost/", false).is_err());
    }

    #[test]
    fn rejects_invalid_url() {
        assert!(normalize_proxy_target_url("not a url", false).is_err());
    }

    #[test]
    fn preserves_explicit_port_on_loopback() {
        let result = normalize_proxy_target_url("http://127.0.0.1:4321/app", false).unwrap();
        assert_eq!(result.origin, "http://127.0.0.1:4321");
    }

    #[test]
    fn preserves_explicit_port_on_external_host() {
        let result = normalize_proxy_target_url("https://docs.gridforge.dev:8443/path", true).unwrap();
        assert_eq!(result.origin, "https://docs.gridforge.dev:8443");
    }
}
