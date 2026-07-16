//! Dictation 服务 — 提供方解析 + 就绪快照。
//!
//! 对应 Node `dictation/service.js` (302 LOC), 但**仅 openai-compatible 路径**。
//!
//! ## 本地推理路径本轮不移植 (明确暴露)
//!
//! Node 的 `local` 提供方走 sherpa-onnx worker 进程 (Parakeet STT + Kokoro TTS)。
//! 该 native 栈 (`local/*` 6 文件, ~1235 LOC) 在 Rust 端没有对等物, 本轮返回
//! 明确的 `local_models_unsupported` 错误 — **不隐藏降级**。后续阶段决定 native
//! 方案 (保留 Node worker 子进程 vs sherpa-rs) 后再实现。
//!
//! 提供方:
//! - `openai-compatible`: 任意 OpenAI-compatible `/v1/audio/transcriptions` 端点
//!   (faster-whisper / whisper.cpp / OpenAI), 复用 `openai_session`。
//! - `local` (默认): **桩**, 返回 `local_models_unsupported`。

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{json, Value};

use super::openai_session::{OpenAiCompatibleSessionConfig, OpenAiCompatibleTranscriptionSession};
use super::stream_manager::{CreateSttOutcome, StartOptions};

// =========================================================================
// 模型目录 (只读常量, status 路由报告 "未安装")
// =========================================================================

/// 单个 STT/TTS 模型的目录条目 (对应 Node `model-catalog.js` 条目)。
#[derive(Debug, Clone)]
pub struct ModelSpec {
    pub id: &'static str,
    pub description: &'static str,
    /// 模型类型。本轮 status 路由未按类型区分报告, 但保留以维持目录语义。
    #[allow(dead_code)]
    pub kind: ModelKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelKind {
    Stt,
    Tts,
}

/// STT 模型目录 (对应 Node `LOCAL_STT_MODEL_CATALOG`)。
/// 本轮不下载/不安装, 仅用于 status 路由的报告一致性。
pub static LOCAL_STT_MODEL_SPECS: &[ModelSpec] = &[
    ModelSpec {
        id: "parakeet-tdt-0.6b-v2-int8",
        description: "NVIDIA Parakeet TDT v2 (English)",
        kind: ModelKind::Stt,
    },
    ModelSpec {
        id: "parakeet-tdt-0.6b-v3-int8",
        description: "NVIDIA Parakeet TDT v3 (25 European languages, auto-detected)",
        kind: ModelKind::Stt,
    },
    ModelSpec {
        id: "whisper-base-int8",
        description: "OpenAI Whisper base (multilingual, smaller and lighter)",
        kind: ModelKind::Stt,
    },
    ModelSpec {
        id: "whisper-tiny-int8",
        description: "OpenAI Whisper tiny (multilingual, fastest and lightest)",
        kind: ModelKind::Stt,
    },
];

/// TTS 模型目录 (对应 Node `LOCAL_TTS_MODEL_CATALOG`)。
pub static LOCAL_TTS_MODEL_SPECS: &[ModelSpec] = &[ModelSpec {
    id: "kokoro-en-v0_19",
    description: "Kokoro TTS (English, natural voices)",
    kind: ModelKind::Tts,
}];

pub const DEFAULT_LOCAL_STT_MODEL: &str = "parakeet-tdt-0.6b-v2-int8";
#[allow(dead_code)] // 为本地 TTS 路径预留 (后续阶段)
pub const DEFAULT_LOCAL_TTS_MODEL: &str = "kokoro-en-v0_19";

fn is_local_stt_model_id(id: &str) -> bool {
    LOCAL_STT_MODEL_SPECS.iter().any(|s| s.id == id)
}
fn is_local_tts_model_id(id: &str) -> bool {
    LOCAL_TTS_MODEL_SPECS.iter().any(|s| s.id == id)
}
fn is_local_model_id(id: &str) -> bool {
    is_local_stt_model_id(id) || is_local_tts_model_id(id)
}

/// 构建 "未安装" 的模型描述条目 (本地推理不支持, 所有模型恒为未安装)。
fn describe_model_unsupported(spec: &ModelSpec) -> Value {
    json!({
        "id": spec.id,
        "description": spec.description,
        "installed": false,
        "downloading": false,
        "downloadProgress": null,
        "downloadError": null,
    })
}

// =========================================================================
// 服务
// =========================================================================

/// Dictation 服务 (持有 models_dir 路径, 本轮无 worker/无下载状态)。
#[derive(Clone)]
pub struct DictationService {
    #[allow(dead_code)]
    models_dir: PathBuf,
}

impl DictationService {
    pub fn new(models_dir: PathBuf) -> Arc<Self> {
        Arc::new(Self { models_dir })
    }

    /// 创建 STT 会话 (对应 Node `createSttSession`)。
    /// `local` → 桩错误; `openai-compatible` → 真实会话。
    pub async fn create_stt_session(&self, options: StartOptions) -> CreateSttOutcome {
        let provider = if options.provider.as_deref() == Some("openai-compatible") {
            "openai-compatible"
        } else {
            "local"
        };

        if provider == "openai-compatible" {
            let config = options.openai_compatible.unwrap_or_default();
            let mut session = OpenAiCompatibleTranscriptionSession::new(
                OpenAiCompatibleSessionConfig {
                    base_url: config.base_url,
                    model: config.model,
                    api_key: config.api_key,
                    language: options.language,
                },
            );
            if let Err(e) = session.connect() {
                return CreateSttOutcome::Error {
                    error: e,
                    retryable: false,
                    reason_code: Some("stt_not_configured".to_string()),
                };
            }
            return CreateSttOutcome::Session(Box::new(session));
        }

        // local 提供方: 本轮不支持
        local_unavailable()
    }

    /// 就绪快照 (对应 Node `getStatus`)。
    pub async fn get_status(&self, provider: Option<&str>, local_model: Option<&str>) -> Value {
        let provider = if provider == Some("openai-compatible") {
            "openai-compatible"
        } else {
            "local"
        };
        let model_id = local_model
            .filter(|id| is_local_stt_model_id(id))
            .unwrap_or(DEFAULT_LOCAL_STT_MODEL);

        let models: Vec<Value> = LOCAL_STT_MODEL_SPECS
            .iter()
            .map(describe_model_unsupported)
            .collect();
        let tts_models: Vec<Value> = LOCAL_TTS_MODEL_SPECS
            .iter()
            .map(describe_model_unsupported)
            .collect();

        if provider == "openai-compatible" {
            return json!({
                "provider": provider,
                "available": true,
                "models": models,
                "ttsModels": tts_models,
            });
        }

        // local: 不支持
        json!({
            "provider": provider,
            "available": false,
            "reasonCode": super::LOCAL_MODELS_UNSUPPORTED_REASON,
            "activeModel": model_id,
            "models": models,
            "ttsModels": tts_models,
        })
    }

    /// 本地 TTS 合成 (对应 Node `synthesizeSpeech`)。本轮不支持。
    pub async fn synthesize_speech(&self) -> SynthesizeResult {
        SynthesizeResult::Error {
            error: "Local TTS models are not available in this build.".to_string(),
            retryable: false,
            reason_code: Some(super::LOCAL_MODELS_UNSUPPORTED_REASON.to_string()),
        }
    }

    /// 请求模型下载 (对应 Node `requestModelDownload`)。本轮不支持。
    pub async fn request_model_download(&self, model_id: &str) -> ModelActionResult {
        if !is_local_model_id(model_id) {
            return ModelActionResult {
                ok: false,
                error: Some("Unknown model id".to_string()),
                ..Default::default()
            };
        }
        ModelActionResult {
            ok: false,
            error: Some("Local model management is not supported in this build.".to_string()),
            ..Default::default()
        }
    }

    /// 删除模型 (对应 Node `deleteModel`)。本轮不支持。
    pub async fn delete_model(&self, model_id: &str) -> ModelActionResult {
        if !is_local_model_id(model_id) {
            return ModelActionResult {
                ok: false,
                error: Some("Unknown model id".to_string()),
                ..Default::default()
            };
        }
        ModelActionResult {
            ok: false,
            error: Some("Local model management is not supported in this build.".to_string()),
            ..Default::default()
        }
    }
}

/// `synthesize_speech` 的结果 (对应 Node `synthesizeSpeech` 返回)。
pub enum SynthesizeResult {
    /// 成功合成的音频。本轮 local TTS 不支持, 此变体不会被构造 — 保留
    /// 以维持与 Node 返回类型的对等 (后续阶段接入 sherpa TTS 时启用)。
    #[allow(dead_code)]
    Audio { audio: Vec<u8>, format: String },
    Error {
        error: String,
        retryable: bool,
        reason_code: Option<String>,
    },
}

/// 模型管理操作结果 (对应 Node `requestModelDownload`/`deleteModel` 返回)。
#[derive(Debug, Default, Clone)]
pub struct ModelActionResult {
    pub ok: bool,
    pub installed: bool,
    pub error: Option<String>,
}

impl ModelActionResult {
    pub fn to_json(&self) -> Value {
        let mut v = json!({ "ok": self.ok });
        if self.installed {
            v["installed"] = json!(true);
        }
        if let Some(e) = &self.error {
            v["error"] = json!(e);
        }
        v
    }
}

/// local 提供方就绪错误 (明确暴露不支持, 非隐藏降级)。
fn local_unavailable() -> CreateSttOutcome {
    CreateSttOutcome::Error {
        error: super::LOCAL_MODELS_UNSUPPORTED_ERROR.to_string(),
        retryable: false,
        reason_code: Some(super::LOCAL_MODELS_UNSUPPORTED_REASON.to_string()),
    }
}

// 供 routes.rs 构造 manager 工厂时引用 SttSession trait
#[allow(unused_imports)]
use super::stream_manager::SttSession as _SttSessionTrait;

// =========================================================================
// 测试
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_provider_returns_unsupported() {
        let svc = DictationService::new(PathBuf::from("/tmp/models"));
        let outcome = svc.create_stt_session(StartOptions::default()).await;
        match outcome {
            CreateSttOutcome::Error {
                reason_code,
                retryable,
                ..
            } => {
                assert_eq!(reason_code.as_deref(), Some(super::super::LOCAL_MODELS_UNSUPPORTED_REASON));
                assert!(!retryable);
            }
            CreateSttOutcome::Session(_) => panic!("local provider should not return a session"),
        }
    }

    #[tokio::test]
    async fn status_local_reports_unsupported() {
        let svc = DictationService::new(PathBuf::from("/tmp/models"));
        let status = svc.get_status(None, None).await;
        assert_eq!(status["provider"], "local");
        assert_eq!(status["available"], false);
        assert_eq!(status["reasonCode"], super::super::LOCAL_MODELS_UNSUPPORTED_REASON);
        assert!(status["models"].is_array());
        assert_eq!(status["models"].as_array().unwrap().len(), LOCAL_STT_MODEL_SPECS.len());
    }

    #[tokio::test]
    async fn status_openai_compatible_available() {
        let svc = DictationService::new(PathBuf::from("/tmp/models"));
        let status = svc
            .get_status(Some("openai-compatible"), None)
            .await;
        assert_eq!(status["provider"], "openai-compatible");
        assert_eq!(status["available"], true);
    }

    #[tokio::test]
    async fn synthesize_speech_local_unsupported() {
        let svc = DictationService::new(PathBuf::from("/tmp/models"));
        match svc.synthesize_speech().await {
            SynthesizeResult::Error { reason_code, .. } => {
                assert_eq!(reason_code.as_deref(), Some(super::super::LOCAL_MODELS_UNSUPPORTED_REASON));
            }
            SynthesizeResult::Audio { .. } => panic!("local TTS should not be available"),
        }
    }

    #[tokio::test]
    async fn request_download_unknown_model_errors() {
        let svc = DictationService::new(PathBuf::from("/tmp/models"));
        let r = svc.request_model_download("nonexistent").await;
        assert!(!r.ok);
        assert_eq!(r.error.as_deref(), Some("Unknown model id"));
    }

    #[tokio::test]
    async fn request_download_known_model_unsupported() {
        let svc = DictationService::new(PathBuf::from("/tmp/models"));
        let r = svc.request_model_download("parakeet-tdt-0.6b-v2-int8").await;
        assert!(!r.ok);
        assert!(r.error.as_deref().unwrap().contains("not supported"));
    }
}
