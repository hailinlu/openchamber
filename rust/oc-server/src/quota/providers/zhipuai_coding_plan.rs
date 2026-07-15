//! `quota/providers/zhipuai-coding-plan.js` 移植。

use serde_json::{json, Value};

use crate::opencode::auth::read_auth_file;
use crate::opencode::config::read_config;
use crate::quota::providers::{api_error, fetch_error, not_configured};
use crate::quota::utils::auth::{get_auth_entry, normalize_auth_entry};
use crate::quota::utils::formatters::{
    build_result, resolve_window_seconds, to_usage_window, BuildResultArgs, ToUsageWindowArgs,
};
use crate::quota::utils::transformers::normalize_timestamp;
use crate::quota::{block_on, http_client};

pub const PROVIDER_ID: &str = "zhipuai-coding-plan";
pub const PROVIDER_NAME: &str = "Zhipu AI Coding Plan";
pub const ALIASES: &[&str] = &["zhipuai-coding-plan", "zhipuai", "zhipu"];

fn get_api_key() -> Option<String> {
    // 先查 auth.json
    let auth = read_auth_file().unwrap_or_default();
    if let Some(entry) = normalize_auth_entry(get_auth_entry(&auth, ALIASES)) {
        let k = entry
            .get("key")
            .or_else(|| entry.get("token"))
            .and_then(|x| x.as_str());
        if let Some(s) = k {
            return Some(s.to_string());
        }
    }

    // 回退到 merged config 的 provider options.apiKey
    if let Ok(merged) = read_config(None) {
        for alias in ALIASES {
            if let Some(ak) = merged
                .get("provider")
                .and_then(|p| p.get(*alias))
                .and_then(|pr| pr.get("options"))
                .and_then(|opt| opt.get("apiKey"))
                .and_then(|k| k.as_str())
            {
                return Some(ak.to_string());
            }
        }
    }

    None
}

pub fn is_configured() -> bool {
    get_api_key().is_some()
}

pub async fn fetch_quota_async() -> Value {
    let Some(api_key) = get_api_key() else {
        return not_configured(PROVIDER_ID, PROVIDER_NAME);
    };

    let client = http_client();
    let resp = client
        .get("https://open.bigmodel.cn/api/monitor/usage/quota/limit")
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

    let tokens_limit = limits.iter().find(|l| l.get("type").and_then(|t| t.as_str()) == Some("TOKENS_LIMIT"));
    let mcp_tools = limits.iter().find(|l| l.get("type").and_then(|t| t.as_str()) == Some("TIME_LIMIT"));

    let mut windows = serde_json::Map::new();

    if let Some(tl) = tokens_limit {
        let ws = resolve_window_seconds(tl);
        let reset_at = tl.get("nextResetTime").and_then(normalize_timestamp);
        let used_percent = tl.get("percentage").and_then(|v| v.as_f64());
        windows.insert(
            "Tokens".into(),
            to_usage_window(ToUsageWindowArgs {
                used_percent,
                window_seconds: ws,
                reset_at,
                value_label: None,
            }),
        );
    }

    if let Some(mt) = mcp_tools {
        let month_seconds: i64 = 30 * 24 * 60 * 60;
        let reset_at = mt.get("nextResetTime").and_then(normalize_timestamp);
        let used_percent = mt.get("percentage").and_then(|v| v.as_f64());
        windows.insert(
            "MCP Tools".into(),
            to_usage_window(ToUsageWindowArgs {
                used_percent,
                window_seconds: Some(month_seconds),
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

pub fn fetch_quota() -> Value {
    block_on(fetch_quota_async())
}

pub fn fetch_quota_sync() -> Value {
    fetch_quota()
}

pub use fetch_quota as fetch_zhipuai_quota;
