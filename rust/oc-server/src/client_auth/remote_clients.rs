//! Remote client token store — trusted-device bearer token 存储。
//!
//! 移植自 `packages/web/server/lib/client-auth/remote-clients.js` (269 行)。
//! 文件持久化 + Mutex 串行化所有 read-modify-write。
//! Token: `oc_client_` + 32 bytes base64url。Hash: SHA-256 hex。

use std::path::PathBuf;
use std::sync::Mutex;

use base64::Engine;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::github::settings::data_dir;

use super::{MAX_LABEL_LENGTH, STORE_VERSION, TOKEN_BYTES, TOKEN_PREFIX};

// ─── 数据结构 ────────────────────────────────────────

/// Client 记录 (磁盘 JSON + 内部)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientRecord {
    pub id: String,
    pub label: String,
    pub token_hash: String,
    pub created_at: String,
    pub last_used_at: Option<String>,
    pub revoked_at: Option<String>,
    pub expires_at: Option<String>,
    pub client_kind: Option<String>,
    pub dedupe_key: Option<String>,
    #[serde(default)]
    pub uses_relay: bool,
    pub last_transport: Option<String>,
    pub auth_method: Option<String>,
    pub pairing_id: Option<String>,
    pub device_name: Option<String>,
    pub device_platform: Option<String>,
    pub device_model: Option<String>,
    pub app_version: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ClientStore {
    version: i64,
    clients: Vec<ClientRecord>,
}

/// Create client 参数。
#[derive(Debug, Clone, Default)]
pub struct CreateClientParams {
    pub label: Option<String>,
    pub expires_at: Option<String>,
    pub client_kind: Option<String>,
    pub dedupe_key: Option<String>,
    pub auth_method: Option<String>,
    pub pairing_id: Option<String>,
    pub device_name: Option<String>,
    pub device_platform: Option<String>,
    pub device_model: Option<String>,
    pub app_version: Option<String>,
    pub uses_relay: bool,
}

/// Authenticate 结果。
pub struct AuthResult {
    pub client_id: String,
    pub client: Value,
}

/// Remote client auth runtime。
pub struct RemoteClientAuthRuntime {
    store_path: PathBuf,
    write_lock: Mutex<()>,
}

impl RemoteClientAuthRuntime {
    pub fn new() -> Self {
        RemoteClientAuthRuntime {
            store_path: data_dir().join("remote-clients.json"),
            write_lock: Mutex::new(()),
        }
    }

    /// 加载 store, 不存在则返回空。
    fn load_store(&self) -> ClientStore {
        match std::fs::read_to_string(&self.store_path) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_else(|_| ClientStore {
                version: STORE_VERSION,
                clients: vec![],
            }),
            Err(_) => ClientStore {
                version: STORE_VERSION,
                clients: vec![],
            },
        }
    }

    /// 持久化 store (原子写, mode 0o600)。
    fn persist_store(&self, store: &ClientStore) -> Result<(), std::io::Error> {
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

    /// 创建 client (移植自 `createClient`, remote-clients.js)。
    pub fn create_client(
        &self,
        params: CreateClientParams,
    ) -> Result<(Value, String), std::io::Error> {
        let _guard = self.write_lock.lock().unwrap();
        let mut store = self.load_store();

        let token = generate_token();
        let token_hash = hash_token(&token);
        let id = generate_id();

        let label = params
            .label
            .as_ref()
            .map(|l| normalize_label(l))
            .unwrap_or_else(|| "Remote client".to_string());

        let new_record = ClientRecord {
            id: id.clone(),
            label,
            token_hash,
            created_at: now_iso(),
            last_used_at: None,
            revoked_at: None,
            expires_at: params.expires_at.filter(|s| !s.is_empty()),
            client_kind: params.client_kind.filter(|s| !s.is_empty()),
            dedupe_key: params.dedupe_key.filter(|s| !s.is_empty()),
            uses_relay: params.uses_relay,
            last_transport: None,
            auth_method: params.auth_method.filter(|s| !s.is_empty()),
            pairing_id: params.pairing_id.filter(|s| !s.is_empty()),
            device_name: params.device_name.filter(|s| !s.is_empty()),
            device_platform: params.device_platform.filter(|s| !s.is_empty()),
            device_model: params.device_model.filter(|s| !s.is_empty()),
            app_version: params.app_version.filter(|s| !s.is_empty()),
        };

        // Dedupe: 如果有 dedupeKey, 移除同 key 的旧记录
        if let Some(ref dk) = new_record.dedupe_key {
            store.clients.retain(|c| c.dedupe_key.as_deref() != Some(dk.as_str()));
        }
        // 如果有 clientKind, 也移除同 label 但无 clientKind/dedupeKey 的旧记录
        if new_record.client_kind.is_some() {
            let label_clone = new_record.label.clone();
            store.clients.retain(|c| {
                !(c.label == label_clone && c.client_kind.is_none() && c.dedupe_key.is_none())
            });
        }

        let public_client = public_client(&new_record);
        store.clients.push(new_record);
        self.persist_store(&store)?;
        Ok((public_client, token))
    }

    /// 验证 bearer token (移植自 `authenticateBearerToken`, remote-clients.js)。
    pub fn authenticate_bearer_token(
        &self,
        token: &str,
        is_relay: bool,
    ) -> Option<AuthResult> {
        // 快速前缀检查
        if !token.starts_with(TOKEN_PREFIX) {
            return None;
        }

        let _guard = self.write_lock.lock().unwrap();
        let mut store = self.load_store();
        let token_hash = hash_token(token);
        let now = now_iso();
        let now_ms = now_millis();

        // 查找匹配的非撤销 client 的索引
        let idx = store.clients.iter().position(|c| {
            c.revoked_at.is_none() && constant_time_equal_hex(&c.token_hash, &token_hash)
        })?;

        // 过期检查 (用不可变引用)
        {
            let client = &store.clients[idx];
            if let Some(ref exp) = client.expires_at {
                if exp.as_str() <= now.as_str() {
                    return None;
                }
            }
        }

        // 节流写 lastUsedAt (60s 间隔, 或 transport 变化)
        let should_update = {
            let client = &store.clients[idx];
            client.last_used_at.is_none()
                || (now_ms - parse_iso_to_millis(client.last_used_at.as_deref().unwrap_or(&now)))
                    >= 60_000
                || client.last_transport.as_deref() != Some(if is_relay { "relay" } else { "direct" })
        };

        if should_update {
            let client = &mut store.clients[idx];
            client.last_used_at = Some(now.clone());
            client.last_transport = Some(if is_relay { "relay".to_string() } else { "direct".to_string() });
            let _ = self.persist_store(&store);
        }

        let client = &store.clients[idx];
        Some(AuthResult {
            client_id: client.id.clone(),
            client: public_client(client),
        })
    }

    /// 列出所有 client (公共视图, 移植自 `listClients`)。
    pub fn list_clients(&self) -> Vec<Value> {
        let _guard = self.write_lock.lock().unwrap();
        let store = self.load_store();
        store
            .clients
            .iter()
            .filter(|c| c.revoked_at.is_none())
            .map(public_client)
            .collect()
    }

    /// 获取单个 client by id (公共视图)。
    #[allow(dead_code)]
    pub fn get_client(&self, id: &str) -> Option<Value> {
        let _guard = self.write_lock.lock().unwrap();
        let store = self.load_store();
        store
            .clients
            .iter()
            .find(|c| c.id == id && c.revoked_at.is_none())
            .map(public_client)
    }

    /// 撤销单个 client (移植自 `revokeClient`)。
    pub fn revoke_client(&self, id: &str) -> Result<(bool, Option<Value>), std::io::Error> {
        let _guard = self.write_lock.lock().unwrap();
        let mut store = self.load_store();
        let now = now_iso();
        let mut found = None;
        for c in &mut store.clients {
            if c.id == id && c.revoked_at.is_none() {
                c.revoked_at = Some(now.clone());
                found = Some(public_client(c));
                break;
            }
        }
        if found.is_some() {
            self.persist_store(&store)?;
        }
        Ok((found.is_some(), found))
    }

    /// 撤销所有 client。
    pub fn revoke_all_clients(&self) -> Result<usize, std::io::Error> {
        let _guard = self.write_lock.lock().unwrap();
        let mut store = self.load_store();
        let now = now_iso();
        let mut count = 0;
        for c in &mut store.clients {
            if c.revoked_at.is_none() {
                c.revoked_at = Some(now.clone());
                count += 1;
            }
        }
        if count > 0 {
            self.persist_store(&store)?;
        }
        Ok(count)
    }

    /// 是否有活跃的 relay client (移植自 `hasActiveRelayClients`)。
    #[allow(dead_code)]
    pub fn has_active_relay_clients(&self) -> bool {
        let _guard = self.write_lock.lock().unwrap();
        let store = self.load_store();
        let now = now_iso();
        store.clients.iter().any(|c| {
            c.uses_relay
                && c.revoked_at.is_none()
                && c.expires_at.as_deref().is_none_or(|exp| exp > now.as_str())
        })
    }
}

impl Default for RemoteClientAuthRuntime {
    fn default() -> Self {
        Self::new()
    }
}

// ─── 辅助函数 ────────────────────────────────────────

/// 生成 token: `oc_client_` + 32 bytes base64url。
fn generate_token() -> String {
    let mut bytes = vec![0u8; TOKEN_BYTES];
    rand::thread_rng().fill_bytes(&mut bytes);
    format!(
        "{TOKEN_PREFIX}{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&bytes)
    )
}

/// 生成 id: 24 hex chars (12 bytes)。
fn generate_id() -> String {
    let mut bytes = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex_encode(&bytes)
}

/// SHA-256 hex 哈希。
fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hex_encode(hasher.finalize().as_slice())
}

/// constant-time hex 比较。
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

/// 转换为公共视图 (omit tokenHash, dedupeKey)。
fn public_client(c: &ClientRecord) -> Value {
    json!({
        "id": c.id,
        "label": c.label,
        "createdAt": c.created_at,
        "lastUsedAt": c.last_used_at,
        "revokedAt": c.revoked_at,
        "expiresAt": c.expires_at,
        "clientKind": c.client_kind,
        "authMethod": c.auth_method,
        "pairingId": c.pairing_id,
        "deviceName": c.device_name,
        "devicePlatform": c.device_platform,
        "deviceModel": c.device_model,
        "appVersion": c.app_version,
        "usesRelay": c.uses_relay,
        "lastTransport": c.last_transport,
    })
}

fn normalize_label(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        "Remote client".to_string()
    } else {
        trimmed.chars().take(MAX_LABEL_LENGTH).collect()
    }
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_token_format() {
        let token = generate_token();
        assert!(token.starts_with("oc_client_"));
        assert!(token.len() > TOKEN_PREFIX.len() + 40);
    }

    #[test]
    fn test_generate_id_format() {
        let id = generate_id();
        assert_eq!(id.len(), 24);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_hash_token_consistency() {
        let token = "oc_client_test123";
        assert_eq!(hash_token(token), hash_token(token));
        assert_ne!(hash_token("a"), hash_token("b"));
    }

    #[test]
    fn test_constant_time_equal_hex() {
        assert!(constant_time_equal_hex("abc123", "abc123"));
        assert!(!constant_time_equal_hex("abc123", "abc124"));
        assert!(!constant_time_equal_hex("abc", "abcd"));
    }

    #[test]
    fn test_normalize_label() {
        assert_eq!(normalize_label("  hello  "), "hello");
        assert_eq!(normalize_label(""), "Remote client");
        let long = "a".repeat(100);
        assert_eq!(normalize_label(&long).len(), MAX_LABEL_LENGTH);
    }

    #[test]
    fn test_public_client_omits_sensitive() {
        let record = ClientRecord {
            id: "test".to_string(),
            label: "Test".to_string(),
            token_hash: "secret".to_string(),
            created_at: "2024".to_string(),
            last_used_at: None,
            revoked_at: None,
            expires_at: None,
            client_kind: None,
            dedupe_key: Some("dk".to_string()),
            uses_relay: false,
            last_transport: None,
            auth_method: None,
            pairing_id: None,
            device_name: None,
            device_platform: None,
            device_model: None,
            app_version: None,
        };
        let public = public_client(&record);
        assert!(public.get("tokenHash").is_none());
        assert!(public.get("dedupeKey").is_none());
        assert_eq!(public.get("id").unwrap().as_str().unwrap(), "test");
    }
}
