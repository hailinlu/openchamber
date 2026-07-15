//! TTS 服务 — 调用 OpenAI `POST /v1/audio/speech`。
//!
//! 对应 Node `tts/service.js`:
//!   - `ttsService.isAvailable()` — env `OPENAI_API_KEY` 或 auth file 命中即 true
//!   - `ttsService.generateSpeechStream(opts)` — 调 OpenAI TTS, 返回 MP3 bytes
//!
//! Node 使用 `openai` SDK, Rust 直接 `reqwest` + `serde_json::json!` 拼请求体:
//!   - 默认 OpenAI: `{ model, voice, input, speed, instructions?, response_format: "mp3" }`
//!   - 自定义 baseURL (OpenAI-compatible): `{ model, voice, input, speed }` (省略
//!     `instructions` 和 `response_format`, 部分兼容服务器不支持)

use std::sync::Arc;

use serde::Serialize;
use serde_json::json;

use crate::opencode::auth::read_auth_file;

/// OpenAI TTS 标准 voices (与 Node `TTS_VOICES` 完全一致)。
pub const TTS_VOICES: &[&str] = &[
    "alloy", "ash", "ballad", "coral", "echo", "fable",
    "nova", "onyx", "sage", "shimmer", "verse", "marin", "cedar",
];

/// `generateSpeechStream` 返回值。
#[derive(Debug)]
pub struct SpeechResult {
    pub buffer: Vec<u8>,
    pub content_type: String,
}

/// `generateSpeechStream` 入参 (Option 字段缺失时用默认)。
#[derive(Debug, Default)]
pub struct SpeechOptions {
    pub text: String,
    pub voice: Option<String>,
    pub model: Option<String>,
    pub speed: Option<f64>,
    pub instructions: Option<String>,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
}

/// TTS 服务单例 (stateless, 但保持与 Node 同款构造模式)。
#[derive(Clone, Default)]
pub struct TtsService {
    _private: Arc<()>,
}

impl TtsService {
    pub fn new() -> Self {
        Self {
            _private: Arc::new(()),
        }
    }

    /// TTS 是否可用: env `OPENAI_API_KEY` 存在 OR auth file 有 openai/codex/chatgpt。
    ///
    /// 对应 Node `TTSService.isAvailable()` (走 `_getClient()`)。
    pub fn is_available(&self) -> bool {
        get_openai_api_key().is_some()
    }

    /// 生成语音 (返回完整 buffer + content type)。
    pub async fn generate_speech_stream(
        &self,
        opts: SpeechOptions,
    ) -> Result<SpeechResult, String> {
        let normalized = super::base_url::normalize_custom_openai_base_url(
            opts.base_url.as_deref().unwrap_or(""),
        )?;

        let text = opts.text.trim();
        if text.is_empty() {
            return Err("Text is required for TTS".to_string());
        }

        // 解析 api_key / baseURL → 决定是否使用客户端提供的覆盖
        let (effective_api_key, using_client_override) = if normalized.is_some() || opts.api_key.is_some() {
            let key = opts
                .api_key
                .clone()
                .filter(|k| !k.is_empty())
                .or_else(get_openai_api_key)
                .unwrap_or_else(|| "not-required".to_string());
            (key, true)
        } else {
            match get_openai_api_key() {
                Some(k) => (k, false),
                None => {
                    return Err(
                        "TTS service not available. Configure OpenAI in OpenCode, provide an API key, or set a custom server URL in settings."
                            .to_string(),
                    );
                }
            }
        };

        // 构造请求 URL
        let request_url = if let Some(ref base) = normalized {
            format!("{}/audio/speech", base.trim_end_matches('/'))
        } else {
            "https://api.openai.com/v1/audio/speech".to_string()
        };

        // 构造 body
        let voice = opts
            .voice
            .clone()
            .unwrap_or_else(|| "coral".to_string());
        let model = opts
            .model
            .clone()
            .unwrap_or_else(|| "gpt-4o-mini-tts".to_string());
        let speed = opts.speed.unwrap_or(1.0);

        let mut body = json!({
            "model": model,
            "voice": voice,
            "input": text,
            "speed": speed,
        });

        if normalized.is_none() {
            // 官方 OpenAI 路径: 带 instructions + 强制 mp3
            if let Some(instr) = opts.instructions.as_ref().filter(|s| !s.is_empty()) {
                body["instructions"] = json!(instr);
            }
            body["response_format"] = json!("mp3");
        }
        // normalized != None (OpenAI-compatible) → 仅发送安全子集

        let _ = using_client_override; // 当前仅用于决策; 保持显式分支便于日后扩展

        tracing::info!(
            "[TTSService] Generating speech — model: {}, voice: {}, baseURL: {}",
            body["model"],
            body["voice"],
            normalized.as_deref().unwrap_or("(openai)")
        );

        let client = reqwest::Client::new();
        let response = client
            .post(&request_url)
            .header("Authorization", format!("Bearer {}", effective_api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("Failed to generate speech: {}", e))?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(format!(
                "Failed to generate speech: HTTP {} — {}",
                status, text
            ));
        }

        let buffer = response
            .bytes()
            .await
            .map_err(|e| format!("Failed to generate speech: read body: {}", e))?
            .to_vec();

        Ok(SpeechResult {
            buffer,
            content_type: "audio/mpeg".to_string(),
        })
    }
}

/// 按优先级读取 OpenAI API key:
///   1. `OPENAI_API_KEY` env
///   2. auth file (string format, 整个 entry 是 token)
///   3. auth file `.access` (OAuth)
///   4. auth file `.token`
///
/// 对应 Node `getOpenAIApiKey` (`service.js` line 18-48)。
pub fn get_openai_api_key() -> Option<String> {
    // 1. env
    if let Ok(env_key) = std::env::var("OPENAI_API_KEY") {
        let trimmed = env_key.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }

    // 2-4. auth file (openai / codex / chatgpt)
    let auth = match read_auth_file() {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("[TTSService] Failed to read auth file: {}", e);
            return None;
        }
    };

    let entry = auth
        .get("openai")
        .or_else(|| auth.get("codex"))
        .or_else(|| auth.get("chatgpt"))?;

    if let Some(s) = entry.as_str() {
        return Some(s.to_string());
    }
    if let Some(obj) = entry.as_object() {
        if let Some(v) = obj.get("access").and_then(|v| v.as_str()) {
            return Some(v.to_string());
        }
        if let Some(v) = obj.get("token").and_then(|v| v.as_str()) {
            return Some(v.to_string());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// 序列化 (供 openapi 输出 / log)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct TtsVoicesResponse {
    pub available: bool,
    pub voices: Vec<&'static str>,
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opencode::auth::tests as auth_tests;
    use serde_json::json;

    /// 用 base_url 的 TEST_LOCK 保证 env 串行。
    /// NOTE: auth_tests::TEST_LOCK 由 set_temp_home() 内部获取, 此处不重复拿。
    fn lock_auth_env() -> std::sync::MutexGuard<'static, ()> {
        crate::tts::base_url::tests::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 清空 env `OPENAI_API_KEY`, 返回 guard 在 drop 时还原。
    struct ApiKeyGuard {
        prev: Option<String>,
    }
    impl Drop for ApiKeyGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var("OPENAI_API_KEY", v),
                None => std::env::remove_var("OPENAI_API_KEY"),
            }
        }
    }
    fn clear_api_key_env() -> ApiKeyGuard {
        let prev = std::env::var("OPENAI_API_KEY").ok();
        std::env::remove_var("OPENAI_API_KEY");
        ApiKeyGuard { prev }
    }

    #[test]
    fn env_key_wins_over_auth_file() {
        let _base_lock = lock_auth_env();
        let (_home, _home_lock) = auth_tests::set_temp_home();
        let _key_guard = clear_api_key_env();
        std::env::set_var("OPENAI_API_KEY", "sk-from-env");

        // 同时写一份 auth file 的 openai token, env 应当优先
        crate::opencode::auth::write_auth_file(&json!({
            "openai": { "access": "sk-from-auth" }
        }))
        .unwrap();

        let key = get_openai_api_key().unwrap();
        assert_eq!(key, "sk-from-env");
    }

    #[test]
    fn auth_file_string_format_fallback() {
        let _base_lock = lock_auth_env();
        let (_home, _home_lock) = auth_tests::set_temp_home();
        let _key_guard = clear_api_key_env();
        crate::opencode::auth::write_auth_file(&json!({
            "openai": "sk-from-auth-string"
        }))
        .unwrap();

        let key = get_openai_api_key().unwrap();
        assert_eq!(key, "sk-from-auth-string");
    }

    #[test]
    fn auth_file_access_field_fallback() {
        let _base_lock = lock_auth_env();
        let (_home, _home_lock) = auth_tests::set_temp_home();
        let _key_guard = clear_api_key_env();
        crate::opencode::auth::write_auth_file(&json!({
            "openai": { "access": "sk-oauth-access" }
        }))
        .unwrap();

        let key = get_openai_api_key().unwrap();
        assert_eq!(key, "sk-oauth-access");
    }

    #[test]
    fn auth_file_token_field_fallback() {
        let _base_lock = lock_auth_env();
        let (_home, _home_lock) = auth_tests::set_temp_home();
        let _key_guard = clear_api_key_env();
        crate::opencode::auth::write_auth_file(&json!({
            "openai": { "token": "sk-token" }
        }))
        .unwrap();

        let key = get_openai_api_key().unwrap();
        assert_eq!(key, "sk-token");
    }

    #[test]
    fn codex_alias_fallback() {
        let _base_lock = lock_auth_env();
        let (_home, _home_lock) = auth_tests::set_temp_home();
        let _key_guard = clear_api_key_env();
        crate::opencode::auth::write_auth_file(&json!({
            "codex": { "access": "sk-codex-access" }
        }))
        .unwrap();

        let key = get_openai_api_key().unwrap();
        assert_eq!(key, "sk-codex-access");
    }

    #[test]
    fn no_key_returns_none() {
        let _base_lock = lock_auth_env();
        let (_home, _home_lock) = auth_tests::set_temp_home();
        let _key_guard = clear_api_key_env();
        // auth file 也为空
        crate::opencode::auth::write_auth_file(&json!({})).unwrap();

        assert!(get_openai_api_key().is_none());
        assert!(!TtsService::new().is_available());
    }

    #[test]
    fn is_available_true_when_env_set() {
        let _base_lock = lock_auth_env();
        let (_home, _home_lock) = auth_tests::set_temp_home();
        let _key_guard = clear_api_key_env();
        std::env::set_var("OPENAI_API_KEY", "sk-x");

        assert!(TtsService::new().is_available());
    }

    #[test]
    fn voices_constant_has_13_entries() {
        assert_eq!(TTS_VOICES.len(), 13);
        assert!(TTS_VOICES.contains(&"coral"));
        assert!(TTS_VOICES.contains(&"marin"));
        assert!(TTS_VOICES.contains(&"cedar"));
    }
}