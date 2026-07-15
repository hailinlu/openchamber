//! Web-push 订阅持久化 + UI 可见性 Map。
//!
//! 对应 Node `notifications/push-runtime.js` 的持久化 + 可见性部分。
//!
//! 文件: `$DATA_DIR/push-subscriptions.json`, 结构:
//! ```json
//! { "version": 1, "subscriptionsBySession": { "<token>": [ ... ] } }
//! ```
//! 串行化: `Mutex<()>` write_lock 保证 read-modify-write 原子性。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{now_millis, MAX_SUBS_PER_SESSION, PUSH_SUBSCRIPTIONS_VERSION, UI_VISIBILITY_TTL_MS};

/// 单条 push 订阅。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushSubscription {
    pub endpoint: String,
    pub p256dh: String,
    pub auth: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_seen_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
}

/// 文件存储格式。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PushStoreFile {
    version: u32,
    #[serde(rename = "subscriptionsBySession", default)]
    subscriptions_by_session: HashMap<String, Vec<Value>>,
}

impl Default for PushStoreFile {
    fn default() -> Self {
        Self {
            version: PUSH_SUBSCRIPTIONS_VERSION,
            subscriptions_by_session: HashMap::new(),
        }
    }
}

/// 可见性状态。
#[derive(Debug, Clone)]
struct VisibilityState {
    visible: bool,
    updated_at: i64,
    platform: Option<String>,
}

/// Web-push 订阅 + 可见性存储。
pub struct PushStore {
    path: PathBuf,
    write_lock: Mutex<()>,
    visibility: Mutex<HashMap<String, VisibilityState>>,
}

impl PushStore {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            write_lock: Mutex::new(()),
            visibility: Mutex::new(HashMap::new()),
        }
    }

    // -----------------------------------------------------------------------
    // 文件持久化
    // -----------------------------------------------------------------------

    fn read_from_disk(&self) -> PushStoreFile {
        match std::fs::read_to_string(&self.path) {
            Ok(content) => {
                serde_json::from_str(&content).unwrap_or_default()
            }
            Err(_) => PushStoreFile::default(),
        }
    }

    fn write_to_disk(&self, data: &PushStoreFile) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(content) = serde_json::to_string_pretty(data) {
            let _ = std::fs::write(&self.path, content);
        }
    }

    fn persist_update<F>(&self, mutate: F)
    where
        F: FnOnce(&mut PushStoreFile),
    {
        let _lock = self.write_lock.lock().unwrap();
        let mut current = self.read_from_disk();
        if current.version != PUSH_SUBSCRIPTIONS_VERSION {
            current = PushStoreFile::default();
        }
        mutate(&mut current);
        self.write_to_disk(&current);
    }

    // -----------------------------------------------------------------------
    // 订阅 CRUD
    // -----------------------------------------------------------------------

    /// 添加或更新订阅。
    ///
    /// 对应 Node `addOrUpdatePushSubscription`。
    pub fn add_or_update_subscription(
        &self,
        ui_session_token: &str,
        endpoint: &str,
        p256dh: &str,
        auth: &str,
        user_agent: Option<&str>,
        platform: Option<&str>,
    ) {
        if ui_session_token.is_empty() {
            return;
        }

        let now = now_millis();
        let endpoint_owned = endpoint.to_string();
        let p256dh_owned = p256dh.to_string();
        let auth_owned = auth.to_string();
        let ua_owned = user_agent.map(|s| s.to_string());
        let platform_owned = platform.map(|s| s.to_string());
        let token = ui_session_token.to_string();

        self.persist_update(move |store| {
            let existing = store
                .subscriptions_by_session
                .entry(token.clone())
                .or_default();

            // 保留之前同 endpoint 的 platform
            let previous_platform: Option<String> = existing
                .iter()
                .find(|e| {
                    e.get("endpoint").and_then(|v| v.as_str()) == Some(&endpoint_owned)
                })
                .and_then(|e| e.get("platform").and_then(|v| v.as_str()).map(|s| s.to_string()));

            // 过滤掉同 endpoint 的旧条目
            existing.retain(|e| {
                e.get("endpoint").and_then(|v| v.as_str()).map(|s| s != endpoint_owned).unwrap_or(true)
            });

            let new_entry = json!({
                "endpoint": endpoint_owned,
                "p256dh": p256dh_owned,
                "auth": auth_owned,
                "createdAt": now,
                "lastSeenAt": now,
                "userAgent": ua_owned,
                "platform": platform_owned.or(previous_platform),
            });
            existing.insert(0, new_entry);
            existing.truncate(MAX_SUBS_PER_SESSION);
        });
    }

    /// 移除指定 endpoint 的订阅。
    ///
    /// 对应 Node `removePushSubscription`。
    pub fn remove_subscription(&self, ui_session_token: &str, endpoint: &str) {
        if ui_session_token.is_empty() || endpoint.is_empty() {
            return;
        }
        let token = ui_session_token.to_string();
        let endpoint_owned = endpoint.to_string();

        self.persist_update(move |store| {
            let should_remove = {
                let subs = store
                    .subscriptions_by_session
                    .get(&token)
                    .cloned()
                    .unwrap_or_default();
                let filtered: Vec<Value> = subs
                    .into_iter()
                    .filter(|e| {
                        e.get("endpoint").and_then(|v| v.as_str()).map(|s| s != endpoint_owned).unwrap_or(true)
                    })
                    .collect();
                if filtered.is_empty() {
                    store.subscriptions_by_session.remove(&token);
                } else {
                    store.subscriptions_by_session.insert(token.clone(), filtered);
                }
                false
            };
            let _ = should_remove;
        });
    }

    /// 从所有 session 中移除指定 endpoint。
    ///
    /// 对应 Node `removePushSubscriptionFromAllSessions` (死订阅清理用)。
    pub fn remove_from_all_sessions(&self, endpoint: &str) {
        if endpoint.is_empty() {
            return;
        }
        let endpoint_owned = endpoint.to_string();

        self.persist_update(move |store| {
            let tokens: Vec<String> = store.subscriptions_by_session.keys().cloned().collect();
            for token in tokens {
                if let Some(subs) = store.subscriptions_by_session.get(&token) {
                    let filtered: Vec<Value> = subs
                        .iter()
                        .filter(|e| {
                            e.get("endpoint").and_then(|v| v.as_str()).map(|s| s != endpoint_owned).unwrap_or(true)
                        })
                        .cloned()
                        .collect();
                    if filtered.is_empty() {
                        store.subscriptions_by_session.remove(&token);
                    } else {
                        store.subscriptions_by_session.insert(token, filtered);
                    }
                }
            }
        });
    }

    /// 读取所有订阅 (去重 endpoint), 返回解析后的列表。
    ///
    /// 对应 Node `sendPushToAllUiSessions` 中的订阅收集。
    pub fn read_all_subscriptions(&self) -> Vec<PushSubscription> {
        let store = self.read_from_disk();
        let mut seen = std::collections::HashSet::new();
        let mut result = Vec::new();

        for subs in store.subscriptions_by_session.values() {
            for entry in subs {
                let endpoint = entry.get("endpoint").and_then(|v| v.as_str()).unwrap_or("");
                let p256dh = entry.get("p256dh").and_then(|v| v.as_str()).unwrap_or("");
                let auth = entry.get("auth").and_then(|v| v.as_str()).unwrap_or("");
                if endpoint.is_empty() || p256dh.is_empty() || auth.is_empty() {
                    continue;
                }
                if seen.contains(endpoint) {
                    continue;
                }
                seen.insert(endpoint.to_string());
                result.push(PushSubscription {
                    endpoint: endpoint.to_string(),
                    p256dh: p256dh.to_string(),
                    auth: auth.to_string(),
                    created_at: entry.get("createdAt").and_then(|v| v.as_i64()),
                    last_seen_at: entry.get("lastSeenAt").and_then(|v| v.as_i64()),
                    user_agent: entry
                        .get("userAgent")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string()),
                    platform: entry
                        .get("platform")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string()),
                });
            }
        }

        result
    }

    // -----------------------------------------------------------------------
    // 可见性 (in-memory, TTL 30s)
    // -----------------------------------------------------------------------

    /// 修剪过期的可见性状态。
    fn prune_visibility(vis: &mut HashMap<String, VisibilityState>, now: i64) {
        vis.retain(|_, state| now - state.updated_at <= UI_VISIBILITY_TTL_MS);
    }

    /// 更新 UI 可见性。
    ///
    /// 对应 Node `updateUiVisibility`。
    pub fn update_visibility(&self, token: &str, visible: bool, platform: Option<&str>) {
        if token.is_empty() {
            return;
        }
        let now = now_millis();
        let mut vis = self.visibility.lock().unwrap();
        let existing = vis.get(token).cloned();
        let next_platform = platform
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .or_else(|| existing.and_then(|e| e.platform));
        vis.insert(
            token.to_string(),
            VisibilityState {
                visible,
                updated_at: now,
                platform: next_platform,
            },
        );
    }

    /// 是否有任意 UI 可见。
    ///
    /// 对应 Node `isAnyUiVisible`。
    pub fn is_any_ui_visible(&self) -> bool {
        let now = now_millis();
        let mut vis = self.visibility.lock().unwrap();
        Self::prune_visibility(&mut vis, now);
        vis.values().any(|s| s.visible && now - s.updated_at <= UI_VISIBILITY_TTL_MS)
    }

    /// 是否有任意交互客户端 (非移动端) 可见。
    ///
    /// 对应 Node `isAnyInteractiveClientVisible`。
    pub fn is_any_interactive_client_visible(&self) -> bool {
        let now = now_millis();
        let mut vis = self.visibility.lock().unwrap();
        Self::prune_visibility(&mut vis, now);
        vis.values().any(|s| {
            s.visible
                && now - s.updated_at <= UI_VISIBILITY_TTL_MS
                && !super::types::is_mobile_platform(s.platform.as_deref())
        })
    }

    /// 指定 token 是否可见。
    ///
    /// 对应 Node `isUiVisible`。
    pub fn is_ui_visible(&self, token: &str) -> bool {
        let now = now_millis();
        let mut vis = self.visibility.lock().unwrap();
        Self::prune_visibility(&mut vis, now);
        vis.get(token)
            .map(|s| s.visible && now - s.updated_at <= UI_VISIBILITY_TTL_MS)
            .unwrap_or(false)
    }
}

// =========================================================================
// 测试
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    static TEST_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn temp_store() -> PushStore {
        let dir = std::env::temp_dir();
        let seq = TEST_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let path = dir.join(format!(
            "oc-test-push-{}-{}-{}.json",
            std::process::id(),
            now_millis(),
            seq
        ));
        // 确保不存在
        let _ = std::fs::remove_file(&path);
        PushStore::new(path)
    }

    #[test]
    fn add_and_read_subscription() {
        let store = temp_store();
        store.add_or_update_subscription("token1", "ep1", "p1", "a1", None, None);
        let subs = store.read_all_subscriptions();
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].endpoint, "ep1");
    }

    #[test]
    fn add_dedup_by_endpoint() {
        let store = temp_store();
        store.add_or_update_subscription("token1", "ep1", "p1", "a1", None, None);
        store.add_or_update_subscription("token1", "ep1", "p2", "a2", None, None);
        let subs = store.read_all_subscriptions();
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].p256dh, "p2"); // 更新后的值
    }

    #[test]
    fn add_dedup_across_sessions() {
        let store = temp_store();
        store.add_or_update_subscription("token1", "ep1", "p1", "a1", None, None);
        store.add_or_update_subscription("token2", "ep1", "p1", "a1", None, None);
        let subs = store.read_all_subscriptions();
        assert_eq!(subs.len(), 1); // 去重 endpoint
    }

    #[test]
    fn remove_subscription() {
        let store = temp_store();
        store.add_or_update_subscription("token1", "ep1", "p1", "a1", None, None);
        store.add_or_update_subscription("token1", "ep2", "p2", "a2", None, None);
        store.remove_subscription("token1", "ep1");
        let subs = store.read_all_subscriptions();
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].endpoint, "ep2");
    }

    #[test]
    fn remove_from_all_sessions() {
        let store = temp_store();
        store.add_or_update_subscription("token1", "ep1", "p1", "a1", None, None);
        store.add_or_update_subscription("token2", "ep1", "p1", "a1", None, None);
        store.remove_from_all_sessions("ep1");
        let subs = store.read_all_subscriptions();
        assert!(subs.is_empty());
    }

    #[test]
    fn max_subs_per_session() {
        let store = temp_store();
        for i in 0..(MAX_SUBS_PER_SESSION + 5) {
            store.add_or_update_subscription(
                "token1",
                &format!("ep{}", i),
                "p",
                "a",
                None,
                None,
            );
        }
        let subs = store.read_all_subscriptions();
        assert_eq!(subs.len(), MAX_SUBS_PER_SESSION);
    }

    #[test]
    fn visibility_update_and_check() {
        let store = temp_store();
        store.update_visibility("token1", true, None);
        assert!(store.is_ui_visible("token1"));
        assert!(store.is_any_ui_visible());
    }

    #[test]
    fn visibility_not_visible_default() {
        let store = temp_store();
        assert!(!store.is_ui_visible("nonexistent"));
        assert!(!store.is_any_ui_visible());
    }

    #[test]
    fn visibility_interactive_vs_mobile() {
        let store = temp_store();
        store.update_visibility("mobile", true, Some("ios"));
        assert!(store.is_any_ui_visible());
        assert!(!store.is_any_interactive_client_visible());

        store.update_visibility("desktop", true, Some("web"));
        assert!(store.is_any_interactive_client_visible());
    }
}
