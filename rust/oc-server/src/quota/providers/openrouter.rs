//! `quota/providers/openrouter.js` 移植。

use serde_json::{json, Value};

use crate::opencode::auth::read_auth_file;
use crate::quota::providers::{api_error, fetch_error, not_configured};
use crate::quota::utils::auth::{get_auth_entry, normalize_auth_entry};
use crate::quota::utils::formatters::{build_result, format_money, to_number, to_usage_window, BuildResultArgs, ToUsageWindowArgs};
use crate::quota::{block_on, http_client};

pub const PROVIDER_ID: &str = "openrouter";
pub const PROVIDER_NAME: &str = "OpenRouter";
pub const ALIASES: &[&str] = &["openrouter"];

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
        .get("https://openrouter.ai/api/v1/credits")
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

    let credits = payload.get("data").cloned().unwrap_or(json!({}));
    let total_credits = to_number(credits.get("total_credits").unwrap_or(&Value::Null));
    let total_usage = to_number(credits.get("total_usage").unwrap_or(&Value::Null));
    let remaining = match (total_credits, total_usage) {
        (Some(t), Some(u)) => Some((t - u).max(0.0)),
        _ => None,
    };
    let value_label = match (remaining, total_usage) {
        (Some(r), Some(u)) => match (format_money(r), format_money(u)) {
            (Some(rs), Some(us)) => Some(format!("${rs} left · ${us} spent")),
            _ => None,
        },
        _ => None,
    };

    let mut windows = serde_json::Map::new();
    windows.insert(
        "credits".into(),
        to_usage_window(ToUsageWindowArgs {
            used_percent: None,
            window_seconds: None,
            reset_at: None,
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

pub use fetch_quota as fetch_openrouter_quota;
