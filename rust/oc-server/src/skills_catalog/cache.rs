//! 简单的内存 TTL 缓存 — 对应 Node `cache.js`。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use once_cell::sync::Lazy;
use serde_json::Value;

static SKILL_CACHE: Lazy<Mutex<HashMap<String, CacheEntry>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

struct CacheEntry {
    value: Value,
    expires_at: Instant,
}

/// 默认 TTL: 30 分钟。
const DEFAULT_TTL: Duration = Duration::from_secs(30 * 60);

/// 构建缓存 key。
pub fn get_cache_key(normalized_repo: &str, subpath: Option<&str>, identity_id: Option<&str>) -> String {
    format!("{}|{}|{}", normalized_repo, subpath.unwrap_or(""), identity_id.unwrap_or(""))
}

/// 读取缓存, 过期或不存在时返回 `None`。
pub fn get_cached_scan(key: &str) -> Option<Value> {
    let cache = SKILL_CACHE.lock().unwrap();
    cache.get(key).and_then(|entry| {
        if Instant::now() < entry.expires_at {
            Some(entry.value.clone())
        } else {
            None
        }
    })
}

/// 写入缓存, 默认 30 分钟 TTL。
pub fn set_cached_scan(key: &str, value: Value, ttl_ms: Option<u64>) {
    let ttl = ttl_ms.map(Duration::from_millis).unwrap_or(DEFAULT_TTL);
    let mut cache = SKILL_CACHE.lock().unwrap();
    cache.insert(key.to_string(), CacheEntry {
        value,
        expires_at: Instant::now() + ttl,
    });
}

/// 清空所有缓存。
pub fn clear_cache() {
    let mut cache = SKILL_CACHE.lock().unwrap();
    cache.clear();
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cache_roundtrip() {
        let key = get_cache_key("test/repo", Some("skills"), None);
        let val = json!([{"skillName": "test"}]);

        set_cached_scan(&key, val.clone(), Some(10_000));
        let got = get_cached_scan(&key);
        assert!(got.is_some());
        assert_eq!(got.unwrap(), val);
    }

    #[test]
    fn cache_expires() {
        let key = "expire-test";
        set_cached_scan(key, json!("val"), Some(1)); // 1ms TTL
        std::thread::sleep(Duration::from_millis(5));
        assert!(get_cached_scan(key).is_none());
    }

    #[test]
    fn cache_miss() {
        assert!(get_cached_scan("nonexistent").is_none());
    }

    #[test]
    fn clear_cache_empties() {
        set_cached_scan("c1", json!("v1"), None);
        set_cached_scan("c2", json!("v2"), None);
        clear_cache();
        assert!(get_cached_scan("c1").is_none());
        assert!(get_cached_scan("c2").is_none());
    }
}
