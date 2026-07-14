//! 系统集成命令 — 复现 Electron main.mjs handleInvoke 的系统分支。
//!
//! app_version: 包版本
//! lan_address: 枚举网卡 IPv4 (非 loopback)
//! launch_at_login: tauri-plugin-autostart
//! notify: tauri-plugin-notification
//! minimize_to_tray / keep_awake: settings 读写 (keep_awake 暂 stub)

use serde_json::{json, Value};
use tauri::AppHandle;
use tauri_plugin_autostart::ManagerExt;

/// `desktop_get_lan_address` — 枚举网卡 IPv4, 返回第一个非 loopback 地址。
///
/// 复现 Electron detectLanIPv4Address: 找一个可用于 LAN 发现的本地 IP。
pub async fn get_lan_address(_args: &Value, _app: &AppHandle) -> Result<Value, String> {
    let addr = detect_lan_ipv4().unwrap_or_default();
    Ok(json!(addr))
}

/// 检测 LAN IPv4 地址 (非 loopback, 优先 RFC1918 私有地址)。
fn detect_lan_ipv4() -> Option<String> {
    // 用 std::net 枚举所有网卡的地址。
    // 在没有外部 crate 的情况下, 用 OS 命令探测 (跨平台):
    #[cfg(target_os = "macos")]
    {
        if let Some(ip) = probe_ipconfig("ipconfig") {
            return Some(ip);
        }
    }
    #[cfg(target_os = "windows")]
    {
        if let Some(ip) = probe_windows_ip() {
            return Some(ip);
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(ip) = probe_linux_ip() {
            return Some(ip);
        }
    }
    None
}

#[cfg(target_os = "windows")]
fn probe_windows_ip() -> Option<String> {
    use std::process::Command;
    // ipconfig 输出可能本地化; 用 PowerShell 解析更可靠
    let output = Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "(Get-NetIPAddress -AddressFamily IPv4 | Where-Object { $_.PrefixOrigin -eq 'Dhcp' -or $_.PrefixOrigin -eq 'Manual' } | Where-Object { $_.IPAddress -notlike '127.*' -and $_.IPAddress -notlike '169.*' } | Select-Object -First 1).IPAddress",
        ])
        .output()
        .ok()?;
    let ip = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if ip.is_empty() { None } else { Some(ip) }
}

#[cfg(target_os = "macos")]
fn probe_ipconfig(_tool: &str) -> Option<String> {
    use std::process::Command;
    // macOS: ifconfig en0 的 inet
    let output = Command::new("ipconfig")
        .args(["getifaddr", "en0"])
        .output()
        .ok()?;
    let ip = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if ip.is_empty() { None } else { Some(ip) }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn probe_linux_ip() -> Option<String> {
    use std::process::Command;
    // hostname -I 返回空格分隔的所有 IPv4
    let output = Command::new("hostname").arg("-I").output().ok()?;
    let ips = String::from_utf8_lossy(&output.stdout);
    ips.split_whitespace()
        .find(|ip| !ip.starts_with("127.") && !ip.starts_with("169.254."))
        .map(|s| s.to_string())
}

/// `desktop_get_launch_at_login` — → `{ supported, enabled }`
pub async fn get_launch_at_login(_args: &Value, app: &AppHandle) -> Result<Value, String> {
    // tauri-plugin-autostart: macOS + Windows + Linux 均支持
    let enabled = app.autolaunch().is_enabled().unwrap_or(false);
    Ok(json!({ "supported": true, "enabled": enabled }))
}

/// `desktop_set_launch_at_login` — args: `{ enabled }`
pub async fn set_launch_at_login(args: &Value, app: &AppHandle) -> Result<Value, String> {
    let enabled = args.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);
    let manager = app.autolaunch();
    if enabled {
        manager.enable().map_err(|e| e.to_string())?;
    } else {
        manager.disable().map_err(|e| e.to_string())?;
    }
    let now_enabled = manager.is_enabled().unwrap_or(false);
    Ok(json!({ "supported": true, "enabled": now_enabled }))
}

/// `desktop_notify` — args: NotificationPayload `{ title, body, ... }`
///
/// 用 tauri-plugin-notification 发送。
pub async fn notify(args: &Value, app: &AppHandle) -> Result<Value, String> {
    use tauri_plugin_notification::NotificationExt;
    let title = args
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("OpenChamber")
        .to_string();
    let body = args
        .get("body")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    app.notification()
        .builder()
        .title(&title)
        .body(&body)
        .show()
        .map_err(|e| e.to_string())?;

    Ok(Value::Null)
}

/// `desktop_get_minimize_to_tray` — → `{ supported, enabled }`
///
/// 仅 Windows 支持最小化到托盘 (Electron 一致)。settings 持久化后移。
pub async fn get_minimize_to_tray(_args: &Value, _app: &AppHandle) -> Result<Value, String> {
    #[cfg(target_os = "windows")]
    {
        // TODO: 读 settings.json desktopMinimizeToTrayEnabled
        Ok(json!({ "supported": true, "enabled": false }))
    }
    #[cfg(not(target_os = "windows"))]
    {
        Ok(json!({ "supported": false, "enabled": false }))
    }
}

/// `desktop_set_minimize_to_tray` — args: `{ enabled }`
pub async fn set_minimize_to_tray(args: &Value, _app: &AppHandle) -> Result<Value, String> {
    #[cfg(target_os = "windows")]
    {
        let _enabled = args.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);
        // TODO: 写 settings.json + 重建托盘
        Ok(json!({ "supported": true, "enabled": _enabled }))
    }
    #[cfg(not(target_os = "windows"))]
    {
        Ok(json!({ "supported": false, "enabled": false }))
    }
}

/// `desktop_get_keep_awake` — → `{ supported, enabled, active }`
///
/// powerSaveBlocker 等价: 防止系统休眠。暂 stub (后续接平台 power API)。
pub async fn get_keep_awake(_args: &Value, _app: &AppHandle) -> Result<Value, String> {
    Ok(json!({ "supported": false, "enabled": false, "active": false }))
}

/// `desktop_set_keep_awake` — args: `{ enabled }`
pub async fn set_keep_awake(_args: &Value, _app: &AppHandle) -> Result<Value, String> {
    Ok(json!({ "supported": false, "enabled": false, "active": false }))
}
