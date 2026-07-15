//! Session-assist metadata — AssistMetadata 序列化与字段 clamp。
//!
//! 对应 Node `session-assist/runtime.js` 中生成 recap/suggestion 后的
//! `metadata.openchamber.assist` 写入。

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

/// recap 字符上限。
pub const RECAP_CHAR_LIMIT: usize = 320;
/// suggestion 字符上限。
pub const SUGGESTION_CHAR_LIMIT: usize = 500;

/// AssistMetadata — session.metadata.openchamber.assist 内容。
///
/// UI 客户端读取此对象显示 recap + suggestion; stale 检查通过 `forMessageID`。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AssistMetadata {
    #[serde(default)]
    pub recap: String,
    #[serde(default)]
    pub suggestion: String,
    #[serde(rename = "forMessageID")]
    #[serde(default)]
    pub for_message_id: String,
    #[serde(default)]
    pub generated_at: i64,
}

/// Trim + 字符 clamp。
pub fn clamp_recap(value: &str) -> String {
    value.trim().chars().take(RECAP_CHAR_LIMIT).collect()
}

pub fn clamp_suggestion(value: &str) -> String {
    value.trim().chars().take(SUGGESTION_CHAR_LIMIT).collect()
}

/// 把 AssistMetadata 转为 JSON value, 写入 session metadata.openchamber.assist。
pub fn to_value(assist: &AssistMetadata) -> Value {
    serde_json::to_value(assist).expect("AssistMetadata always serializable")
}

/// 在 session metadata.openchamber 上合并 assist 子对象 (保留其他 openchamber 字段)。
///
/// 对应 Node `currentNamespace = metadata.openchamber || {}` + 写入新 assist 对象。
pub fn merge_assist_into_openchamber(session: &Value, assist: &AssistMetadata) -> Value {
    let metadata = session
        .get("metadata")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_else(Map::new);
    let mut metadata = metadata;

    let namespace = metadata
        .get("openchamber")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_else(Map::new);
    let mut namespace = namespace;
    namespace.insert("assist".to_string(), to_value(assist));

    metadata.insert("openchamber".to_string(), Value::Object(namespace));
    json!({ "metadata": Value::Object(metadata) })
}

// =========================================================================
// 测试
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn clamp_respects_limits() {
        assert_eq!(clamp_recap("  hello  "), "hello");
        let long = "x".repeat(RECAP_CHAR_LIMIT + 100);
        assert_eq!(clamp_recap(&long).len(), RECAP_CHAR_LIMIT);

        assert_eq!(clamp_suggestion("  hi  "), "hi");
        let long = "x".repeat(SUGGESTION_CHAR_LIMIT + 100);
        assert_eq!(clamp_suggestion(&long).len(), SUGGESTION_CHAR_LIMIT);
    }

    #[test]
    fn assist_metadata_serializes_camel_case() {
        let m = AssistMetadata {
            recap: "did X".into(),
            suggestion: "do Y".into(),
            for_message_id: "msg_1".into(),
            generated_at: 12345,
        };
        let v = to_value(&m);
        assert_eq!(v["recap"], "did X");
        assert_eq!(v["forMessageID"], "msg_1");
        assert_eq!(v["generatedAt"], 12345);
    }

    #[test]
    fn merge_preserves_other_openchamber_fields() {
        let session = json!({
            "metadata": {
                "openchamber": {
                    "goal": {"id": "g1"},
                    "dismissals": ["msg_1"]
                }
            }
        });
        let assist = AssistMetadata {
            recap: "r".into(),
            suggestion: "s".into(),
            for_message_id: "msg_2".into(),
            generated_at: 100,
        };
        let merged = merge_assist_into_openchamber(&session, &assist);
        assert_eq!(merged["metadata"]["openchamber"]["assist"]["recap"], "r");
        assert_eq!(merged["metadata"]["openchamber"]["goal"]["id"], "g1");
        assert_eq!(merged["metadata"]["openchamber"]["dismissals"][0], "msg_1");
    }

    #[test]
    fn merge_creates_namespaces_when_missing() {
        let session = json!({"id": "s1"});
        let assist = AssistMetadata {
            recap: "r".into(),
            suggestion: "s".into(),
            for_message_id: "msg_1".into(),
            generated_at: 100,
        };
        let merged = merge_assist_into_openchamber(&session, &assist);
        assert_eq!(merged["metadata"]["openchamber"]["assist"]["recap"], "r");
    }
}