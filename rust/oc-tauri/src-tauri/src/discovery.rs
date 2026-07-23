//! App Discovery — 复现 Electron main.mjs 的 host probe + pairing。
//!
//! 无 mDNS/Bonjour。Electron 也没有。Discovery 通过 HTTP probe 实现:
//! - `desktop_host_probe`: 两阶段探测 (identity gate → version fetch)
//! - `desktop_hosts_get` / `desktop_hosts_set`: settings.json 的 `desktopHosts` 持久化
//! - Pairing deep-link: 顺序探测候选 URL 的 `/health`，first-wins
//!
//! 复现 Electron:
//! - `probeHostWithTimeout` (main.mjs:871-~930)
//! - `selectPairingCandidateUrl` (main.mjs:1936-1945)
//! - `requestJsonWithTimeout` (main.mjs:1924-1934)

use std::time::Duration;

use serde_json::{json, Value};
use tauri::AppHandle;

use crate::settings::SettingsStore;

/// `desktop_hosts_get` — 返回 `{ hosts, defaultHostId }`
///
/// 读 settings.json `desktopHosts` + `desktopDefaultHostId`。
pub async fn hosts_get(_args: &Value, _app: &AppHandle) -> Result<Value, String> {
    let root = SettingsStore::read();
    let hosts = root.get("desktopHosts").cloned().unwrap_or(json!([]));
    let default_host_id = root
        .get("desktopDefaultHostId")
        .cloned()
        .unwrap_or(Value::Null);
    Ok(json!({ "hosts": hosts, "defaultHostId": default_host_id }))
}

/// `desktop_hosts_set` — args: `{ hosts, defaultHostId }`
///
/// 写 settings.json。
pub async fn hosts_set(args: &Value, _app: &AppHandle) -> Result<Value, String> {
    let hosts = args.get("hosts").cloned().unwrap_or(json!([]));
    let default_host_id = args.get("defaultHostId").cloned().unwrap_or(Value::Null);

    let store = SettingsStore::new();
    store.mutate(|root| {
        root["desktopHosts"] = hosts.clone();
        root["desktopDefaultHostId"] = default_host_id.clone();
        Ok(None)
    })?;

    Ok(json!({ "hosts": hosts, "defaultHostId": default_host_id }))
}

/// `desktop_host_probe` — args: `{ url, token?, expectedServerId? }`
///
/// 两阶段探测:
/// 1. (可选) identity gate: GET /health (unauth) → 比较 serverId
/// 2. version: GET /api/version (auth) → HTTP status → status string
///
/// 返回 `{ status: 'ok'|'auth'|'unreachable'|'wrong-service', latencyMs }`
pub async fn host_probe(args: &Value, _app: &AppHandle) -> Result<Value, String> {
    let url = args
        .get("url")
        .and_then(|v| v.as_str())
        .ok_or("url is required")?;
    let token = args.get("token").and_then(|v| v.as_str());
    let expected_server_id = args.get("expectedServerId").and_then(|v| v.as_str());

    let base_url = url.trim_end_matches('/');

    // Stage 1: identity gate (如果提供了 expectedServerId)
    if let Some(expected_id) = expected_server_id {
        let health_url = format!("{}/health", base_url);
        match fetch_json_with_timeout(&health_url, None, Duration::from_secs(3)).await {
            Ok(json) => {
                let actual_id = json.get("serverId").and_then(|v| v.as_str()).unwrap_or("");
                if actual_id != expected_id {
                    return Ok(json!({ "status": "wrong-service", "latencyMs": null }));
                }
            }
            Err(_) => {
                // 健康检查失败不中止；继续版本探测
            }
        }
    }

    // Stage 2: version probe
    let version_url = version_probe_url(base_url);
    let start = std::time::Instant::now();
    match fetch_status_with_timeout(&version_url, token, Duration::from_secs(10)).await {
        Ok(status_code) => {
            let latency_ms = start.elapsed().as_millis() as u64;
            let status = match status_code {
                200..=299 => "ok",
                401 | 403 => "auth",
                _ => "unreachable",
            };
            Ok(json!({ "status": status, "latencyMs": latency_ms }))
        }
        Err(_) => Ok(json!({ "status": "unreachable", "latencyMs": null })),
    }
}

/// 构造 version probe URL。
///
/// oc-server 的版本端点是 `/api/version` (`routes.rs:86-97`),不是 `/version`。
/// 历史上 Tauri 探测的是 `/version`,与 oc-server 路由不匹配 → 服务端
/// 走 SPA fallback 路径(`/api/*` 显式 NOT_FOUND 之外的剩余 → 走 headless
/// fallback 或 404)→ 返回 200 但不是有效响应 / 直接 404 → 探测超时 →
/// host probe 错误标 `unreachable`, UI 把 Local 误判为不可达。
///
/// `/api/version` 是 `/api/` 前缀,被 `static_files::is_api_path` 视为 API 路径
/// (line 32-35), 不会落到 SPA fallback, 一定由 axum 路由处理 → 返回 JSON。
fn version_probe_url(base_url: &str) -> String {
    format!("{}/api/version", base_url)
}

/// `desktop_install_id_get` — 返回稳定 per-install ID。
///
/// 复现 Electron getOrCreateDesktopInstallId (main.mjs:547-559)。
pub async fn install_id_get(_args: &Value, _app: &AppHandle) -> Result<Value, String> {
    let store = SettingsStore::new();
    let id = store.get_or_create_install_id()?;
    Ok(json!({ "installId": id }))
}

/// `desktop_local_client_token_get` — 返回 local client auth token。
///
/// 从 settings.json 读取 `desktopLocalClientToken`。
/// (在 sidecar 模式下，token 由 web server 生成并写入 settings；
/// Tauri 壳只负责读取并暴露给 UI。)
pub async fn local_client_token_get(_args: &Value, _app: &AppHandle) -> Result<Value, String> {
    let token = SettingsStore::get("desktopLocalClientToken")
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .unwrap_or_default();
    Ok(json!({ "clientToken": token }))
}

/// `desktop_remote_password_login` — args: `{ url, password }`
///
/// 尝试用密码登录远程 host，获取 client token。
/// POST `{url}/api/client-auth/login` with `{ password }`。
///
/// 返回 `{ ok, clientToken?, error? }`
pub async fn remote_password_login(args: &Value, _app: &AppHandle) -> Result<Value, String> {
    let url = args
        .get("url")
        .and_then(|v| v.as_str())
        .ok_or("url is required")?;
    let password = args
        .get("password")
        .and_then(|v| v.as_str())
        .ok_or("password is required")?;

    let login_url = format!("{}/api/client-auth/login", url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?;

    let resp = client
        .post(&login_url)
        .json(&json!({ "password": password }))
        .send()
        .await
        .map_err(|e| e.to_string())?;

    let status = resp.status().as_u16();
    if status == 200 || status == 201 {
        let body: Value = resp.json().await.map_err(|e| e.to_string())?;
        let token = body
            .get("clientToken")
            .or_else(|| body.get("token"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        Ok(json!({ "ok": true, "clientToken": token }))
    } else {
        Ok(json!({ "ok": false, "error": format!("HTTP {}", status) }))
    }
}

/// 顺序探测候选 URL 列表的 `/health`，first-wins。
///
/// 复现 Electron selectPairingCandidateUrl (main.mjs:1936-1945)。
#[allow(dead_code)]
pub async fn select_pairing_candidate(
    candidates: &[Value],
    timeout: Duration,
) -> Option<String> {
    for candidate in candidates {
        let url = candidate.get("url").and_then(|v| v.as_str())?;
        let health_url = format!("{}/health", url.trim_end_matches('/'));
        if fetch_json_with_timeout(&health_url, None, timeout)
            .await
            .is_ok()
        {
            return Some(url.to_string());
        }
    }
    None
}

// --- 内部辅助 ---

/// GET 请求获取 JSON，带超时。
async fn fetch_json_with_timeout(
    url: &str,
    bearer_token: Option<&str>,
    timeout: Duration,
) -> Result<Value, String> {
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|e| e.to_string())?;

    let mut req = client.get(url);
    if let Some(token) = bearer_token {
        req = req.header("Authorization", format!("Bearer {}", token));
    }

    let resp = req.send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    resp.json::<Value>().await.map_err(|e| e.to_string())
}

/// GET 请求仅获取 HTTP status code，带超时。
async fn fetch_status_with_timeout(
    url: &str,
    bearer_token: Option<&str>,
    timeout: Duration,
) -> Result<u16, String> {
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|e| e.to_string())?;

    let mut req = client.get(url);
    if let Some(token) = bearer_token {
        req = req.header("Authorization", format!("Bearer {}", token));
    }

    let resp = req.send().await.map_err(|e| e.to_string())?;
    Ok(resp.status().as_u16())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fetch_json_handles_invalid_url() {
        let result = fetch_json_with_timeout("not-a-url", None, Duration::from_secs(1)).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn fetch_status_handles_invalid_url() {
        let result = fetch_status_with_timeout("not-a-url", None, Duration::from_secs(1)).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn select_pairing_candidate_empty_list() {
        let result = select_pairing_candidate(&[], Duration::from_secs(1)).await;
        assert!(result.is_none());
    }

    /// Regression: Tauri host_probe 必须探测 oc-server 真实存在的端点 `/api/version`
    /// (`rust/oc-server/src/routes.rs:86`), 而不是历史上的 `/version` (oc-server
    /// 未注册该路由)。误探测 `/version` → 请求要么命中 SPA fallback (返回 HTML 200
    /// 但 `fetch_status` 仍会看到 200 然后错误判定 `ok` —— 任何监听 127.0.0.1 的 HTTP
    /// 服务都可能误报), 要么命中 headless fallback (JSON 200) —— 都会让 Local
    /// 实例可达性被错误判定。
    ///
    /// 这里锁死 endpoint 字符串, 防止以后再次错配。`host_probe` 会在调用
    /// `version_probe_url` 之前先 `url.trim_end_matches('/')`, 所以
    /// `version_probe_url` 自身不需要再做去尾斜杠处理 —— 那是 caller 的责任。
    #[test]
    fn version_probe_url_uses_oc_server_endpoint() {
        assert_eq!(
            version_probe_url("http://127.0.0.1:58980"),
            "http://127.0.0.1:58980/api/version"
        );
        assert_eq!(
            version_probe_url("https://example.com:443"),
            "https://example.com:443/api/version"
        );
        // 锁死 path 段 (`/api/version`), 防止有人手抖改回 `/version`。
        assert!(
            version_probe_url("http://127.0.0.1:1").ends_with("/api/version"),
            "version endpoint must be /api/version (oc-server route)"
        );
    }
}
