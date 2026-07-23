//! OpenCode auth.json 读写 + provider CRUD。
//!
//! 对应 Node `opencode/auth.js` (82 行):
//!   - readAuthFile / writeAuthFile / removeProviderAuth / getProviderAuth / listProviderAuths
//!
//! 与 Node API 字节对齐 (暴露全部 public 函数), 不论 Group 2 路由是否用到。
#![allow(dead_code)]
#![allow(unused_imports)]

//! Rust 强化: write 走 `.tmp → rename` 原子写(Node 用 `fs.writeFileSync` 直接写)。
//! 保留 Node 行为: 写前 `copyFileSync(target, target + ".gridforge.backup")` 创建备份。
//! 权限: Unix `0o600`(同 github::settings::write_settings)。

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::opencode::paths::{auth_file, opencode_data_dir};

/// auth.json 读写错误。
#[derive(Debug)]
pub enum AuthError {
    /// 读取失败(非 ENOENT)。
    Read(String),
    /// 写入失败。
    Write(String),
    /// JSON 解析失败。
    Parse(String),
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthError::Read(m) => write!(f, "failed to read OpenCode auth configuration: {}", m),
            AuthError::Write(m) => write!(f, "failed to write OpenCode auth configuration: {}", m),
            AuthError::Parse(m) => write!(f, "failed to parse OpenCode auth configuration: {}", m),
        }
    }
}

impl std::error::Error for AuthError {}

/// auth.json 备份后缀(对齐 Node `auth.js` line 32)。
const BACKUP_SUFFIX: &str = ".gridforge.backup";

/// 读取 auth.json。文件不存在返回空对象;解析失败返回 `AuthError::Parse`。
///
/// 对应 Node `readAuthFile` (`auth.js` line 8-23)。
pub fn read_auth_file() -> Result<Value, AuthError> {
    read_auth_file_at(&auth_file())
}

fn read_auth_file_at(path: &Path) -> Result<Value, AuthError> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(json!({})),
        Err(e) => return Err(AuthError::Read(e.to_string())),
    };
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(trimmed).map_err(|e| AuthError::Parse(e.to_string()))
}

/// 原子写入 auth.json: 创建 dir + 备份 + `.tmp → rename`。
///
/// 对应 Node `writeAuthFile` (`auth.js` line 25-43)。Rust 强化:
///   1. Node 直接 `fs.writeFileSync`;Rust 走 `.tmp → rename` 原子写
///   2. Node 不设权限;Rust 设 Unix `0o600`
pub fn write_auth_file(auth: &Value) -> Result<(), AuthError> {
    let path = auth_file();
    write_auth_file_at(auth, &path)
}

fn write_auth_file_at(auth: &Value, path: &Path) -> Result<(), AuthError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| AuthError::Write(format!("create_dir_all({}): {}", parent.display(), e)))?;
    }

    // 备份(对齐 Node 行为): 文件存在时复制为 <path>.gridforge.backup
    if path.exists() {
        let backup = format!("{}{}", path.display(), BACKUP_SUFFIX);
        std::fs::copy(path, &backup)
            .map_err(|e| AuthError::Write(format!("backup to {}: {}", backup, e)))?;
    }

    let content = serde_json::to_string_pretty(auth)
        .map_err(|e| AuthError::Write(format!("serialize: {}", e)))?;

    // 原子写: .tmp-{pid}-{ts}-{rand} → rename
    let tmp_path = format!(
        "{}.tmp-{}-{}-{}",
        path.display(),
        std::process::id(),
        chrono::Utc::now().timestamp_millis(),
        rand::random::<u32>(),
    );
    let tmp = PathBuf::from(&tmp_path);

    if let Err(e) = std::fs::write(&tmp, &content) {
        let _ = std::fs::remove_file(&tmp);
        return Err(AuthError::Write(format!("write tmp {}: {}", tmp.display(), e)));
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }

    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(AuthError::Write(format!("rename {} → {}: {}", tmp.display(), path.display(), e)));
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }

    let _ = opencode_data_dir(); // 触达 module,确保 import 不被 unused 警告
    Ok(())
}

/// 读取单个 provider 的 auth entry。
///
/// 对应 Node `getProviderAuth` (`auth.js` line 63-66)。
pub fn get_provider_auth(provider_id: &str) -> Result<Option<Value>, AuthError> {
    let auth = read_auth_file()?;
    Ok(auth.get(provider_id).cloned())
}

/// 列出所有有 auth 的 provider ID。
///
/// 对应 Node `listProviderAuths` (`auth.js` line 68-71)。
pub fn list_provider_auths() -> Result<Vec<String>, AuthError> {
    let auth = read_auth_file()?;
    Ok(auth
        .as_object()
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default())
}

/// 移除单个 provider 的 auth entry。返回是否实际移除。
///
/// 对应 Node `removeProviderAuth` (`auth.js` line 45-61)。
pub fn remove_provider_auth(provider_id: &str) -> Result<bool, AuthError> {
    if provider_id.is_empty() {
        return Err(AuthError::Write("provider ID is required".to_string()));
    }
    let mut auth = read_auth_file()?;
    let removed = auth.as_object_mut().and_then(|m| m.remove(provider_id)).is_some();
    if removed {
        write_auth_file(&auth)?;
    }
    Ok(removed)
}

/// 设置单个 provider 的 auth entry(Rust 内部 helper,无 Node 对应)。
pub fn set_provider_auth(provider_id: &str, entry: &Value) -> Result<(), AuthError> {
    if provider_id.is_empty() {
        return Err(AuthError::Write("provider ID is required".to_string()));
    }
    let mut auth = read_auth_file()?;
    if let Some(obj) = auth.as_object_mut() {
        obj.insert(provider_id.to_string(), entry.clone());
    } else {
        let mut new_obj = serde_json::Map::new();
        new_obj.insert(provider_id.to_string(), entry.clone());
        auth = Value::Object(new_obj);
    }
    write_auth_file(&auth)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::env;
    use std::sync::Mutex;

    /// 测试串行化:所有 auth/config 测试都用同一份 home env var,串行跑避免污染。
    /// 也用于跨模块串行化(auth/config/models_metadata 都改 home env var)。
    pub(crate) static TEST_LOCK: Mutex<()> = Mutex::new(());

    /// 设置临时 home 目录,返回 guard 在 drop 时恢复。
    ///
    /// 注意:读取/写入的 env var 由 [`crate::git::paths::home_env_var_name()`] 决定
    /// (Windows=`USERPROFILE`,其他=`HOME`),与生产代码 [`home_dir_string`] 对齐。
    pub(crate) struct HomeGuard {
        prev: Option<String>,
        temp: PathBuf,
    }
    impl Drop for HomeGuard {
        fn drop(&mut self) {
            let var_name = crate::git::paths::home_env_var_name();
            match &self.prev {
                Some(v) => env::set_var(var_name, v),
                None => env::remove_var(var_name),
            }
            let _ = std::fs::remove_dir_all(&self.temp);
        }
    }

    pub(crate) fn set_temp_home() -> (HomeGuard, std::sync::MutexGuard<'static, ()>) {
        let guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let var_name = crate::git::paths::home_env_var_name();
        let prev = env::var(var_name).ok();
        let temp = std::env::temp_dir().join(format!(
            "oc-auth-test-{}-{}",
            std::process::id(),
            rand::random::<u32>()
        ));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        env::set_var(var_name, &temp);
        (HomeGuard { prev, temp }, guard)
    }

    #[test]
    fn read_empty_file_returns_empty_object() {
        let (_home, _lock) = set_temp_home();
        let result = read_auth_file().unwrap();
        assert_eq!(result, json!({}));
    }

    #[test]
    fn read_valid_json_round_trip() {
        let (_home, _lock) = set_temp_home();
        let initial = json!({"openai": {"type": "oauth", "access_token": "x"}});
        write_auth_file(&initial).unwrap();
        let read = read_auth_file().unwrap();
        assert_eq!(read, initial);
    }

    #[test]
    fn read_invalid_json_returns_parse_error() {
        let (_home, _lock) = set_temp_home();
        // 写入非法 JSON
        let path = auth_file();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{not json").unwrap();
        let result = read_auth_file();
        assert!(matches!(result, Err(AuthError::Parse(_))));
    }

    #[test]
    fn write_creates_dir_if_missing() {
        let (_home, _lock) = set_temp_home();
        // opencode_data_dir 还不存在
        assert!(!opencode_data_dir().exists());
        write_auth_file(&json!({})).unwrap();
        assert!(opencode_data_dir().exists());
        assert!(auth_file().exists());
    }

    #[test]
    fn write_creates_backup_if_file_exists() {
        let (_home, _lock) = set_temp_home();
        write_auth_file(&json!({"v": 1})).unwrap();
        write_auth_file(&json!({"v": 2})).unwrap();
        let backup = format!("{}.gridforge.backup", auth_file().display());
        assert!(PathBuf::from(&backup).exists());
        // 备份内容是旧值
        let backup_content = std::fs::read_to_string(&backup).unwrap();
        assert!(backup_content.contains("\"v\": 1"));
    }

    #[test]
    fn provider_crud_round_trip() {
        let (_home, _lock) = set_temp_home();
        // 初始空
        assert_eq!(list_provider_auths().unwrap(), Vec::<String>::new());
        assert!(get_provider_auth("openai").unwrap().is_none());

        // set
        set_provider_auth("openai", &json!({"type": "oauth"})).unwrap();
        assert_eq!(list_provider_auths().unwrap(), vec!["openai".to_string()]);
        assert_eq!(get_provider_auth("openai").unwrap(), Some(json!({"type": "oauth"})));

        // update
        set_provider_auth("openai", &json!({"type": "api", "key": "k"})).unwrap();
        assert_eq!(get_provider_auth("openai").unwrap(), Some(json!({"type": "api", "key": "k"})));

        // remove
        assert!(remove_provider_auth("openai").unwrap());
        assert!(get_provider_auth("openai").unwrap().is_none());
        // remove 不存在的 provider → false,无 error
        assert!(!remove_provider_auth("nonexistent").unwrap());
    }
}
