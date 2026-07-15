//! UI 认证模块 — 浏览器访问认证 (password session / JWT / rate-limit / URL-token scoping / passkeys)。
//!
//! 移植自 `packages/web/server/lib/ui-auth/ui-auth.js` + `ui-passkeys.js`。
//! 与 Node 后端 HTTP API 契约字节对齐。

pub mod types;
pub mod jwt_secret;
pub mod password;
pub mod rate_limit;
pub mod url_token;
pub mod session;
pub mod passkeys;
pub mod routes;

use std::sync::Arc;
use tokio::sync::Mutex;

use crate::client_auth::remote_clients::RemoteClientAuthRuntime;

/// Session cookie 名称 (移植自 ui-auth.js:8)。
pub const SESSION_COOKIE_NAME: &str = "oc_ui_session";
/// 普通 session TTL: 12 小时。
pub const SESSION_TTL_MS: i64 = 12 * 60 * 60 * 1000;
/// 受信任设备 session TTL: 7 天。
pub const TRUSTED_DEVICE_SESSION_TTL_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// URL auth token TTL: 60 秒。
pub const URL_AUTH_TOKEN_TTL_MS: i64 = 60 * 1000;
/// URL auth token 前缀。
pub const URL_AUTH_TOKEN_PREFIX: &str = "oc_url_";

/// 登录限速窗口: 5 分钟。
pub const RATE_LIMIT_WINDOW_MS: i64 = 5 * 60 * 1000;
/// 登录限速最大尝试次数: 10 次 (有 IP)。
pub const RATE_LIMIT_MAX_ATTEMPTS: u32 = 10;
/// 登录锁定时长: 15 分钟。
pub const RATE_LIMIT_LOCKOUT_MS: i64 = 15 * 60 * 1000;
/// 限速记录清理周期: 1 小时。
#[allow(dead_code)]
pub const RATE_LIMIT_CLEANUP_MS: i64 = 60 * 60 * 1000;
/// 无 IP 时最大尝试次数: 3 次。
pub const RATE_LIMIT_NO_IP_MAX_ATTEMPTS: u32 = 3;

/// WebAuthn challenge TTL: 5 分钟。
pub const DEFAULT_CHALLENGE_TTL_MS: i64 = 5 * 60 * 1000;
/// WebAuthn RP 名称。
pub const DEFAULT_RP_NAME: &str = "OpenChamber";

/// UI 认证控制器。
///
/// 当 `enabled == false` (未配置密码) 时,`password_hasher` / `session_manager_secret` / `passkeys`
/// 均为 `None`,所有认证端点返回 disabled 响应。
pub struct UiAuth {
    /// 是否启用了密码保护。
    pub enabled: bool,
    /// 密码哈希器 (enabled 时存在)。
    pub password_hasher: Option<password::PasswordHasher>,
    /// Session JWT 管理器 (enabled 时存在)。
    pub session_manager: Option<session::SessionManager>,
    /// 登录限速器 (始终存在, enabled 时使用)。
    pub rate_limiter: rate_limit::LoginRateLimiter,
    /// URL auth token 存储。
    pub url_token_store: url_token::UrlTokenStore,
    /// Passkey 控制器 (enabled 时存在)。
    pub passkeys: Option<passkeys::UiPasskeys>,
    /// Remote client 认证 (可选, 注入)。
    pub client_auth: Option<Arc<RemoteClientAuthRuntime>>,
    /// 是否要求 client 认证 (无密码模式下的增强门)。
    pub require_client_auth: bool,
    /// JWT secret (用于 HMAC password binding 重建)。
    pub jwt_secret: Mutex<Option<Vec<u8>>>,
}

impl UiAuth {
    /// 创建 enabled 控制器 (配置了密码)。
    pub fn new_enabled(
        normalized_password: &str,
        jwt_secret_bytes: Vec<u8>,
        client_auth: Option<Arc<RemoteClientAuthRuntime>>,
    ) -> Self {
        let password_hasher = password::PasswordHasher::new(normalized_password);
        let session_manager = session::SessionManager::new(
            SESSION_COOKIE_NAME.to_string(),
            jwt_secret_bytes.clone(),
        );
        // password binding = HMAC-SHA256(jwt_secret, password)
        let password_binding = compute_password_binding(&jwt_secret_bytes, normalized_password);
        let passkeys = passkeys::UiPasskeys::new(
            password_binding,
            DEFAULT_RP_NAME.to_string(),
            DEFAULT_CHALLENGE_TTL_MS,
        );

        UiAuth {
            enabled: true,
            password_hasher: Some(password_hasher),
            session_manager: Some(session_manager),
            rate_limiter: rate_limit::LoginRateLimiter::new(),
            url_token_store: url_token::UrlTokenStore::new(),
            passkeys: Some(passkeys),
            client_auth,
            require_client_auth: false,
            jwt_secret: Mutex::new(Some(jwt_secret_bytes)),
        }
    }

    /// 创建 disabled 控制器 (未配置密码)。
    pub fn new_disabled(
        client_auth: Option<Arc<RemoteClientAuthRuntime>>,
        require_client_auth: bool,
    ) -> Self {
        UiAuth {
            enabled: false,
            password_hasher: None,
            session_manager: None,
            rate_limiter: rate_limit::LoginRateLimiter::new(),
            url_token_store: url_token::UrlTokenStore::new(),
            passkeys: None,
            client_auth,
            require_client_auth,
            jwt_secret: Mutex::new(None),
        }
    }

    /// 全局登出: 旋转 JWT secret + 清除 passkeys。
    pub async fn reset_auth(&self) -> Result<ResetAuthResult, ResetAuthError> {
        // 检查 OPENCODE_JWT_SECRET
        if std::env::var("OPENCODE_JWT_SECRET").is_ok() {
            return Err(ResetAuthError::EnvSecretFixed);
        }

        // 生成新 secret
        let new_secret_hex = generate_random_hex(32);
        let new_secret_bytes = jwt_secret::persist_jwt_secret(&new_secret_hex)?;

        // 清除 URL auth tokens
        self.url_token_store.clear();

        // 清除所有 passkeys
        let cleared_passkeys = if let Some(ref pk) = self.passkeys {
            pk.clear_all_passkeys()
        } else {
            0
        };

        // 重建 password binding + passkey controller
        if let Some(ref sm) = self.session_manager {
            sm.update_secret(new_secret_bytes.clone());
        }

        // 更新 jwt_secret mutex
        let mut guard = self.jwt_secret.lock().await;
        *guard = Some(new_secret_bytes);

        Ok(ResetAuthResult {
            cleared_passkeys,
            signed_out_everywhere: true,
        })
    }
}

/// reset_auth 返回值。
pub struct ResetAuthResult {
    pub cleared_passkeys: usize,
    #[allow(dead_code)]
    pub signed_out_everywhere: bool,
}

/// reset_auth 错误。
#[derive(Debug)]
pub enum ResetAuthError {
    /// OPENCODE_JWT_SECRET 已设置, 无法旋转。
    EnvSecretFixed,
    /// I/O 错误。
    Io(std::io::Error),
}

impl From<std::io::Error> for ResetAuthError {
    fn from(e: std::io::Error) -> Self {
        ResetAuthError::Io(e)
    }
}

/// 计算 password binding: HMAC-SHA256(jwt_secret_bytes, password) → hex。
fn compute_password_binding(jwt_secret: &[u8], password: &str) -> String {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    type HmacSha256 = Hmac<Sha256>;
    let mut mac = HmacSha256::new_from_slice(jwt_secret).expect("HMAC accepts any key length");
    mac.update(password.as_bytes());
    hex_encode(mac.finalize().into_bytes().as_slice())
}

/// 生成 n 个随机字节的 hex 编码。
fn generate_random_hex(n: usize) -> String {
    use rand::RngCore;
    let mut bytes = vec![0u8; n];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex_encode(&bytes)
}

/// hex 编码 (不引入 hex crate)。
fn hex_encode(bytes: &[u8]) -> String {
    let mut result = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        result.push_str(&format!("{b:02x}"));
    }
    result
}
