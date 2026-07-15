//! Pairing session store + redeem flow。
//!
//! 移植自 `packages/web/server/lib/client-auth/pairing.js` (308 行)。
//! 文件持久化 + Mutex 串行化。
//! Secret: 32 bytes base64url。Hash: SHA-256 hex。
//! 所有 redeem 失败返回同一 generic error, 不泄露失败原因。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use base64::Engine;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::github::settings::data_dir;

use super::remote_clients::{CreateClientParams, RemoteClientAuthRuntime};
use super::{PAIRING_ID_PREFIX, SECRET_BYTES, STORE_VERSION};

const DEFAULT_TTL_MS: i64 = 10 * 60 * 1000; // 10 minutes
const GENERIC_REDEEM_ERROR: &str = "Invalid or expired pairing session";
const VALID_CLIENT_KINDS: &[&str] = &["mobile", "desktop"];
const PAIRING_LABEL_PLACEHOLDER: &str = "Pair new device";

// ─── 数据结构 ────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PairingSession {
    id: String,
    secret_hash: String,
    created_at: String,
    expires_at: String,
    used_at: Option<String>,
    cancelled_at: Option<String>,
    client_id: Option<String>,
    label: Option<String>,
    fingerprint: String,
    allowed_client_kinds: Vec<String>,
    created_by_client_id: Option<String>,
    #[serde(default)]
    uses_relay: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct PairingStore {
    version: i64,
    sessions: Vec<PairingSession>,
}

/// Create pairing session 参数。
#[derive(Debug, Clone, Default)]
pub struct CreateSessionParams {
    pub label: Option<String>,
    pub allowed_client_kinds: Vec<String>,
    pub created_by_client_id: Option<String>,
    pub uses_relay: bool,
}

/// Redeem pairing session 参数。
#[derive(Debug, Clone, Default)]
pub struct RedeemParams {
    pub pairing_id: String,
    pub secret: String,
    pub client_label: Option<String>,
    pub client_kind: Option<String>,
    pub device_name: Option<String>,
    pub device_platform: Option<String>,
    pub device_model: Option<String>,
    pub app_version: Option<String>,
    pub dedupe_key: Option<String>,
}

/// Pairing session runtime。
pub struct ClientPairingRuntime {
    store_path: PathBuf,
    write_lock: Mutex<()>,
    remote_clients: Arc<RemoteClientAuthRuntime>,
    ttl_ms: i64,
}

impl ClientPairingRuntime {
    pub fn new(remote_clients: Arc<RemoteClientAuthRuntime>) -> Self {
        ClientPairingRuntime {
            store_path: data_dir().join("client-pairing-sessions.json"),
            write_lock: Mutex::new(()),
            remote_clients,
            ttl_ms: DEFAULT_TTL_MS,
        }
    }

    fn load_store(&self) -> PairingStore {
        match std::fs::read_to_string(&self.store_path) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_else(|_| PairingStore {
                version: STORE_VERSION,
                sessions: vec![],
            }),
            Err(_) => PairingStore {
                version: STORE_VERSION,
                sessions: vec![],
            },
        }
    }

    fn persist_store(&self, store: &PairingStore) -> Result<(), std::io::Error> {
        if let Some(parent) = self.store_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = serde_json::to_string_pretty(store).unwrap_or_else(|_| "{}".to_string());
        let tmp = self.store_path.with_extension(format!(
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
        std::fs::rename(&tmp, &self.store_path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.store_path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }

    /// 创建 pairing session (移植自 `createPairingSession`, pairing.js)。
    pub fn create_session(
        &self,
        params: CreateSessionParams,
    ) -> Result<(Value, String), std::io::Error> {
        let _guard = self.write_lock.lock().unwrap();
        let mut store = self.load_store();
        // sweep expired
        sweep_expired(&mut store.sessions);

        let secret = generate_secret();
        let secret_hash = hash_secret(&secret);
        let id = generate_id();
        let now = now_iso();
        let expires = now_iso_plus(self.ttl_ms);
        let fingerprint = generate_fingerprint();

        let allowed = normalize_allowed_kinds(&params.allowed_client_kinds);

        let session = PairingSession {
            id: id.clone(),
            secret_hash,
            created_at: now,
            expires_at: expires,
            used_at: None,
            cancelled_at: None,
            client_id: None,
            label: params.label.filter(|s| !s.is_empty()),
            fingerprint,
            allowed_client_kinds: allowed,
            created_by_client_id: params.created_by_client_id.filter(|s| !s.is_empty()),
            uses_relay: params.uses_relay,
        };

        let public = public_session(&session);
        store.sessions.push(session);
        self.persist_store(&store)?;
        Ok((public, secret))
    }

    /// Redeem pairing session (移植自 `redeemPairingSession`, pairing.js)。
    ///
    /// 所有失败返回同一 generic error。
    pub fn redeem_session(
        &self,
        params: RedeemParams,
    ) -> Result<(Value, Value, String), PairingRedeemError> {
        let pairing_id = params.pairing_id.trim().to_string();
        let secret = params.secret.trim().to_string();
        let client_kind = params
            .client_kind
            .as_deref()
            .filter(|s| VALID_CLIENT_KINDS.contains(s))
            .unwrap_or("mobile")
            .to_string();

        if pairing_id.is_empty() || secret.is_empty() {
            return Err(PairingRedeemError);
        }

        let _guard = self.write_lock.lock().unwrap();
        let mut store = self.load_store();

        // 查找 session
        let session_idx = store.sessions.iter().position(|s| s.id == pairing_id);
        let session = match session_idx {
            Some(idx) => &store.sessions[idx],
            None => return Err(PairingRedeemError),
        };

        // 检查条件: cancelled/used/expired/kind/secret — 全部同一 error
        let now = now_iso();
        if session.cancelled_at.is_some()
            || session.used_at.is_some()
            || session.expires_at.as_str() <= now.as_str()
            || !session.allowed_client_kinds.contains(&client_kind)
            || !constant_time_equal_hex(&session.secret_hash, &hash_secret(&secret))
        {
            return Err(PairingRedeemError);
        }

        // Label precedence: session.label || clientLabel || deviceName || "Remote client"
        let label = session
            .label
            .clone()
            .or_else(|| params.client_label.clone())
            .or_else(|| params.device_name.clone())
            .unwrap_or_else(|| "Remote client".to_string());

        let dedupe = params
            .dedupe_key
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("pairing:{}", session.id));

        // 调用 remote_clients 创建 client token
        let create_params = CreateClientParams {
            label: Some(label),
            client_kind: Some(client_kind),
            dedupe_key: Some(dedupe),
            auth_method: Some("pairing".to_string()),
            pairing_id: Some(session.id.clone()),
            device_name: params.device_name,
            device_platform: params.device_platform,
            device_model: params.device_model,
            app_version: params.app_version,
            uses_relay: session.uses_relay,
            expires_at: None,
        };

        // 如果 createClient 失败, 不消费 session
        let (client, token) = self
            .remote_clients
            .create_client(create_params)
            .map_err(|_| PairingRedeemError)?;

        // 成功: 标记 session used
        let session = &mut store.sessions[session_idx.unwrap()];
        session.used_at = Some(now.clone());
        session.client_id = client.get("id").and_then(|v| v.as_str()).map(|s| s.to_string());
        let public = public_session(session);
        let _ = self.persist_store(&store);

        Ok((public, client, token))
    }

    /// 列出 pending sessions (移植自 `listPendingSessions`)。
    pub fn list_pending(&self) -> Vec<Value> {
        let _guard = self.write_lock.lock().unwrap();
        let store = self.load_store();
        let now = now_iso();
        store
            .sessions
            .iter()
            .filter(|s| is_pending(s, &now))
            .map(public_session)
            .collect()
    }

    /// 取消 session (移植自 `cancelPairingSession`)。
    pub fn cancel_session(&self, id: &str) -> Result<(bool, Option<Value>), std::io::Error> {
        let _guard = self.write_lock.lock().unwrap();
        let mut store = self.load_store();
        let now = now_iso();
        let mut found = None;
        for s in &mut store.sessions {
            if s.id == id && s.cancelled_at.is_none() {
                s.cancelled_at = Some(now.clone());
                found = Some(public_session(s));
                break;
            }
        }
        if found.is_some() {
            self.persist_store(&store)?;
        }
        Ok((found.is_some(), found))
    }

    /// 是否有活跃的 relay pairing session。
    #[allow(dead_code)]
    pub fn has_active_relay_session(&self) -> bool {
        let _guard = self.write_lock.lock().unwrap();
        let store = self.load_store();
        let now = now_iso();
        store
            .sessions
            .iter()
            .any(|s| s.uses_relay && is_pending(s, &now))
    }

    /// 清理过期 sessions。
    #[allow(dead_code)]
    pub fn sweep(&self) -> Result<usize, std::io::Error> {
        let _guard = self.write_lock.lock().unwrap();
        let mut store = self.load_store();
        let before = store.sessions.len();
        sweep_expired(&mut store.sessions);
        let purged = before - store.sessions.len();
        if purged > 0 {
            self.persist_store(&store)?;
        }
        Ok(purged)
    }
}

// ─── 辅助函数 ────────────────────────────────────────

fn is_pending(session: &PairingSession, now: &str) -> bool {
    session.used_at.is_none()
        && session.cancelled_at.is_none()
        && session.expires_at.as_str() > now
}

fn sweep_expired(sessions: &mut Vec<PairingSession>) {
    let now = now_iso();
    let now_ms = now_millis();
    let ttl_ms = DEFAULT_TTL_MS;
    sessions.retain(|s| {
        // terminal sessions: drop if terminal timestamp > now - ttl
        if let Some(ref used) = s.used_at {
            return now_ms - parse_iso_to_millis(used) < ttl_ms;
        }
        if let Some(ref cancelled) = s.cancelled_at {
            return now_ms - parse_iso_to_millis(cancelled) < ttl_ms;
        }
        // never used/cancelled: drop if expired
        s.expires_at > now
    });
}

fn public_session(s: &PairingSession) -> Value {
    json!({
        "id": s.id,
        "createdAt": s.created_at,
        "expiresAt": s.expires_at,
        "usedAt": s.used_at,
        "cancelledAt": s.cancelled_at,
        "clientId": s.client_id,
        "label": s.label.clone().unwrap_or_else(|| PAIRING_LABEL_PLACEHOLDER.to_string()),
        "fingerprint": s.fingerprint,
        "allowedClientKinds": s.allowed_client_kinds,
        "createdByClientId": s.created_by_client_id,
        "usesRelay": s.uses_relay,
    })
}

fn normalize_allowed_kinds(kinds: &[String]) -> Vec<String> {
    let filtered: Vec<String> = kinds
        .iter()
        .filter(|k| VALID_CLIENT_KINDS.contains(&k.as_str()))
        .cloned()
        .collect();
    if filtered.is_empty() {
        vec!["mobile".to_string(), "desktop".to_string()]
    } else {
        filtered
    }
}

fn generate_id() -> String {
    let mut bytes = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut bytes);
    format!("{PAIRING_ID_PREFIX}{}", hex_encode(&bytes))
}

fn generate_secret() -> String {
    let mut bytes = vec![0u8; SECRET_BYTES];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&bytes)
}

fn generate_fingerprint() -> String {
    let mut bytes = [0u8; 4];
    rand::thread_rng().fill_bytes(&mut bytes);
    let hex = hex_encode(&bytes).to_uppercase();
    // format as XXXX-XXXX
    format!("{}-{}", &hex[..4], &hex[4..])
}

fn hash_secret(secret: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(secret.as_bytes());
    hex_encode(hasher.finalize().as_slice())
}

fn constant_time_equal_hex(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut result: u8 = 0;
    for (ca, cb) in a.bytes().zip(b.bytes()) {
        result |= ca ^ cb;
    }
    result == 0
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn now_iso_plus(ms: i64) -> String {
    let dt = chrono::Utc::now() + chrono::Duration::milliseconds(ms);
    dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn now_millis() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn parse_iso_to_millis(iso: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(iso)
        .ok()
        .map(|dt| dt.timestamp_millis())
        .unwrap_or(0)
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut result = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        result.push_str(&format!("{b:02x}"));
    }
    result
}

/// Pairing redeem error — 所有失败都用同一个 message。
#[derive(Debug)]
pub struct PairingRedeemError;

impl std::fmt::Display for PairingRedeemError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{GENERIC_REDEEM_ERROR}")
    }
}

impl std::error::Error for PairingRedeemError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_id_format() {
        let id = generate_id();
        assert!(id.starts_with(PAIRING_ID_PREFIX));
        assert_eq!(id.len(), PAIRING_ID_PREFIX.len() + 24);
    }

    #[test]
    fn test_generate_fingerprint_format() {
        let fp = generate_fingerprint();
        assert_eq!(fp.len(), 9); // XXXX-XXXX
        assert_eq!(fp.as_bytes()[4], b'-');
    }

    #[test]
    fn test_hash_secret_consistency() {
        assert_eq!(hash_secret("abc"), hash_secret("abc"));
        assert_ne!(hash_secret("abc"), hash_secret("abd"));
    }

    #[test]
    fn test_normalize_allowed_kinds_default() {
        let kinds = normalize_allowed_kinds(&[]);
        assert_eq!(kinds, vec!["mobile", "desktop"]);
    }

    #[test]
    fn test_normalize_allowed_kinds_filtered() {
        let kinds = normalize_allowed_kinds(&["mobile".to_string(), "invalid".to_string()]);
        assert_eq!(kinds, vec!["mobile"]);
    }

    #[test]
    fn test_constant_time_equal_hex() {
        assert!(constant_time_equal_hex("abc123", "abc123"));
        assert!(!constant_time_equal_hex("abc123", "abc124"));
        assert!(!constant_time_equal_hex("ab", "abc"));
    }
}
