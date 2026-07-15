//! 状态端点: `/health`, `/api/version`, `/api/system/info`, `/robots.txt`。
//!
//! 对应现有: `packages/web/server/lib/opencode/core-routes.js` line 235-360。
//!
//! JSON 响应形状与 Node 侧逐字段对齐:
//!   - /health: { status, timestamp, openchamberVersion, runtime, compatibility, ... }
//!   - /api/version: { status, openchamberVersion, runtime, startedAt, compatibility }
//!   - /api/system/info: { openchamberVersion, runtime, pid, startedAt }

use std::sync::Arc;

use axum::extract::State;
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
        "openchamberVersion": state.version,
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
        "openchamberVersion": state.version,
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
        "openchamberVersion": state.version,
        "runtime": "rust",
        "pid": std::process::id(),
        "startedAt": state.started_at,
    }))
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
