//! Quota credentials 文件读写 (atomic write, mode 0o600)。
//!
//! 对应 Node `quota/credentials/store.js`。
//!
//! 文件位置:
//!   - `OPENCHAMBER_DATA_DIR` → `<dir>/quota/<provider>.json`
//!   - 默认 `~/.config/openchamber/quota/<provider>.json`
//!
//! 只有 `opencode-go / ollama-cloud / cursor` 三类 managed provider。

use std::path::PathBuf;

use serde_json::Value;

pub const MANAGED_QUOTA_PROVIDERS: &[&str] = &["opencode-go", "ollama-cloud", "cursor"];

/// 错误类型。
#[derive(Debug)]
pub enum CredentialError {
    /// 不支持的 provider ID。
    Unsupported(String),
    /// IO 错误。
    Io(#[allow(dead_code)] String),
    /// JSON 错误。
    Serde(#[allow(dead_code)] String),
}

impl std::fmt::Display for CredentialError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CredentialError::Unsupported(p) => write!(f, "Unsupported credential provider: {p}"),
            CredentialError::Io(m) => write!(f, "io error: {m}"),
            CredentialError::Serde(m) => write!(f, "serde error: {m}"),
        }
    }
}

impl std::error::Error for CredentialError {}

impl From<std::io::Error> for CredentialError {
    fn from(e: std::io::Error) -> Self {
        CredentialError::Io(e.to_string())
    }
}

impl From<serde_json::Error> for CredentialError {
    fn from(e: serde_json::Error) -> Self {
        CredentialError::Serde(e.to_string())
    }
}

/// `~/.config/openchamber/quota` 目录(或 `$OPENCHAMBER_DATA_DIR/quota`)。
pub fn credentials_directory() -> PathBuf {
    let base = if let Ok(dir) = std::env::var("OPENCHAMBER_DATA_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir).join("quota");
        }
        crate::git::paths::home_dir().join(".config").join("openchamber")
    } else {
        crate::git::paths::home_dir().join(".config").join("openchamber")
    };
    base.join("quota")
}

fn credential_path(provider_id: &str) -> Result<PathBuf, CredentialError> {
    if !MANAGED_QUOTA_PROVIDERS.contains(&provider_id) {
        return Err(CredentialError::Unsupported(provider_id.to_string()));
    }
    Ok(credentials_directory().join(format!("{provider_id}.json")))
}

/// 读取并 normalize credential。
///
/// 文件不存在/解析失败时返回 `None`(ENOENT 不报错)。
pub fn read_quota_credential<F>(provider_id: &str, normalize: F) -> Option<Value>
where
    F: FnOnce(Value) -> Option<Value>,
{
    let path = credential_path(provider_id).ok()?;
    let raw = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::warn!(provider = %provider_id, error = %e, "failed to read quota credentials");
            return None;
        }
    };
    let parsed: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(_) => return None,
    };
    normalize(parsed)
}

/// 原子写入 credential: `.tmp-{pid}-{ts}` → rename, mode 0o600。
pub fn write_quota_credential(provider_id: &str, credential: &Value) -> Result<(), CredentialError> {
    let target = credential_path(provider_id)?;
    let dir = target.parent().ok_or(CredentialError::Io("no parent dir".into()))?;
    std::fs::create_dir_all(dir)?;
    set_mode_0o700(dir);

    let tmp_path = format!(
        "{}.{}.{}.tmp",
        target.display(),
        std::process::id(),
        chrono::Utc::now().timestamp_millis(),
    );

    let content = serde_json::to_string_pretty(credential)?;
    let body = format!("{content}\n");

    // 写入 tmp
    std::fs::write(&tmp_path, &body)?;
    set_mode_0o600(std::path::Path::new(&tmp_path));

    // rename (atomic on POSIX)
    if let Err(e) = std::fs::rename(&tmp_path, &target) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e.into());
    }
    set_mode_0o600(&target);

    Ok(())
}

/// 删除 credential 文件 (ENOENT 不报错)。
pub fn delete_quota_credential(provider_id: &str) -> Result<(), CredentialError> {
    let path = credential_path(provider_id)?;
    match std::fs::remove_file(&path) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

#[cfg(unix)]
fn set_mode_0o700(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
}

#[cfg(unix)]
fn set_mode_0o600(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn set_mode_0o700(_path: &std::path::Path) {}
#[cfg(not(unix))]
fn set_mode_0o600(_path: &std::path::Path) {}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::opencode::auth::tests as auth_tests;
    use serde_json::json;

    pub(crate) fn set_temp_home() -> (
        auth_tests::HomeGuard,
        std::sync::MutexGuard<'static, ()>,
    ) {
        auth_tests::set_temp_home()
    }

    fn unique_dir() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "quota-store-{}-{}",
            std::process::id(),
            rand::random::<u32>()
        ))
    }

    /// 强制目录用临时位置 (绕开 HOME env,直接覆盖内部变量)。
    fn with_temp_data_dir<F: FnOnce()>(f: F) {
        let prev = std::env::var("OPENCHAMBER_DATA_DIR").ok();
        let dir = unique_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("OPENCHAMBER_DATA_DIR", &dir);
        f();
        match prev {
            Some(v) => std::env::set_var("OPENCHAMBER_DATA_DIR", v),
            None => std::env::remove_var("OPENCHAMBER_DATA_DIR"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_nonexistent_returns_none() {
        with_temp_data_dir(|| {
            assert!(read_quota_credential(
                "opencode-go",
                |v| Some(v),
            )
            .is_none());
        });
    }

    #[test]
    fn write_read_roundtrip() {
        with_temp_data_dir(|| {
            let cred = json!({"workspaceId": "ws1", "authCookie": "ck1"});
            write_quota_credential("opencode-go", &cred).unwrap();
            let read = read_quota_credential("opencode-go", |v| Some(v)).unwrap();
            assert_eq!(read, cred);
        });
    }

    #[test]
    fn write_creates_dir_with_0700_mode() {
        with_temp_data_dir(|| {
            let cred = json!({"cookie": "abc"});
            write_quota_credential("ollama-cloud", &cred).unwrap();
            let dir = credentials_directory();
            assert!(dir.exists());
            let perms = std::fs::metadata(&dir).unwrap().permissions();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(perms.mode() & 0o777, 0o700);
            }
            let _ = perms;
        });
    }

    #[test]
    fn delete_ignores_enoent() {
        with_temp_data_dir(|| {
            // 文件不存在 -> OK
            delete_quota_credential("cursor").unwrap();
            // 删除已存在文件 -> OK
            write_quota_credential("cursor", &json!({"a": "b"})).unwrap();
            delete_quota_credential("cursor").unwrap();
            // 已删除时再次调用 -> OK
            delete_quota_credential("cursor").unwrap();
        });
    }

    #[test]
    fn unsupported_provider_rejected() {
        with_temp_data_dir(|| {
            let r = write_quota_credential("not-a-managed", &json!({}));
            assert!(r.is_err());
            let r = read_quota_credential("not-a-managed", |v| Some(v));
            assert!(r.is_none());
            let r = delete_quota_credential("not-a-managed");
            assert!(r.is_err());
        });
    }

    #[test]
    #[cfg(unix)]
    fn file_mode_is_0o600() {
        with_temp_data_dir(|| {
            write_quota_credential("cursor", &json!({"a": "b"})).unwrap();
            let path = credential_path("cursor").unwrap();
            let perms = std::fs::metadata(&path).unwrap().permissions();
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(perms.mode() & 0o777, 0o600);
        });
    }
}
