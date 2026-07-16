//! Task 验证/clamp/normalize 纯函数 — 移植自 Node
//! `projects/project-config.js` 的全部 normalize helpers。
//!
//! 这些函数确保写入磁盘的 task 满足 schema 约束:
//! - name/prompt/cron 长度 clamp
//! - schedule.kind 必须是 daily/weekly/once/cron
//! - execution.providerID/modelID/prompt 必填
//! - timezone 必须是合法 IANA zone
//! - cron 表达式必须可解析
//! - state 时间戳 round + lastError clamp + lastStatus 归一化
//! - task.id immutability (已存在的 task 不能改 id)

use serde_json::{json, Value};

// =========================================================================
// 常量 (与 Node project-config.js 完全一致)
// =========================================================================

pub const PROJECT_CONFIG_VERSION: u32 = 1;
pub const MAX_TASK_NAME_LENGTH: usize = 80;
pub const MAX_TASK_PROMPT_LENGTH: usize = 20_000;
pub const MAX_CRON_LENGTH: usize = 200;
pub const MAX_LAST_ERROR_LENGTH: usize = 2_000;

// =========================================================================
// 基础 helpers
// =========================================================================

/// trim 后非空才返回 Some, 否则 None。
pub fn as_non_empty_string(value: &Value) -> Option<String> {
    value.as_str().map(|s| s.trim()).filter(|s| !s.is_empty()).map(|s| s.to_string())
}

/// 字符串截断到 maxLength (按 char count, 与 JS `slice(0, n)` 一致)。
pub fn clamp_length(value: &str, max_length: usize) -> String {
    if value.chars().count() > max_length {
        value.chars().take(max_length).collect()
    } else {
        value.to_string()
    }
}

// =========================================================================
// status / time / date / weekdays
// =========================================================================

/// 归一化 lastStatus: running|success|error|idle, default idle。
pub fn normalize_status(value: &Value) -> String {
    match value.as_str() {
        Some("running") | Some("success") | Some("error") | Some("idle") => {
            value.as_str().unwrap().to_string()
        }
        _ => "idle".to_string(),
    }
}

/// HH:MM (00:00–23:59) 校验。
fn hhmm_pattern() -> &'static regex::Regex {
    static PATTERN: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| {
        regex::Regex::new(r"^([01]\d|2[0-3]):([0-5]\d)$").unwrap()
    });
    &PATTERN
}

/// 解析 `HH:MM`, 失败返回 None。
pub fn normalize_time_value(value: &Value) -> Option<String> {
    let s = as_non_empty_string(value)?;
    if hhmm_pattern().is_match(&s) {
        Some(s)
    } else {
        None
    }
}

/// YYYY-MM-DD 校验 + round-trip (确保不是 "2024-02-31" 这种非法日期)。
pub fn normalize_date_value(value: &Value) -> Option<String> {
    let date = as_non_empty_string(value)?;
    if !regex::Regex::new(r"^\d{4}-\d{2}-\d{2}$").unwrap().is_match(&date) {
        return None;
    }
    // round-trip: parse as UTC → format back → 必须完全一致
    let parsed = chrono::NaiveDate::parse_from_str(&date, "%Y-%m-%d").ok()?;
    let reformatted = parsed.format("%Y-%m-%d").to_string();
    if reformatted != date {
        return None;
    }
    Some(date)
}

/// weekdays: 0-6 整数数组, unique sorted。
pub fn normalize_weekdays(value: &Value) -> Option<Vec<i64>> {
    let arr = value.as_array()?;
    let mut unique: Vec<i64> = Vec::new();
    for entry in arr {
        let n = entry.as_i64().or_else(|| entry.as_u64().map(|u| u as i64))?;
        if !entry.is_i64() && !entry.is_u64() {
            return None; // 必须是整数
        }
        if !(0..=6).contains(&n) {
            return None;
        }
        if !unique.contains(&n) {
            unique.push(n);
        }
    }
    if unique.is_empty() {
        return None;
    }
    unique.sort();
    Some(unique)
}

// =========================================================================
// schedule times / timezone
// =========================================================================

/// 从 schedule 解析 time-of-day 列表 (优先 `times: []`, fallback `time: "HH:MM"`)。
/// 如果为空, 尝试从 existingSchedule 补全。
/// 返回 sorted unique, 空则 None。
pub fn resolve_schedule_times(value: &Value, existing_schedule: Option<&Value>) -> Option<Vec<String>> {
    let mut times: Vec<String> = Vec::new();

    if let Some(arr) = value.get("times").and_then(Value::as_array) {
        for item in arr {
            if let Some(normalized) = normalize_time_value(item) {
                times.push(normalized);
            } else {
                return None; // Node: throw 'schedule.times must contain HH:mm values'
            }
        }
    }

    if let Some(legacy) = normalize_time_value(value.get("time").unwrap_or(&Value::Null)) {
        times.push(legacy);
    }

    // 空时从 existing 补
    if times.is_empty() {
        if let Some(existing) = existing_schedule {
            if let Some(arr) = existing.get("times").and_then(Value::as_array) {
                for item in arr {
                    if let Some(normalized) = normalize_time_value(item) {
                        times.push(normalized);
                    }
                }
            }
        }
    }

    // sorted unique
    times.sort();
    times.dedup();
    if times.is_empty() {
        None
    } else {
        Some(times)
    }
}

/// 默认时区: 本地 zoneName (从 chrono::Local offset 推导), fallback UTC。
///
/// 注意: `chrono::Local` 的 timezone 对象不实现 Display, 无法直接获取 IANA 名称。
/// 所以这里只能 fallback 到 "UTC"。生产环境中 task 的 timezone 通常由前端显式传入。
pub fn resolve_default_timezone() -> String {
    "UTC".to_string()
}

/// 检查是否合法 IANA timezone (用 chrono-tz FromStr)。
pub fn is_valid_iana_zone(name: &str) -> bool {
    name.parse::<chrono_tz::Tz>().is_ok()
}

/// 归一化 timezone: 空 → fallback; 非法 → None。
pub fn normalize_timezone(value: &Value, fallback: &str) -> Option<String> {
    let tz = as_non_empty_string(value);
    match tz {
        None => Some(fallback.to_string()),
        Some(name) => {
            if is_valid_iana_zone(&name) {
                Some(name)
            } else {
                None
            }
        }
    }
}

// =========================================================================
// cron validation
// =========================================================================

/// 验证 cron 表达式 (Node 用 cron-parser; Rust 用 cron crate)。
///
/// Node `cron-parser` 默认 **5-field** (`分 时 日 月 周`)。
/// Rust `cron` crate 用 **7-field** (`秒 分 时 日 月 周 年`)。
///
/// 对齐策略: 把用户输入的 5-field 表达式 prepend `0 ` (秒=0) + append ` *` (年通配) → 7-field。
pub fn validate_cron_expression(expression: &str, timezone: &str) -> bool {
    let _ = timezone;
    // Node cron-parser 用 5-field (分 时 日 月 周)。
    // 先验证输入确实是 5 个 whitespace-separated field。
    let fields: Vec<&str> = expression.split_whitespace().collect();
    if fields.len() != 5 {
        return false;
    }
    // Rust cron crate 用 7-field (秒 分 时 日 月 周 年)。
    // prepend "0 " (秒=0) + append " *" (年通配) → 7-field。
    let seven_field = format!("0 {expression} *");
    match seven_field.as_str().parse::<cron::Schedule>() {
        Ok(schedule) => {
            // 必须能算出至少一个 upcoming (否则表达式如 "0 0 31 2 *" 会通过 parse 但永不触发)
            schedule.upcoming(chrono::Utc).next().is_some()
        }
        Err(_) => false,
    }
}

// =========================================================================
// schedule / execution / state 归一化
// =========================================================================

/// 归一化 schedule — 返回干净 schedule object 或 error message。
pub fn normalize_schedule(value: &Value, existing_schedule: Option<&Value>) -> Result<Value, String> {
    if !value.is_object() {
        return Err("schedule is required".to_string());
    }

    let kind = as_non_empty_string(value.get("kind").unwrap_or(&Value::Null));
    if !matches!(kind.as_deref(), Some("daily") | Some("weekly") | Some("once") | Some("cron")) {
        return Err("schedule.kind must be daily, weekly, once, or cron".to_string());
    }
    let kind = kind.unwrap();

    let fallback_tz = existing_schedule
        .and_then(|s| s.get("timezone"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(resolve_default_timezone);
    let timezone = normalize_timezone(value.get("timezone").unwrap_or(&Value::Null), &fallback_tz)
        .ok_or_else(|| "schedule.timezone must be a valid IANA timezone".to_string())?;

    if kind == "daily" {
        let times = resolve_schedule_times(value, existing_schedule)
            .ok_or_else(|| "schedule.times must include at least one HH:mm value for daily schedule".to_string())?;
        return Ok(json!({ "kind": kind, "times": times, "timezone": timezone }));
    }

    if kind == "weekly" {
        let times = resolve_schedule_times(value, existing_schedule)
            .ok_or_else(|| "schedule.times must include at least one HH:mm value for weekly schedule".to_string())?;
        let weekdays = normalize_weekdays(value.get("weekdays").unwrap_or(&Value::Null))
            .ok_or_else(|| "schedule.weekdays must include values from 0 to 6 for weekly schedule".to_string())?;
        return Ok(json!({ "kind": kind, "times": times, "weekdays": weekdays, "timezone": timezone }));
    }

    if kind == "once" {
        let date = normalize_date_value(value.get("date").unwrap_or(&Value::Null))
            .ok_or_else(|| "schedule.date must be YYYY-MM-DD for once schedule".to_string())?;
        let time = normalize_time_value(value.get("time").unwrap_or(&Value::Null))
            .ok_or_else(|| "schedule.time must be HH:mm for once schedule".to_string())?;
        return Ok(json!({ "kind": kind, "date": date, "time": time, "timezone": timezone }));
    }

    // cron
    let cron_raw = as_non_empty_string(value.get("cron").unwrap_or(&Value::Null))
        .unwrap_or_default();
    let cron = clamp_length(&cron_raw, MAX_CRON_LENGTH);
    if cron.is_empty() {
        return Err("schedule.cron is required for cron schedule".to_string());
    }
    if !validate_cron_expression(&cron, &timezone) {
        return Err("schedule.cron is invalid".to_string());
    }
    Ok(json!({ "kind": kind, "cron": cron, "timezone": timezone }))
}

/// 归一化 execution — 必填 prompt/providerID/modelID + clamp prompt。
pub fn normalize_execution(value: &Value) -> Result<Value, String> {
    if !value.is_object() {
        return Err("execution is required".to_string());
    }

    let prompt_raw = as_non_empty_string(value.get("prompt").unwrap_or(&Value::Null)).unwrap_or_default();
    let prompt = clamp_length(&prompt_raw, MAX_TASK_PROMPT_LENGTH);
    let provider_id = as_non_empty_string(value.get("providerID").unwrap_or(&Value::Null));
    let model_id = as_non_empty_string(value.get("modelID").unwrap_or(&Value::Null));
    let variant = as_non_empty_string(value.get("variant").unwrap_or(&Value::Null));
    let agent = as_non_empty_string(value.get("agent").unwrap_or(&Value::Null));
    let goal_enabled = value.get("goalEnabled").and_then(Value::as_bool).unwrap_or(false);
    let goal_token_budget = value.get("goalTokenBudget").and_then(Value::as_f64).filter(|f| f.is_finite() && *f > 0.0).map(|f| f.floor() as i64);

    if prompt.is_empty() {
        return Err("execution.prompt is required".to_string());
    }
    let provider_id = provider_id.ok_or_else(|| "execution.providerID is required".to_string())?;
    let model_id = model_id.ok_or_else(|| "execution.modelID is required".to_string())?;

    let mut result = json!({
        "prompt": prompt,
        "providerID": provider_id,
        "modelID": model_id,
    });
    if let Some(v) = variant {
        result["variant"] = json!(v);
    }
    if let Some(a) = agent {
        result["agent"] = json!(a);
    }
    if goal_enabled {
        result["goalEnabled"] = json!(true);
        if let Some(budget) = goal_token_budget {
            result["goalTokenBudget"] = json!(budget);
        }
    }
    Ok(result)
}

/// 归一化 state — 时间戳 round + lastError clamp + lastStatus 归一化。
pub fn normalize_state(value: &Value, fallback: &Value) -> Value {
    let source = if value.is_object() { value } else { fallback };
    let fallback = if fallback.is_object() { fallback } else { &Value::Null };

    let now = chrono::Utc::now().timestamp_millis();

    let round_num = |v: &Value| -> Option<i64> {
        v.as_f64()
            .filter(|f| f.is_finite())
            .map(|f| (f.max(0.0).round()) as i64)
    };

    let mut result = serde_json::Map::new();

    // createdAt
    let created_at = round_num(source.get("createdAt").unwrap_or(&Value::Null))
        .or_else(|| round_num(fallback.get("createdAt").unwrap_or(&Value::Null)))
        .unwrap_or(now);
    result.insert("createdAt".into(), json!(created_at));

    // updatedAt
    let updated_at = round_num(source.get("updatedAt").unwrap_or(&Value::Null))
        .or_else(|| round_num(fallback.get("updatedAt").unwrap_or(&Value::Null)))
        .unwrap_or(now);
    result.insert("updatedAt".into(), json!(updated_at));

    // lastStatus
    let last_status = normalize_status(source.get("lastStatus").unwrap_or(&Value::Null));
    result.insert("lastStatus".into(), json!(last_status));

    // 可选字段 — 仅在 round 后 > 0 才写入
    if let Some(last_run_at) = round_num(source.get("lastRunAt").unwrap_or(&Value::Null)) {
        result.insert("lastRunAt".into(), json!(last_run_at));
    }
    if let Some(last_duration) = round_num(source.get("lastDurationMs").unwrap_or(&Value::Null)) {
        result.insert("lastDurationMs".into(), json!(last_duration));
    }
    if let Some(next_run_at) = round_num(source.get("nextRunAt").unwrap_or(&Value::Null)) {
        result.insert("nextRunAt".into(), json!(next_run_at));
    }
    if let Some(last_session_id) = as_non_empty_string(source.get("lastSessionId").unwrap_or(&Value::Null)) {
        result.insert("lastSessionId".into(), json!(last_session_id));
    }
    if let Some(last_error_raw) = as_non_empty_string(source.get("lastError").unwrap_or(&Value::Null)) {
        let last_error = clamp_length(&last_error_raw, MAX_LAST_ERROR_LENGTH);
        result.insert("lastError".into(), json!(last_error));
    }

    Value::Object(result)
}

// =========================================================================
// normalize_task_for_storage — 完整 pipeline
// =========================================================================

pub struct NormalizeOptions<'a> {
    pub now: i64,
    pub existing_task: Option<&'a Value>,
    pub allow_create: bool,
    pub refresh_updated_at: bool,
}

/// 完整 task 归一化 pipeline — 与 Node `normalizeTaskForStorage` 一致。
pub fn normalize_task_for_storage(value: &Value, opts: NormalizeOptions) -> Result<Value, String> {
    if !value.is_object() {
        return Err("task is required".to_string());
    }

    let incoming_id = as_non_empty_string(value.get("id").unwrap_or(&Value::Null));
    let existing_id = opts.existing_task.and_then(|t| as_non_empty_string(t.get("id").unwrap_or(&Value::Null)));

    // task.id immutability: 如果有 existing task 且 incoming id 与 existing id 不一致
    if opts.existing_task.is_some() {
        if let (Some(incoming), Some(existing)) = (&incoming_id, &existing_id) {
            if incoming != existing {
                return Err("task.id is immutable".to_string());
            }
        }
    }

    // allowCreate gate
    if opts.existing_task.is_none() && incoming_id.is_some() && !opts.allow_create {
        return Err("task.id does not exist".to_string());
    }

    // ID: existing > incoming > generate uuid
    let id = existing_id
        .or(incoming_id)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    // name: required + clamp
    let name_raw = as_non_empty_string(value.get("name").unwrap_or(&Value::Null)).unwrap_or_default();
    let name = clamp_length(&name_raw, MAX_TASK_NAME_LENGTH);
    if name.is_empty() {
        return Err("task.name is required".to_string());
    }

    // enabled: 显式 boolean, fallback existing, 再 fallback true
    let enabled = value
        .get("enabled")
        .and_then(Value::as_bool)
        .or_else(|| opts.existing_task.and_then(|t| t.get("enabled").and_then(Value::as_bool)))
        .unwrap_or(true);

    let existing_schedule = opts.existing_task.and_then(|t| t.get("schedule"));
    let schedule = normalize_schedule(value.get("schedule").unwrap_or(&Value::Null), existing_schedule)?;
    let execution = normalize_execution(value.get("execution").unwrap_or(&Value::Null))?;

    let now_ms = opts.now.max(0);
    let existing_state = opts.existing_task.and_then(|t| t.get("state")).unwrap_or(&Value::Null);
    let base_state = normalize_state(value.get("state").unwrap_or(&Value::Null), existing_state);

    // state.createdAt: 优先 existing, 再 fallback base, 再 fallback now
    let state_created_at = opts
        .existing_task
        .and_then(|t| t.get("state"))
        .and_then(|s| s.get("createdAt"))
        .and_then(|v| v.as_i64())
        .or_else(|| base_state.get("createdAt").and_then(Value::as_i64))
        .unwrap_or(now_ms);

    // state.updatedAt: refresh ? now : existing/fallback
    let state_updated_at = if opts.refresh_updated_at {
        now_ms
    } else {
        base_state.get("updatedAt").and_then(Value::as_i64).unwrap_or(now_ms)
    };

    let mut state = base_state;
    if let Some(obj) = state.as_object_mut() {
        obj.insert("createdAt".into(), json!(state_created_at));
        obj.insert("updatedAt".into(), json!(state_updated_at));
    }

    Ok(json!({
        "id": id,
        "name": name,
        "enabled": enabled,
        "schedule": schedule,
        "execution": execution,
        "state": state,
    }))
}

/// 用于 list 读取时的 normalize (refreshUpdatedAt=false, allowCreate=true)。
/// 单个 task normalize 失败 → 跳过 (与 Node `readProjectConfigFromDisk` 一致)。
pub fn normalize_task_for_read(task: &Value, now_ms: i64) -> Option<Value> {
    normalize_task_for_storage(
        task,
        NormalizeOptions {
            now: now_ms,
            existing_task: None,
            allow_create: true,
            refresh_updated_at: false,
        },
    )
    .ok()
}

// =========================================================================
// 测试
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn now_ms() -> i64 {
        chrono::Utc::now().timestamp_millis()
    }

    // --- status ---

    #[test]
    fn test_normalize_status() {
        assert_eq!(normalize_status(&json!("running")), "running");
        assert_eq!(normalize_status(&json!("invalid")), "idle");
        assert_eq!(normalize_status(&json!(null)), "idle");
    }

    // --- time / date ---

    #[test]
    fn test_time_value() {
        assert_eq!(normalize_time_value(&json!("09:00")), Some("09:00".into()));
        assert_eq!(normalize_time_value(&json!("23:59")), Some("23:59".into()));
        assert_eq!(normalize_time_value(&json!("24:00")), None);
        assert_eq!(normalize_time_value(&json!("")), None);
    }

    #[test]
    fn test_date_value_valid() {
        assert_eq!(normalize_date_value(&json!("2024-03-15")), Some("2024-03-15".into()));
    }

    #[test]
    fn test_date_value_invalid_roundtrip() {
        // 2 月 31 日不存在 — NaiveDate 解析会失败
        assert_eq!(normalize_date_value(&json!("2024-02-31")), None);
        assert_eq!(normalize_date_value(&json!("not-a-date")), None);
    }

    // --- weekdays ---

    #[test]
    fn test_weekdays() {
        assert_eq!(normalize_weekdays(&json!([1, 5, 3])), Some(vec![1, 3, 5]));
        assert_eq!(normalize_weekdays(&json!([0, 0, 6])), Some(vec![0, 6]));
        assert_eq!(normalize_weekdays(&json!([7])), None);
        assert_eq!(normalize_weekdays(&json!([])), None);
    }

    // --- timezone ---

    #[test]
    fn test_timezone_valid() {
        assert_eq!(
            normalize_timezone(&json!("UTC"), "UTC"),
            Some("UTC".into())
        );
        assert_eq!(
            normalize_timezone(&json!("America/New_York"), "UTC"),
            Some("America/New_York".into())
        );
    }

    #[test]
    fn test_timezone_invalid_fallback() {
        // 空 → fallback
        assert_eq!(normalize_timezone(&json!(null), "UTC"), Some("UTC".into()));
        // 非法 → None
        assert_eq!(normalize_timezone(&json!("Foo/Bar"), "UTC"), None);
    }

    // --- cron ---

    #[test]
    fn test_cron_valid() {
        assert!(validate_cron_expression("0 9 * * *", "UTC"));
        assert!(validate_cron_expression("*/15 * * * *", "UTC"));
    }

    #[test]
    fn test_cron_invalid() {
        assert!(!validate_cron_expression("not cron", "UTC"));
        assert!(!validate_cron_expression("* * * *", "UTC")); // 字段不足
    }

    // --- schedule ---

    #[test]
    fn test_normalize_schedule_daily() {
        let result = normalize_schedule(
            &json!({ "kind": "daily", "times": ["09:00", "18:00"], "timezone": "UTC" }),
            None,
        );
        assert!(result.is_ok());
        let s = result.unwrap();
        assert_eq!(s["kind"], "daily");
        assert_eq!(s["times"], json!(["09:00", "18:00"]));
    }

    #[test]
    fn test_normalize_schedule_weekly() {
        let result = normalize_schedule(
            &json!({ "kind": "weekly", "times": ["09:00"], "weekdays": [1, 3], "timezone": "UTC" }),
            None,
        );
        assert!(result.is_ok());
        let s = result.unwrap();
        assert_eq!(s["kind"], "weekly");
        assert_eq!(s["weekdays"], json!([1, 3]));
    }

    #[test]
    fn test_normalize_schedule_once() {
        let result = normalize_schedule(
            &json!({ "kind": "once", "date": "2025-12-25", "time": "10:00", "timezone": "UTC" }),
            None,
        );
        assert!(result.is_ok());
        assert_eq!(result.unwrap()["date"], "2025-12-25");
    }

    #[test]
    fn test_normalize_schedule_cron() {
        let result = normalize_schedule(
            &json!({ "kind": "cron", "cron": "0 9 * * 1-5", "timezone": "UTC" }),
            None,
        );
        assert!(result.is_ok());
        assert_eq!(result.unwrap()["cron"], "0 9 * * 1-5");
    }

    #[test]
    fn test_normalize_schedule_bad_kind() {
        let result = normalize_schedule(&json!({ "kind": "hourly" }), None);
        assert!(result.is_err());
    }

    // --- execution ---

    #[test]
    fn test_normalize_execution_ok() {
        let result = normalize_execution(&json!({
            "prompt": "hello",
            "providerID": "openai",
            "modelID": "gpt-4o"
        }));
        assert!(result.is_ok());
    }

    #[test]
    fn test_normalize_execution_missing_fields() {
        assert!(normalize_execution(&json!({"prompt": "x"})).is_err());
        assert!(normalize_execution(&json!({"prompt": "x", "providerID": "p"})).is_err());
        assert!(normalize_execution(&json!({})).is_err());
    }

    #[test]
    fn test_normalize_execution_clamps_prompt() {
        let long_prompt = "a".repeat(30_000);
        let result = normalize_execution(&json!({
            "prompt": long_prompt,
            "providerID": "p",
            "modelID": "m"
        }));
        assert!(result.is_ok());
        let exec = result.unwrap();
        assert_eq!(exec["prompt"].as_str().unwrap().chars().count(), MAX_TASK_PROMPT_LENGTH);
    }

    // --- state ---

    #[test]
    fn test_normalize_state_defaults() {
        let state = normalize_state(&json!({}), &json!({}));
        assert!(state.get("createdAt").is_some());
        assert!(state.get("updatedAt").is_some());
        assert_eq!(state["lastStatus"], "idle");
    }

    #[test]
    fn test_normalize_state_clamps_last_error() {
        let long_err = "e".repeat(3000);
        let state = normalize_state(&json!({"lastError": long_err}), &json!({}));
        assert_eq!(state["lastError"].as_str().unwrap().chars().count(), MAX_LAST_ERROR_LENGTH);
    }

    #[test]
    fn test_normalize_state_rounds_timestamps() {
        let state = normalize_state(&json!({"lastRunAt": 1234.6, "nextRunAt": -5}), &json!({}));
        assert_eq!(state["lastRunAt"], 1235);
        // 负数 → 跳过 (round_num 在 < 0 时仍返回 Some(0) 因为 max(0))
        // Node 行为: Math.max(0, Math.round(-5)) = 0, 但条件 typeof === 'number' → 会写入 0
        assert_eq!(state["nextRunAt"], 0);
    }

    // --- normalize_task_for_storage ---

    #[test]
    fn test_normalize_task_new() {
        let now = now_ms();
        let result = normalize_task_for_storage(
            &json!({
                "name": "my task",
                "schedule": { "kind": "daily", "times": ["09:00"], "timezone": "UTC" },
                "execution": { "prompt": "hi", "providerID": "openai", "modelID": "gpt-4o" }
            }),
            NormalizeOptions {
                now,
                existing_task: None,
                allow_create: true,
                refresh_updated_at: true,
            },
        );
        assert!(result.is_ok());
        let task = result.unwrap();
        assert!(!task["id"].as_str().unwrap().is_empty()); // uuid 生成的
        assert_eq!(task["name"], "my task");
        assert_eq!(task["enabled"], true);
        assert_eq!(task["state"]["updatedAt"], now);
    }

    #[test]
    fn test_normalize_task_missing_name() {
        let result = normalize_task_for_storage(
            &json!({
                "schedule": { "kind": "daily", "times": ["09:00"], "timezone": "UTC" },
                "execution": { "prompt": "hi", "providerID": "openai", "modelID": "gpt-4o" }
            }),
            NormalizeOptions {
                now: now_ms(),
                existing_task: None,
                allow_create: true,
                refresh_updated_at: true,
            },
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("task.name is required"));
    }

    #[test]
    fn test_normalize_task_id_immutable() {
        let existing = json!({
            "id": "task-abc",
            "name": "old",
            "schedule": { "kind": "daily", "times": ["09:00"], "timezone": "UTC" },
            "execution": { "prompt": "x", "providerID": "p", "modelID": "m" },
            "state": { "createdAt": 1000 }
        });
        let result = normalize_task_for_storage(
            &json!({
                "id": "task-different",
                "name": "new",
                "schedule": { "kind": "daily", "times": ["09:00"], "timezone": "UTC" },
                "execution": { "prompt": "x", "providerID": "p", "modelID": "m" }
            }),
            NormalizeOptions {
                now: now_ms(),
                existing_task: Some(&existing),
                allow_create: true,
                refresh_updated_at: true,
            },
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("task.id is immutable"));
    }

    #[test]
    fn test_normalize_task_preserves_existing_state_created_at() {
        let existing = json!({
            "id": "task-1",
            "name": "old",
            "schedule": { "kind": "daily", "times": ["09:00"], "timezone": "UTC" },
            "execution": { "prompt": "x", "providerID": "p", "modelID": "m" },
            "state": { "createdAt": 1111 }
        });
        let now = now_ms();
        let result = normalize_task_for_storage(
            &json!({
                "id": "task-1",
                "name": "new",
                "schedule": { "kind": "daily", "times": ["09:00"], "timezone": "UTC" },
                "execution": { "prompt": "y", "providerID": "p", "modelID": "m" }
            }),
            NormalizeOptions {
                now,
                existing_task: Some(&existing),
                allow_create: true,
                refresh_updated_at: true,
            },
        );
        let task = result.unwrap();
        assert_eq!(task["state"]["createdAt"], 1111); // 保留 existing
        assert_eq!(task["state"]["updatedAt"], now); // 刷新为 now
    }
}
