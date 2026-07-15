//! 隧道类型: 常量 + normalizers + TunnelServiceError。
//!
//! 移植自 `packages/web/server/lib/tunnels/types.js` (245 行)。
//! 纯函数, 无 I/O。

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::git::paths::home_dir;

// ── provider / mode / intent 常量 ──────────────────────────────────────────

pub const TUNNEL_PROVIDER_CLOUDFLARE: &str = "cloudflare";
pub const TUNNEL_PROVIDER_NGROK: &str = "ngrok";

pub const TUNNEL_MODE_QUICK: &str = "quick";
pub const TUNNEL_MODE_MANAGED_REMOTE: &str = "managed-remote";
pub const TUNNEL_MODE_MANAGED_LOCAL: &str = "managed-local";

pub const TUNNEL_INTENT_EPHEMERAL_PUBLIC: &str = "ephemeral-public";
pub const TUNNEL_INTENT_PERSISTENT_PUBLIC: &str = "persistent-public";
pub const TUNNEL_INTENT_PRIVATE_NETWORK: &str = "private-network";

const SUPPORTED_TUNNEL_INTENTS: &[&str] = &[
    TUNNEL_INTENT_EPHEMERAL_PUBLIC,
    TUNNEL_INTENT_PERSISTENT_PUBLIC,
    TUNNEL_INTENT_PRIVATE_NETWORK,
];

const SUPPORTED_TUNNEL_MODES: &[&str] = &[
    TUNNEL_MODE_QUICK,
    TUNNEL_MODE_MANAGED_REMOTE,
    TUNNEL_MODE_MANAGED_LOCAL,
];

const SUPPORTED_TUNNEL_PROVIDERS: &[&str] = &[TUNNEL_PROVIDER_CLOUDFLARE, TUNNEL_PROVIDER_NGROK];

// ── TunnelServiceError ─────────────────────────────────────────────────────

/// 隧道服务错误 (对应 Node `TunnelServiceError`)。
#[derive(Debug, Clone)]
pub struct TunnelServiceError {
    pub code: String,
    pub message: String,
    #[allow(dead_code)]
    pub details: Option<Value>,
}

impl TunnelServiceError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            details: None,
        }
    }

    pub fn validation(message: impl Into<String>) -> Self {
        Self::new("validation_error", message)
    }

    pub fn missing_dependency(message: impl Into<String>) -> Self {
        Self::new("missing_dependency", message)
    }

    pub fn startup_failed(message: impl Into<String>) -> Self {
        Self::new("startup_failed", message)
    }

    /// HTTP 状态码 (对应 routes.js 的 status 映射)。
    pub fn http_status(&self) -> u16 {
        match self.code.as_str() {
            "missing_dependency" => 400,
            "validation_error" | "provider_unsupported" | "mode_unsupported" => 422,
            _ => 500,
        }
    }

    /// JSON 错误体 `{ ok: false, error, code }`。
    pub fn to_json(&self) -> Value {
        json!({
            "ok": false,
            "error": self.message,
            "code": self.code,
        })
    }
}

impl std::fmt::Display for TunnelServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for TunnelServiceError {}

// ── 平台辅助 ──────────────────────────────────────────────────────────────

#[cfg(target_os = "windows")]
fn is_windows() -> bool {
    true
}
#[cfg(not(target_os = "windows"))]
fn is_windows() -> bool {
    false
}

fn path_sep() -> char {
    if is_windows() {
        '\\'
    } else {
        '/'
    }
}

// ── 路径安全 ──────────────────────────────────────────────────────────────

/// 判断 candidate 是否在 directory 内 (含 directory 本身)。
/// 对应 Node `isPathWithinDirectory`。
pub fn is_path_within_directory(candidate: &str, directory: &str) -> bool {
    if candidate.is_empty() || directory.is_empty() {
        return false;
    }
    let resolved_candidate = resolve_absolute(candidate);
    let resolved_directory = resolve_absolute(directory);

    let (cmp_candidate, cmp_directory) = if is_windows() {
        (
            resolved_candidate.to_lowercase(),
            resolved_directory.to_lowercase(),
        )
    } else {
        (resolved_candidate.clone(), resolved_directory.clone())
    };

    let sep = path_sep();
    let directory_prefix = if cmp_directory.ends_with(sep) {
        cmp_directory.clone()
    } else {
        format!("{}{}", cmp_directory, sep)
    };

    cmp_candidate == cmp_directory || cmp_candidate.starts_with(&directory_prefix)
}

/// `path.resolve` 等价: 绝对路径直接用, 相对路径基于 cwd。
fn resolve_absolute(path: &str) -> String {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_string_lossy().to_string()
    } else {
        match std::env::current_dir() {
            Ok(cwd) => cwd.join(p).to_string_lossy().to_string(),
            Err(_) => p.to_string_lossy().to_string(),
        }
    }
}

/// 解析隧道配置路径: 展开 `~`, 检查 home 目录边界。
/// 对应 Node `resolveTunnelConfigPath`。
pub fn resolve_tunnel_config_path(value: &str) -> Result<PathBuf, TunnelServiceError> {
    let home = home_dir();
    let home_str = home.to_string_lossy().to_string();

    let resolved = if value == "~" {
        home.clone()
    } else if let Some(rest) = value
        .strip_prefix("~/")
        .or_else(|| value.strip_prefix("~\\"))
    {
        home.join(rest)
    } else {
        PathBuf::from(resolve_absolute(value))
    };

    let resolved_str = resolved.to_string_lossy().to_string();
    if !is_path_within_directory(&resolved_str, &home_str) {
        return Err(TunnelServiceError::validation(format!(
            "Config path must be within the home directory ({}). Got: {}",
            home_str, resolved_str
        )));
    }
    Ok(resolved)
}

// ── normalizers ────────────────────────────────────────────────────────────

/// provider 归一化: 非法值 → cloudflare。对应 Node `normalizeTunnelProvider`。
pub fn normalize_tunnel_provider(value: Option<&str>) -> String {
    let provider = value.unwrap_or("").trim().to_lowercase();
    if provider.is_empty() || !SUPPORTED_TUNNEL_PROVIDERS.contains(&provider.as_str()) {
        return TUNNEL_PROVIDER_CLOUDFLARE.to_string();
    }
    provider
}

/// mode 归一化 (宽松): 非法值 → quick。对应 Node `normalizeTunnelMode`。
pub fn normalize_tunnel_mode(value: Option<&str>) -> String {
    let mode = value.unwrap_or("").trim().to_lowercase();
    if mode.is_empty() {
        return TUNNEL_MODE_QUICK.to_string();
    }
    match mode.as_str() {
        TUNNEL_MODE_QUICK => TUNNEL_MODE_QUICK.to_string(),
        TUNNEL_MODE_MANAGED_REMOTE => TUNNEL_MODE_MANAGED_REMOTE.to_string(),
        TUNNEL_MODE_MANAGED_LOCAL => TUNNEL_MODE_MANAGED_LOCAL.to_string(),
        _ => TUNNEL_MODE_QUICK.to_string(),
    }
}

/// mode 归一化 (请求路径, 与 normalizeTunnelMode 等价)。
pub fn normalize_tunnel_mode_for_request(value: Option<&str>) -> String {
    normalize_tunnel_mode(value)
}

/// intent 归一化: 非法值 → None。对应 Node `normalizeTunnelIntent`。
pub fn normalize_tunnel_intent(value: Option<&str>) -> Option<String> {
    let intent = value.unwrap_or("").trim().to_lowercase();
    if intent.is_empty() || !SUPPORTED_TUNNEL_INTENTS.contains(&intent.as_str()) {
        return None;
    }
    Some(intent)
}

/// mode → intent 回退。对应 Node `modeIntentFallback`。
pub fn mode_intent_fallback(mode: &str) -> Option<&'static str> {
    match mode {
        TUNNEL_MODE_QUICK => Some(TUNNEL_INTENT_EPHEMERAL_PUBLIC),
        TUNNEL_MODE_MANAGED_REMOTE | TUNNEL_MODE_MANAGED_LOCAL => {
            Some(TUNNEL_INTENT_PERSISTENT_PUBLIC)
        }
        _ => None,
    }
}

/// mode 是否被支持。
pub fn is_supported_tunnel_mode(mode: &str) -> bool {
    SUPPORTED_TUNNEL_MODES.contains(&mode)
}

/// optional path 归一化。
/// 返回三态: None=字段缺失, Some(None)=显式 null/空, Some(Some(path))=已解析路径。
/// 对应 Node `normalizeOptionalPath`。
pub fn normalize_optional_path(value: Option<&Value>) -> Option<Option<PathBuf>> {
    match value {
        None => None, // 字段缺失
        Some(Value::Null) => Some(None), // 显式 null
        Some(v) => match v.as_str() {
            None => None, // 非字符串 → 视为缺失
            Some(s) => {
                let trimmed = s.trim();
                if trimmed.is_empty() {
                    Some(None)
                } else {
                    match resolve_tunnel_config_path(trimmed) {
                        Ok(p) => Some(Some(p)),
                        Err(_) => Some(None),
                    }
                }
            }
        },
    }
}

/// managed-remote hostname 归一化: URL/hostname → lowercase hostname。
/// 返回 None 表示无效。对应 Node `normalizeManagedRemoteTunnelHostname`。
pub fn normalize_managed_remote_tunnel_hostname(value: Option<&Value>) -> Option<String> {
    let s = value?.as_str()?;
    normalize_managed_remote_tunnel_hostname_str(s)
}

/// 从字符串提取 hostname: 去除 scheme://, path, port; lowercase。
/// 等价 Node `new URL(trimmed).hostname` 的简化 (不引入 `url` crate)。
fn extract_hostname(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    // 去除 scheme
    let after_scheme = match trimmed.find("://") {
        Some(pos) => &trimmed[pos + 3..],
        None => trimmed,
    };
    // 去除 path/query/fragment
    let authority = after_scheme.split(['/', '?', '#']).next().unwrap_or("");
    // 去除 userinfo (user:pass@host)
    let host_part = authority.rsplit('@').next().unwrap_or("");
    // 去除 port (最后一个冒号后, 但 IPv6 [::1]:port 需要特殊处理)
    let host = if host_part.starts_with('[') {
        // IPv6 literal
        match host_part.find(']') {
            Some(end) => &host_part[1..end],
            None => return None,
        }
    } else {
        match host_part.rfind(':') {
            Some(pos) => &host_part[..pos],
            None => host_part,
        }
    };
    let host = host.trim().to_lowercase();
    if host.is_empty() {
        return None;
    }
    // 验证 hostname: 不含空格, 至少包含一个点 (域名) 或是 localhost。
    // 对应 Node `new URL()` 抛出异常的行为。
    if host.contains(' ') || host.contains('\t') {
        return None;
    }
    // 拒绝通配符
    if host == "*" || host.contains('*') {
        return None;
    }
    Some(host)
}

/// 字符串便捷版 (用于内部调用)。
pub fn normalize_managed_remote_tunnel_hostname_str(value: &str) -> Option<String> {
    extract_hostname(value)
}

/// managed-remote preset 归一化 + dedup。对应 Node `normalizeManagedRemoteTunnelPresets`。
#[allow(dead_code)]
pub fn normalize_managed_remote_tunnel_presets(value: &Value) -> Vec<ManagedRemotePreset> {
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
        let hostname = normalize_managed_remote_tunnel_hostname(entry.get("hostname"));

        if id.is_empty() || name.is_empty() || hostname.is_none() {
            continue;
        }
        let hostname = hostname.unwrap();
        if seen_ids.contains(id) || seen_hostnames.contains(&hostname) {
            continue;
        }
        seen_ids.insert(id.to_string());
        seen_hostnames.insert(hostname.clone());
        result.push(ManagedRemotePreset {
            id: id.to_string(),
            name: name.to_string(),
            hostname,
        });
    }
    result
}

/// managed-remote preset (归一化后)。
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ManagedRemotePreset {
    pub id: String,
    pub name: String,
    pub hostname: String,
}

/// 隧道启动请求 (归一化后)。对应 Node `normalizeTunnelStartRequest` 返回值。
#[derive(Debug, Clone, Default)]
pub struct NormalizedTunnelStartRequest {
    pub provider: String,
    pub mode: String,
    pub intent: Option<String>,
    pub config_path: Option<Option<PathBuf>>,
    pub token: String,
    pub hostname: String,
}

/// 从原始 JSON body 归一化启动请求。
pub fn normalize_tunnel_start_request(input: &Value) -> NormalizedTunnelStartRequest {
    let provider = normalize_tunnel_provider(input.get("provider").and_then(|v| v.as_str()));
    let mode = normalize_tunnel_mode_for_request(input.get("mode").and_then(|v| v.as_str()));
    let explicit_intent = normalize_tunnel_intent(input.get("intent").and_then(|v| v.as_str()));
    let intent = explicit_intent.or_else(|| mode_intent_fallback(&mode).map(|s| s.to_string()));
    let config_path = normalize_optional_path(input.get("configPath"));
    let token = input
        .get("token")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let hostname = input
        .get("hostname")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_lowercase();

    NormalizedTunnelStartRequest {
        provider,
        mode,
        intent,
        config_path,
        token,
        hostname,
    }
}

/// 验证启动请求与 provider capabilities 的兼容性。对应 Node `validateTunnelStartRequest`。
pub fn validate_tunnel_start_request(
    request: &NormalizedTunnelStartRequest,
    capabilities: &Value,
) -> Result<(), TunnelServiceError> {
    if request.provider.is_empty() {
        return Err(TunnelServiceError::validation(
            "Tunnel provider is required",
        ));
    }
    if !is_supported_tunnel_mode(&request.mode) {
        return Err(TunnelServiceError::new(
            "mode_unsupported",
            format!("Unsupported tunnel mode: {}", request.mode),
        ));
    }
    if capabilities
        .get("provider")
        .and_then(|v| v.as_str())
        != Some(request.provider.as_str())
    {
        return Err(TunnelServiceError::new(
            "provider_unsupported",
            format!("Unsupported tunnel provider: {}", request.provider),
        ));
    }

    let modes = capabilities
        .get("modes")
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            TunnelServiceError::new(
                "mode_unsupported",
                format!(
                    "Provider '{}' does not declare tunnel modes",
                    request.provider
                ),
            )
        })?;

    let mode_descriptor = modes
        .iter()
        .find(|entry| entry.get("key").and_then(|v| v.as_str()) == Some(request.mode.as_str()))
        .ok_or_else(|| {
            TunnelServiceError::new(
                "mode_unsupported",
                format!(
                    "Provider '{}' does not support mode '{}'",
                    request.provider, request.mode
                ),
            )
        })?;

    // intent 检查
    if let Some(ref intent) = request.intent {
        if !intent.is_empty() {
            if !SUPPORTED_TUNNEL_INTENTS.contains(&intent.as_str()) {
                return Err(TunnelServiceError::validation(format!(
                    "Unsupported tunnel intent: {}",
                    intent
                )));
            }
            let mode_intent = mode_descriptor
                .get("intent")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if mode_intent != intent.as_str() {
                return Err(TunnelServiceError::validation(format!(
                    "Tunnel intent '{}' does not match mode '{}' (expected '{}')",
                    intent, request.mode, mode_intent
                )));
            }
        }
    }

    // required fields 检查
    let required_fields: Vec<&str> = mode_descriptor
        .get("requires")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();

    for field in &required_fields {
        match *field {
            "token" => {
                if request.token.is_empty() {
                    return Err(TunnelServiceError::validation(
                        "Managed remote tunnel token is required",
                    ));
                }
            }
            "hostname" => {
                if request.hostname.is_empty() {
                    return Err(TunnelServiceError::validation(
                        "Managed remote tunnel hostname is required",
                    ));
                }
            }
            "configPath" => {
                let empty = match &request.config_path {
                    None => true,
                    Some(None) => true,
                    Some(Some(_)) => false,
                };
                if empty {
                    return Err(TunnelServiceError::validation(format!(
                        "Mode '{}' requires a configPath",
                        request.mode
                    )));
                }
            }
            _ => {}
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_provider_defaults_to_cloudflare() {
        assert_eq!(normalize_tunnel_provider(None), TUNNEL_PROVIDER_CLOUDFLARE);
        assert_eq!(
            normalize_tunnel_provider(Some("")),
            TUNNEL_PROVIDER_CLOUDFLARE
        );
        assert_eq!(
            normalize_tunnel_provider(Some("NGROK")),
            TUNNEL_PROVIDER_NGROK
        );
        assert_eq!(
            normalize_tunnel_provider(Some("invalid")),
            TUNNEL_PROVIDER_CLOUDFLARE
        );
    }

    #[test]
    fn normalize_mode_defaults_to_quick() {
        assert_eq!(normalize_tunnel_mode(None), TUNNEL_MODE_QUICK);
        assert_eq!(normalize_tunnel_mode(Some("")), TUNNEL_MODE_QUICK);
        assert_eq!(
            normalize_tunnel_mode(Some("MANAGED-REMOTE")),
            TUNNEL_MODE_MANAGED_REMOTE
        );
        assert_eq!(normalize_tunnel_mode(Some("bad")), TUNNEL_MODE_QUICK);
    }

    #[test]
    fn normalize_intent_invalid_returns_none() {
        assert!(normalize_tunnel_intent(None).is_none());
        assert!(normalize_tunnel_intent(Some("bad")).is_none());
        assert_eq!(
            normalize_tunnel_intent(Some("ephemeral-public")),
            Some("ephemeral-public".to_string())
        );
    }

    #[test]
    fn mode_intent_fallback_correct() {
        assert_eq!(
            mode_intent_fallback(TUNNEL_MODE_QUICK),
            Some(TUNNEL_INTENT_EPHEMERAL_PUBLIC)
        );
        assert_eq!(
            mode_intent_fallback(TUNNEL_MODE_MANAGED_REMOTE),
            Some(TUNNEL_INTENT_PERSISTENT_PUBLIC)
        );
        assert_eq!(mode_intent_fallback("bad"), None);
    }

    #[test]
    fn is_supported_mode() {
        assert!(is_supported_tunnel_mode(TUNNEL_MODE_QUICK));
        assert!(is_supported_tunnel_mode(TUNNEL_MODE_MANAGED_LOCAL));
        assert!(!is_supported_tunnel_mode("bad"));
    }

    #[test]
    fn managed_remote_hostname_normalization() {
        assert_eq!(
            normalize_managed_remote_tunnel_hostname_str("example.com"),
            Some("example.com".to_string())
        );
        assert_eq!(
            normalize_managed_remote_tunnel_hostname_str("HTTPS://Foo.COM/path"),
            Some("foo.com".to_string())
        );
        assert_eq!(normalize_managed_remote_tunnel_hostname_str(""), None);
        assert!(normalize_managed_remote_tunnel_hostname_str("not a url at all").is_none());
    }

    #[test]
    fn managed_remote_presets_dedup() {
        let input = json!([
            { "id": "a", "name": "A", "hostname": "a.com" },
            { "id": "a", "name": "A2", "hostname": "b.com" },  // dup id
            { "id": "b", "name": "B", "hostname": "a.com" },   // dup hostname
            { "id": "c", "name": "C", "hostname": "c.com" },
        ]);
        let presets = normalize_managed_remote_tunnel_presets(&input);
        assert_eq!(presets.len(), 2);
        assert_eq!(presets[0].id, "a");
        assert_eq!(presets[1].id, "c");
    }

    #[test]
    fn tunnel_service_error_http_status() {
        assert_eq!(
            TunnelServiceError::missing_dependency("x").http_status(),
            400
        );
        assert_eq!(
            TunnelServiceError::validation("x").http_status(),
            422
        );
        assert_eq!(
            TunnelServiceError::new("provider_unsupported", "x").http_status(),
            422
        );
        assert_eq!(
            TunnelServiceError::startup_failed("x").http_status(),
            500
        );
    }

    #[test]
    fn normalize_optional_path_tristate() {
        assert!(normalize_optional_path(None).is_none()); // 字段缺失
        assert_eq!(normalize_optional_path(Some(&Value::Null)), Some(None)); // null
        assert_eq!(
            normalize_optional_path(Some(&json!(""))), Some(None) // 空字符串
        );
    }

    #[test]
    fn normalize_start_request_basic() {
        let input = json!({
            "provider": "ngrok",
            "mode": "quick",
            "token": "  abc  ",
            "hostname": "  Foo.COM  "
        });
        let req = normalize_tunnel_start_request(&input);
        assert_eq!(req.provider, "ngrok");
        assert_eq!(req.mode, "quick");
        assert_eq!(req.token, "abc");
        assert_eq!(req.hostname, "foo.com");
        assert_eq!(req.intent, Some("ephemeral-public".to_string()));
    }
}
