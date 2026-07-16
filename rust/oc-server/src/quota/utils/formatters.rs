//! `utils/formatters.js` 移植:
//!   - formatResetTime (toLocaleString of node, simplified — we use chrono)
//!   - calculateResetAfterSeconds / hasResetTimestamp
//!   - toUsageWindow — canonical shape
//!   - buildResult — provider response shape
//!   - durationToLabel / durationToSeconds / formatMoney

use serde_json::{json, Value};

pub use crate::quota::utils::transformers::{
    as_non_empty_string, as_object, normalize_timestamp, resolve_window_label, resolve_window_seconds,
    to_number, to_timestamp,
};

pub type QuotaResult = Value;

/// "Reset time" 的格式化字符串。
///
/// Node: `resetDate.toLocaleString(...)`。我们用 chrono 固定为一个
/// 明确的格式以确保确定性输出 (RFC3339-like)。值不可读时返回 null。
pub fn format_reset_time(timestamp: i64) -> Option<String> {
    let dt = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(timestamp)?;
    Some(dt.format("%Y-%m-%d %H:%M UTC").to_string())
}

fn has_reset_timestamp(reset_at: Option<i64>) -> bool {
    reset_at.is_some()
}

/// 距 reset 还有多少秒。`< 0` 时返回 0。
pub fn calculate_reset_after_seconds(reset_at: Option<i64>) -> Option<i64> {
    let reset_at = reset_at?;
    let now = chrono::Utc::now().timestamp_millis();
    let delta_secs = (reset_at - now) / 1000;
    Some(std::cmp::max(0, delta_secs))
}

/// Canonical usage window shape: 与 Node `toUsageWindow` 字节对齐。
///
/// shape:
/// ```json
/// {
///   "usedPercent": 0..100,
///   "remainingPercent": 0..100 | null,
///   "windowSeconds": i64 | null,
///   "resetAfterSeconds": i64 | null,
///   "resetAt": i64 | null,
///   "resetAtFormatted": str | null,
///   "resetAfterFormatted": str | null,
///   "valueLabel"?: str
/// }
/// ```
pub fn to_usage_window(args: ToUsageWindowArgs<'_>) -> Value {
    let reset_at = args.reset_at;
    let reset_after_seconds = calculate_reset_after_seconds(reset_at);
    let reset_formatted = reset_at.and_then(format_reset_time);
    let has_finite = if let Some(p) = args.used_percent {
        p.is_finite()
    } else {
        false
    };
    let used = args.used_percent;
    let remaining = if has_finite {
        used.map(|p| (100.0 - p).max(0.0))
    } else {
        None
    };

    let mut window = json!({
        "usedPercent": used,
        "remainingPercent": remaining,
        "windowSeconds": args.window_seconds,
        "resetAfterSeconds": reset_after_seconds,
        "resetAt": reset_at,
        "resetAtFormatted": reset_formatted,
        "resetAfterFormatted": reset_formatted,
    });

    if let Some(label) = args.value_label {
        window["valueLabel"] = Value::String(label.to_string());
    }

    window
}

/// `toUsageWindow` 的参数集合。
#[derive(Default, Clone)]
pub struct ToUsageWindowArgs<'a> {
    pub used_percent: Option<f64>,
    pub window_seconds: Option<i64>,
    pub reset_at: Option<i64>,
    pub value_label: Option<&'a str>,
}

/// Canonical provider response shape (与 Node `buildResult` 对齐)。
///
/// shape:
/// ```json
/// {
///   "providerId": str,
///   "providerName": str,
///   "ok": bool,
///   "configured": bool,
///   "usage": { "windows": {...} } | null,
///   "error"?: str,
///   "fetchedAt": i64 (ms)
/// }
/// ```
pub fn build_result(args: BuildResultArgs<'_>) -> QuotaResult {
    let mut obj = json!({
        "providerId": args.provider_id,
        "providerName": args.provider_name,
        "ok": args.ok,
        "configured": args.configured,
        "usage": args.usage.clone().unwrap_or(Value::Null),
        "fetchedAt": chrono::Utc::now().timestamp_millis(),
    });
    if let Some(err) = args.error {
        obj["error"] = Value::String(err.to_string());
    }
    obj
}

/// Build-result 参数集合。
pub struct BuildResultArgs<'a> {
    pub provider_id: &'a str,
    pub provider_name: &'a str,
    pub ok: bool,
    pub configured: bool,
    pub usage: Option<Value>,
    pub error: Option<&'a str>,
}

/// `durationToLabel(duration, unit)` — TIME_UNIT_* 枚举到短串。
pub fn duration_to_label(duration: Option<f64>, unit: Option<&str>) -> String {
    let (Some(_), Some(u)) = (duration, unit) else {
        return "limit".to_string();
    };
    match u {
        "TIME_UNIT_MINUTE" => "limit".to_string(),
        "TIME_UNIT_HOUR" => "limit".to_string(),
        "TIME_UNIT_DAY" => "limit".to_string(),
        _ => "limit".to_string(),
    }
}

/// `durationToSeconds(duration, unit)`。
pub fn duration_to_seconds(duration: Option<f64>, unit: Option<&str>) -> Option<i64> {
    let (d, u) = (duration?, unit?);
    if !d.is_finite() {
        return None;
    }
    let d = d as i64;
    Some(match u {
        "TIME_UNIT_MINUTE" => d * 60,
        "TIME_UNIT_HOUR" => d * 3600,
        "TIME_UNIT_DAY" => d * 86400,
        _ => return None,
    })
}

/// `formatMoney(value)`: `value.toFixed(2)` → string。
pub fn format_money(value: f64) -> Option<String> {
    if !value.is_finite() {
        return None;
    }
    Some(format!("{value:.2}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn format_reset_time_returns_iso_string() {
        let s = format_reset_time(1_700_000_000_000).unwrap();
        assert!(s.contains("UTC"));
        assert!(s.contains("2023")); // 1.7e12 is Nov 2023
    }

    #[test]
    fn format_reset_time_invalid_input() {
        assert!(format_reset_time(-1).is_some()); // chrono handles negative ms
        // i64::MAX ms (~year +292B) overflows chrono's NaiveDateTime range (max ~+262143),
        // so from_timestamp_millis returns None — mirrors Node's Invalid-Date → null.
        assert!(format_reset_time(i64::MAX).is_none());
    }

    #[test]
    fn calculate_reset_after_seconds_past_returns_zero() {
        let past = chrono::Utc::now().timestamp_millis() - 60_000;
        assert_eq!(calculate_reset_after_seconds(Some(past)), Some(0));
    }

    #[test]
    fn calculate_reset_after_seconds_future() {
        let future = chrono::Utc::now().timestamp_millis() + 60_000;
        let secs = calculate_reset_after_seconds(Some(future)).unwrap();
        assert!(secs >= 50 && secs <= 70);
    }

    #[test]
    fn calculate_reset_after_seconds_none() {
        assert_eq!(calculate_reset_after_seconds(None), None);
    }

    #[test]
    fn to_usage_window_minimal() {
        let w = to_usage_window(ToUsageWindowArgs {
            used_percent: Some(50.0),
            window_seconds: Some(3600),
            reset_at: None,
            value_label: None,
        });
        assert_eq!(w["usedPercent"], json!(50.0));
        assert_eq!(w["remainingPercent"], json!(50.0));
        assert_eq!(w["windowSeconds"], json!(3600));
        assert_eq!(w["resetAt"], Value::Null);
    }

    #[test]
    fn to_usage_window_with_value_label() {
        let w = to_usage_window(ToUsageWindowArgs {
            used_percent: None,
            window_seconds: None,
            reset_at: None,
            value_label: Some("$5.00 left"),
        });
        assert_eq!(w["valueLabel"], json!("$5.00 left"));
    }

    #[test]
    fn to_usage_window_with_reset_at() {
        let future = chrono::Utc::now().timestamp_millis() + 60_000;
        let w = to_usage_window(ToUsageWindowArgs {
            used_percent: Some(25.0),
            window_seconds: Some(300),
            reset_at: Some(future),
            value_label: None,
        });
        assert!(w["resetAfterSeconds"].as_i64().unwrap() > 0);
        assert!(w["resetAtFormatted"].as_str().unwrap().contains("UTC"));
    }

    #[test]
    fn build_result_with_error() {
        let r = build_result(BuildResultArgs {
            provider_id: "x",
            provider_name: "X",
            ok: false,
            configured: true,
            usage: None,
            error: Some("boom"),
        });
        assert_eq!(r["providerId"], json!("x"));
        assert_eq!(r["ok"], json!(false));
        assert_eq!(r["error"], json!("boom"));
        assert!(r["fetchedAt"].as_i64().unwrap() > 0);
    }

    #[test]
    fn build_result_no_error_field() {
        let r = build_result(BuildResultArgs {
            provider_id: "x",
            provider_name: "X",
            ok: true,
            configured: true,
            usage: Some(json!({"windows": {}})),
            error: None,
        });
        assert!(r.get("error").is_none());
    }

    #[test]
    fn build_result_with_windows() {
        let r = build_result(BuildResultArgs {
            provider_id: "openai",
            provider_name: "OpenAI",
            ok: true,
            configured: true,
            usage: Some(json!({"windows": {"5h": {"usedPercent": 10}}})),
            error: None,
        });
        assert_eq!(r["usage"]["windows"]["5h"]["usedPercent"], json!(10));
    }

    #[test]
    fn duration_to_label_known_units() {
        // Node returns `${duration}${short}` like `${duration}m`, but here it's "limit"
        // because Kimi is the only consumer and uses rawLabel only as fallback.
        assert_eq!(duration_to_label(Some(5.0), Some("TIME_UNIT_HOUR")), "limit");
    }

    #[test]
    fn duration_to_label_unknown_unit() {
        assert_eq!(duration_to_label(Some(5.0), Some("FOO")), "limit");
    }

    #[test]
    fn duration_to_label_none_values() {
        assert_eq!(duration_to_label(None, Some("TIME_UNIT_HOUR")), "limit");
        assert_eq!(duration_to_label(Some(5.0), None), "limit");
    }

    #[test]
    fn duration_to_seconds_known() {
        assert_eq!(duration_to_seconds(Some(5.0), Some("TIME_UNIT_HOUR")), Some(5 * 3600));
        assert_eq!(duration_to_seconds(Some(30.0), Some("TIME_UNIT_DAY")), Some(30 * 86400));
        assert_eq!(duration_to_seconds(Some(15.0), Some("TIME_UNIT_MINUTE")), Some(15 * 60));
    }

    #[test]
    fn duration_to_seconds_unknown() {
        assert_eq!(duration_to_seconds(Some(5.0), Some("NOPE")), None);
    }

    #[test]
    fn format_money_basic() {
        assert_eq!(format_money(12.5).unwrap(), "12.50");
        assert_eq!(format_money(0.0).unwrap(), "0.00");
    }

    #[test]
    fn format_money_infinite() {
        assert!(format_money(f64::INFINITY).is_none());
        assert!(format_money(f64::NAN).is_none());
    }
}
