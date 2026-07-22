//! cloudflared 子进程管理。
//!
//! 移植自 `packages/web/server/lib/cloudflare-tunnel.js` (619 行)。
//!
//! 3 种模式:
//!   - quick: `cloudflared tunnel --url <origin>` → 从 stdout 提取 trycloudflare URL
//!   - managed-remote: `cloudflared tunnel run --token-file <path>` → publicUrl = https://hostname
//!   - managed-local: `cloudflared tunnel [--config <path>] run` → 从 config 提取 hostname

use std::path::PathBuf;
use std::time::Duration;

use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::time::timeout;

use crate::tunnels::executable_search::resolve_executable_launch_target;
use crate::tunnels::install_help::get_tunnel_dependency_install_info;
use crate::tunnels::providers::{build_spawn_command, StartContext, TunnelController};
use crate::tunnels::types::{
    normalize_managed_remote_tunnel_hostname_str, TunnelServiceError,
    NormalizedTunnelStartRequest, TUNNEL_MODE_MANAGED_LOCAL, TUNNEL_MODE_MANAGED_REMOTE,
    TUNNEL_MODE_QUICK, TUNNEL_PROVIDER_CLOUDFLARE,
};

const DEFAULT_STARTUP_TIMEOUT_MS: u64 = 30000;
const MANAGED_TUNNEL_STARTUP_TIMEOUT_MS: u64 = 20000;
const MANAGED_TUNNEL_LIVENESS_FALLBACK_MS: u64 = 6000;

const MANAGED_LOCAL_CONFIG_MAX_BYTES: u64 = 256 * 1024;

static TRY_CF_URL_REGEX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"https://[a-z0-9-]+\.trycloudflare\.com").unwrap());

static READY_LOG_PATTERNS: Lazy<Vec<Regex>> = Lazy::new(|| {
    vec![
        Regex::new(r"(?i)registered tunnel connection").unwrap(),
        Regex::new(r"(?i)connection[^\n]*registered").unwrap(),
        Regex::new(r"(?i)starting metrics server").unwrap(),
        Regex::new(r"(?i)connected to edge").unwrap(),
    ]
});

static FATAL_LOG_PATTERNS: Lazy<Vec<Regex>> = Lazy::new(|| {
    vec![
        Regex::new(r"(?i)error parsing.*config").unwrap(),
        Regex::new(r"(?i)failed to .*config").unwrap(),
        Regex::new(r"(?i)invalid token").unwrap(),
        Regex::new(r"(?i)unauthorized").unwrap(),
        Regex::new(r"(?i)credentials file .* not found").unwrap(),
        Regex::new(r"(?i)provided tunnel credentials are invalid").unwrap(),
    ]
});

static MANAGED_LOCAL_CONFIG_ALLOWED_EXTENSIONS: Lazy<std::collections::HashSet<&'static str>> =
    Lazy::new(|| [".yml", ".yaml", ".json"].iter().copied().collect());

/// cloudflare provider capabilities。
pub fn capabilities() -> Value {
    json!({
        "provider": TUNNEL_PROVIDER_CLOUDFLARE,
        "defaults": {
            "mode": TUNNEL_MODE_QUICK,
            "optionDefaults": {},
        },
        "modes": [
            {
                "key": TUNNEL_MODE_QUICK,
                "label": "Quick Tunnel",
                "intent": "ephemeral-public",
                "requires": [],
                "supports": ["sessionTTL"],
                "stability": "ga",
            },
            {
                "key": TUNNEL_MODE_MANAGED_REMOTE,
                "label": "Managed Remote Tunnel",
                "intent": "persistent-public",
                "requires": ["token", "hostname"],
                "supports": ["customDomain", "sessionTTL"],
                "stability": "ga",
            },
            {
                "key": TUNNEL_MODE_MANAGED_LOCAL,
                "label": "Managed Local Tunnel",
                "intent": "persistent-public",
                "requires": [],
                "supports": ["configFile", "customDomain", "sessionTTL"],
                "stability": "ga",
            },
        ],
    })
}

/// 检查 cloudflared 可用性。
pub async fn check_availability() -> Value {
    let install_info = get_tunnel_dependency_install_info(TUNNEL_PROVIDER_CLOUDFLARE);

    match resolve_executable_launch_target("cloudflared") {
        Some((command, env)) => {
            let output = tokio::process::Command::new(&command)
                .arg("--version")
                .envs(&env)
                .output()
                .await;

            match output {
                Ok(out) if out.status.success() => {
                    let version = String::from_utf8_lossy(&out.stdout)
                        .trim()
                        .to_string();
                    let version = if version.is_empty() {
                        String::from_utf8_lossy(&out.stderr).trim().to_string()
                    } else {
                        version
                    };
                    json!({
                        "available": true,
                        "path": command,
                        "version": version,
                        "dependency": install_info.dependency,
                        "installCommand": install_info.install_command,
                        "installUrl": install_info.install_url,
                        "platform": install_info.platform,
                        "message": install_info.message,
                    })
                }
                _ => json!({
                    "available": false,
                    "path": null,
                    "version": null,
                    "dependency": install_info.dependency,
                    "installCommand": install_info.install_command,
                    "installUrl": install_info.install_url,
                    "platform": install_info.platform,
                    "message": install_info.message,
                }),
            }
        }
        None => json!({
            "available": false,
            "path": null,
            "version": null,
            "dependency": install_info.dependency,
            "installCommand": install_info.install_command,
            "installUrl": install_info.install_url,
            "platform": install_info.platform,
            "message": install_info.message,
        }),
    }
}

/// 检查 Cloudflare API 可达性。
pub async fn check_api_reachability() -> (bool, Option<u16>, Option<String>) {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(e) => return (false, None, Some(e.to_string())),
    };

    match client.get("https://api.trycloudflare.com/").send().await {
        Ok(resp) => (true, Some(resp.status().as_u16()), None),
        Err(e) => (false, None, Some(e.to_string())),
    }
}

/// 诊断。
pub async fn diagnose(request: &Value) -> Value {
    let dependency = check_availability().await;
    let (network_reachable, network_status, network_error) = check_api_reachability().await;
    let install_info = get_tunnel_dependency_install_info(TUNNEL_PROVIDER_CLOUDFLARE);

    let dependency_available = dependency.get("available").and_then(|v| v.as_bool()).unwrap_or(false);
    let startup_ready = dependency_available && network_reachable;
    let startup_detail = if startup_ready {
        "Provider dependency and network checks passed."
    } else {
        "Resolve provider checks before starting tunnels."
    };

    let provider_checks = json!([
        {
            "id": "dependency",
            "label": "cloudflared installed",
            "status": if dependency_available { "pass" } else { "fail" },
            "detail": if dependency_available {
                dependency.get("version").and_then(|v| v.as_str())
                    .or_else(|| dependency.get("path").and_then(|v| v.as_str()))
                    .unwrap_or("cloudflared available")
            } else {
                &install_info.message
            },
        },
        {
            "id": "network",
            "label": "Cloudflare API reachable",
            "status": if network_reachable { "pass" } else { "fail" },
            "detail": if network_reachable {
                network_status.map(|s| format!("HTTP {}", s)).unwrap_or_else(|| "Reachable".to_string())
            } else {
                network_error.as_deref().unwrap_or("Could not reach api.trycloudflare.com").to_string()
            },
        },
    ]);

    // quick mode checks
    let quick_checks = json!([
        {
            "id": "startup_readiness",
            "label": "Provider startup readiness",
            "status": if startup_ready { "pass" } else { "fail" },
            "detail": startup_detail,
        },
        {
            "id": "quick_mode_prerequisites",
            "label": "Quick tunnel prerequisites",
            "status": if network_reachable { "pass" } else { "fail" },
            "detail": if network_reachable {
                "Cloudflare edge is reachable for quick tunnels."
            } else {
                "Cloudflare edge is not reachable for quick tunnels."
            },
        },
    ]);

    // managed local checks
    let config_path_str = request.get("configPath").and_then(|v| v.as_str()).unwrap_or("");
    let hostname_str = request.get("hostname").and_then(|v| v.as_str()).unwrap_or("");
    let managed_local_inspection = inspect_managed_local_config(config_path_str, hostname_str);
    let managed_local_checks = json!([
        {
            "id": "startup_readiness",
            "label": "Provider startup readiness",
            "status": if startup_ready { "pass" } else { "fail" },
            "detail": startup_detail,
        },
        {
            "id": "managed_local_config",
            "label": "Managed local config",
            "status": if managed_local_inspection.ok { "pass" } else { "fail" },
            "detail": if managed_local_inspection.ok {
                format!(
                    "{}{}",
                    managed_local_inspection.effective_config_path,
                    managed_local_inspection.resolved_hostname.as_deref()
                        .map(|h| format!(" ({})", h))
                        .unwrap_or_default()
                )
            } else {
                managed_local_inspection.error.clone().unwrap_or_default()
            },
        },
    ]);

    // managed remote checks
    let normalized_host = normalize_managed_remote_tunnel_hostname_str(hostname_str);
    let hostname_missing = normalized_host.is_none();
    let token_str = request.get("token").and_then(|v| v.as_str()).unwrap_or("");
    let token_missing = token_str.trim().is_empty();
    let has_saved_managed_remote_profile = request
        .get("hasSavedManagedRemoteProfile")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let token_provided = request.get("tokenProvided").and_then(|v| v.as_bool()).unwrap_or(false);
    let hostname_provided = request.get("hostnameProvided").and_then(|v| v.as_bool()).unwrap_or(false);
    let has_explicit_managed_remote_input = token_provided || hostname_provided;
    let can_use_saved_profile_for_hostname =
        !has_explicit_managed_remote_input && hostname_missing && has_saved_managed_remote_profile;
    let can_use_saved_profile_for_token =
        !has_explicit_managed_remote_input && token_missing && has_saved_managed_remote_profile;
    let saved_profile_ready_detail = "at least one saved profile present";

    let remote_token_validation = validate_token_shape(token_str);

    let managed_remote_checks = json!([
        {
            "id": "startup_readiness",
            "label": "Provider startup readiness",
            "status": if startup_ready { "pass" } else { "fail" },
            "detail": startup_detail,
        },
        {
            "id": "managed_remote_hostname",
            "label": "Managed remote hostname",
            "status": if normalized_host.is_some() || can_use_saved_profile_for_hostname { "pass" } else { "fail" },
            "detail": if let Some(ref h) = normalized_host {
                h.clone()
            } else if can_use_saved_profile_for_hostname {
                saved_profile_ready_detail.to_string()
            } else {
                "Managed remote hostname is required (use --hostname).".to_string()
            },
        },
        {
            "id": "managed_remote_token",
            "label": "Managed remote token",
            "status": if remote_token_validation.ok || can_use_saved_profile_for_token { "pass" } else { "fail" },
            "detail": if can_use_saved_profile_for_token {
                saved_profile_ready_detail.to_string()
            } else {
                remote_token_validation.detail.clone()
            },
        },
    ]);

    let all_modes = json!([
        describe_mode(TUNNEL_MODE_QUICK, quick_checks),
        describe_mode(TUNNEL_MODE_MANAGED_REMOTE, managed_remote_checks),
        describe_mode(TUNNEL_MODE_MANAGED_LOCAL, managed_local_checks),
    ]);

    let mode_filter = request
        .get("mode")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty());

    let modes_array = all_modes.as_array().unwrap();
    let modes: Vec<Value> = match &mode_filter {
        Some(filter) => modes_array
            .iter()
            .filter(|entry| entry.get("mode").and_then(|v| v.as_str()) == Some(filter.as_str()))
            .cloned()
            .collect(),
        None => modes_array.clone(),
    };

    json!({
        "providerChecks": provider_checks,
        "modes": modes,
    })
}

fn describe_mode(mode: &str, checks: Value) -> Value {
    let checks_arr = checks.as_array().cloned().unwrap_or_default();
    let failures = checks_arr
        .iter()
        .filter(|c| c.get("status").and_then(|v| v.as_str()) == Some("fail"))
        .count();
    let warnings = checks_arr
        .iter()
        .filter(|c| c.get("status").and_then(|v| v.as_str()) == Some("warn"))
        .count();

    let blockers: Vec<String> = checks_arr
        .iter()
        .filter(|c| {
            c.get("status").and_then(|v| v.as_str()) == Some("fail")
                && c.get("id").and_then(|v| v.as_str()) != Some("startup_readiness")
        })
        .map(|c| {
            c.get("detail")
                .and_then(|v| v.as_str())
                .or_else(|| c.get("label").and_then(|v| v.as_str()))
                .or_else(|| c.get("id").and_then(|v| v.as_str()))
                .unwrap_or("")
                .to_string()
        })
        .collect();

    json!({
        "mode": mode,
        "checks": checks_arr,
        "summary": {
            "ready": failures == 0,
            "failures": failures,
            "warnings": warnings,
        },
        "ready": failures == 0,
        "blockers": blockers,
    })
}

struct TokenValidation {
    ok: bool,
    detail: String,
}

fn validate_token_shape(value: &str) -> TokenValidation {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return TokenValidation {
            ok: false,
            detail: "Managed remote token is missing.".to_string(),
        };
    }
    if trimmed.chars().any(|c| c.is_whitespace()) {
        return TokenValidation {
            ok: false,
            detail: "Managed remote token has whitespace; provide the raw token value.".to_string(),
        };
    }
    TokenValidation {
        ok: true,
        detail: "Managed remote token looks valid.".to_string(),
    }
}

pub struct ManagedLocalInspection {
    pub ok: bool,
    pub effective_config_path: String,
    pub resolved_hostname: Option<String>,
    pub error: Option<String>,
}

/// 检查 managed-local config。对应 Node `inspectManagedLocalCloudflareConfig`。
pub fn inspect_managed_local_config(config_path: &str, hostname: &str) -> ManagedLocalInspection {
    let requested_path = config_path.trim();
    let default_path = default_cloudflared_config_path();
    let effective_config_path = if requested_path.is_empty() {
        default_path.to_string_lossy().to_string()
    } else {
        requested_path.to_string()
    };

    let label = if requested_path.is_empty() {
        "Managed local tunnel default config"
    } else {
        "Managed local tunnel config"
    };

    // 检查文件可读
    if let Err(e) = assert_readable_file(&effective_config_path, label) {
        return ManagedLocalInspection {
            ok: false,
            effective_config_path,
            resolved_hostname: None,
            error: Some(e),
        };
    }

    // 提取 hostname
    let config_hostname = match extract_hostname_from_config(&effective_config_path) {
        Ok(h) => h,
        Err(e) => {
            return ManagedLocalInspection {
                ok: false,
                effective_config_path,
                resolved_hostname: None,
                error: Some(e),
            };
        }
    };

    let resolved_hostname = normalize_managed_remote_tunnel_hostname_str(hostname)
        .or(config_hostname);

    match resolved_hostname {
        Some(h) => ManagedLocalInspection {
            ok: true,
            effective_config_path,
            resolved_hostname: Some(h),
            error: None,
        },
        None => ManagedLocalInspection {
            ok: false,
            effective_config_path,
            resolved_hostname: None,
            error: Some(
                "Managed local tunnel hostname is required (set --hostname or include ingress hostname in config)."
                    .to_string(),
            ),
        },
    }
}

fn default_cloudflared_config_path() -> PathBuf {
    crate::git::paths::home_dir().join(".cloudflared").join("config.yml")
}

fn assert_readable_file(path: &str, label: &str) -> Result<(), String> {
    let metadata = std::fs::metadata(path).map_err(|_| {
        format!(
            "{} file was not found. Select a valid cloudflared config file.",
            label
        )
    })?;

    if !metadata.is_file() {
        return Err(format!(
            "{} path is not a file. Select a cloudflared config file.",
            label
        ));
    }

    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{}", e.to_lowercase()))
        .unwrap_or_default();

    if !MANAGED_LOCAL_CONFIG_ALLOWED_EXTENSIONS.contains(ext.as_str()) {
        return Err(format!(
            "{} must be a .yml, .yaml, or .json file.",
            label
        ));
    }

    if metadata.len() == 0 {
        return Err(format!("{} file is empty.", label));
    }

    if metadata.len() > MANAGED_LOCAL_CONFIG_MAX_BYTES {
        return Err(format!(
            "{} file is too large (max {} bytes).",
            label, MANAGED_LOCAL_CONFIG_MAX_BYTES
        ));
    }

    // 可读性检查 (在 Unix 上 metadata 成功已隐含可访问)
    Ok(())
}

/// 从 cloudflared YAML/JSON config 提取 ingress hostname。
fn extract_hostname_from_config(path: &str) -> Result<Option<String>, String> {
    let raw = std::fs::read_to_string(path).map_err(|_| {
        "Could not read the managed local tunnel config file. Check that the file exists and is accessible.".to_string()
    })?;

    // 解析 YAML 或 JSON (serde_yaml 能解析两者)
    let parsed: Value = serde_yaml::from_str(&raw).map_err(|_| {
        "Managed local tunnel config is invalid. Use a valid cloudflared YAML/JSON config file.".to_string()
    })?;

    let ingress = parsed.get("ingress").and_then(|v| v.as_array());
    if let Some(rules) = ingress {
        for rule in rules {
            if let Some(hostname) = rule
                .get("hostname")
                .and_then(|v| v.as_str())
                .and_then(normalize_managed_remote_tunnel_hostname_str)
            {
                return Ok(Some(hostname));
            }
        }
    }

    Ok(None)
}

fn is_ready_log_line(line: &str) -> bool {
    READY_LOG_PATTERNS.iter().any(|p| p.is_match(line))
}

fn is_fatal_log_line(line: &str) -> bool {
    FATAL_LOG_PATTERNS.iter().any(|p| p.is_match(line))
}

/// 启动隧道 (按 mode dispatch)。
pub async fn start(
    request: &NormalizedTunnelStartRequest,
    context: &StartContext,
) -> Result<TunnelController, TunnelServiceError> {
    match request.mode.as_str() {
        TUNNEL_MODE_MANAGED_REMOTE => {
            start_managed_remote(&request.token, &request.hostname).await
        }
        TUNNEL_MODE_MANAGED_LOCAL => {
            let config_path = match &request.config_path {
                Some(Some(p)) => p.to_string_lossy().to_string(),
                _ => String::new(),
            };
            start_managed_local(&config_path, &request.hostname).await
        }
        _ => {
            // quick mode
            let origin_url = context
                .origin_url
                .as_deref()
                .ok_or_else(|| TunnelServiceError::validation("originUrl is required for quick tunnel mode"))?;
            start_quick(origin_url).await
        }
    }
}

/// Quick tunnel: `cloudflared tunnel --url <origin>`。
async fn start_quick(origin_url: &str) -> Result<TunnelController, TunnelServiceError> {
    let (command, env) = resolve_executable_launch_target("cloudflared")
        .ok_or_else(|| TunnelServiceError::missing_dependency("cloudflared is not installed"))?;

    // 创建临时 HOME 目录
    let temp_dir = std::env::temp_dir().join(format!(
        "gridforge-cf-{}",
        chrono::Utc::now().timestamp_millis()
    ));
    std::fs::create_dir_all(&temp_dir).map_err(|e| {
        TunnelServiceError::startup_failed(format!("Failed to create temp dir: {}", e))
    })?;

    let mut env_with_home = env.clone();
    env_with_home.insert("HOME".to_string(), temp_dir.to_string_lossy().to_string());
    env_with_home.insert("CF_TELEMETRY_DISABLE".to_string(), "1".to_string());

    let mut cmd = build_spawn_command(&command, &["tunnel", "--url", origin_url], &env_with_home);
    let mut child = cmd.spawn().map_err(|e| {
        let _ = std::fs::remove_dir_all(&temp_dir);
        TunnelServiceError::startup_failed(format!("Failed to spawn cloudflared: {}", e))
    })?;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    // 扫描 stdout+stderr 提取 trycloudflare URL, 30s 超时
    let public_url_result: Result<String, String> = {
        let url_found = std::sync::Arc::new(tokio::sync::Mutex::new(None::<String>));

        let mut tasks = vec![];

        if let Some(stdout) = stdout {
            let url_found = url_found.clone();
            tasks.push(tokio::spawn(async move {
                let mut reader = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    if let Some(m) = TRY_CF_URL_REGEX.find(&line) {
                        *url_found.lock().await = Some(m.as_str().to_string());
                        return;
                    }
                }
            }));
        }

        if let Some(stderr) = stderr {
            let url_found = url_found.clone();
            tasks.push(tokio::spawn(async move {
                let mut reader = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    if let Some(m) = TRY_CF_URL_REGEX.find(&line) {
                        *url_found.lock().await = Some(m.as_str().to_string());
                        return;
                    }
                }
            }));
        }

        // 等待任一 task 找到 URL 或超时
        let timeout_result = timeout(
            Duration::from_millis(DEFAULT_STARTUP_TIMEOUT_MS),
            async {
                loop {
                    if let Some(ref url) = *url_found.lock().await {
                        return Ok(url.clone());
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
        )
        .await;

        for task in tasks {
            task.abort();
        }

        match timeout_result {
            Ok(Ok(url)) => Ok(url),
            Ok(Err(e)) => Err(e),
            Err(_) => Err("Tunnel URL not received within 30 seconds".to_string()),
        }
    };

    let public_url = match public_url_result {
        Ok(url) => url,
        Err(e) => {
            let _ = child.start_kill();
            let _ = std::fs::remove_dir_all(&temp_dir);
            return Err(TunnelServiceError::startup_failed(e));
        }
    };

    let temp_dir_clone = temp_dir.clone();
    Ok(TunnelController::new(TUNNEL_MODE_QUICK)
        .with_public_url(public_url)
        .with_child(child)
        .with_cleanup(move || {
            let _ = std::fs::remove_dir_all(&temp_dir_clone);
        }))
}

/// Managed remote tunnel: `cloudflared tunnel run --token-file <path>`。
async fn start_managed_remote(
    token: &str,
    hostname: &str,
) -> Result<TunnelController, TunnelServiceError> {
    let normalized_token = token.trim();
    let normalized_host = hostname.trim().to_lowercase();

    if normalized_token.is_empty() {
        return Err(TunnelServiceError::validation(
            "Managed remote tunnel token is required",
        ));
    }
    if normalized_host.is_empty() {
        return Err(TunnelServiceError::validation(
            "Managed remote tunnel hostname is required",
        ));
    }

    let (command, env) = resolve_executable_launch_target("cloudflared")
        .ok_or_else(|| TunnelServiceError::missing_dependency("cloudflared is not installed"))?;

    // 写 token 到临时文件
    let temp_dir = std::env::temp_dir().join(format!(
        "gridforge-cf-token-{}",
        chrono::Utc::now().timestamp_millis()
    ));
    std::fs::create_dir_all(&temp_dir).map_err(|e| {
        TunnelServiceError::startup_failed(format!("Failed to create temp dir: {}", e))
    })?;
    let token_file = temp_dir.join("token");
    std::fs::write(&token_file, normalized_token).map_err(|e| {
        let _ = std::fs::remove_dir_all(&temp_dir);
        TunnelServiceError::startup_failed(format!("Failed to write token file: {}", e))
    })?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600));
    }

    let token_file_str = token_file.to_string_lossy().to_string();
    let mut cmd = build_spawn_command(
        &command,
        &["tunnel", "run", "--token-file", &token_file_str],
        &env,
    );
    let mut child = cmd.spawn().map_err(|e| {
        let _ = std::fs::remove_dir_all(&temp_dir);
        TunnelServiceError::startup_failed(format!("Failed to spawn cloudflared: {}", e))
    })?;

    let public_url = format!("https://{}", normalized_host);

    // 等待就绪
    if let Err(e) = wait_for_managed_ready(&mut child, "managed-remote tunnel").await {
        let _ = child.start_kill();
        let _ = std::fs::remove_dir_all(&temp_dir);
        return Err(TunnelServiceError::startup_failed(e));
    }

    let temp_dir_clone = temp_dir.clone();
    Ok(TunnelController::new(TUNNEL_MODE_MANAGED_REMOTE)
        .with_public_url(public_url)
        .with_child(child)
        .with_cleanup(move || {
            let _ = std::fs::remove_dir_all(&temp_dir_clone);
        }))
}

/// Managed local tunnel: `cloudflared tunnel [--config <path>] run`。
async fn start_managed_local(
    config_path: &str,
    hostname: &str,
) -> Result<TunnelController, TunnelServiceError> {
    let requested_path = config_path.trim();
    let default_path = default_cloudflared_config_path();
    let effective_config_path = if requested_path.is_empty() {
        default_path.to_string_lossy().to_string()
    } else {
        requested_path.to_string()
    };

    let label = if requested_path.is_empty() {
        "Managed local tunnel default config"
    } else {
        "Managed local tunnel config"
    };
    assert_readable_file(&effective_config_path, label).map_err(TunnelServiceError::validation)?;

    let config_hostname = extract_hostname_from_config(&effective_config_path)
        .map_err(TunnelServiceError::startup_failed)?;

    let resolved_host = normalize_managed_remote_tunnel_hostname_str(hostname).or(config_hostname);
    let resolved_host = resolved_host.ok_or_else(|| {
        TunnelServiceError::validation(
            "Managed local tunnel hostname is required (use --tunnel-hostname or add an ingress hostname to the cloudflared config)",
        )
    })?;

    let (command, env) = resolve_executable_launch_target("cloudflared")
        .ok_or_else(|| TunnelServiceError::missing_dependency("cloudflared is not installed"))?;

    let mut args = vec!["tunnel".to_string()];
    if !requested_path.is_empty() {
        args.push("--config".to_string());
        args.push(effective_config_path.clone());
    }
    args.push("run".to_string());
    let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    let mut cmd = build_spawn_command(&command, &args_ref, &env);
    let mut child = cmd.spawn().map_err(|e| {
        TunnelServiceError::startup_failed(format!("Failed to spawn cloudflared: {}", e))
    })?;

    let public_url = format!("https://{}", resolved_host);

    if let Err(e) = wait_for_managed_ready(&mut child, "managed-local tunnel").await {
        let _ = child.start_kill();
        return Err(TunnelServiceError::startup_failed(e));
    }

    let config_path_buf = PathBuf::from(&effective_config_path);
    Ok(TunnelController::new(TUNNEL_MODE_MANAGED_LOCAL)
        .with_public_url(public_url)
        .with_child(child)
        .with_config_path(config_path_buf)
        .with_resolved_hostname(resolved_host))
}

/// 等待 managed tunnel 就绪: 扫描 log 行匹配 READY/FATAL patterns。
/// 6s fallback (saw output), 20s hard timeout。
async fn wait_for_managed_ready(
    child: &mut tokio::process::Child,
    mode_label: &str,
) -> Result<(), String> {
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let saw_output = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ready_result = std::sync::Arc::new(tokio::sync::Mutex::new(None::<Result<(), String>>));

    let ready_timeout = timeout(
        Duration::from_millis(MANAGED_TUNNEL_STARTUP_TIMEOUT_MS),
        async {
            // 合并 stdout + stderr 扫描
            let mut tasks = vec![];

            if let Some(stdout) = stdout {
                let saw_output = saw_output.clone();
                let ready_result = ready_result.clone();
                let mode_label = mode_label.to_string();
                tasks.push(tokio::spawn(async move {
                    let mut reader = BufReader::new(stdout).lines();
                    while let Ok(Some(line)) = reader.next_line().await {
                        let trimmed = line.trim();
                        if !trimmed.is_empty() {
                            saw_output.store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                        if is_ready_log_line(trimmed) {
                            *ready_result.lock().await = Some(Ok(()));
                            return;
                        }
                        if is_fatal_log_line(trimmed) {
                            *ready_result.lock().await = Some(Err(format!(
                                "Cloudflared failed to start {}: {}",
                                mode_label, trimmed
                            )));
                            return;
                        }
                    }
                }));
            }

            if let Some(stderr) = stderr {
                let saw_output = saw_output.clone();
                let ready_result = ready_result.clone();
                let mode_label = mode_label.to_string();
                tasks.push(tokio::spawn(async move {
                    let mut reader = BufReader::new(stderr).lines();
                    while let Ok(Some(line)) = reader.next_line().await {
                        let trimmed = line.trim();
                        if !trimmed.is_empty() {
                            saw_output.store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                        if is_ready_log_line(trimmed) {
                            *ready_result.lock().await = Some(Ok(()));
                            return;
                        }
                        if is_fatal_log_line(trimmed) {
                            *ready_result.lock().await = Some(Err(format!(
                                "Cloudflared failed to start {}: {}",
                                mode_label, trimmed
                            )));
                            return;
                        }
                    }
                }));
            }

            for task in tasks {
                let _ = task.await;
            }
        },
    )
    .await;

    // 检查结果
    let result = ready_result.lock().await.clone();
    if let Some(r) = result {
        return r;
    }

    // 6s fallback: 如果有 output, 视为就绪
    if saw_output.load(std::sync::atomic::Ordering::Relaxed) {
        // 等待 fallback 时间
        tokio::time::sleep(Duration::from_millis(MANAGED_TUNNEL_LIVENESS_FALLBACK_MS)).await;
        let result = ready_result.lock().await.clone();
        if let Some(r) = result {
            return r;
        }
        if saw_output.load(std::sync::atomic::Ordering::Relaxed) {
            return Ok(());
        }
    }

    match ready_timeout {
        Ok(_) => Err(format!(
            "Timed out waiting for cloudflared to initialize {}. Check your tunnel config and credentials.",
            mode_label
        )),
        Err(_) => Err(format!(
            "Timed out waiting for cloudflared to initialize {}. Check your tunnel config and credentials.",
            mode_label
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ready_log_detection() {
        assert!(is_ready_log_line("Registered tunnel connection"));
        assert!(is_ready_log_line("Starting metrics server"));
        assert!(!is_ready_log_line("some random line"));
    }

    #[test]
    fn fatal_log_detection() {
        assert!(is_fatal_log_line("Error parsing config file"));
        assert!(is_fatal_log_line("Invalid token"));
        assert!(!is_fatal_log_line("Registered tunnel connection"));
    }

    #[test]
    fn token_validation_shape() {
        assert!(!validate_token_shape("").ok);
        assert!(!validate_token_shape("has whitespace").ok);
        assert!(validate_token_shape("validtoken").ok);
    }

    #[test]
    fn try_cf_regex_matches() {
        assert!(TRY_CF_URL_REGEX.is_match("https://abc-123.trycloudflare.com"));
        assert!(!TRY_CF_URL_REGEX.is_match("https://example.com"));
    }

    #[test]
    fn capabilities_has_three_modes() {
        let caps = capabilities();
        let modes = caps.get("modes").and_then(|v| v.as_array()).unwrap();
        assert_eq!(modes.len(), 3);
    }
}
