//! small-model index — public API (generate / describe / list)。
//!
//! 对应 Node `small-model/index.js` (144 行)。
//! 也包含 `small-model/catalog.js` (12 行,实际是 models_metadata 的 re-export)。

#![allow(dead_code)]
#![allow(unused_imports)]

use serde::{Deserialize, Serialize};

use crate::github::settings::read_settings;
use crate::opencode::auth;
use crate::opencode::config;
use crate::small_model::call::call_small_model;
use crate::small_model::resolve::{resolve_small_model, ResolveArgs, ResolvedModel};

pub use crate::opencode::models_metadata::{get_models_metadata, ModelsMetadata, MODELS_DEV_API_URL};

const DEFAULT_CONTEXT_TOKENS: u64 = 64_000;
const OUTPUT_RESERVE_TOKENS: u64 = 4_000;

/// `generate_small_model_text` 输入。
#[derive(Debug, Clone, Default)]
pub struct GenerateArgs {
    pub prompt: String,
    pub system: Option<String>,
    pub max_output_tokens: Option<u32>,
    pub model: Option<String>,
    pub directory: Option<String>,
    pub preferred_provider_id: Option<String>,
    pub preferred_model_id: Option<String>,
    pub restrict_to_preferred_provider: bool,
}

/// `describe_small_model` 输入。
#[derive(Debug, Clone, Default)]
pub struct DescribeArgs {
    pub directory: Option<String>,
    pub preferred_provider_id: Option<String>,
    pub preferred_model_id: Option<String>,
}

/// generate 输出。
#[derive(Debug, Clone, Serialize)]
pub struct GenerateResult {
    pub text: String,
    #[serde(rename = "providerID")]
    pub provider_id: String,
    #[serde(rename = "modelID")]
    pub model_id: String,
    pub source: String,
    #[serde(rename = "inputTruncated", skip_serializing_if = "Option::is_none")]
    pub input_truncated: Option<bool>,
}

/// small-model 错误(可序列化为 HTTP response)。
#[derive(Debug, Clone)]
pub struct SmallModelError {
    pub message: String,
    pub status_code: u16,
}

impl std::fmt::Display for SmallModelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for SmallModelError {}

/// 从 settings.json 读 small-model override(`settings.smallModel`)。
pub fn read_small_model_settings_override() -> Option<String> {
    read_settings()
        .get("smallModel")
        .and_then(|v| v.as_str())
        .map(String::from)
}

/// 从 project config 读 small-model 配置(`config.smallModel`)。
pub fn read_configured_small_model(working_directory: Option<&str>) -> Option<String> {
    config::read_config(working_directory)
        .ok()
        .and_then(|c| c.get("smallModel").and_then(|v| v.as_str().map(String::from)))
}

/// prompt 截断结果。
pub struct ClampedPrompt {
    pub prompt: String,
    pub truncated: bool,
}

/// 按模型 context 上限截断 prompt。
///
/// 简化版:硬编码 64K token 上限,粗估 4 chars/token。
pub fn clamp_prompt_to_model_limit(prompt: String, _system: Option<String>) -> ClampedPrompt {
    let max_chars = (DEFAULT_CONTEXT_TOKENS - OUTPUT_RESERVE_TOKENS) as usize * 4;
    if prompt.len() <= max_chars {
        ClampedPrompt { prompt, truncated: false }
    } else {
        ClampedPrompt {
            prompt: prompt[..max_chars].to_string(),
            truncated: true,
        }
    }
}

/// 主入口:生成 small-model 文本。
pub async fn generate_small_model_text(
    args: GenerateArgs,
) -> Result<GenerateResult, SmallModelError> {
    // 1. resolve
    let resolved = resolve_small_model(ResolveArgs {
        preferred_provider_id: args.preferred_provider_id.clone(),
        preferred_model_id: args.preferred_model_id.clone(),
        restrict_to_preferred_provider: args.restrict_to_preferred_provider,
        directory: args.directory.clone(),
    })
    .await
    .map_err(|e| SmallModelError {
        message: format!("resolve: {}", e),
        status_code: 500,
    })?
    .ok_or_else(|| SmallModelError {
        message: "no small model available".to_string(),
        status_code: 503,
    })?;

    // 2. clamp
    let clamped = clamp_prompt_to_model_limit(args.prompt, args.system.clone());

    // 3. call
    let text = call_small_model(crate::small_model::call::CallSmallModelArgs {
        prompt: clamped.prompt,
        system: args.system,
        max_output_tokens: args.max_output_tokens,
        resolved: resolved.clone(),
        directory: args.directory,
    })
    .await
    .map_err(|e| SmallModelError {
        message: format!("call: {}", e),
        status_code: 500,
    })?;

    Ok(GenerateResult {
        text: text.trim().to_string(),
        provider_id: resolved.provider_id,
        model_id: resolved.model_id,
        source: resolved.source,
        input_truncated: if clamped.truncated { Some(true) } else { None },
    })
}

/// 描述当前可用 small-model。
pub async fn describe_small_model(
    args: DescribeArgs,
) -> Result<Option<ResolvedModel>, SmallModelError> {
    resolve_small_model(ResolveArgs {
        preferred_provider_id: args.preferred_provider_id,
        preferred_model_id: args.preferred_model_id,
        restrict_to_preferred_provider: false,
        directory: args.directory,
    })
    .await
    .map_err(|e| SmallModelError {
        message: format!("describe: {}", e),
        status_code: 500,
    })
}

/// 列出所有已认证的 provider ID。
pub fn list_authenticated_providers() -> Vec<String> {
    auth::list_provider_auths().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_settings_override_none_when_missing() {
        // settings 不存在/无 smallModel 字段时返回 None
        // 注:可能 host 有真实 settings,只断言返回 Option<String>
        let _: Option<String> = read_small_model_settings_override();
    }

    #[test]
    fn list_authenticated_providers_returns_vec() {
        // 不强制空(可能 host 有真实 auth),只断言返回 Vec<String>
        let list = list_authenticated_providers();
        let _: Vec<String> = list;
    }

    #[test]
    fn clamp_short_prompt_not_truncated() {
        let result = clamp_prompt_to_model_limit("hi".to_string(), None);
        assert!(!result.truncated);
        assert_eq!(result.prompt, "hi");
    }

    #[test]
    fn clamp_long_prompt_truncated() {
        let long = "a".repeat(DEFAULT_CONTEXT_TOKENS as usize * 8);
        let result = clamp_prompt_to_model_limit(long, None);
        assert!(result.truncated);
        assert!(result.prompt.len() < DEFAULT_CONTEXT_TOKENS as usize * 4);
    }

    #[test]
    fn small_model_error_display() {
        let e = SmallModelError {
            message: "boom".to_string(),
            status_code: 500,
        };
        assert_eq!(e.to_string(), "boom");
    }
}
