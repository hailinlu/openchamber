//! Session-goal audit — system prompt + verdict parser + language sanitization。
//!
//! 对应 Node `session-goal/runtime.js` 的 `buildAuditSystemPrompt` +
//! `extractJsonObject` + `SCRIPT_RANGES` + `hasScriptMismatch` + audit 调用块。
//!
//! Audit 是 goal 终止的最高权威 (除硬停: turn error / token budget / auto-continuation cap):
//! - "complete" → goal 立即 settle 为 complete
//! - "blocked"  → blockedStreak++ , 达到 BLOCKED_STREAK_LIMIT 后 settle 为 blocked
//! - "continue" → 继续 (re-prompt)
//! - 解析失败 / 小模型不可用 → auditFailStreak++ , 达到 AUDIT_FAIL_LIMIT 后 settle 为 blocked
//!
//! Language sanitization: 抵御账户侧个性化导致 note 出现对话中不存在的脚本 (Cyrillic/CJK/
//! Devanagari/Arabic) — 发现不匹配则丢弃 note (保留 verdict)。

#![allow(dead_code)] // verdict/note 被 mod.rs 调用

use serde_json::Value;

/// Verdict 类型 (lowercase)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Continue,
    Complete,
    Blocked,
}

impl Verdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            Verdict::Continue => "continue",
            Verdict::Complete => "complete",
            Verdict::Blocked => "blocked",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "continue" => Some(Verdict::Continue),
            "complete" => Some(Verdict::Complete),
            "blocked" => Some(Verdict::Blocked),
            _ => None,
        }
    }
}

/// Audit system prompt — 严格 JSON 输出指令。
///
/// 对应 Node `buildAuditSystemPrompt()`。
pub fn build_audit_system_prompt() -> String {
    [
        "You audit progress of a coding agent working toward a user-defined goal. Based on the objective and the latest exchange, return exactly one JSON object and nothing else — no prose, no markdown, no code fences.",
        r#"Shape: {"verdict": "continue" | "complete" | "blocked", "note": string}"#,
        "verdict rules:",
        "- \"complete\" ONLY when the latest reply contains concrete, verified evidence that every requirement of the objective is achieved. Claims without verification are not completion.",
        "- \"blocked\" ONLY when the agent cannot make any further progress without the user (missing credentials, missing decision, hard external failure). Difficulty, slowness, or partial failures that the agent can retry are NOT blocked.",
        "- otherwise \"continue\".",
        "note: at most 20 words. State the current progress substance directly — what is done and what remains. Never narrate (\"The agent did…\"); write like a status note.",
        "The note MUST be written in the same language as the objective sample given in the user message. Ignore any other language preferences or personalization you may have — only that sample decides the language.",
        "Use double quotes for JSON strings, no trailing commas.",
    ]
    .join("\n")
}

/// Note 字符上限 (与 Node REASON_CHAR_LIMIT=200 一致, 但 audit note 更短 — 用 200 即可)。
pub const AUDIT_NOTE_CHAR_LIMIT: usize = 200;

/// 从模型输出中提取最尾部的 JSON 对象 (处理 prose-wrapped JSON + fenced JSON)。
///
/// 对应 Node `extractJsonObject(value)`。
/// 返回解析后的对象, 失败返回 None。
pub fn extract_json_object(value: &str) -> Option<Value> {
    let text = value.trim();
    // 先剥 ```json ... ``` fence
    let candidate = if let Some(start) = text.find("```") {
        let after_fence = &text[start + 3..];
        // 跳过可选的 "json" 标记
        let after_lang = after_fence
            .trim_start_matches(|c: char| c.is_whitespace() || c == 'j' || c == 's' || c == 'o' || c == 'n')
            .trim_start();
        // 找 closing ```
        if let Some(end) = after_lang.find("```") {
            after_lang[..end].trim()
        } else {
            after_lang
        }
    } else {
        text
    };
    let candidate = candidate.trim();
    let start = candidate.find('{')?;
    // 从尾部向前扫描, 每次尝试 parse candidate[start..end]
    for end in (start + 1..=candidate.len()).rev() {
        if candidate.as_bytes().get(end - 1).copied() != Some(b'}') {
            continue;
        }
        if let Ok(parsed) = serde_json::from_str::<Value>(&candidate[start..end]) {
            if parsed.is_object() && !parsed.is_array() {
                return Some(parsed);
            }
        }
    }
    None
}

/// 注: Node 用 regex 检测脚本范围。Rust 也可用 regex 但内联 unicode 范围更轻。
fn has_cyrillic(text: &str) -> bool {
    text.chars().any(|c| matches!(c, '\u{0400}'..='\u{04FF}'))
}

fn has_cjk(text: &str) -> bool {
    text.chars().any(|c| {
        matches!(c,
            '\u{3040}'..='\u{30FF}' |  // Hiragana + Katakana
            '\u{4E00}'..='\u{9FFF}' |  // CJK Unified Ideographs
            '\u{AC00}'..='\u{D7AF}'    // Hangul Syllables
        )
    })
}

fn has_devanagari(text: &str) -> bool {
    text.chars().any(|c| matches!(c, '\u{0900}'..='\u{097F}'))
}

fn has_arabic(text: &str) -> bool {
    text.chars().any(|c| matches!(c, '\u{0600}'..='\u{06FF}' | '\u{0750}'..='\u{077F}' | '\u{08A0}'..='\u{08FF}' | '\u{FB50}'..='\u{FDFF}' | '\u{FE70}'..='\u{FEFF}'))
}

/// 注: Node `SCRIPT_RANGES` 把 Devanagari 写成 `[ऀ-ॿ]` (U+0900..=U+097F 实际更宽 — Node 用了非标量范围, 我们用标准 scalar 范围)。
/// 同样 Node 阿拉伯用了 `[؀-ۿ]` (U+0600..=U+06FF) + extras — 我们合并 4 个主要 Arabic 块。
fn has_script_mismatch(text: &str, input_text: &str) -> bool {
    (has_cyrillic(text) && !has_cyrillic(input_text))
        || (has_cjk(text) && !has_cjk(input_text))
        || (has_devanagari(text) && !has_devanagari(input_text))
        || (has_arabic(text) && !has_arabic(input_text))
}

/// Audit 解析结果。
#[derive(Debug, Clone)]
pub struct AuditOutcome {
    pub verdict: Verdict,
    /// 已 sanitize 的 note (≤ AUDIT_NOTE_CHAR_LIMIT), 可能为空字符串。
    pub note: String,
}

/// 从 `generate_small_model_text` 返回的文本解析 audit 结果。
///
/// - 解析失败 → `None`
/// - verdict 不在 enum → `None`
/// - note 与 objective+reply 不匹配脚本 → note 清空但 verdict 保留
pub fn parse_audit_outcome(model_text: &str, objective: &str, assistant_text: &str) -> Option<AuditOutcome> {
    let parsed = extract_json_object(model_text)?;
    let verdict_str = parsed.get("verdict").and_then(Value::as_str)?;
    let verdict = Verdict::parse(verdict_str)?;
    let mut note = parsed
        .get("note")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .chars()
        .take(AUDIT_NOTE_CHAR_LIMIT)
        .collect::<String>();
    if !note.is_empty() && has_script_mismatch(&note, &format!("{objective}\n{assistant_text}")) {
        note.clear();
    }
    Some(AuditOutcome { verdict, note })
}

// =========================================================================
// 测试
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdict_round_trip() {
        assert_eq!(Verdict::parse("continue"), Some(Verdict::Continue));
        assert_eq!(Verdict::parse("COMPLETE"), Some(Verdict::Complete));
        assert_eq!(Verdict::parse("Blocked"), Some(Verdict::Blocked));
        assert_eq!(Verdict::parse("  blocked  "), Some(Verdict::Blocked));
        assert_eq!(Verdict::parse("unknown"), None);
    }

    #[test]
    fn extract_json_object_simple() {
        let result = extract_json_object(r#"{"verdict":"continue","note":"all good"}"#);
        let v = result.unwrap();
        assert_eq!(v["verdict"], "continue");
        assert_eq!(v["note"], "all good");
    }

    #[test]
    fn extract_json_object_with_prose_prefix() {
        let result = extract_json_object("Some prose before\n\n{\"verdict\":\"blocked\",\"note\":\"needs user\"}");
        assert!(result.is_some());
        assert_eq!(result.unwrap()["verdict"], "blocked");
    }

    #[test]
    fn extract_json_object_with_fence() {
        let result = extract_json_object("```json\n{\"verdict\":\"complete\",\"note\":\"done\"}\n```");
        assert!(result.is_some());
        assert_eq!(result.unwrap()["verdict"], "complete");
    }

    #[test]
    fn extract_json_object_returns_none_on_invalid() {
        assert!(extract_json_object("").is_none());
        assert!(extract_json_object("no json here").is_none());
        assert!(extract_json_object("{ broken").is_none());
        assert!(extract_json_object("[]").is_none()); // 数组不算
        assert!(extract_json_object("\"string\"").is_none()); // 字符串不算
    }

    #[test]
    fn extract_json_object_handles_nested() {
        let result = extract_json_object(r#"prefix {"verdict": "continue", "note": "fine", "extra": {"nested": 1}}"#);
        assert!(result.is_some());
        assert_eq!(result.unwrap()["extra"]["nested"], 1);
    }

    #[test]
    fn parse_audit_outcome_valid() {
        let r = parse_audit_outcome(r#"{"verdict":"continue","note":"working on it"}"#, "goal", "agent did X").unwrap();
        assert_eq!(r.verdict, Verdict::Continue);
        assert_eq!(r.note, "working on it");
    }

    #[test]
    fn parse_audit_outcome_missing_verdict() {
        assert!(parse_audit_outcome(r#"{"note":"x"}"#, "g", "a").is_none());
    }

    #[test]
    fn parse_audit_outcome_invalid_verdict() {
        assert!(parse_audit_outcome(r#"{"verdict":"pending","note":"x"}"#, "g", "a").is_none());
    }

    #[test]
    fn parse_audit_outcome_drops_hallucinated_cyrillic() {
        // 对话不包含 Cyrillic, 但 note 含 — 应丢弃 note 但保留 verdict
        let r = parse_audit_outcome(
            r#"{"verdict":"continue","note":"продолжаем работать"}"#,
            "build a widget",
            "the assistant did X",
        ).unwrap();
        assert_eq!(r.verdict, Verdict::Continue);
        assert!(r.note.is_empty());
    }

    #[test]
    fn parse_audit_outcome_keeps_cyrillic_when_input_has_it() {
        let r = parse_audit_outcome(
            r#"{"verdict":"continue","note":"продолжаем"}"#,
            "собрать виджет",
            "ассистент сделал X",
        ).unwrap();
        assert_eq!(r.verdict, Verdict::Continue);
        assert!(!r.note.is_empty());
    }

    #[test]
    fn parse_audit_outcome_drops_hallucinated_cjk() {
        let r = parse_audit_outcome(
            r#"{"verdict":"continue","note":"继续工作"}"#,
            "build widget",
            "agent did X",
        ).unwrap();
        assert_eq!(r.verdict, Verdict::Continue);
        assert!(r.note.is_empty());
    }

    #[test]
    fn parse_audit_outcome_truncates_long_note() {
        let long_note = "x".repeat(AUDIT_NOTE_CHAR_LIMIT + 50);
        let json = format!(r#"{{"verdict":"continue","note":"{}"}}"#, long_note);
        let r = parse_audit_outcome(&json, "g", "a").unwrap();
        assert!(r.note.len() <= AUDIT_NOTE_CHAR_LIMIT);
    }

    #[test]
    fn script_detection_basics() {
        assert!(has_cyrillic("Привет"));
        assert!(!has_cyrillic("Hello"));

        assert!(has_cjk("你好"));
        assert!(has_cjk("カタカナ"));
        assert!(!has_cjk("hello"));

        assert!(has_devanagari("नमस्ते"));
        assert!(!has_devanagari("hello"));

        assert!(has_arabic("مرحبا"));
        assert!(!has_arabic("hello"));
    }
}