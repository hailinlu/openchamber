//! `/api/config/mcp` 路由 — MCP 服务器配置 CRUD。
//!
//! 对应 Node `packages/web/server/lib/opencode/mcp.js` (278 行) +
//! `config-entity-routes.js` (MCP 部分, ~90 行)。
//!
//! 完全复用 `opencode::config` 的 ConfigLayers、read_config_layers、write_config
//! 等函数。

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::opencode::config::{
    get_config_for_path, get_primary_user_config_path, read_config_file, read_config_layers,
    write_config,
};
use crate::state::AppState;

// ============================================================================
// 类型
// ============================================================================

/// 前端看到的 MCP scope。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpScope {
    User,
    Project,
}


/// MCP 服务器配置 (返回给前端的形状)。
#[derive(Debug, Clone, Serialize)]
struct McpServerEntry {
    name: String,
    #[serde(flatten)]
    config: McpEntryConfig,
    scope: McpScope,
}

/// MCP 条目内部形状 (对应 Node `buildMcpEntry` 返回值)。
#[derive(Debug, Clone, Serialize)]
struct McpEntryConfig {
    #[serde(rename = "type")]
    mcp_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    command: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    environment: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    headers: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    oauth: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    timeout: Option<u64>,
    enabled: bool,
}

/// GET /api/config/mcp query params。
#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub directory: Option<String>,
}

/// PATCH /api/config/mcp/:name body。
#[derive(Debug, Deserialize)]
pub struct CreateBody {
    pub scope: Option<String>,
    #[serde(flatten)]
    pub config: Value,
}

/// PATCH /api/config/mcp/:name body。
type UpdateBody = Value;

// ============================================================================
// CRUD 函数 (对应 Node `mcp.js`)
// ============================================================================

/// 确定 MCP 条目的 scope。
fn resolve_mcp_scope(layers: &crate::opencode::config::ConfigLayers, source_path: Option<&std::path::Path>) -> McpScope {
    match source_path {
        Some(p) if Some(p) == layers.paths.project_path.as_deref() => McpScope::Project,
        _ => McpScope::User,
    }
}

/// 列出所有 MCP 服务器。
///
/// 对应 Node `listMcpConfigs` (`mcp.js:43`)。
fn list_mcp_configs(working_directory: Option<&str>) -> Result<Vec<McpServerEntry>, String> {
    let layers = read_config_layers(working_directory).map_err(|e| e.to_string())?;
    let mcp = layers.merged_config.get("mcp");

    let Some(mcp_obj) = mcp.and_then(|v| v.as_object()) else {
        return Ok(vec![]);
    };

    let mut entries: Vec<McpServerEntry> = Vec::new();
    for (name, entry) in mcp_obj {
        if !entry.is_object() {
            continue;
        }
        // 查 source 归属 (custom → project → user)
        let source_path = get_mcp_entry_source_path(&layers, name);
        let scope = resolve_mcp_scope(&layers, source_path.as_deref());

        entries.push(McpServerEntry {
            name: name.clone(),
            config: build_mcp_entry(entry),
            scope,
        });
    }

    Ok(entries)
}

/// 查 MCP 条目所属的 config 文件路径。
fn get_mcp_entry_source_path(
    layers: &crate::opencode::config::ConfigLayers,
    entry_name: &str,
) -> Option<std::path::PathBuf> {
    // custom → project → user
    if let Some(c) = &layers.paths.custom_path {
        if let Some(cfg) = layers.custom_config.get("mcp").and_then(|m| m.get(entry_name)) {
            if cfg.is_object() {
                return Some(c.clone());
            }
        }
    }
    if let Some(p) = &layers.paths.project_path {
        if let Some(cfg) = layers.project_config.get("mcp").and_then(|m| m.get(entry_name)) {
            if cfg.is_object() {
                return Some(p.clone());
            }
        }
    }
    if let Some(cfg) = layers.user_config.get("mcp").and_then(|m| m.get(entry_name)) {
        if cfg.is_object() {
            return Some(layers.paths.user_path.clone());
        }
    }
    None
}

/// 构建 MCP 条目 (对应 Node `buildMcpEntry`, `mcp.js:168`)。
fn build_mcp_entry(data: &Value) -> McpEntryConfig {
    let is_remote = data.get("type").and_then(|v| v.as_str()) == Some("remote");
    let mcp_type = if is_remote { "remote" } else { "local" };

    let command = if !is_remote {
        data.get("command")
            .and_then(|v| v.as_array())
            .filter(|a| !a.is_empty())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect::<Vec<_>>()
            })
    } else {
        None
    };

    let url = if is_remote {
        data.get("url").and_then(|v| v.as_str().map(|s| s.trim().to_string()))
            .filter(|s| !s.is_empty())
    } else {
        None
    };

    let environment = data.get("environment")
        .filter(|v| v.is_object())
        .filter(|v| v.as_object().map_or(false, |o| !o.is_empty()))
        .cloned();

    let headers = if is_remote {
        data.get("headers")
            .filter(|v| v.is_object())
            .filter(|v| v.as_object().map_or(false, |o| !o.is_empty()))
            .cloned()
    } else {
        None
    };

    let oauth = if is_remote {
        if data.get("oauth") == Some(&Value::Bool(false)) {
            Some(Value::Bool(false))
        } else {
            data.get("oauth")
                .filter(|v| v.is_object())
                .map(|v| {
                    let mut clean = Value::Object(serde_json::Map::new());
                    if let Some(obj) = v.as_object() {
                        for key in ["clientId", "clientSecret", "scope", "redirectUri"] {
                            if let Some(val) = obj.get(key).and_then(|x| x.as_str()).filter(|s| !s.is_empty()) {
                                clean.as_object_mut().unwrap().insert(key.to_string(), Value::String(val.to_string()));
                            }
                        }
                    }
                    clean
                })
                .filter(|v| v.as_object().map_or(false, |o| !o.is_empty()))
        }
    } else {
        None
    };

    let timeout = if is_remote {
        data.get("timeout")
            .and_then(|v| v.as_f64())
            .filter(|&n| n > 0.0 && n.is_finite())
            .map(|n| n as u64)
    } else {
        None
    };

    let enabled = data.get("enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    McpEntryConfig {
        mcp_type: mcp_type.to_string(),
        command,
        url,
        environment,
        headers,
        oauth,
        timeout,
        enabled,
    }
}

/// 验证 MCP 名称 (对应 Node `validateMcpName`, `mcp.js:18`)。
fn validate_mcp_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("MCP server name is required".to_string());
    }
    let re = regex::Regex::new(r"^[a-z0-9][a-z0-9_-]*[a-z0-9]$|^[a-z0-9]$").unwrap();
    if !re.is_match(name) {
        return Err("MCP server name must be lowercase alphanumeric with hyphens/underscores".to_string());
    }
    Ok(())
}

/// 确保项目级 MCP config 目录存在。
fn ensure_project_mcp_config_path(working_directory: &str) -> Result<std::path::PathBuf, String> {
    let config_dir = std::path::Path::new(working_directory).join(".opencode");
    std::fs::create_dir_all(&config_dir)
        .map_err(|e| format!("Failed to create project config directory: {}", e))?;
    Ok(config_dir.join("opencode.json"))
}

// ============================================================================
// 路由 handlers
// ============================================================================

/// GET /api/config/mcp — 列出所有 MCP 服务器。
pub async fn list_mcp(
    State(_state): State<Arc<AppState>>,
    Query(query): Query<ListQuery>,
) -> impl IntoResponse {
    match list_mcp_configs(query.directory.as_deref()) {
        Ok(entries) => (StatusCode::OK, Json(serde_json::to_value(entries).unwrap_or(json!([])))).into_response(),
        Err(e) => {
            tracing::error!("[API:GET /api/config/mcp] Failed: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e }))).into_response()
        }
    }
}

/// GET /api/config/mcp/:name — 获取单个 MCP 服务器。
pub async fn get_mcp(
    State(_state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(query): Query<ListQuery>,
) -> impl IntoResponse {
    let layers = match read_config_layers(query.directory.as_deref()) {
        Ok(l) => l,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response();
        }
    };
    let entry = layers.merged_config.get("mcp")
        .and_then(|m| m.get(&name));

    match entry {
        Some(entry_val) if entry_val.is_object() => {
            let source_path = get_mcp_entry_source_path(&layers, &name);
            let scope = resolve_mcp_scope(&layers, source_path.as_deref());
            let result = McpServerEntry {
                name,
                config: build_mcp_entry(entry_val),
                scope,
            };
            (StatusCode::OK, Json(serde_json::to_value(result).unwrap_or(json!({})))).into_response()
        }
        _ => (StatusCode::NOT_FOUND, Json(json!({ "error": format!("MCP server \"{}\" not found", name) }))).into_response(),
    }
}

/// POST /api/config/mcp/:name — 创建 MCP 服务器。
pub async fn create_mcp(
    State(_state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(query): Query<ListQuery>,
    Json(body): Json<CreateBody>,
) -> impl IntoResponse {
    if let Err(e) = validate_mcp_name(&name) {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response();
    }

    let layers = match read_config_layers(query.directory.as_deref()) {
        Ok(l) => l,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response();
        }
    };

    // 检查是否已存在
    if get_mcp_entry_source_path(&layers, &name).is_some() {
        return (StatusCode::CONFLICT, Json(json!({ "error": format!("MCP server \"{}\" already exists", name) }))).into_response();
    }

    let scope = body.scope.as_deref().unwrap_or("user");

    let target_path: std::path::PathBuf;
    let mut config: Value;

    if scope == "project" {
        let Some(ref dir) = query.directory else {
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": "Project scope requires working directory" }))).into_response();
        };
        target_path = match ensure_project_mcp_config_path(dir) {
            Ok(p) => p,
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e }))).into_response(),
        };
        config = if target_path.exists() {
            read_config_file(&target_path).unwrap_or(json!({}))
        } else {
            json!({})
        };
    } else {
        // user scope — 写入 primary user config
        target_path = get_primary_user_config_path(&[
            crate::opencode::paths::opencode_config_dir().join("config.json"),
            crate::opencode::paths::opencode_config_dir().join("opencode.json"),
            crate::opencode::paths::opencode_config_dir().join("opencode.jsonc"),
        ]);
        let source = get_config_for_path(&layers, Some(&target_path));
        config = source.clone();
    };

    // 确保 mcp 是 object
    if !config.get("mcp").map_or(false, |v| v.is_object()) {
        config.as_object_mut().unwrap().insert("mcp".to_string(), json!({}));
    }

    // 构建条目并写入
    let entry_data = build_mcp_entry_value(&body.config, scope == "project");
    config["mcp"][&name] = entry_data;

    if let Err(e) = write_config(&config, &target_path) {
        tracing::error!("[API:POST /api/config/mcp/:name] Write failed: {}", e);
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response();
    }

    tracing::info!("Created MCP server config: {}", name);

    Json(json!({
        "success": true,
        "requiresReload": true,
        "message": format!("MCP server \"{}\" created. Reloading interface…", name),
        "reloadDelayMs": 800,
    })).into_response()
}

/// PATCH /api/config/mcp/:name — 更新 MCP 服务器。
pub async fn update_mcp(
    State(_state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(query): Query<ListQuery>,
    Json(updates): Json<UpdateBody>,
) -> impl IntoResponse {
    let layers = match read_config_layers(query.directory.as_deref()) {
        Ok(l) => l,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response();
        }
    };

    let source_path = get_mcp_entry_source_path(&layers, &name);
    let source_path = match source_path {
        Some(p) => p,
        None => {
            return (StatusCode::NOT_FOUND, Json(json!({ "error": format!("MCP server \"{}\" not found", name) }))).into_response();
        }
    };

    let config = get_config_for_path(&layers, Some(&source_path));

    // 确保 mcp 是 object
    let mut config = config.clone();
    if !config.get("mcp").map_or(false, |v| v.is_object()) {
        config.as_object_mut().unwrap().insert("mcp".to_string(), json!({}));
    }

    let existing = config["mcp"][&name].clone();
    let merged = merge_mcp_entry(&existing, &updates);
    config["mcp"][&name] = merged;

    if let Err(e) = write_config(&config, &source_path) {
        tracing::error!("[API:PATCH /api/config/mcp/:name] Write failed: {}", e);
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response();
    }

    tracing::info!("Updated MCP server config: {}", name);

    Json(json!({
        "success": true,
        "requiresReload": true,
        "message": format!("MCP server \"{}\" updated. Reloading interface…", name),
        "reloadDelayMs": 800,
    })).into_response()
}

/// DELETE /api/config/mcp/:name — 删除 MCP 服务器。
pub async fn delete_mcp(
    State(_state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(query): Query<ListQuery>,
) -> impl IntoResponse {
    let layers = match read_config_layers(query.directory.as_deref()) {
        Ok(l) => l,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response();
        }
    };

    let source_path = get_mcp_entry_source_path(&layers, &name);
    let source_path = match source_path {
        Some(p) => p,
        None => {
            return (StatusCode::NOT_FOUND, Json(json!({ "error": format!("MCP server \"{}\" not found", name) }))).into_response();
        }
    };

    let config = get_config_for_path(&layers, Some(&source_path));
    let mut config = config.clone();

    if let Some(mcp_obj) = config.get_mut("mcp").and_then(|m| m.as_object_mut()) {
        mcp_obj.remove(&name);
        if mcp_obj.is_empty() {
            config.as_object_mut().unwrap().remove("mcp");
        }
    }

    if let Err(e) = write_config(&config, &source_path) {
        tracing::error!("[API:DELETE /api/config/mcp/:name] Write failed: {}", e);
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response();
    }

    tracing::info!("Deleted MCP server config: {}", name);

    Json(json!({
        "success": true,
        "requiresReload": true,
        "message": format!("MCP server \"{}\" deleted. Reloading interface…", name),
        "reloadDelayMs": 800,
    })).into_response()
}

// ============================================================================
// 内部 helpers
// ============================================================================

/// 从请求 body 构建 MCP 条目 Value (用于 create)。
fn build_mcp_entry_value(data: &Value, _is_project: bool) -> Value {
    let mut entry = serde_json::Map::new();

    // type
    let mcp_type = data.get("type").and_then(|v| v.as_str()).unwrap_or("local");
    entry.insert("type".to_string(), json!(mcp_type));

    if mcp_type == "remote" {
        // url
        if let Some(url) = data.get("url").and_then(|v| v.as_str()).map(|s| s.trim()).filter(|s| !s.is_empty()) {
            entry.insert("url".to_string(), json!(url));
        }
        // headers
        if let Some(h) = data.get("headers").filter(|v| v.is_object()) {
            let clean: Value = h.clone();
            entry.insert("headers".to_string(), clean);
        }
        // oauth
        if data.get("oauth") == Some(&Value::Bool(false)) {
            entry.insert("oauth".to_string(), Value::Bool(false));
        } else if let Some(o) = data.get("oauth").filter(|v| v.is_object()) {
            entry.insert("oauth".to_string(), o.clone());
        }
        // timeout
        if let Some(t) = data.get("timeout").and_then(|v| v.as_f64()).filter(|&n| n > 0.0 && n.is_finite()) {
            entry.insert("timeout".to_string(), json!(t as u64));
        }
    } else {
        // local: command
        if let Some(cmd) = data.get("command").and_then(|v| v.as_array()).filter(|a| !a.is_empty()) {
            entry.insert("command".to_string(), json!(cmd));
        }
    }

    // environment
    if let Some(env) = data.get("environment").filter(|v| v.is_object()) {
        let clean: Value = env.clone();
        entry.insert("environment".to_string(), clean);
    }

    // enabled
    let enabled = data.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
    entry.insert("enabled".to_string(), json!(enabled));

    Value::Object(entry)
}

/// 合并现有 MCP 条目与更新 (类似 Node `{ ...existing, ...updateData }`)。
fn merge_mcp_entry(existing: &Value, updates: &Value) -> Value {
    let mut result = existing.clone();
    if let Some(obj) = updates.as_object() {
        if let Some(result_obj) = result.as_object_mut() {
            for (k, v) in obj {
                if k == "name" || k == "scope" {
                    continue; // 忽略元数据字段
                }
                result_obj.insert(k.clone(), v.clone());
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn validate_mcp_name_valid() {
        assert!(validate_mcp_name("my-server").is_ok());
        assert!(validate_mcp_name("a").is_ok());
        assert!(validate_mcp_name("dev-tools-v2").is_ok());
    }

    #[test]
    fn validate_mcp_name_invalid() {
        assert!(validate_mcp_name("").is_err());
        assert!(validate_mcp_name("UPPERCASE").is_err());
        assert!(validate_mcp_name("has space").is_err());
        assert!(validate_mcp_name("-leading").is_err());
        assert!(validate_mcp_name("trailing-").is_err());
    }

    #[test]
    fn build_mcp_entry_local() {
        let data = json!({
            "type": "local",
            "command": ["npx", "-y", "some-server"],
            "environment": {"KEY": "val"},
            "enabled": true,
        });
        let entry = build_mcp_entry(&data);
        assert_eq!(entry.mcp_type, "local");
        assert_eq!(entry.command, Some(vec!["npx".to_string(), "-y".to_string(), "some-server".to_string()]));
        assert!(entry.url.is_none());
        assert!(entry.enabled);
    }

    #[test]
    fn build_mcp_entry_remote() {
        let data = json!({
            "type": "remote",
            "url": "https://example.com/mcp",
            "headers": {"Authorization": "Bearer xyz"},
            "timeout": 30,
            "enabled": true,
        });
        let entry = build_mcp_entry(&data);
        assert_eq!(entry.mcp_type, "remote");
        assert_eq!(entry.url, Some("https://example.com/mcp".to_string()));
        assert!(entry.command.is_none());
        assert_eq!(entry.timeout, Some(30));
    }

    #[test]
    fn build_mcp_entry_default_enabled() {
        let data = json!({"type": "local"});
        let entry = build_mcp_entry(&data);
        assert!(entry.enabled);
    }

    #[test]
    fn build_mcp_entry_disabled() {
        let data = json!({"type": "local", "enabled": false});
        let entry = build_mcp_entry(&data);
        assert!(!entry.enabled);
    }

    #[test]
    fn resolve_mcp_scope_user() {
        let layers = crate::opencode::config::ConfigLayers {
            user_config: json!({}),
            project_config: json!({}),
            custom_config: json!({}),
            merged_config: json!({}),
            paths: crate::opencode::config::ConfigPathsInternal {
                user_path: std::path::PathBuf::from("/home/user/.config/opencode/opencode.json"),
                project_path: Some(std::path::PathBuf::from("/project/.opencode/opencode.json")),
                custom_path: None,
            },
        };
        let scope = resolve_mcp_scope(&layers, Some(std::path::Path::new("/home/user/.config/opencode/opencode.json")));
        assert_eq!(scope, McpScope::User);
    }

    #[test]
    fn resolve_mcp_scope_project() {
        let layers = crate::opencode::config::ConfigLayers {
            user_config: json!({}),
            project_config: json!({}),
            custom_config: json!({}),
            merged_config: json!({}),
            paths: crate::opencode::config::ConfigPathsInternal {
                user_path: std::path::PathBuf::from("/home/user/.config/opencode/opencode.json"),
                project_path: Some(std::path::PathBuf::from("/project/.opencode/opencode.json")),
                custom_path: None,
            },
        };
        let scope = resolve_mcp_scope(&layers, Some(std::path::Path::new("/project/.opencode/opencode.json")));
        assert_eq!(scope, McpScope::Project);
    }
}
