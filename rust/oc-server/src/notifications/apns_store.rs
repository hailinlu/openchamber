//! APNs token 持久化。
//!
//! 对应 Node `notifications/apns-runtime.js` 的 token 持久化部分。
//!
//! 文件: `$DATA_DIR/apns-tokens.json`, 结构:
//! ```json
//! { "version": 1, "tokensBySession": { "<token>": [ ... ] } }
//! ```

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{now_millis, APNS_TOKENS_VERSION, MAX_SUBS_PER_SESSION};

/// 单条 APNs token。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApnsTokenEntry {
    #[serde(rename = "deviceToken")]
    pub device_token: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_seen_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
    #[serde(default = "default_platform")]
    pub platform: String,
}

fn default_platform() -> String {
    "ios".to_string()
}

/// 文件存储格式。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ApnsStoreFile {
    version: u32,
    #[serde(rename = "tokensBySession", default)]
    tokens_by_session: HashMap<String, Vec<Value>>,
}

impl Default for ApnsStoreFile {
    fn default() -> Self {
        Self {
            version: APNS_TOKENS_VERSION,
            tokens_by_session: HashMap::new(),
        }
    }
}

/// APNs token 存储。
pub struct ApnsStore {
    path: PathBuf,
    write_lock: Mutex<()>,
}

impl ApnsStore {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            write_lock: Mutex::new(()),
        }
    }

    fn read_from_disk(&self) -> ApnsStoreFile {
        match std::fs::read_to_string(&self.path) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
            Err(_) => ApnsStoreFile::default(),
        }
    }

    fn write_to_disk(&self, data: &ApnsStoreFile) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(content) = serde_json::to_string_pretty(data) {
            let _ = std::fs::write(&self.path, content);
        }
    }

    fn persist_update<F>(&self, mutate: F)
    where
        F: FnOnce(&mut ApnsStoreFile),
    {
        let _lock = self.write_lock.lock().unwrap();
        let mut current = self.read_from_disk();
        if current.version != APNS_TOKENS_VERSION {
            current = ApnsStoreFile::default();
        }
        mutate(&mut current);
        self.write_to_disk(&current);
    }

    /// 规范化 platform: 非 'android' → 'ios'。
    fn normalize_platform(platform: Option<&str>) -> String {
        if platform == Some("android") {
            "android".to_string()
        } else {
            "ios".to_string()
        }
    }

    /// 添加或更新 APNs token。
    ///
    /// 对应 Node `addOrUpdateApnsToken`。
    pub fn add_or_update_token(
        &self,
        ui_session_token: &str,
        device_token: &str,
        user_agent: Option<&str>,
        platform: Option<&str>,
    ) {
        if ui_session_token.is_empty() || device_token.trim().is_empty() {
            return;
        }
        let token = device_token.trim().to_string();
        let token_platform = Self::normalize_platform(platform);
        let now = now_millis();
        let ua_owned = user_agent
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        let session = ui_session_token.to_string();

        self.persist_update(move |store| {
            let existing = store
                .tokens_by_session
                .entry(session.clone())
                .or_default();

            // 过滤掉同 device_token 的旧条目
            existing.retain(|e| {
                e.get("deviceToken")
                    .and_then(|v| v.as_str())
                    .map(|s| s != token)
                    .unwrap_or(true)
            });

            let new_entry = json!({
                "deviceToken": token,
                "createdAt": now,
                "lastSeenAt": now,
                "userAgent": ua_owned,
                "platform": token_platform,
            });
            existing.insert(0, new_entry);
            existing.truncate(MAX_SUBS_PER_SESSION);
        });
    }

    /// 移除指定 token。
    pub fn remove_token(&self, ui_session_token: &str, device_token: &str) {
        if ui_session_token.is_empty() || device_token.is_empty() {
            return;
        }
        let session = ui_session_token.to_string();
        let token = device_token.to_string();

        self.persist_update(move |store| {
            if let Some(tokens) = store.tokens_by_session.get(&session) {
                let filtered: Vec<Value> = tokens
                    .iter()
                    .filter(|e| {
                        e.get("deviceToken")
                            .and_then(|v| v.as_str())
                            .map(|s| s != token)
                            .unwrap_or(true)
                    })
                    .cloned()
                    .collect();
                if filtered.is_empty() {
                    store.tokens_by_session.remove(&session);
                } else {
                    store.tokens_by_session.insert(session, filtered);
                }
            }
        });
    }

    /// 从所有 session 中移除指定 device_token。
    ///
    /// 对应 Node `removeApnsTokenFromAllSessions`。
    pub fn remove_token_from_all_sessions(&self, device_token: &str) {
        if device_token.is_empty() {
            return;
        }
        let token = device_token.to_string();

        self.persist_update(move |store| {
            let sessions: Vec<String> = store.tokens_by_session.keys().cloned().collect();
            for session in sessions {
                if let Some(tokens) = store.tokens_by_session.get(&session) {
                    let filtered: Vec<Value> = tokens
                        .iter()
                        .filter(|e| {
                            e.get("deviceToken")
                                .and_then(|v| v.as_str())
                                .map(|s| s != token)
                                .unwrap_or(true)
                        })
                        .cloned()
                        .collect();
                    if filtered.is_empty() {
                        store.tokens_by_session.remove(&session);
                    } else {
                        store.tokens_by_session.insert(session, filtered);
                    }
                }
            }
        });
    }

    /// 读取所有 device tokens (去重)。
    pub fn read_all_tokens(&self) -> Vec<String> {
        let store = self.read_from_disk();
        let mut seen = std::collections::HashSet::new();
        let mut result = Vec::new();

        for tokens in store.tokens_by_session.values() {
            for entry in tokens {
                let dt = entry.get("deviceToken").and_then(|v| v.as_str()).unwrap_or("");
                if dt.is_empty() || seen.contains(dt) {
                    continue;
                }
                seen.insert(dt.to_string());
                result.push(dt.to_string());
            }
        }

        result
    }
}

// =========================================================================
// 测试
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    static TEST_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn temp_store() -> ApnsStore {
        let dir = std::env::temp_dir();
        let seq = TEST_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let path = dir.join(format!(
            "oc-test-apns-{}-{}-{}.json",
            std::process::id(),
            now_millis(),
            seq
        ));
        let _ = std::fs::remove_file(&path);
        ApnsStore::new(path)
    }

    #[test]
    fn add_and_read_token() {
        let store = temp_store();
        store.add_or_update_token("session1", "abc123", None, None);
        let tokens = store.read_all_tokens();
        assert_eq!(tokens, vec!["abc123".to_string()]);
    }

    #[test]
    fn add_dedup_token() {
        let store = temp_store();
        store.add_or_update_token("session1", "abc123", None, None);
        store.add_or_update_token("session1", "abc123", None, None);
        let tokens = store.read_all_tokens();
        assert_eq!(tokens.len(), 1);
    }

    #[test]
    fn platform_normalization() {
        let store = temp_store();
        store.add_or_update_token("session1", "t1", None, Some("android"));
        store.add_or_update_token("session2", "t2", None, Some("ios"));
        store.add_or_update_token("session3", "t3", None, None);

        let file_data = std::fs::read_to_string(&store.path).unwrap();
        let parsed: ApnsStoreFile = serde_json::from_str(&file_data).unwrap();
        let t1 = &parsed.tokens_by_session["session1"][0];
        assert_eq!(t1.get("platform").and_then(|v| v.as_str()), Some("android"));
    }

    #[test]
    fn remove_token() {
        let store = temp_store();
        store.add_or_update_token("session1", "t1", None, None);
        store.add_or_update_token("session1", "t2", None, None);
        store.remove_token("session1", "t1");
        let tokens = store.read_all_tokens();
        assert_eq!(tokens, vec!["t2".to_string()]);
    }

    #[test]
    fn remove_from_all_sessions() {
        let store = temp_store();
        store.add_or_update_token("session1", "t1", None, None);
        store.add_or_update_token("session2", "t1", None, None);
        store.remove_token_from_all_sessions("t1");
        let tokens = store.read_all_tokens();
        assert!(tokens.is_empty());
    }
}
