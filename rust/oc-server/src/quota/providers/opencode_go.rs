//! `quota/providers/opencode-go.js` 移植。

use regex::Regex;
use serde_json::{json, Value};

use crate::quota::credentials::read_managed_credential;
use crate::quota::providers::{fetch_error, not_configured};
use crate::quota::utils::formatters::{build_result, to_usage_window, ToUsageWindowArgs};
use crate::quota::{block_on, http_client};

pub const PROVIDER_ID: &str = "opencode-go";
pub const PROVIDER_NAME: &str = "OpenCode Go";

const PATTERNS: &[(&str, &str)] = &[
    ("5h", "rollingUsage"),
    ("weekly", "weeklyUsage"),
    ("monthly", "monthlyUsage"),
];

fn capture_number(name: &str, body: &str) -> Option<f64> {
    // 匹配: "name": 123.45  或  "name"="123.45"
    let pattern = format!(r#"["']?{name}["']?\s*:\s*["']?(-?\d+(?:\.\d+)?)"#);
    let re = Regex::new(&pattern).ok()?;
    let caps = re.captures(body)?;
    let s = caps.get(1)?.as_str();
    let v: f64 = s.parse().ok()?;
    if v.is_finite() {
        Some(v)
    } else {
        None
    }
}

/// 把 HTML 中 4 种变体的 `\"` 还原为 `"`。
fn normalize(html: &str) -> String {
    html.replace("&quot;", "\"")
        .replace("&#34;", "\"")
        .replace("\\u0022", "\"")
        .replace("\\\"", "\"")
}

/// 解析 OpenCode Go 仪表盘 HTML 中的 3 个 window 使用率。
///
/// 返回 `{ "5h": <UsageWindow>, "weekly": <UsageWindow>, "monthly": <UsageWindow> }`,
/// 任何 window 在 HTML 中缺失时不会出现在返回 map 中。
pub fn parse_open_code_go_usage(html: &str, now_ms: Option<i64>) -> serde_json::Map<String, Value> {
    let now = now_ms.unwrap_or_else(|| chrono::Utc::now().timestamp_millis());
    let mut windows = serde_json::Map::new();

    if html.is_empty() {
        return windows;
    }

    let normalized = normalize(html);

    for (key, field) in PATTERNS {
        let escaped = regex::escape(field);
        // 匹配 {"field":<可选 \$R[N]=\s*> { body }}; body 不含 {}
        let pattern = format!(
            r#"["']?{escaped}["']?\s*:\s*(?:\$R\[\d+\]\s*=\s*)?\{{([^{{}}]*)\}}"#
        );
        let re = match Regex::new(&pattern) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let cap = match re.captures(&normalized) {
            Some(c) => c,
            None => continue,
        };
        let body = match cap.get(1) {
            Some(b) => b.as_str(),
            None => continue,
        };
        let used_percent = capture_number("usagePercent", body);
        let reset_in_sec = capture_number("resetInSec", body);
        let (Some(up), Some(rs)) = (used_percent, reset_in_sec) else {
            continue;
        };
        let clamped = up.clamp(0.0, 100.0);
        let reset_at = Some(now + (rs.max(0.0) as i64) * 1000);
        windows.insert(
            (*key).to_string(),
            to_usage_window(ToUsageWindowArgs {
                used_percent: Some(clamped),
                window_seconds: None,
                reset_at,
                value_label: None,
            }),
        );
    }

    windows
}

/// 实际 GET OpenCode Go 仪表盘并解析 usage windows。
///
/// 返回 windows map,非 build_result 形状 — 调用方负责包装。
pub async fn fetch_open_code_go_usage_inner(credential: &Value) -> Result<serde_json::Map<String, Value>, String> {
    let workspace_id = credential
        .get("workspaceId")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "opencode-go credential missing workspaceId".to_string())?;
    let auth_cookie = credential
        .get("authCookie")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "opencode-go credential missing authCookie".to_string())?;

    let url = format!(
        "https://opencode.ai/workspace/{}/go",
        url_escape(workspace_id)
    );

    let client = http_client();
    let resp = client
        .get(&url)
        .header("Accept", "text/html,application/xhtml+xml")
        .header("Cookie", format!("auth={auth_cookie}"))
        .header("User-Agent", "OpenChamber quota provider")
        .send()
        .await
        .map_err(|e| e.to_string())?;

    let status = resp.status().as_u16();
    if status == 401 || status == 403 || (300..400).contains(&status) {
        return Err("OpenCode Go authentication failed".to_string());
    }
    if !resp.status().is_success() {
        return Err(format!("OpenCode Go dashboard returned HTTP {status}"));
    }
    let body = resp.text().await.map_err(|e| e.to_string())?;
    let windows = parse_open_code_go_usage(&body, None);
    if windows.is_empty() {
        return Err("OpenCode Go usage data could not be parsed".to_string());
    }
    Ok(windows)
}

fn url_escape(s: &str) -> String {
    // 不要引入新依赖,用 reqwest::Url 提供的编码
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            _ => format!("%{:02X}", c as u32),
        })
        .collect()
}

pub fn is_configured() -> bool {
    read_managed_credential(PROVIDER_ID).is_some()
}

pub async fn fetch_quota_async() -> Value {
    let Some(credential) = read_managed_credential(PROVIDER_ID) else {
        return not_configured(PROVIDER_ID, PROVIDER_NAME);
    };
    match fetch_open_code_go_usage_inner(&credential).await {
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_extracts_all_three_windows() {
        let html = r#"
            rollingUsage: { usagePercent: 25, resetInSec: 1200 },
            weeklyUsage: { usagePercent: 40, resetInSec: 86400 },
            monthlyUsage: { usagePercent: 60, resetInSec: 2592000 }
        "#;
        let now = 1_700_000_000_000_i64;
        let windows = parse_open_code_go_usage(html, Some(now));
        assert_eq!(windows.len(), 3);
        assert_eq!(windows["5h"]["usedPercent"], json!(25.0));
        assert_eq!(windows["weekly"]["usedPercent"], json!(40.0));
        assert_eq!(windows["monthly"]["usedPercent"], json!(60.0));
        assert_eq!(windows["5h"]["resetAt"], json!(now + 1200 * 1000));
    }

    #[test]
    fn parse_handles_quoted_keys_and_unicode_escapes() {
        let html = r#"
            "rollingUsage":{"usagePercent":"30.5","resetInSec":3600}
            "weeklyUsage":\u007B"usagePercent":15,"resetInSec":43200\u007D
        "#;
        let windows = parse_open_code_go_usage(html, Some(0));
        assert!(windows.contains_key("5h"));
        assert!(windows.contains_key("weekly"));
    }

    #[test]
    fn parse_handles_html_entity_encoded_quotes() {
        let html = "&quot;rollingUsage&quot;: { &quot;usagePercent&quot;: 12, &quot;resetInSec&quot;: 300 }";
        let windows = parse_open_code_go_usage(html, Some(0));
        assert!(windows.contains_key("5h"));
    }

    #[test]
    fn parse_clamps_percent() {
        let html = "rollingUsage: { usagePercent: 250, resetInSec: 600 }";
        let windows = parse_open_code_go_usage(html, Some(0));
        assert_eq!(windows["5h"]["usedPercent"], json!(100.0));
    }

    #[test]
    fn parse_empty_returns_empty() {
        let windows = parse_open_code_go_usage("", Some(0));
        assert!(windows.is_empty());
    }

    #[test]
    fn parse_missing_field_skips_window() {
        let html = "rollingUsage: { usagePercent: 25, resetInSec: 1200 }";
        let windows = parse_open_code_go_usage(html, Some(0));
        assert_eq!(windows.len(), 1);
        assert!(!windows.contains_key("weekly"));
    }
}
