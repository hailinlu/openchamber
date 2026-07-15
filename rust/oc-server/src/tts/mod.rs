//! TTS 模块 — Text-to-Speech / Speech-to-Text / 文本净化。
//!
//! 对应 Node `packages/web/server/lib/tts/` (6 文件, 634 LOC)。
//!
//! 子模块:
//!   - `base_url`         — 自定义 OpenAI base URL 校验/规范化
//!   - `service`          — `TtsService` + `generate_speech_stream`
//!   - `stt`              — `transcribe_audio` (OpenAI-compatible transcription)
//!   - `capability_runtime` — macOS `say` 能力探测
//!   - `summarize`        — re-export `text::summarization` 的净化/蒸馏
//!   - `routes`           — 6 个 axum handler

pub mod base_url;
pub mod capability_runtime;
pub mod routes;
pub mod service;
pub mod stt;
pub mod summarize;

#[allow(unused_imports)]
pub use capability_runtime::{
    detect_say_tts_capability, parse_say_voices, SayTtsCapability, SayTtsVoice,
};
#[allow(unused_imports)]
pub use service::{get_openai_api_key, SpeechOptions, SpeechResult, TtsService, TTS_VOICES};
#[allow(unused_imports)]
pub use stt::{mime_type_to_ext, transcribe_audio, TranscribeOptions};
#[allow(unused_imports)]
pub use summarize::{sanitize_for_note, sanitize_for_tts, summarize_text, SummarizeResult};