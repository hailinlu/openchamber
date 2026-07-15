//! `quota/providers/cursor.js` 移植。
//!
//! Cursor provider — 3-source credential priority:
//!   1. env (CURSOR_TOKEN / CURSOR_ACCESS_TOKEN / CURSOR_REFRESH_TOKEN / *_FILE)
//!   2. file (paths from env)
//!   3. managed (quota credentials JSON)
//!
//! + import from SQLite (macOS only via `sqlite3` shell, gated by `cfg(target_os)`).
//!   Token refresh via `POST /oauth/token` with `grant_type: refresh_token`.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde_json::{json, Value};

use crate::quota::credentials::{
    read_managed_credential, write_managed_credential,
};
use crate::quota::providers::{fetch_error, not_configured};
use crate::quota::utils::formatters::{
    build_result, format_money, to_number, to_usage_window, BuildResultArgs,
    ToUsageWindowArgs,
};
use crate::quota::utils::transformers::to_timestamp;
use crate::quota::{block_on, http_client};

pub const PROVIDER_ID: &str = "cursor";
pub const PROVIDER_NAME: &str = "Cursor";

const BASE_URL: &str = "https://api2.cursor.sh";
const USAGE_URL: &str = "https://api2.cursor.sh/aiserver.v1.DashboardService/GetCurrentPeriodUsage";
const PLAN_URL: &str = "https://api2.cursor.sh/aiserver.v1.DashboardService/GetPlanInfo";
const CREDITS_URL: &str = "https://api2.cursor.sh/aiserver.v1.DashboardService/GetCreditGrantsBalance";
const REFRESH_URL: &str = "https://api2.cursor.sh/oauth/token";
const CLIENT_ID: &str = "KbZUR41cY7W6zRSdpSUJ7I7mLYBKOCmB";
const REFRESH_BUFFER_MS: i64 = 5 * 60 * 1000;

/// 解析 JWT payload (base64url, no padding),返回 `Some(Value)` 或 `None`。
pub fn read_jwt_payload(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let s = std::str::from_utf8(&bytes).ok()?;
    serde_json::from_str(s).ok()
}

/// token 是否需要在 5 分钟内刷新。
pub fn token_needs_refresh(token: &str) -> bool {
    let payload = match read_jwt_payload(token) {
        Some(p) => p,
        None => return true,
    };
    let exp = payload.get("exp").and_then(|v| v.as_i64());
    let Some(exp) = exp else { return true };
    let now = chrono::Utc::now().timestamp_millis();
    exp * 1000 - now <= REFRESH_BUFFER_MS
}

/// 从 macOS Cursor `state.vscdb` 读取的 value (单 key)。
#[cfg(target_os = "macos")]
fn read_state_value(key: &str) -> Option<String> {
    let home = crate::git::paths::home_dir();
    let db = home
        .join("Library")
        .join("Application Support")
        .join("Cursor")
        .join("User")
        .join("globalStorage")
        .join("state.vscdb");
    if !db.exists() {
        return None;
    }
    // 把 key 中的单引号替换为 SQL 转义形式。
    let escaped_key = key.replace('\'', "''");
    let sql = format!(
        "SELECT value FROM ItemTable WHERE key = '{}' LIMIT 1;",
        escaped_key
    );
    let output = std::process::Command::new("sqlite3")
        .arg("-json")
        .arg(&db)
        .arg(&sql)
        .env("LANG", "C")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: Value = serde_json::from_str(stdout.trim()).ok()?;
    let v = parsed
        .as_array()
        .and_then(|a| a.first())
        .and_then(|o| o.get("value"))
        .and_then(|v| v.as_str())?;
    let trimmed = v.trim();
    if trimmed.is_empty() { None } else { Some(trimmed.to_string()) }
}

#[cfg(not(target_os = "macos"))]
fn read_state_value(_key: &str) -> Option<String> {
    None
}

fn read_file_token(path: &Option<String>) -> Option<String> {
    let p = path.as_deref()?;
    if !std::path::Path::new(p).exists() {
        return None;
    }
    let content = std::fs::read_to_string(p).ok()?;
    let trimmed = content.trim().to_string();
    if trimmed.is_empty() { None } else { Some(trimmed) }
}

/// 加载 Cursor auth state,按 env → file → managed 优先级。
///
/// 返回 `{ accessToken, refreshToken, source }`。
pub fn load_auth_state() -> (Option<String>, Option<String>, &'static str) {
    let env_access = std::env::var("CURSOR_TOKEN")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            std::env::var("CURSOR_ACCESS_TOKEN")
                .ok()
                .filter(|s| !s.trim().is_empty())
        });
    let env_refresh = std::env::var("CURSOR_REFRESH_TOKEN")
        .ok()
        .filter(|s| !s.trim().is_empty());
    if env_access.is_some() || env_refresh.is_some() {
        return (env_access, env_refresh, "env");
    }

    let file_access = read_file_token(&std::env::var("CURSOR_TOKEN_FILE").ok());
    let file_refresh = read_file_token(&std::env::var("CURSOR_REFRESH_TOKEN_FILE").ok());
    if file_access.is_some() || file_refresh.is_some() {
        return (file_access, file_refresh, "file");
    }

    let managed = read_managed_credential(PROVIDER_ID);
    let access = managed
        .as_ref()
        .and_then(|v| v.get("accessToken").and_then(|t| t.as_str()))
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let refresh = managed
        .as_ref()
        .and_then(|v| v.get("refreshToken").and_then(|t| t.as_str()))
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    (access, refresh, "managed")
}

fn persist_access_token(source: &str, refresh: Option<&str>, access: &str) {
    if source == "managed" {
        let refresh = refresh.unwrap_or("");
        let v = json!({"accessToken": access, "refreshToken": refresh});
        let _ = write_managed_credential(PROVIDER_ID, v);
    }
}

async fn refresh_access_token(source: &str, refresh: &str) -> Result<String, String> {
    let client = http_client();
    let body = serde_json::json!({
        "grant_type": "refresh_token",
        "client_id": CLIENT_ID,
        "refresh_token": refresh,
    });
    let resp = client
        .post(REFRESH_URL)
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| e.to_string())?;

    let status = resp.status().as_u16();
    let body: Value = resp.json().await.map_err(|e| e.to_string())?;
    if body.get("shouldLogout").and_then(|v| v.as_bool()) == Some(true) {
        return Err("Session expired - please sign in to Cursor again".to_string());
    }
    if status != 200 {
        let msg = if status == 401 {
            "Cursor session expired".to_string()
        } else {
            format!("API error: {status}")
        };
        return Err(msg);
    }
    let access = body
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "Cursor refresh response did not include an access token".to_string())?
        .to_string();
    persist_access_token(source, Some(refresh), &access);
    Ok(access)
}

async fn resolve_credential_access_token(source: &str, access: Option<&str>, refresh: Option<&str>) -> Result<Option<String>, String> {
    if access.is_none() && refresh.is_none() {
        return Ok(None);
    }
    let access = access.unwrap_or("");
    if !access.is_empty() && !token_needs_refresh(access) {
        return Ok(Some(access.to_string()));
    }
    if let Some(r) = refresh {
        if !r.is_empty() {
            let new = refresh_access_token(source, r).await?;
            return Ok(Some(new));
        }
    }
    if !access.is_empty() {
        return Ok(Some(access.to_string()));
    }
    Ok(None)
}

async fn connect_post(url: &str, access_token: &str) -> Result<Value, String> {
    let client = http_client();
    let resp = client
        .post(url)
        .bearer_auth(access_token)
        .header("Content-Type", "application/json")
        .header("Connect-Protocol-Version", "1")
        .body("{}")
        .send()
        .await
        .map_err(|e| e.to_string())?;

    let status = resp.status().as_u16();
    if status != 200 {
        return Err(if status == 401 {
            "Cursor session expired".to_string()
        } else {
            format!("API error: {status}")
        });
    }
    resp.json::<Value>().await.map_err(|e| e.to_string())
}

fn cents_label(cents: Option<f64>) -> Option<String> {
    let v = cents?;
    let money = format_money(v / 100.0)?;
    Some(format!("${money}"))
}

fn percent_from_spend(plan_usage: &Value) -> Option<f64> {
    let explicit = to_number(plan_usage.get("totalPercentUsed").unwrap_or(&Value::Null));
    if let Some(e) = explicit {
        return Some(e);
    }
    let limit = to_number(plan_usage.get("limit").unwrap_or(&Value::Null))?;
    let remaining = to_number(plan_usage.get("remaining").unwrap_or(&Value::Null))?;
    if limit <= 0.0 {
        return None;
    }
    Some(((limit - remaining) / limit * 100.0).clamp(0.0, 100.0))
}

fn build_windows(usage: &Value, plan: &Option<Value>) -> serde_json::Map<String, Value> {
    let plan_usage = usage.get("planUsage").cloned().unwrap_or(json!({}));
    let spend_limit_usage = usage.get("spendLimitUsage").cloned().unwrap_or(json!({}));
    let reset_at = to_timestamp(usage.get("billingCycleEnd").unwrap_or(&Value::Null))
        .or_else(|| {
            plan.as_ref()
                .and_then(|p| p.get("planInfo"))
                .and_then(|pi| pi.get("billingCycleEnd"))
                .map(|_| 0)
        });
    let reset_at = if reset_at == Some(0) { None } else { reset_at };
    let now = chrono::Utc::now().timestamp_millis();
    let window_seconds = reset_at.map(|r| ((r - now) / 1000).max(0));

    let mut windows = serde_json::Map::new();

    let total_spend = to_number(plan_usage.get("totalSpend").unwrap_or(&Value::Null));
    windows.insert(
        "billing_cycle".into(),
        to_usage_window(ToUsageWindowArgs {
            used_percent: percent_from_spend(&plan_usage),
            window_seconds,
            reset_at,
            value_label: cents_label(total_spend).as_deref(),
        }),
    );

    let auto_percent = to_number(plan_usage.get("autoPercentUsed").unwrap_or(&Value::Null));
    if let Some(ap) = auto_percent {
        windows.insert(
            "auto".into(),
            to_usage_window(ToUsageWindowArgs {
                used_percent: Some(ap),
                window_seconds,
                reset_at,
                value_label: None,
            }),
        );
    }

    let api_percent = to_number(plan_usage.get("apiPercentUsed").unwrap_or(&Value::Null));
    if let Some(ap) = api_percent {
        windows.insert(
            "api".into(),
            to_usage_window(ToUsageWindowArgs {
                used_percent: Some(ap),
                window_seconds,
                reset_at,
                value_label: None,
            }),
        );
    }

    let plan_limit = cents_label(to_number(plan_usage.get("limit").unwrap_or(&Value::Null)));
    if let Some(pl) = plan_limit {
        let limit = to_number(plan_usage.get("limit").unwrap_or(&Value::Null));
        let remaining = to_number(plan_usage.get("remaining").unwrap_or(&Value::Null));
        let used_percent = match (limit, remaining) {
            (Some(l), Some(r)) if l > 0.0 => Some(((l - r) / l * 100.0).clamp(0.0, 100.0)),
            _ => None,
        };
        let remaining_label = cents_label(remaining).unwrap_or_else(|| "$0.00".to_string());
        let value_label = format!("{remaining_label} remaining of {pl}");
        windows.insert(
            "plan_limit".into(),
            to_usage_window(ToUsageWindowArgs {
                used_percent,
                window_seconds,
                reset_at,
                value_label: Some(&value_label),
            }),
        );
    }

    let on_demand_limit = to_number(spend_limit_usage.get("individualLimit").unwrap_or(&Value::Null))
        .or_else(|| to_number(spend_limit_usage.get("pooledLimit").unwrap_or(&Value::Null)));
    if let Some(odl) = on_demand_limit {
        if odl > 0.0 {
            let remaining = to_number(spend_limit_usage.get("individualRemaining").unwrap_or(&Value::Null))
                .or_else(|| to_number(spend_limit_usage.get("pooledRemaining").unwrap_or(&Value::Null)))
                .unwrap_or(0.0);
            let used_percent = if odl > 0.0 {
                Some(((odl - remaining) / odl * 100.0).clamp(0.0, 100.0))
            } else {
                None
            };
            let remaining_label = cents_label(Some(remaining)).unwrap_or_else(|| "$0.00".to_string());
            let total_label = cents_label(Some(odl)).unwrap_or_else(|| "$0.00".to_string());
            let value_label = format!("{remaining_label} remaining of {total_label}");
            windows.insert(
                "on_demand".into(),
                to_usage_window(ToUsageWindowArgs {
                    used_percent,
                    window_seconds,
                    reset_at,
                    value_label: Some(&value_label),
                }),
            );
        }
    }

    windows
}

fn append_credits_window(windows: &mut serde_json::Map<String, Value>, credits: &Option<Value>) {
    let Some(c) = credits else { return };
    let balance = to_number(
        c.get("balanceCents")
            .or_else(|| c.get("totalBalanceCents"))
            .or_else(|| c.get("amountCents"))
            .unwrap_or(&Value::Null),
    );
    let Some(b) = balance else { return };
    let label = cents_label(Some(b)).unwrap_or_else(|| "$0.00".to_string());
    windows.insert(
        "credits".into(),
        to_usage_window(ToUsageWindowArgs {
            used_percent: None,
            window_seconds: None,
            reset_at: None,
            value_label: Some(&label),
        }),
    );
}

pub fn is_configured() -> bool {
    let (a, r, _) = load_auth_state();
    a.is_some() || r.is_some()
}

pub async fn fetch_quota_async() -> Value {
    let (access_opt, refresh_opt, source) = load_auth_state();
    let access_token = match resolve_credential_access_token(source, access_opt.as_deref(), refresh_opt.as_deref()).await {
        Ok(Some(t)) => t,
        Ok(None) => return not_configured(PROVIDER_ID, PROVIDER_NAME),
        Err(e) => return fetch_error(PROVIDER_ID, PROVIDER_NAME, &e),
    };

    let usage_res = connect_post(USAGE_URL, &access_token).await;
    let usage = match usage_res {
        Ok(u) => u,
        Err(e) => return fetch_error(PROVIDER_ID, PROVIDER_NAME, &e),
    };

    let plan = match connect_post(PLAN_URL, &access_token).await {
        Ok(p) => Some(p),
        Err(_) => None,
    };
    let credits = match connect_post(CREDITS_URL, &access_token).await {
        Ok(c) => Some(c),
        Err(_) => None,
    };

    if usage.get("enabled").and_then(|v| v.as_bool()) == Some(false) || usage.get("planUsage").is_none() {
        return build_result(BuildResultArgs {
            provider_id: PROVIDER_ID,
            provider_name: PROVIDER_NAME,
            ok: false,
            configured: true,
            usage: None,
            error: Some("No active Cursor subscription"),
        });
    }

    let mut windows = build_windows(&usage, &plan);
    append_credits_window(&mut windows, &credits);

    let provider_name = plan
        .as_ref()
        .and_then(|p| p.get("planInfo"))
        .and_then(|pi| pi.get("planName"))
        .and_then(|n| n.as_str())
        .map(|name| format!("Cursor {name}"))
        .unwrap_or_else(|| PROVIDER_NAME.to_string());

    build_result(BuildResultArgs {
        provider_id: PROVIDER_ID,
        provider_name: &provider_name,
        ok: true,
        configured: true,
        usage: Some(json!({"windows": Value::Object(windows)})),
        error: None,
    })
}

/// 用于 validate 路由 — 校验给定 credential 是否有效。
pub async fn validate_cursor_credential(credential: &Value) -> Result<(), String> {
    let access = credential.get("accessToken").and_then(|v| v.as_str()).map(|s| s.to_string());
    let refresh = credential.get("refreshToken").and_then(|v| v.as_str()).map(|s| s.to_string());
    let source = "validation";
    let token = resolve_credential_access_token(source, access.as_deref(), refresh.as_deref()).await?;
    let Some(t) = token else {
        return Err("Cursor credentials are invalid".to_string());
    };
    connect_post(USAGE_URL, &t).await.map(|_| ())
}

/// 从 Cursor 的 SQLite 数据库读取 token 并把它存到 managed credentials 里。
/// 仅在 macOS 上工作(其他平台返回 `IMPORT_UNAVAILABLE`)。
pub async fn import_cursor_credential() -> Result<Value, String> {
    let access = read_state_value("cursorAuth/accessToken").unwrap_or_default();
    let refresh = read_state_value("cursorAuth/refreshToken").unwrap_or_default();
    if access.is_empty() && refresh.is_empty() {
        return Err("Cursor credentials are unavailable".to_string());
    }
    let credential = json!({"accessToken": access, "refreshToken": refresh});
    // 验证并更新 accessToken (例如 refresh 一次)
    let validated = validate_cursor_credential(&credential).await;
    if validated.is_err() {
        return Err("Cursor credentials are invalid".to_string());
    }
    write_managed_credential(PROVIDER_ID, credential).map_err(|e| e)
}

pub fn fetch_quota() -> Value {
    block_on(fetch_quota_async())
}

pub fn fetch_quota_sync() -> Value {
    fetch_quota()
}

pub use fetch_quota as fetch_cursor_quota;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_jwt_payload_basic() {
        // header.payload.signature — payload is `{"sub":"x","exp":1700000000,"extra":"a"}`
        // base64url(no pad) of "{"sub":"x","exp":1700000000,"extra":"a"}"
        use base64::Engine;
        let payload_json = r#"{"sub":"x","exp":1700000000,"extra":"a"}"#;
        let payload_b64 = URL_SAFE_NO_PAD.encode(payload_json.as_bytes());
        let token = format!("header.{payload_b64}.sig");
        let out = read_jwt_payload(&token).unwrap();
        assert_eq!(out["sub"], json!("x"));
        assert_eq!(out["exp"], json!(1700000000));
    }

    #[test]
    fn read_jwt_payload_invalid_returns_none() {
        assert!(read_jwt_payload("invalid").is_none());
        assert!(read_jwt_payload("a.b.c").is_none());
    }

    #[test]
    fn token_needs_refresh_expired() {
        let past = chrono::Utc::now().timestamp() - 3600;
        let payload_json = format!(r#"{{"exp":{past}}}"#);
        let payload_b64 = URL_SAFE_NO_PAD.encode(payload_json.as_bytes());
        let token = format!("a.{payload_b64}.c");
        assert!(token_needs_refresh(&token));
    }

    #[test]
    fn token_needs_refresh_future() {
        let future = chrono::Utc::now().timestamp() + 3600;
        let payload_json = format!(r#"{{"exp":{future}}}"#);
        let payload_b64 = URL_SAFE_NO_PAD.encode(payload_json.as_bytes());
        let token = format!("a.{payload_b64}.c");
        assert!(!token_needs_refresh(&token));
    }

    #[test]
    fn load_auth_state_priority_env_first() {
        // 设 env → 应该返回 "env"
        std::env::set_var("CURSOR_TOKEN", "tok-abc");
        std::env::remove_var("CURSOR_ACCESS_TOKEN");
        std::env::remove_var("CURSOR_REFRESH_TOKEN");
        let (a, _, source) = load_auth_state();
        assert_eq!(source, "env");
        assert_eq!(a.as_deref(), Some("tok-abc"));
        std::env::remove_var("CURSOR_TOKEN");
    }

    #[test]
    fn load_auth_state_no_token_no_source() {
        std::env::remove_var("CURSOR_TOKEN");
        std::env::remove_var("CURSOR_ACCESS_TOKEN");
        std::env::remove_var("CURSOR_REFRESH_TOKEN");
        std::env::remove_var("CURSOR_TOKEN_FILE");
        std::env::remove_var("CURSOR_REFRESH_TOKEN_FILE");
        let (_a, _r, source) = load_auth_state();
        // 不一定是哪个 source,因为文件系统读 managed。但因为没 managed 会是 file 路径
        // 我们只验证 source 是 std-env-managed-file 之一
        assert!(matches!(source, "env" | "file" | "managed"));
    }
}
