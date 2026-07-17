//! `quota/providers/ollama-cloud.js` 移植。

use regex::Regex;
use serde_json::{json, Value};

use crate::quota::credentials::read_managed_credential;
use crate::quota::providers::{fetch_error, not_configured};
use crate::quota::utils::formatters::{build_result, to_number, to_usage_window, ToUsageWindowArgs};
use crate::quota::{block_on, http_client};

pub const PROVIDER_ID: &str = "ollama-cloud";
pub const PROVIDER_NAME: &str = "Ollama Cloud";

/// 解析 ollama.com/settings HTML 中的使用情况。
///
/// Returns `{ "session": <w>, "weekly": <w>, "premium": <w> }` — 任一字段缺失则不会出现在结果中。
pub fn parse_ollama_settings_html(html: &str) -> serde_json::Map<String, Value> {
    let mut windows = serde_json::Map::new();

    if let Ok(re) = Regex::new(r"(?i)Session\s+usage[^0-9]*([0-9.]+)%") {
        if let Some(c) = re.captures(html) {
            if let Some(m) = c.get(1) {
                let v = to_number(&Value::String(m.as_str().to_string()));
                windows.insert(
                    "session".into(),
                    to_usage_window(ToUsageWindowArgs {
                        used_percent: v,
                        window_seconds: None,
                        reset_at: None,
                        value_label: None,
                    }),
                );
            }
        }
    }
    if let Ok(re) = Regex::new(r"(?i)Weekly\s+usage[^0-9]*([0-9.]+)%") {
        if let Some(c) = re.captures(html) {
            if let Some(m) = c.get(1) {
                let v = to_number(&Value::String(m.as_str().to_string()));
                windows.insert(
                    "weekly".into(),
                    to_usage_window(ToUsageWindowArgs {
                        used_percent: v,
                        window_seconds: None,
                        reset_at: None,
                        value_label: None,
                    }),
                );
            }
        }
    }
    if let Ok(re) = Regex::new(r"(?i)Premium[^0-9]*([0-9]+)\s*/\s*([0-9]+)") {
        if let Some(c) = re.captures(html) {
            let used = c.get(1).and_then(|m| m.as_str().parse::<i64>().ok());
            let total = c.get(2).and_then(|m| m.as_str().parse::<i64>().ok());
            let used_percent = match (used, total) {
                (Some(u), Some(t)) if t > 0 => Some(((u as f64 / t as f64) * 100.0).min(100.0)),
                _ => None,
            };
            let value_label = match (used, total) {
                (u, t) => format!("{} / {}", u.unwrap_or(0), t.unwrap_or(0)),
            };
            windows.insert(
                "premium".into(),
                to_usage_window(ToUsageWindowArgs {
                    used_percent,
                    window_seconds: None,
                    reset_at: None,
                    value_label: Some(value_label.as_str()),
                }),
            );
        }
    }
    windows
}

pub async fn fetch_ollama_cloud_usage_inner(credential: &Value) -> Result<serde_json::Map<String, Value>, String> {
    let cookie = credential
        .get("cookie")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "ollama-cloud credential missing cookie".to_string())?
        .to_string();

    let client = http_client();
    let resp = client
        .get("https://ollama.com/settings")
        .header("Cookie", cookie)
        .header("User-Agent", "GridForge quota provider")
        .send()
        .await
        .map_err(|e| e.to_string())?;

    let status = resp.status().as_u16();
    if status == 401 || status == 403 || (300..400).contains(&status) {
        return Err("Ollama Cloud authentication failed".to_string());
    }
    if !resp.status().is_success() {
        return Err(format!("Ollama Cloud returned HTTP {status}"));
    }
    let body = resp.text().await.map_err(|e| e.to_string())?;
    let windows = parse_ollama_settings_html(&body);
    if windows.is_empty() {
        return Err("Ollama Cloud usage data could not be parsed".to_string());
    }
    Ok(windows)
}

pub fn is_configured() -> bool {
    read_managed_credential(PROVIDER_ID).is_some()
}

pub async fn fetch_quota_async() -> Value {
    let Some(credential) = read_managed_credential(PROVIDER_ID) else {
        return not_configured(PROVIDER_ID, PROVIDER_NAME);
    };
    match fetch_ollama_cloud_usage_inner(&credential).await {
        Ok(windows) => build_result(crate::quota::utils::formatters::BuildResultArgs {
            provider_id: PROVIDER_ID,
            provider_name: PROVIDER_NAME,
            ok: true,
            configured: true,
            usage: Some(json!({"windows": Value::Object(windows)})),
            error: None,
        }),
        Err(e) => fetch_error(PROVIDER_ID, PROVIDER_NAME, &e),
    }
}

pub fn fetch_quota() -> Value {
    block_on(fetch_quota_async())
}

pub fn fetch_quota_sync() -> Value {
    fetch_quota()
}

pub use fetch_quota as fetch_ollama_cloud_quota;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_all_three_windows() {
        let html = r#"
            Session usage 12.5% this week
            Weekly usage 75%
            Premium 120 / 500
        "#;
        let windows = parse_ollama_settings_html(html);
        assert!(windows.contains_key("session"));
        assert!(windows.contains_key("weekly"));
        assert!(windows.contains_key("premium"));
    }

    #[test]
    fn parse_session_only() {
        let html = "Session usage 25%";
        let windows = parse_ollama_settings_html(html);
        assert!(windows.contains_key("session"));
        assert!(!windows.contains_key("weekly"));
    }

    #[test]
    fn parse_premium_uses_used_total() {
        let html = "Premium 30 / 60";
        let windows = parse_ollama_settings_html(html);
        let p = &windows["premium"];
        assert_eq!(p["valueLabel"], json!("30 / 60"));
        assert_eq!(p["usedPercent"], json!(50.0));
    }

    #[test]
    fn parse_empty_html() {
        let windows = parse_ollama_settings_html("");
        assert!(windows.is_empty());
    }
}
