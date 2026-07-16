//! Schedule parsing + next-run calculation — 移植自 Node
//! `scheduled-tasks/runtime.js` 中三个纯函数 (`parseScheduledCommandPrompt`,
//! `computeNextRunAt`, `formatScheduledSessionTitle`)。
//!
//! 时区处理用 `chrono-tz` (内嵌 IANA tzdata), 正确支持非 UTC 时区。

use chrono::{DateTime, Datelike, TimeZone, Timelike};
use serde_json::Value;

/// chrono-tz 的时区类型别名。
type Tz = chrono_tz::Tz;

/// 任务标题最大长度 (suffix ` yyyy-MM-dd HH:mm` 前缀预留 16 chars)。
pub const TASK_TITLE_MAX_LENGTH: usize = 120;

/// 5s slack — 防止刚刚过点的 timestamp 被立即触发。
pub const TASK_DUE_SLACK_MS: i64 = 5_000;

/// `HH:MM` 正则 (00:00 - 23:59)。
fn hhmm_pattern() -> &'static regex::Regex {
    static PATTERN: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| {
        regex::Regex::new(r"^([01]\d|2[0-3]):([0-5]\d)$").unwrap()
    });
    &PATTERN
}

/// 解析 `HH:MM` 为 (hour, minute), 失败返回 None。
pub fn parse_time_parts(time: &str) -> Option<(u32, u32)> {
    let caps = hhmm_pattern().captures(time.trim())?;
    let h: u32 = caps.get(1)?.as_str().parse().ok()?;
    let m: u32 = caps.get(2)?.as_str().parse().ok()?;
    Some((h, m))
}

/// 把 (hour, minute) 套到 `base` 上, 返回新 DateTime (时区沿用 `base`)。
fn apply_time_to_date<Tz: TimeZone>(base: &DateTime<Tz>, hour: u32, minute: u32) -> Option<DateTime<Tz>> {
    let mut new_dt = base.clone();
    new_dt = new_dt.with_hour(hour)?;
    new_dt = new_dt.with_minute(minute)?;
    new_dt = new_dt.with_second(0)?;
    new_dt = new_dt.with_nanosecond(0)?;
    Some(new_dt)
}

/// 从 schedule 解析 time-of-day 列表 (优先 `times: []`, fallback `time: "HH:MM"`)。
/// 返回 sorted unique。
pub fn resolve_schedule_times(schedule: &Value) -> Vec<String> {
    let mut times: Vec<String> = Vec::new();
    if let Some(arr) = schedule.get("times").and_then(Value::as_array) {
        for t in arr {
            if let Some(s) = t.as_str() {
                if hhmm_pattern().is_match(s.trim()) {
                    times.push(s.trim().to_string());
                }
            }
        }
    }
    if times.is_empty() {
        if let Some(s) = schedule.get("time").and_then(Value::as_str) {
            let s = s.trim();
            if hhmm_pattern().is_match(s) {
                times.push(s.to_string());
            }
        }
    }
    times.sort();
    times.dedup();
    times
}

/// chrono `weekday()` 返回 `1=Mon..7=Sun`。Node 用 `0=Sun..6=Sat`。换算为 0-based 周日=0。
pub fn weekday_as_zero_based<Tz: TimeZone>(dt: &DateTime<Tz>) -> i64 {
    // chrono `weekday()`: Mon=1..Sun=7
    // Desired:           Sun=0, Mon=1..Sat=6
    // wd % 7: Mon=1..Sat=6, Sun=7%7=0 ✓
    (dt.weekday().number_from_monday() % 7) as i64
}

/// 解析 `/cmd args` 类型的 slash-command prompt。
///
/// 返回 `(command_name_without_slash, arguments_joined)`。
///
/// 规则 (与 Node 一致):
/// - 非 string 或 trim 后不以 `/` 开头 → `None`
/// - 只取第一行 (按 `\n` 或 `\r\n` 切), 在第一个空白处 split
/// - 空 command (例如 `/`) → `None`
pub fn parse_scheduled_command_prompt(prompt: &str) -> Option<(String, String)> {
    if prompt.is_empty() {
        return None;
    }
    let trimmed = prompt.trim();
    if !trimmed.starts_with('/') {
        return None;
    }
    let first_line = trimmed.split('\n').next().unwrap_or("");
    let first_line = first_line.split('\r').next().unwrap_or("");
    let mut parts = first_line.split_whitespace();
    let head = parts.next()?;
    let command_name = head.trim_start_matches('/').trim().to_string();
    if command_name.is_empty() {
        return None;
    }
    let arguments: String = parts.collect::<Vec<_>>().join(" ").trim().to_string();
    Some((command_name, arguments))
}

/// 计算任务下一次触发时间 (毫秒 epoch)。
///
/// 严格对齐 Node `computeNextRunAt`:
/// - task.enabled == false → None
/// - schedule.kind ∈ {daily, weekly, once, cron}
/// - cron (无依赖) 返回 None + TODO (见下)
pub fn compute_next_run_at(task: &Value, now_ms: i64) -> Option<i64> {
    let enabled = task.get("enabled").and_then(Value::as_bool).unwrap_or(false);
    if !enabled {
        return None;
    }
    let schedule = task.get("schedule")?;
    if !schedule.is_object() {
        return None;
    }

    let tz_name = schedule
        .get("timezone")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let kind = schedule.get("kind").and_then(Value::as_str).unwrap_or("");

    if kind == "daily" {
        let times = resolve_schedule_times(schedule);
        if times.is_empty() {
            return None;
        }
        let now_local = local_now_with_tz(now_ms, tz_name)?;
        let min_allowed = add_ms(&now_local, TASK_DUE_SLACK_MS)?;

        for time in &times {
            let (h, m) = parse_time_parts(time)?;
            let candidate = apply_time_to_date(&now_local, h, m)?;
            if candidate.timestamp_millis() > min_allowed.timestamp_millis() {
                return Some(candidate.timestamp_millis());
            }
        }
        // 明日第一个时间
        let tomorrow = now_local + chrono::Duration::days(1);
        let (h, m) = parse_time_parts(&times[0])?;
        let first_tomorrow = apply_time_to_date(&tomorrow, h, m)?;
        Some(first_tomorrow.timestamp_millis())
    } else if kind == "weekly" {
        let weekdays = schedule.get("weekdays").and_then(Value::as_array)?;
        if weekdays.is_empty() {
            return None;
        }
        let times = resolve_schedule_times(schedule);
        if times.is_empty() {
            return None;
        }
        let set: Vec<i64> = weekdays
            .iter()
            .filter_map(|v| v.as_i64().or_else(|| v.as_u64().map(|u| u as i64)))
            .collect();
        let now_local = local_now_with_tz(now_ms, tz_name)?;
        let min_allowed = add_ms(&now_local, TASK_DUE_SLACK_MS)?;

        for day_offset in 0..=14i64 {
            let day_candidate = now_local + chrono::Duration::days(day_offset);
            let wd = weekday_as_zero_based(&day_candidate);
            if !set.contains(&wd) {
                continue;
            }
            for time in &times {
                let (h, m) = parse_time_parts(time)?;
                let with_time = apply_time_to_date(&day_candidate, h, m)?;
                if with_time.timestamp_millis() > min_allowed.timestamp_millis() {
                    return Some(with_time.timestamp_millis());
                }
            }
        }
        None
    } else if kind == "once" {
        let date = schedule.get("date").and_then(Value::as_str)?;
        let time = schedule.get("time").and_then(Value::as_str)?;
        let (h, m) = parse_time_parts(time)?;
        let mut parts = date.splitn(3, '-');
        let y: i32 = parts.next()?.parse().ok()?;
        let mo: u32 = parts.next()?.parse().ok()?;
        let d: u32 = parts.next()?.parse().ok()?;
        let zoned = parse_in_tz(y, mo, d, h, m, tz_name)?;
        // min_allowed = now + slack, 在同一时区下计算
        let now_local = local_now_with_tz(now_ms, tz_name)?;
        let min_allowed = add_ms(&now_local, TASK_DUE_SLACK_MS)?;
        if zoned.timestamp_millis() <= min_allowed.timestamp_millis() {
            return None;
        }
        Some(zoned.timestamp_millis())
    } else if kind == "cron" {
        // Node 用 cron-parser (5-field: 分 时 日 月 周)。
        // Rust `cron` crate 用 7-field (秒 分 时 日 月 周 年)。
        // 对齐策略: prepend "0 " (秒=0) + append " *" (年通配) → 7-field。
        let cron_expr = schedule.get("cron").and_then(Value::as_str)?;
        let seven_field = format!("0 {cron_expr} *");
        let cron_sched: cron::Schedule = seven_field.parse().ok()?;
        let tz = parse_tz(tz_name).unwrap_or(chrono_tz::UTC);
        let now_dt = local_now_with_tz(now_ms, tz_name).unwrap_or_else(|| {
            chrono::Utc
                .timestamp_millis_opt(now_ms)
                .single()
                .unwrap_or_else(chrono::Utc::now)
                .with_timezone(&tz)
        });
        let min_allowed = now_dt + chrono::Duration::milliseconds(TASK_DUE_SLACK_MS);
        // `after()` 从 min_allowed 开始迭代, 找第一个满足 cron schedule 的时间。
        cron_sched
            .after(&min_allowed)
            .next()
            .map(|dt| dt.timestamp_millis())
    } else {
        None
    }
}

/// 格式化 session title `<name> yyyy-MM-dd HH:mm`, 最多 120 chars。
pub fn format_scheduled_session_title(task: &Value, now_ms: i64) -> String {
    let tz_name = task
        .get("schedule")
        .and_then(|s| s.get("timezone"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let dt = local_now_with_tz(now_ms, tz_name).unwrap_or_else(|| {
        let utc = chrono::Utc.timestamp_millis_opt(now_ms).single().unwrap_or_else(chrono::Utc::now);
        utc.with_timezone(&chrono_tz::UTC)
    });
    let stamp = dt.format("%Y-%m-%d %H:%M").to_string();
    let task_name = task
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("Scheduled task")
        .to_string();
    let suffix = format!(" {}", stamp);
    let max_name = TASK_TITLE_MAX_LENGTH.saturating_sub(suffix.len());
    let max_name = max_name.max(1);
    let trimmed_name = if task_name.chars().count() > max_name {
        task_name.chars().take(max_name).collect::<String>()
    } else {
        task_name
    };
    format!("{}{}", trimmed_name, suffix)
}

// =========================================================================
// tz-aware helpers — 使用 chrono-tz 正确处理 IANA 时区
// =========================================================================

/// 解析 IANA 时区名 ("UTC", "America/New_York", "Asia/Shanghai" …) → Tz。
/// 无效/缺失 → None。
fn parse_tz(name: Option<&str>) -> Option<Tz> {
    let n = name?.trim();
    if n.is_empty() {
        return None;
    }
    n.parse::<Tz>().ok()
}

/// now_ms → tz-aware DateTime (如果 tz_name 是合法 IANA zone)。
fn local_now_with_tz(now_ms: i64, tz_name: Option<&str>) -> Option<DateTime<Tz>> {
    let tz = parse_tz(tz_name)?;
    let secs = now_ms.div_euclid(1000);
    let nsec = (now_ms.rem_euclid(1000) as u32) * 1_000_000;
    let utc = chrono::Utc.timestamp_opt(secs, nsec).single()?;
    Some(utc.with_timezone(&tz))
}

/// 在指定时区解析 y-mo-d h:m:s → tz-aware DateTime。
fn parse_in_tz(y: i32, mo: u32, d: u32, h: u32, m: u32, tz_name: Option<&str>) -> Option<DateTime<Tz>> {
    let tz = parse_tz(tz_name).unwrap_or(chrono_tz::UTC);
    tz.with_ymd_and_hms(y, mo, d, h, m, 0).single()
}

fn add_ms<Tz: TimeZone>(dt: &DateTime<Tz>, ms: i64) -> Option<DateTime<Tz>> {
    Some(dt.clone() + chrono::Duration::milliseconds(ms))
}

// =========================================================================
// 测试
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn utc_millis(y: i32, mo: u32, d: u32, h: u32, m: u32, s: u32) -> i64 {
        use chrono::TimeZone;
        chrono::Utc.with_ymd_and_hms(y, mo, d, h, m, s).single().unwrap().timestamp_millis()
    }

    #[test]
    fn parse_time_parts_basic() {
        assert_eq!(parse_time_parts("09:30"), Some((9, 30)));
        assert_eq!(parse_time_parts("23:59"), Some((23, 59)));
        assert_eq!(parse_time_parts("00:00"), Some((0, 0)));
        assert_eq!(parse_time_parts("24:00"), None);
        assert_eq!(parse_time_parts("9:30"), None);
        assert_eq!(parse_time_parts(""), None);
    }

    #[test]
    fn resolve_schedule_times_prefers_array() {
        let s = json!({ "times": ["09:30", "08:00"], "time": "10:00" });
        let mut v = resolve_schedule_times(&s);
        v.sort();
        assert_eq!(v, vec!["08:00".to_string(), "09:30".to_string()]);
        let s2 = json!({ "time": "10:00" });
        assert_eq!(resolve_schedule_times(&s2), vec!["10:00".to_string()]);
        let s3 = json!({});
        assert!(resolve_schedule_times(&s3).is_empty());
    }

    #[test]
    fn weekday_zero_based_mapping() {
        use chrono::TimeZone;
        // 2025-01-05 = Sunday → weekday 0
        let dt = chrono_tz::UTC.with_ymd_and_hms(2025, 1, 5, 12, 0, 0).unwrap();
        assert_eq!(weekday_as_zero_based(&dt), 0);
        // 2025-01-06 = Monday → 1
        let dt = chrono_tz::UTC.with_ymd_and_hms(2025, 1, 6, 12, 0, 0).unwrap();
        assert_eq!(weekday_as_zero_based(&dt), 1);
    }

    // parse_scheduled_command_prompt
    #[test]
    fn parse_slash_command_basic() {
        let (cmd, args) = parse_scheduled_command_prompt("/review src/components").unwrap();
        assert_eq!(cmd, "review");
        assert_eq!(args, "src/components");
    }

    #[test]
    fn parse_slash_command_no_args() {
        let (cmd, args) = parse_scheduled_command_prompt("/build").unwrap();
        assert_eq!(cmd, "build");
        assert_eq!(args, "");
    }

    #[test]
    fn parse_slash_command_first_line_only() {
        let (cmd, args) =
            parse_scheduled_command_prompt("/lint src\nsecond line ignored").unwrap();
        assert_eq!(cmd, "lint");
        assert_eq!(args, "src");
    }

    #[test]
    fn parse_slash_command_returns_none_for_non_string_or_no_slash() {
        assert!(parse_scheduled_command_prompt("Summarize open issues").is_none());
        assert!(parse_scheduled_command_prompt("/").is_none());
        assert!(parse_scheduled_command_prompt("   plain text").is_none());
    }

    // compute_next_run_at — daily
    #[test]
    fn compute_daily_single_time_today() {
        // 2025-01-01 08:00 UTC → next 09:30 same day (>= now+5s)
        let now = utc_millis(2025, 1, 1, 8, 0, 0);
        let task = json!({
            "enabled": true,
            "schedule": { "kind": "daily", "times": ["09:30"], "timezone": "UTC" }
        });
        let next = compute_next_run_at(&task, now).unwrap();
        assert_eq!(next, utc_millis(2025, 1, 1, 9, 30, 0));
    }

    #[test]
    fn compute_daily_picks_nearest_of_multiple() {
        // 09:20 UTC, times [09:15, 09:45, 18:00] → 09:45 (past 09:15 < now+5s)
        let now = utc_millis(2025, 1, 1, 9, 20, 0);
        let task = json!({
            "enabled": true,
            "schedule": { "kind": "daily", "times": ["09:15", "09:45", "18:00"], "timezone": "UTC" }
        });
        let next = compute_next_run_at(&task, now).unwrap();
        assert_eq!(next, utc_millis(2025, 1, 1, 9, 45, 0));
    }

    #[test]
    fn compute_daily_rolls_to_tomorrow_when_past_all() {
        let now = utc_millis(2025, 1, 1, 23, 0, 0);
        let task = json!({
            "enabled": true,
            "schedule": { "kind": "daily", "times": ["09:00"], "timezone": "UTC" }
        });
        let next = compute_next_run_at(&task, now).unwrap();
        assert_eq!(next, utc_millis(2025, 1, 2, 9, 0, 0));
    }

    #[test]
    fn compute_weekly_basic() {
        // Mon 2025-01-06 10:00 UTC, weekdays [1, 3], times ["09:00"] → Wed
        let now = utc_millis(2025, 1, 6, 10, 0, 0);
        let task = json!({
            "enabled": true,
            "schedule": { "kind": "weekly", "weekdays": [1, 3], "times": ["09:00"], "timezone": "UTC" }
        });
        let next = compute_next_run_at(&task, now).unwrap();
        assert_eq!(next, utc_millis(2025, 1, 8, 9, 0, 0));
    }

    #[test]
    fn compute_weekly_sunday_zero_index() {
        // Sun 2025-01-05 08:00 UTC, weekdays [0]
        let now = utc_millis(2025, 1, 5, 8, 0, 0);
        let task = json!({
            "enabled": true,
            "schedule": { "kind": "weekly", "weekdays": [0], "times": ["10:00"], "timezone": "UTC" }
        });
        let next = compute_next_run_at(&task, now).unwrap();
        assert_eq!(next, utc_millis(2025, 1, 5, 10, 0, 0));
    }

    #[test]
    fn compute_once_future() {
        let now = utc_millis(2026, 4, 15, 10, 0, 0);
        let task = json!({
            "enabled": true,
            "schedule": { "kind": "once", "date": "2026-04-16", "time": "13:30", "timezone": "UTC" }
        });
        let next = compute_next_run_at(&task, now).unwrap();
        assert_eq!(next, utc_millis(2026, 4, 16, 13, 30, 0));
    }

    #[test]
    fn compute_once_past_returns_none() {
        let now = utc_millis(2026, 4, 16, 14, 0, 0);
        let task = json!({
            "enabled": true,
            "schedule": { "kind": "once", "date": "2026-04-16", "time": "13:30", "timezone": "UTC" }
        });
        assert_eq!(compute_next_run_at(&task, now), None);
    }

    #[test]
    fn compute_disabled_returns_none() {
        let now = utc_millis(2025, 1, 1, 8, 0, 0);
        let task = json!({
            "enabled": false,
            "schedule": { "kind": "daily", "times": ["09:30"], "timezone": "UTC" }
        });
        assert_eq!(compute_next_run_at(&task, now), None);
    }

    #[test]
    fn compute_invalid_time_returns_none() {
        let now = utc_millis(2025, 1, 1, 8, 0, 0);
        let task = json!({
            "enabled": true,
            "schedule": { "kind": "daily", "times": ["bad"], "timezone": "UTC" }
        });
        // Falls through to "明日" branch — but `times.is_empty()` → None
        assert_eq!(compute_next_run_at(&task, now), None);
    }

    #[test]
    fn compute_cron_next_run() {
        // */5 分钟, UTC, now=08:00:00 → next=08:05:00
        let now = utc_millis(2025, 1, 1, 8, 0, 0);
        let task = json!({
            "enabled": true,
            "schedule": { "kind": "cron", "cron": "*/5 * * * *", "timezone": "UTC" }
        });
        let next = compute_next_run_at(&task, now).unwrap();
        assert_eq!(next, utc_millis(2025, 1, 1, 8, 5, 0));
    }

    #[test]
    fn compute_cron_daily_at_9() {
        // 0 9 * * * = 每天 09:00, now=08:00 → today 09:00
        let now = utc_millis(2025, 1, 1, 8, 0, 0);
        let task = json!({
            "enabled": true,
            "schedule": { "kind": "cron", "cron": "0 9 * * *", "timezone": "UTC" }
        });
        let next = compute_next_run_at(&task, now).unwrap();
        assert_eq!(next, utc_millis(2025, 1, 1, 9, 0, 0));
    }

    #[test]
    fn compute_cron_invalid_returns_none() {
        let now = utc_millis(2025, 1, 1, 8, 0, 0);
        let task = json!({
            "enabled": true,
            "schedule": { "kind": "cron", "cron": "not valid", "timezone": "UTC" }
        });
        assert_eq!(compute_next_run_at(&task, now), None);
    }

    #[test]
    fn compute_daily_non_utc_timezone() {
        // America/New_York (UTC-5 in Jan), daily 09:00 EST
        // now = 2025-01-01 14:00 UTC = 09:00 EST → today 09:00 is < now+5s
        // → next is tomorrow 09:00 EST = 2025-01-02 14:00 UTC
        let now = utc_millis(2025, 1, 1, 14, 0, 0);
        let task = json!({
            "enabled": true,
            "schedule": { "kind": "daily", "times": ["09:00"], "timezone": "America/New_York" }
        });
        let next = compute_next_run_at(&task, now).unwrap();
        // 2025-01-02 09:00 EST = 14:00 UTC
        assert_eq!(next, utc_millis(2025, 1, 2, 14, 0, 0));
    }

    #[test]
    fn compute_daily_asia_shanghai_timezone() {
        // Asia/Shanghai (UTC+8), daily 09:00 CST
        // now = 2025-01-01 00:00 UTC = 08:00 CST → 09:00 CST same day = 01:00 UTC
        let now = utc_millis(2025, 1, 1, 0, 0, 0);
        let task = json!({
            "enabled": true,
            "schedule": { "kind": "daily", "times": ["09:00"], "timezone": "Asia/Shanghai" }
        });
        let next = compute_next_run_at(&task, now).unwrap();
        // 2025-01-01 09:00 CST = 01:00 UTC
        assert_eq!(next, utc_millis(2025, 1, 1, 1, 0, 0));
    }

    #[test]
    fn compute_once_non_utc_timezone() {
        // once 2025-06-15 10:00 Europe/Paris (UTC+2 summer)
        // = 08:00 UTC; now = 2025-06-15 06:00 UTC → future, returns
        let now = utc_millis(2025, 6, 15, 6, 0, 0);
        let task = json!({
            "enabled": true,
            "schedule": { "kind": "once", "date": "2025-06-15", "time": "10:00", "timezone": "Europe/Paris" }
        });
        let next = compute_next_run_at(&task, now).unwrap();
        // 10:00 CEST = 08:00 UTC
        assert_eq!(next, utc_millis(2025, 6, 15, 8, 0, 0));
    }

    #[test]
    fn format_title_truncates_long_name() {
        let long_name = "A".repeat(200);
        let task = json!({ "name": long_name, "schedule": { "timezone": "UTC" } });
        let title = format_scheduled_session_title(&task, utc_millis(2025, 3, 10, 7, 5, 0));
        assert!(title.len() <= TASK_TITLE_MAX_LENGTH);
        assert!(title.ends_with("2025-03-10 07:05"));
    }

    #[test]
    fn format_title_short_name() {
        let task = json!({ "name": "Morning Sync", "schedule": { "timezone": "UTC" } });
        let title = format_scheduled_session_title(&task, utc_millis(2025, 3, 10, 7, 5, 0));
        assert_eq!(title, "Morning Sync 2025-03-10 07:05");
    }

    #[test]
    fn format_title_missing_name_uses_default() {
        let task = json!({});
        let title = format_scheduled_session_title(&task, utc_millis(2025, 1, 1, 0, 0, 0));
        assert!(title.starts_with("Scheduled task "));
    }
}
