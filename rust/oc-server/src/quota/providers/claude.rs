//! `quota/providers/claude.js` 移植。
//!
//! Claude provider — aliases: anthropic/claude, hits `https://api.anthropic.com/api/oauth/usage`.

use serde_json::{json, Value};

use crate::opencode::auth::read_auth_file;
use crate::quota::providers::{api_error, fetch_error, not_configured};
use crate::quota::utils::auth::{get_auth_entry, normalize_auth_entry};
use crate::quota::utils::formatters::{build_result, to_number, to_usage_window, BuildResultArgs, ToUsageWindowArgs};
use crate::quota::utils::transformers::to_timestamp as util_to_timestamp;
use crate::quota::{block_on, http_client};

pub const PROVIDER_ID: &str = "claude";
pub const PROVIDER_NAME: &str = "Claude";
pub const ALIASES: &[&str] = &["anthropic", "claude"];

fn read_access_token() -> Option<String> {
    let auth = read_auth_file().unwrap_or_default();
    let entry = normalize_auth_entry(get_auth_entry(&auth, ALIASES));
    entry
        .as_ref()
        .and_then(|v| v.get("access").or_else(|| v.get("token")).and_then(|x| x.as_str()))
        .map(|s| s.to_string())
}

pub fn is_configured() -> bool {
    read_access_token().is_some()
}

fn add_window(windows: &mut serde_json::Map<String, Value>, label: &str, src: Option<&Value>) {
    let Some(src) = src else { return };
    let used = src.get("utilization").and_then(|v| to_number(v));
    let reset = util_to_timestamp(src.get("resets_at").unwrap_or(&Value::Null));
    windows.insert(
        label.to_string(),
        to_usage_window(ToUsageWindowArgs {
            used_percent: used,
            window_seconds: None,
            reset_at: reset,
            value_label: None,
        }),
    );
}

pub async fn fetch_quota_async() -> Value {
    let Some(access_token) = read_access_token() else {
        return not_configured(PROVIDER_ID, PROVIDER_NAME);
    };

    let client = http_client();
    let resp = client
        .get("https://api.anthropic.com/api/oauth/usage")
        .bearer_auth(&access_token)
        .header("anthropic-beta", "oauth-2025-04-20")
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
    add_window(&mut windows, "5h", payload.get("five_hour"));
    add_window(&mut windows, "7d", payload.get("seven_day"));
    add_window(&mut windows, "7d-sonnet", payload.get("seven_day_sonnet"));
    add_window(&mut windows, "7d-opus", payload.get("seven_day_opus"));

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

pub use fetch_quota as fetch_claude_quota;
