//! JWT secret 文件管理。
//!
//! 移植自 `ui-auth.js` lines 365-406。
//! Secret 存储为 hex string (32 bytes → 64 chars), 文件 mode 0o600。

use std::path::PathBuf;

/// JWT secret 文件路径: `$DATA_DIR/jwt-secret`。
fn jwt_secret_file() -> PathBuf {
    crate::github::settings::data_dir().join("jwt-secret")
}

/// 获取或创建 JWT secret, 返回 secret string 的 UTF-8 字节。
///
/// 优先级: `OPENCODE_JWT_SECRET` env → 文件 → 生成新 secret。
/// 移植自 `getOrCreateJwtSecret` (ui-auth.js:370-394)。
pub fn get_or_create_jwt_secret() -> Vec<u8> {
    // 1. Env 覆盖
    if let Ok(env_secret) = std::env::var("OPENCODE_JWT_SECRET") {
        if !env_secret.is_empty() {
            return env_secret.into_bytes();
        }
    }

    // 2. 从文件读取
    let path = jwt_secret_file();
    if let Ok(content) = std::fs::read_to_string(&path) {
        let trimmed = content.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string().into_bytes();
        }
    }

    // 3. 生成新 secret
    let secret = generate_random_hex(32);
    match persist_secret_to_file(&secret) {
        Ok(()) => {
            tracing::info!("[JWT] Generated and persisted new secret to {}", path.display());
        }
        Err(e) => {
            tracing::warn!("[JWT] Failed to persist secret: {e}");
        }
    }
    secret.into_bytes()
}

/// 持久化新 JWT secret (全局登出旋转)。
///
/// 如果 `OPENCODE_JWT_SECRET` 已设置, 返回 Err (不支持旋转)。
/// 移植自 `persistJwtSecret` (ui-auth.js:396-406)。
pub fn persist_jwt_secret(secret_hex: &str) -> Result<Vec<u8>, std::io::Error> {
    if std::env::var("OPENCODE_JWT_SECRET").is_ok() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Global sign-out is unavailable while OPENCODE_JWT_SECRET is set",
        ));
    }

    persist_secret_to_file(secret_hex)?;
    Ok(secret_hex.to_string().into_bytes())
}

fn persist_secret_to_file(secret_hex: &str) -> Result<(), std::io::Error> {
    let path = jwt_secret_file();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, secret_hex)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// 生成 n 个随机字节的 hex 编码。
fn generate_random_hex(n: usize) -> String {
    use rand::RngCore;
    let mut bytes = vec![0u8; n];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex_encode(&bytes)
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut result = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        result.push_str(&format!("{b:02x}"));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_random_hex_length() {
        let hex = generate_random_hex(32);
        assert_eq!(hex.len(), 64);
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_generate_random_hex_uniqueness() {
        let a = generate_random_hex(32);
        let b = generate_random_hex(32);
        assert_ne!(a, b);
    }
}
