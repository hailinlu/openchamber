//! 解析器 + normalizers — push/APNs body 解析 + session ID/directory 提取。
//!
//! 对应 Node `notifications/routes.js` 的 `parsePushSubscribeBody` /
//! `parsePushUnsubscribeBody` + `notifications/runtime.js` 的提取辅助函数。

use serde_json::Value;

/// 解析 push subscribe body: `{endpoint, keys:{p256dh, auth}}`。
///
/// 对应 Node `parsePushSubscribeBody`。返回 `(endpoint, p256dh, auth)`。
pub fn parse_push_subscribe_body(body: &Value) -> Option<(String, String, String)> {
    let obj = body.as_object()?;

    let endpoint = obj.get("endpoint")?.as_str()?;
    let endpoint = endpoint.trim();
    if endpoint.is_empty() {
        return None;
    }

    let keys = obj.get("keys")?.as_object()?;
    let p256dh = keys.get("p256dh")?.as_str()?;
    let p256dh = p256dh.trim();
    if p256dh.is_empty() {
        return None;
    }

    let auth = keys.get("auth")?.as_str()?;
    let auth = auth.trim();
    if auth.is_empty() {
        return None;
    }

    Some((
        endpoint.to_string(),
        p256dh.to_string(),
        auth.to_string(),
    ))
}

/// 解析 push unsubscribe body: `{endpoint}`。
///
/// 对应 Node `parsePushUnsubscribeBody`。返回 endpoint。
pub fn parse_push_unsubscribe_body(body: &Value) -> Option<String> {
    let endpoint = body.get("endpoint")?.as_str()?;
    let endpoint = endpoint.trim();
    if endpoint.is_empty() {
        return None;
    }
    Some(endpoint.to_string())
}

/// 从 SSE payload 提取 session ID。
///
/// 对应 Node `extractSessionIdFromPayload`。
/// 查找 properties.info.sessionID/sessionId, properties.sessionID/sessionId/session。
pub fn extract_session_id_from_payload(payload: &Value) -> Option<String> {
    let props = payload.get("properties")?;
    let info = props.get("info");

    // info.sessionID / info.sessionId
    if let Some(info) = info {
        for key in &["sessionID", "sessionId"] {
            if let Some(s) = info.get(key).and_then(|v| v.as_str()) {
                if !s.is_empty() {
                    return Some(s.to_string());
                }
            }
        }
    }

    // properties.sessionID / sessionId / session
    for key in &["sessionID", "sessionId", "session"] {
        if let Some(s) = props.get(key).and_then(|v| v.as_str()) {
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }

    None
}

/// 从 SSE payload 提取 directory。
///
/// 对应 Node `extractDirectoryFromPayload`。
/// 查找 properties.directory 或 properties.info.directory, trim 后非空才返回。
pub fn extract_directory_from_payload(payload: &Value) -> Option<String> {
    let props = payload.get("properties")?;

    // properties.directory
    if let Some(d) = props.get("directory").and_then(|v| v.as_str()) {
        let trimmed = d.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }

    // properties.info.directory
    if let Some(info) = props.get("info") {
        if let Some(d) = info.get("directory").and_then(|v| v.as_str()) {
            let trimmed = d.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }

    None
}

/// 从 session.created/session.updated payload 提取 parentID。
///
/// 对应 Node `getParentIdFromPayload`。
/// 返回 `Some(Some(id))` = 有 parentID, `Some(None)` = 明确无 parentID,
/// `None` = 非 session.created/updated 事件。
pub fn get_parent_id_from_payload(payload: &Value) -> Option<Option<String>> {
    let type_str = payload.get("type").and_then(|v| v.as_str())?;
    if type_str != "session.created" && type_str != "session.updated" {
        return None;
    }

    let parent_id = payload
        .get("properties")
        .and_then(|p| p.get("info"))
        .and_then(|i| i.get("parentID"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    Some(parent_id)
}

/// 格式化 agent mode 名称: 分割 `-_\s`, 每段首字母大写。
///
/// 对应 Node `formatMode`。
pub fn format_mode(raw: &str) -> String {
    let value = raw.trim();
    let normalized = if value.is_empty() { "agent" } else { value };
    normalized
        .split(|c: char| c == '-' || c == '_' || c.is_whitespace())
        .filter(|s| !s.is_empty())
        .map(|token| {
            let mut chars = token.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// 格式化 model ID: 分割 `-_`, 合并连续数字段为 `N.N`, 每段首字母大写。
///
/// 对应 Node `formatModelId`。
pub fn format_model_id(raw: &str) -> String {
    let value = raw.trim();
    if value.is_empty() {
        return "Assistant".to_string();
    }

    let tokens: Vec<&str> = value.split(['-', '_']).collect::<Vec<_>>();
    let mut result: Vec<String> = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let current = tokens[i];
        if i + 1 < tokens.len() {
            let next = tokens[i + 1];
            if !current.is_empty()
                && current.chars().all(|c| c.is_ascii_digit())
                && !next.is_empty()
                && next.chars().all(|c| c.is_ascii_digit())
            {
                result.push(format!("{}.{}", current, next));
                i += 2;
                continue;
            }
        }
        if !current.is_empty() {
            result.push(current.to_string());
        }
        i += 1;
    }

    result
        .iter()
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// 将 PEM 字符串中的字面 `\n` 还原为真实换行。
///
/// 对应 Node `normalizePem`。env 变量常以 `\\n` 序列存储 .p8 密钥。
pub fn normalize_pem(value: &str) -> String {
    value.replace("\\n", "\n").trim().to_string()
}

/// 格式化 project label: `-`/`_` → 空格, 每个词首字母大写。
///
/// 对应 Node `formatProjectLabel`。
pub fn format_project_label(label: &str) -> String {
    if label.is_empty() {
        return String::new();
    }
    let replaced = label.replace(['-', '_'], " ");
    let mut result = String::with_capacity(replaced.len());
    let mut capitalize_next = true;
    for ch in replaced.chars() {
        if ch.is_whitespace() {
            capitalize_next = true;
            result.push(ch);
        } else if capitalize_next {
            for upper in ch.to_uppercase() {
                result.push(upper);
            }
            capitalize_next = false;
        } else {
            result.push(ch);
        }
    }
    result
}

/// 判断平台是否为移动端 (ios/android)。
pub fn is_mobile_platform(platform: Option<&str>) -> bool {
    matches!(platform, Some("ios") | Some("android"))
}

// =========================================================================
// 测试
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_push_subscribe_valid() {
        let body = json!({
            "endpoint": "https://fcm.googleapis.com/fcm/send/abc",
            "keys": { "p256dh": "key1", "auth": "auth1" }
        });
        let result = parse_push_subscribe_body(&body);
        assert_eq!(
            result,
            Some((
                "https://fcm.googleapis.com/fcm/send/abc".to_string(),
                "key1".to_string(),
                "auth1".to_string()
            ))
        );
    }

    #[test]
    fn parse_push_subscribe_missing_keys() {
        let body = json!({ "endpoint": "ep" });
        assert!(parse_push_subscribe_body(&body).is_none());
    }

    #[test]
    fn parse_push_subscribe_empty_endpoint() {
        let body = json!({ "endpoint": "  ", "keys": {"p256dh":"k","auth":"a"} });
        assert!(parse_push_subscribe_body(&body).is_none());
    }

    #[test]
    fn parse_push_unsubscribe_valid() {
        let body = json!({ "endpoint": "  ep123  " });
        assert_eq!(
            parse_push_unsubscribe_body(&body),
            Some("ep123".to_string())
        );
    }

    #[test]
    fn extract_session_id_from_info() {
        let payload = json!({
            "type": "message.updated",
            "properties": { "info": { "sessionID": "sess-123" } }
        });
        assert_eq!(
            extract_session_id_from_payload(&payload),
            Some("sess-123".to_string())
        );
    }

    #[test]
    fn extract_session_id_from_props() {
        let payload = json!({
            "type": "session.idle",
            "properties": { "sessionID": "sess-456" }
        });
        assert_eq!(
            extract_session_id_from_payload(&payload),
            Some("sess-456".to_string())
        );
    }

    #[test]
    fn extract_session_id_none() {
        let payload = json!({ "type": "other", "properties": {} });
        assert!(extract_session_id_from_payload(&payload).is_none());
    }

    #[test]
    fn extract_directory_from_props() {
        let payload = json!({
            "properties": { "directory": "/work/dir" }
        });
        assert_eq!(
            extract_directory_from_payload(&payload),
            Some("/work/dir".to_string())
        );
    }

    #[test]
    fn extract_directory_empty_returns_none() {
        let payload = json!({ "properties": { "directory": "  " } });
        assert!(extract_directory_from_payload(&payload).is_none());
    }

    #[test]
    fn format_mode_default() {
        assert_eq!(format_mode(""), "Agent");
    }

    #[test]
    fn format_mode_split() {
        assert_eq!(format_mode("plan-mode"), "Plan Mode");
        assert_eq!(format_mode("build_agent"), "Build Agent");
    }

    #[test]
    fn format_model_id_default() {
        assert_eq!(format_model_id(""), "Assistant");
    }

    #[test]
    fn format_model_id_with_version() {
        assert_eq!(format_model_id("claude-3-5-sonnet"), "Claude 3.5 Sonnet");
    }

    #[test]
    fn format_model_id_simple() {
        assert_eq!(format_model_id("gpt-4"), "Gpt 4");
    }

    #[test]
    fn normalize_pem_restores_newlines() {
        let input = "-----BEGIN PRIVATE KEY-----\\nMIIE\\n-----END-----";
        let result = normalize_pem(input);
        assert!(result.contains('\n'));
        assert!(!result.contains("\\n"));
    }

    #[test]
    fn format_project_label_basic() {
        assert_eq!(format_project_label("my-cool-project"), "My Cool Project");
        assert_eq!(format_project_label("under_score"), "Under Score");
    }

    #[test]
    fn format_project_label_empty() {
        assert_eq!(format_project_label(""), "");
    }

    #[test]
    fn is_mobile_check() {
        assert!(is_mobile_platform(Some("ios")));
        assert!(is_mobile_platform(Some("android")));
        assert!(!is_mobile_platform(Some("web")));
        assert!(!is_mobile_platform(None));
    }

    #[test]
    fn get_parent_id_from_session_updated() {
        let payload = json!({
            "type": "session.updated",
            "properties": { "info": { "parentID": "parent-1" } }
        });
        assert_eq!(
            get_parent_id_from_payload(&payload),
            Some(Some("parent-1".to_string()))
        );
    }

    #[test]
    fn get_parent_id_no_parent() {
        let payload = json!({
            "type": "session.updated",
            "properties": { "info": {} }
        });
        assert_eq!(get_parent_id_from_payload(&payload), Some(None));
    }

    #[test]
    fn get_parent_id_wrong_type() {
        let payload = json!({ "type": "message.updated" });
        assert!(get_parent_id_from_payload(&payload).is_none());
    }
}
