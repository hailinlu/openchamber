//! Quota HTTP endpoints (7 endpoints, `/api/quota/*`).
//!
//! 对应 Node `quota/routes.js`:
//!   - GET    /api/quota/providers
//!   - GET    /api/quota/credentials/:providerId
//!   - PUT    /api/quota/credentials/:providerId          (16 KB body limit)
//!   - POST   /api/quota/credentials/:providerId/validate
//!   - POST   /api/quota/credentials/:providerId/import   (cursor only)
//!   - DELETE /api/quota/credentials/:providerId
//!   - GET    /api/quota/:providerId

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use serde_json::{json, Value};

use crate::error::ApiResult;
use crate::quota::credentials::{
    delete_managed_credential, get_managed_credential_status, normalize, read_managed_credential,
    write_managed_credential,
};
use crate::quota::providers::{
    cursor, fetch_quota_for_provider, list_configured_quota_providers, opencode_go, ollama_cloud,
};

use crate::state::AppState;

/// 是否是 managed credential provider。
fn is_managed_provider(id: &str) -> bool {
    matches!(id, "opencode-go" | "ollama-cloud" | "cursor")
}

/// `GET /api/quota/providers` — 返回 `{providers: [...]}`。
pub async fn list_providers(State(_state): State<Arc<AppState>>) -> ApiResult<Json<Value>> {
    Ok(Json(json!({
        "providers": list_configured_quota_providers()
    })))
}

/// `GET /api/quota/credentials/:providerId` — masked status。
pub async fn get_credential_status(
    State(_state): State<Arc<AppState>>,
    Path(provider_id): Path<String>,
) -> ApiResult<Json<Value>> {
    if !is_managed_provider(&provider_id) {
        return Err(unsupported_provider());
    }
    Ok(Json(get_managed_credential_status(&provider_id)))
}

/// `PUT /api/quota/credentials/:providerId` — 写并校验(16KB body limit)。
///
/// 我们用 axum 默认 `Json` extractor (没有显式 16KB 限制 — 接近 Node 默认)。
/// 实际 limit 通过 `RequestBodyLimitLayer` 应在外层 middleware 处理。
pub async fn put_credential(
    State(_state): State<Arc<AppState>>,
    Path(provider_id): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    if !is_managed_provider(&provider_id) {
        return Err(unsupported_provider());
    }

    let normalized = normalize(&provider_id, body).ok_or_else(|| {
        crate::error::ApiError(oc_core::Error::BadRequest("Invalid credential".to_string()))
    })?;

    // 验证 (run validators)
    if let Err(msg) = run_validator(&provider_id, &normalized).await {
        return Err(oc_core::Error::BadRequest(msg).into());
    }

    write_managed_credential(&provider_id, json!(normalized))
        .map_err(|e| oc_core::Error::BadRequest(e).into())
        .map(Json)
}

/// 验证 (Node `validators[providerId]`) — 同步实现走 `block_on`。
async fn run_validator(provider_id: &str, credential: &Value) -> Result<(), String> {
    match provider_id {
        "opencode-go" => {
            // fetchOpenCodeGoUsage 也是 returning windows; throw 错误即失败
            let v = credential.clone();
            let r = crate::quota::block_on(async move {
                opencode_go::fetch_open_code_go_usage_inner(&v).await.map(|_| ())
            });
            r
        }
        "ollama-cloud" => {
            let v = credential.clone();
            let r = crate::quota::block_on(async move {
                ollama_cloud::fetch_ollama_cloud_usage_inner(&v).await.map(|_| ())
            });
            r
        }
        "cursor" => cursor::validate_cursor_credential(credential).await,
        _ => Ok(()),
    }
}

/// `POST /api/quota/credentials/:providerId/validate` — 用 stored credential 重新验证。
pub async fn validate_credential(
    State(_state): State<Arc<AppState>>,
    Path(provider_id): Path<String>,
) -> ApiResult<Json<Value>> {
    if !is_managed_provider(&provider_id) {
        return Err(unsupported_provider());
    }
    let Some(cred) = read_managed_credential(&provider_id) else {
        return Err(crate::error::ApiError(oc_core::Error::NotFound(
            "NOT_CONFIGURED: Not configured".to_string(),
        )));
    };
    run_validator(&provider_id, &cred).await.map_err(|e| crate::error::ApiError(oc_core::Error::BadRequest(e)))?;
    Ok(Json(json!({ "valid": true })))
}

/// `POST /api/quota/credentials/:providerId/import` — only `cursor`。
pub async fn import_credential(
    State(_state): State<Arc<AppState>>,
    Path(provider_id): Path<String>,
) -> ApiResult<Json<Value>> {
    if !is_managed_provider(&provider_id) {
        return Err(unsupported_provider());
    }
    if provider_id != "cursor" {
        return Err(not_importable());
    }
    let v = cursor::import_cursor_credential().await.map_err(oc_core::Error::BadRequest)?;
    Ok(Json(v))
}

/// `DELETE /api/quota/credentials/:providerId`。
pub async fn delete_credential(
    State(_state): State<Arc<AppState>>,
    Path(provider_id): Path<String>,
) -> ApiResult<Json<Value>> {
    if !is_managed_provider(&provider_id) {
        return Err(unsupported_provider());
    }
    delete_managed_credential(&provider_id).map_err(|e| crate::error::ApiError(oc_core::Error::BadRequest(e)))?;
    Ok(Json(json!({ "configured": false })))
}

/// `GET /api/quota/:providerId` — fetch_quota。
pub async fn fetch_quota(
    State(_state): State<Arc<AppState>>,
    Path(provider_id): Path<String>,
) -> ApiResult<Json<Value>> {
    if provider_id.is_empty() {
        return Err(oc_core::Error::BadRequest("Provider ID is required".to_string()).into());
    }
    // 同步执行 fetch_quota_for_provider(已经在 tokio runtime 中)
    let result = fetch_quota_for_provider(&provider_id);
    Ok(Json(result))
}

// ---------------------------------------------------------------------------
// Error helpers
// ---------------------------------------------------------------------------

fn unsupported_provider() -> crate::error::ApiError {
    // Node shape: `{ "code": "UNSUPPORTED_PROVIDER", "error": "..." }`
    oc_core::Error::NotFound("UNSUPPORTED_PROVIDER: Unsupported credential provider".to_string()).into()
}

fn not_importable() -> crate::error::ApiError {
    oc_core::Error::NotFound("IMPORT_UNAVAILABLE: Import unavailable".to_string()).into()
}

// Make sure ApiResult/Json/IntoResponse are used.
#[allow(dead_code)]
fn _ensure_imports_compile(
    h: HeaderMap,
) -> impl IntoResponse {
    (StatusCode::OK, Json(json!({})))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::{delete, get, post};
    use clap::Parser;
    use tower::ServiceExt;

    fn test_state() -> Arc<AppState> {
        let config = Config::try_parse_from(["oc-server"]).unwrap();
        Arc::new(AppState::new(
            config,
            "http://127.0.0.1:4096".to_string(),
            "Basic test".to_string(),
        ))
    }

    fn build_router(state: Arc<AppState>) -> axum::Router {
        axum::Router::new()
            .route("/api/quota/providers", get(list_providers))
            .route("/api/quota/credentials/{providerId}", get(get_credential_status).delete(delete_credential))
            .route("/api/quota/credentials/{providerId}/validate", post(validate_credential))
            .route("/api/quota/credentials/{providerId}/import", post(import_credential))
            .route("/api/quota/{providerId}", get(fetch_quota))
            .with_state(state)
    }

    #[tokio::test]
    async fn list_providers_returns_ok() {
        let state = test_state();
        let resp = list_providers(State(state)).await.into_response();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn unsupported_provider_404_on_status() {
        let state = test_state();
        let resp = get_credential_status(State(state), Path("nope".to_string()))
            .await
            .into_response();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn unsupported_provider_404_on_delete() {
        let state = test_state();
        let resp = delete_credential(State(state), Path("nope".to_string()))
            .await
            .into_response();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn unsupported_provider_404_on_import() {
        let state = test_state();
        let resp = import_credential(State(state), Path("nope".to_string()))
            .await
            .into_response();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn non_cursor_import_returns_404() {
        // 浏览器无 is_managed_provider 但需要传一个支持的 provider。
        let state = test_state();
        // Use "opencode-go" as a valid managed provider id but verify that
        // the import logic detects non-cursor and returns 404.
        let resp = import_credential(State(state), Path("opencode-go".to_string()))
            .await
            .into_response();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn fetch_quota_unknown_provider_returns_not_found_shape() {
        let state = test_state();
        let resp = fetch_quota(State(state), Path("this-does-not-exist".to_string()))
            .await
            .unwrap()
            .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        // body should be a JSON with ok=false and configured=false
        let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["ok"], json!(false));
        assert_eq!(v["configured"], json!(false));
        assert_eq!(v["error"], json!("Unsupported provider"));
    }

    #[tokio::test]
    async fn put_credential_unsupported_provider_404() {
        let state = test_state();
        let resp = put_credential(
            State(state),
            Path("nope".to_string()),
            Json(json!({"workspaceId": "x", "authCookie": "y"})),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn validate_no_stored_returns_404() {
        let state = test_state();
        let resp = validate_credential(State(state), Path("opencode-go".to_string()))
            .await
            .into_response();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn router_smoke() {
        let state = test_state();
        let router = build_router(state);
        let req = Request::builder()
            .uri("/api/quota/providers")
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
