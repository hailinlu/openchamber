//! `/api/provider/:providerId/*` 路由 — Provider 来源检测 + 认证管理。
//!
//! 对应 Node `packages/web/server/lib/opencode/routes.js`:
//! - `GET /api/provider/:providerId/source` (line 377)
//! - `DELETE /api/provider/:providerId/auth` (line 413)

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::opencode::auth;
use crate::opencode::config::{is_plain_object, read_config_layers, ConfigLayers};
use crate::state::AppState;

/// DELETE query params。
#[derive(Deserialize)]
pub struct DeleteAuthQuery {
    scope: Option<String>,
    directory: Option<String>,
}

/// GET /api/provider/:providerId/source
///
/// 返回 provider 在 user/project/custom 三层配置中的来源信息。
/// 对齐 Node `getProviderSources` + routes.js:377 的响应形状。
pub async fn get_provider_source(
    Path(provider_id): Path<String>,
) -> impl IntoResponse {
    if provider_id.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "Provider ID is required" })),
        )
            .into_response();
    }

    let layers = match read_config_layers(None) {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(error = %e, "failed to read config layers for provider source");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Failed to read provider configuration" })),
            )
                .into_response();
        }
    };

    let sources = check_provider_in_layers(&provider_id, &layers);

    // 检查 auth.json 中是否有该 provider 的认证
    let auth_exists = auth::get_provider_auth(&provider_id)
        .ok()
        .flatten()
        .map(|entry| entry.is_object() && entry.as_object().map_or(false, |o| !o.is_empty()))
        .unwrap_or(false);

    // 构造响应 (对齐 Node 响应形状)
    let mut response_sources = json!({
        "auth": { "exists": auth_exists },
        "user": {
            "exists": sources.user_exists,
            "path": sources.user_path,
        },
        "project": {
            "exists": sources.project_exists,
            "path": sources.project_path,
        },
        "custom": {
            "exists": sources.custom_exists,
            "path": sources.custom_path,
        },
    });

    // 注入 auth.exists (Node routes.js 设置)
    if let Some(obj) = response_sources.as_object_mut() {
        if let Some(auth_entry) = obj.get_mut("auth") {
            if let Some(auth_obj) = auth_entry.as_object_mut() {
                auth_obj.insert("exists".to_string(), json!(auth_exists));
            }
        }
    }

    (
        StatusCode::OK,
        Json(json!({
            "providerId": provider_id,
            "sources": response_sources,
        })),
    )
        .into_response()
}

/// Provider 在各层配置中的来源信息。
struct ProviderSourceInfo {
    user_exists: bool,
    project_exists: bool,
    custom_exists: bool,
    user_path: Option<String>,
    project_path: Option<String>,
    custom_path: Option<String>,
}

/// 检查 provider ID 在 user/project/custom 三层配置中是否存在。
fn check_provider_in_layers(provider_id: &str, layers: &ConfigLayers) -> ProviderSourceInfo {
    let user_exists = has_provider(&layers.user_config, provider_id);
    let project_exists = has_provider(&layers.project_config, provider_id);
    let custom_exists = has_provider(&layers.custom_config, provider_id);

    let user_path = Some(layers.paths.user_path.to_string_lossy().to_string());
    let project_path = layers
        .paths
        .project_path
        .as_ref()
        .map(|p| p.to_string_lossy().to_string());
    let custom_path = layers
        .paths
        .custom_path
        .as_ref()
        .map(|p| p.to_string_lossy().to_string());

    ProviderSourceInfo {
        user_exists,
        project_exists,
        custom_exists,
        user_path,
        project_path,
        custom_path,
    }
}

/// 检查 config JSON 中是否含有该 provider (在 `provider` 或 `providers` key 下)。
fn has_provider(config: &Value, provider_id: &str) -> bool {
    // 检查 `provider.{providerId}`
    if let Some(provider_map) = config.get("provider") {
        if is_plain_object(provider_map) {
            if provider_map.get(provider_id).is_some() {
                return true;
            }
        }
    }
    // 检查 `providers.{providerId}`
    if let Some(providers_map) = config.get("providers") {
        if is_plain_object(providers_map) {
            if providers_map.get(provider_id).is_some() {
                return true;
            }
        }
    }
    false
}

/// DELETE /api/provider/:providerId/auth
///
/// 清除 Provider 认证信息。支持 scope 参数:
/// - `auth` (默认): 清除 auth.json 中的 entry
/// - `all`: 清除 auth.json + user config + project config + custom config
///
/// 对齐 Node routes.js:413-473 的响应形状。
pub async fn delete_provider_auth(
    Path(provider_id): Path<String>,
    Query(query): Query<DeleteAuthQuery>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    if provider_id.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "Provider ID is required" })),
        )
            .into_response();
    }

    let scope = query.scope.as_deref().unwrap_or("auth");
    let directory = query.directory.as_deref();
    let reload_delay_ms = 2000;

    let removed = match scope {
        "auth" => remove_auth_only(&provider_id),
        "all" => remove_auth_all(&provider_id, directory, &state),
        _ => false,
    };

    if removed {
        return (
            StatusCode::OK,
            Json(json!({
                "success": true,
                "removed": true,
                "requiresReload": true,
                "message": "Provider disconnected successfully",
                "reloadDelayMs": reload_delay_ms,
            })),
        )
            .into_response();
    }

    (
        StatusCode::OK,
        Json(json!({
            "success": true,
            "removed": false,
            "requiresReload": false,
            "message": "Provider was not connected",
        })),
    )
        .into_response()
}

/// 仅移除 auth.json 中的 provider entry。
fn remove_auth_only(provider_id: &str) -> bool {
    auth::remove_provider_auth(provider_id).unwrap_or(false)
}

/// 移除所有层级的 provider 配置 (auth.json + user + project + custom)。
fn remove_auth_all(provider_id: &str, directory: Option<&str>, _state: &Arc<AppState>) -> bool {
    let mut any_removed = false;

    // 移除 auth.json
    if auth::remove_provider_auth(provider_id).unwrap_or(false) {
        any_removed = true;
    }

    // 移除 user/project/custom 三层 config 中的 provider
    if let Ok(layers) = read_config_layers(directory) {
        let paths = [
            ("user", layers.paths.user_path.as_path()),
        ];
        for (_scope, path) in &paths {
            if let Ok(mut config) = crate::opencode::config::read_config_file(path) {
                let changed = remove_from_provider_config(&mut config, provider_id);
                if changed {
                    let _ = crate::opencode::config::write_config(&config, path);
                    any_removed = true;
                }
            }
        }

        // project config
        if let Some(ref proj_path) = layers.paths.project_path {
            if let Ok(mut config) = crate::opencode::config::read_config_file(proj_path) {
                let changed = remove_from_provider_config(&mut config, provider_id);
                if changed {
                    let _ = crate::opencode::config::write_config(&config, proj_path);
                    any_removed = true;
                }
            }
        }

        // custom config
        if let Some(ref custom_path) = layers.paths.custom_path {
            if let Ok(mut config) = crate::opencode::config::read_config_file(custom_path) {
                let changed = remove_from_provider_config(&mut config, provider_id);
                if changed {
                    let _ = crate::opencode::config::write_config(&config, custom_path);
                    any_removed = true;
                }
            }
        }
    }

    any_removed
}

/// 从 config Value 中移除指定 provider ID (在 provider 或 providers 下)。
fn remove_from_provider_config(config: &mut Value, provider_id: &str) -> bool {
    let mut changed = false;
    if let Some(obj) = config.as_object_mut() {
        // provider.{id}
        if let Some(provider_map) = obj.get_mut("provider") {
            if let Some(pmap) = provider_map.as_object_mut() {
                if pmap.remove(provider_id).is_some() {
                    changed = true;
                    // 如果 provider map 变空, 移除整个 key
                    if pmap.is_empty() {
                        obj.remove("provider");
                    }
                }
            }
        }
        // providers.{id}
        if let Some(providers_map) = obj.get_mut("providers") {
            if let Some(pmap) = providers_map.as_object_mut() {
                if pmap.remove(provider_id).is_some() {
                    changed = true;
                    if pmap.is_empty() {
                        obj.remove("providers");
                    }
                }
            }
        }
    }
    changed
}
