//! `quota/providers/nanogpt.js` 移植。

use serde_json::{json, Value};

use crate::opencode::auth::read_auth_file;
use crate::quota::providers::{api_error, fetch_error, not_configured};
use crate::quota::utils::auth::{get_auth_entry, normalize_auth_entry};
use crate::quota::utils::formatters::{build_result, to_number, to_usage_window, BuildResultArgs, ToUsageWindowArgs};
use crate::quota::utils::transformers::to_timestamp;
use crate::quota::{block_on, http_client};

pub const PROVIDER_ID: &str = "nano-gpt";
pub const PROVIDER_NAME: &str = "NanoGPT";
pub const ALIASES: &[&str] = &["nano-gpt", "nanogpt", "nano_gpt"];

const DAILY_WINDOW_SECONDS: i64 = 86_400;

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
        .get("https://nano-gpt.com/api/subscription/v1/usage")
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

    let period = payload.get("period");
    let daily = payload.get("daily");
    let monthly = payload.get("monthly");
    let state = payload
        .get("state")
        .and_then(|v| v.as_str())
        .unwrap_or("active");

    let mut windows = serde_json::Map::new();

    if let Some(d) = daily {
        let percent_used = d.get("percentUsed").and_then(|v| v.as_f64());
        let used_percent = if let Some(pu) = percent_used {
            Some((pu * 100.0).clamp(0.0, 100.0))
        } else {
            let used = to_number(d.get("used").unwrap_or(&Value::Null));
            let limit = to_number(
                d.get("limit")
                    .unwrap_or(&Value::Null)
                    .as_object()
                    .map(|_| Value::Null)
                    .as_ref()
                    .unwrap_or(&Value::Null),
            )
            .or_else(|| {
                d.get("limits")
                    .and_then(|l| l.get("daily"))
                    .and_then(|v| v.as_f64())
                    .map(|n| n)
            });
            if let (Some(u), Some(l)) = (used, limit) {
                if l > 0.0 {
                    Some((u / l * 100.0).clamp(0.0, 100.0))
                } else {
                    None
                }
            } else {
                None
            }
        };
        let reset_at = to_timestamp(d.get("resetAt").unwrap_or(&Value::Null));
        let value_label = if state != "active" {
            Some(format!("({state})"))
        } else {
            None
        };
        windows.insert(
            "daily".into(),
            to_usage_window(ToUsageWindowArgs {
                used_percent,
                window_seconds: Some(DAILY_WINDOW_SECONDS),
                reset_at,
                value_label: value_label.as_deref(),
            }),
        );
    }

    if let Some(m) = monthly {
        let percent_used = m.get("percentUsed").and_then(|v| v.as_f64());
        let used_percent = if let Some(pu) = percent_used {
            Some((pu * 100.0).clamp(0.0, 100.0))
        } else {
            let used = to_number(m.get("used").unwrap_or(&Value::Null));
            let limit = m
                .get("limit")
                .and_then(|v| v.as_f64())
                .or_else(|| m.get("limits").and_then(|l| l.get("monthly")).and_then(|v| v.as_f64()));
            if let (Some(u), Some(l)) = (used, limit) {
                if l > 0.0 {
                    Some((u / l * 100.0).clamp(0.0, 100.0))
                } else {
                    None
                }
            } else {
                None
            }
        };
        let reset_at = to_timestamp(m.get("resetAt").unwrap_or(&Value::Null))
            .or_else(|| to_timestamp(period.and_then(|p| p.get("currentPeriodEnd")).unwrap_or(&Value::Null)));
        let value_label = if state != "active" {
            Some(format!("({state})"))
        } else {
            None
        };
        windows.insert(
            "monthly".into(),
            to_usage_window(ToUsageWindowArgs {
                used_percent,
                window_seconds: None,
                reset_at,
                value_label: value_label.as_deref(),
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

pub use fetch_quota as fetch_nanogpt_quota;
