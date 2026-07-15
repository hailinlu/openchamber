//! Session JWT 签发/验证 + cookie。
//!
//! 移植自 `ui-auth.js` lines 689-710, 650-671。
//! HS256 JWT, claims `{ type: "ui-session", exp, iat }`。
//! JWT secret 是 hex string 的 UTF-8 字节 (与 jose 一致)。

use std::sync::RwLock;

use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};

use super::{
    SESSION_TTL_MS, TRUSTED_DEVICE_SESSION_TTL_MS,
    types::build_cookie,
};

/// JWT claims (移植自 `SignJWT({ type: 'ui-session' })`)。
#[derive(Debug, Serialize, Deserialize)]
struct SessionClaims {
    #[serde(rename = "type")]
    claims_type: String,
    exp: usize, // epoch seconds
    iat: usize, // epoch seconds
}

/// Session 管理器。
pub struct SessionManager {
    cookie_name: String,
    // RwLock 因为 rotate 时需要更新 secret
    encoding_key: RwLock<EncodingKey>,
    decoding_key: RwLock<DecodingKey>,
}

impl SessionManager {
    pub fn new(cookie_name: String, jwt_secret: Vec<u8>) -> Self {
        let enc_key = EncodingKey::from_secret(&jwt_secret);
        let dec_key = DecodingKey::from_secret(&jwt_secret);
        SessionManager {
            cookie_name,
            encoding_key: RwLock::new(enc_key),
            decoding_key: RwLock::new(dec_key),
        }
    }

    /// 更新 JWT secret (用于 reset_auth 旋转)。
    pub fn update_secret(&self, jwt_secret: Vec<u8>) {
        let enc_key = EncodingKey::from_secret(&jwt_secret);
        let dec_key = DecodingKey::from_secret(&jwt_secret);
        *self.encoding_key.write().unwrap() = enc_key;
        *self.decoding_key.write().unwrap() = dec_key;
    }

    /// 签发 session JWT (移植自 `issueSession`, ui-auth.js:701-710)。
    ///
    /// TTL: 12h (normal) / 7d (trusted device)。
    pub fn issue_session(&self, trust_device: bool) -> Result<String, String> {
        let ttl_ms = resolve_session_ttl_ms(trust_device);
        let now_secs = now_secs();
        let claims = SessionClaims {
            claims_type: "ui-session".to_string(),
            exp: now_secs + (ttl_ms / 1000) as usize,
            iat: now_secs,
        };
        let header = Header::new(Algorithm::HS256);
        let key = self.encoding_key.read().unwrap();
        encode(&header, &claims, &key).map_err(|e| format!("JWT encode error: {e}"))
    }

    /// 验证 session JWT (移植自 `isSessionValid`, ui-auth.js:689-699)。
    pub fn is_session_valid(&self, token: &str) -> bool {
        if token.is_empty() {
            return false;
        }
        let key = self.decoding_key.read().unwrap();
        let mut validation = Validation::new(Algorithm::HS256);
        validation.validate_exp = true;
        decode::<SessionClaims>(token, &key, &validation).is_ok()
    }

    /// 构建 session Set-Cookie header (移植自 `setSessionCookie`, ui-auth.js:650-660)。
    pub fn build_session_cookie(&self, token: &str, ttl_ms: i64, secure: bool) -> String {
        let max_age_seconds = ttl_ms / 1000;
        let encoded_token = url_encode(token);
        build_cookie(&self.cookie_name, &encoded_token, max_age_seconds, secure)
    }

    /// 构建清除 session 的 Set-Cookie header (移植自 `clearSessionCookie`, ui-auth.js:662-671)。
    pub fn build_clear_cookie(&self, secure: bool) -> String {
        build_cookie(&self.cookie_name, "", 0, secure)
    }
}

/// 计算 session TTL (移植自 `resolveSessionTtlMs`, ui-auth.js:620)。
pub fn resolve_session_ttl_ms(trust_device: bool) -> i64 {
    if trust_device {
        TRUSTED_DEVICE_SESSION_TTL_MS
    } else {
        SESSION_TTL_MS
    }
}

fn now_secs() -> usize {
    chrono::Utc::now().timestamp() as usize
}

/// 对 token 进行 URL 编码 (对应 `encodeURIComponent`)。
/// 只编码需要转义的字符: 非字母数字且非 `-_.!~*'()` 的字符。
fn url_encode(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    for &b in input.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')') {
            result.push(b as char);
        } else {
            result.push_str(&format!("%{b:02X}"));
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_manager() -> SessionManager {
        SessionManager::new(
            "oc_ui_session".to_string(),
            b"test-secret-32-bytes-hex-string!!".to_vec(),
        )
    }

    #[test]
    fn test_issue_and_validate_session() {
        let mgr = test_manager();
        let token = mgr.issue_session(false).unwrap();
        assert!(mgr.is_session_valid(&token));
    }

    #[test]
    fn test_validate_empty() {
        let mgr = test_manager();
        assert!(!mgr.is_session_valid(""));
        assert!(!mgr.is_session_valid("invalid.token.here"));
    }

    #[test]
    fn test_issue_trusted_device_longer_ttl() {
        let mgr = test_manager();
        let normal = mgr.issue_session(false).unwrap();
        let trusted = mgr.issue_session(true).unwrap();
        // Both valid now
        assert!(mgr.is_session_valid(&normal));
        assert!(mgr.is_session_valid(&trusted));
    }

    #[test]
    fn test_session_invalid_after_secret_rotation() {
        let mgr = test_manager();
        let token = mgr.issue_session(false).unwrap();
        assert!(mgr.is_session_valid(&token));
        mgr.update_secret(b"different-secret-now!!!!!!!!!!".to_vec());
        assert!(!mgr.is_session_valid(&token));
    }

    #[test]
    fn test_build_session_cookie() {
        let mgr = test_manager();
        let cookie = mgr.build_session_cookie("token123", 43200000, false);
        assert!(cookie.starts_with("oc_ui_session=token123"));
        assert!(cookie.contains("Max-Age=43200"));
        assert!(!cookie.contains("Secure"));
    }

    #[test]
    fn test_build_session_cookie_secure() {
        let mgr = test_manager();
        let cookie = mgr.build_session_cookie("token123", 43200000, true);
        assert!(cookie.ends_with("; Secure"));
    }

    #[test]
    fn test_build_clear_cookie() {
        let mgr = test_manager();
        let cookie = mgr.build_clear_cookie(false);
        assert!(cookie.starts_with("oc_ui_session=;"));
        assert!(cookie.contains("Max-Age=0"));
    }

    #[test]
    fn test_url_encode() {
        assert_eq!(url_encode("abc123-_.!~*'()"), "abc123-_.!~*'()");
        assert_eq!(url_encode("a b+c"), "a%20b%2Bc");
        assert_eq!(url_encode("a.b.c"), "a.b.c");
    }

    #[test]
    fn test_resolve_session_ttl() {
        assert_eq!(resolve_session_ttl_ms(false), SESSION_TTL_MS);
        assert_eq!(resolve_session_ttl_ms(true), TRUSTED_DEVICE_SESSION_TTL_MS);
    }
}
