//! Preview 目标存储 + TTL 清理 + 请求解析。
//!
//! 对应 `createPreviewProxyRuntime` 内部状态 (proxy-runtime.js:1186-1275):
//! target registry + TTL sweeper + `createTarget` / `resolveTargetFromRequest`
//! / `stripProxyPrefix` / `removeRawQueryParam`。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rand::Rng;
use tokio::sync::Mutex;
use url::Url;

use super::cookies::parse_cookie_header;
use super::{
    extract_target_id, strip_proxy_prefix, CLIENT_TOKEN_QUERY_PARAM, DEFAULT_TARGET_TTL_MS,
    MIN_TARGET_TTL_MS, PREVIEW_RELOAD_PARAM, SWEEP_INTERVAL_MS, TOKEN_COOKIE_NAME,
    TOKEN_QUERY_PARAM, URL_AUTH_TOKEN_QUERY_PARAM,
};

/// 一个 preview 代理目标。
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct PreviewTarget {
    pub id: String,
    pub origin: String,
    pub token: String,
    pub created_at: Instant,
    pub expires_at: Instant,
}

/// 目标存储 + TTL 清理。
pub struct PreviewTargetStore {
    targets: Mutex<HashMap<String, PreviewTarget>>,
}

impl PreviewTargetStore {
    pub fn new() -> Self {
        Self {
            targets: Mutex::new(HashMap::new()),
        }
    }

    /// 创建新目标, 返回 (id, token, expires_at_ms_since_epoch)。
    ///
    /// 对应 `createTarget` (proxy-runtime.js:1215-1228)。
    pub async fn create_target(
        &self,
        origin: String,
        ttl_ms: u64,
    ) -> (String, String, u64) {
        let id = random_hex(16);
        let token = random_hex(16);
        let now = Instant::now();
        let effective_ttl = if ttl_ms > 0 {
            ttl_ms.max(MIN_TARGET_TTL_MS)
        } else {
            DEFAULT_TARGET_TTL_MS
        };
        let expires_at = now + Duration::from_millis(effective_ttl);
        let target = PreviewTarget {
            id: id.clone(),
            origin,
            token: token.clone(),
            created_at: now,
            expires_at,
        };
        let expires_ms = effective_ttl;
        self.targets.lock().await.insert(id.clone(), target);
        (id, token, expires_ms)
    }

    /// 解析请求 → target (校验 id + 过期 + token)。
    ///
    /// 对应 `resolveTargetFromRequest` (proxy-runtime.js:1230-1254)。
    /// `path` = 请求路径, `query` = query string (不含 ?), `cookie_header` = Cookie header 值。
    pub async fn resolve_target_from_request(
        &self,
        path: &str,
        query: Option<&str>,
        cookie_header: Option<&str>,
    ) -> Result<ResolvedTarget, ResolveError> {
        let id = extract_target_id(path).ok_or(ResolveError {
            status: 404,
            error: "Preview target not found".to_string(),
        })?;

        let mut targets = self.targets.lock().await;
        let now = Instant::now();
        let target = targets.get(id).cloned();
        let target = match target {
            Some(t) if t.expires_at > now => t,
            _ => {
                targets.remove(id);
                return Err(ResolveError {
                    status: 404,
                    error: "Preview target expired".to_string(),
                });
            }
        };
        drop(targets);

        // token 校验: query param > cookie
        let query_token = query
            .and_then(|q| parse_query_param(q, TOKEN_QUERY_PARAM))
            .unwrap_or_default();
        let cookies = parse_cookie_header(cookie_header);
        let cookie_token = cookies
            .get(TOKEN_COOKIE_NAME)
            .cloned()
            .unwrap_or_default();
        let token = if !query_token.is_empty() {
            query_token
        } else {
            cookie_token
        };
        if token.is_empty() || token != target.token {
            return Err(ResolveError {
                status: 403,
                error: "Preview token missing".to_string(),
            });
        }

        // url_auth_token (供 body 重写追加)
        let url_auth_token = query
            .and_then(|q| parse_query_param(q, URL_AUTH_TOKEN_QUERY_PARAM))
            .unwrap_or_default();

        let stripped_path = strip_proxy_prefix(path, &target.id);
        Ok(ResolvedTarget {
            target,
            stripped_path,
            url_auth_token,
        })
    }

    /// 启动 TTL 清理后台 task。
    ///
    /// 对应 `ensureSweeper` (proxy-runtime.js:1206-1213)。
    pub fn start_sweeper(self: &Arc<Self>) {
        let store = self.clone();
        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(Duration::from_millis(SWEEP_INTERVAL_MS));
            loop {
                interval.tick().await;
                let now = Instant::now();
                let mut targets = store.targets.lock().await;
                targets.retain(|_, t| t.expires_at > now);
            }
        });
    }

    /// 清空所有目标 (用于测试)。
    #[allow(dead_code)]
    pub async fn clear(&self) {
        self.targets.lock().await.clear();
    }

    /// 当前目标数 (用于测试)。
    #[allow(dead_code)]
    pub async fn len(&self) -> usize {
        self.targets.lock().await.len()
    }
}

/// 解析成功的结果。
#[derive(Debug)]
pub struct ResolvedTarget {
    pub target: PreviewTarget,
    pub stripped_path: String,
    pub url_auth_token: String,
}

/// 解析失败。
#[derive(Debug)]
pub struct ResolveError {
    pub status: u16,
    pub error: String,
}

/// 从 query string 解析单个参数值。
fn parse_query_param(query: &str, name: &str) -> Option<String> {
    let q = query.strip_prefix('?').unwrap_or(query);
    for pair in q.split('&') {
        let mut parts = pair.splitn(2, '=');
        let key = parts.next().unwrap_or("");
        let decoded = url_decode(key);
        if decoded == name {
            return parts.next().map(url_decode);
        }
    }
    None
}

/// 简单 URL 解码 (对齐 `decodeURIComponent`)。
fn url_decode(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'+' {
            result.push(' ');
            i += 1;
        } else if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(
                std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""),
                16,
            ) {
                result.push(b as char);
                i += 3;
                continue;
            }
            result.push(bytes[i] as char);
            i += 1;
        } else {
            result.push(bytes[i] as char);
            i += 1;
        }
    }
    result
}

/// 生成 n 字节的十六进制字符串 (对应 `crypto.randomBytes(n).toString('hex')`)。
fn random_hex(n_bytes: usize) -> String {
    let mut rng = rand::thread_rng();
    (0..n_bytes)
        .map(|_| format!("{:02x}", rng.gen::<u8>()))
        .collect()
}

/// 从 raw query string 移除指定参数。
///
/// 对应 `removeRawQueryParam` (proxy-runtime.js:1265-1275)。
pub fn remove_raw_query_param(search: &str, param_name: &str) -> String {
    if search.len() <= 1 {
        return String::new();
    }
    let query = search.strip_prefix('?').unwrap_or(search);
    let parts: Vec<&str> = query
        .split('&')
        .filter(|part| {
            let name = part.split('=').next().unwrap_or("");
            url_decode(name) != param_name
        })
        .collect();
    if parts.is_empty() {
        String::new()
    } else {
        format!("?{}", parts.join("&"))
    }
}

/// 构造上游 URL: `{target.origin}{path}{stripped_query}`。
///
/// `query` 是原始 query string (可能含需要 strip 的 token 参数)。
pub fn build_upstream_url(origin: &str, path: &str, query: Option<&str>) -> String {
    let mut search = query.unwrap_or("").to_string();
    // strip 所有 OpenChamber 内部参数
    search = remove_raw_query_param(&search, PREVIEW_RELOAD_PARAM);
    search = remove_raw_query_param(&search, TOKEN_QUERY_PARAM);
    search = remove_raw_query_param(&search, CLIENT_TOKEN_QUERY_PARAM);
    search = remove_raw_query_param(&search, URL_AUTH_TOKEN_QUERY_PARAM);
    format!("{}{}{}", origin, path, search)
}

/// 将 HTTP origin 转为 WS origin (`http` → `ws`, `https` → `wss`)。
pub fn http_origin_to_ws(origin: &str) -> String {
    if let Ok(url) = Url::parse(origin) {
        let ws_scheme = match url.scheme() {
            "https" => "wss",
            _ => "ws",
        };
        let host = url.host_str().unwrap_or("");
        let port = url.port().map(|p| format!(":{}", p)).unwrap_or_default();
        let host_bracketed = if host.contains(':') {
            format!("[{}]", host)
        } else {
            host.to_string()
        };
        format!("{}://{}{}", ws_scheme, host_bracketed, port)
    } else {
        origin.replacen("http://", "ws://", 1).replacen("https://", "wss://", 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remove_raw_query_param_removes_target() {
        assert_eq!(
            remove_raw_query_param("?oc_preview_token=abc&keep=1", "oc_preview_token"),
            "?keep=1"
        );
    }

    #[test]
    fn remove_raw_query_param_empty_when_all_removed() {
        assert_eq!(
            remove_raw_query_param("?oc_preview_token=abc", "oc_preview_token"),
            ""
        );
    }

    #[test]
    fn remove_raw_query_param_handles_empty() {
        assert_eq!(remove_raw_query_param("", "x"), "");
        assert_eq!(remove_raw_query_param("?", "x"), "");
    }

    #[test]
    fn build_upstream_url_strips_tokens() {
        let url = build_upstream_url(
            "http://127.0.0.1:3000",
            "/app",
            Some("foo=bar&oc_preview_token=secret&oc_url_token=t&ocPreview=1&oc_client_token=c"),
        );
        assert_eq!(url, "http://127.0.0.1:3000/app?foo=bar");
    }

    #[test]
    fn build_upstream_url_no_query() {
        let url = build_upstream_url("http://127.0.0.1:3000", "/app", None);
        assert_eq!(url, "http://127.0.0.1:3000/app");
    }

    #[test]
    fn http_origin_to_ws_converts() {
        assert_eq!(
            http_origin_to_ws("http://127.0.0.1:3000"),
            "ws://127.0.0.1:3000"
        );
        assert_eq!(
            http_origin_to_ws("https://example.com"),
            "wss://example.com"
        );
    }

    #[tokio::test]
    async fn create_and_resolve_target() {
        let store = Arc::new(PreviewTargetStore::new());
        let (id, token, _) = store
            .create_target("http://127.0.0.1:3000".to_string(), DEFAULT_TARGET_TTL_MS)
            .await;
        let path = format!("/api/preview/proxy/{}/app", id);
        let resolved = store
            .resolve_target_from_request(&path, Some(&format!("oc_preview_token={}", token)), None)
            .await
            .expect("resolve should succeed");
        assert_eq!(resolved.target.origin, "http://127.0.0.1:3000");
        assert_eq!(resolved.stripped_path, "/app");
    }

    #[tokio::test]
    async fn resolve_rejects_wrong_token() {
        let store = Arc::new(PreviewTargetStore::new());
        let (id, _, _) = store
            .create_target("http://127.0.0.1:3000".to_string(), DEFAULT_TARGET_TTL_MS)
            .await;
        let path = format!("/api/preview/proxy/{}/app", id);
        let err = store
            .resolve_target_from_request(&path, Some("oc_preview_token=wrong"), None)
            .await
            .expect_err("should reject wrong token");
        assert_eq!(err.status, 403);
    }

    #[tokio::test]
    async fn resolve_rejects_unknown_id() {
        let store = Arc::new(PreviewTargetStore::new());
        let err = store
            .resolve_target_from_request(
                "/api/preview/proxy/nonexistentid123456/app",
                Some("oc_preview_token=x"),
                None,
            )
            .await
            .expect_err("should reject unknown id");
        assert_eq!(err.status, 404);
    }

    #[tokio::test]
    async fn resolve_via_cookie() {
        let store = Arc::new(PreviewTargetStore::new());
        let (id, token, _) = store
            .create_target("http://127.0.0.1:3000".to_string(), DEFAULT_TARGET_TTL_MS)
            .await;
        let path = format!("/api/preview/proxy/{}/", id);
        let cookie = format!("oc_preview_token={}", token);
        let resolved = store
            .resolve_target_from_request(&path, None, Some(&cookie))
            .await
            .expect("resolve via cookie should succeed");
        assert_eq!(resolved.stripped_path, "/");
    }

    #[test]
    fn random_hex_is_correct_length() {
        let hex = random_hex(16);
        assert_eq!(hex.len(), 32);
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
