//! ngrok 子进程管理。
//!
//! 移植自 `packages/web/server/lib/ngrok-tunnel.js` (343 行)。
//!
//! 单一模式: quick (`ngrok http --log=stdout --log-format=json 127.0.0.1:<port>`)
//! URL 提取: stdout JSON log 解析 + 250ms 轮询 localhost:4040 API

use std::time::Duration;

use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::time::timeout;

use crate::tunnels::executable_search::{create_executable_search_env, resolve_executable_launch_target};
use crate::tunnels::install_help::get_tunnel_dependency_install_info;
use crate::tunnels::providers::{build_spawn_command, StartContext, TunnelController};
use crate::tunnels::types::{TunnelServiceError, NormalizedTunnelStartRequest, TUNNEL_MODE_QUICK, TUNNEL_PROVIDER_NGROK};

const DEFAULT_STARTUP_TIMEOUT_MS: u64 = 30000;
const NGROK_API_URL: &str = "http://127.0.0.1:4040/api/tunnels";

static NGROK_PUBLIC_URL_REGEX: Lazy<Regex> =
    Lazy::new(|| Regex::new("https://[^\\s\"']+").unwrap());

/// ngrok provider capabilities。
pub fn capabilities() -> Value {
    json!({
        "provider": TUNNEL_PROVIDER_NGROK,
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
                "stability": "beta",
            },
        ],
    })
}

/// 检查 ngrok 可用性。
pub async fn check_availability() -> Value {
    let install_info = get_tunnel_dependency_install_info(TUNNEL_PROVIDER_NGROK);

    match resolve_executable_launch_target("ngrok") {
        Some((command, env)) => {
            let output = tokio::process::Command::new(&command)
                .arg("version")
                .envs(&env)
                .output()
                .await;

            match output {
                Ok(out) if out.status.success() => {
                    let version = String::from_utf8_lossy(&out.stdout).trim().to_string();
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

/// 检查 ngrok authtoken 是否已配置。
pub async fn check_authtoken_configured(ngrok_path: Option<&str>) -> (bool, String) {
    // 1. 检查 env
    if let Ok(token) = std::env::var("NGROK_AUTHTOKEN") {
        if !token.trim().is_empty() {
            return (true, "NGROK_AUTHTOKEN is set.".to_string());
        }
    }

    // 2. spawn `ngrok config check`
    let target = match ngrok_path {
        Some(path) => Some((path.to_string(), create_executable_search_env())),
        None => resolve_executable_launch_target("ngrok"),
    };

    let (command, env) = match target {
        Some(t) => t,
        None => {
            return (
                false,
                get_tunnel_dependency_install_info(TUNNEL_PROVIDER_NGROK).message,
            );
        }
    };

    let output = tokio::process::Command::new(&command)
        .arg("config")
        .arg("check")
        .envs(&env)
        .output()
        .await;

    match output {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout).to_string();
            let stderr = String::from_utf8_lossy(&out.stderr).to_string();
            let combined = format!("{}{}", stdout, stderr);
            let trimmed = combined.trim().to_string();

            if out.status.success() {
                (
                    true,
                    if trimmed.is_empty() {
                        "ngrok config is valid.".to_string()
                    } else {
                        trimmed
                    },
                )
            } else {
                (
                    false,
                    if trimmed.is_empty() {
                        "Run: ngrok config add-authtoken <your-ngrok-token>".to_string()
                    } else {
                        trimmed
                    },
                )
            }
        }
        Err(e) => (false, e.to_string()),
    }
}

/// 检查 ngrok API 可达性。
pub async fn check_api_reachability() -> (bool, Option<u16>, Option<String>) {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(e) => return (false, None, Some(e.to_string())),
    };

    match client.get("https://api.ngrok.com/").send().await {
        Ok(resp) => (true, Some(resp.status().as_u16()), None),
        Err(e) => (false, None, Some(e.to_string())),
    }
}

/// 诊断。
pub async fn diagnose(_request: &Value) -> Value {
    let dependency = check_availability().await;
    let dep_path = dependency.get("path").and_then(|v| v.as_str()).map(|s| s.to_string());
    let authtoken = check_authtoken_configured(dep_path.as_deref()).await;
    let (network_reachable, network_status, network_error) = check_api_reachability().await;
    let install_info = get_tunnel_dependency_install_info(TUNNEL_PROVIDER_NGROK);

    let dependency_available = dependency.get("available").and_then(|v| v.as_bool()).unwrap_or(false);
    let startup_ready = dependency_available && authtoken.0 && network_reachable;

    json!({
        "providerChecks": [
            {
                "id": "dependency",
                "label": "ngrok installed",
                "status": if dependency_available { "pass" } else { "fail" },
                "detail": if dependency_available {
                    dependency.get("version").and_then(|v| v.as_str())
                        .or_else(|| dependency.get("path").and_then(|v| v.as_str()))
                        .unwrap_or("ngrok available")
                } else {
                    &install_info.message
                },
            },
            {
                "id": "authtoken",
                "label": "ngrok authtoken configured",
                "status": if authtoken.0 { "pass" } else { "fail" },
                "detail": if authtoken.0 {
                    &authtoken.1
                } else {
                    authtoken.1.as_str()
                },
            },
            {
                "id": "network",
                "label": "ngrok API reachable",
                "status": if network_reachable { "pass" } else { "fail" },
                "detail": if network_reachable {
                    network_status.map(|s| format!("HTTP {}", s)).unwrap_or_else(|| "Reachable".to_string())
                } else {
                    network_error.as_deref().unwrap_or("Could not reach api.ngrok.com").to_string()
                },
            },
        ],
        "modes": [
            {
                "mode": TUNNEL_MODE_QUICK,
                "checks": [
                    {
                        "id": "startup_readiness",
                        "label": "Provider startup readiness",
                        "status": if startup_ready { "pass" } else { "fail" },
                        "detail": if startup_ready {
                            "Provider dependency, auth, and network checks passed."
                        } else {
                            "Resolve provider checks before starting tunnels."
                        },
                    },
                ],
                "summary": {
                    "ready": startup_ready,
                    "failures": if startup_ready { 0 } else { 1 },
                    "warnings": 0,
                },
                "ready": startup_ready,
                "blockers": if startup_ready {
                    json!([])
                } else {
                    json!(["Resolve provider checks before starting tunnels."])
                },
            },
        ],
    })
}

/// 启动 ngrok quick tunnel。
pub async fn start(
    request: &NormalizedTunnelStartRequest,
    context: &StartContext,
) -> Result<TunnelController, TunnelServiceError> {
    if request.mode != TUNNEL_MODE_QUICK {
        return Err(TunnelServiceError::new(
            "mode_unsupported",
            format!("Ngrok only supports '{}' mode right now", TUNNEL_MODE_QUICK),
        ));
    }

    let active_port = context.active_port.ok_or_else(|| {
        TunnelServiceError::validation("A local port is required to start an ngrok tunnel")
    })?;

    start_quick(active_port).await
}

/// 从文本提取 ngrok public URL (JSON log 或正则)。
pub fn extract_ngrok_public_url_from_text(text: &str) -> Option<String> {
    if text.trim().is_empty() {
        return None;
    }

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // 尝试 JSON 解析
        if let Ok(parsed) = serde_json::from_str::<Value>(trimmed) {
            if let Some(url) = parsed.get("url").and_then(|v| v.as_str()) {
                if let Some(normalized) = normalize_ngrok_public_url(url) {
                    return Some(normalized);
                }
            }
            if let Some(url) = parsed.get("public_url").and_then(|v| v.as_str()) {
                if let Some(normalized) = normalize_ngrok_public_url(url) {
                    return Some(normalized);
                }
            }
        }

        // 正则匹配
        if let Some(m) = NGROK_PUBLIC_URL_REGEX.find(trimmed) {
            if let Some(normalized) = normalize_ngrok_public_url(m.as_str()) {
                return Some(normalized);
            }
        }
    }

    None
}

/// 归一化 ngrok URL: 必须 https + hostname 包含 ngrok。
fn normalize_ngrok_public_url(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }

    // 简化 URL 解析
    let after_scheme = match trimmed.find("://") {
        Some(pos) => &trimmed[pos + 3..],
        None => return None,
    };

    // 检查 scheme 是 https
    if !trimmed.starts_with("https://") {
        return None;
    }

    let host_part = after_scheme.split(['/', '?', '#']).next().unwrap_or("");
    let hostname = host_part.rsplit('@').next().unwrap_or("");
    let hostname = match hostname.rfind(':') {
        Some(pos) => &hostname[..pos],
        None => hostname,
    };

    if hostname.contains("ngrok") {
        Some(trimmed.trim_end_matches('/').to_string())
    } else {
        None
    }
}

/// 汇总 ngrok 输出 (提取错误信息)。
pub fn summarize_ngrok_output(lines: &[String]) -> String {
    let non_empty: Vec<&String> = lines.iter().filter(|l| !l.is_empty()).collect();
    if non_empty.is_empty() {
        return String::new();
    }

    // 从后往前找 error level JSON
    for line in non_empty.iter().rev() {
        if let Ok(parsed) = serde_json::from_str::<Value>(line) {
            let level = parsed.get("lvl").and_then(|v| v.as_str()).unwrap_or("");
            let level_lower = level.to_lowercase();
            if level_lower != "eror" && level_lower != "error" && level_lower != "crit" {
                continue;
            }
            if let Some(err) = parsed.get("err").and_then(|v| v.as_str()) {
                let normalized = normalize_diagnostic_text(err);
                if !normalized.is_empty() && normalized != "<nil>" {
                    return normalized;
                }
            }
        }
    }

    // 从后往前找任意 err/msg
    for line in non_empty.iter().rev() {
        if let Ok(parsed) = serde_json::from_str::<Value>(line) {
            if let Some(err) = parsed.get("err").and_then(|v| v.as_str()) {
                let normalized = normalize_diagnostic_text(err);
                if !normalized.is_empty() && normalized != "<nil>" && !normalized.to_lowercase().contains("context canceled") {
                    return normalized;
                }
            }
            if let Some(msg) = parsed.get("msg").and_then(|v| v.as_str()) {
                let normalized = normalize_diagnostic_text(msg);
                if normalized.to_lowercase().contains("failed")
                    || normalized.to_lowercase().contains("error")
                    || normalized.to_lowercase().contains("invalid")
                    || normalized.to_lowercase().contains("auth")
                {
                    return normalized;
                }
            }
        }
    }

    // ERROR: 前缀行
    let error_lines: Vec<String> = non_empty
        .iter()
        .filter(|l| l.to_lowercase().starts_with("error:"))
        .map(|l| normalize_diagnostic_text(&l.to_lowercase().replacen("error:", "", 1)))
        .filter(|s| !s.is_empty())
        .collect();
    if !error_lines.is_empty() {
        return error_lines.iter().take(4).cloned().collect::<Vec<_>>().join(" ");
    }

    // 最后一行
    if let Some(last) = non_empty.last() {
        if let Ok(parsed) = serde_json::from_str::<Value>(last) {
            if let Some(err) = parsed.get("err").and_then(|v| v.as_str()) {
                if !err.trim().is_empty() {
                    return normalize_diagnostic_text(err);
                }
            }
            if let Some(msg) = parsed.get("msg").and_then(|v| v.as_str()) {
                if !msg.trim().is_empty() {
                    return normalize_diagnostic_text(msg);
                }
            }
        }
        return normalize_diagnostic_text(last);
    }

    String::new()
}

fn normalize_diagnostic_text(value: &str) -> String {
    value
        .replace('\r', "")
        .split('\n')
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// 启动 ngrok quick tunnel。
async fn start_quick(port: u16) -> Result<TunnelController, TunnelServiceError> {
    let ngrok_check = check_availability().await;
    if !ngrok_check
        .get("available")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return Err(TunnelServiceError::missing_dependency(
            get_tunnel_dependency_install_info(TUNNEL_PROVIDER_NGROK).message,
        ));
    }

    let ngrok_path = ngrok_check
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or("ngrok");
    let authtoken_check = check_authtoken_configured(Some(ngrok_path)).await;
    if !authtoken_check.0 {
        return Err(TunnelServiceError::startup_failed(format!(
            "ngrok authtoken is not configured. {}",
            authtoken_check.1
        )));
    }

    let target_str = format!("127.0.0.1:{}", port);
    let env = create_executable_search_env();

    let mut cmd = build_spawn_command(
        ngrok_path,
        &["http", "--log=stdout", "--log-format=json", &target_str],
        &env,
    );
    let mut child = cmd.spawn().map_err(|e| {
        TunnelServiceError::startup_failed(format!("Ngrok failed to start: {}", e))
    })?;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    // 双通道提取 URL: stdout JSON + API 轮询
    let url_found = std::sync::Arc::new(tokio::sync::Mutex::new(None::<String>));
    let recent_output = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::<String>::new()));

    let mut tasks = vec![];

    // stdout 扫描
    if let Some(stdout) = stdout {
        let url_found = url_found.clone();
        let recent_output = recent_output.clone();
        tasks.push(tokio::spawn(async move {
            let mut reader = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                if let Some(url) = extract_ngrok_public_url_from_text(&line) {
                    *url_found.lock().await = Some(url);
                }
                let trimmed = line.trim().to_string();
                if !trimmed.is_empty() {
                    let mut output = recent_output.lock().await;
                    output.push(trimmed);
                    if output.len() > 200 {
                        output.remove(0);
                    }
                }
            }
        }));
    }

    // stderr 扫描
    if let Some(stderr) = stderr {
        let url_found = url_found.clone();
        let recent_output = recent_output.clone();
        tasks.push(tokio::spawn(async move {
            let mut reader = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                if let Some(url) = extract_ngrok_public_url_from_text(&line) {
                    *url_found.lock().await = Some(url);
                }
                let trimmed = line.trim().to_string();
                if !trimmed.is_empty() {
                    let mut output = recent_output.lock().await;
                    output.push(trimmed);
                    if output.len() > 200 {
                        output.remove(0);
                    }
                }
            }
        }));
    }

    // API 轮询 + 超时
    let url_found_poll = url_found.clone();
    let poll_task = tokio::spawn(async move {
        loop {
            if url_found_poll.lock().await.is_some() {
                return;
            }
            if let Some(url) = fetch_ngrok_public_url().await {
                *url_found_poll.lock().await = Some(url);
                return;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    });

    let timeout_result: Result<Option<String>, _> = timeout(
        Duration::from_millis(DEFAULT_STARTUP_TIMEOUT_MS),
        async {
            loop {
                if let Some(ref url) = *url_found.lock().await {
                    return Some(url.clone());
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        },
    )
    .await;

    // 清理 tasks
    for task in &mut tasks {
        task.abort();
    }
    poll_task.abort();

    let public_url = match timeout_result {
        Ok(Some(url)) => url,
        Ok(None) => {
            let _ = child.start_kill();
            let output = recent_output.lock().await.clone();
            let summary = summarize_ngrok_output(&output);
            let message = if summary.is_empty() {
                "Ngrok tunnel URL not received".to_string()
            } else {
                format!("Ngrok tunnel URL not received: {}", summary)
            };
            return Err(TunnelServiceError::startup_failed(message));
        }
        Err(_) => {
            let _ = child.start_kill();
            let output = recent_output.lock().await.clone();
            let summary = summarize_ngrok_output(&output);
            let message = if summary.is_empty() {
                "Ngrok tunnel URL not received within 30 seconds".to_string()
            } else {
                format!("Ngrok tunnel URL not received within 30 seconds: {}", summary)
            };
            return Err(TunnelServiceError::startup_failed(message));
        }
    };

    Ok(TunnelController::new(TUNNEL_MODE_QUICK)
        .with_public_url(public_url)
        .with_child(child))
}

/// 从 ngrok API 获取 public URL。
async fn fetch_ngrok_public_url() -> Option<String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .ok()?;

    let resp = client.get(NGROK_API_URL).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }

    let payload: Value = resp.json().await.ok()?;
    let tunnels = payload.get("tunnels").and_then(|v| v.as_array())?;

    // 优先找 https tunnel
    for tunnel in tunnels {
        if tunnel.get("proto").and_then(|v| v.as_str()) == Some("https") {
            if let Some(url) = tunnel
                .get("public_url")
                .and_then(|v| v.as_str())
                .and_then(normalize_ngrok_public_url)
            {
                return Some(url);
            }
        }
    }

    // fallback: 任意有效 tunnel
    for tunnel in tunnels {
        if let Some(url) = tunnel
            .get("public_url")
            .and_then(|v| v.as_str())
            .and_then(normalize_ngrok_public_url)
        {
            return Some(url);
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_url_from_json() {
        let text = r#"{"lvl":"info","msg":"started tunnel","url":"https://abc.ngrok.io"}"#;
        assert_eq!(
            extract_ngrok_public_url_from_text(text),
            Some("https://abc.ngrok.io".to_string())
        );
    }

    #[test]
    fn extract_url_from_regex() {
        let text = "Tunnel online at https://xyz.ngrok-free.app";
        assert!(extract_ngrok_public_url_from_text(text).is_some());
    }

    #[test]
    fn extract_url_rejects_non_ngrok() {
        let text = r#"{"url":"https://example.com"}"#;
        assert!(extract_ngrok_public_url_from_text(text).is_none());
    }

    #[test]
    fn extract_url_rejects_http() {
        let text = r#"{"url":"http://abc.ngrok.io"}"#;
        assert!(extract_ngrok_public_url_from_text(text).is_none());
    }

    #[test]
    fn normalize_ngrok_url_strips_trailing_slash() {
        assert_eq!(
            normalize_ngrok_public_url("https://abc.ngrok.io/"),
            Some("https://abc.ngrok.io".to_string())
        );
    }

    #[test]
    fn summarize_empty() {
        assert_eq!(summarize_ngrok_output(&[]), "");
    }

    #[test]
    fn summarize_error_level() {
        let lines = vec![
            r#"{"lvl":"info","msg":"starting"}"#.to_string(),
            r#"{"lvl":"eror","err":"auth failed"}"#.to_string(),
        ];
        assert_eq!(summarize_ngrok_output(&lines), "auth failed");
    }

    #[test]
    fn capabilities_has_one_mode() {
        let caps = capabilities();
        let modes = caps.get("modes").and_then(|v| v.as_array()).unwrap();
        assert_eq!(modes.len(), 1);
    }
}
