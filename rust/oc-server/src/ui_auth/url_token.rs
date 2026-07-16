//! URL auth token store。
//!
//! 移植自 `ui-auth.js` lines 419-446。
//! 全内存, Mutex<HashMap> 保护。Token: `oc_url_` + 24 bytes base64url。TTL: 60s。

use std::collections::HashMap;
use std::sync::Mutex;

use base64::Engine;

use super::{URL_AUTH_TOKEN_PREFIX, URL_AUTH_TOKEN_TTL_MS};

/// URL auth token 记录。
struct UrlTokenEntry {
    #[allow(dead_code)]
    session_token: String,
    expires_at: i64, // epoch ms
}

/// URL auth token 存储 (全内存)。
pub struct UrlTokenStore {
    inner: Mutex<HashMap<String, UrlTokenEntry>>,
}

impl UrlTokenStore {
    pub fn new() -> Self {
        UrlTokenStore {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// 签发新 URL auth token, 返回 (token, expires_at_ms)。
    ///
    /// 移植自 `issueUrlAuthTokenForSession` (ui-auth.js:428-434)。
    pub fn issue(&self, session_token: &str) -> (String, i64) {
        self.sweep();
        let token = format!(
            "{URL_AUTH_TOKEN_PREFIX}{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random_bytes(24))
        );
        let expires_at = now_millis() + URL_AUTH_TOKEN_TTL_MS;
        let mut inner = self.inner.lock().unwrap();
        inner.insert(
            token.clone(),
            UrlTokenEntry {
                session_token: session_token.to_string(),
                expires_at,
            },
        );
        (token, expires_at)
    }

    /// 认证 URL auth token, 返回绑定的 session_token。
    ///
    /// 移植自 `authenticateUrlAuthToken` (ui-auth.js:436-446)。
    pub fn authenticate(&self, token: &str) -> Option<String> {
        if !token.starts_with(URL_AUTH_TOKEN_PREFIX) {
            return None;
        }
        let mut inner = self.inner.lock().unwrap();
        let entry = inner.get(token)?;
        if entry.expires_at <= now_millis() {
            inner.remove(token);
            return None;
        }
        Some(entry.session_token.clone())
    }

    /// 移除过期 token (移植自 `sweepUrlAuthTokens`, ui-auth.js:419-426)。
    pub fn sweep(&self) {
        let now = now_millis();
        let mut inner = self.inner.lock().unwrap();
        inner.retain(|_, entry| entry.expires_at > now);
    }

    /// 清除所有 URL auth tokens (用于 reset_auth)。
    pub fn clear(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.clear();
    }
}

impl Default for UrlTokenStore {
    fn default() -> Self {
        Self::new()
    }
}

fn now_millis() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn random_bytes(n: usize) -> Vec<u8> {
    use rand::RngCore;
    let mut bytes = vec![0u8; n];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_issue_and_authenticate() {
        let store = UrlTokenStore::new();
        let (token, expires_at) = store.issue("session-abc");
        assert!(token.starts_with("oc_url_"));
        assert!(expires_at > now_millis());
        assert_eq!(store.authenticate(&token), Some("session-abc".to_string()));
    }

    #[test]
    fn test_authenticate_wrong_prefix() {
        let store = UrlTokenStore::new();
        assert_eq!(store.authenticate("wrong_token"), None);
        assert_eq!(store.authenticate("oc_url_nonexistent"), None);
    }

    #[test]
    fn test_issue_uniqueness() {
        let store = UrlTokenStore::new();
        let (t1, _) = store.issue("a");
        let (t2, _) = store.issue("b");
        assert_ne!(t1, t2);
    }

    #[test]
    fn test_clear() {
        let store = UrlTokenStore::new();
        let (token, _) = store.issue("session");
        assert!(store.authenticate(&token).is_some());
        store.clear();
        assert!(store.authenticate(&token).is_none());
    }
}
