//! GitHub settings.json 读写 (与 Node 共享同一 settings 文件)。
//!
//! 移植自 `packages/web/server/lib/github/auth.js` 的 `readSettingsFile` /
//! `writeSettingsFile` 部分。
//!
//! settings 文件: `$OPENCHAMBER_DATA_DIR/settings.json` 或
//! `~/.config/openchamber/settings.json`。
//! 原子写: `.tmp → rename`, mode 0o600。

use std::path::PathBuf;

use serde_json::Value;

use crate::git::paths::home_dir;

/// settings.json 路径。
pub fn settings_file() -> PathBuf {
    data_dir().join("settings.json")
}

/// `~/.config/openchamber` — 对应 Node `OPENCHAMBER_USER_CONFIG_ROOT` (硬编码, 不读 env)。
///
/// 与 `data_dir()` 不同: `data_dir` 读 `OPENCHAMBER_DATA_DIR` env;
/// `user_config_root` 始终是 `~/.config/openchamber`。
/// projects/ 配置文件存放在这里 (Node `OPENCHAMBER_PROJECTS_CONFIG_DIR`)。
pub fn user_config_root() -> PathBuf {
    home_dir().join(".config").join("openchamber")
}

/// OPENCHAMBER_DATA_DIR 或 ~/.config/openchamber。
pub fn data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("OPENCHAMBER_DATA_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    home_dir().join(".config").join("openchamber")
}

/// github-auth.json 路径。
pub fn auth_storage_file() -> PathBuf {
    data_dir().join("github-auth.json")
}

/// 读取 settings.json, 文件不存在/解析失败返回空 `{}`。
pub fn read_settings() -> Value {
    let path = settings_file();
    match std::fs::read_to_string(&path) {
        Ok(content) => serde_json::from_str(&content).unwrap_or(Value::Object(Default::default())),
        Err(_) => Value::Object(Default::default()),
    }
}

/// 原子写入 settings.json, mode 0o600。
pub fn write_settings(settings: &Value) -> oc_core::Result<()> {
    let path = settings_file();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let content = serde_json::to_string_pretty(settings)?;

    // 原子写: .tmp → rename
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

/// 读取 githubClientId: env → settings → default。
pub fn get_github_client_id() -> String {
    if let Ok(raw) = std::env::var("OPENCHAMBER_GITHUB_CLIENT_ID") {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    let settings = read_settings();
    if let Some(stored) = settings.get("githubClientId").and_then(|v| v.as_str()) {
        let trimmed = stored.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    crate::github::DEFAULT_GITHUB_CLIENT_ID.to_string()
}

/// 读取 githubScopes: env → settings → default。
pub fn get_github_scopes() -> String {
    if let Ok(raw) = std::env::var("OPENCHAMBER_GITHUB_SCOPES") {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    let settings = read_settings();
    if let Some(stored) = settings.get("githubScopes").and_then(|v| v.as_str()) {
        let trimmed = stored.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    crate::github::DEFAULT_GITHUB_SCOPES.to_string()
}

/// gh-CLI 是否被禁用: settings.ghCliDisabled。
pub fn is_gh_cli_disabled() -> bool {
    read_settings()
        .get("ghCliDisabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// 设置 gh-CLI 禁用状态。如果禁用, 同时清除 ghCliActive。
pub fn set_gh_cli_disabled(disabled: bool) -> oc_core::Result<()> {
    let mut settings = read_settings();
    if let Value::Object(ref mut map) = settings {
        map.insert("ghCliDisabled".to_string(), Value::Bool(disabled));
        if disabled {
            map.insert("ghCliActive".to_string(), Value::Bool(false));
        }
    }
    write_settings(&settings)
}

/// gh-CLI 是否激活: settings.ghCliActive && !settings.ghCliDisabled。
pub fn is_gh_cli_active() -> bool {
    let settings = read_settings();
    let disabled = settings
        .get("ghCliDisabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let active = settings
        .get("ghCliActive")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    !disabled && active
}

/// 设置 gh-CLI 激活状态。如果 ghCliDisabled, 强制为 false。
pub fn set_gh_cli_active(active: bool) -> oc_core::Result<()> {
    let mut settings = read_settings();
    let disabled = settings
        .get("ghCliDisabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if let Value::Object(ref mut map) = settings {
        map.insert(
            "ghCliActive".to_string(),
            Value::Bool(active && !disabled),
        );
    }
    write_settings(&settings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_id_env_override() {
        std::env::set_var("OPENCHAMBER_GITHUB_CLIENT_ID", "env_client_id");
        assert_eq!(get_github_client_id(), "env_client_id");
        std::env::remove_var("OPENCHAMBER_GITHUB_CLIENT_ID");
    }

    #[test]
    fn client_id_default() {
        std::env::remove_var("OPENCHAMBER_GITHUB_CLIENT_ID");
        // 不依赖 settings.json 内容
        let id = get_github_client_id();
        assert!(!id.is_empty());
    }

    #[test]
    fn scopes_env_override() {
        std::env::set_var("OPENCHAMBER_GITHUB_SCOPES", "repo");
        assert_eq!(get_github_scopes(), "repo");
        std::env::remove_var("OPENCHAMBER_GITHUB_SCOPES");
    }
}
