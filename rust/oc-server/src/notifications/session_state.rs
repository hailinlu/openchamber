//! Session 状态机 — activity phase + status + attention + viewed。
//!
//! 对应 Node `lib/opencode/session-runtime.js` (`createSessionRuntime`)。
//! 全内存状态, `Mutex<HashMap>`。
//!
//! 核心数据结构:
//! - `activity_phases`: `Map<sessionId, {phase: busy/cooldown/idle, updatedAt}>`
//! - `session_states`: `Map<sessionId, {status, lastUpdateAt, lastEventId, metadata}>`
//! - `attention_states`: `Map<sessionId, AttentionState>`
//! - cooldown 定时器 (2s) 由 tokio task 实现

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use serde_json::{json, Value};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use super::{now_millis, SESSION_ATTENTION_MAX_AGE_MS, SESSION_COOLDOWN_DURATION_MS, SESSION_STATE_MAX_AGE_MS};

/// activity phase: busy/cooldown/idle。
#[derive(Debug, Clone)]
struct ActivityPhaseEntry {
    phase: String,
    updated_at: i64,
}

/// session 状态条目。
#[derive(Debug, Clone)]
struct SessionStateEntry {
    status: String,
    last_update_at: i64,
    last_event_id: String,
    metadata: Value,
}

/// attention 状态。
#[derive(Debug, Clone)]
struct AttentionState {
    needs_attention: bool,
    last_user_message_at: Option<i64>,
    last_status_change_at: i64,
    viewed_by_clients: HashSet<String>,
    status: String,
}

impl Default for AttentionState {
    fn default() -> Self {
        Self {
            needs_attention: false,
            last_user_message_at: None,
            last_status_change_at: now_millis(),
            viewed_by_clients: HashSet::new(),
            status: "idle".to_string(),
        }
    }
}

/// Session 状态运行时。
pub struct SessionStateRuntime {
    activity_phases: Mutex<HashMap<String, ActivityPhaseEntry>>,
    session_states: Mutex<HashMap<String, SessionStateEntry>>,
    attention_states: Mutex<HashMap<String, AttentionState>>,
    /// cooldown 定时器 handle (per session)。
    cooldown_handles: Mutex<HashMap<String, JoinHandle<()>>>,
    /// 事件广播 channel (SSE 事件: openchamber:session-status, openchamber:session-activity)。
    event_tx: broadcast::Sender<Value>,
}

impl SessionStateRuntime {
    pub fn new() -> Self {
        let (event_tx, _) = broadcast::channel(256);
        Self {
            activity_phases: Mutex::new(HashMap::new()),
            session_states: Mutex::new(HashMap::new()),
            attention_states: Mutex::new(HashMap::new()),
            cooldown_handles: Mutex::new(HashMap::new()),
            event_tx,
        }
    }

    /// 订阅 SSE 事件 (openchamber:session-status / openchamber:session-activity)。
    pub fn subscribe_events(&self) -> broadcast::Receiver<Value> {
        self.event_tx.subscribe()
    }

    /// 广播事件。
    fn broadcast_event(&self, payload: Value) {
        let _ = self.event_tx.send(payload);
    }

    // -----------------------------------------------------------------------
    // Activity phase (busy → cooldown → idle)
    // -----------------------------------------------------------------------

    /// 设置 activity phase。
    ///
    /// 对应 Node `setSessionActivityPhase`。
    /// busy → cooldown (2s) → idle。
    /// 返回 true 如果状态变更了。
    fn set_activity_phase(self: &std::sync::Arc<Self>, session_id: &str, phase: &str) -> bool {
        if session_id.is_empty() {
            return false;
        }

        let mut phases = self.activity_phases.lock().unwrap();
        let current = phases.get(session_id);

        // 相同 phase → 无变化
        if current.map(|c| c.phase == phase).unwrap_or(false) {
            return false;
        }

        // cooldown 只能从 busy 转入
        if phase == "cooldown" && current.map(|c| c.phase != "busy").unwrap_or(true) {
            return false;
        }

        // 取消已有的 cooldown 定时器
        let mut handles = self.cooldown_handles.lock().unwrap();
        if let Some(handle) = handles.remove(session_id) {
            handle.abort();
        }

        phases.insert(
            session_id.to_string(),
            ActivityPhaseEntry {
                phase: phase.to_string(),
                updated_at: now_millis(),
            },
        );
        drop(phases);

        // cooldown → 启动 2s 定时器后转 idle
        if phase == "cooldown" {
            let self_clone = self.clone();
            let sid = session_id.to_string();
            let handle = tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(SESSION_COOLDOWN_DURATION_MS)).await;
                let phases = self_clone.activity_phases.lock().unwrap();
                let is_cooldown = phases
                    .get(&sid)
                    .map(|c| c.phase == "cooldown")
                    .unwrap_or(false);
                drop(phases);
                if is_cooldown {
                    self_clone.set_activity_phase(&sid, "idle");
                } else {
                    self_clone.cooldown_handles.lock().unwrap().remove(&sid);
                }
            });
            handles.insert(session_id.to_string(), handle);
        }

        // 广播 openchamber:session-activity
        self.broadcast_event(json!({
            "type": "openchamber:session-activity",
            "properties": {
                "sessionId": session_id,
                "phase": phase,
            }
        }));

        true
    }

    // -----------------------------------------------------------------------
    // Session state + attention
    // -----------------------------------------------------------------------

    /// 更新 attention 状态。
    fn update_attention_status(&self, session_id: &str, status: &str) {
        let mut states = self.attention_states.lock().unwrap();
        let state = states.entry(session_id.to_string()).or_default();
        let prev_status = state.status.clone();
        state.status = status.to_string();
        state.last_status_change_at = now_millis();

        // busy/retry → idle 且有用户消息且无客户端查看 → needsAttention
        if (prev_status == "busy" || prev_status == "retry")
            && status == "idle"
            && state.last_user_message_at.is_some()
            && state.viewed_by_clients.is_empty()
        {
            state.needs_attention = true;
        }
    }

    /// 更新 session 状态。
    ///
    /// 对应 Node `updateSessionState`。由 `process_sse_payload` 调用。
    pub fn update_session_state(
        self: &std::sync::Arc<Self>,
        session_id: &str,
        status: &str,
        event_id: Option<&str>,
        metadata: Value,
    ) {
        if session_id.is_empty() {
            return;
        }

        let now = now_millis();
        let should_skip = {
            let states = self.session_states.lock().unwrap();
            if let Some(existing) = states.get(session_id) {
                existing.last_update_at > now - 5000 && status == existing.status
            } else {
                false
            }
        };
        if should_skip {
            return;
        }

        let prev_needs_attention = {
            self.attention_states.lock().unwrap().get(session_id)
        }.map(|s| s.needs_attention);

        // 写入 session state
        let effective_event_id = event_id
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("server-{}", now));

        {
            let mut states = self.session_states.lock().unwrap();
            let existing = states.get(session_id).cloned();
            let merged_metadata = if let Some(ref ex) = existing {
                merge_metadata(&ex.metadata, &metadata)
            } else {
                metadata
            };
            states.insert(
                session_id.to_string(),
                SessionStateEntry {
                    status: status.to_string(),
                    last_update_at: now,
                    last_event_id: effective_event_id,
                    metadata: merged_metadata,
                },
            );
        }

        self.update_attention_status(session_id, status);

        // 广播 session-status (如果状态变更或 attention 变更)
        let needs_attention = {
            self.attention_states.lock().unwrap().get(session_id)
        }.map(|s| s.needs_attention).unwrap_or(false);

        let attention_changed = prev_needs_attention.map(|p| p != needs_attention).unwrap_or(false);

        let status_changed = {
            let states = self.session_states.lock().unwrap();
            states.get(session_id).map(|s| s.status.as_str()) != Some(status)
                || prev_needs_attention.is_none()
        };

        // 判断是否需要广播 (新 session 或 状态变更 或 attention 变更)
        let should_broadcast = prev_needs_attention.is_none() || status_changed || attention_changed;
        if should_broadcast {
            let (status_val, metadata_val) = {
                let states = self.session_states.lock().unwrap();
                let entry = states.get(session_id);
                (
                    entry.map(|e| e.status.clone()).unwrap_or_default(),
                    entry.map(|e| e.metadata.clone()).unwrap_or(Value::Null),
                )
            };
            self.broadcast_event(json!({
                "type": "openchamber:session-status",
                "properties": {
                    "sessionID": session_id,
                    "status": status_val,
                    "timestamp": now,
                    "metadata": metadata_val,
                    "needsAttention": needs_attention,
                }
            }));
        }

        // 更新 activity phase
        let phase = if status == "busy" || status == "retry" {
            "busy"
        } else {
            "idle"
        };
        // idle 不打断 cooldown
        let skip_phase = phase == "idle" && {
            self.activity_phases.lock().unwrap()
                .get(session_id)
                .map(|p| p.phase == "cooldown")
                .unwrap_or(false)
        };
        if !skip_phase {
            self.set_activity_phase(session_id, phase);
        }
    }

    // -----------------------------------------------------------------------
    // Viewed / unviewed / message sent
    // -----------------------------------------------------------------------

    /// 标记 session 已查看。
    ///
    /// 对应 Node `markSessionViewed`。如果 needsAttention 为 true, 清除并广播。
    pub fn mark_session_viewed(&self, session_id: &str, client_id: &str) {
        let mut states = self.attention_states.lock().unwrap();
        let state = states.entry(session_id.to_string()).or_default();
        let was_needs_attention = state.needs_attention;
        state.viewed_by_clients.insert(client_id.to_string());

        if was_needs_attention {
            state.needs_attention = false;
            let now = now_millis();
            drop(states);
            self.broadcast_event(json!({
                "type": "openchamber:session-status",
                "properties": {
                    "sessionID": session_id,
                    "status": "idle",
                    "timestamp": now,
                    "metadata": {},
                    "needsAttention": false,
                }
            }));
        }
    }

    /// 标记 session 未查看。
    pub fn mark_session_unviewed(&self, session_id: &str, client_id: &str) {
        let mut states = self.attention_states.lock().unwrap();
        if let Some(state) = states.get_mut(session_id) {
            state.viewed_by_clients.remove(client_id);
        }
    }

    /// 标记用户消息已发送。
    pub fn mark_user_message_sent(&self, session_id: &str) {
        let mut states = self.attention_states.lock().unwrap();
        let state = states.entry(session_id.to_string()).or_default();
        state.last_user_message_at = Some(now_millis());
    }

    // -----------------------------------------------------------------------
    // Snapshots
    // -----------------------------------------------------------------------

    /// 获取 session activity 快照。
    ///
    /// 对应 Node `getSessionActivitySnapshot`。
    pub fn get_activity_snapshot(&self) -> Value {
        let phases = self.activity_phases.lock().unwrap();
        let mut result = serde_json::Map::new();
        for (sid, data) in phases.iter() {
            result.insert(sid.clone(), json!({ "type": data.phase }));
        }
        Value::Object(result)
    }

    /// 获取 session 状态快照 (排除 >24h 的)。
    ///
    /// 对应 Node `getSessionStateSnapshot`。
    pub fn get_state_snapshot(&self) -> Value {
        let now = now_millis();
        let states = self.session_states.lock().unwrap();
        let mut result = serde_json::Map::new();
        for (sid, data) in states.iter() {
            if now - data.last_update_at > SESSION_STATE_MAX_AGE_MS {
                continue;
            }
            result.insert(
                sid.clone(),
                json!({
                    "status": data.status,
                    "lastUpdateAt": data.last_update_at,
                    "metadata": data.metadata.clone(),
                }),
            );
        }
        Value::Object(result)
    }

    /// 获取单个 session 状态。
    pub fn get_session_state(&self, session_id: &str) -> Option<Value> {
        if session_id.is_empty() {
            return None;
        }
        let states = self.session_states.lock().unwrap();
        states.get(session_id).map(|s| {
            json!({
                "status": s.status,
                "lastUpdateAt": s.last_update_at,
                "lastEventId": s.last_event_id,
                "metadata": s.metadata,
            })
        })
    }

    /// 获取 attention 快照 (排除 >24h 的)。
    ///
    /// 对应 Node `getSessionAttentionSnapshot`。
    pub fn get_attention_snapshot(&self) -> Value {
        let now = now_millis();
        let states = self.attention_states.lock().unwrap();
        let mut result = serde_json::Map::new();
        for (sid, state) in states.iter() {
            if now - state.last_status_change_at > SESSION_ATTENTION_MAX_AGE_MS {
                continue;
            }
            result.insert(
                sid.clone(),
                json!({
                    "needsAttention": state.needs_attention,
                    "lastUserMessageAt": state.last_user_message_at,
                    "lastStatusChangeAt": state.last_status_change_at,
                    "status": state.status,
                    "isViewed": !state.viewed_by_clients.is_empty(),
                }),
            );
        }
        Value::Object(result)
    }

    /// 获取单个 session attention 状态。
    pub fn get_attention_state(&self, session_id: &str) -> Option<Value> {
        if session_id.is_empty() {
            return None;
        }
        let states = self.attention_states.lock().unwrap();
        states.get(session_id).map(|s| {
            json!({
                "needsAttention": s.needs_attention,
                "lastUserMessageAt": s.last_user_message_at,
                "lastStatusChangeAt": s.last_status_change_at,
                "status": s.status,
                "isViewed": !s.viewed_by_clients.is_empty(),
            })
        })
    }

    // -----------------------------------------------------------------------
    // SSE payload 处理
    // -----------------------------------------------------------------------

    /// 从 session.status payload 提取状态更新。
    ///
    /// 对应 Node `extractSessionStatusUpdate`。
    fn extract_session_status_update(payload: &Value) -> Option<(String, String, Option<String>, Value)> {
        if payload.get("type").and_then(|v| v.as_str()) != Some("session.status") {
            return None;
        }

        let props = payload.get("properties")?.as_object()?;
        let status = props.get("status").and_then(|v| v.as_object());
        let info = props.get("info").and_then(|v| v.as_object());

        let session_id = props.get("sessionID").and_then(|v| v.as_str())?.trim();
        if session_id.is_empty() {
            return None;
        }

        // status.type 优先, info.type 回退
        let type_str = status
            .and_then(|s| s.get("type"))
            .and_then(|v| v.as_str())
            .or_else(|| info.and_then(|i| i.get("type")).and_then(|v| v.as_str()))?
            .trim()
            .to_string();

        if type_str.is_empty() {
            return None;
        }

        let event_id = payload
            .get("id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());

        // 构建 metadata
        let attempt = status
            .and_then(|s| s.get("attempt"))
            .or_else(|| info.and_then(|i| i.get("attempt")));
        let message = status
            .and_then(|s| s.get("message"))
            .or_else(|| info.and_then(|i| i.get("message")));
        let next = status
            .and_then(|s| s.get("next"))
            .or_else(|| info.and_then(|i| i.get("next")));

        let mut metadata = serde_json::Map::new();
        if let Some(a) = attempt {
            metadata.insert("attempt".to_string(), a.clone());
        }
        if let Some(m) = message {
            metadata.insert("message".to_string(), m.clone());
        }
        if let Some(n) = next {
            metadata.insert("next".to_string(), n.clone());
        }

        Some((
            session_id.to_string(),
            type_str,
            event_id,
            Value::Object(metadata),
        ))
    }

    /// 处理 OpenCode SSE payload, 更新 session 状态。
    ///
    /// 对应 Node `processOpenCodeSsePayload`。由 trigger fanout 调用。
    pub fn process_sse_payload(self: &std::sync::Arc<Self>, payload: &Value) {
        // activity phase 更新 (session.status: busy/retry → busy, idle → cooldown)
        let payload_type = payload.get("type").and_then(|v| v.as_str());
        if payload_type == Some("session.status") {
            if let Some((session_id, update_type, _, _)) = Self::extract_session_status_update(payload) {
                if update_type == "busy" || update_type == "retry" {
                    self.set_activity_phase(&session_id, "busy");
                } else if update_type == "idle" {
                    self.set_activity_phase(&session_id, "cooldown");
                }
            }
        }

        // session state 更新
        if let Some((session_id, update_type, event_id, metadata)) =
            Self::extract_session_status_update(payload)
        {
            self.update_session_state(
                &session_id,
                &update_type,
                event_id.as_deref(),
                metadata,
            );
        }
    }
}

/// 合并 metadata (浅合并, 新值覆盖旧值)。
fn merge_metadata(base: &Value, overlay: &Value) -> Value {
    match (base, overlay) {
        (Value::Object(base_map), Value::Object(overlay_map)) => {
            let mut result = base_map.clone();
            for (k, v) in overlay_map {
                result.insert(k.clone(), v.clone());
            }
            Value::Object(result)
        }
        (_, overlay) => overlay.clone(),
    }
}

impl Default for SessionStateRuntime {
    fn default() -> Self {
        Self::new()
    }
}

// =========================================================================
// 测试
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_snapshot_empty() {
        let rt = SessionStateRuntime::new();
        assert_eq!(rt.get_activity_snapshot(), json!({}));
    }

    #[test]
    fn state_snapshot_empty() {
        let rt = SessionStateRuntime::new();
        assert_eq!(rt.get_state_snapshot(), json!({}));
    }

    #[test]
    fn mark_viewed_sets_viewed() {
        let rt = SessionStateRuntime::new();
        rt.mark_session_viewed("sess1", "client1");
        let attention = rt.get_attention_state("sess1").unwrap();
        assert_eq!(attention.get("isViewed").and_then(|v| v.as_bool()), Some(true));
    }

    #[test]
    fn mark_unviewed_removes_client() {
        let rt = SessionStateRuntime::new();
        rt.mark_session_viewed("sess1", "client1");
        rt.mark_session_unviewed("sess1", "client1");
        let attention = rt.get_attention_state("sess1").unwrap();
        assert_eq!(attention.get("isViewed").and_then(|v| v.as_bool()), Some(false));
    }

    #[test]
    fn mark_message_sent_sets_timestamp() {
        let rt = SessionStateRuntime::new();
        rt.mark_user_message_sent("sess1");
        let attention = rt.get_attention_state("sess1").unwrap();
        assert!(attention.get("lastUserMessageAt").and_then(|v| v.as_i64()).is_some());
    }

    #[test]
    fn get_session_state_not_found() {
        let rt = SessionStateRuntime::new();
        assert!(rt.get_session_state("nonexistent").is_none());
    }

    #[test]
    fn extract_status_update_valid() {
        let payload = json!({
            "type": "session.status",
            "id": "evt-1",
            "properties": {
                "sessionID": "sess-1",
                "status": { "type": "busy", "attempt": 1 }
            }
        });
        let result = SessionStateRuntime::extract_session_status_update(&payload);
        assert!(result.is_some());
        let (sid, t, eid, meta) = result.unwrap();
        assert_eq!(sid, "sess-1");
        assert_eq!(t, "busy");
        assert_eq!(eid, Some("evt-1".to_string()));
        assert_eq!(meta.get("attempt").and_then(|v| v.as_i64()), Some(1));
    }

    #[test]
    fn extract_status_update_wrong_type() {
        let payload = json!({ "type": "message.updated" });
        assert!(SessionStateRuntime::extract_session_status_update(&payload).is_none());
    }

    #[test]
    fn merge_metadata_overlays() {
        let base = json!({ "a": 1, "b": 2 });
        let overlay = json!({ "b": 3, "c": 4 });
        let result = merge_metadata(&base, &overlay);
        assert_eq!(result, json!({ "a": 1, "b": 3, "c": 4 }));
    }
}
