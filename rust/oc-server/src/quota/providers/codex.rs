//! `quota/providers/codex.js` 移植。

use serde_json::{json, Value};

use crate::opencode::auth::read_auth_file;
use crate::quota::providers::{api_error, fetch_error, not_configured};
use crate::quota::utils::auth::{get_auth_entry, normalize_auth_entry};
use crate::quota::utils::formatters::{
    build_result, format_money, resolve_window_label, to_number, to_usage_window, BuildResultArgs,
    ToUsageWindowArgs,
};
use crate::quota::{block_on, http_client};

pub const PROVIDER_ID: &str = "codex";
pub const PROVIDER_NAME: &str = "Codex";
pub const ALIASES: &[&str] = &["openai", "codex", "chatgpt"];

fn read_entry() -> Option<(String, Option<String>)> {
    let auth = read_auth_file().unwrap_or_default();
    let entry = normalize_auth_entry(get_auth_entry(&auth, ALIASES))?;
    let token = entry
        .get("access")
        .or_else(|| entry.get("token"))
        .and_then(|x| x.as_str())
        .map(|s| s.to_string());
    let account = entry
        .get("accountId")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string());
    Some((token?, account))
}

pub fn is_configured() -> bool {
    read_entry().is_some()
}

fn to_ms(value: Option<&Value>) -> Option<i64> {
    let n = value?.as_f64()?;
    let n = n as i64;
    Some(if n < 1_000_000_000_000 { n * 1000 } else { n })
}

pub async fn fetch_quota_async() -> Value {
    let Some((access_token, account_id)) = read_entry() else {
        return not_configured(PROVIDER_ID, PROVIDER_NAME);
    };

    let client = http_client();
    let mut req = client
        .get("https://chatgpt.com/backend-api/wham/usage")
        .bearer_auth(&access_token)
        .header("Content-Type", "application/json");
    if let Some(ref aid) = account_id {
        if !aid.is_empty() {
            req = req.header("ChatGPT-Account-Id", aid);
        }
    }

    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => return fetch_error(PROVIDER_ID, PROVIDER_NAME, &e.to_string()),
    };
    let status = resp.status();

    if !status.is_success() {
        let err_msg = if status.as_u16() == 401 {
            "Session expired \u{2014} please re-authenticate with OpenAI"
        } else {
            // Re-use api_error pattern but with custom message
            return build_result(BuildResultArgs {
                provider_id: PROVIDER_ID,
                provider_name: PROVIDER_NAME,
                ok: false,
                configured: true,
                usage: None,
                error: Some(&format!("API error: {}", status.as_u16())[..]),
            });
        };
        return build_result(BuildResultArgs {
            provider_id: PROVIDER_ID,
            provider_name: PROVIDER_NAME,
            ok: false,
            configured: true,
            usage: None,
            error: Some(err_msg),
        });
    }

    let payload: Value = match resp.json().await {
        Ok(p) => p,
        Err(e) => return fetch_error(PROVIDER_ID, PROVIDER_NAME, &e.to_string()),
    };

    let primary = payload.get("rate_limit").and_then(|r| r.get("primary_window"));
    let secondary = payload.get("rate_limit").and_then(|r| r.get("secondary_window"));
    let credits = payload.get("credits");

    let mut windows = serde_json::Map::new();

    if let Some(p) = primary {
        let ws = p.get("limit_window_seconds").and_then(|v| v.as_i64());
        windows.insert(
            resolve_window_label(ws),
            to_usage_window(ToUsageWindowArgs {
                used_percent: p.get("used_percent").and_then(|v| v.as_f64()),
                window_seconds: ws,
                reset_at: to_ms(p.get("reset_at")),
                value_label: None,
            }),
        );
    }
    if let Some(s) = secondary {
        let ws = s.get("limit_window_seconds").and_then(|v| v.as_i64());
        windows.insert(
            resolve_window_label(ws),
            to_usage_window(ToUsageWindowArgs {
                used_percent: s.get("used_percent").and_then(|v| v.as_f64()),
                window_seconds: ws,
                reset_at: to_ms(s.get("reset_at")),
                value_label: None,
            }),
        );
    }
    if let Some(c) = credits {
        let balance = to_number(c.get("balance").unwrap_or(&Value::Null));
        let unlimited = c.get("unlimited").and_then(|v| v.as_bool()).unwrap_or(false);
        let label = if unlimited {
            Some("Unlimited".to_string())
        } else if let Some(b) = balance {
            format_money(b).map(|m| format!("${m}"))
        } else {
            None
        };
        windows.insert(
            "credits_balance".into(),
            to_usage_window(ToUsageWindowArgs {
                used_percent: None,
                window_seconds: None,
                reset_at: None,
                value_label: label.as_deref(),
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

pub use fetch_quota as fetch_codex_quota;
