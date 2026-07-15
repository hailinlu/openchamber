//! 文本规范化 — markdown→plain text + truncate。
//!
//! 对应 Node `notifications/message.js`。
//! 纯函数, 无副作用, 无 I/O。

use regex::Regex;

use super::DEFAULT_NOTIFICATION_MESSAGE_MAX_LENGTH;

use std::sync::OnceLock;

/// 解析为正整数, 无效时返回 fallback。
fn resolve_positive_usize(value: Option<i64>, fallback: usize) -> usize {
    match value {
        Some(v) if v > 0 => v as usize,
        _ => fallback,
    }
}

// 正则用 OnceLock 延迟初始化 (线程安全单例)。

fn re_fenced_code() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"```[\s\S]*?```").unwrap())
}

fn re_inline_code() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"`([^`]*)`").unwrap())
}

fn re_list_marker() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"(?m)^[\t ]*[-*+]\s+").unwrap())
}

fn re_heading() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"(?m)^#{1,6}\s+").unwrap())
}

fn re_bold() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"\*\*(.*?)\*\*").unwrap())
}

fn re_bold_under() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"__(.*?)__").unwrap())
}

fn re_italic() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"\*(.*?)\*").unwrap())
}

fn re_italic_under() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"_(.*?)_").unwrap())
}

fn re_link() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"\[(.*?)\]\((.*?)\)").unwrap())
}

fn re_newline_ws() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"\s*\n\s*").unwrap())
}

fn re_multi_ws() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"\s+").unwrap())
}

/// 将 markdown 文本规范化为纯文本。
///
/// 对应 Node `normalizeNotificationPlainText`。
/// 剥离: 围栏代码块, 行内代码标记, 列表标记, 标题标记, 粗体/斜体标记, 链接 URL。
/// 折叠空白, trim。
pub fn normalize_notification_plain_text(text: &str) -> String {
    let s = re_fenced_code().replace_all(text, " ");
    let s = re_inline_code().replace_all(&s, "$1");
    let s = re_list_marker().replace_all(&s, "");
    let s = re_heading().replace_all(&s, "");
    let s = re_bold().replace_all(&s, "$1");
    let s = re_bold_under().replace_all(&s, "$1");
    let s = re_italic().replace_all(&s, "$1");
    let s = re_italic_under().replace_all(&s, "$1");
    let s = re_link().replace_all(&s, "$1");
    let s = re_newline_ws().replace_all(&s, " ");
    let s = re_multi_ws().replace_all(&s, " ");
    s.trim().to_string()
}

/// 截断文本到 maxLength, 超出时追加 `...`。
///
/// 对应 Node `truncateNotificationText`。
pub fn truncate_notification_text(text: &str, max_length: Option<i64>) -> String {
    let safe_max = resolve_positive_usize(max_length, DEFAULT_NOTIFICATION_MESSAGE_MAX_LENGTH);
    if text.len() <= safe_max {
        return text.to_string();
    }
    // Node 用 text.slice(0, safeMaxLength) — 按 UTF-16 code unit 边界截断。
    // Rust String 按 char 边界更安全; 对于通知文本, 差异可忽略。
    let truncated: String = text.chars().take(safe_max).collect();
    format!("{truncated}...")
}

/// 规范化 + 截断消息, 对应 Node `prepareNotificationLastMessage`。
///
/// `message` 是原始消息文本, `max_length` 来自 settings.maxLastMessageLength。
pub fn prepare_notification_last_message(
    message: Option<&str>,
    max_length: Option<i64>,
) -> String {
    let original = message.unwrap_or("");
    if original.is_empty() {
        return String::new();
    }
    let plain = normalize_notification_plain_text(original);
    truncate_notification_text(&plain, max_length)
}

// =========================================================================
// 测试
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_strips_fenced_code() {
        let input = "Hello ```code block``` world";
        assert_eq!(normalize_notification_plain_text(input), "Hello world");
    }

    #[test]
    fn normalize_strips_inline_code() {
        let input = "Use `npm install` to install";
        assert_eq!(normalize_notification_plain_text(input), "Use npm install to install");
    }

    #[test]
    fn normalize_strips_list_markers() {
        let input = "- item one\n- item two\n* item three";
        assert_eq!(
            normalize_notification_plain_text(input),
            "item one item two item three"
        );
    }

    #[test]
    fn normalize_strips_headings() {
        let input = "## Heading\nSome text";
        assert_eq!(normalize_notification_plain_text(input), "Heading Some text");
    }

    #[test]
    fn normalize_strips_bold_italic() {
        let input = "**bold** and *italic* and __under__ and _under2_";
        assert_eq!(
            normalize_notification_plain_text(input),
            "bold and italic and under and under2"
        );
    }

    #[test]
    fn normalize_strips_links() {
        let input = "See [docs](https://example.com) for info";
        assert_eq!(normalize_notification_plain_text(input), "See docs for info");
    }

    #[test]
    fn normalize_collapses_whitespace() {
        let input = "  multiple   \n\n  spaces  ";
        assert_eq!(normalize_notification_plain_text(input), "multiple spaces");
    }

    #[test]
    fn truncate_short_text_unchanged() {
        assert_eq!(truncate_notification_text("short", Some(100)), "short");
    }

    #[test]
    fn truncate_long_text_adds_ellipsis() {
        let input = "a".repeat(300);
        let result = truncate_notification_text(&input, Some(10));
        assert_eq!(result, "aaaaaaaaaa...");
    }

    #[test]
    fn truncate_uses_default_max() {
        let input = "a".repeat(300);
        let result = truncate_notification_text(&input, None);
        // 默认 250 + "..."
        assert_eq!(result.len(), 253);
        assert!(result.ends_with("..."));
    }

    #[test]
    fn prepare_empty_returns_empty() {
        assert_eq!(prepare_notification_last_message(None, Some(100)), "");
        assert_eq!(prepare_notification_last_message(Some(""), Some(100)), "");
    }

    #[test]
    fn prepare_normalizes_and_truncates() {
        let input = "Hello **world** ```code```";
        let result = prepare_notification_last_message(Some(input), Some(100));
        assert_eq!(result, "Hello world");
    }
}
