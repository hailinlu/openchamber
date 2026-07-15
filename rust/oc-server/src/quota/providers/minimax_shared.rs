//! `quota/providers/minimax-shared.js` 移植。
//!
//! 工厂模式: 不同 URL 集合对应不同的 MiniMax Coding Plan 端点。

use serde_json::{json, Value};

use crate::opencode::auth::read_auth_file;
use crate::quota::providers::not_configured;
use crate::quota::utils::auth::{get_auth_entry, normalize_auth_entry};
use crate::quota::utils::formatters::{build_result, to_usage_window, BuildResultArgs, ToUsageWindowArgs};
use crate::quota::utils::transformers::{to_number, to_timestamp};
use crate::quota::{block_on, http_client};

/// Weekly 状态码 — 3 表示该 window 对当前 plan 不适用。
const WINDOW_STATUS_INACTIVE: f64 = 3.0;

const TEXT_MODELS: &[&str] = &["general", "chat", "text"];

#[derive(Clone)]
pub struct MiniMaxProviderConfig {
    pub provider_id: &'static str,
    pub provider_name: &'static str,
    pub aliases: &'static [&'static str],
    pub token_plan_url: &'static str,
    pub coding_plan_url: &'static str,
}

pub struct MiniMaxProvider {
    pub provider_id: &'static str,
    pub provider_name: &'static str,
    pub aliases: &'static [&'static str],
    pub is_configured: Box<dyn Fn() -> bool + Send + Sync>,
    pub fetch_quota: Box<dyn Fn() -> Value + Send + Sync>,
}

fn read_api_key(aliases: &'static [&'static str]) -> Option<String> {
    let auth = read_auth_file().unwrap_or_default();
    let entry = normalize_auth_entry(get_auth_entry(&auth, aliases));
    entry
        .as_ref()
        .and_then(|v| v.get("key").or_else(|| v.get("token")).and_then(|x| x.as_str()))
        .map(|s| s.to_string())
}

/// `pickChatModel` — priority:
///   1. m3 (model_name matches `/^minimax-m/i` AND interval total > 0)
///   2. text-ish name
///   3. has `current_interval_remaining_percent`
///   4. first
pub fn pick_chat_model(model_remains: Option<&Vec<Value>>) -> Option<Value> {
    let arr = model_remains?;
    if arr.is_empty() {
        return None;
    }

    // 1. m3 candidate
    let m3 = arr.iter().find(|m| {
        let name = m.get("model_name").and_then(|v| v.as_str()).unwrap_or("");
        regex_match_ci("^minimax-m", name)
            && to_number(m.get("current_interval_total_count").unwrap_or(&Value::Null))
                .map(|n| n > 0.0)
                .unwrap_or(false)
    });
    if let Some(c) = m3 {
        return Some(c.clone());
    }

    // 2. text-ish
    let text = arr.iter().find(|m| {
        let name = m
            .get("model_name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_lowercase();
        TEXT_MODELS.iter().any(|t| name == *t)
    });
    if let Some(c) = text {
        return Some(c.clone());
    }

    // 3. has remaining_percent
    let percent = arr.iter().find(|m| {
        m.get("current_interval_remaining_percent")
            .and_then(|v| v.as_f64())
            .is_some()
    });
    if let Some(c) = percent {
        return Some(c.clone());
    }

    // 4. first
    Some(arr[0].clone())
}

fn regex_match_ci(pattern: &str, s: &str) -> bool {
    use regex::Regex;
    let re = match Regex::new(&format!("(?i){pattern}")) {
        Ok(r) => r,
        Err(_) => return false,
    };
    re.is_match(s)
}

fn coerce_percent(value: Option<&Value>) -> Option<f64> {
    let n = to_number(value?)?;
    Some(n.clamp(0.0, 100.0))
}

fn is_window_active(status: Option<&Value>) -> bool {
    let value = match status {
        Some(v) => v,
        None => return true,
    };
    let Some(n) = to_number(value) else {
        return true;
    };
    n != WINDOW_STATUS_INACTIVE
}

fn calculate_window_seconds(start_at: Option<i64>, reset_at: Option<i64>, remains_time_ms: Option<f64>) -> Option<i64> {
    if let (Some(s), Some(r)) = (start_at, reset_at) {
        if r > s {
            return Some((r - s) / 1000);
        }
    }
    if let Some(ms) = remains_time_ms {
        if ms > 0.0 {
            return Some((ms / 1000.0) as i64);
        }
    }
    None
}

#[derive(Default, Clone, Copy)]
struct Usage {
    interval_used_percent: Option<f64>,
    interval_window_seconds: Option<i64>,
    interval_reset_at: Option<i64>,
    weekly_used_percent: Option<f64>,
    weekly_window_seconds: Option<i64>,
    weekly_reset_at: Option<i64>,
}

fn calculate_usage(model: &Value, is_token_plan: bool) -> Usage {
    let interval_total = to_number(model.get("current_interval_total_count").unwrap_or(&Value::Null));
    let interval_usage_raw = to_number(model.get("current_interval_usage_count").unwrap_or(&Value::Null));
    let interval_start_at = to_timestamp(model.get("start_time").unwrap_or(&Value::Null));
    let interval_reset_at = to_timestamp(model.get("end_time").unwrap_or(&Value::Null));
    let interval_remains_time = to_number(model.get("remains_time").unwrap_or(&Value::Null));
    let interval_remaining_percent = coerce_percent(model.get("current_interval_remaining_percent"));

    let weekly_total = to_number(model.get("current_weekly_total_count").unwrap_or(&Value::Null));
    let weekly_usage_raw = to_number(model.get("current_weekly_usage_count").unwrap_or(&Value::Null));
    let weekly_start_at = to_timestamp(model.get("weekly_start_time").unwrap_or(&Value::Null));
    let weekly_reset_at = to_timestamp(model.get("weekly_end_time").unwrap_or(&Value::Null));
    let weekly_remains_time = to_number(model.get("weekly_remains_time").unwrap_or(&Value::Null));
    let weekly_remaining_percent = coerce_percent(model.get("current_weekly_remaining_percent"));

    let interval_used_percent = if let Some(p) = interval_remaining_percent {
        Some(100.0 - p)
    } else if let (Some(total), Some(usage)) = (interval_total, interval_usage_raw) {
        if total > 0.0 {
            let used = if is_token_plan {
                (total - usage).max(0.0)
            } else {
                usage
            };
            Some((used / total * 100.0).clamp(0.0, 100.0))
        } else {
            None
        }
    } else {
        None
    };

    let weekly_used_percent = if let Some(p) = weekly_remaining_percent {
        Some(100.0 - p)
    } else if let (Some(total), Some(usage)) = (weekly_total, weekly_usage_raw) {
        if total > 0.0 {
            let used = if is_token_plan {
                (total - usage).max(0.0)
            } else {
                usage
            };
            Some((used / total * 100.0).clamp(0.0, 100.0))
        } else {
            None
        }
    } else {
        None
    };

    Usage {
        interval_used_percent,
        interval_window_seconds: calculate_window_seconds(interval_start_at, interval_reset_at, interval_remains_time),
        interval_reset_at,
        weekly_used_percent,
        weekly_window_seconds: calculate_window_seconds(weekly_start_at, weekly_reset_at, weekly_remains_time),
        weekly_reset_at,
    }
}

fn is_usable_payload(payload: &Value) -> bool {
    if let Some(base) = payload.get("base_resp") {
        if base.get("status_code").and_then(|v| v.as_i64()) != Some(0) {
            return false;
        }
    }
    let rems = payload.get("model_remains").and_then(|v| v.as_array());
    matches!(rems, Some(r) if !r.is_empty())
}

async fn fetch_endpoint(url: &str, api_key: &str) -> Option<Value> {
    let client = http_client();
    let resp = match client
        .get(url)
        .bearer_auth(api_key)
        .header("Content-Type", "application/json")
        .send()
        .await
    {
        Ok(r) => r,
        Err(_) => return None,
    };
    if !resp.status().is_success() {
        return None;
    }
    let payload: Value = resp.json().await.ok()?;
    is_usable_payload(&payload).then_some(payload)
}

/// 工厂 — 创建带 URL 路由的 provider。
pub fn create_minimax_coding_plan_provider(cfg: MiniMaxProviderConfig) -> MiniMaxProvider {
    let pid = cfg.provider_id;
    let pname = cfg.provider_name;
    let aliases = cfg.aliases;
    let token_url = cfg.token_plan_url;
    let coding_url = cfg.coding_plan_url;

    let is_configured: Box<dyn Fn() -> bool + Send + Sync> = {
        let aliases_captured = aliases;
        Box::new(move || read_api_key(aliases_captured).is_some())
    };

    let fetch_quota: Box<dyn Fn() -> Value + Send + Sync> = Box::new(move || {
        let token_url = token_url.clone();
        let coding_url = coding_url.clone();
        block_on(async move {
            let Some(api_key) = read_api_key(aliases) else {
                return not_configured(pid, pname);
            };

            let mut is_token_plan = true;
            let payload = match fetch_endpoint(token_url, &api_key).await {
                Some(p) => p,
                None => match fetch_endpoint(coding_url, &api_key).await {
                    Some(p) => {
                        is_token_plan = false;
                        p
                    }
                    None => {
                        return build_result(BuildResultArgs {
                            provider_id: pid,
                            provider_name: pname,
                            ok: false,
                            configured: true,
                            usage: None,
                            error: Some("API returned no usable quota data"),
                        });
                    }
                },
            };

            let model_remains_arr = payload
                .get("model_remains")
                .and_then(|v| v.as_array())
                .cloned();
            let Some(model) = pick_chat_model(model_remains_arr.as_ref()) else {
                return build_result(BuildResultArgs {
                    provider_id: pid,
                    provider_name: pname,
                    ok: false,
                    configured: true,
                    usage: None,
                    error: Some("No model quota data available"),
                });
            };

            let u = calculate_usage(&model, is_token_plan);

            let mut windows = serde_json::Map::new();
            windows.insert(
                "5h".into(),
                to_usage_window(ToUsageWindowArgs {
                    used_percent: u.interval_used_percent,
                    window_seconds: u.interval_window_seconds,
                    reset_at: u.interval_reset_at,
                    value_label: None,
                }),
            );

            // 仅在 plan 支持且 weekly data 存在时添加。
            let weekly_active = is_window_active(model.get("current_weekly_status"));
            let has_weekly_data = weekly_active
                && (coerce_percent(model.get("current_weekly_remaining_percent")).is_some()
                    || to_number(model.get("current_weekly_total_count").unwrap_or(&Value::Null))
                        .map(|n| n > 0.0)
                        .unwrap_or(false));

            if has_weekly_data {
                windows.insert(
                    "weekly".into(),
                    to_usage_window(ToUsageWindowArgs {
                        used_percent: u.weekly_used_percent,
                        window_seconds: u.weekly_window_seconds,
                        reset_at: u.weekly_reset_at,
                        value_label: None,
                    }),
                );
            }

            build_result(BuildResultArgs {
                provider_id: pid,
                provider_name: pname,
                ok: true,
                configured: true,
                usage: Some(json!({"windows": Value::Object(windows)})),
                error: None,
            })
        })
    });

    MiniMaxProvider {
        provider_id: pid,
        provider_name: pname,
        aliases,
        is_configured,
        fetch_quota,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pick_chat_model_priority() {
        // 1. m3 first
        let arr = vec![
            json!({"model_name": "other"}),
            json!({"model_name": "minimax-m", "current_interval_total_count": 5}),
        ];
        let m = pick_chat_model(Some(&arr)).unwrap();
        assert_eq!(m["model_name"], "minimax-m");

        // 2. text-ish
        let arr = vec![json!({"model_name": "general"}), json!({"model_name": "x"})];
        let m = pick_chat_model(Some(&arr)).unwrap();
        assert_eq!(m["model_name"], "general");

        // 3. has remaining_percent
        let arr = vec![
            json!({"model_name": "x"}),
            json!({"model_name": "y", "current_interval_remaining_percent": 50}),
        ];
        let m = pick_chat_model(Some(&arr)).unwrap();
        assert_eq!(m["model_name"], "y");

        // 4. first
        let arr = vec![json!({"model_name": "z"})];
        let m = pick_chat_model(Some(&arr)).unwrap();
        assert_eq!(m["model_name"], "z");

        // empty -> None
        assert!(pick_chat_model(Some(&vec![])).is_none());
        assert!(pick_chat_model(None).is_none());
    }

    #[test]
    fn is_window_active_default_true() {
        assert!(is_window_active(None));
        assert!(is_window_active(Some(&json!(0))));
        assert!(!is_window_active(Some(&json!(3))));
    }

    #[test]
    fn calculate_window_seconds_uses_reset_minus_start() {
        let s = Some(1_700_000_000_000);
        let r = Some(1_700_003_600_000);
        assert_eq!(calculate_window_seconds(s, r, None), Some(3600));
    }

    #[test]
    fn calculate_window_seconds_uses_remains_time() {
        assert_eq!(calculate_window_seconds(None, None, Some(60_000.0)), Some(60));
    }

    #[test]
    fn is_usable_payload_validates_status_code_zero() {
        let p = json!({"base_resp": {"status_code": 0}, "model_remains": [{}]});
        assert!(is_usable_payload(&p));
        let p = json!({"base_resp": {"status_code": 1}, "model_remains": [{}]});
        assert!(!is_usable_payload(&p));
    }

    #[test]
    fn coerce_percent_clamps() {
        assert_eq!(coerce_percent(Some(&json!(120.0))), Some(100.0));
        assert_eq!(coerce_percent(Some(&json!(-5.0))), Some(0.0));
        assert_eq!(coerce_percent(Some(&json!(42.0))), Some(42.0));
    }

    #[test]
    fn m3_with_zero_total_is_not_picked() {
        let arr = vec![
            json!({"model_name": "minimax-m", "current_interval_total_count": 0}),
            json!({"model_name": "general"}),
        ];
        let m = pick_chat_model(Some(&arr)).unwrap();
        // m3 跳过 → 命中 text-ish (general)
        assert_eq!(m["model_name"], "general");
    }

    #[test]
    fn factory_builds_provider_with_correct_id() {
        let cfg = MiniMaxProviderConfig {
            provider_id: "test",
            provider_name: "Test",
            aliases: &["test"],
            token_plan_url: "https://example.com/tp",
            coding_plan_url: "https://example.com/cp",
        };
        let p = create_minimax_coding_plan_provider(cfg);
        assert_eq!(p.provider_id, "test");
        assert_eq!(p.provider_name, "Test");
    }
}
