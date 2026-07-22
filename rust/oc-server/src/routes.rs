//! 状态端点: `/health`, `/api/version`, `/api/system/info`, `/robots.txt`。
//!
//! 对应现有: `packages/web/server/lib/opencode/core-routes.js` line 235-360。
//!
//! JSON 响应形状与 Node 侧逐字段对齐:
//!   - /health: { status, timestamp, gridforgeVersion, runtime, compatibility, ... }
//!   - /api/version: { status, gridforgeVersion, runtime, startedAt, compatibility }
//!   - /api/system/info: { gridforgeVersion, runtime, pid, startedAt }

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use once_cell::sync::Lazy;
use serde_json::{json, Value};

use crate::state::AppState;

/// 兼容性声明 (对应 `core-routes.js` line 116-127)。
static COMPATIBILITY: Lazy<Value> = Lazy::new(|| {
    json!({
        "apiVersion": 1,
        "minClientApiVersion": 1,
        "capabilities": [
            "api.health.v1",
            "api.runtime-url.v1",
            "api.raw-file.v1",
            "realtime.sse.v1",
            "realtime.websocket.global-events.v1",
            "terminal.websocket.v1",
            "api.fs.v1",
            "api.text.v1",
            "api.git.v1",
            "api.github.v1",
            "api.tunnels.v1",
            "api.ui-auth.v1",
            "api.client-auth.v1",
            "api.notifications.v1",
            "api.permission-auto-accept.v1",
            "api.session-folders.v1",
            "api.magic-prompts.v1",
            "api.small-model.v1",
            "api.session-assist.v1",
            "api.session-goal.v1",
            "api.quota.v1",
            "api.tts.v1",
            "api.scheduled-tasks.v1",
            "api.skills-catalog.v1",
            "api.preview.v1",
            "api.dictation.v1",
            "api.relay.v1",
        ]
    })
});

/// `GET /health` — 健康检查。
///
/// 对应 `core-routes.js` line 235。
pub async fn health(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let mut body = json!({
        "status": "ok",
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "gridforgeVersion": state.version,
        "runtime": "rust",
        "compatibility": COMPATIBILITY.clone(),
    });

    // serverId: 阶段 1 不实现 (无签名密钥), omit 字段 (同 Node 侧 falsy 时 omit)。
    // 后续阶段实现 ui-auth 时添加。

    // health snapshot: OpenCode 状态
    if state.opencode_ready.load(std::sync::atomic::Ordering::Relaxed) {
        body["openCodeRunning"] = json!(true);
        body["isOpenCodeReady"] = json!(true);
    } else {
        body["openCodeRunning"] = json!(false);
        body["isOpenCodeReady"] = json!(false);
    }
    body["apiOnly"] = json!(state.config.api_only);

    Json(body)
}

/// `GET /api/version` — 版本信息。
///
/// 对应 `core-routes.js` line 248。
pub async fn version(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(json!({
        "status": "ok",
        "gridforgeVersion": state.version,
        "runtime": "rust",
        "startedAt": state.started_at,
        "compatibility": COMPATIBILITY.clone(),
    }))
}

/// `GET /api/system/info` — 系统信息。
///
/// 对应 `core-routes.js` line 353。
pub async fn system_info(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(json!({
        "gridforgeVersion": state.version,
        "runtime": "rust",
        "pid": std::process::id(),
        "startedAt": state.started_at,
    }))
}

/// `GET /api/opencode/health` — OpenCode 健康检查代理。
///
/// 对应 `routes.js` line 251。前端 `opencodeClient.checkHealth()` 调用此端点。
pub async fn opencode_health(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let base_url = state.opencode_base_url.trim_end_matches('/');
    let health_url = format!("{}/global/health", base_url);
    let client = reqwest::Client::new();

    match client
        .get(&health_url)
        .header("Accept", "application/json")
        .header(reqwest::header::AUTHORIZATION, &state.opencode_auth_header)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
    {
        Ok(resp) => {
            let status = resp.status();
            match resp.json::<Value>().await {
                Ok(health) => {
                    let healthy = health.get("healthy").and_then(|v| v.as_bool()).unwrap_or(false);
                    if status.is_success() {
                        (StatusCode::OK, Json(json!({ "healthy": healthy }))).into_response()
                    } else {
                        let error = health
                            .get("error")
                            .and_then(|v| v.as_str())
                            .unwrap_or(status.canonical_reason().unwrap_or("OpenCode health check failed"));
                        (status, Json(json!({ "healthy": false, "error": error }))).into_response()
                    }
                }
                Err(e) => {
                    (StatusCode::OK, Json(json!({ "healthy": false, "error": e.status().map_or_else(|| "OpenCode health check failed".to_string(), |s| s.to_string()) }))).into_response()
                }
            }
        }
        Err(e) => {
            (StatusCode::SERVICE_UNAVAILABLE, Json(json!({
                "healthy": false,
                "error": e.to_string(),
            })))
                .into_response()
        }
    }
}

/// `GET /robots.txt` — 禁止爬虫。
///
/// 对应 `server/index.js` line 1323。
pub async fn robots_txt() -> impl IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        "User-agent: *\nDisallow: /\n",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use clap::Parser;

    fn test_state() -> Arc<AppState> {
        let config = Config::try_parse_from(["oc-server"]).unwrap();
        Arc::new(AppState::new(
            config,
            "http://127.0.0.1:4096".to_string(),
            "Basic test".to_string(),
        ))
    }

    #[tokio::test]
    async fn health_returns_ok() {
        let state = test_state();
        let response = health(State(state)).await.into_response();
        assert_eq!(response.status(), 200);
    }

    #[tokio::test]
    async fn version_returns_ok() {
        let state = test_state();
        let response = version(State(state)).await.into_response();
        assert_eq!(response.status(), 200);
    }

    #[tokio::test]
    async fn system_info_returns_ok() {
        let state = test_state();
        let response = system_info(State(state)).await.into_response();
        assert_eq!(response.status(), 200);
    }

    #[test]
    fn compatibility_has_correct_capabilities() {
        assert_eq!(COMPATIBILITY["apiVersion"], 1);
        let caps = COMPATIBILITY["capabilities"].as_array().unwrap();
        assert!(caps.len() >= 6);
        assert!(caps.iter().any(|v| v == "api.health.v1"));
    }
}
