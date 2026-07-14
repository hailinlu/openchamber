//! SSH 类型定义 — 复现 Electron ssh-manager.mjs 的数据模型。
//!
//! Instance / Status / Forward / HostCandidate。

use serde::{Deserialize, Serialize};

/// SSH 实例配置 (持久化在 settings.json `desktopSshInstances` 中)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Instance {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nickname: Option<String>,
    pub ssh_command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssh_parsed: Option<ParsedSsh>,
    #[serde(default = "default_connection_timeout")]
    pub connection_timeout_sec: u32,
    #[serde(default)]
    pub remote_openchamber: RemoteOpenchamber,
    #[serde(default)]
    pub local_forward: LocalForward,
    #[serde(default)]
    pub auth: Auth,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub port_forwards: Vec<Forward>,
}

fn default_connection_timeout() -> u32 {
    60
}

/// 解析后的 SSH 命令 (destination + args)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedSsh {
    pub destination: String,
    pub args: Vec<String>,
}

/// 远程 OpenChamber 配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteOpenchamber {
    #[serde(default = "default_mode")]
    pub mode: String,
    #[serde(default = "default_true_fn")]
    pub keep_running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preferred_port: Option<u16>,
    #[serde(default = "default_install_method")]
    pub install_method: String,
    #[serde(default)]
    pub upload_bundle_over_ssh: bool,
}

fn default_mode() -> String {
    "managed".to_string()
}
fn default_true_fn() -> bool {
    true
}
fn default_install_method() -> String {
    "bun".to_string()
}

impl Default for RemoteOpenchamber {
    fn default() -> Self {
        Self {
            mode: default_mode(),
            keep_running: true,
            preferred_port: None,
            install_method: default_install_method(),
            upload_bundle_over_ssh: false,
        }
    }
}

/// 本地转发配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalForward {
    #[serde(default = "default_bind_host")]
    pub bind_host: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preferred_local_port: Option<u16>,
}

fn default_bind_host() -> String {
    "127.0.0.1".to_string()
}

impl Default for LocalForward {
    fn default() -> Self {
        Self {
            bind_host: default_bind_host(),
            preferred_local_port: None,
        }
    }
}

/// 认证配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(Default)]
pub struct Auth {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssh_password: Option<StoredSecret>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub openchamber_password: Option<StoredSecret>,
}


/// 存储的密钥 (密码等)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredSecret {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_store")]
    pub store: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

fn default_store() -> String {
    "settings".to_string()
}

/// 端口转发配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Forward {
    pub id: String,
    #[serde(default = "default_true_fn")]
    pub enabled: bool,
    #[serde(default = "default_forward_type")]
    #[serde(rename = "type")]
    pub ftype: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_port: Option<u16>,
}

fn default_forward_type() -> String {
    "local".to_string()
}

/// 连接阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    Idle,
    ConfigResolved,
    AuthCheck,
    MasterConnecting,
    RemoteProbe,
    Installing,
    Updating,
    ServerDetecting,
    ServerStarting,
    Forwarding,
    Ready,
    Degraded,
    Error,
}

impl Phase {
    pub fn as_str(&self) -> &'static str {
        match self {
            Phase::Idle => "idle",
            Phase::ConfigResolved => "config_resolved",
            Phase::AuthCheck => "auth_check",
            Phase::MasterConnecting => "master_connecting",
            Phase::RemoteProbe => "remote_probe",
            Phase::Installing => "installing",
            Phase::Updating => "updating",
            Phase::ServerDetecting => "server_detecting",
            Phase::ServerStarting => "server_starting",
            Phase::Forwarding => "forwarding",
            Phase::Ready => "ready",
            Phase::Degraded => "degraded",
            Phase::Error => "error",
        }
    }
}

/// SSH 实例状态快照 (推送到 UI)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    pub id: String,
    pub phase: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_port: Option<u16>,
    #[serde(default)]
    pub started_by_us: bool,
    #[serde(default)]
    pub retry_attempt: u32,
    #[serde(default)]
    pub requires_user_action: bool,
    pub updated_at_ms: u64,
}

impl Status {
    pub fn idle(id: &str) -> Self {
        Self {
            id: id.to_string(),
            phase: Phase::Idle.as_str().to_string(),
            detail: None,
            local_url: None,
            local_port: None,
            remote_port: None,
            started_by_us: false,
            retry_attempt: 0,
            requires_user_action: false,
            updated_at_ms: now_millis(),
        }
    }

    pub fn new(id: &str, phase: Phase) -> Self {
        Self {
            id: id.to_string(),
            phase: phase.as_str().to_string(),
            detail: None,
            local_url: None,
            local_port: None,
            remote_port: None,
            started_by_us: false,
            retry_attempt: 0,
            requires_user_action: false,
            updated_at_ms: now_millis(),
        }
    }
}

/// SSH config 中的 Host 候选 (importHosts 返回)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostCandidate {
    pub host: String,
    pub pattern: bool,
    pub source: String, // "user" | "global"
    pub ssh_command: String,
}

// --- 辅助函数 ---

pub fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 规范化 bind host (仅允许 127.0.0.1 / localhost / 0.0.0.0)。
pub fn sanitize_bind_host(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return "127.0.0.1".to_string();
    }
    match trimmed {
        "127.0.0.1" | "localhost" | "0.0.0.0" => trimmed.to_string(),
        _ => "127.0.0.1".to_string(),
    }
}

/// djb2 变种哈希 (用于 control socket 路径)。
pub fn djb2_hash(s: &str) -> String {
    let mut hash: u32 = 5381;
    for byte in s.bytes() {
        hash = hash.wrapping_mul(33).wrapping_add(byte as u32);
    }
    format!("{:08x}", hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_as_str() {
        assert_eq!(Phase::Idle.as_str(), "idle");
        assert_eq!(Phase::Ready.as_str(), "ready");
        assert_eq!(Phase::Error.as_str(), "error");
    }

    #[test]
    fn status_idle() {
        let s = Status::idle("test-1");
        assert_eq!(s.id, "test-1");
        assert_eq!(s.phase, "idle");
        assert!(!s.started_by_us);
    }

    #[test]
    fn sanitize_bind_host_defaults() {
        assert_eq!(sanitize_bind_host(""), "127.0.0.1");
        assert_eq!(sanitize_bind_host("invalid"), "127.0.0.1");
        assert_eq!(sanitize_bind_host("0.0.0.0"), "0.0.0.0");
        assert_eq!(sanitize_bind_host("localhost"), "localhost");
    }

    #[test]
    fn djb2_hash_deterministic() {
        let h1 = djb2_hash("test-id-123");
        let h2 = djb2_hash("test-id-123");
        assert_eq!(h1, h2);
        assert_ne!(djb2_hash("a"), djb2_hash("b"));
    }
}
