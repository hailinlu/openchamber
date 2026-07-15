//! `quota/providers/openai.js` 移植。
//!
//! OpenAI provider — read access token from opencode auth.json (aliases: openai/codex/chatgpt),
//! hit `https://chatgpt.com/backend-api/wham/usage`.
#![allow(dead_code)]

use serde_json::{json, Value};

use crate::opencode::auth::read_auth_file;
use crate::quota::providers::{api_error, fetch_error, not_configured};
use crate::quota::utils::auth::{get_auth_entry, normalize_auth_entry};
use crate::quota::utils::formatters::{build_result, to_usage_window, BuildResultArgs, ToUsageWindowArgs};
use crate::quota::{block_on, http_client};

pub const PROVIDER_ID: &str = "openai";
pub const PROVIDER_NAME: &str = "OpenAI";
pub const ALIASES: &[&str] = &["openai", "codex", "chatgpt"];

fn read_access_token() -> Option<String> {
    let auth = read_auth_file().unwrap_or_default();
    let entry = normalize_auth_entry(get_auth_entry(&auth, ALIASES));
    entry
        .as_ref()
        .and_then(|v| v.get("access").or_else(|| v.get("token")).and_then(|x| x.as_str()))
        .map(|s| s.to_string())
}

fn to_ms(value: Option<&Value>) -> Option<i64> {
    let n = value?.as_f64()?;
    let n = n as i64;
    Some(if n < 1_000_000_000_000 { n * 1000 } else { n })
}

pub fn is_configured() -> bool {
    read_access_token().is_some()
}

pub async fn fetch_quota_async() -> Value {
    let Some(access_token) = read_access_token() else {
        return not_configured(PROVIDER_ID, PROVIDER_NAME);
    };

    let client = http_client();
    let resp = client
        .get("https://chatgpt.com/backend-api/wham/usage")
        .bearer_auth(&access_token)
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

    let primary = payload.get("rate_limit").and_then(|r| r.get("primary_window"));
    let secondary = payload.get("rate_limit").and_then(|r| r.get("secondary_window"));

    let mut windows = serde_json::Map::new();
    if let Some(p) = primary {
        windows.insert(
            "5h".into(),
            to_usage_window(ToUsageWindowArgs {
                used_percent: p.get("used_percent").and_then(|v| v.as_f64()),
                window_seconds: p.get("limit_window_seconds").and_then(|v| v.as_i64()),
                reset_at: to_ms(p.get("reset_at")),
                value_label: None,
            }),
        );
    }
    if let Some(s) = secondary {
        windows.insert(
            "weekly".into(),
            to_usage_window(ToUsageWindowArgs {
                used_percent: s.get("used_percent").and_then(|v| v.as_f64()),
                window_seconds: s.get("limit_window_seconds").and_then(|v| v.as_i64()),
                reset_at: to_ms(s.get("reset_at")),
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

/// 同步 wrapper (供 registry `fn() -> Value` 使用)。
pub fn fetch_quota() -> Value {
    block_on(fetch_quota_async())
}

/// 同步版本 — function-pointer signature.
pub fn fetch_quota_sync() -> Value {
    fetch_quota()
}

/// Node 兼容别名。
pub use fetch_quota as fetch_openai_quota;
