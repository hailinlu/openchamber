//! Managed remote tunnel token 持久化。
//!
//! 移植自 `packages/web/server/lib/tunnels/managed-config.js` (201 行)。
//!
//! 文件: `$DATA_DIR/cloudflare-managed-remote-tunnels.json`
//! 格式: `{ version: 1, tunnels: [{ id, name, hostname, token, updatedAt }] }`
//! mode 0o600。写入串行化 (tokio Mutex)。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::github::settings::data_dir;
use crate::tunnels::types::normalize_managed_remote_tunnel_hostname_str;
use crate::tunnels::CLOUDFLARE_MANAGED_REMOTE_TUNNELS_VERSION;

/// managed remote tunnel 配置文件路径。
pub fn managed_remote_tunnels_file_path() -> PathBuf {
    data_dir().join("cloudflare-managed-remote-tunnels.json")
}

/// 旧版 named tunnel 配置文件路径 (用于迁移)。
fn legacy_named_tunnels_file_path() -> PathBuf {
    data_dir().join("cloudflare-named-tunnels.json")
}

/// managed remote tunnel 条目。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagedTunnelEntry {
    pub id: String,
    pub name: String,
    pub hostname: String,
    pub token: String,
    #[serde(rename = "updatedAt")]
    pub updated_at: i64,
}

/// managed remote tunnel 配置文件内容。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagedTunnelConfig {
    pub version: i64,
    pub tunnels: Vec<ManagedTunnelEntry>,
}

impl Default for ManagedTunnelConfig {
    fn default() -> Self {
        Self {
            version: CLOUDFLARE_MANAGED_REMOTE_TUNNELS_VERSION,
            tunnels: vec![],
        }
    }
}

/// 运行时 (串行化写入锁)。
pub struct ManagedConfigRuntime {
    lock: Mutex<()>,
}

impl ManagedConfigRuntime {
    pub fn new() -> Self {
        Self {
            lock: Mutex::new(()),
        }
    }

    /// 清洗配置条目: 去重 (id + hostname), 过滤无效。
    fn sanitize_entries(value: &Value) -> Vec<ManagedTunnelEntry> {
        let arr = match value.as_array() {
            Some(a) => a,
            None => return vec![],
        };

        let mut result = vec![];
        let mut seen_ids = std::collections::HashSet::new();
        let mut seen_hostnames = std::collections::HashSet::new();

        for entry in arr {
            let id = entry.get("id").and_then(|v| v.as_str()).unwrap_or("").trim();
            let name = entry
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            let hostname = entry
                .get("hostname")
                .and_then(|v| v.as_str())
                .and_then(normalize_managed_remote_tunnel_hostname_str);
            let token = entry
                .get("token")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            let updated_at = entry
                .get("updatedAt")
                .and_then(|v| v.as_i64())
                .unwrap_or_else(now_ts);

            if id.is_empty() || name.is_empty() || hostname.is_none() || token.is_empty() {
                continue;
            }
            let hostname = hostname.unwrap();
            if seen_ids.contains(id) || seen_hostnames.contains(&hostname) {
                continue;
            }
            seen_ids.insert(id.to_string());
            seen_hostnames.insert(hostname.clone());
            result.push(ManagedTunnelEntry {
                id: id.to_string(),
                name: name.to_string(),
                hostname,
                token: token.to_string(),
                updated_at,
            });
        }
        result
    }

    async fn write_to_disk(data: &ManagedTunnelConfig) -> std::io::Result<()> {
        let path = managed_remote_tunnels_file_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = serde_json::to_string_pretty(data)?;
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

    /// 从旧版文件迁移。
    async fn migrate_from_legacy(&self) -> ManagedTunnelConfig {
        let legacy_path = legacy_named_tunnels_file_path();
        match tokio::fs::read_to_string(&legacy_path).await {
            Ok(raw) => {
                match serde_json::from_str::<Value>(&raw) {
                    Ok(parsed) => {
                        let tunnels = Self::sanitize_entries(parsed.get("tunnels").unwrap_or(&Value::Null));
                        let migrated = ManagedTunnelConfig {
                            version: CLOUDFLARE_MANAGED_REMOTE_TUNNELS_VERSION,
                            tunnels,
                        };
                        let _ = Self::write_to_disk(&migrated).await;
                        migrated
                    }
                    Err(_) => ManagedTunnelConfig::default(),
                }
            }
            Err(_) => ManagedTunnelConfig::default(),
        }
    }

    /// 读取配置 (文件不存在时尝试迁移)。
    pub async fn read(&self) -> ManagedTunnelConfig {
        let path = managed_remote_tunnels_file_path();
        match tokio::fs::read_to_string(&path).await {
            Ok(raw) => {
                match serde_json::from_str::<Value>(&raw) {
                    Ok(parsed) => ManagedTunnelConfig {
                        version: CLOUDFLARE_MANAGED_REMOTE_TUNNELS_VERSION,
                        tunnels: Self::sanitize_entries(parsed.get("tunnels").unwrap_or(&Value::Null)),
                    },
                    Err(_) => ManagedTunnelConfig::default(),
                }
            }
            Err(_) => self.migrate_from_legacy().await,
        }
    }

    /// upsert: 按 id 或 hostname 去重后插入/更新。
    pub async fn upsert(
        &self,
        id: &str,
        name: &str,
        hostname: &str,
        token: &str,
    ) -> Option<ManagedTunnelConfig> {
        let normalized_id = id.trim();
        let normalized_name = name.trim();
        let normalized_hostname = normalize_managed_remote_tunnel_hostname_str(hostname)?;
        let normalized_token = token.trim();
        if normalized_id.is_empty()
            || normalized_name.is_empty()
            || normalized_hostname.is_empty()
            || normalized_token.is_empty()
        {
            return None;
        }

        let _guard = self.lock.lock().await;

        let current = self.read().await;
        let without_conflicts: Vec<ManagedTunnelEntry> = current
            .tunnels
            .into_iter()
            .filter(|entry| {
                entry.id != normalized_id && entry.hostname != normalized_hostname
            })
            .collect();

        let mut next = without_conflicts;
        next.push(ManagedTunnelEntry {
            id: normalized_id.to_string(),
            name: normalized_name.to_string(),
            hostname: normalized_hostname.clone(),
            token: normalized_token.to_string(),
            updated_at: now_ts(),
        });

        let config = ManagedTunnelConfig {
            version: CLOUDFLARE_MANAGED_REMOTE_TUNNELS_VERSION,
            tunnels: Self::sanitize_entries(&json!(next
                .iter()
                .map(|e| serde_json::to_value(e).unwrap())
                .collect::<Vec<_>>())),
        };

        let _ = Self::write_to_disk(&config).await;
        Some(config)
    }

    /// resolve: 按 presetId 或 hostname 查找 token。
    pub async fn resolve(&self, preset_id: &str, hostname: &str) -> String {
        let normalized_preset_id = preset_id.trim();
        let normalized_hostname = normalize_managed_remote_tunnel_hostname_str(hostname);
        let config = self.read().await;

        if !normalized_preset_id.is_empty() {
            if let Some(entry) = config.tunnels.iter().find(|e| e.id == normalized_preset_id) {
                if !entry.token.is_empty() {
                    return entry.token.clone();
                }
            }
        }

        if let Some(host) = normalized_hostname {
            if let Some(entry) = config.tunnels.iter().find(|e| e.hostname == host) {
                if !entry.token.is_empty() {
                    return entry.token.clone();
                }
            }
        }

        String::new()
    }
}

impl Default for ManagedConfigRuntime {
    fn default() -> Self {
        Self::new()
    }
}

fn now_ts() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sanitize_empty_array() {
        let result = ManagedConfigRuntime::sanitize_entries(&json!([]));
        assert!(result.is_empty());
    }

    #[test]
    fn sanitize_filters_invalid() {
        let input = json!([
            { "id": "", "name": "A", "hostname": "a.com", "token": "tok", "updatedAt": 100 },
            { "id": "b", "name": "", "hostname": "b.com", "token": "tok", "updatedAt": 100 },
            { "id": "c", "name": "C", "hostname": "", "token": "tok", "updatedAt": 100 },
            { "id": "d", "name": "D", "hostname": "d.com", "token": "", "updatedAt": 100 },
            { "id": "e", "name": "E", "hostname": "e.com", "token": "tok", "updatedAt": 100 },
        ]);
        let result = ManagedConfigRuntime::sanitize_entries(&input);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, "e");
    }

    #[test]
    fn sanitize_dedup() {
        let input = json!([
            { "id": "a", "name": "A", "hostname": "a.com", "token": "tok1", "updatedAt": 100 },
            { "id": "a", "name": "A2", "hostname": "b.com", "token": "tok2", "updatedAt": 200 },
            { "id": "c", "name": "C", "hostname": "a.com", "token": "tok3", "updatedAt": 300 },
        ]);
        let result = ManagedConfigRuntime::sanitize_entries(&input);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, "a");
        assert_eq!(result[0].hostname, "a.com");
    }

    #[test]
    fn sanitize_non_array() {
        let result = ManagedConfigRuntime::sanitize_entries(&json!("not an array"));
        assert!(result.is_empty());
    }
}
