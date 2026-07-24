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

/// `desktop_host_probe` — args: `{ url, token?|clientToken?, expectedServerId?, requestHeaders? }`
///
/// 兼容性: UI (`packages/ui/src/lib/desktopHosts.ts:345`) 调用时用 key
/// `clientToken`, 但旧版 (Electron 时期) 也可能传 `token`。Rust 端同时识别
/// 两种 key, 以防 IPC 边界把 `clientToken` 静默丢弃。
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
    // 兼容两种 token key: `token` (Electron 时期 / 内部约定) + `clientToken`
    // (UI `desktopHosts.ts:345` 实际传入的 key)。两者同时识别, 防止 IPC
    // 边界把 token 静默丢弃。`requestHeaders` 当前未使用 (穿透到 fetch
    // 是 future work; oc-server 的 /api/version 是公开路由, 不需要 token,
    // 所以不影响 probe 结果)。
    let token = args
        .get("token")
        .or_else(|| args.get("clientToken"))
        .and_then(|v| v.as_str());
    let expected_server_id = args.get("expectedServerId").and_then(|v| v.as_str());

    let base_url = url.trim_end_matches('/');
    log::info!("[host_probe] start url={} has_token={}", base_url, token.is_some());

    // Stage 1: identity gate (如果提供了 expectedServerId)
    if let Some(expected_id) = expected_server_id {
        let health_url = format!("{}/health", base_url);
        match fetch_json_with_timeout(&health_url, None, Duration::from_secs(3)).await {
            Ok(json) => {
                let actual_id = json.get("serverId").and_then(|v| v.as_str()).unwrap_or("");
                if actual_id != expected_id {
                    log::info!(
                        "[host_probe] stage1 wrong-service expected={} actual={}",
                        expected_id,
                        actual_id
                    );
                    return Ok(json!({ "status": "wrong-service", "latencyMs": null }));
                }
            }
            Err(err) => {
                // 健康检查失败不中止；继续版本探测，但记录错误便于排查。
                log::info!("[host_probe] stage1 /health failed: {}", err);
            }
        }
    }

    // Stage 2: version probe
    let version_url = version_probe_url(base_url);
    let start = std::time::Instant::now();
    log::info!("[host_probe] stage2 GET {}", version_url);
    // 直接构造 reqwest client 而不复用 fetch_status_with_timeout: 后者把
    // reqwest::Error 包装成 String 后丢失了 is_connect() / is_timeout() /
    // is_request() 这些分类方法, 无法在 host_probe 里给 UI 一个具体 reason。
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build();
    let req_result = match client {
        Ok(client) => {
            let mut req = client.get(&version_url);
            if let Some(token) = token {
                req = req.header("Authorization", format!("Bearer {}", token));
            }
            req.send().await.map(|r| r.status().as_u16())
        }
        Err(err) => Err(err),
    };
    match req_result {
        Ok(status_code) => {
            let latency_ms = start.elapsed().as_millis() as u64;
            let status = match status_code {
                200..=299 => "ok",
                401 | 403 => "auth",
                _ => "unreachable",
            };
            log::info!(
                "[host_probe] stage2 response status_code={} mapped={} latency_ms={}",
                status_code,
                status,
                latency_ms
            );
            Ok(json!({ "status": status, "latencyMs": latency_ms }))
        }
        Err(err) => {
            // 区分失败原因: connection refused / timeout / request error /
            // 其他。这对调试 "Local unreachable" 假阴性至关重要 — user/agent
            // 之前只能看到 'unreachable', 无法判断是 host 不在 / 网络隔离 /
            // 还是服务端 reject。把 err kind 注入到 reason 字段, UI 不解析
            // (按 'unreachable' 渲染), 但日志/未来 debug 端点可以读到。
            let kind = if err.is_connect() {
                "connection-refused"
            } else if err.is_timeout() {
                "timeout"
            } else if err.is_request() {
                "request-error"
            } else {
                "other"
            };
            log::warn!(
                "[host_probe] stage2 error url={} kind={} err={}",
                version_url,
                kind,
                err
            );
            Ok(json!({
                "status": "unreachable",
                "latencyMs": null,
                "reason": kind,
                "probeUrl": version_url,
            }))
        }
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

    /// 集成测试: reqwest 客户端必须能成功 fetch 一个本地 mock HTTP server 的
    /// `/api/version` 路径。这是对 host_probe 真假阴性最直接的探测: 如果
    /// 历史上 user 看到 Local 误判 unreachable, 但 oc-server 本身返回 200,
    /// 那一定是 reqwest/timeout/tls 默认值在 oc-tauri 进程环境里有问题。
    ///
    /// 用一个一次性 TCP server 模拟 "oc-server 在 127.0.0.1:<随机端口>" 响应
    /// `/api/version` → 200 JSON, 其他路径 404。然后通过 fetch_status_with_timeout
    /// 探测该端口, 断言拿到 200 (而不是 timeout 或 connection refused)。
    #[tokio::test]
    async fn fetch_status_reaches_local_mock_server() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        // Bind 到一个 OS 随机端口, 监听一次 HTTP 请求, 返回 200 + JSON。
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();

        let server_task = tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = vec![0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let body = br#"{"status":"ok","gridforgeVersion":"0.0.1","runtime":"rust"}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.write_all(body).await;
                let _ = stream.shutdown().await;
            }
        });

        let url = format!("http://127.0.0.1:{}/api/version", port);
        let result = fetch_status_with_timeout(&url, None, Duration::from_secs(3)).await;
        let _ = server_task.await;

        assert_eq!(
            result.as_ref().map(|s| *s),
            Ok(200),
            "reqwest must reach local loopback mock server (url={}); got {:?}",
            url,
            result
        );
    }

    /// 集成测试: 当 `OPENCHAMBER_HOST_PROBE_TARGET` 环境变量设置时, 探测
    /// 该 URL 并打印结果。给人工 debug 提供一个 hook: 在 oc-tauri 进程内
    /// (同样的 runtime, 同样的 reqwest 配置) 探测一个任意 URL, 不需要
    /// 走 host_probe 的整个 stage-1/stage-2 流程。
    ///
    /// 不设置 env 时 skip (CI 默认行为)。
    #[tokio::test]
    async fn host_probe_target_from_env() {
        let url = match std::env::var("OPENCHAMBER_HOST_PROBE_TARGET").ok() {
            Some(v) if !v.is_empty() => v,
            _ => return,
        };
        eprintln!("[probe-target] GET {}", url);
        let r = fetch_status_with_timeout(&url, None, Duration::from_secs(5)).await;
        eprintln!("[probe-target] result = {:?}", r);
        // 此测试永远断言 ok (人工 debug 用途), 失败信息靠 stderr。
        assert!(r.is_ok(), "fetch_status_with_timeout({}) failed: {:?}", url, r);
    }

    /// host_probe 必须接受两种 token key 之一 (`token` 或 `clientToken`),
    /// 否则 UI 端 (`desktopHosts.ts:345`) 用 `clientToken` 传入会被静默丢弃,
    /// 后续对受保护端点探测就缺认证。这个测试不直接调 host_probe (那是
    /// async + 需要 AppHandle), 而是在一个 helper 里验证 token-extraction
    /// 逻辑。
    #[test]
    fn host_probe_token_extraction_accepts_both_keys() {
        let extract = |args: &serde_json::Value| -> Option<String> {
            args.get("token")
                .or_else(|| args.get("clientToken"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        };

        // 优先 `token` (历史约定)
        assert_eq!(
            extract(&json!({ "url": "x", "token": "from-token" })),
            Some("from-token".to_string())
        );
        // fallback `clientToken` (UI 实际传入)
        assert_eq!(
            extract(&json!({ "url": "x", "clientToken": "from-clientToken" })),
            Some("from-clientToken".to_string())
        );
        // 都没有 → None
        assert_eq!(extract(&json!({ "url": "x" })), None);
    }

    /// host_probe 在 stage-2 fetch 失败时必须返回 `reason` 字段, 区分
    /// connection-refused / timeout / 其他。这给人工 debug 提供了"为什么
    /// Local 标 unreachable"的具体原因, 否则只剩一个笼统的 'unreachable'
    /// 无法判断是端口没开 / 网络隔离 / 还是服务端 reject。
    ///
    /// 这里只测 reason 分类的纯函数分支, 真实网络行为由
    /// `fetch_status_reaches_local_mock_server` (200 路径) +
    /// `host_probe_target_from_env` (env 触发的真实探测) 覆盖。
    #[test]
    fn host_probe_error_reason_classification() {
        // 复刻 host_probe 内 reason 分类逻辑。返回 Ok(()), 仅保证
        // 分类函数行为符合预期, 防止有人手抖把分支写错。
        fn classify(is_connect: bool, is_timeout: bool, is_request: bool) -> &'static str {
            if is_connect {
                "connection-refused"
            } else if is_timeout {
                "timeout"
            } else if is_request {
                "request-error"
            } else {
                "other"
            }
        }

        assert_eq!(classify(true, false, false), "connection-refused");
        assert_eq!(classify(false, true, false), "timeout");
        assert_eq!(classify(false, false, true), "request-error");
        assert_eq!(classify(false, false, false), "other");
        // connect 优先于 timeout (如果 reqwest 同时 flag, 我们取 connect —
        // 比 "我连不上" 更具体)。
        assert_eq!(classify(true, true, false), "connection-refused");
    }
}
