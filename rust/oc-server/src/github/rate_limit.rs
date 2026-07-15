//! GitHub rate-limit cooldown gate。
//!
//! 移植自 `packages/web/server/lib/github/rate-limit.js` (66 行)。
//!
//! Octokit 未配置 throttling plugin, 所以 primary/secondary rate limit
//! 以抛出的 403/429 形式出现。解析 PR status 会扇出几十个调用; 一旦
//! GitHub 开始限流, 每个后续调用都浪费一个 RTT。检测到限流响应时
//! 记录 cooldown, 跳过 GitHub 工作直到 cooldown 过期。

use std::sync::Mutex;
use std::time::{Duration, Instant};

use reqwest::header::HeaderMap;

use crate::github::client::GitHubApiError;

const MAX_COOLDOWN_MS: u64 = 15 * 60 * 1000;
const DEFAULT_COOLDOWN_MS: u64 = 60 * 1000;

/// 进程全局 rate-limit 状态。
pub struct RateLimitState {
    rate_limited_until: Mutex<Option<Instant>>,
}

impl Default for RateLimitState {
    fn default() -> Self {
        Self::new()
    }
}

impl RateLimitState {
    pub fn new() -> Self {
        Self {
            rate_limited_until: Mutex::new(None),
        }
    }

    /// 当前是否处于 rate-limit cooldown 中。
    pub fn is_rate_limited(&self) -> bool {
        let guard = self.rate_limited_until.lock().unwrap();
        match *guard {
            Some(until) => Instant::now() < until,
            None => false,
        }
    }

    /// 如果 error 是 rate-limit 错误, 记录 cooldown。返回是否是 rate-limit。
    pub fn note_if_rate_limit(&self, error: &GitHubApiError) -> bool {
        if !is_rate_limit_error(error) {
            return false;
        }
        self.note_rate_limit(error);
        true
    }

    /// 记录 cooldown (不检查是否是 rate-limit 错误)。
    fn note_rate_limit(&self, error: &GitHubApiError) {
        let retry_ms = parse_retry_after_ms(error)
            .unwrap_or(DEFAULT_COOLDOWN_MS)
            .min(MAX_COOLDOWN_MS);
        let until = Instant::now() + Duration::from_millis(retry_ms);

        let mut guard = self.rate_limited_until.lock().unwrap();
        let should_update = match *guard {
            Some(current) => until > current,
            None => true,
        };
        if should_update {
            *guard = Some(until);
            tracing::warn!(
                "[github] rate limited — pausing GitHub PR status calls for ~{}s",
                (retry_ms / 1000).max(1)
            );
        }
    }
}

/// 判断 GitHub API error 是否为 rate-limit。
///
/// 对应 Node `isGitHubRateLimitError`:
/// - 429 → true
/// - 403 + `x-ratelimit-remaining: 0` → true
/// - 403 + 有 `retry-after` header → true
/// - 403 + message 包含 "rate limit" → true
pub fn is_rate_limit_error(error: &GitHubApiError) -> bool {
    let status = error.status;
    if status == 429 {
        return true;
    }
    if status != 403 {
        return false;
    }
    let remaining = header_value(&error.headers, "x-ratelimit-remaining");
    if remaining.as_deref() == Some("0") {
        return true;
    }
    if header_value(&error.headers, "retry-after").is_some() {
        return true;
    }
    error.message.to_lowercase().contains("rate limit")
}

/// 从 error headers 解析 retry-after / x-ratelimit-reset, 返回毫秒。
fn parse_retry_after_ms(error: &GitHubApiError) -> Option<u64> {
    if let Some(retry_after) = header_value(&error.headers, "retry-after") {
        if let Ok(secs) = retry_after.parse::<f64>() {
            if secs > 0.0 {
                return Some((secs * 1000.0) as u64);
            }
        }
    }
    if let Some(reset) = header_value(&error.headers, "x-ratelimit-reset") {
        if let Ok(reset_secs) = reset.parse::<f64>() {
            let now_secs = chrono::Utc::now().timestamp_millis() as f64 / 1000.0;
            let delta = (reset_secs * 1000.0) - (now_secs * 1000.0);
            if delta > 0.0 {
                return Some(delta as u64);
            }
        }
    }
    None
}

/// 从 HeaderMap 读取 header 值 (大小写不敏感)。
fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::client::GitHubApiError;
    use reqwest::header::HeaderMap;

    fn make_error(status: u16, message: &str, headers: HeaderMap) -> GitHubApiError {
        GitHubApiError {
            status,
            message: message.to_string(),
            headers,
            body: serde_json::Value::Null,
        }
    }

    #[test]
    fn is_rate_limit_429() {
        let err = make_error(429, "Too many requests", HeaderMap::new());
        assert!(is_rate_limit_error(&err));
    }

    #[test]
    fn is_rate_limit_403_remaining_zero() {
        let mut headers = HeaderMap::new();
        headers.insert("x-ratelimit-remaining", "0".parse().unwrap());
        let err = make_error(403, "Forbidden", headers);
        assert!(is_rate_limit_error(&err));
    }

    #[test]
    fn is_rate_limit_403_retry_after() {
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", "60".parse().unwrap());
        let err = make_error(403, "Forbidden", headers);
        assert!(is_rate_limit_error(&err));
    }

    #[test]
    fn is_rate_limit_403_message() {
        let err = make_error(403, "API rate limit exceeded", HeaderMap::new());
        assert!(is_rate_limit_error(&err));
    }

    #[test]
    fn is_rate_limit_403_other() {
        let mut headers = HeaderMap::new();
        headers.insert("x-ratelimit-remaining", "100".parse().unwrap());
        let err = make_error(403, "Forbidden", headers);
        assert!(!is_rate_limit_error(&err));
    }

    #[test]
    fn is_rate_limit_404() {
        let err = make_error(404, "Not Found", HeaderMap::new());
        assert!(!is_rate_limit_error(&err));
    }

    #[test]
    fn rate_limit_state_not_limited_by_default() {
        let state = RateLimitState::new();
        assert!(!state.is_rate_limited());
    }

    #[test]
    fn rate_limit_state_note_and_expire() {
        let state = RateLimitState::new();
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", "0".parse().unwrap()); // 0 → DEFAULT_COOLDOWN_MS
        let err = make_error(429, "rate limited", headers);
        assert!(state.note_if_rate_limit(&err));
        assert!(state.is_rate_limited());
    }

    #[test]
    fn rate_limit_state_note_not_rate_limit() {
        let state = RateLimitState::new();
        let err = make_error(404, "Not Found", HeaderMap::new());
        assert!(!state.note_if_rate_limit(&err));
        assert!(!state.is_rate_limited());
    }

    #[test]
    fn parse_retry_after_from_header() {
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", "120".parse().unwrap());
        let err = make_error(429, "rate limited", headers);
        let ms = parse_retry_after_ms(&err);
        assert_eq!(ms, Some(120_000));
    }
}
