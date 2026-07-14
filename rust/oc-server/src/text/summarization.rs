//! 文本净化与蒸馏服务。
//!
//! 移植 `packages/web/server/lib/text/summarization.js`。
//!
//! 三种模式:
//!   - `tts`: 可朗读的简洁文本
//!   - `notification`: 简洁通知文本
//!   - `note`: 蒸馏后的项目笔记
//!
//! 关键约束:
//!   - 正则替换链的顺序与 JS 完全一致
//!   - `…` 是 U+2026, 不是三个句点
//!   - 句分割 `(?<=[.!?])\s+` 用手动扫描实现 (Rust regex 不支持 lookbehind)
//!   - `summarize_text` 总是返回 `summarized: false` (Zen 模型已退役)

use once_cell::sync::Lazy;
use regex::Regex;
use serde::Serialize;

// ---------------------------------------------------------------------------
// 正则编译 (对应 summarization.js 的内联正则)
// ---------------------------------------------------------------------------

// --- sanitizeForTTS ---
static RE_TTS_FENCE: Lazy<Regex> = Lazy::new(|| Regex::new(r"```[\s\S]*?```").unwrap());
static RE_TTS_INLINE: Lazy<Regex> = Lazy::new(|| Regex::new(r"`[^`]*`").unwrap());
static RE_TTS_MARKUP: Lazy<Regex> = Lazy::new(|| Regex::new(r"[*_~`#]").unwrap());
static RE_TTS_LEADING: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?m)^\s*[$#>]\s*").unwrap());
static RE_TTS_PUNCT: Lazy<Regex> = Lazy::new(|| Regex::new(r"[|&;<>]").unwrap());
static RE_TTS_BACKSLASH: Lazy<Regex> = Lazy::new(|| Regex::new(r"\\").unwrap());
static RE_TTS_BRACKETS: Lazy<Regex> = Lazy::new(|| Regex::new(r"[\[\]{}()]").unwrap());
static RE_TTS_QUOTES: Lazy<Regex> = Lazy::new(|| Regex::new(r#"["']"#).unwrap());
static RE_TTS_URL: Lazy<Regex> = Lazy::new(|| Regex::new(r"https?://[^\s]+").unwrap());
static RE_TTS_PATH: Lazy<Regex> = Lazy::new(|| Regex::new(r"/[\w\-./]+").unwrap());
static RE_WS: Lazy<Regex> = Lazy::new(|| Regex::new(r"\s+").unwrap());

// --- sanitizeForNotification ---
static RE_NOTIF_FENCE: Lazy<Regex> = Lazy::new(|| Regex::new(r"```[\s\S]*?```").unwrap());
static RE_NOTIF_INLINE: Lazy<Regex> = Lazy::new(|| Regex::new(r"`([^`]*)`").unwrap());
static RE_NOTIF_LIST: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?m)^[\t ]*[-*+]\s+").unwrap());
static RE_NOTIF_HEADING: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?m)^#{1,6}\s+").unwrap());
static RE_NOTIF_BOLD: Lazy<Regex> = Lazy::new(|| Regex::new(r"\*\*(.*?)\*\*").unwrap());
static RE_NOTIF_UNDER: Lazy<Regex> = Lazy::new(|| Regex::new(r"__(.*?)__").unwrap());
static RE_NOTIF_ITALIC: Lazy<Regex> = Lazy::new(|| Regex::new(r"\*(.*?)\*").unwrap());
static RE_NOTIF_UNDERSCORE: Lazy<Regex> = Lazy::new(|| Regex::new(r"_(.*?)_").unwrap());
static RE_NOTIF_LINK: Lazy<Regex> = Lazy::new(|| Regex::new(r"\[(.*?)\]\((.*?)\)").unwrap());
static RE_NOTIF_NEWLINE_WS: Lazy<Regex> = Lazy::new(|| Regex::new(r"\s*\n\s*").unwrap());

// --- sanitizeForNote (同 notification 但额外处理 URL/引号) ---
static RE_NOTE_URL: Lazy<Regex> = Lazy::new(|| Regex::new(r"https?://[^\s]+").unwrap());
static RE_NOTE_QUOTES: Lazy<Regex> = Lazy::new(|| Regex::new(r#"["']"#).unwrap());

// --- distill 辅助 ---
static RE_DISTILL_SUMMARY_PREFIX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)^In summary[:,]?\s*").unwrap());
static RE_DISTILL_HERE_PREFIX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)^Here(?:s| is) (?:a )?note[:,]?\s*").unwrap());
static RE_DISTILL_CLAUSE: Lazy<Regex> = Lazy::new(|| Regex::new(r"[;:()-]\s+").unwrap());
static RE_DISTILL_COMMA: Lazy<Regex> = Lazy::new(|| Regex::new(r",\s+").unwrap());

// U+2026 HORIZONTAL ELLIPSIS
const ELLIPSIS: char = '\u{2026}';

// ---------------------------------------------------------------------------
// 三种净化函数
// ---------------------------------------------------------------------------

/// 对应 `sanitizeForTTS(text)` — 12 步正则替换链。
pub fn sanitize_for_tts(text: &str) -> String {
    let s = RE_TTS_FENCE.replace_all(text, " ");
    let s = RE_TTS_INLINE.replace_all(&s, " ");
    let s = RE_TTS_MARKUP.replace_all(&s, "");
    let s = RE_TTS_LEADING.replace_all(&s, "");
    let s = RE_TTS_PUNCT.replace_all(&s, " ");
    let s = RE_TTS_BACKSLASH.replace_all(&s, "");
    let s = RE_TTS_BRACKETS.replace_all(&s, "");
    let s = RE_TTS_QUOTES.replace_all(&s, "");
    let s = RE_TTS_URL.replace_all(&s, " a link ");
    let s = RE_TTS_PATH.replace_all(&s, "");
    let s = RE_WS.replace_all(&s, " ");
    s.trim().to_string()
}

/// 对应 `sanitizeForNotification(text)` — 内部使用。
fn sanitize_for_notification(text: &str) -> String {
    let s = RE_NOTIF_FENCE.replace_all(text, " ");
    let s = RE_NOTIF_INLINE.replace_all(&s, "$1");
    let s = RE_NOTIF_LIST.replace_all(&s, "");
    let s = RE_NOTIF_HEADING.replace_all(&s, "");
    let s = RE_NOTIF_BOLD.replace_all(&s, "$1");
    let s = RE_NOTIF_UNDER.replace_all(&s, "$1");
    let s = RE_NOTIF_ITALIC.replace_all(&s, "$1");
    let s = RE_NOTIF_UNDERSCORE.replace_all(&s, "$1");
    let s = RE_NOTIF_LINK.replace_all(&s, "$1");
    let s = RE_NOTIF_NEWLINE_WS.replace_all(&s, " ");
    let s = RE_WS.replace_all(&s, " ");
    s.trim().to_string()
}

/// 对应 `sanitizeForNote(text)` — 在 notification 基础上去掉 URL + 引号。
pub fn sanitize_for_note(text: &str) -> String {
    let s = RE_NOTIF_FENCE.replace_all(text, " ");
    let s = RE_NOTIF_INLINE.replace_all(&s, "$1");
    let s = RE_NOTIF_LIST.replace_all(&s, "");
    let s = RE_NOTIF_HEADING.replace_all(&s, "");
    let s = RE_NOTIF_BOLD.replace_all(&s, "$1");
    let s = RE_NOTIF_UNDER.replace_all(&s, "$1");
    let s = RE_NOTIF_ITALIC.replace_all(&s, "$1");
    let s = RE_NOTIF_UNDERSCORE.replace_all(&s, "$1");
    let s = RE_NOTIF_LINK.replace_all(&s, "$1");
    let s = RE_NOTE_URL.replace_all(&s, "");
    let s = RE_NOTE_QUOTES.replace_all(&s, "");
    let s = RE_WS.replace_all(&s, " ");
    s.trim().to_string()
}

// ---------------------------------------------------------------------------
// 句分割 (手动实现 lookbehind `(?<=[.!?])\s+`)
// ---------------------------------------------------------------------------

/// 在句末标点 (.!?) 后的空白处分割文本。
///
/// 对应 JS 的 `text.split(/(?<=[.!?])\s+/)`。
/// 语义: 遇到 `.`, `!`, `?` 后面跟着一个或多个空白字符时,
/// 在空白序列处分割 (保留标点在前一段, 空白被消费)。
///
/// Rust `regex` crate 不支持 lookbehind, 用手动扫描实现。
fn split_sentences(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut result = Vec::new();
    let mut start = 0;
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];

        // 检查: 当前字符是 . ! ? 且后面有空白
        if (c == '.' || c == '!' || c == '?') && i + 1 < chars.len() {
            // 跳过后续的空白字符
            let mut j = i + 1;
            let mut has_ws = false;
            while j < chars.len() && chars[j].is_whitespace() {
                has_ws = true;
                j += 1;
            }
            if has_ws {
                // 提取 [start..=i] (包含标点), 空白被消费
                let segment: String = chars[start..=i].iter().collect();
                let trimmed = segment.trim().to_string();
                if !trimmed.is_empty() {
                    result.push(trimmed);
                }
                start = j;
                i = j;
                continue;
            }
        }
        i += 1;
    }

    // 最后一段
    if start < chars.len() {
        let segment: String = chars[start..].iter().collect();
        let trimmed = segment.trim().to_string();
        if !trimmed.is_empty() {
            result.push(trimmed);
        }
    }

    result
}

// ---------------------------------------------------------------------------
// 蒸馏函数
// ---------------------------------------------------------------------------

/// 对应 `distillNoteFallback(text, maxLength)`。
fn distill_note_fallback(text: &str, max_length: usize) -> String {
    let sanitized = sanitize_for_note(text);
    if sanitized.is_empty() {
        return String::new();
    }

    let after_summary = RE_DISTILL_SUMMARY_PREFIX.replace(&sanitized, "");
    let normalized = RE_DISTILL_HERE_PREFIX.replace(&after_summary, "").trim().to_string();

    let sentences = split_sentences(&normalized);

    let first = sentences.first().map(|s| s.as_str()).unwrap_or(&normalized);
    // 进一步在子句边界分割
    let best = RE_DISTILL_CLAUSE
        .split(first)
        .next()
        .unwrap_or(first);
    let best = RE_DISTILL_COMMA.split(best).next().unwrap_or(best);
    let best = best.trim();

    let ideal_limit = max_length.max(32).min((normalized.len() as f64 * 0.65) as usize);

    if best.len() <= ideal_limit {
        return best.to_string();
    }

    // JS: best.slice(0, Math.max(0, idealLimit - 1)).trim()
    let clip_at = ideal_limit.saturating_sub(1);
    let clipped: String = best.chars().take(clip_at).collect::<String>().trim().to_string();
    if !clipped.is_empty() {
        format!("{}{}", clipped, ELLIPSIS)
    } else {
        // JS: best.slice(0, idealLimit).trim()
        let fallback: String = best.chars().take(ideal_limit).collect::<String>().trim().to_string();
        fallback
    }
}

/// 对应 `distillNotificationFallback(text, maxLength)`。
fn distill_notification_fallback(text: &str, max_length: usize) -> String {
    let sanitized = sanitize_for_notification(text);
    if sanitized.is_empty() {
        return String::new();
    }

    let sentences = split_sentences(&sanitized);

    // JS: sentences.find(s => s.length >= 20) || sentences[0] || sanitized
    let candidate = sentences
        .iter()
        .find(|s| s.chars().count() >= 20)
        .or(sentences.first())
        .map(|s| s.as_str())
        .unwrap_or(&sanitized);

    let limit = max_length.max(20);

    if candidate.chars().count() <= limit {
        return candidate.to_string();
    }

    // JS: candidate.slice(0, Math.max(0, limit - 1)).trim()
    let clip_at = limit.saturating_sub(1);
    let clipped: String = candidate.chars().take(clip_at).collect::<String>().trim().to_string();
    if !clipped.is_empty() {
        format!("{}{}", clipped, ELLIPSIS)
    } else {
        let fallback: String = candidate.chars().take(limit).collect::<String>().trim().to_string();
        fallback
    }
}

/// 对应 `fallbackByMode(text, maxLength, mode)`。
fn fallback_by_mode(text: &str, max_length: usize, mode: &str) -> String {
    match mode {
        "note" => distill_note_fallback(text, max_length),
        "notification" => distill_notification_fallback(text, max_length),
        _ => sanitize_by_mode(text, mode),
    }
}

/// 对应 `sanitizeByMode(text, mode)`。
fn sanitize_by_mode(text: &str, mode: &str) -> String {
    match mode {
        "note" => sanitize_for_note(text),
        "notification" => sanitize_for_notification(text),
        _ => sanitize_for_tts(text),
    }
}

// ---------------------------------------------------------------------------
// summarizeText — 主入口
// ---------------------------------------------------------------------------

/// `summarizeText` 的返回类型。
///
/// `original_length` / `summary_length` 仅在 `text.length > threshold` 时出现
/// (serde `skip_serializing_if = "Option::is_none"` 实现条件 omit)。
#[derive(Serialize, Debug)]
pub struct SummarizeResult {
    pub summary: String,
    pub summarized: bool,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original_length: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary_length: Option<usize>,
}

/// 对应 `summarizeText({ text, threshold, maxLength, zenModel, mode })`。
///
/// `zenModel` 参数已退役 (Zen 模型不可用), 接受但忽略。
///
/// 返回 `summarized: false` — 所有 mode 使用本地净化/蒸馏。
pub fn summarize_text(
    text: &str,
    threshold: usize,
    max_length: usize,
    _zen_model: Option<&str>,
    mode: &str,
) -> SummarizeResult {
    let summary = fallback_by_mode(text, max_length, mode);
    let summary_len = summary.len();

    if text.is_empty() || text.len() <= threshold {
        return SummarizeResult {
            summary,
            summarized: false,
            reason: if text.is_empty() {
                "No text provided".to_string()
            } else {
                "Text under threshold".to_string()
            },
            original_length: None,
            summary_length: None,
        };
    }

    SummarizeResult {
        summary,
        summarized: false,
        reason: "Model summarization provider unavailable".to_string(),
        original_length: Some(text.len()),
        summary_length: Some(summary_len),
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- sanitize_for_tts 测试 (移植 summarization.test.js) ---

    #[test]
    fn tts_strips_inline_code() {
        assert_eq!(sanitize_for_tts("Read `const value = 1` aloud"), "Read aloud");
    }

    #[test]
    fn tts_strips_fenced_code_block() {
        assert_eq!(
            sanitize_for_tts("Before\n```js\nconst value = 1\n```\nAfter"),
            "Before After"
        );
    }

    #[test]
    fn tts_empty_string() {
        assert_eq!(sanitize_for_tts(""), "");
    }

    #[test]
    fn tts_strips_urls() {
        assert_eq!(
            sanitize_for_tts("Visit https://example.com now"),
            "Visit a link now"
        );
    }

    #[test]
    fn tts_strips_paths() {
        assert_eq!(sanitize_for_tts("Edit /usr/local/bin/foo"), "Edit");
    }

    // --- sanitize_for_note 测试 ---

    #[test]
    fn note_strips_markdown() {
        assert_eq!(
            sanitize_for_note("**bold** and _italic_ and [link](https://x.com)"),
            "bold and italic and link"
        );
    }

    #[test]
    fn note_strips_url() {
        // note 模式去掉 URL (替换为空)
        assert_eq!(
            sanitize_for_note("Check https://example.com please"),
            "Check please"
        );
    }

    // --- 句分割测试 ---

    #[test]
    fn split_sentences_basic() {
        let parts = split_sentences("Hello world. Foo bar! Baz?");
        assert_eq!(parts, vec!["Hello world.", "Foo bar!", "Baz?"]);
    }

    #[test]
    fn split_sentences_no_terminator() {
        let parts = split_sentences("No terminator here");
        assert_eq!(parts, vec!["No terminator here"]);
    }

    #[test]
    fn split_sentences_multiple_spaces() {
        let parts = split_sentences("First.   Second");
        assert_eq!(parts, vec!["First.", "Second"]);
    }

    // --- distill 测试 ---

    #[test]
    fn distill_note_first_sentence() {
        let result = distill_note_fallback("First sentence. Second sentence with the useful insight.", 100);
        assert_eq!(result, "First sentence.");
    }

    #[test]
    fn distill_notification_clips_with_ellipsis() {
        let input = "The implementation now correctly loads notification templates before dispatching the notification. It also fetches the latest assistant message when the event payload does not include message parts. This should make completion notifications match user settings.";
        let result = distill_notification_fallback(input, 80);
        // JS test 期望 80 字符的 clip + U+2026
        assert_eq!(result, "The implementation now correctly loads notification templates before dispatchin\u{2026}");
    }

    // --- summarize_text 测试 ---

    #[test]
    fn summarize_under_threshold() {
        let result = summarize_text("hello", 200, 500, None, "tts");
        assert_eq!(result.summary, "hello");
        assert!(!result.summarized);
        assert_eq!(result.reason, "Text under threshold");
        assert_eq!(result.original_length, None);
        assert_eq!(result.summary_length, None);
    }

    #[test]
    fn summarize_empty_text() {
        let result = summarize_text("", 200, 500, None, "tts");
        assert_eq!(result.summary, "");
        assert!(!result.summarized);
        assert_eq!(result.reason, "No text provided");
    }

    #[test]
    fn summarize_over_threshold() {
        let long_text = "This is a long text that exceeds the threshold for summarization.";
        let result = summarize_text(long_text, 10, 500, None, "tts");
        assert!(!result.summarized);
        assert_eq!(result.reason, "Model summarization provider unavailable");
        assert_eq!(result.original_length, Some(long_text.len()));
        assert!(result.summary_length.is_some());
    }

    #[test]
    fn summarize_notification_mode_over_threshold() {
        let input = "The implementation now correctly loads notification templates before dispatching the notification. It also fetches the latest assistant message when the event payload does not include message parts. This should make completion notifications match user settings.";
        let result = summarize_text(input, 0, 80, None, "notification");
        assert_eq!(
            result.summary,
            "The implementation now correctly loads notification templates before dispatchin\u{2026}"
        );
        assert!(!result.summarized);
        assert_eq!(result.reason, "Model summarization provider unavailable");
    }

    #[test]
    fn summarize_note_mode_over_threshold() {
        let input = "First sentence. Second sentence with the useful insight.";
        let result = summarize_text(input, 0, 100, None, "note");
        assert_eq!(result.summary, "First sentence.");
        assert!(!result.summarized);
        assert_eq!(result.reason, "Model summarization provider unavailable");
    }
}
