//! GitHub auth 存储 CRUD + gh-CLI 凭证缓存。
//!
//! 移植自 `packages/web/server/lib/github/auth.js` (361 行) +
//! `gh-cli-credential.js` (38 行)。

use std::sync::Mutex;
use std::time::{Duration, Instant};

use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::github::settings;

// ============================================================
// Types
// ============================================================

/// GitHub user 信息 (对应 Node 的 user 对象)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitHubUser {
    pub login: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avatar_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

/// GitHub auth 条目 (对应 Node 的 auth entry)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitHubAuthEntry {
    pub access_token: String,
    #[serde(default)]
    pub scope: String,
    #[serde(default = "default_token_type")]
    pub token_type: String,
    #[serde(default)]
    pub created_at: Option<i64>,
    #[serde(default)]
    pub user: Option<GitHubUser>,
    #[serde(default)]
    pub current: bool,
    #[serde(default)]
    pub account_id: String,
}

fn default_token_type() -> String {
    "bearer".to_string()
}

// ============================================================
// Storage
// ============================================================

/// 确保存储目录存在。
fn ensure_storage_dir() -> oc_core::Result<()> {
    let path = settings::auth_storage_file();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(())
}

/// 原子写入 auth 文件, mode 0o600。
fn write_auth_list(list: &[Value]) -> oc_core::Result<()> {
    ensure_storage_dir()?;
    let path = settings::auth_storage_file();
    let content = serde_json::to_string_pretty(list)?;

    let tmp = path.with_extension(format!(
        "json.{}.{}.tmp",
        std::process::id(),
        chrono::Utc::now().timestamp_millis()
    ));
    std::fs::write(&tmp, &content)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }

    std::fs::rename(&tmp, &path)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }

    Ok(())
}

/// 读取 auth 列表 (JSON 数组)。
fn read_auth_raw() -> Vec<Value> {
    let path = settings::auth_storage_file();
    match std::fs::read_to_string(&path) {
        Ok(content) => {
            let trimmed = content.trim();
            if trimmed.is_empty() {
                return Vec::new();
            }
            match serde_json::from_str::<Value>(trimmed) {
                Ok(Value::Array(arr)) => arr,
                Ok(obj) => vec![obj], // 兼容单个对象
                Err(_) => Vec::new(),
            }
        }
        Err(_) => Vec::new(),
    }
}

// ============================================================
// Normalization
// ============================================================

/// `resolveAccountId` — 解析 account ID。
///
/// 优先级: explicit accountId → user.login → String(user.id) → `token:${accessToken.slice(0,8)}`
pub fn resolve_account_id(user: Option<&GitHubUser>, access_token: &str, account_id: &str) -> String {
    if !account_id.trim().is_empty() {
        return account_id.trim().to_string();
    }
    if let Some(user) = user {
        if !user.login.trim().is_empty() {
            return user.login.trim().to_string();
        }
    }
    if let Some(user) = user {
        if let Some(id) = user.id {
            return id.to_string();
        }
    }
    if !access_token.trim().is_empty() {
        let prefix = access_token.chars().take(8).collect::<String>();
        return format!("token:{}", prefix);
    }
    String::new()
}

/// 标准化单个 auth 条目。
fn normalize_auth_entry(entry: &Value) -> Option<Value> {
    let access_token = entry.get("accessToken").and_then(|v| v.as_str()).unwrap_or("");
    if access_token.is_empty() {
        return None;
    }

    let user = entry.get("user").filter(|v| v.is_object()).map(|u| {
        json!({
            "login": u.get("login").and_then(|v| v.as_str()).unwrap_or(""),
            "avatarUrl": u.get("avatarUrl").and_then(|v| v.as_str()).unwrap_or(""),
            "id": u.get("id").and_then(|v| v.as_i64()),
            "name": u.get("name").and_then(|v| v.as_str()).unwrap_or(""),
            "email": u.get("email").and_then(|v| v.as_str()).unwrap_or(""),
        })
    });

    let user_struct = user.as_ref().map(|u| GitHubUser {
        login: u.get("login").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        avatar_url: u.get("avatarUrl").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).map(String::from),
        id: u.get("id").and_then(|v| v.as_i64()),
        name: u.get("name").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).map(String::from),
        email: u.get("email").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).map(String::from),
    });

    let explicit_account_id = entry
        .get("accountId")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let resolved_account_id =
        resolve_account_id(user_struct.as_ref(), access_token, explicit_account_id);

    Some(json!({
        "accessToken": access_token,
        "scope": entry.get("scope").and_then(|v| v.as_str()).unwrap_or(""),
        "tokenType": entry.get("tokenType").and_then(|v| v.as_str()).unwrap_or("bearer"),
        "createdAt": entry.get("createdAt").and_then(|v| v.as_i64()),
        "user": user,
        "current": entry.get("current").and_then(|v| v.as_bool()).unwrap_or(false),
        "accountId": resolved_account_id,
    }))
}

/// 标准化 auth 列表: 恰好一个 current, 所有 accountId 填充。
/// 返回 (normalized_list, changed)。
fn normalize_auth_list(raw: &[Value]) -> (Vec<Value>, bool) {
    let mut list: Vec<Value> = raw
        .iter()
        .filter_map(normalize_auth_entry)
        .collect();

    if list.is_empty() {
        return (Vec::new(), false);
    }

    let mut changed = false;

    // 确保恰好一个 current
    let mut current_found = false;
    for entry in list.iter_mut() {
        let is_current = entry.get("current").and_then(|v| v.as_bool()).unwrap_or(false);
        if is_current && !current_found {
            current_found = true;
        } else if is_current && current_found {
            entry["current"] = json!(false);
            changed = true;
        }
    }
    if !current_found {
        list[0]["current"] = json!(true);
        changed = true;
    }

    // 确保 accountId 填充
    for entry in list.iter_mut() {
        let account_id = entry.get("accountId").and_then(|v| v.as_str()).unwrap_or("");
        if account_id.is_empty() {
            let access_token = entry
                .get("accessToken")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let new_id = resolve_account_id(None, access_token, "");
            entry["accountId"] = json!(new_id);
            changed = true;
        }
    }

    (list, changed)
}

// ============================================================
// Public API
// ============================================================

/// 读取并标准化 auth 列表。如果有变化, 回写。
fn read_auth_list() -> Vec<Value> {
    let raw = read_auth_raw();
    let (list, changed) = normalize_auth_list(&raw);
    if changed {
        let _ = write_auth_list(&list);
    }
    list
}

/// 获取当前 (current) auth 条目。
pub fn get_github_auth() -> Option<GitHubAuthEntry> {
    let list = read_auth_list();
    if list.is_empty() {
        return None;
    }
    let current = list
        .iter()
        .find(|e| e.get("current").and_then(|v| v.as_bool()).unwrap_or(false))
        .or_else(|| list.first())?;

    let access_token = current.get("accessToken").and_then(|v| v.as_str())?;
    if access_token.is_empty() {
        return None;
    }

    serde_json::from_value(current.clone()).ok()
}

/// 获取所有 accounts (有 user + accountId 的条目)。
pub fn get_github_auth_accounts() -> Vec<Value> {
    let list = read_auth_list();
    list.iter()
        .filter(|entry| {
            entry.get("user").map(|u| !u.is_null()).unwrap_or(false)
                && entry
                    .get("accountId")
                    .and_then(|v| v.as_str())
                    .map(|s| !s.is_empty())
                    .unwrap_or(false)
        })
        .map(|entry| {
            json!({
                "id": entry.get("accountId").and_then(|v| v.as_str()).unwrap_or(""),
                "user": entry.get("user").cloned().unwrap_or(Value::Null),
                "scope": entry.get("scope").and_then(|v| v.as_str()).unwrap_or(""),
                "current": entry.get("current").and_then(|v| v.as_bool()).unwrap_or(false),
            })
        })
        .collect()
}

/// 设置/更新 GitHub auth (对应 Node `setGitHubAuth`)。
pub fn set_github_auth(
    access_token: &str,
    scope: &str,
    token_type: Option<&str>,
    user: Option<GitHubUser>,
    account_id: Option<&str>,
) -> GitHubAuthEntry {
    if access_token.is_empty() {
        panic!("accessToken is required");
    }

    let resolved_account_id =
        resolve_account_id(user.as_ref(), access_token, account_id.unwrap_or(""));

    let mut list = read_auth_list();
    let existing_index = list.iter().position(|entry| {
        entry
            .get("accountId")
            .and_then(|v| v.as_str())
            .map(|s| s == resolved_account_id)
            .unwrap_or(false)
    });

    let now = chrono::Utc::now().timestamp_millis();
    let next_entry = json!({
        "accessToken": access_token,
        "scope": scope,
        "tokenType": token_type.unwrap_or("bearer"),
        "createdAt": now,
        "user": user.as_ref().map(|u| serde_json::to_value(u).unwrap_or(Value::Null)).unwrap_or(Value::Null),
        "current": true,
        "accountId": resolved_account_id,
    });

    let target_index = if let Some(idx) = existing_index {
        list[idx] = next_entry.clone();
        idx
    } else {
        list.push(next_entry.clone());
        list.len() - 1
    };

    // 更新 current 标志
    for (idx, entry) in list.iter_mut().enumerate() {
        entry["current"] = json!(idx == target_index);
    }

    let _ = write_auth_list(&list);
    serde_json::from_value(next_entry).expect("auth entry serialization is consistent")
}

/// 激活指定 account (对应 Node `activateGitHubAuth`)。
pub fn activate_github_auth(account_id: &str) -> bool {
    if account_id.trim().is_empty() {
        return false;
    }
    let mut list = read_auth_list();
    let target_id = account_id.trim();
    let index = list.iter().position(|entry| {
        entry
            .get("accountId")
            .and_then(|v| v.as_str())
            .map(|s| s.trim() == target_id)
            .unwrap_or(false)
    });

    let Some(index) = index else {
        return false;
    };

    // 激活 OAuth account 时关闭 gh-CLI
    let _ = settings::set_gh_cli_active(false);

    for (idx, entry) in list.iter_mut().enumerate() {
        entry["current"] = json!(idx == index);
    }
    let _ = write_auth_list(&list);
    true
}

/// 清除当前 GitHub auth (对应 Node `clearGitHubAuth`)。
/// 只移除 current 条目; 如果空了就删文件。
pub fn clear_github_auth() -> bool {
    let list = read_auth_list();
    if list.is_empty() {
        return true;
    }

    let remaining: Vec<Value> = list
        .iter()
        .filter(|entry| !entry.get("current").and_then(|v| v.as_bool()).unwrap_or(false))
        .cloned()
        .collect();

    if remaining.is_empty() {
        // 删除文件
        let path = settings::auth_storage_file();
        let _ = std::fs::remove_file(&path);
        return true;
    }

    // remaining[0] 设为 current
    let mut remaining = remaining;
    for (idx, entry) in remaining.iter_mut().enumerate() {
        entry["current"] = json!(idx == 0);
    }
    let _ = write_auth_list(&remaining);
    true
}

// ============================================================
// gh-CLI credential cache
// ============================================================

const GH_CLI_CACHE_TTL: Duration = Duration::from_secs(30);

struct GhCliCache {
    token: Option<String>,
    fetched_at: Option<Instant>,
}

static GH_CLI_CACHE: Lazy<Mutex<GhCliCache>> = Lazy::new(|| {
    Mutex::new(GhCliCache {
        token: None,
        fetched_at: None,
    })
});

/// 获取 gh-CLI token (`gh auth token`), 30s 缓存。
pub fn get_gh_cli_token() -> Option<String> {
    let now = Instant::now();
    {
        let cache = GH_CLI_CACHE.lock().unwrap();
        if let (Some(ref token), Some(fetched_at)) = (&cache.token, cache.fetched_at) {
            if now.duration_since(fetched_at) < GH_CLI_CACHE_TTL {
                return Some(token.clone());
            }
        }
    }

    let token = fetch_gh_cli_token();

    let mut cache = GH_CLI_CACHE.lock().unwrap();
    cache.token = token.clone();
    cache.fetched_at = Some(now);

    token
}

/// 清除 gh-CLI token 缓存。
pub fn clear_gh_cli_token_cache() {
    let mut cache = GH_CLI_CACHE.lock().unwrap();
    cache.token = None;
    cache.fetched_at = None;
}

/// 执行 `gh auth token` 获取 token。
fn fetch_gh_cli_token() -> Option<String> {
    use std::process::Command;

    let mut cmd = Command::new("gh");
    cmd.args(["auth", "token"]);

    #[cfg(unix)]
    {}
    #[cfg(windows)]
    {
        // windowsHide 等价 — Rust 的 Command 在 Windows 上默认不显示窗口
    }

    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let output = cmd.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if token.is_empty() {
        None
    } else {
        Some(token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_account_id_explicit() {
        assert_eq!(
            resolve_account_id(None, "tok_abc123", "explicit_id"),
            "explicit_id"
        );
    }

    #[test]
    fn resolve_account_id_login() {
        let user = GitHubUser {
            login: "octocat".to_string(),
            avatar_url: None,
            id: Some(12345),
            name: None,
            email: None,
        };
        assert_eq!(resolve_account_id(Some(&user), "tok_abc123", ""), "octocat");
    }

    #[test]
    fn resolve_account_id_numeric() {
        let user = GitHubUser {
            login: String::new(),
            avatar_url: None,
            id: Some(12345),
            name: None,
            email: None,
        };
        assert_eq!(resolve_account_id(Some(&user), "tok_abc123", ""), "12345");
    }

    #[test]
    fn resolve_account_id_token_prefix() {
        assert_eq!(
            resolve_account_id(None, "ghp_abc1234567890", ""),
            "token:ghp_abc1"
        );
    }

    #[test]
    fn normalize_auth_list_single_current() {
        let raw = vec![json!({
            "accessToken": "tok1",
            "current": true,
            "user": { "login": "user1", "id": 1 },
        })];
        let (list, changed) = normalize_auth_list(&raw);
        assert_eq!(list.len(), 1);
        assert!(list[0]["current"].as_bool().unwrap());
        // accountId 由 normalize_auth_entry 从 user.login 解析 (无需 normalize_auth_list 回填)
        assert_eq!(list[0]["accountId"], "user1");
        // 恰好一个 current, accountId 已填充 → 无变化
        assert!(!changed);
    }

    #[test]
    fn normalize_auth_list_multiple_currents() {
        let raw = vec![
            json!({ "accessToken": "tok1", "current": true, "accountId": "a1" }),
            json!({ "accessToken": "tok2", "current": true, "accountId": "a2" }),
        ];
        let (list, _) = normalize_auth_list(&raw);
        let currents: Vec<_> = list.iter().filter(|e| e["current"].as_bool().unwrap_or(false)).collect();
        assert_eq!(currents.len(), 1);
        assert_eq!(currents[0]["accountId"], "a1");
    }

    #[test]
    fn normalize_auth_list_no_current() {
        let raw = vec![
            json!({ "accessToken": "tok1", "current": false, "accountId": "a1" }),
            json!({ "accessToken": "tok2", "current": false, "accountId": "a2" }),
        ];
        let (list, changed) = normalize_auth_list(&raw);
        assert!(list[0]["current"].as_bool().unwrap());
        assert!(!list[1]["current"].as_bool().unwrap());
        assert!(changed);
    }

    #[test]
    fn normalize_auth_list_empty_token_filtered() {
        let raw = vec![
            json!({ "accessToken": "", "current": true, "accountId": "a1" }),
            json!({ "accessToken": "tok2", "current": false, "accountId": "a2" }),
        ];
        let (list, _) = normalize_auth_list(&raw);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["accountId"], "a2");
    }

    #[test]
    fn normalize_auth_list_empty() {
        let raw: Vec<Value> = vec![];
        let (list, changed) = normalize_auth_list(&raw);
        assert!(list.is_empty());
        assert!(!changed);
    }
}
