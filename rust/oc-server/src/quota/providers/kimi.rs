//! `quota/providers/kimi.js` 移植。

use serde_json::{json, Value};

use crate::opencode::auth::read_auth_file;
use crate::quota::providers::{api_error, fetch_error, not_configured};
use crate::quota::utils::auth::{get_auth_entry, normalize_auth_entry};
use crate::quota::utils::formatters::{
    build_result, duration_to_label, duration_to_seconds, to_number, to_timestamp as ts_to_ts, to_usage_window, BuildResultArgs,
    ToUsageWindowArgs,
};
use crate::quota::utils::transformers::to_timestamp;
use crate::quota::{block_on, http_client};

pub const PROVIDER_ID: &str = "kimi-for-coding";
pub const PROVIDER_NAME: &str = "Kimi for Coding";
pub const ALIASES: &[&str] = &["kimi-for-coding", "kimi"];

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
        .get("https://api.kimi.com/coding/v1/usages")
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

    let mut windows = serde_json::Map::new();
    if let Some(usage) = payload.get("usage") {
        let limit = to_number(usage.get("limit").unwrap_or(&Value::Null));
        let remaining = to_number(usage.get("remaining").unwrap_or(&Value::Null));
        let used_percent = if let (Some(l), Some(r)) = (limit, remaining) {
            if l > 0.0 {
                Some((100.0 - (r / l) * 100.0).clamp(0.0, 100.0))
            } else {
                None
            }
        } else {
            None
        };
        windows.insert(
            "weekly".into(),
            to_usage_window(ToUsageWindowArgs {
                used_percent,
                window_seconds: None,
                reset_at: ts_to_ts(usage.get("resetTime").unwrap_or(&Value::Null)),
                value_label: None,
            }),
        );
    }

    let limits = payload
        .get("limits")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    for limit in limits {
        let window = limit.get("window");
        let detail = limit.get("detail");
        let raw_label = duration_to_label(window.and_then(|w| w.get("duration").and_then(|v| v.as_f64())), window.and_then(|w| w.get("timeUnit")).and_then(|v| v.as_str()));
        let window_seconds = duration_to_seconds(
            window.and_then(|w| w.get("duration").and_then(|v| v.as_f64())),
            window.and_then(|w| w.get("timeUnit")).and_then(|v| v.as_str()),
        );
        let label = if window_seconds == Some(5 * 3600) {
            format!("Rate Limit ({raw_label})")
        } else {
            raw_label
        };
        let total = to_number(detail.and_then(|d| d.get("limit")).unwrap_or(&Value::Null));
        let remaining = to_number(detail.and_then(|d| d.get("remaining")).unwrap_or(&Value::Null));
        let used_percent = if let (Some(t), Some(r)) = (total, remaining) {
            if t > 0.0 {
                Some((100.0 - (r / t) * 100.0).clamp(0.0, 100.0))
            } else {
                None
            }
        } else {
            None
        };
        windows.insert(
            label,
            to_usage_window(ToUsageWindowArgs {
                used_percent,
                window_seconds,
                reset_at: to_timestamp(detail.and_then(|d| d.get("resetTime")).unwrap_or(&Value::Null)),
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

pub fn fetch_quota() -> Value {
    block_on(fetch_quota_async())
}

pub fn fetch_quota_sync() -> Value {
    fetch_quota()
}

pub use fetch_quota as fetch_kimi_quota;
