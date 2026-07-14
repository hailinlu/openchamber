//! WS 帧序列化 + SSE envelope 解析。
//!
//! 对应 `packages/web/server/lib/event-stream/protocol.js`。

use serde::Serialize;
use serde_json::Value;

// =========================================================================
// WS 帧类型 (对应 protocol.js 的 sendMessageStreamWsFrame 帧形状)
// =========================================================================

/// WS 帧类型 — JSON-over-text-frames, 4 种。
///
/// 浏览器侧类型联合见 `event-pipeline.ts:62-69`。
#[derive(Serialize, Debug, Clone)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum WsFrame {
    /// server→client: 上游已连接, 可以开始接收事件。
    Ready {
        scope: String,
    },
    /// server→client: 转发的 OpenCode 事件 (或合成 UI 事件)。
    Event {
        payload: Value,
        #[serde(skip_serializing_if = "Option::is_none", rename = "eventId")]
        event_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        directory: Option<String>,
    },
    /// server→client: 致命错误, 客户端应关闭。
    Error {
        message: String,
    },
    /// server→client: 一次性背压警告, 客户端应进入慢刷模式。
    Backpressure {
        #[serde(rename = "bufferedBytes")]
        buffered_bytes: usize,
        #[serde(rename = "maxBytes")]
        max_bytes: usize,
    },
}

impl WsFrame {
    /// 序列化为 JSON 字符串 (对应 `socket.send(JSON.stringify(payload))`)。
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".into())
    }
}

// =========================================================================
// SSE envelope 解析 (对应 protocol.js 的 parseSseEventEnvelope)
// =========================================================================

/// 解析后的 SSE 事件信封。
#[derive(Debug, Clone)]
pub struct SseEnvelope {
    pub event_id: Option<String>,
    pub directory: Option<String>,
    pub payload: Value,
}

/// 解析一个 SSE 事件块 (以 `\n\n` 分隔的文本块)。
///
/// 对应 `parseSseEventEnvelope`:
/// 1. 提取 `id:` 行 → `event_id`
/// 2. 提取所有 `data:` 行 → `join('\n')` → `JSON.parse`
/// 3. 两种 payload shape:
///    a) `{ payload: {...}, directory? }` → `payload` 取内层, `directory` 取外层
///    b) bare event → `directory` 从 `parsed.directory` / `parsed.properties.directory`
///    / `parsed.properties.info.directory` 级联
/// 4. 解析失败返回 `None` (不抛异常)
pub fn parse_sse_block(block: &str) -> Option<SseEnvelope> {
    if block.is_empty() {
        return None;
    }

    let mut event_id: Option<String> = None;
    let mut data_lines: Vec<&str> = Vec::new();

    for line in block.split('\n') {
        if let Some(rest) = line.strip_prefix("id:") {
            let trimmed = rest.trim();
            if !trimmed.is_empty() {
                event_id = Some(trimmed.to_string());
            }
        } else if let Some(rest) = line.strip_prefix("data:") {
            // 对应 JS: `line.slice(5).replace(/^\s/, '')` — 去掉 data: 后第一个空格
            let value = if let Some(stripped) = rest.strip_prefix(' ') {
                stripped
            } else {
                rest
            };
            data_lines.push(value);
        }
    }

    if data_lines.is_empty() {
        return None;
    }

    let payload_text = data_lines.join("\n");
    let trimmed = payload_text.trim();
    if trimmed.is_empty() {
        return None;
    }

    let parsed: Value = serde_json::from_str(trimmed).ok()?;

    // Shape a) { payload: {...}, directory? }
    if let Some(payload_obj) = parsed.get("payload") {
        if payload_obj.is_object() {
            let directory = parsed
                .get("directory")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string());
            return Some(SseEnvelope {
                event_id,
                directory,
                payload: payload_obj.clone(),
            });
        }
    }

    // Shape b) bare event — directory 级联查找
    let directory = parsed
        .get("directory")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| {
            parsed
                .get("properties")
                .and_then(|p| p.get("directory"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
        })
        .or_else(|| {
            parsed
                .get("properties")
                .and_then(|p| p.get("info"))
                .and_then(|i| i.get("directory"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
        });

    Some(SseEnvelope {
        event_id,
        directory,
        payload: parsed,
    })
}

// =========================================================================
// 测试
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // --- WsFrame 序列化 ---

    #[test]
    fn ws_frame_ready_serializes() {
        let frame = WsFrame::Ready {
            scope: "global".into(),
        };
        let json = frame.to_json();
        assert_eq!(json, r#"{"type":"ready","scope":"global"}"#);
    }

    #[test]
    fn ws_frame_event_serializes_with_all_fields() {
        let frame = WsFrame::Event {
            payload: json!({"type": "session.updated"}),
            event_id: Some("evt-123".into()),
            directory: Some("/work/dir".into()),
        };
        let json = frame.to_json();
        assert!(json.contains(r#""type":"event""#));
        assert!(json.contains(r#""eventId":"evt-123""#));
        assert!(json.contains(r#""directory":"/work/dir""#));
    }

    #[test]
    fn ws_frame_event_skips_none_fields() {
        let frame = WsFrame::Event {
            payload: json!({"type": "ping"}),
            event_id: None,
            directory: None,
        };
        let json = frame.to_json();
        assert!(!json.contains("eventId"));
        assert!(!json.contains("directory"));
    }

    #[test]
    fn ws_frame_error_serializes() {
        let frame = WsFrame::Error {
            message: "OpenCode event stream unavailable".into(),
        };
        let json = frame.to_json();
        assert!(json.contains(r#""type":"error""#));
        assert!(json.contains(r#""message":"OpenCode event stream unavailable""#));
    }

    #[test]
    fn ws_frame_backpressure_serializes() {
        let frame = WsFrame::Backpressure {
            buffered_bytes: 13_000_000,
            max_bytes: 16_777_216,
        };
        let json = frame.to_json();
        assert!(json.contains(r#""type":"backpressure""#));
        assert!(json.contains(r#""bufferedBytes":13000000"#));
        assert!(json.contains(r#""maxBytes":16777216"#));
    }

    // --- parse_sse_block ---

    #[test]
    fn parse_sse_block_wrapped_payload() {
        let block = "id:evt-42\ndata: {\"payload\":{\"type\":\"session.updated\"},\"directory\":\"/work\"}";
        let env = parse_sse_block(block).unwrap();
        assert_eq!(env.event_id.as_deref(), Some("evt-42"));
        assert_eq!(env.directory.as_deref(), Some("/work"));
        assert_eq!(env.payload, json!({"type":"session.updated"}));
    }

    #[test]
    fn parse_sse_block_bare_payload_with_directory() {
        let block = "data: {\"type\":\"message\",\"directory\":\"/proj\"}";
        let env = parse_sse_block(block).unwrap();
        assert_eq!(env.directory.as_deref(), Some("/proj"));
        assert_eq!(env.payload["type"], "message");
    }

    #[test]
    fn parse_sse_block_bare_payload_properties_directory() {
        let block = "data: {\"type\":\"message\",\"properties\":{\"directory\":\"/via-props\"}}";
        let env = parse_sse_block(block).unwrap();
        assert_eq!(env.directory.as_deref(), Some("/via-props"));
    }

    #[test]
    fn parse_sse_block_bare_payload_properties_info_directory() {
        let block = "data: {\"type\":\"message\",\"properties\":{\"info\":{\"directory\":\"/deep\"}}}";
        let env = parse_sse_block(block).unwrap();
        assert_eq!(env.directory.as_deref(), Some("/deep"));
    }

    #[test]
    fn parse_sse_block_multi_data_lines() {
        let block = "id:evt-1\ndata: {\"part1\":\ndata: \"value\"}";
        let env = parse_sse_block(block).unwrap();
        assert_eq!(env.event_id.as_deref(), Some("evt-1"));
        assert_eq!(env.payload, json!({"part1": "value"}));
    }

    #[test]
    fn parse_sse_block_no_data_returns_none() {
        assert!(parse_sse_block("id:evt-1\n:event:ping").is_none());
    }

    #[test]
    fn parse_sse_block_empty_returns_none() {
        assert!(parse_sse_block("").is_none());
    }

    #[test]
    fn parse_sse_block_invalid_json_returns_none() {
        assert!(parse_sse_block("data: {not json}").is_none());
    }

    #[test]
    fn parse_sse_block_no_id() {
        let block = "data: {\"type\":\"event\"}";
        let env = parse_sse_block(block).unwrap();
        assert!(env.event_id.is_none());
    }

    #[test]
    fn parse_sse_block_wrapped_payload_null_directory() {
        let block = "data: {\"payload\":{\"type\":\"x\"},\"directory\":\"\"}";
        let env = parse_sse_block(block).unwrap();
        // 空字符串 directory 被过滤
        assert!(env.directory.is_none());
    }
}
