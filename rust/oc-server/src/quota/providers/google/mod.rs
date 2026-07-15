//! Google provider sub-module — multi-source (gemini / antigravity) quota fetch。

pub mod api;
pub mod auth;
pub mod transforms;

use serde_json::{json, Value};

use crate::quota::providers::not_configured;
use crate::quota::utils::formatters::{build_result, BuildResultArgs};
use crate::quota::{block_on, http_client};

pub const PROVIDER_ID: &str = "google";
pub const PROVIDER_NAME: &str = "Google";

pub use auth::{resolve_google_auth_sources, resolve_google_oauth_client, DEFAULT_PROJECT_ID};

pub fn is_configured() -> bool {
    resolve_google_auth_sources().is_empty() == false
}

pub async fn fetch_google_quota_async() -> Value {
    let auth_sources = resolve_google_auth_sources();
    if auth_sources.is_empty() {
        return not_configured(PROVIDER_ID, PROVIDER_NAME);
    }

    let mut models = serde_json::Map::new();
    let mut source_errors: Vec<String> = Vec::new();

    for source in auth_sources {
        let now = chrono::Utc::now().timestamp_millis();
        let mut access_token = source.access_token;

        // Refresh token if needed
        if access_token.is_none() || (source.expires.is_some() && source.expires.unwrap_or(0) <= now) {
            let Some(ref rt) = source.refresh_token else {
                source_errors.push(format!("{}: Missing refresh token", source.source_label));
                continue;
            };
            let client = resolve_google_oauth_client(source.source_id);
            match api::refresh_google_access_token(rt, &client.client_id, &client.client_secret).await {
                Some(t) => access_token = Some(t),
                None => {
                    source_errors.push(format!("{}: Failed to refresh OAuth token", source.source_label));
                    continue;
                }
            }
        }

        let Some(at) = access_token else { continue };
        let project_id = source.project_id.as_deref().unwrap_or(DEFAULT_PROJECT_ID);

        let mut merged_any_model = false;

        if source.source_id == "gemini" {
            if let Some(payload) = api::fetch_google_quota_buckets(&http_client(), &at, project_id).await {
                let buckets = payload.get("buckets").and_then(|b| b.as_array()).cloned().unwrap_or_default();
                for bucket in buckets {
                    if let Some(map) = transforms::transform_quota_bucket(&bucket, source.source_id) {
                        for (k, v) in map {
                            models.insert(k, v);
                            merged_any_model = true;
                        }
                    }
                }
            }
        }

        if let Some(payload) = api::fetch_google_models(&http_client(), &at, project_id).await {
            let model_map = payload.get("models").and_then(|m| m.as_object()).cloned().unwrap_or_default();
            for (model_name, model_data) in model_map {
                if let Some(map) = transforms::transform_model_data(&model_name, &model_data, source.source_id) {
                    for (k, v) in map {
                        models.insert(k, v);
                        merged_any_model = true;
                    }
                }
            }
        }

        if !merged_any_model {
            source_errors.push(format!("{}: Failed to fetch models", source.source_label));
        }
    }

    if models.is_empty() {
        let err = source_errors.first().map(|e| e.as_str()).unwrap_or("Failed to fetch models");
        return build_result(BuildResultArgs {
            provider_id: PROVIDER_ID,
            provider_name: PROVIDER_NAME,
            ok: false,
            configured: true,
            usage: None,
            error: Some(err),
        });
    }

    let usage = json!({
        "windows": {},
        "models": Value::Object(models),
    });

    build_result(BuildResultArgs {
        provider_id: PROVIDER_ID,
        provider_name: PROVIDER_NAME,
        ok: true,
        configured: true,
        usage: Some(usage),
        error: None,
    })
}

pub fn fetch_google_quota() -> Value {
    block_on(fetch_google_quota_async())
}

pub fn fetch_google_quota_sync() -> Value {
    fetch_google_quota()
}

pub use fetch_google_quota as fetch_google_quota_pub;
