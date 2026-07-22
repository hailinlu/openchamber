//! Custom OpenAI base URL 校验 + 规范化。
//!
//! 对应 Node `tts/base-url.js` (`normalizeCustomOpenAIBaseURL`, 65 行):
//!   - empty → `Ok(None)`
//!   - invalid URL → `Err("Custom server URL is invalid")`
//!   - non-http(s) protocol → `Err("Custom server URL must use http or https")`
//!   - has userinfo (user/pass) → `Err("Custom server URL must not include credentials")`
//!   - 非本地 host + 未开启远程白名单 → 拒绝
//!   - 其它 → strip hash/search, strip trailing `/`, 返回 `{protocol}//{host}{path}`
//!
//! Rust API 形状与 Node 不同: 用 `Result<Option<String>, String>` 而非
//! `{ value } | { error }`, 调用方通过 `is_err()` / `ok().flatten()` 处理。

/// 本地 base URL host 白名单 (对齐 Node `LOCAL_BASE_URL_HOSTS`)。
const LOCAL_BASE_URL_HOSTS: &[&str] = &[
    "localhost",
    "127.0.0.1",
    "::1",
    "host.docker.internal",
];

/// 规范化自定义 OpenAI base URL。
///
/// 输入为空 → `Ok(None)`。
/// 输入合法 → `Ok(Some("https://host/path"))`。
/// 输入非法 → `Err(msg)` (msg 与 Node 错误串保持一致, 供 API 返回)。
pub fn normalize_custom_openai_base_url(value: &str) -> Result<Option<String>, String> {
    if value.trim().is_empty() {
        return Ok(None);
    }

    let parsed = url::Url::parse(value.trim()).map_err(|_| "Custom server URL is invalid".to_string())?;

    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err("Custom server URL must use http or https".to_string());
    }

    // url crate 把 username/password 暴露在 `.username()` / `.password_some()`
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("Custom server URL must not include credentials".to_string());
    }

    let host = parsed.host_str().unwrap_or("");
    let allow_remote = is_remote_allowed();
    if !allow_remote && !is_allowed_local_host(host) {
        return Err(
            "Remote custom server URLs are disabled. Set GRIDFORGE_ALLOW_REMOTE_OPENAI_COMPAT_URLS=true to allow this host."
                .to_string(),
        );
    }

    // 重建: 保留 protocol + host + 规范化后的 path (去尾部斜杠), 丢弃 hash/search
    let mut clean = parsed.clone();
    clean.set_fragment(None);
    clean.set_query(None);

    let scheme = clean.scheme();
    let host_part = clean.host_str().unwrap_or("");
    // url crate 的 port 处理: explicit non-default port 需要带 :port, default port 隐藏
    let port_part = match clean.port() {
        Some(p) => format!(":{}", p),
        None => String::new(),
    };

    let path = clean.path().trim_end_matches('/').to_string();

    Ok(Some(format!("{}://{}{}{}", scheme, host_part, port_part, path)))
}

/// 本地 host 校验 (大小写不敏感, 支持 IPv6 `[::1]` 包装)。
fn is_allowed_local_host(hostname: &str) -> bool {
    let normalized = normalize_hostname(hostname);
    LOCAL_BASE_URL_HOSTS.iter().any(|h| *h == normalized)
}

fn normalize_hostname(hostname: &str) -> String {
    let trimmed = hostname.trim().to_lowercase();
    if trimmed.starts_with('[') && trimmed.ends_with(']') {
        trimmed[1..trimmed.len() - 1].to_string()
    } else {
        trimmed
    }
}

/// `GRIDFORGE_RUNTIME=desktop` 或 `GRIDFORGE_ALLOW_REMOTE_OPENAI_COMPAT_URLS=1|true` 时允许远程 URL。
fn is_remote_allowed() -> bool {
    let runtime = std::env::var("GRIDFORGE_RUNTIME")
        .ok()
        .map(|v| v.trim().to_lowercase())
        .unwrap_or_default();
    if runtime == "desktop" {
        return true;
    }
    is_env_flag_enabled(
        &std::env::var("GRIDFORGE_ALLOW_REMOTE_OPENAI_COMPAT_URLS").unwrap_or_default(),
    )
}

fn is_env_flag_enabled(value: &str) -> bool {
    let normalized = value.trim().to_lowercase();
    normalized == "1" || normalized == "true"
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::Mutex;

    /// 串行化所有 base_url 测试 (它们修改同一份环境变量)。
    pub(crate) static TEST_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard {
        prev_runtime: Option<String>,
        prev_allow_remote: Option<String>,
        // 持有共享锁直到 EnvGuard drop, 保证测试体在锁内执行。
        _lock: std::sync::MutexGuard<'static, ()>,
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.prev_runtime {
                Some(v) => std::env::set_var("GRIDFORGE_RUNTIME", v),
                None => std::env::remove_var("GRIDFORGE_RUNTIME"),
            }
            match &self.prev_allow_remote {
                Some(v) => std::env::set_var("GRIDFORGE_ALLOW_REMOTE_OPENAI_COMPAT_URLS", v),
                None => std::env::remove_var("GRIDFORGE_ALLOW_REMOTE_OPENAI_COMPAT_URLS"),
            }
        }
    }

    /// 把两个 env var 重置为非 desktop / non-allow 状态, 返回 guard 在 drop 时还原。
    ///
    /// guard 持有共享 `TEST_LOCK` 直到调用方 drop 它, 保证整个测试体
    /// (含 set_var + 断言) 在锁内执行, 避免并行竞争。
    fn lock_env_block_remote() -> EnvGuard {
        let lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev_runtime = std::env::var("GRIDFORGE_RUNTIME").ok();
        let prev_allow_remote = std::env::var("GRIDFORGE_ALLOW_REMOTE_OPENAI_COMPAT_URLS").ok();
        std::env::remove_var("GRIDFORGE_RUNTIME");
        std::env::remove_var("GRIDFORGE_ALLOW_REMOTE_OPENAI_COMPAT_URLS");
        EnvGuard {
            prev_runtime,
            prev_allow_remote,
            _lock: lock,
        }
    }

    #[test]
    fn empty_input_returns_none() {
        let _g = lock_env_block_remote();
        assert_eq!(normalize_custom_openai_base_url("").unwrap(), None);
        assert_eq!(normalize_custom_openai_base_url("   ").unwrap(), None);
    }

    #[test]
    fn invalid_url_returns_error() {
        let _g = lock_env_block_remote();
        let err = normalize_custom_openai_base_url("not-a-url").unwrap_err();
        assert_eq!(err, "Custom server URL is invalid");
    }

    #[test]
    fn non_http_scheme_rejected() {
        let _g = lock_env_block_remote();
        let err = normalize_custom_openai_base_url("ftp://example.com/v1").unwrap_err();
        assert_eq!(err, "Custom server URL must use http or https");
    }

    #[test]
    fn credentials_in_url_rejected() {
        let _g = lock_env_block_remote();
        let err = normalize_custom_openai_base_url("https://user:pass@example.com/v1").unwrap_err();
        assert_eq!(err, "Custom server URL must not include credentials");
    }

    #[test]
    fn localhost_allowed() {
        let _g = lock_env_block_remote();
        let result = normalize_custom_openai_base_url("http://localhost:11434/v1").unwrap();
        assert_eq!(result, Some("http://localhost:11434/v1".to_string()));
    }

    #[test]
    fn loopback_ip_allowed() {
        let _g = lock_env_block_remote();
        let result = normalize_custom_openai_base_url("http://127.0.0.1:8080/v1/").unwrap();
        // trailing slash 应被 strip
        assert_eq!(result, Some("http://127.0.0.1:8080/v1".to_string()));
    }

    #[test]
    fn docker_internal_host_allowed() {
        let _g = lock_env_block_remote();
        let result = normalize_custom_openai_base_url("http://host.docker.internal:8080").unwrap();
        assert_eq!(result, Some("http://host.docker.internal:8080".to_string()));
    }

    #[test]
    fn remote_host_blocked_by_default() {
        let _g = lock_env_block_remote();
        let err = normalize_custom_openai_base_url("https://api.example.com/v1").unwrap_err();
        assert!(
            err.contains("Remote custom server URLs are disabled"),
            "got: {}",
            err
        );
    }

    #[test]
    fn remote_host_allowed_when_env_flag_set() {
        let _g = lock_env_block_remote();
        std::env::set_var("GRIDFORGE_ALLOW_REMOTE_OPENAI_COMPAT_URLS", "true");
        let result = normalize_custom_openai_base_url("https://api.example.com/v1").unwrap();
        assert_eq!(result, Some("https://api.example.com/v1".to_string()));
    }

    #[test]
    fn remote_host_allowed_when_runtime_desktop() {
        let _g = lock_env_block_remote();
        std::env::set_var("GRIDFORGE_RUNTIME", "desktop");
        let result = normalize_custom_openai_base_url("https://api.example.com/v1").unwrap();
        assert_eq!(result, Some("https://api.example.com/v1".to_string()));
    }

    #[test]
    fn trailing_slash_stripped() {
        let _g = lock_env_block_remote();
        let result = normalize_custom_openai_base_url("http://localhost:11434/v1///").unwrap();
        assert_eq!(result, Some("http://localhost:11434/v1".to_string()));
    }

    #[test]
    fn hash_and_query_stripped() {
        let _g = lock_env_block_remote();
        let result = normalize_custom_openai_base_url("http://localhost:11434/v1?token=x#frag").unwrap();
        assert_eq!(result, Some("http://localhost:11434/v1".to_string()));
    }
}