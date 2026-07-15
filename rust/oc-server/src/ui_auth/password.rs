//! scrypt 密码哈希。
//!
//! 移植自 `ui-auth.js` lines 616-687。
//! Salt 进程级生成 (非持久化), scrypt 参数 N=16384 r=8 p=1, key length 64。
//! 使用 scrypt crate 的 PHC ($scrypt$...) 格式 + `password_hash` trait。

use scrypt::password_hash::{
    rand_core::OsRng, PasswordHash, PasswordHasher as _, PasswordVerifier as _, SaltString,
};
use scrypt::Scrypt;

/// 密码哈希器。Salt 进程级生成, 预计算 hash。
pub struct PasswordHasher {
    /// PHC 格式的 scrypt 哈希字符串 ($scrypt$...)。
    hash: String,
}

impl PasswordHasher {
    /// 创建新哈希器。密码在进程启动时被 scrypt 哈希。
    ///
    /// 使用 scrypt crate 默认参数 (N=2^15=32768, r=8, p=1)。
    /// 这与 Node 默认 N=16384 略有不同, 但不影响密码验证 — 只要 verify 用同一 hash。
    /// 关键行为一致: 进程级 salt + 预计算 + constant-time compare (Scrypt::verify_password 内部实现)。
    pub fn new(password: &str) -> Self {
        let salt = SaltString::generate(&mut OsRng);
        let hash = Scrypt
            .hash_password(password.as_bytes(), &salt)
            .expect("scrypt hash password")
            .to_string();
        PasswordHasher { hash }
    }

    /// 验证候选密码是否匹配。
    ///
    /// 移植自 `verifyPassword` (ui-auth.js:673-687)。
    pub fn verify(&self, candidate: &str) -> bool {
        let normalized = super::types::normalize_password(candidate);
        if normalized.is_empty() {
            return false;
        }
        let parsed = match PasswordHash::new(&self.hash) {
            Ok(p) => p,
            Err(_) => return false,
        };
        Scrypt.verify_password(normalized.as_bytes(), &parsed).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_password_verify_correct() {
        let hasher = PasswordHasher::new("mypassword");
        assert!(hasher.verify("mypassword"));
    }

    #[test]
    fn test_password_verify_wrong() {
        let hasher = PasswordHasher::new("mypassword");
        assert!(!hasher.verify("wrongpassword"));
    }

    #[test]
    fn test_password_verify_empty() {
        let hasher = PasswordHasher::new("mypassword");
        assert!(!hasher.verify(""));
        assert!(!hasher.verify("   "));
    }

    #[test]
    fn test_password_verify_trimmed() {
        let hasher = PasswordHasher::new("mypassword");
        assert!(hasher.verify("  mypassword  "));
    }

    #[test]
    fn test_password_different_instances() {
        // 两个实例用相同密码, 内部 salt 不同, 但都能验证该密码
        let h1 = PasswordHasher::new("test");
        let h2 = PasswordHasher::new("test");
        assert!(h1.verify("test"));
        assert!(h2.verify("test"));
        // hash 字符串不同 (不同 salt)
        assert_ne!(h1.hash, h2.hash);
    }
}
