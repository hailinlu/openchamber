//! TTS 模块的文本净化/蒸馏 re-export。
//!
//! Node `tts/index.js` 导出 `summarizeText / sanitizeForTTS / sanitizeForNote`,
//! Rust 里直接调用 `crate::text::summarization::*`。
//!
//! 这里提供 thin wrapper, 让 `tts` 模块对外有一致的 re-export 表面。

pub use crate::text::summarization::{
    sanitize_for_note as sanitize_for_note_in_text,
    sanitize_for_tts as sanitize_for_tts_in_text,
    summarize_text, SummarizeResult,
};

/// TTS 净化包装: 透传到 `text::summarization::sanitize_for_tts`。
pub fn sanitize_for_tts(text: &str) -> String {
    sanitize_for_tts_in_text(text)
}

/// Note 净化包装: 透传到 `text::summarization::sanitize_for_note`。
pub fn sanitize_for_note(text: &str) -> String {
    sanitize_for_note_in_text(text)
}