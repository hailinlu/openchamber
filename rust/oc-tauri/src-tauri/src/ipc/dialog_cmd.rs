//! 文件对话框 + 文件授权命令。
//!
//! 这两个命令对应 Electron 的独立 IPC 通道 (不是 desktop_* 命令):
//! - `openchamber:dialog:open` → `openchamber_dialog_open`
//! - `openchamber:file:grant-existing` → `openchamber_file_grant`
//!
//! origin 门: 仅 local-origin 可用 (复现 main.mjs:4543-4548 的 isLocalSender 门)。
//!
//! Grant 跨进程: Tauri 进程不能直接 import Node sidecar 的 `mintOutsideFileGrant`
//! (grant Map 存在于 sidecar 进程)。改为 HTTP 调用 sidecar 的 `POST /api/fs/grant`。

use serde_json::{json, Value};
use tauri::{AppHandle, WebviewWindow};
use tauri_plugin_dialog::DialogExt;

/// `openchamber_dialog_open` — args: `{ options: { directory, multiple, returnGrant, title, filters, defaultPath } }`
///
/// 复现 Electron dialog.showOpenDialog。
/// 返回: string | string[] | null (multiple → 数组, single → string, 取消 → null)。
/// 当 returnGrant=true 时返回 `{ path, outsideFileGrant, expiresAt }` (或数组)。
#[tauri::command]
pub async fn openchamber_dialog_open(
    options: Value,
    app: AppHandle,
    window: WebviewWindow,
) -> Result<Value, String> {
    // origin 门: 仅 local-origin 可用
    if !is_local_origin(&window) {
        return Err("IPC not available for this origin".into());
    }

    let directory = options
        .get("directory")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let multiple = options
        .get("multiple")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let return_grant = options
        .get("returnGrant")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let title = options.get("title").and_then(|v| v.as_str());
    let default_path = options.get("defaultPath").and_then(|v| v.as_str());
    let filters_raw = options.get("filters").and_then(|v| v.as_array());

    let mut dialog = app.dialog().file();

    if let Some(t) = title {
        dialog = dialog.set_title(t);
    }
    if let Some(p) = default_path {
        if !p.trim().is_empty() {
            dialog = dialog.set_directory(p);
        }
    }
    if directory {
        dialog = dialog.set_directory(default_path.unwrap_or(""));
    }

    // 添加文件过滤器
    if let Some(filters) = filters_raw {
        for filter in filters {
            if let Some(name) = filter.get("name").and_then(|v| v.as_str()) {
                let exts: Vec<String> = filter
                    .get("extensions")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|e| e.as_str().map(|s| s.to_string()))
                            .collect()
                    })
                    .unwrap_or_default();
                let ext_refs: Vec<&str> = exts.iter().map(|s| s.as_str()).collect();
                dialog = dialog.add_filter(name, &ext_refs);
            }
        }
    }

    // 选择
    if directory {
        if multiple {
            let paths = dialog.blocking_pick_folders();
            let arr: Vec<String> = paths.unwrap_or_default().iter().map(|p| p.to_string()).collect();
            return Ok(json!(arr));
        } else {
            let path = dialog.blocking_pick_folder();
            return Ok(path.map(|p| json!(p.to_string())).unwrap_or(Value::Null));
        }
    }

    let picked = if multiple {
        let paths = dialog.blocking_pick_files();
        paths
            .unwrap_or_default()
            .into_iter()
            .map(|p| p.to_string())
            .collect::<Vec<_>>()
    } else {
        match dialog.blocking_pick_file() {
            Some(p) => vec![p.to_string()],
            None => vec![],
        }
    };

    // returnGrant 分支: 对每个选中文件 mint grant token (复现 Electron grantFilePath)
    if return_grant && !picked.is_empty() {
        if multiple {
            let grants: Vec<Value> = mint_grants_for_paths(picked).await;
            return Ok(json!(grants));
        } else {
            return mint_grant_via_sidecar(&picked[0])
                .await
                .map(|g| json!(g))
                .or_else(|_| Ok(json!({ "path": picked[0] })));
        }
    }

    if multiple {
        Ok(json!(picked))
    } else {
        Ok(picked.into_iter().next().map(Value::String).unwrap_or(Value::Null))
    }
}

/// 对多个路径逐个 mint grant (顺序调用, 避免 sidecar 并发压力)。
async fn mint_grants_for_paths(paths: Vec<String>) -> Vec<Value> {
    let mut results = Vec::with_capacity(paths.len());
    for p in &paths {
        let val = mint_grant_via_sidecar(p)
            .await
            .map(|g| json!(g))
            .unwrap_or_else(|_| json!({ "path": p }));
        results.push(val);
    }
    results
}

/// `openchamber_file_grant` — args: `{ filePath: string }`
///
/// 复现 Electron `mintOutsideFileGrant` (security-scoped read token)。
/// Tauri 进程不能直接 import Node 的 grant Map, 改为 HTTP 调 sidecar `POST /api/fs/grant`。
/// 失败时返回路径本身 (无 grant), 与 Electron `grantFilePath` 的降级行为一致。
#[tauri::command]
pub async fn openchamber_file_grant(
    file_path: String,
    window: WebviewWindow,
) -> Result<Value, String> {
    if !is_local_origin(&window) {
        return Err("IPC not available for this origin".into());
    }

    if file_path.trim().is_empty() {
        return Err("filePath is required".into());
    }

    mint_grant_via_sidecar(&file_path)
        .await
        .map(|g| json!(g))
        .or_else(|_| Ok(json!({ "path": file_path })))
}

/// 通过 sidecar HTTP 端点 mint outside-workspace file grant。
///
/// 调用 `POST {sidecar_base}/api/fs/grant` → `{ path, outsideFileGrant, expiresAt }`。
/// sidecar 未启动或 HTTP 失败时返回 Err (调用方决定降级行为)。
async fn mint_grant_via_sidecar(file_path: &str) -> Result<Value, String> {
    let base_url = crate::sidecar_base_url()
        .ok_or_else(|| "sidecar not started".to_string())?;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/api/fs/grant", base_url))
        .json(&json!({
            "path": file_path,
            "scopes": ["stat", "read", "raw"],
        }))
        .send()
        .await
        .map_err(|e| format!("grant request failed: {}", e))?;

    if !resp.status().is_success() {
        return Err(format!("grant endpoint returned {}", resp.status()));
    }

    resp.json::<Value>()
        .await
        .map_err(|e| format!("failed to parse grant response: {}", e))
}

/// 判断窗口 origin 是否 local (loopback 或 openchamber-ui:// 协议)。
fn is_local_origin(window: &WebviewWindow) -> bool {
    let url = match window.url() {
        Ok(u) => u,
        Err(_) => return false,
    };
    let scheme = url.scheme();
    // openchamber-ui://app (packaged UI) 或 http://127.0.0.1:* (loopback)
    scheme == "openchamber-ui"
        || (scheme == "http" || scheme == "https")
            && url
                .host_str()
                .map(|h| h == "127.0.0.1" || h == "localhost")
                .unwrap_or(false)
}
