//! `utils/transformers.js` 移植:
//!   - asObject / asNonEmptyString
//!   - toNumber / toTimestamp / normalizeTimestamp
//!   - resolveWindowSeconds (ZAI token window map)
//!   - resolveWindowLabel (days/hours/seconds)

use serde_json::Value;

/// `value && typeof value === 'object'` 的类型守卫。
pub fn as_object(value: &Value) -> Option<&serde_json::Map<String, Value>> {
    value.as_object()
}

/// 修剪空白后返回非空字符串,否则 None。
pub fn as_non_empty_string(value: &Value) -> Option<String> {
    value.as_str().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// 安全转为有限数字 (或 null)。对 string 也做 `Number(value)` 转换。
pub fn to_number(value: &Value) -> Option<f64> {
    if let Some(n) = value.as_f64() {
        if n.is_finite() {
            return Some(n);
        }
        return None;
    }
    if let Some(s) = value.as_str() {
        if let Ok(n) = s.parse::<f64>() {
            if n.is_finite() {
                return Some(n);
            }
        }
        return None;
    }
    None
}

/// 智能 timestamp: 数字 < 1e12 当作秒级 unix 时间戳,否则毫秒。
/// 字符串走 `Date.parse`。
pub fn to_timestamp(value: &Value) -> Option<i64> {
    if value.is_null() {
        return None;
    }
    if let Some(n) = value.as_f64() {
        if !n.is_finite() {
            return None;
        }
        let n = n as i64;
        return Some(if n < 1_000_000_000_000 { n * 1000 } else { n });
    }
    if let Some(s) = value.as_str() {
        let parsed = chrono::DateTime::parse_from_rfc3339(s)
            .map(|d| d.timestamp_millis())
            .ok()
            .or_else(|| {
                // 退化方案: RFC2822 风格
                chrono::DateTime::parse_from_rfc2822(s)
                    .ok()
                    .map(|d| d.timestamp_millis())
            });
        return parsed;
    }
    None
}

/// 仅当为数字时 normalize (秒→毫秒);否则 null。
pub fn normalize_timestamp(value: &Value) -> Option<i64> {
    let n = value.as_f64()?;
    if !n.is_finite() {
        return None;
    }
    let n = n as i64;
    Some(if n < 1_000_000_000_000 { n * 1000 } else { n })
}

/// ZAI token window 的 `unit → 秒` 映射。
const ZAI_TOKEN_WINDOW_SECONDS: &[(i64, i64)] = &[(3, 3600)];

/// 从 limit 对象的 (unit, number) 计算 window 秒数。
pub fn resolve_window_seconds(limit: &Value) -> Option<i64> {
    let number = limit.get("number")?.as_f64()?;
    if !number.is_finite() {
        return None;
    }
    let unit = limit.get("unit")?.as_i64()?;
    let unit_seconds = ZAI_TOKEN_WINDOW_SECONDS.iter().find(|(u, _)| *u == unit)?.1;
    Some(unit_seconds * (number as i64))
}

/// 把 window 秒数转成友好 label (7d / 24h / 3600s)。
pub fn resolve_window_label(window_seconds: Option<i64>) -> String {
    let Some(ws) = window_seconds else {
        return "tokens".to_string();
    };
    if ws > 0 && ws % 86400 == 0 {
        let days = ws / 86400;
        return if days == 7 { "weekly".to_string() } else { format!("{days}d") };
    }
    if ws > 0 && ws % 3600 == 0 {
        return format!("{}h", ws / 3600);
    }
    format!("{ws}s")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn to_number_handles_number_strings() {
        assert_eq!(to_number(&json!(42)), Some(42.0));
        assert_eq!(to_number(&json!("3.15")), Some(3.15));
        assert_eq!(to_number(&json!("abc")), None);
        assert_eq!(to_number(&json!(null)), None);
        assert_eq!(to_number(&json!(true)), None);
    }

    #[test]
    fn to_number_rejects_infinity() {
        // JSON parser typically rejects infinity, but let's guard anyway
        assert_eq!(to_number(&json!("Infinity")), None);
    }

    #[test]
    fn to_timestamp_seconds_vs_ms() {
        // 1700000000 seconds → ms
        let secs = json!(1700000000u64);
        let ms = to_timestamp(&secs).unwrap();
        assert_eq!(ms, 1_700_000_000_000);
        // 1.7e12 (already ms)
        let ms_already = json!(1_700_000_000_000u64);
        assert_eq!(to_timestamp(&ms_already).unwrap(), 1_700_000_000_000);
    }

    #[test]
    fn to_timestamp_iso_string() {
        let ts = to_timestamp(&json!("2025-01-01T00:00:00Z")).unwrap();
        assert!(ts > 0);
    }

    #[test]
    fn to_timestamp_null_returns_none() {
        assert_eq!(to_timestamp(&json!(null)), None);
    }

    #[test]
    fn normalize_timestamp_seconverts() {
        // 值 < 1e12 被视为秒, ×1000 → 毫秒 (与 Node normalizeTimestamp 一致)
        assert_eq!(normalize_timestamp(&json!(100u64)).unwrap(), 100_000);
        assert_eq!(normalize_timestamp(&json!(100_000u64)).unwrap(), 100_000_000);
    }

    #[test]
    fn normalize_timestamp_non_number_is_none() {
        assert_eq!(normalize_timestamp(&json!("x")), None);
        assert_eq!(normalize_timestamp(&json!(null)), None);
    }

    #[test]
    fn resolve_window_seconds_zai_unit_3_hour() {
        // unit=3, number=5 → 5h
        let limit = json!({"unit": 3, "number": 5});
        assert_eq!(resolve_window_seconds(&limit), Some(5 * 3600));
    }

    #[test]
    fn resolve_window_seconds_unknown_unit() {
        let limit = json!({"unit": 5, "number": 10});
        assert_eq!(resolve_window_seconds(&limit), None);
    }

    #[test]
    fn resolve_window_seconds_missing() {
        assert_eq!(resolve_window_seconds(&json!({})), None);
    }

    #[test]
    fn resolve_window_label_weekly() {
        assert_eq!(resolve_window_label(Some(7 * 86400)), "weekly");
        assert_eq!(resolve_window_label(Some(14 * 86400)), "14d");
        assert_eq!(resolve_window_label(Some(5 * 3600)), "5h");
        assert_eq!(resolve_window_label(Some(3600)), "1h");
        assert_eq!(resolve_window_label(Some(120)), "120s");
    }

    #[test]
    fn resolve_window_label_none() {
        assert_eq!(resolve_window_label(None), "tokens");
    }

    #[test]
    fn as_object_works() {
        assert!(as_object(&json!({})).is_some());
        assert!(as_object(&json!("x")).is_none());
        assert!(as_object(&json!(null)).is_none());
    }

    #[test]
    fn as_non_empty_string_filters() {
        assert_eq!(as_non_empty_string(&json!("hi")), Some("hi".to_string()));
        assert_eq!(as_non_empty_string(&json!("")), None);
        assert_eq!(as_non_empty_string(&json!("   ")), None);
        assert_eq!(as_non_empty_string(&json!("  hi  ")), Some("hi".to_string()));
        assert_eq!(as_non_empty_string(&json!(42)), None);
    }
}
