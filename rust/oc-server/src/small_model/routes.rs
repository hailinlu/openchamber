//! small-model 路由 — 2 个 axum handlers。
//!
//! 对应 Node `small-model/routes.js` (44 行):
//!   - GET  /api/small-model            → { available, model, authenticatedProviders }
//!   - POST /api/small-model/generate   → { text, providerID, modelID, source, inputTruncated? }

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::small_model::index::{
    describe_small_model, generate_small_model_text, list_authenticated_providers, DescribeArgs,
    GenerateArgs,
};
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct SmallModelQuery {
    pub directory: Option<String>,
    #[serde(rename = "providerID")]
    pub provider_id: Option<String>,
    #[serde(rename = "modelID")]
    pub model_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SmallModelGenerateBody {
    pub prompt: String,
    pub system: Option<String>,
    #[serde(rename = "maxOutputTokens")]
    pub max_output_tokens: Option<u32>,
    pub model: Option<String>,
    pub directory: Option<String>,
    #[serde(rename = "preferredProviderID")]
    pub preferred_provider_id: Option<String>,
    #[serde(rename = "preferredModelID")]
    pub preferred_model_id: Option<String>,
    #[serde(default)]
    #[serde(rename = "restrictToPreferredProvider")]
    pub restrict_to_preferred_provider: bool,
}

/// GET /api/small-model — 描述当前可用 small-model。
pub async fn get_small_model(
    State(_state): State<Arc<AppState>>,
    Query(query): Query<SmallModelQuery>,
) -> Response {
    match describe_small_model(DescribeArgs {
        directory: query.directory,
        preferred_provider_id: query.provider_id,
        preferred_model_id: query.model_id,
    })
    .await
    {
        Ok(resolved) => (
            StatusCode::OK,
            Json(json!({
                "available": resolved.is_some(),
                "model": resolved,
                "authenticatedProviders": list_authenticated_providers(),
            })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::from_u16(e.status_code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            Json(json!({ "error": e.message })),
        )
            .into_response(),
    }
}

/// POST /api/small-model/generate — 生成文本。
pub async fn post_small_model_generate(
    State(_state): State<Arc<AppState>>,
    Json(body): Json<SmallModelGenerateBody>,
) -> Response {
    match generate_small_model_text(GenerateArgs {
        prompt: body.prompt,
        system: body.system,
        max_output_tokens: body.max_output_tokens,
        model: body.model,
        directory: body.directory,
        preferred_provider_id: body.preferred_provider_id,
        preferred_model_id: body.preferred_model_id,
        restrict_to_preferred_provider: body.restrict_to_preferred_provider,
    })
    .await
    {
        Ok(result) => (StatusCode::OK, Json(result)).into_response(),
        Err(e) => {
            let code =
                StatusCode::from_u16(e.status_code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            if code.as_u16() >= 500 {
                tracing::error!("small-model generate failed: {}", e.message);
            }
            (code, Json(json!({ "error": e.message }))).into_response()
        }
    }
}
