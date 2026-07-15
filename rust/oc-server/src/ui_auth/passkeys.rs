//! WebAuthn passkey store — passkey 注册/认证。
//!
//! 移植自 `packages/web/server/lib/ui-auth/ui-passkeys.js` (545 行)。
//! 使用 `webauthn-rs` crate 替代 `@simplewebauthn/server`。
//!
//! 文件持久化: `$DATA_DIR/ui-passkeys.json`。
//! In-memory challenge store: registration + authentication challenges。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use base64::Engine;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use url::Url;
use uuid::Uuid;
use webauthn_rs::prelude::*;

use crate::github::settings::data_dir;

// ─── 持久化存储 ──────────────────────────────────────

/// Passkey 存储文件版本。
const STORE_VERSION: i64 = 1;

/// 存储的 passkey 记录 (磁盘格式)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredPasskey {
    pub id: String,
    /// base64url 编码的 COSE 公钥 (webauthn-rs Passkey 序列化)。
    pub passkey_json: String,
    pub counter: u32,
    pub transports: Vec<String>,
    pub device_type: String,
    pub backed_up: bool,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
    pub label: String,
    pub rp_id: String,
}

/// Passkey 存储结构 (磁盘 JSON)。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PasskeyStore {
    version: i64,
    user_id: String,
    password_binding: String,
    passkeys: Vec<StoredPasskey>,
}

// ─── Challenge 记录 (内存) ──────────────────────────

/// 注册 challenge 记录。
#[allow(dead_code)]
#[derive(Clone)]
struct RegistrationChallenge {
    challenge: String,
    expected_origins: Vec<String>,
    expected_rp_ids: Vec<String>,
    rp_id: String,
    label: String,
    created_at: i64,
    expires_at: i64,
    /// webauthn-rs 的注册状态 (PasskeyRegistration)。
    registration_state: PasskeyRegistration,
}

/// 认证 challenge 记录。
#[allow(dead_code)]
#[derive(Clone)]
struct AuthenticationChallenge {
    challenge: String,
    expected_origins: Vec<String>,
    expected_rp_ids: Vec<String>,
    created_at: i64,
    expires_at: i64,
    /// webauthn-rs 的认证状态 (PasskeyAuthentication)。
    authentication_state: PasskeyAuthentication,
}

/// 内部状态。
struct PasskeysInner {
    registration_challenges: HashMap<String, RegistrationChallenge>,
    authentication_challenges: HashMap<String, AuthenticationChallenge>,
}

/// Passkey 控制器。
pub struct UiPasskeys {
    inner: Mutex<PasskeysInner>,
    password_binding: String,
    store_file: PathBuf,
    #[allow(dead_code)]
    rp_name: String,
}

impl UiPasskeys {
    pub fn new(password_binding: String, rp_name: String, _challenge_ttl_ms: i64) -> Self {
        let store_file = data_dir().join("ui-passkeys.json");
        UiPasskeys {
            inner: Mutex::new(PasskeysInner {
                registration_challenges: HashMap::new(),
                authentication_challenges: HashMap::new(),
            }),
            password_binding,
            store_file,
            rp_name,
        }
    }

    /// 加载 passkey store (移植自 `loadStore`, ui-passkeys.js:141-183)。
    fn load_store(&self) -> PasskeyStore {
        let empty = || PasskeyStore {
            version: STORE_VERSION,
            user_id: create_user_id(),
            password_binding: self.password_binding.clone(),
            passkeys: vec![],
        };

        let mut store = match std::fs::read_to_string(&self.store_file) {
            Ok(content) => {
                match serde_json::from_str::<PasskeyStore>(&content) {
                    Ok(s) => s,
                    Err(_) => empty(),
                }
            }
            Err(_) => empty(),
        };

        // passwordBinding 不匹配 → 清空所有 passkeys
        if self.password_binding.is_empty() {
            if !store.passkeys.is_empty() || !store.password_binding.is_empty() {
                store.passkeys.clear();
                store.password_binding.clear();
                let _ = self.persist_store(&store);
            }
            return store;
        }

        if store.password_binding != self.password_binding {
            store = PasskeyStore {
                version: STORE_VERSION,
                user_id: if store.user_id.is_empty() { create_user_id() } else { store.user_id.clone() },
                password_binding: self.password_binding.clone(),
                passkeys: vec![],
            };
            let _ = self.persist_store(&store);
            return store;
        }

        if !self.store_file.exists() {
            let _ = self.persist_store(&store);
        }

        store
    }

    /// 持久化 passkey store (移植自 `persistStore`, ui-passkeys.js:129-132)。
    fn persist_store(&self, store: &PasskeyStore) -> Result<(), std::io::Error> {
        if let Some(parent) = self.store_file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = serde_json::to_string_pretty(store).unwrap_or_else(|_| "{}".to_string());
        // 原子写: .tmp → rename
        let tmp = self.store_file.with_extension(format!(
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
        std::fs::rename(&tmp, &self.store_file)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.store_file, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }

    /// 获取 passkey 状态 (移植自 `getStatus`, ui-passkeys.js:222-231)。
    pub fn get_status(&self, rp_id: &str) -> Value {
        let store = self.load_store();
        let pk_for_rp = get_passkeys_for_rp(&store.passkeys, rp_id);
        json!({
            "enabled": !self.password_binding.is_empty(),
            "hasPasskeys": !rp_id.is_empty() && !pk_for_rp.is_empty(),
            "passkeyCount": if rp_id.is_empty() { 0 } else { pk_for_rp.len() },
            "rpID": rp_id,
        })
    }

    /// 列出 passkeys (移植自 `listPasskeys`, ui-passkeys.js:233-250)。
    pub fn list_passkeys(&self, rp_id: &str) -> Vec<Value> {
        if self.password_binding.is_empty() {
            return vec![];
        }
        let store = self.load_store();
        if rp_id.is_empty() {
            return vec![];
        }
        get_passkeys_for_rp(&store.passkeys, rp_id)
            .iter()
            .map(|pk| {
                json!({
                    "id": pk.id,
                    "label": pk.label,
                    "createdAt": pk.created_at,
                    "lastUsedAt": pk.last_used_at,
                    "deviceType": pk.device_type,
                    "backedUp": pk.backed_up,
                })
            })
            .collect()
    }

    /// 撤销 passkey (移植自 `revokePasskey`, ui-passkeys.js:252-283)。
    pub fn revoke_passkey(&self, rp_id: &str, passkey_id: &str) -> Result<Value, PasskeyError> {
        if self.password_binding.is_empty() {
            return Err(PasskeyError::NotEnabled);
        }
        let id = passkey_id.trim();
        if id.is_empty() {
            return Err(PasskeyError::InvalidInput("Passkey ID is required"));
        }
        let mut store = self.load_store();
        let existing = store
            .passkeys
            .iter()
            .find(|pk| pk.id == id && pk.rp_id == rp_id);
        if existing.is_none() {
            return Err(PasskeyError::NotFound);
        }
        store.passkeys.retain(|pk| !(pk.id == id && pk.rp_id == rp_id));
        self.persist_store(&store)?;
        let count = store.passkeys.iter().filter(|pk| pk.rp_id == rp_id).count();
        Ok(json!({ "revoked": true, "passkeyCount": count }))
    }

    /// 清除所有 passkeys (移植自 `clearAllPasskeys`, ui-passkeys.js:285-301)。
    pub fn clear_all_passkeys(&self) -> usize {
        if self.password_binding.is_empty() {
            return 0;
        }
        let mut store = self.load_store();
        let cleared = store.passkeys.len();
        store.user_id = create_user_id();
        store.passkeys.clear();
        let _ = self.persist_store(&store);
        cleared
    }

    // ─── WebAuthn 注册/认证 ──────────────────────────

    /// 开始 passkey 注册 (移植自 `beginRegistration`, ui-passkeys.js:303-361)。
    pub fn begin_registration(
        &self,
        origin: &str,
        rp_id: &str,
        label: &str,
    ) -> Result<Value, PasskeyError> {
        if self.password_binding.is_empty() {
            return Err(PasskeyError::NotEnabled);
        }
        if rp_id.is_empty() {
            return Err(PasskeyError::InvalidInput(
                "Unable to resolve a valid passkey host for this request",
            ));
        }
        if origin.is_empty() {
            return Err(PasskeyError::InvalidInput(
                "Unable to resolve a valid passkey origin for this request",
            ));
        }

        self.cleanup_challenges();

        let store = self.load_store();
        let user_id = decode_user_id(&store.user_id)
            .ok_or(PasskeyError::StorageInvalid)?;

        let webauthn = build_webauthn(rp_id, origin)?;
        let exclude: Option<Vec<CredentialID>> = if get_passkeys_for_rp(&store.passkeys, rp_id).is_empty() {
            None
        } else {
            Some(
                get_passkeys_for_rp(&store.passkeys, rp_id)
                    .iter()
                    .map(|pk| CredentialID::from(base64_decode(&pk.id).unwrap_or_default()))
                    .collect(),
            )
        };

        // webauthn-rs 用 Uuid 作为 user_unique_id
        let user_uuid = uuid_from_bytes(&user_id);
        let (ccr, reg_state) = webauthn
            .start_passkey_registration(user_uuid, "openchamber-ui", "OpenChamber UI", exclude)
            .map_err(|e| PasskeyError::Webauthn(format!("{e:?}")))?;

        let request_id = generate_request_id();
        let now = now_millis();
        let challenge_ttl = super::DEFAULT_CHALLENGE_TTL_MS;

        let challenge_str = serde_json::to_string(&ccr.public_key.challenge)
            .unwrap_or_default()
            .trim_matches('"')
            .to_string();

        let record = RegistrationChallenge {
            challenge: challenge_str,
            expected_origins: vec![origin.to_string()],
            expected_rp_ids: vec![rp_id.to_string()],
            rp_id: rp_id.to_string(),
            label: normalize_label(label, "This device"),
            created_at: now,
            expires_at: now + challenge_ttl,
            registration_state: reg_state,
        };

        let mut inner = self.inner.lock().unwrap();
        inner.registration_challenges.insert(request_id.clone(), record);

        // 返回给前端: requestId + optionsJSON (CreationChallengeResponse)
        Ok(json!({
            "requestId": request_id,
            "optionsJSON": ccr,
        }))
    }

    /// 完成 passkey 注册 (移植自 `finishRegistration`, ui-passkeys.js:363-424)。
    pub fn finish_registration(
        &self,
        request_id: &str,
        response: &Value,
    ) -> Result<Value, PasskeyError> {
        if self.password_binding.is_empty() {
            return Err(PasskeyError::NotEnabled);
        }
        self.cleanup_challenges();

        let mut store = self.load_store();

        let mut inner = self.inner.lock().unwrap();
        let record = inner
            .registration_challenges
            .remove(request_id)
            .ok_or(PasskeyError::ChallengeExpired)?;

        if request_id.is_empty() {
            return Err(PasskeyError::ChallengeExpired);
        }

        // 反序列化 client response → RegisterPublicKeyCredential
        let reg_cred: RegisterPublicKeyCredential = serde_json::from_value(response.clone())
            .map_err(|e| PasskeyError::DynamicInput(format!("Invalid registration response: {e}")))?;

        // 重建 Webauthn 实例
        let origin = record
            .expected_origins
            .first()
            .ok_or(PasskeyError::InvalidInput("Missing origin"))?;
        let webauthn = build_webauthn(&record.rp_id, origin)?;

        let passkey = webauthn
            .finish_passkey_registration(&reg_cred, &record.registration_state)
            .map_err(|e| PasskeyError::VerificationFailed(format!("{e:?}")))?;

        let cred_id = base64_encode(passkey.cred_id().as_ref());
        let passkey_json = serde_json::to_string(&passkey)
            .map_err(|e| PasskeyError::StorageInvalidWithMsg(format!("{e}")))?;

        // 移除同 ID 的旧 passkey
        store.passkeys.retain(|pk| pk.id != cred_id);
        store.passkeys.push(StoredPasskey {
            id: cred_id,
            passkey_json,
            counter: 0,
            transports: vec![],
            device_type: "multiDevice".to_string(),
            backed_up: true,
            created_at: now_millis(),
            last_used_at: None,
            label: record.label.clone(),
            rp_id: record.rp_id.clone(),
        });

        let rp_id = record.rp_id.clone();
        self.persist_store(&store)?;
        drop(inner);

        let count = store.passkeys.iter().filter(|pk| pk.rp_id == rp_id).count();
        Ok(json!({ "verified": true, "passkeyCount": count }))
    }

    /// 开始 passkey 认证 (移植自 `beginAuthentication`, ui-passkeys.js:426-462)。
    pub fn begin_authentication(&self, origin: &str, rp_id: &str) -> Result<Value, PasskeyError> {
        if self.password_binding.is_empty() {
            return Err(PasskeyError::NotEnabled);
        }
        self.cleanup_challenges();

        let store = self.load_store();
        let passkeys: Vec<Passkey> = get_passkeys_for_rp(&store.passkeys, rp_id)
            .iter()
            .filter_map(|pk| deserialize_passkey(&pk.passkey_json))
            .collect();

        if rp_id.is_empty() || passkeys.is_empty() {
            return Err(PasskeyError::NotFound);
        }

        let webauthn = build_webauthn(rp_id, origin)?;
        let (rcr, auth_state) = webauthn
            .start_passkey_authentication(&passkeys)
            .map_err(|e| PasskeyError::Webauthn(format!("{e:?}")))?;

        let request_id = generate_request_id();
        let now = now_millis();
        let challenge_ttl = super::DEFAULT_CHALLENGE_TTL_MS;

        let record = AuthenticationChallenge {
            challenge: serde_json::to_string(&rcr.public_key.challenge)
                .unwrap_or_default()
                .trim_matches('"')
                .to_string(),
            expected_origins: vec![origin.to_string()],
            expected_rp_ids: vec![rp_id.to_string()],
            created_at: now,
            expires_at: now + challenge_ttl,
            authentication_state: auth_state,
        };

        let mut inner = self.inner.lock().unwrap();
        inner
            .authentication_challenges
            .insert(request_id.clone(), record);

        Ok(json!({
            "requestId": request_id,
            "optionsJSON": rcr,
        }))
    }

    /// 完成 passkey 认证 (移植自 `finishAuthentication`, ui-passkeys.js:464-525)。
    pub fn finish_authentication(
        &self,
        request_id: &str,
        response: &Value,
    ) -> Result<(), PasskeyError> {
        if self.password_binding.is_empty() {
            return Err(PasskeyError::NotEnabled);
        }
        self.cleanup_challenges();

        let mut inner = self.inner.lock().unwrap();
        let record = inner
            .authentication_challenges
            .remove(request_id)
            .ok_or(PasskeyError::ChallengeExpired)?;

        // 从 response 提取 credential id
        let cred_id_str = response
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        // 重建 Webauthn
        let origin = record
            .expected_origins
            .first()
            .ok_or(PasskeyError::InvalidInput("Missing origin"))?;
        let webauthn = build_webauthn(&record.expected_rp_ids[0], origin)?;

        // 反序列化 client response → PublicKeyCredential
        let pub_cred: PublicKeyCredential = serde_json::from_value(response.clone())
            .map_err(|e| PasskeyError::DynamicInput(format!("Invalid authentication response: {e}")))?;

        let auth_result = webauthn
            .finish_passkey_authentication(&pub_cred, &record.authentication_state)
            .map_err(|e| PasskeyError::VerificationFailed(format!("{e:?}")))?;

        // 更新 counter + lastUsedAt
        drop(inner);
        let mut store = self.load_store();
        let now = now_millis();
        for pk in &mut store.passkeys {
            if pk.id == cred_id_str {
                pk.counter = auth_result.counter();
                pk.last_used_at = Some(now);
                break;
            }
        }
        let _ = self.persist_store(&store);
        Ok(())
    }

    /// 清理过期 challenge (移植自 `cleanupChallengeMap`, ui-passkeys.js:185-192)。
    fn cleanup_challenges(&self) {
        let now = now_millis();
        let mut inner = self.inner.lock().unwrap();
        inner.registration_challenges.retain(|_, r| now < r.expires_at);
        inner
            .authentication_challenges
            .retain(|_, r| now < r.expires_at);
    }
}

// ─── 辅助函数 ────────────────────────────────────────

fn create_user_id() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn decode_user_id(value: &str) -> Option<Vec<u8>> {
    if value.is_empty() {
        return None;
    }
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(value)
        .ok()
}

fn uuid_from_bytes(bytes: &[u8]) -> Uuid {
    if bytes.len() >= 16 {
        Uuid::from_slice(&bytes[..16]).unwrap_or_else(|_| Uuid::new_v4())
    } else {
        Uuid::new_v4()
    }
}

fn get_passkeys_for_rp<'a>(passkeys: &'a [StoredPasskey], rp_id: &str) -> Vec<&'a StoredPasskey> {
    passkeys.iter().filter(|pk| pk.rp_id == rp_id).collect()
}

fn normalize_label(value: &str, fallback: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return fallback.to_string();
    }
    // collapse whitespace
    let normalized: String = trimmed.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        fallback.to_string()
    } else {
        normalized.chars().take(120).collect()
    }
}

/// 构建 Webauthn 实例 (per rp_id + origin)。
fn build_webauthn(rp_id: &str, origin: &str) -> Result<Webauthn, PasskeyError> {
    let rp_origin = Url::parse(origin)
        .map_err(|e| PasskeyError::DynamicInput(format!("Invalid origin '{origin}': {e}")))?;
    let builder = WebauthnBuilder::new(rp_id, &rp_origin)
        .map_err(|e| PasskeyError::DynamicInput(format!("Invalid WebAuthn config: {e:?}")))?;
    builder
        .build()
        .map_err(|e| PasskeyError::Webauthn(format!("WebAuthn build failed: {e:?}")))
}

fn deserialize_passkey(json_str: &str) -> Option<Passkey> {
    serde_json::from_str(json_str).ok()
}

fn generate_request_id() -> String {
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn now_millis() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn base64_encode(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(s).ok()
}

// ─── 错误类型 ────────────────────────────────────────

/// Passkey 操作错误。
#[derive(Debug)]
pub enum PasskeyError {
    NotEnabled,
    InvalidInput(&'static str),
    DynamicInput(String),
    NotFound,
    ChallengeExpired,
    StorageInvalid,
    StorageInvalidWithMsg(String),
    VerificationFailed(String),
    Webauthn(String),
    Io(std::io::Error),
}

impl PasskeyError {
    pub fn message(&self) -> String {
        match self {
            PasskeyError::NotEnabled => "Passkeys require UI password protection to be enabled".to_string(),
            PasskeyError::InvalidInput(msg) => (*msg).to_string(),
            PasskeyError::DynamicInput(msg) => msg.clone(),
            PasskeyError::NotFound => "No passkeys are registered for this host yet".to_string(),
            PasskeyError::ChallengeExpired => "Passkey setup has expired. Please try again.".to_string(),
            PasskeyError::StorageInvalid => "Passkey storage is invalid. Please try again.".to_string(),
            PasskeyError::StorageInvalidWithMsg(msg) => format!("Passkey storage error: {msg}"),
            PasskeyError::VerificationFailed(msg) => msg.clone(),
            PasskeyError::Webauthn(msg) => format!("WebAuthn error: {msg}"),
            PasskeyError::Io(e) => format!("I/O error: {e}"),
        }
    }

    pub fn status_code(&self) -> u16 {
        match self {
            PasskeyError::NotFound => 404,
            PasskeyError::Io(_) | PasskeyError::StorageInvalid | PasskeyError::StorageInvalidWithMsg(_) => 500,
            _ => 400,
        }
    }
}

impl From<std::io::Error> for PasskeyError {
    fn from(e: std::io::Error) -> Self {
        PasskeyError::Io(e)
    }
}

impl std::fmt::Display for PasskeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message())
    }
}

impl std::error::Error for PasskeyError {}

// 需要 url crate — 它是 webauthn-rs 的传递依赖, 直接使用
