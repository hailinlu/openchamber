//! 文件对话框 + 文件授权命令。
//!
//! 这两个命令对应 Electron 的独立 IPC 通道 (不是 desktop_* 命令):
//! - `openchamber:dialog:open` → `openchamber_dialog_open`
//! - `openchamber:file:grant-existing` → `openchamber_file_grant`
//!
//! origin 门: 仅 local-origin 可用 (复现 main.mjs:4543-4548 的 isLocalSender 门)。

use serde_json::{json, Value};
use tauri::{AppHandle, WebviewWindow};
use tauri_plugin_dialog::DialogExt;

/// `openchamber_dialog_open` — args: `{ options: { directory, multiple, title, filters, defaultPath } }`
///
/// 复现 Electron dialog.showOpenDialog。
/// 返回: string | string[] | null (multiple → 数组, single → string, 取消 → null)。
///
/// returnGrant 分支暂不支持 (需接 web server 的 mintOutsideFileGrant)。
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
            return Ok(format_multi_result(paths));
        } else {
            let path = dialog.blocking_pick_folder();
            return Ok(format_single_result(path));
        }
    }

    if multiple {
        let paths = dialog.blocking_pick_files();
        Ok(format_multi_result(paths))
    } else {
        let path = dialog.blocking_pick_file();
        Ok(format_single_result(path))
    }
}

/// 格式化单选结果 → string | null
fn format_single_result(result: Option<tauri_plugin_dialog::FilePath>) -> Value {
    match result {
        Some(path) => json!(path.to_string()),
        None => Value::Null,
    }
}

/// 格式化多选结果 → string[]
fn format_multi_result(result: Option<Vec<tauri_plugin_dialog::FilePath>>) -> Value {
    match result {
        Some(paths) => {
            let arr: Vec<String> = paths.iter().map(|p| p.to_string()).collect();
            json!(arr)
        }
        None => json!([]),
    }
}

/// `openchamber_file_grant` — args: `{ filePath: string }`
///
/// 复现 mintOutsideFileGrant (security-scoped read token)。
/// 暂 stub: 需接 web server 的 grant API。返回路径本身作为 grant 占位。
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

    // TODO: 调用 web server /api/fs/grant mintOutsideFileGrant
    // 暂返回占位 grant (path + 空 grant + 30分钟过期)
    let expires_at = chrono::Utc::now().timestamp_millis() + 30 * 60 * 1000;
    Ok(json!({
        "path": file_path,
        "outsideFileGrant": null,
        "expiresAt": expires_at,
    }))
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
