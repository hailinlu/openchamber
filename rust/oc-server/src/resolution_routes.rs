//! `/api/config/opencode-resolution` 路由 — OpenCode 安装源检测。
//!
//! 对应 Node `packages/web/server/lib/opencode/routes.js:145` 的
//! `GET /api/config/opencode-resolution`。

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::State;
use axum::response::IntoResponse;
use axum::Json;
use serde_json::{json, Value};

use crate::config::Config;
use crate::state::AppState;

/// GET /api/config/opencode-resolution — 返回 OpenCode binary 的解析快照。
///
/// 简化实现: 不包含 Node 端完整的 binary 探测链 (which/whence/env 检测),
/// 但返回 oc-server 实际用于启动 OpenCode 的信息, 足以让 UI 判断
/// OpenCode 是否可用及其来源。
pub async fn get_opencode_resolution(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let settings = read_settings().await.unwrap_or_default();

    let configured = settings
        .get("opencodeBinary")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    let resolved = state.config.opencode_binary.clone();
    let resolved_dir = PathBuf::from(&resolved)
        .parent()
        .map(|p| p.to_string_lossy().to_string());

    let source = determine_source(&state.config, configured.as_deref());

    // Node/bun binary 从环境变量探测
    let node = std::env::var("NODE").ok()
        .or_else(|| which("node").map(|p| p.to_string_lossy().to_string()));
    let bun = std::env::var("BUN").ok()
        .or_else(|| which("bun").map(|p| p.to_string_lossy().to_string()));

    Json(json!({
        "configured": configured,
        "resolved": resolved,
        "resolvedDir": resolved_dir,
        "source": source,
        "detectedNow": null,
        "detectedSourceNow": null,
        "launchBinary": null,
        "launchArgs": [],
        "launchWrapperType": null,
        "viaWsl": false,
        "wslBinary": null,
        "wslPath": null,
        "wslDistro": null,
        "node": node,
        "bun": bun,
    }))
}

/// 判断 OpenCode 来源类型。
fn determine_source(config: &Config, configured: Option<&str>) -> &'static str {
    if config.is_external_opencode() {
        return "external";
    }
    if configured.is_some() {
        return "settings";
    }
    // 检查是否覆写了默认值 (默认是 "opencode")
    if config.opencode_binary != "opencode" {
        return "settings";
    }
    "env"
}

/// 读取 settings.json。
async fn read_settings() -> Option<Value> {
    let path = resolve_settings_path();
    match tokio::fs::read_to_string(&path).await {
        Ok(content) => serde_json::from_str(&content).ok(),
        Err(_) => None,
    }
}

fn resolve_settings_path() -> PathBuf {
    if let Ok(dir) = std::env::var("OPENCHAMBER_DATA_DIR") {
        PathBuf::from(dir).join("settings.json")
    } else {
        #[cfg(not(target_os = "windows"))]
        {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
            PathBuf::from(home).join(".config/openchamber/settings.json")
        }
        #[cfg(target_os = "windows")]
        {
            let home = std::env::var("USERPROFILE").unwrap_or_else(|_| "C:\\".to_string());
            PathBuf::from(home).join(".config/openchamber/settings.json")
        }
    }
}

/// 简单的 which 实现 — 在 PATH 中查找可执行文件。
fn which(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Ok(meta) = std::fs::metadata(&candidate) {
                    if meta.permissions().mode() & 0o111 != 0 {
                        return Some(candidate);
                    }
                }
            }
            #[cfg(not(unix))]
            {
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    None
}
