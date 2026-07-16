//! 终端 WebSocket 协议工具 — 控制帧编解码、消息归一化、重连速率限制。
//!
//! 对应 Node `terminal-ws-protocol.js`。
//!
//! 帧格式: `[0x01 tag byte][UTF-8 JSON]`。
//! 文本帧 = 终端输入 (写入 PTY); 二进制帧首字节 `0x01` = 控制帧, 否则 = 原始输入。

use serde_json::Value;

/// 控制帧 JSON 标签字节 (首字节为该值表示 JSON 控制帧)。
pub const TERMINAL_WS_CONTROL_TAG_JSON: u8 = 0x01;

/// 重连时间戳窗口内的最大重连次数。
pub fn is_rebind_rate_limited(timestamps_len: usize, max_per_window: usize) -> bool {
    timestamps_len >= max_per_window
}

/// 裁剪重连时间戳, 仅保留窗口内的。
pub fn prune_rebind_timestamps(
    timestamps: &[u128],
    now_ms: u128,
    window_ms: u128,
) -> Vec<u128> {
    timestamps
        .iter()
        .copied()
        .filter(|&ts| now_ms.saturating_sub(ts) < window_ms)
        .collect()
}

/// 从二进制帧解析控制帧 JSON。
///
/// 返回 `Some(Value)` 仅当: 长度 ≥ 2、首字节 == `TERMINAL_WS_CONTROL_TAG_JSON`、
/// 尾部是合法 JSON 对象。否则返回 `None`。
pub fn read_control_frame(raw: &[u8]) -> Option<Value> {
    if raw.len() < 2 || raw[0] != TERMINAL_WS_CONTROL_TAG_JSON {
        return None;
    }
    let json_bytes = &raw[1..];
    let parsed: Value = serde_json::from_slice(json_bytes).ok()?;
    if !parsed.is_object() {
        return None;
    }
    Some(parsed)
}

/// 构造控制帧 (tag byte + JSON)。
pub fn create_control_frame(payload: &Value) -> Vec<u8> {
    let json = serde_json::to_vec(payload).unwrap_or_else(|_| b"{}".to_vec());
    let mut out = Vec::with_capacity(1 + json.len());
    out.push(TERMINAL_WS_CONTROL_TAG_JSON);
    out.extend_from_slice(&json);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ----- read_control_frame -------------------------------------------------

    #[test]
    fn read_control_frame_parses_valid_frame() {
        let payload = json!({"t": "b", "s": "sess-1", "r": 5});
        let mut raw = vec![TERMINAL_WS_CONTROL_TAG_JSON];
        raw.extend_from_slice(&serde_json::to_vec(&payload).unwrap());
        let parsed = read_control_frame(&raw).unwrap();
        assert_eq!(parsed["t"], "b");
        assert_eq!(parsed["s"], "sess-1");
        assert_eq!(parsed["r"], 5);
    }

    #[test]
    fn read_control_frame_rejects_short_buffer() {
        assert!(read_control_frame(&[TERMINAL_WS_CONTROL_TAG_JSON]).is_none());
        assert!(read_control_frame(&[]).is_none());
    }

    #[test]
    fn read_control_frame_rejects_wrong_tag() {
        let raw = b"\x02{\"t\":\"p\"}";
        assert!(read_control_frame(raw).is_none());
    }

    #[test]
    fn read_control_frame_rejects_malformed_json() {
        let mut raw = vec![TERMINAL_WS_CONTROL_TAG_JSON];
        raw.extend_from_slice(b"{ broken");
        assert!(read_control_frame(&raw).is_none());
    }

    #[test]
    fn read_control_frame_rejects_non_object_json() {
        let mut raw = vec![TERMINAL_WS_CONTROL_TAG_JSON];
        raw.extend_from_slice(b"42");
        assert!(read_control_frame(&raw).is_none());
        let mut raw2 = vec![TERMINAL_WS_CONTROL_TAG_JSON];
        raw2.extend_from_slice(b"[1,2,3]");
        assert!(read_control_frame(&raw2).is_none());
        let mut raw3 = vec![TERMINAL_WS_CONTROL_TAG_JSON];
        raw3.extend_from_slice(b"\"hello\"");
        assert!(read_control_frame(&raw3).is_none());
    }

    // ----- create_control_frame ------------------------------------------------

    #[test]
    fn create_control_frame_round_trips() {
        let payload = json!({"t": "d", "s": "sess-1", "i": 3, "d": "hello"});
        let frame = create_control_frame(&payload);
        assert_eq!(frame[0], TERMINAL_WS_CONTROL_TAG_JSON);
        let parsed = read_control_frame(&frame).unwrap();
        assert_eq!(parsed, payload);
    }

    // ----- rebind rate limiting ------------------------------------------------

    #[test]
    fn prune_drops_old_timestamps() {
        let timestamps = vec![100, 200, 300, 400];
        let pruned = prune_rebind_timestamps(&timestamps, 500, 150);
        // 窗口 [350, 500), 仅保留 400
        assert_eq!(pruned, vec![400]);
    }

    #[test]
    fn prune_keeps_all_when_window_large() {
        let timestamps = vec![100, 200, 300];
        let pruned = prune_rebind_timestamps(&timestamps, 300, 1000);
        assert_eq!(pruned, vec![100, 200, 300]);
    }

    #[test]
    fn prune_empty_returns_empty() {
        let pruned = prune_rebind_timestamps(&[], 500, 150);
        assert!(pruned.is_empty());
    }

    #[test]
    fn is_rate_limited_at_threshold() {
        // 对齐 Node: timestamps.length >= maxPerWindow 即限流。
        assert!(!is_rebind_rate_limited(2, 3));
        assert!(is_rebind_rate_limited(3, 3)); // 达到阈值即限流
        assert!(is_rebind_rate_limited(4, 3));
    }
}
