//! `quota/providers/zai.js` 移植。

use serde_json::{json, Value};

use crate::opencode::auth::read_auth_file;
use crate::quota::providers::{api_error, fetch_error, not_configured};
use crate::quota::utils::auth::{get_auth_entry, normalize_auth_entry};
use crate::quota::utils::formatters::{
    build_result, resolve_window_label, resolve_window_seconds, to_usage_window, BuildResultArgs,
    ToUsageWindowArgs,
};
use crate::quota::utils::transformers::normalize_timestamp;
use crate::quota::{block_on, http_client};

pub const PROVIDER_ID: &str = "zai-coding-plan";
pub const PROVIDER_NAME: &str = "z.ai";
pub const ALIASES: &[&str] = &["zai-coding-plan", "zai", "z.ai"];

fn read_api_key() -> Option<String> {
    let auth = read_auth_file().unwrap_or_default();
    let entry = normalize_auth_entry(get_auth_entry(&auth, ALIASES));
    entry
        .as_ref()
        .and_then(|v| v.get("key").or_else(|| v.get("token")).and_then(|x| x.as_str()))
        .map(|s| s.to_string())
}

pub fn is_configured() -> bool {
    read_api_key().is_some()
}

pub async fn fetch_quota_async() -> Value {
    let Some(api_key) = read_api_key() else {
        return not_configured(PROVIDER_ID, PROVIDER_NAME);
    };

    let client = http_client();
    let resp = client
        .get("https://api.z.ai/api/monitor/usage/quota/limit")
        .bearer_auth(&api_key)
        .header("Content-Type", "application/json")
        .send()
        .await;

    let resp = match resp {
        Ok(r) => r,
        Err(e) => return fetch_error(PROVIDER_ID, PROVIDER_NAME, &e.to_string()),
    };
    if !resp.status().is_success() {
        return api_error(PROVIDER_ID, PROVIDER_NAME, resp.status().as_u16());
    }
    let payload: Value = match resp.json().await {
        Ok(p) => p,
        Err(e) => return fetch_error(PROVIDER_ID, PROVIDER_NAME, &e.to_string()),
    };

    let limits = payload
        .get("data")
        .and_then(|d| d.get("limits"))
        .and_then(|l| l.as_array())
        .cloned()
        .unwrap_or_default();

    let tokens_limit = limits.iter().find(|l| {
        l.get("type").and_then(|t| t.as_str()) == Some("TOKENS_LIMIT")
    });

    let ws = tokens_limit.and_then(resolve_window_seconds_borrowed);
    let window_label = resolve_window_label(ws);
    let reset_at = tokens_limit
        .and_then(|tl| tl.get("nextResetTime"))
        .and_then(normalize_timestamp);
    let used_percent = tokens_limit
        .and_then(|tl| tl.get("percentage"))
        .and_then(|v| v.as_f64());

    let mut windows = serde_json::Map::new();
    if tokens_limit.is_some() {
        windows.insert(
            window_label,
            to_usage_window(ToUsageWindowArgs {
                used_percent,
                window_seconds: ws,
                reset_at,
                value_label: None,
            }),
        );
    }

    build_result(BuildResultArgs {
        provider_id: PROVIDER_ID,
        provider_name: PROVIDER_NAME,
        ok: true,
        configured: true,
        usage: Some(json!({"windows": Value::Object(windows)})),
        error: None,
    })
}

/// `resolve_window_seconds` 需要 `&Value`,我们借用避免 clone。
fn resolve_window_seconds_borrowed(limit: &Value) -> Option<i64> {
    resolve_window_seconds(limit)
}

pub fn fetch_quota() -> Value {
    block_on(fetch_quota_async())
}

pub fn fetch_quota_sync() -> Value {
    fetch_quota()
}

pub use fetch_quota as fetch_zai_quota;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn resolve_window_seconds_for_tokens_limit() {
        // unit=3 (1h), number=5 → 5h
        let limit = json!({"unit": 3, "number": 5});
        assert_eq!(resolve_window_seconds(&limit), Some(5 * 3600));
        let label = resolve_window_label(Some(5 * 3600));
        assert_eq!(label, "5h");
    }

    #[test]
    fn resolve_window_label_weekly_zai() {
        let label = resolve_window_label(Some(7 * 86400));
        assert_eq!(label, "weekly");
    }
}
