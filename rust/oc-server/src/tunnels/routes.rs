//! Tunnels 路由 — 7 个隧道 handler + `/connect` handler。
//!
//! 移植自:
//!   - `packages/web/server/lib/tunnels/routes.js` (610 行)
//!   - `/connect` 路由 (core-routes.js line 944-971)
//!
//! 所有 handler 返回 `ApiResult<Json<Value>>` 或自定义 response (用于 /connect)。

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use crate::github::settings::read_settings;
use crate::state::AppState;
use crate::tunnels::managed_config::ManagedConfigRuntime;
use crate::tunnels::tunnel_auth::{
    build_cookie, get_rate_limit_key, TUNNEL_SESSION_COOKIE_NAME,
};
use crate::tunnels::types::{
    normalize_managed_remote_tunnel_hostname, normalize_optional_path,
    normalize_tunnel_mode, normalize_tunnel_provider,
    is_supported_tunnel_mode, TunnelServiceError,
    TUNNEL_MODE_MANAGED_LOCAL, TUNNEL_MODE_MANAGED_REMOTE, TUNNEL_MODE_QUICK,
    TUNNEL_PROVIDER_CLOUDFLARE,
};
use crate::tunnels::{
    normalize_tunnel_bootstrap_ttl_ms, normalize_tunnel_session_ttl_ms,
};

// ============================================================
// Helper functions
// ============================================================

fn opt_query(params: &HashMap<String, String>, key: &str) -> String {
    params
        .get(key)
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// 返回当前平台标识 (对应 Node `process.platform`)。
fn current_platform() -> &'static str {
    #[cfg(target_os = "macos")]
    { "darwin" }
    #[cfg(target_os = "windows")]
    { "win32" }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    { "linux" }
}

fn opt_query_hostname(params: &HashMap<String, String>, key: &str) -> Option<String> {
    params
        .get(key)
        .and_then(|s| normalize_managed_remote_tunnel_hostname(Some(&Value::from(s.as_str()))))
}

/// 从 settings + query/body 解析 hostname (多源 fallback)。
fn resolve_hostname(
    params: &HashMap<String, String>,
    body: &Value,
    settings: &Value,
) -> Option<String> {
    // body.hostname
    if let Some(h) = normalize_managed_remote_tunnel_hostname(body.get("hostname")) {
        return Some(h);
    }
    // body.tunnelHostname
    if let Some(h) = normalize_managed_remote_tunnel_hostname(body.get("tunnelHostname")) {
        return Some(h);
    }
    // body.managedRemoteTunnelHostname
    if let Some(h) = normalize_managed_remote_tunnel_hostname(body.get("managedRemoteTunnelHostname")) {
        return Some(h);
    }
    // query.hostname
    if let Some(h) = opt_query_hostname(params, "hostname") {
        return Some(h);
    }
    // query.tunnelHostname
    if let Some(h) = opt_query_hostname(params, "tunnelHostname") {
        return Some(h);
    }
    // query.managedRemoteTunnelHostname
    if let Some(h) = opt_query_hostname(params, "managedRemoteTunnelHostname") {
        return Some(h);
    }
    // settings.managedRemoteTunnelHostname
    normalize_managed_remote_tunnel_hostname(settings.get("managedRemoteTunnelHostname"))
}

/// 从 settings + query/body 解析 token (多源 fallback)。
async fn resolve_token(
    body: &Value,
    settings: &Value,
    hostname: &Option<String>,
    selected_preset_id: &str,
    managed_config: &ManagedConfigRuntime,
    runtime_hostname: &str,
    runtime_token: &str,
) -> String {
    // body.token
    let request_token = body.get("token").and_then(|v| v.as_str()).unwrap_or("").trim();
    if !request_token.is_empty() {
        return request_token.to_string();
    }
    // body.tunnelToken
    let request_tunnel_token = body
        .get("tunnelToken")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    if !request_tunnel_token.is_empty() {
        return request_tunnel_token.to_string();
    }
    // body.managedRemoteTunnelToken
    let request_managed_token = body
        .get("managedRemoteTunnelToken")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    if !request_managed_token.is_empty() {
        return request_managed_token.to_string();
    }
    // runtime token (if hostname matches)
    if !runtime_hostname.is_empty() {
        if let Some(h) = hostname {
            if runtime_hostname == h && !runtime_token.is_empty() {
                return runtime_token.to_string();
            }
        }
    }
    // config token
    let config_token = managed_config
        .resolve(selected_preset_id, hostname.as_deref().unwrap_or(""))
        .await;
    if !config_token.is_empty() {
        return config_token;
    }
    // settings.managedRemoteTunnelToken
    settings
        .get("managedRemoteTunnelToken")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string()
}

/// 从 URL 解析 hostname (lowercase)。
fn resolve_normalized_tunnel_host(public_url: &str) -> Option<String> {
    if public_url.trim().is_empty() {
        return None;
    }
    let after_scheme = public_url.find("://").map(|p| &public_url[p + 3..]).unwrap_or(public_url);
    let authority = after_scheme.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority.rsplit('@').next().unwrap_or("");
    let host = match host.rfind(':') {
        Some(pos) => &host[..pos],
        None => host,
    };
    let host = host.trim().to_lowercase();
    if host.is_empty() {
        None
    } else {
        Some(host)
    }
}

fn generate_uuid() -> String {
    let mut bytes = [0u8; 16];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3],
        bytes[4], bytes[5],
        bytes[6], bytes[7],
        bytes[8], bytes[9],
        bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
    )
}

// ============================================================
// Route handlers
// ============================================================

/// `GET /api/openchamber/tunnel/check` — 依赖可用性检查。
pub async fn tunnel_check(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Json<Value> {
    let requested_provider = if !opt_query(&params, "provider").is_empty() {
        normalize_tunnel_provider(Some(&opt_query(&params, "provider")))
    } else {
        // resolvePreferredTunnelProvider
        let active_provider = state.tunnel_service.resolve_active_provider().await;
        if let Some(ap) = active_provider {
            normalize_tunnel_provider(Some(&ap))
        } else {
            let settings = read_settings();
            normalize_tunnel_provider(settings.get("tunnelProvider").and_then(|v| v.as_str()))
        }
    };

    match state
        .tunnel_service
        .check_availability(&requested_provider)
        .await
    {
        Ok(result) => Json(json!({
            "available": result.get("available").and_then(|v| v.as_bool()).unwrap_or(false),
            "provider": requested_provider,
            "version": result.get("version").cloned().unwrap_or(Value::Null),
            "dependency": result.get("dependency").cloned().unwrap_or(Value::Null),
            "installCommand": result.get("installCommand").cloned().unwrap_or(Value::Null),
            "installUrl": result.get("installUrl").cloned().unwrap_or(Value::Null),
            "platform": result.get("platform").cloned().unwrap_or_else(|| json!(current_platform())),
            "message": result.get("message").cloned().unwrap_or(Value::Null),
        })),
        Err(_) => Json(json!({
            "available": false,
            "provider": null,
            "version": null,
            "dependency": null,
            "installCommand": null,
            "installUrl": null,
            "platform": current_platform(),
            "message": null,
        })),
    }
}

/// `GET/POST /api/openchamber/tunnel/doctor` — 诊断检查。
pub async fn tunnel_doctor(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
    body: Option<Json<Value>>,
) -> Response {
    let body = body.map(|b| b.0).unwrap_or(Value::Null);

    let provider_id = if !opt_query(&params, "provider").is_empty() {
        normalize_tunnel_provider(Some(&opt_query(&params, "provider")))
    } else {
        let active_provider = state.tunnel_service.resolve_active_provider().await;
        if let Some(ap) = active_provider {
            normalize_tunnel_provider(Some(&ap))
        } else {
            let settings = read_settings();
            normalize_tunnel_provider(settings.get("tunnelProvider").and_then(|v| v.as_str()))
        }
    };

    let mode_filter_raw = opt_query(&params, "mode");
    let mode_filter = if mode_filter_raw.is_empty() {
        None
    } else {
        Some(mode_filter_raw.to_lowercase())
    };

    let settings = read_settings();
    let selected_preset_id = opt_query(&params, "managedRemoteTunnelPresetId");

    let request_config_path = normalize_optional_path(Some(&Value::from(opt_query(&params, "configPath").as_str())))
        .or_else(|| {
            normalize_optional_path(settings.get("managedLocalTunnelConfigPath"))
        });

    let hostname = resolve_hostname(&params, &body, &settings);

    let _managed_remote_tunnel_config = state.managed_config.read().await;

    // 构建 doctor request
    let request_token = body.get("token").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    let request_tunnel_token = body.get("tunnelToken").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    let request_managed_remote_token = body.get("managedRemoteTunnelToken").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    let request_token_provided = body.get("tokenProvided").and_then(|v| v.as_bool()).unwrap_or(false)
        || body.get("tunnelTokenProvided").and_then(|v| v.as_bool()).unwrap_or(false)
        || body.get("managedRemoteTunnelTokenProvided").and_then(|v| v.as_bool()).unwrap_or(false);
    let request_hostname_provided = body.get("hostnameProvided").and_then(|v| v.as_bool()).unwrap_or(false)
        || body.get("tunnelHostnameProvided").and_then(|v| v.as_bool()).unwrap_or(false)
        || body.get("managedRemoteTunnelHostnameProvided").and_then(|v| v.as_bool()).unwrap_or(false);

    let stored_managed_remote_token = settings.get("managedRemoteTunnelToken").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    let server_has_saved_managed_remote_profile = state
        .managed_config
        .read()
        .await
        .tunnels
        .iter()
        .any(|entry| {
            let saved_hostname = normalize_managed_remote_tunnel_hostname(Some(&Value::from(entry.hostname.as_str())));
            let saved_token = !entry.token.is_empty();
            saved_hostname.is_some() && saved_token
        });
    let cli_has_saved_managed_remote_profile = opt_query(&params, "hasSavedManagedRemoteProfile") == "1";
    let has_saved_managed_remote_profile = server_has_saved_managed_remote_profile || cli_has_saved_managed_remote_profile;

    let config_managed_remote_token = if provider_id == TUNNEL_PROVIDER_CLOUDFLARE {
        state
            .managed_config
            .resolve(&selected_preset_id, hostname.as_deref().unwrap_or(""))
            .await
    } else {
        String::new()
    };

    let runtime_hostname = state.tunnel_runtime.get_runtime_managed_remote_hostname().await;
    let runtime_token = state.tunnel_runtime.get_runtime_managed_remote_token().await;

    // token 优先级
    let token = if !request_token.is_empty() {
        request_token
    } else if !request_tunnel_token.is_empty() {
        request_tunnel_token
    } else if !request_managed_remote_token.is_empty() {
        request_managed_remote_token
    } else if !runtime_hostname.is_empty() && hostname.is_some() && runtime_hostname == hostname.clone().unwrap() && !runtime_token.is_empty() {
        runtime_token.clone()
    } else if !config_managed_remote_token.is_empty() {
        config_managed_remote_token
    } else {
        stored_managed_remote_token
    };

    let doctor_request = json!({
        "mode": mode_filter,
        "hostname": hostname,
        "token": token,
        "tokenProvided": request_token_provided,
        "hostnameProvided": request_hostname_provided,
        "configPath": request_config_path.as_ref().and_then(|p| p.as_ref().and_then(|p| p.to_str()).map(|s| s.to_string())),
        "hasSavedManagedRemoteProfile": has_saved_managed_remote_profile,
    });

    match run_tunnel_doctor(&state, &provider_id, mode_filter.as_deref(), &doctor_request).await {
        Ok(result) => Json(result).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "ok": false, "error": e.message, "code": e.code })),
        )
            .into_response(),
    }
}

/// 运行 tunnel doctor。
async fn run_tunnel_doctor(
    _state: &Arc<AppState>,
    provider_id: &str,
    mode_filter: Option<&str>,
    doctor_request: &Value,
) -> Result<Value, TunnelServiceError> {
    // 获取 capabilities
    let capabilities = match provider_id {
        "ngrok" => crate::tunnels::providers::ngrok::capabilities(),
        _ => crate::tunnels::providers::cloudflare::capabilities(),
    };

    let mode_keys: Vec<String> = capabilities
        .get("modes")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|entry| entry.get("key").and_then(|v| v.as_str()).map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    if let Some(filter) = mode_filter {
        if !mode_keys.contains(&filter.to_string()) {
            return Err(TunnelServiceError::new(
                "mode_unsupported",
                format!("Provider '{}' does not support mode '{}'", provider_id, filter),
            ));
        }
    }

    // provider diagnose
    let diagnosed = crate::tunnels::providers::diagnose(provider_id, doctor_request).await?;

    let provider_checks = diagnosed
        .get("providerChecks")
        .cloned()
        .unwrap_or(Value::Array(vec![]));
    let all_modes = diagnosed
        .get("modes")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let modes: Vec<Value> = match mode_filter {
        Some(filter) => all_modes
            .iter()
            .filter(|entry| {
                entry.get("mode").and_then(|v| v.as_str()) == Some(filter)
            })
            .cloned()
            .collect(),
        None => all_modes,
    };

    Ok(json!({
        "ok": true,
        "provider": provider_id,
        "providerChecks": provider_checks,
        "modes": modes,
    }))
}

/// `GET /api/openchamber/tunnel/providers` — 列出 provider capabilities。
pub async fn tunnel_providers() -> Json<Value> {
    Json(json!({
        "providers": crate::tunnels::providers::list_capabilities(),
    }))
}

/// `GET /api/openchamber/tunnel/status` — 隧道状态。
pub async fn tunnel_status(State(state): State<Arc<AppState>>) -> Response {
    let settings = read_settings();
    let normalized_mode = normalize_tunnel_mode(settings.get("tunnelMode").and_then(|v| v.as_str()));
    let managed_remote_hostname = normalize_managed_remote_tunnel_hostname(settings.get("managedRemoteTunnelHostname"));
    let managed_remote_tunnel_config = state.managed_config.read().await;
    let managed_remote_tunnel_preset_summaries: Vec<Value> = managed_remote_tunnel_config
        .tunnels
        .iter()
        .map(|entry| {
            json!({
                "id": entry.id,
                "name": entry.name,
                "hostname": entry.hostname,
            })
        })
        .collect();
    let has_stored_managed_remote_token = settings
        .get("managedRemoteTunnelToken")
        .and_then(|v| v.as_str())
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    let has_managed_remote_tunnel_token =
        !state.tunnel_runtime.get_runtime_managed_remote_token().await.is_empty()
            || !managed_remote_tunnel_config.tunnels.is_empty()
            || has_stored_managed_remote_token;

    let bootstrap_ttl_ms = match settings.get("tunnelBootstrapTtlMs") {
        Some(Value::Null) | None => None,
        Some(v) => normalize_tunnel_bootstrap_ttl_ms(v.as_i64()),
    };
    let session_ttl_ms = normalize_tunnel_session_ttl_ms(settings.get("tunnelSessionTtlMs").and_then(|v| v.as_i64()));

    let active_sessions = state.tunnel_auth.list_tunnel_sessions();
    let active_provider = state.tunnel_service.resolve_active_provider().await;
    let provider = active_provider
        .clone()
        .unwrap_or_else(|| normalize_tunnel_provider(settings.get("tunnelProvider").and_then(|v| v.as_str())));

    let public_url = state.tunnel_service.get_public_url().await;

    let managed_remote_tunnel_preset_ids: Vec<String> = managed_remote_tunnel_config
        .tunnels
        .iter()
        .map(|entry| entry.id.clone())
        .collect();

    let local_port = (state.get_active_port)();

    if public_url.is_none() {
        return Json(json!({
            "active": false,
            "url": null,
            "mode": normalized_mode,
            "provider": provider,
            "providerMetadata": null,
            "hasManagedRemoteTunnelToken": has_managed_remote_tunnel_token,
            "managedRemoteTunnelHostname": managed_remote_hostname.clone().map(Value::from).unwrap_or(Value::Null),
            "managedRemoteTunnelPresets": managed_remote_tunnel_preset_summaries,
            "managedRemoteTunnelTokenPresetIds": managed_remote_tunnel_preset_ids,
            "hasBootstrapToken": false,
            "bootstrapExpiresAt": null,
            "policy": "tunnel-gated",
            "activeTunnelMode": state.tunnel_auth.get_active_tunnel_mode().map(Value::from).unwrap_or(Value::Null),
            "activeSessions": active_sessions,
            "localPort": local_port,
            "ttlConfig": {
                "bootstrapTtlMs": bootstrap_ttl_ms,
                "sessionTtlMs": session_ttl_ms,
            },
        }))
        .into_response();
    }

    let public_url = public_url.unwrap();

    // active tunnel sync
    let active_normalized_mode = resolve_active_normalized_mode(&state).await;
    let active_tunnel_id = state.tunnel_auth.get_active_tunnel_id();
    let active_tunnel_host = state.tunnel_auth.get_active_tunnel_host();
    let resolved_tunnel_host = resolve_normalized_tunnel_host(&public_url);
    let active_tunnel_mode = state.tunnel_auth.get_active_tunnel_mode();

    let needs_active_tunnel_sync = active_tunnel_id.is_none()
        || active_tunnel_host.is_none()
        || resolved_tunnel_host.is_none()
        || active_tunnel_host != resolved_tunnel_host
        || active_tunnel_mode != Some(active_normalized_mode.clone());

    if needs_active_tunnel_sync {
        let tunnel_id = active_tunnel_id.unwrap_or_else(generate_uuid);
        state
            .tunnel_auth
            .set_active_tunnel(&tunnel_id, &public_url, Some(&active_normalized_mode));
    }

    let (has_bootstrap_token, bootstrap_expires_at) = state.tunnel_auth.get_bootstrap_status();
    let provider_metadata = state.tunnel_service.get_provider_metadata().await;

    Json(json!({
        "active": true,
        "url": public_url,
        "mode": active_normalized_mode,
        "provider": provider,
        "providerMetadata": provider_metadata,
        "hasManagedRemoteTunnelToken": has_managed_remote_tunnel_token,
        "managedRemoteTunnelHostname": managed_remote_hostname.clone().map(Value::from).unwrap_or(Value::Null),
        "managedRemoteTunnelPresets": managed_remote_tunnel_preset_summaries,
        "managedRemoteTunnelTokenPresetIds": managed_remote_tunnel_preset_ids,
        "hasBootstrapToken": has_bootstrap_token,
        "bootstrapExpiresAt": bootstrap_expires_at,
        "policy": "tunnel-gated",
        "activeTunnelMode": active_normalized_mode,
        "activeSessions": state.tunnel_auth.list_tunnel_sessions(),
        "localPort": local_port,
        "ttlConfig": {
            "bootstrapTtlMs": bootstrap_ttl_ms,
            "sessionTtlMs": session_ttl_ms,
        },
    }))
    .into_response()
}

/// 解析活动 tunnel 的 normalized mode。
async fn resolve_active_normalized_mode(state: &Arc<AppState>) -> String {
    let mode = state.tunnel_service.resolve_active_mode().await;
    match mode.as_deref() {
        Some(TUNNEL_MODE_MANAGED_LOCAL) => TUNNEL_MODE_MANAGED_LOCAL.to_string(),
        Some(TUNNEL_MODE_MANAGED_REMOTE) => TUNNEL_MODE_MANAGED_REMOTE.to_string(),
        _ => TUNNEL_MODE_QUICK.to_string(),
    }
}

/// `PUT /api/openchamber/tunnel/managed-remote-token` — upsert managed remote token。
pub async fn tunnel_managed_remote_token(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let preset_id = body.get("presetId").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    let preset_name = body.get("presetName").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    let managed_remote_tunnel_hostname = normalize_managed_remote_tunnel_hostname(body.get("managedRemoteTunnelHostname"));
    let managed_remote_tunnel_token = body.get("managedRemoteTunnelToken").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();

    if preset_id.is_empty()
        || preset_name.is_empty()
        || managed_remote_tunnel_hostname.is_none()
        || managed_remote_tunnel_token.is_empty()
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "ok": false,
                "error": "presetId, presetName, managedRemoteTunnelHostname and managedRemoteTunnelToken are required",
            })),
        )
            .into_response();
    }

    state
        .managed_config
        .upsert(
            &preset_id,
            &preset_name,
            &managed_remote_tunnel_hostname.unwrap(),
            &managed_remote_tunnel_token,
        )
        .await;

    let config = state.managed_config.read().await;
    Json(json!({
        "ok": true,
        "managedRemoteTunnelTokenPresetIds": config.tunnels.iter().map(|e| json!(e.id)).collect::<Vec<_>>(),
    }))
    .into_response()
}

/// `POST /api/openchamber/tunnel/start` — 启动隧道。
pub async fn tunnel_start(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let settings = read_settings();

    // provider 验证
    if let Some(provider_str) = body.get("provider").and_then(|v| v.as_str()) {
        let raw_provider = provider_str.trim().to_lowercase();
        if !raw_provider.is_empty() {
            let caps = match raw_provider.as_str() {
                "ngrok" => crate::tunnels::providers::ngrok::capabilities(),
                "cloudflare" => crate::tunnels::providers::cloudflare::capabilities(),
                _ => {
                    return (
                        StatusCode::UNPROCESSABLE_ENTITY,
                        Json(json!({
                            "ok": false,
                            "error": format!("Unsupported tunnel provider: {}", raw_provider),
                            "code": "provider_unsupported",
                        })),
                    )
                        .into_response();
                }
            };
            let _ = caps; // 验证 provider 存在
        }
    }

    let provider = normalize_tunnel_provider(
        body.get("provider")
            .and_then(|v| v.as_str())
            .or_else(|| settings.get("tunnelProvider").and_then(|v| v.as_str())),
    );

    let mode_input = body
        .get("mode")
        .and_then(|v| v.as_str())
        .or_else(|| settings.get("tunnelMode").and_then(|v| v.as_str()));
    let intent = body.get("intent").and_then(|v| v.as_str()).map(|s| s.trim().to_lowercase());
    let mode = match mode_input {
        Some(m) => m.trim().to_lowercase(),
        None => normalize_tunnel_mode(None),
    };

    // 显式 mode 验证
    if let Some(ModeInput::Explicit(m)) = body.get("mode").and_then(|v| v.as_str()).map(|s| {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            ModeInput::Default
        } else {
            ModeInput::Explicit(trimmed.to_lowercase())
        }
    }) {
        if !is_supported_tunnel_mode(&m) {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({
                    "ok": false,
                    "error": format!("Unsupported tunnel mode: {}", m),
                    "code": "mode_unsupported",
                })),
            )
                .into_response();
        }
    }

    let selected_preset_id = body.get("managedRemoteTunnelPresetId").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    let selected_preset_name = body.get("managedRemoteTunnelPresetName").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    let request_config_path = normalize_optional_path(body.get("configPath"))
        .or_else(|| normalize_optional_path(settings.get("managedLocalTunnelConfigPath")));
    let hostname = resolve_hostname(&HashMap::new(), &body, &settings);

    let runtime_hostname = state.tunnel_runtime.get_runtime_managed_remote_hostname().await;
    let runtime_token = state.tunnel_runtime.get_runtime_managed_remote_token().await;

    let token = resolve_token(
        &body,
        &settings,
        &hostname,
        &selected_preset_id,
        &state.managed_config,
        &runtime_hostname,
        &runtime_token,
    )
    .await;

    // TTL 解析
    let request_connect_ttl_ms = normalize_tunnel_bootstrap_ttl_ms(
        body.get("connectTtlMs").and_then(|v| v.as_i64()),
    );
    let request_session_ttl_ms: Option<i64> = body
        .get("sessionTtlMs")
        .and_then(|v| v.as_i64())
        .map(|v| normalize_tunnel_session_ttl_ms(Some(v)));

    let bootstrap_ttl_ms = request_connect_ttl_ms.or_else(|| {
        match settings.get("tunnelBootstrapTtlMs") {
            Some(Value::Null) | None => None,
            Some(v) => normalize_tunnel_bootstrap_ttl_ms(v.as_i64()),
        }
    });
    let session_ttl_ms = request_session_ttl_ms
        .unwrap_or_else(|| normalize_tunnel_session_ttl_ms(settings.get("tunnelSessionTtlMs").and_then(|v| v.as_i64())));

    // previous tunnel artifacts
    let previous_tunnel_id = state.tunnel_auth.get_active_tunnel_id();
    let previous_mode = state.tunnel_auth.get_active_tunnel_mode();
    let previous_provider = state.tunnel_service.resolve_active_provider().await;
    let previous_url = state.tunnel_service.get_public_url().await;

    // managed remote: 更新 runtime + persist token
    if provider == TUNNEL_PROVIDER_CLOUDFLARE && mode == TUNNEL_MODE_MANAGED_REMOTE {
        state
            .tunnel_runtime
            .set_runtime_managed_remote_hostname(hostname.as_deref().unwrap_or(""))
            .await;
        state
            .tunnel_runtime
            .set_runtime_managed_remote_token(&token)
            .await;

        if !token.is_empty() && hostname.is_some() {
            let id = if !selected_preset_id.is_empty() {
                selected_preset_id.clone()
            } else {
                hostname.clone().unwrap()
            };
            let name = if !selected_preset_name.is_empty() {
                selected_preset_name.clone()
            } else {
                hostname.clone().unwrap()
            };
            state
                .managed_config
                .upsert(&id, &name, &hostname.clone().unwrap(), &token)
                .await;
        }
    }

    // 构建启动请求
    let start_request = json!({
        "provider": provider,
        "mode": mode,
        "intent": intent,
        "configPath": match &request_config_path {
            None => Value::Null,
            Some(None) => Value::Null,
            Some(Some(p)) => json!(p.to_string_lossy()),
        },
        "token": token,
        "hostname": hostname.as_deref().unwrap_or(""),
    });

    let result = match state.tunnel_service.start(&start_request).await {
        Ok(r) => r,
        Err(e) => {
            // 清理
            state.tunnel_runtime.set_controller(None).await;
            state.tunnel_auth.clear_active_tunnel();
            let status = StatusCode::from_u16(e.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            return (status, Json(e.to_json())).into_response();
        }
    };

    // replaced tunnel 检测
    let replaced_tunnel = previous_tunnel_id.is_some()
        && (previous_mode.as_deref() != Some(result.active_mode.as_str())
            || previous_provider.as_deref() != Some(result.provider.as_str())
            || previous_url.as_deref() != Some(result.public_url.as_str()));

    let (mut revoked_bootstrap_count, mut invalidated_session_count) = (0u32, 0u32);
    if replaced_tunnel {
        if let Some(ref prev_id) = previous_tunnel_id {
            let revoked = state.tunnel_auth.revoke_tunnel_artifacts(prev_id);
            revoked_bootstrap_count = revoked.0;
            invalidated_session_count = revoked.1;
        }
    }

    // 设置 active tunnel
    let tunnel_id = match &previous_tunnel_id {
        Some(id) if !replaced_tunnel => id.clone(),
        _ => generate_uuid(),
    };
    state
        .tunnel_auth
        .set_active_tunnel(&tunnel_id, &result.public_url, Some(&result.active_mode));

    // 签发 bootstrap token
    let (bootstrap_token, bootstrap_expires_at) =
        state.tunnel_auth.issue_bootstrap_token(bootstrap_ttl_ms);
    let connect_url = format!(
        "{}/connect?t={}",
        result.public_url.trim_end_matches('/'),
        url_encode(&bootstrap_token)
    );

    let managed_remote_tunnel_config = state.managed_config.read().await;
    let is_cloudflare_provider = result.provider == TUNNEL_PROVIDER_CLOUDFLARE;

    Json(json!({
        "ok": true,
        "url": result.public_url,
        "mode": result.active_mode,
        "provider": result.provider,
        "providerMetadata": result.provider_metadata,
        "managedRemoteTunnelHostname": if is_cloudflare_provider { hostname.clone() } else { None },
        "managedRemoteTunnelTokenPresetIds": if is_cloudflare_provider {
            Value::Array(managed_remote_tunnel_config.tunnels.iter().map(|e| json!(e.id)).collect::<Vec<_>>())
        } else {
            json!([])
        },
        "connectUrl": connect_url,
        "bootstrapExpiresAt": bootstrap_expires_at,
        "replacedTunnel": replaced_tunnel,
        "replaced": if replaced_tunnel {
            json!({
                "mode": previous_mode,
                "provider": previous_provider,
                "url": previous_url,
            })
        } else {
            Value::Null
        },
        "revokedBootstrapCount": revoked_bootstrap_count,
        "invalidatedSessionCount": invalidated_session_count,
        "policy": "tunnel-gated",
        "activeTunnelMode": result.active_mode,
        "activeSessions": state.tunnel_auth.list_tunnel_sessions(),
        "localPort": (state.get_active_port)(),
        "ttlConfig": {
            "bootstrapTtlMs": bootstrap_ttl_ms,
            "sessionTtlMs": session_ttl_ms,
        },
    }))
    .into_response()
}

enum ModeInput {
    Explicit(String),
    Default,
}

/// `POST /api/openchamber/tunnel/stop` — 停止活动隧道。
pub async fn tunnel_stop(State(state): State<Arc<AppState>>) -> Json<Value> {
    let mut revoked_bootstrap_count = 0u32;
    let mut invalidated_session_count = 0u32;

    if let Some(active_tunnel_id) = state.tunnel_auth.get_active_tunnel_id() {
        let revoked = state.tunnel_auth.revoke_tunnel_artifacts(&active_tunnel_id);
        revoked_bootstrap_count = revoked.0;
        invalidated_session_count = revoked.1;
    }

    // 停止 controller (如果有)
    state.tunnel_runtime.stop_controller().await;

    state.tunnel_auth.clear_active_tunnel();

    Json(json!({
        "ok": true,
        "revokedBootstrapCount": revoked_bootstrap_count,
        "invalidatedSessionCount": invalidated_session_count,
    }))
}

/// `GET /connect` — bootstrap token → session 交换 + 302 重定向。
pub async fn connect(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let token = opt_query(&params, "t");
    let settings = read_settings();
    let session_ttl_ms = normalize_tunnel_session_ttl_ms(
        settings
            .get("tunnelSessionTtlMs")
            .and_then(|v| v.as_i64()),
    );

    // 获取 client IP (用于 rate-limit key)
    let client_ip = get_client_ip(&headers);
    let rate_limit_key = get_rate_limit_key(client_ip.as_deref());

    let exchange = state
        .tunnel_auth
        .exchange_bootstrap_token(&token, session_ttl_ms, &rate_limit_key);

    let mut response_headers = HeaderMap::new();
    response_headers.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());

    if !exchange.ok {
        if let Some(ref reason) = exchange.reason {
            if reason == "rate-limited" {
                response_headers.insert(
                    header::RETRY_AFTER,
                    exchange.retry_after.to_string().parse().unwrap_or("60".parse().unwrap()),
                );
                return (
                    StatusCode::TOO_MANY_REQUESTS,
                    response_headers,
                    "Too many attempts. Please try again later.",
                )
                    .into_response();
            }
        }
        return (
            StatusCode::UNAUTHORIZED,
            response_headers,
            "Connection link is invalid or expired.",
        )
            .into_response();
    }

    // 成功: 设置 session cookie + 302 redirect
    if let Some(session_id) = &exchange.session_id {
        let secure = is_secure_request(&headers);
        let max_age = session_ttl_ms / 1000;
        let encoded_session = url_encode(session_id);
        let cookie = build_cookie(
            TUNNEL_SESSION_COOKIE_NAME,
            &encoded_session,
            Some(max_age),
            secure,
        );
        response_headers.insert(
            header::SET_COOKIE,
            cookie.parse().unwrap_or("".parse().unwrap()),
        );
    }

    (
        StatusCode::FOUND,
        response_headers,
        [(header::LOCATION, "/")],
    )
        .into_response()
}

/// 从 headers 获取 client IP。
fn get_client_ip(headers: &HeaderMap) -> Option<String> {
    // x-forwarded-for
    if let Some(forwarded) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        let ip = forwarded.split(',').next()?.trim();
        return Some(strip_ipv4_mapped(ip).to_string());
    }
    None
}

/// 去除 ::ffff: 前缀。
fn strip_ipv4_mapped(ip: &str) -> &str {
    if let Some(stripped) = ip.strip_prefix("::ffff:") {
        stripped
    } else {
        ip
    }
}

/// 判断是否为 HTTPS 请求。
fn is_secure_request(headers: &HeaderMap) -> bool {
    if let Some(forwarded_proto) = headers.get("x-forwarded-proto").and_then(|v| v.to_str().ok()) {
        let first = forwarded_proto.split(',').next().unwrap_or("").trim().to_lowercase();
        return first == "https";
    }
    false
}

/// 简单 URL encode (用于 query params 和 cookie values)。
fn url_encode(s: &str) -> String {
    let mut result = String::new();
    for c in s.chars() {
        match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => result.push(c),
            _ => {
                let bytes = c.to_string().into_bytes();
                for b in bytes {
                    result.push_str(&format!("%{:02X}", b));
                }
            }
        }
    }
    result
}
