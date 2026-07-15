//! Google provider - transforms。

use serde_json::{json, Value};

use crate::quota::utils::formatters::{to_usage_window, ToUsageWindowArgs};
use crate::quota::utils::transformers::{as_non_empty_string, to_number};

const GOOGLE_FIVE_HOUR_WINDOW_SECONDS: i64 = 5 * 60 * 60;
const GOOGLE_DAILY_WINDOW_SECONDS: i64 = 24 * 60 * 60;

#[derive(Clone)]
pub struct ParsedRefreshToken {
    pub refresh_token: Option<String>,
    pub project_id: Option<String>,
    pub managed_project_id: Option<String>,
}

/// 解析 refresh token 的 `token|projectId|managedProjectId` 格式。
pub fn parse_google_refresh_token(value: &Value) -> Option<ParsedRefreshToken> {
    let s = as_non_empty_string(value)?;
    let parts: Vec<&str> = s.split('|').collect();
    let raw_token = parts.first().copied().unwrap_or("");
    let raw_project = parts.get(1).copied().unwrap_or("");
    let raw_managed = parts.get(2).copied().unwrap_or("");
    Some(ParsedRefreshToken {
        refresh_token: as_non_empty_string(&Value::String(raw_token.to_string())),
        project_id: as_non_empty_string(&Value::String(raw_project.to_string())),
        managed_project_id: as_non_empty_string(&Value::String(raw_managed.to_string())),
    })
}

/// 决定 window 的 label 和 seconds — gemini 总是 daily,antigravity 看剩余秒数。
pub fn resolve_google_window(source_id: &str, reset_at: Option<i64>) -> (&'static str, i64) {
    if source_id == "antigravity" {
        if let Some(r) = reset_at {
            let now = chrono::Utc::now().timestamp_millis();
            let secs = ((r - now) / 1000).max(0);
            if secs > 10 * 60 * 60 {
                return ("daily", GOOGLE_DAILY_WINDOW_SECONDS);
            }
            return ("5h", GOOGLE_FIVE_HOUR_WINDOW_SECONDS);
        }
    }
    ("daily", GOOGLE_DAILY_WINDOW_SECONDS)
}

/// Bucket → `{ "scope/modelId": { windows: { label: <w> } } }`。
pub fn transform_quota_bucket(bucket: &Value, source_id: &str) -> Option<serde_json::Map<String, Value>> {
    let model_id = as_non_empty_string(bucket.get("modelId")?)?;
    let scoped_name = if model_id.starts_with(&format!("{source_id}/")) {
        model_id
    } else {
        format!("{source_id}/{model_id}")
    };
    let remaining_fraction = to_number(bucket.get("remainingFraction").unwrap_or(&Value::Null));
    let remaining_percent = remaining_fraction.map(|n| (n * 100.0).round());
    let used_percent = remaining_percent.map(|p: f64| (100.0 - p).max(0.0));
    let reset_at_ms = bucket.get("resetTime").and_then(|v| v.as_f64()).map(|n| n as i64);
    let (label, seconds) = resolve_google_window(source_id, reset_at_ms);

    let window = to_usage_window(ToUsageWindowArgs {
        used_percent,
        window_seconds: Some(seconds),
        reset_at: reset_at_ms,
        value_label: None,
    });

    let mut map = serde_json::Map::new();
    let mut windows = serde_json::Map::new();
    windows.insert(label.to_string(), window);
    map.insert(scoped_name, json!({ "windows": Value::Object(windows) }));
    Some(map)
}

/// model payload → `{ "scope/model": { windows: { label: <w> } } }`.
pub fn transform_model_data(model_name: &str, model_data: &Value, source_id: &str) -> Option<serde_json::Map<String, Value>> {
    let scoped_name = if model_name.starts_with(&format!("{source_id}/")) {
        model_name.to_string()
    } else {
        format!("{source_id}/{model_name}")
    };
    let remaining_fraction = to_number(model_data.get("quotaInfo")
        .and_then(|q| q.get("remainingFraction"))
        .unwrap_or(&Value::Null));
    let remaining_percent = remaining_fraction.map(|n| (n * 100.0).round());
    let used_percent = remaining_percent.map(|p: f64| (100.0 - p).max(0.0));
    let reset_at_ms = model_data.get("quotaInfo")
        .and_then(|q| q.get("resetTime"))
        .and_then(|v| v.as_f64())
        .map(|n| n as i64);
    let (label, seconds) = resolve_google_window(source_id, reset_at_ms);

    let window = to_usage_window(ToUsageWindowArgs {
        used_percent,
        window_seconds: Some(seconds),
        reset_at: reset_at_ms,
        value_label: None,
    });

    let mut map = serde_json::Map::new();
    let mut windows = serde_json::Map::new();
    windows.insert(label.to_string(), window);
    map.insert(scoped_name, json!({ "windows": Value::Object(windows) }));
    Some(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn resolve_google_window_gemini_daily() {
        assert_eq!(resolve_google_window("gemini", None), ("daily", 86400));
    }

    #[test]
    fn resolve_google_window_antigravity_high_remaining_is_daily() {
        let far_future = chrono::Utc::now().timestamp_millis() + 100_000_000;
        assert_eq!(resolve_google_window("antigravity", Some(far_future)), ("daily", 86400));
    }

    #[test]
    fn resolve_google_window_antigravity_short_remaining_is_5h() {
        let near = chrono::Utc::now().timestamp_millis() + 60_000;
        assert_eq!(resolve_google_window("antigravity", Some(near)), ("5h", 18000));
    }

    #[test]
    fn parse_google_refresh_token_3_pipe_segments() {
        let parsed = parse_google_refresh_token(&json!("rt|pp|mpp")).unwrap();
        assert_eq!(parsed.refresh_token.as_deref(), Some("rt"));
        assert_eq!(parsed.project_id.as_deref(), Some("pp"));
        assert_eq!(parsed.managed_project_id.as_deref(), Some("mpp"));
    }

    #[test]
    fn parse_google_refresh_token_no_pipe() {
        let parsed = parse_google_refresh_token(&json!("plain-token")).unwrap();
        assert_eq!(parsed.refresh_token.as_deref(), Some("plain-token"));
        assert_eq!(parsed.project_id, None);
        assert_eq!(parsed.managed_project_id, None);
    }

    #[test]
    fn transform_quota_bucket_scopes_name() {
        let bucket = json!({
            "modelId": "gemini-2.5-pro",
            "remainingFraction": 0.42,
            "resetTime": null
        });
        let m = transform_quota_bucket(&bucket, "gemini").unwrap();
        let key = m.keys().next().unwrap();
        assert_eq!(key, "gemini/gemini-2.5-pro");
    }
}
