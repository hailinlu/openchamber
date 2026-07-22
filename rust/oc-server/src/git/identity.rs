//! Git 身份管理 — identity profiles CRUD + global identity + discover credentials。
//!
//! 移植自 `packages/web/server/lib/git/identity-storage.js` 和 `credentials.js`。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::git::paths::home_dir;
use crate::git::runner::GitRunner;

// ============================================================
// Identity Profiles (identity-storage.js)
// ============================================================

/// Identity profile 存储结构。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfilesData {
    #[serde(default)]
    profiles: Vec<Value>,
}

/// 存储文件路径: ~/.config/gridforge/git-identities.json
fn storage_file() -> PathBuf {
    home_dir()
        .join(".config")
        .join("gridforge")
        .join("git-identities.json")
}

/// 确保存储目录存在。
fn ensure_storage_dir() -> oc_core::Result<()> {
    let path = storage_file();
    let dir = path
        .parent()
        .ok_or_else(|| oc_core::Error::Internal("invalid storage path".to_string()))?;
    std::fs::create_dir_all(dir)?;
    Ok(())
}

/// 加载所有 profiles。
pub fn load_profiles() -> ProfilesData {
    let path = storage_file();
    if !path.exists() {
        return ProfilesData { profiles: Vec::new() };
    }

    match std::fs::read_to_string(&path) {
        Ok(content) => {
            serde_json::from_str::<ProfilesData>(&content).unwrap_or(ProfilesData { profiles: Vec::new() })
        }
        Err(_) => ProfilesData { profiles: Vec::new() },
    }
}

/// 保存 profiles。
pub fn save_profiles(data: &ProfilesData) -> oc_core::Result<()> {
    ensure_storage_dir()?;
    let path = storage_file();
    let content = serde_json::to_string_pretty(data)?;
    std::fs::write(&path, content)?;
    Ok(())
}

/// 获取所有 profiles (返回 Vec<Value>)。
pub fn get_profiles() -> Vec<Value> {
    load_profiles().profiles
}

/// 获取单个 profile by id。
pub fn get_profile(id: &str) -> Option<Value> {
    let profiles = get_profiles();
    profiles.into_iter().find(|p| {
        p.get("id")
            .and_then(|v| v.as_str())
            .map(|s| s == id)
            .unwrap_or(false)
    })
}

/// 创建新 profile, 与 Node `createProfile` 对齐。
pub fn create_profile(profile_data: &Value) -> oc_core::Result<Value> {
    let id = profile_data
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let user_name = profile_data
        .get("userName")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let user_email = profile_data
        .get("userEmail")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if id.is_empty() || user_name.is_empty() || user_email.is_empty() {
        return Err(oc_core::Error::BadRequest(
            "Profile must have id, userName, and userEmail".to_string(),
        ));
    }

    let mut data = load_profiles();

    // 检查重复 ID
    if data.profiles.iter().any(|p| {
        p.get("id")
            .and_then(|v| v.as_str())
            .map(|s| s == id)
            .unwrap_or(false)
    }) {
        return Err(oc_core::Error::BadRequest(format!(
            "Profile with ID \"{id}\" already exists"
        )));
    }

    // 构建新 profile (带默认值)
    let new_profile = serde_json::json!({
        "id": id,
        "name": profile_data.get("name").and_then(|v| v.as_str()).unwrap_or(user_name),
        "userName": user_name,
        "userEmail": user_email,
        "authType": profile_data.get("authType").and_then(|v| v.as_str()).unwrap_or("ssh"),
        "sshKey": profile_data.get("sshKey").filter(|v| !v.is_null()),
        "signCommits": profile_data.get("signCommits").filter(|v| !v.is_null()),
        "signingKey": profile_data.get("signingKey").filter(|v| !v.is_null()),
        "host": profile_data.get("host").filter(|v| !v.is_null()),
        "color": profile_data.get("color").and_then(|v| v.as_str()).unwrap_or("keyword"),
        "icon": profile_data.get("icon").and_then(|v| v.as_str()).unwrap_or("branch"),
    });

    data.profiles.push(new_profile.clone());
    save_profiles(&data)?;
    Ok(new_profile)
}

/// 更新 profile by id, 与 Node `updateProfile` 对齐。
pub fn update_profile(id: &str, updates: &Value) -> oc_core::Result<Value> {
    let mut data = load_profiles();

    let index = data.profiles.iter().position(|p| {
        p.get("id")
            .and_then(|v| v.as_str())
            .map(|s| s == id)
            .unwrap_or(false)
    });

    let index = match index {
        Some(i) => i,
        None => {
            return Err(oc_core::Error::BadRequest(format!(
                "Profile with ID \"{id}\" not found"
            )))
        }
    };

    // 合并 updates, 但 id 不变
    let original = &data.profiles[index];
    let mut merged = original.clone();
    if let Some(obj) = updates.as_object() {
        if let Some(merged_obj) = merged.as_object_mut() {
            for (key, value) in obj {
                if key != "id" {
                    merged_obj.insert(key.clone(), value.clone());
                }
            }
        }
    }

    data.profiles[index] = merged.clone();
    save_profiles(&data)?;
    Ok(merged)
}

/// 删除 profile by id, 与 Node `deleteProfile` 对齐。
pub fn delete_profile(id: &str) -> oc_core::Result<bool> {
    let mut data = load_profiles();
    let original_len = data.profiles.len();

    data.profiles.retain(|p| {
        p.get("id")
            .and_then(|v| v.as_str())
            .map(|s| s != id)
            .unwrap_or(true)
    });

    if data.profiles.len() == original_len {
        return Err(oc_core::Error::BadRequest(format!(
            "Profile with ID \"{id}\" not found"
        )));
    }

    save_profiles(&data)?;
    Ok(true)
}

// ============================================================
// Global Identity (service.js: getGlobalIdentity)
// ============================================================

/// 获取全局 git 身份 (user.name, user.email, core.sshCommand)。
///
/// 与 Node `getGlobalIdentity` 对齐。
pub async fn get_global_identity() -> oc_core::Result<Value> {
    let home = home_dir();

    let user_name = GitRunner::run(&home, &["config", "--get", "user.name"])
        .await
        .stdout_text();
    let user_email = GitRunner::run(&home, &["config", "--get", "user.email"])
        .await
        .stdout_text();
    let ssh_command = GitRunner::run(&home, &["config", "--get", "core.sshCommand"])
        .await
        .stdout_text();

    Ok(serde_json::json!({
        "userName": user_name,
        "userEmail": user_email,
        "sshCommand": ssh_command,
    }))
}

/// 获取当前仓库身份 (local 优先, fallback global)。
///
/// 与 Node `getCurrentIdentity` 对齐。
pub async fn get_current_identity(directory: &str) -> oc_core::Result<Value> {
    let dir = crate::git::paths::normalize_directory_path(directory);
    let dir_path = PathBuf::from(&dir);

    // 先尝试 local
    let local_name = GitRunner::run(&dir_path, &["config", "--local", "--get", "user.name"])
        .await
        .stdout_text();
    let local_email = GitRunner::run(&dir_path, &["config", "--local", "--get", "user.email"])
        .await
        .stdout_text();
    let local_ssh = GitRunner::run(&dir_path, &["config", "--local", "--get", "core.sshCommand"])
        .await
        .stdout_text();

    let is_local = !local_name.is_empty() || !local_email.is_empty();

    if is_local {
        return Ok(serde_json::json!({
            "userName": local_name,
            "userEmail": local_email,
            "sshCommand": local_ssh,
            "isLocal": true,
        }));
    }

    // fallback global
    let global = get_global_identity().await?;
    let mut result = global;
    result["isLocal"] = serde_json::Value::Bool(false);
    Ok(result)
}

/// 检查仓库是否有 local 身份。
///
/// 与 Node `hasLocalIdentity` 对齐。
pub async fn has_local_identity(directory: &str) -> oc_core::Result<bool> {
    let dir = crate::git::paths::normalize_directory_path(directory);
    let dir_path = PathBuf::from(&dir);

    let result = GitRunner::run(&dir_path, &["config", "--local", "user.name"]).await;
    Ok(result.success && !result.stdout_text().is_empty())
}

/// 设置 local 身份从 profile。
///
/// 与 Node `setLocalIdentity` 对齐。
pub async fn set_local_identity(directory: &str, profile: &Value) -> oc_core::Result<Value> {
    let dir = crate::git::paths::normalize_directory_path(directory);
    let dir_path = PathBuf::from(&dir);

    let user_name = profile
        .get("userName")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let user_email = profile
        .get("userEmail")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if user_name.is_empty() || user_email.is_empty() {
        return Err(oc_core::Error::BadRequest(
            "userName and userEmail are required".to_string(),
        ));
    }

    // 设置 user.name 和 user.email
    let result = GitRunner::run(&dir_path, &["config", "--local", "user.name", user_name]).await;
    if !result.success {
        return Err(oc_core::Error::Internal(format!(
            "Failed to set user.name: {}",
            result.stderr_text()
        )));
    }

    let result = GitRunner::run(&dir_path, &["config", "--local", "user.email", user_email]).await;
    if !result.success {
        return Err(oc_core::Error::Internal(format!(
            "Failed to set user.email: {}",
            result.stderr_text()
        )));
    }

    // 设置 sshCommand (如果有 sshKey)
    if let Some(ssh_key) = profile.get("sshKey").and_then(|v| v.as_str()) {
        if !ssh_key.is_empty() {
            let ssh_cmd = format!("ssh -i {ssh_key} -o IdentitiesOnly=yes");
            let _ = GitRunner::run(&dir_path, &["config", "--local", "core.sshCommand", &ssh_cmd])
                .await;
        }
    }

    Ok(serde_json::json!({ "success": true, "profile": profile }))
}

// ============================================================
// Credentials Discovery (credentials.js)
// ============================================================

/// `~/.git-credentials` 路径。
fn git_credentials_path() -> PathBuf {
    home_dir().join(".git-credentials")
}

/// 解析 `~/.git-credentials` 文件, 返回 {host, username} 列表。
///
/// 与 Node `discoverGitCredentials` 对齐。
pub fn discover_git_credentials() -> Vec<Value> {
    let path = git_credentials_path();
    if !path.exists() {
        return Vec::new();
    }

    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    let mut credentials = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // 解析 URL: https://username:token@hostname/path 或 git://username@hostname
        if let Some(parsed) = parse_git_credential_url(trimmed) {
            // 去重
            let exists = credentials.iter().any(|c: &Value| {
                c.get("host") == parsed.get("host") && c.get("username") == parsed.get("username")
            });
            if !exists {
                credentials.push(parsed);
            }
        }
    }

    credentials
}

/// 解析单行 git credential URL。
///
/// 格式: `https://username:token@hostname/path`
/// 返回: `{ "host": "hostname/path", "username": "username" }`
fn parse_git_credential_url(line: &str) -> Option<Value> {
    // 简单 URL 解析: scheme://userinfo@host/path
    let scheme_end = line.find("://")?;
    let after_scheme = &line[scheme_end + 3..];

    // 分割 userinfo 和 host/path
    let (userinfo, host_part) = if let Some(at_idx) = after_scheme.find('@') {
        (Some(&after_scheme[..at_idx]), &after_scheme[at_idx + 1..])
    } else {
        (None, after_scheme)
    };

    // 提取 username (userinfo 中冒号前的部分)
    let username = userinfo
        .map(|ui| ui.split(':').next().unwrap_or(""))
        .unwrap_or("");

    // host = hostname + path (去掉 path 中的 / 之前的部分保留)
    // Node 逻辑: host = hostname + pathname (如果不是 "/" 的话)
    let host = if let Some(slash_idx) = host_part.find('/') {
        let hostname = &host_part[..slash_idx];
        let pathname = &host_part[slash_idx..];
        // pathname 不是 "/" 时才追加
        if pathname != "/" {
            format!("{hostname}{pathname}")
        } else {
            hostname.to_string()
        }
    } else {
        host_part.to_string()
    };

    if host.is_empty() || username.is_empty() {
        return None;
    }

    Some(serde_json::json!({
        "host": host,
        "username": username,
    }))
}

/// 获取指定 host 的 credential (username + token)。
///
/// 与 Node `getCredentialForHost` 对齐。
pub fn get_credential_for_host(host: &str) -> Option<Value> {
    let path = git_credentials_path();
    if !path.exists() {
        return None;
    }

    let content = std::fs::read_to_string(&path).ok()?;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if let Some(parsed) = parse_git_credential_url_with_token(trimmed) {
            if parsed.get("host").and_then(|v| v.as_str()) == Some(host) {
                return Some(serde_json::json!({
                    "username": parsed.get("username"),
                    "token": parsed.get("token"),
                }));
            }
        }
    }

    None
}

/// 解析 credential URL (包含 token), 内部使用。
fn parse_git_credential_url_with_token(line: &str) -> Option<Value> {
    let scheme_end = line.find("://")?;
    let after_scheme = &line[scheme_end + 3..];

    let (userinfo, host_part) = if let Some(at_idx) = after_scheme.find('@') {
        (Some(&after_scheme[..at_idx]), &after_scheme[at_idx + 1..])
    } else {
        (None, after_scheme)
    };

    let (username, token) = if let Some(ui) = userinfo {
        let mut parts = ui.splitn(2, ':');
        let u = parts.next().unwrap_or("");
        let t = parts.next().unwrap_or("");
        (u.to_string(), t.to_string())
    } else {
        (String::new(), String::new())
    };

    let host = if let Some(slash_idx) = host_part.find('/') {
        let hostname = &host_part[..slash_idx];
        let pathname = &host_part[slash_idx..];
        if pathname != "/" {
            format!("{hostname}{pathname}")
        } else {
            hostname.to_string()
        }
    } else {
        host_part.to_string()
    };

    Some(serde_json::json!({
        "host": host,
        "username": username,
        "token": token,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_git_credential_url_basic() {
        let result = parse_git_credential_url("https://alice:token123@github.com/user/repo").unwrap();
        assert_eq!(result["host"], "github.com/user/repo");
        assert_eq!(result["username"], "alice");
    }

    #[test]
    fn test_parse_git_credential_url_no_path() {
        let result = parse_git_credential_url("https://bob@gitlab.com").unwrap();
        assert_eq!(result["host"], "gitlab.com");
        assert_eq!(result["username"], "bob");
    }

    #[test]
    fn test_parse_git_credential_url_no_username() {
        let result = parse_git_credential_url("https://github.com/user/repo");
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_git_credential_url_with_token() {
        let result =
            parse_git_credential_url_with_token("https://alice:ghp_token@github.com/repo").unwrap();
        assert_eq!(result["host"], "github.com/repo");
        assert_eq!(result["username"], "alice");
        assert_eq!(result["token"], "ghp_token");
    }

    #[test]
    fn test_create_profile_validation() {
        // 空 ID
        let result = create_profile(&serde_json::json!({
            "id": "",
            "userName": "Alice",
            "userEmail": "alice@example.com"
        }));
        assert!(result.is_err());

        // 缺 userEmail
        let result = create_profile(&serde_json::json!({
            "id": "test",
            "userName": "Alice",
        }));
        assert!(result.is_err());
    }
}
