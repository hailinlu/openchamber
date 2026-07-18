//! Shell / 文件命令 — 复现 Electron main.mjs handleInvoke 的 shell 分支。
//!
//! open_external_url: 验证 http/https → shell open
//! open_path: shell open (macOS 可加 -a app)
//! reveal_path: dir → openPath, file → showItemInFolder 等价
//! save_markdown_file: dialog save → 写文件
//! clear_cache: reload window

use std::fs;
use std::path::Path;

use serde_json::{json, Value};
use tauri::{AppHandle, Manager, WebviewWindow};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_shell::ShellExt;

/// `desktop_open_external_url` — args: `{ url: string }`
///
/// 仅允许 http/https (与 Electron 一致)。
#[allow(deprecated)]
pub async fn open_external_url(args: &Value, _window: &WebviewWindow) -> Result<Value, String> {
    let url = args
        .get("url")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "URL is required".to_string())?
        .trim()
        .to_string();

    if url.is_empty() {
        return Err("URL is required".into());
    }

    // 验证协议 (Electron: 仅 http/https)
    let parsed = url::Url::parse(&url).map_err(|_| "Invalid URL".to_string())?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err("Only HTTP URLs can be opened externally".into());
    }

    // tauri-plugin-shell::open — 用系统默认浏览器打开
    _window
        .app_handle()
        .shell()
        .open(parsed.to_string(), None)
        .map_err(|e| e.to_string())?;

    Ok(Value::Null)
}

/// `desktop_open_path` — args: `{ path: string, app?: string }`
///
/// 打开文件/文件夹。macOS 可指定应用 (-a)。
#[allow(deprecated)]
pub async fn open_path(args: &Value, window: &WebviewWindow) -> Result<Value, String> {
    let target_path = args
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "Path is required".to_string())?
        .trim()
        .to_string();

    if target_path.is_empty() {
        return Err("Path is required".into());
    }

    let _app_name = args.get("app").and_then(|v| v.as_str()).unwrap_or("").trim();

    // macOS: 如果指定了 app, 用 `open -a <app> <path>`
    #[cfg(target_os = "macos")]
    {
        if !_app_name.is_empty() {
            use std::process::Command;
            Command::new("open")
                .args(["-a", _app_name, &target_path])
                .spawn()
                .map_err(|e| format!("Failed to open: {}", e))?;
            return Ok(Value::Null);
        }
    }

    // 默认: shell open (系统默认应用)
    window
        .app_handle()
        .shell()
        .open(target_path, None)
        .map_err(|e| e.to_string())?;

    Ok(Value::Null)
}

/// `desktop_reveal_path` — args: `{ path: string }`
///
/// 文件 → 在文件管理器中定位 (showItemInFolder 等价);
/// 文件夹 → 直接打开文件夹。
#[allow(deprecated)]
pub async fn reveal_path(args: &Value, window: &WebviewWindow) -> Result<Value, String> {
    let target_path = args
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "Path is required".to_string())?
        .trim()
        .to_string();

    if target_path.is_empty() {
        return Err("Path is required".into());
    }

    let path = Path::new(&target_path);
    let is_dir = path.is_dir();

    if is_dir {
        // dir → openPath (打开文件夹)
        window
            .app_handle()
            .shell()
            .open(target_path.clone(), None)
            .map_err(|e| e.to_string())?;
    } else {
        // file → showItemInFolder 等价
        // tauri-plugin-shell 的 open 只能打开, 不能 "reveal"。
        // 用平台命令实现 reveal:
        reveal_in_file_manager(&target_path)?;
    }

    Ok(Value::Null)
}

/// 在系统文件管理器中定位文件 (showItemInFolder 等价)。
fn reveal_in_file_manager(path: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        use std::process::Command;
        Command::new("open")
            .args(["-R", path])
            .spawn()
            .map_err(|e| format!("Failed to reveal: {}", e))?;
    }
    #[cfg(target_os = "windows")]
    {
        use std::process::Command;
        // explorer.exe /select,"<path>"
        Command::new("explorer.exe")
            .args(["/select,", path])
            .spawn()
            .map_err(|e| format!("Failed to reveal: {}", e))?;
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        use std::process::Command;
        // Linux: 尝试 xdg-open 打开父目录
        let parent = std::path::Path::new(path)
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| ".".to_string());
        Command::new("xdg-open")
            .arg(&parent)
            .spawn()
            .map_err(|e| format!("Failed to reveal: {}", e))?;
    }
    Ok(())
}

/// `desktop_save_markdown_file` — args: `{ defaultFileName, content }`
///
/// 弹出保存对话框 → 写文件 → 返回保存路径或 null。
pub async fn save_markdown_file(
    args: &Value,
    app: &AppHandle,
) -> Result<Value, String> {
    let default_name = args
        .get("defaultFileName")
        .and_then(|v| v.as_str())
        .unwrap_or("export.md")
        .to_string();
    let content = args
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // tauri-plugin-dialog save dialog (block_on 在 async 函数内)
    let file_path = app
        .dialog()
        .file()
        .set_file_name(&default_name)
        .add_filter("Markdown", &["md"])
        .blocking_save_file();

    match file_path {
        Some(path) => {
            let path_str = path.to_string();
            fs::write(&path_str, content.as_bytes())
                .map_err(|e| format!("Failed to write file: {}", e))?;
            Ok(json!(path_str))
        }
        None => Ok(Value::Null),
    }
}

/// `desktop_clear_cache` — reload all windows
pub async fn clear_cache(_args: &Value, app: &AppHandle) -> Result<Value, String> {
    // Tauri 没有等价 session.clearStorageData 的全局 API;
    // 简单实现: reload 所有窗口 (WebView 会清 transient 状态)。
    // 完整存储清除后移。
    for (_label, window) in app.webview_windows() {
        let _ = window.eval("location.reload(true)");
    }
    Ok(Value::Null)
}

/// `desktop_get_app_version`
pub async fn get_app_version(_args: &Value, app: &AppHandle) -> Result<Value, String> {
    let version = app
        .package_info()
        .version
        .to_string();
    Ok(json!(version))
}

/// `desktop_open_in_app` — args: `{ path: string }`
///
/// 用系统默认应用打开文件路径。复现 Electron `shell.openPath()`。
#[allow(deprecated)]
pub async fn open_in_app(args: &Value, window: &WebviewWindow) -> Result<Value, String> {
    let path = args
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "path is required".to_string())?
        .trim()
        .to_string();

    if path.is_empty() {
        return Err("path is required".to_string());
    }

    window
        .app_handle()
        .shell()
        .open(path, None)
        .map_err(|e| format!("failed to open path: {}", e))?;

    Ok(Value::Null)
}
