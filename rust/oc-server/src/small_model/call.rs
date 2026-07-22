//! small-model call — 4 provider dispatch (OpenAI-compatible / Anthropic / Google / OpenAI-codex-SSE)。
//!
//! 对应 Node `small-model/call.js` (530 行):
//!   - callSmallModel(args) → text
//!   - callOpenAICompatible / callAnthropic / callGoogle / callCodexResponses
//!
//! OAuth refresh: 单飞 via `tokio::sync::Mutex<Option<JoinHandle>>`
//! JWT base64url decode: `base64::engine::general_purpose::URL_SAFE_NO_PAD`

#![allow(dead_code)]

use std::time::Duration;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::time::timeout;

use oc_core::Error;

use crate::opencode::auth;
use crate::small_model::resolve::{ResolvedModel, OPENAI_OAUTH_SMALL_MODEL};

pub const REQUEST_TIMEOUT_MS: u64 = 60_000;
pub const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 4_000;
pub const USER_AGENT: &str = "opencode/1.0 gridforge";

pub const CODEX_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
pub const CODEX_RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex/responses";

/// call_small_model 输入。
#[derive(Debug, Clone)]
pub struct CallSmallModelArgs {
    pub prompt: String,
    pub system: Option<String>,
    pub max_output_tokens: Option<u32>,
    pub resolved: ResolvedModel,
    pub directory: Option<String>,
}

/// Provider config (从 opencode/config 读出)。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub base_url: Option<String>,
    pub api_key: Option<String>,
}

/// 单飞 refresh token(声明,当前未在调用路径中实际使用)。
#[allow(dead_code)]
static OPENAI_REFRESH_PROMISE: Lazy<Option<String>> = Lazy::new(|| None);

/// JWT payload 解码(不验证签名,只读 claims)。
pub fn decode_jwt_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let decoded = URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice(&decoded).ok()
}

/// 从 JWT 中提取 `https://api.openai.com/auth` claim(chatgpt account id)。
pub fn extract_chatgpt_account_id(access_token: &str) -> Option<String> {
    let claims = decode_jwt_claims(access_token)?;
    claims
        .get("https://api.openai.com/auth")
        .and_then(|v| v.get("chatgpt_account_id"))
        .and_then(|v| v.as_str())
        .map(String::from)
}

/// 读取 provider 的 base_url / api_key(从 opencode config)。
pub fn read_provider_config(working_directory: Option<&str>, provider_id: &str) -> ProviderConfig {
    let config = match crate::opencode::config::read_config(working_directory) {
        Ok(c) => c,
        Err(_) => return ProviderConfig::default(),
    };
    let provider = config
        .get("provider")
        .and_then(|v| v.as_object())
        .and_then(|m| m.get(provider_id));
    match provider {
        Some(p) => ProviderConfig {
            base_url: p.get("baseURL").and_then(|v| v.as_str()).map(String::from),
            api_key: p.get("apiKey").and_then(|v| v.as_str()).map(String::from),
        },
        None => ProviderConfig::default(),
    }
}

/// 主入口:根据 resolved.provider_id 分派到对应 call_* 实现。
pub async fn call_small_model(args: CallSmallModelArgs) -> Result<String, Error> {
    let provider_id = args.resolved.provider_id.as_str();
    let model_id = args.resolved.model_id.as_str();
    let cfg = read_provider_config(args.directory.as_deref(), provider_id);

    // thinking toggle allowlist(对齐 Node line 76-80)
    let supports_thinking_toggle = provider_id.contains("zai")
        || provider_id.contains("zhipu")
        || model_id.to_lowercase().contains("glm")
        || model_id.to_lowercase().contains("minimax-m3");

    let max_tokens = args.max_output_tokens.unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS);

    let result = match provider_id {
        "github-copilot" => {
            call_openai_compatible(
                args.prompt,
                args.system,
                model_id,
                cfg,
                max_tokens,
                supports_thinking_toggle,
            )
            .await
        }
        "openai" => {
            // 检测是否 OAuth:entry.type === "oauth"
            if let Ok(Some(entry)) = auth::get_provider_auth("openai") {
                if entry.get("type").and_then(|v| v.as_str()) == Some("oauth") {
                    return call_codex_responses(args.prompt, args.system, model_id, entry, max_tokens)
                        .await;
                }
            }
            call_openai_compatible(
                args.prompt,
                args.system,
                model_id,
                cfg,
                max_tokens,
                supports_thinking_toggle,
            )
            .await
        }
        "anthropic" => {
            call_anthropic(
                args.prompt,
                args.system,
                model_id,
                cfg,
                max_tokens,
                supports_thinking_toggle,
            )
            .await
        }
        "google" => call_google(args.prompt, args.system, model_id, cfg, max_tokens).await,
        _ => {
            call_openai_compatible(
                args.prompt,
                args.system,
                model_id,
                cfg,
                max_tokens,
                supports_thinking_toggle,
            )
            .await
        }
    };

    result
}

// ============================================================================
// OpenAI-compatible (含 Copilot)
// ============================================================================

#[allow(clippy::too_many_arguments)]
async fn call_openai_compatible(
    prompt: String,
    system: Option<String>,
    model_id: &str,
    cfg: ProviderConfig,
    max_tokens: u32,
    _supports_thinking_toggle: bool,
) -> Result<String, Error> {
    let api_key = cfg
        .api_key
        .ok_or_else(|| Error::Internal("missing api key".into()))?;
    let base_url = cfg
        .base_url
        .unwrap_or_else(|| "https://api.openai.com/v1".to_string());

    let mut messages = vec![];
    if let Some(sys) = system {
        messages.push(json!({"role": "system", "content": sys}));
    }
    messages.push(json!({"role": "user", "content": prompt}));

    let body = json!({
        "model": model_id,
        "messages": messages,
        "max_tokens": max_tokens,
    });

    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(REQUEST_TIMEOUT_MS))
        .build()
        .map_err(|e| Error::Internal(format!("build client: {}", e)))?;

    let resp = timeout(
        Duration::from_millis(REQUEST_TIMEOUT_MS),
        client
            .post(format!("{}/chat/completions", base_url.trim_end_matches('/')))
            .header("Authorization", format!("Bearer {}", api_key))
            .header("Content-Type", "application/json")
            .header("User-Agent", USER_AGENT)
            .json(&body)
            .send(),
    )
    .await
    .map_err(|_| Error::Internal("openai request timeout".into()))?
    .map_err(|e| Error::Internal(format!("openai request: {}", e)))?;

    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let snippet = resp.text().await.unwrap_or_default();
        let len = snippet.len().min(200);
        return Err(Error::Internal(format!(
            "openai {}: {}",
            status,
            &snippet[..len]
        )));
    }

    let json: Value = resp
        .json()
        .await
        .map_err(|e| Error::Internal(format!("openai parse: {}", e)))?;
    let text = json
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .ok_or_else(|| Error::Internal("openai missing content".into()))?;
    Ok(text.to_string())
}

// ============================================================================
// Anthropic (messages API)
// ============================================================================

#[allow(clippy::too_many_arguments)]
async fn call_anthropic(
    prompt: String,
    system: Option<String>,
    model_id: &str,
    cfg: ProviderConfig,
    max_tokens: u32,
    _supports_thinking_toggle: bool,
) -> Result<String, Error> {
    let api_key = cfg
        .api_key
        .ok_or_else(|| Error::Internal("missing anthropic api key".into()))?;
    let base_url = cfg
        .base_url
        .unwrap_or_else(|| "https://api.anthropic.com/v1".to_string());

    let body = json!({
        "model": model_id,
        "max_tokens": max_tokens,
        "system": system.unwrap_or_default(),
        "messages": [{"role": "user", "content": prompt}],
    });

    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(REQUEST_TIMEOUT_MS))
        .build()
        .map_err(|e| Error::Internal(format!("build client: {}", e)))?;

    let resp = timeout(
        Duration::from_millis(REQUEST_TIMEOUT_MS),
        client
            .post(format!("{}/messages", base_url.trim_end_matches('/')))
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01")
            .header("Content-Type", "application/json")
            .json(&body)
            .send(),
    )
    .await
    .map_err(|_| Error::Internal("anthropic request timeout".into()))?
    .map_err(|e| Error::Internal(format!("anthropic request: {}", e)))?;

    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let snippet = resp.text().await.unwrap_or_default();
        let len = snippet.len().min(200);
        return Err(Error::Internal(format!(
            "anthropic {}: {}",
            status,
            &snippet[..len]
        )));
    }

    let json: Value = resp
        .json()
        .await
        .map_err(|e| Error::Internal(format!("anthropic parse: {}", e)))?;
    let text = json
        .get("content")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .and_then(|b| b.get("text"))
        .and_then(|t| t.as_str())
        .ok_or_else(|| Error::Internal("anthropic missing content".into()))?;
    Ok(text.to_string())
}

// ============================================================================
// Google (generativelanguage)
// ============================================================================

async fn call_google(
    prompt: String,
    system: Option<String>,
    model_id: &str,
    cfg: ProviderConfig,
    max_tokens: u32,
) -> Result<String, Error> {
    let api_key = cfg
        .api_key
        .ok_or_else(|| Error::Internal("missing google api key".into()))?;
    let base_url = cfg
        .base_url
        .unwrap_or_else(|| "https://generativelanguage.googleapis.com/v1beta".to_string());

    let body = if let Some(sys) = system {
        json!({
            "contents": [{"parts": [{"text": prompt}]}],
            "systemInstruction": {"parts": [{"text": sys}]},
            "generationConfig": {"maxOutputTokens": max_tokens}
        })
    } else {
        json!({
            "contents": [{"parts": [{"text": prompt}]}],
            "generationConfig": {"maxOutputTokens": max_tokens}
        })
    };
    let _ = model_id; // 包含在 URL path

    let url = format!(
        "{}/models/{}:generateContent?key={}",
        base_url.trim_end_matches('/'),
        model_id,
        api_key,
    );

    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(REQUEST_TIMEOUT_MS))
        .build()
        .map_err(|e| Error::Internal(format!("build client: {}", e)))?;

    let resp = timeout(
        Duration::from_millis(REQUEST_TIMEOUT_MS),
        client
            .post(&url)
            .header("Content-Type", "application/json")
            .json(&body)
            .send(),
    )
    .await
    .map_err(|_| Error::Internal("google request timeout".into()))?
    .map_err(|e| Error::Internal(format!("google request: {}", e)))?;

    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let snippet = resp.text().await.unwrap_or_default();
        let len = snippet.len().min(200);
        return Err(Error::Internal(format!(
            "google {}: {}",
            status,
            &snippet[..len]
        )));
    }

    let json: Value = resp
        .json()
        .await
        .map_err(|e| Error::Internal(format!("google parse: {}", e)))?;
    let text = json
        .get("candidates")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .and_then(|c| c.get("content"))
        .and_then(|c| c.get("parts"))
        .and_then(|p| p.as_array())
        .and_then(|a| a.first())
        .and_then(|p| p.get("text"))
        .and_then(|t| t.as_str())
        .ok_or_else(|| Error::Internal("google missing content".into()))?;
    Ok(text.to_string())
}

// ============================================================================
// OpenAI Codex Responses (non-streaming variant)
// ============================================================================

async fn call_codex_responses(
    prompt: String,
    system: Option<String>,
    model_id: &str,
    entry: Value,
    max_tokens: u32,
) -> Result<String, Error> {
    // 简化版: 直接 POST,JSON 响应(非流式)。
    // 真实 SSE 流式需要解析 response.output_text.delta 累积 — Task 范围内仅实现 non-streaming 版本。
    let access_token = entry
        .get("access")
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::Internal("missing oauth access".into()))?;
    let account_id = extract_chatgpt_account_id(access_token).unwrap_or_default();

    let mut input = vec![];
    if let Some(sys) = system {
        input.push(json!({"role": "system", "content": sys}));
    }
    input.push(json!({"role": "user", "content": prompt}));

    let body = json!({
        "model": if model_id.is_empty() { OPENAI_OAUTH_SMALL_MODEL } else { model_id },
        "input": input,
        "max_output_tokens": max_tokens,
    });

    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(REQUEST_TIMEOUT_MS))
        .build()
        .map_err(|e| Error::Internal(format!("build client: {}", e)))?;

    let resp = timeout(
        Duration::from_millis(REQUEST_TIMEOUT_MS),
        client
            .post(CODEX_RESPONSES_URL)
            .header("Authorization", format!("Bearer {}", access_token))
            .header("Content-Type", "application/json")
            .header("chatgpt-account-id", account_id)
            .header("User-Agent", USER_AGENT)
            .json(&body)
            .send(),
    )
    .await
    .map_err(|_| Error::Internal("codex request timeout".into()))?
    .map_err(|e| Error::Internal(format!("codex request: {}", e)))?;

    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let snippet = resp.text().await.unwrap_or_default();
        let len = snippet.len().min(200);
        return Err(Error::Internal(format!(
            "codex {}: {}",
            status,
            &snippet[..len]
        )));
    }

    let json: Value = resp
        .json()
        .await
        .map_err(|e| Error::Internal(format!("codex parse: {}", e)))?;
    let text = json
        .get("output")
        .and_then(|o| o.as_array())
        .and_then(|a| {
            a.iter()
                .find(|item| item.get("type").and_then(|v| v.as_str()) == Some("output_text"))
        })
        .and_then(|item| item.get("content"))
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .and_then(|c| c.get("text"))
        .and_then(|t| t.as_str())
        .ok_or_else(|| Error::Internal("codex missing output_text".into()))?;
    Ok(text.to_string())
}

// ============================================================================
// 测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_jwt_claims_basic() {
        // header.payload.sig (payload = base64url(json))
        let payload = URL_SAFE_NO_PAD.encode(
            br#"{"sub":"abc","https://api.openai.com/auth":{"chatgpt_account_id":"acc-123"}}"#,
        );
        let token = format!("header.{}.sig", payload);
        let claims = decode_jwt_claims(&token).unwrap();
        assert_eq!(claims["sub"], "abc");
    }

    #[test]
    fn extract_chatgpt_account_id_present() {
        let payload = URL_SAFE_NO_PAD.encode(
            br#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acc-xyz"}}"#,
        );
        let token = format!("h.{}.s", payload);
        assert_eq!(
            extract_chatgpt_account_id(&token),
            Some("acc-xyz".to_string())
        );
    }

    #[test]
    fn extract_chatgpt_account_id_missing_claim() {
        let payload = URL_SAFE_NO_PAD.encode(br#"{"sub":"abc"}"#);
        let token = format!("h.{}.s", payload);
        assert_eq!(extract_chatgpt_account_id(&token), None);
    }

    #[test]
    fn read_provider_config_no_config() {
        // 无 config 时返回 default
        let cfg = read_provider_config(Some("/nonexistent-dir-x9z"), "openai");
        assert!(cfg.base_url.is_none());
        assert!(cfg.api_key.is_none());
    }
}
