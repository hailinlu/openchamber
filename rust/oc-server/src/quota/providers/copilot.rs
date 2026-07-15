//! `quota/providers/copilot.js` 移植。

use serde_json::{json, Value};

use crate::opencode::auth::read_auth_file;
use crate::quota::providers::{api_error, fetch_error, not_configured};
use crate::quota::utils::auth::{get_auth_entry, normalize_auth_entry};
use crate::quota::utils::formatters::{build_result, to_number, to_usage_window, BuildResultArgs, ToUsageWindowArgs};
use crate::quota::utils::transformers::to_timestamp;
use crate::quota::{block_on, http_client};

pub const PROVIDER_ID: &str = "github-copilot";
pub const PROVIDER_NAME: &str = "GitHub Copilot";
pub const PROVIDER_ID_ADDON: &str = "github-copilot-addon";
pub const PROVIDER_NAME_ADDON: &str = "GitHub Copilot Add-on";
pub const ALIASES: &[&str] = &["github-copilot", "copilot"];

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

fn build_windows(payload: &Value) -> serde_json::Map<String, Value> {
    let mut windows = serde_json::Map::new();
    let reset_at = to_timestamp(payload.get("quota_reset_date").unwrap_or(&Value::Null));
    let quota = payload
        .get("quota_snapshots")
        .cloned()
        .unwrap_or(json!({}));

    fn add_window(map: &mut serde_json::Map<String, Value>, label: &str, snap: Option<&Value>, reset_at: Option<i64>) {
        let Some(snap) = snap else { return };
        let entitlement = to_number(snap.get("entitlement").unwrap_or(&Value::Null));
        let remaining = to_number(snap.get("remaining").unwrap_or(&Value::Null));
        let used_percent = match (entitlement, remaining) {
            (Some(e), Some(r)) if e > 0.0 => Some((100.0 - (r / e * 100.0)).max(0.0)),
            _ => None,
        };
        let value_label = match (entitlement, remaining) {
            (Some(e), Some(r)) => Some(format!("{} / {} left", r as i64, e as i64)),
            _ => None,
        };
        map.insert(
            label.to_string(),
            to_usage_window(ToUsageWindowArgs {
                used_percent,
                window_seconds: None,
                reset_at,
                value_label: value_label.as_deref(),
            }),
        );
    }

    add_window(&mut windows, "chat", quota.get("chat"), reset_at);
    add_window(&mut windows, "completions", quota.get("completions"), reset_at);
    add_window(&mut windows, "premium", quota.get("premium_interactions"), reset_at);

    windows
}

async fn fetch_main(
    provider_id: &'static str,
    provider_name: &'static str,
    keep_only_premium: bool,
) -> Value {
    let Some(access_token) = read_access_token() else {
        return not_configured(provider_id, provider_name);
    };

    let client = http_client();
    let resp = client
        .get("https://api.github.com/copilot_internal/user")
        .header("Authorization", format!("token {access_token}"))
        .header("Accept", "application/json")
        .header("Editor-Version", "vscode/1.96.2")
        .header("X-Github-Api-Version", "2025-04-01")
        .send()
        .await;

    let resp = match resp {
        Ok(r) => r,
        Err(e) => return fetch_error(provider_id, provider_name, &e.to_string()),
    };
    if !resp.status().is_success() {
        return api_error(provider_id, provider_name, resp.status().as_u16());
    }
    let payload: Value = match resp.json().await {
        Ok(p) => p,
        Err(e) => return fetch_error(provider_id, provider_name, &e.to_string()),
    };

    let mut windows = build_windows(&payload);
    if keep_only_premium {
        let premium = windows.remove("premium");
        windows = match premium {
            Some(p) => {
                let mut m = serde_json::Map::new();
                m.insert("premium".to_string(), p);
                m
            }
            None => serde_json::Map::new(),
        };
    }

    build_result(BuildResultArgs {
        provider_id,
        provider_name,
        ok: true,
        configured: true,
        usage: Some(json!({"windows": Value::Object(windows)})),
        error: None,
    })
}

pub async fn fetch_quota_async() -> Value {
    fetch_main(PROVIDER_ID, PROVIDER_NAME, false).await
}

pub async fn fetch_quota_addon_async() -> Value {
    fetch_main(PROVIDER_ID_ADDON, PROVIDER_NAME_ADDON, true).await
}

pub fn fetch_quota() -> Value {
    block_on(fetch_quota_async())
}

pub fn fetch_quota_addon() -> Value {
    block_on(fetch_quota_addon_async())
}

pub fn fetch_quota_sync() -> Value {
    fetch_quota()
}

pub fn fetch_quota_addon_sync() -> Value {
    fetch_quota_addon()
}

pub use fetch_quota as fetch_copilot_quota;
pub use fetch_quota_addon as fetch_copilot_addon_quota;
