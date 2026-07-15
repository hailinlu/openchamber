//! `quota/providers/wafer.js` 移植。

use serde_json::{json, Value};

use crate::opencode::auth::read_auth_file;
use crate::quota::providers::{api_error, fetch_error, not_configured};
use crate::quota::utils::auth::{get_auth_entry, normalize_auth_entry};
use crate::quota::utils::formatters::{
    build_result, resolve_window_label, to_number, to_usage_window, BuildResultArgs,
    ToUsageWindowArgs,
};
use crate::quota::utils::transformers::{as_non_empty_string, to_timestamp};
use crate::quota::{block_on, http_client};

pub const PROVIDER_ID: &str = "wafer";
pub const PROVIDER_NAME: &str = "Wafer.ai";
pub const ALIASES: &[&str] = &["wafer", "wafer-ai", "wafer_ai", "wafer.ai"];

const WAFER_QUOTA_URL: &str = "https://pass.wafer.ai/v1/inference/quota";
const WAFER_WINDOW_SECONDS: i64 = 5 * 3600;

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
        .get(WAFER_QUOTA_URL)
        .bearer_auth(&api_key)
        .header("Accept-Encoding", "identity")
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

    let remaining = to_number(payload.get("remaining_included_requests").unwrap_or(&Value::Null));
    let limit = to_number(payload.get("included_request_limit").unwrap_or(&Value::Null));
    let overage = to_number(payload.get("overage_request_count").unwrap_or(&Value::Null));
    let used_percent_raw = to_number(payload.get("current_period_used_percent").unwrap_or(&Value::Null));
    let window_start = to_timestamp(payload.get("window_start").unwrap_or(&Value::Null));
    let window_end = to_timestamp(payload.get("window_end").unwrap_or(&Value::Null));
    let plan_tier = as_non_empty_string(payload.get("plan_tier").unwrap_or(&Value::Null));

    if remaining.is_none() && limit.is_none() && overage.is_none() && used_percent_raw.is_none() {
        return build_result(BuildResultArgs {
            provider_id: PROVIDER_ID,
            provider_name: PROVIDER_NAME,
            ok: false,
            configured: true,
            usage: None,
            error: Some("No quota data in response"),
        });
    }

    let has_overage = overage.map(|o| o > 0.0).unwrap_or(false);
    let used_percent = if has_overage {
        Some((used_percent_raw.unwrap_or(0.0)).max(0.0))
    } else {
        Some((used_percent_raw.unwrap_or(0.0)).clamp(0.0, 100.0))
    };

    let window_seconds = match (window_start, window_end) {
        (Some(s), Some(e)) => Some(((e - s) / 1000).max(0)),
        _ => Some(WAFER_WINDOW_SECONDS),
    };
    let window_label = resolve_window_label(window_seconds);

    let value_label = match (remaining, limit) {
        (Some(r), Some(l)) => {
            let mut parts: Vec<String> = Vec::new();
            if let Some(p) = plan_tier {
                parts.push(p);
            }
            parts.push(format!("{r} / {l} left"));
            if has_overage {
                parts.push(format!("+{:.0} overage", overage.unwrap_or(0.0)));
            }
            Some(parts.join(" \u{00b7} "))
        }
        _ => None,
    };

    let mut windows = serde_json::Map::new();
    windows.insert(
        window_label,
        to_usage_window(ToUsageWindowArgs {
            used_percent,
            window_seconds,
            reset_at: window_end,
            value_label: value_label.as_deref(),
        }),
    );

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

pub use fetch_quota as fetch_wafer_quota;
