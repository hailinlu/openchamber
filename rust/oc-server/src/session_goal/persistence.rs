//! Session metadata 合并读写 helper — 供 session-goal / session-assist 共用。
//!
//! 与 Node 端一致: 读取 `metadata.gridforge.<key>` 时不破坏其他 openchamber 子字段
//! (assist / dismissals / review 等)。实施方式:
//!   1. GET /session/{id}?directory=...
//!   2. 在内存中合并 `{...current.openchamber, <key>: <value>}`
//!   3. PATCH /session/{id}?directory=... body `{metadata: {openchamber: {...merged}}}`
//!
//! 该 deep merge 模式保证并发 metadata 写入 (assist + dismissals + review) 不会互相覆盖。
//!
//! 当前 G3 只在 session-goal runtime 内使用; session-assist 在写 metadata 时复用本模块。

#![allow(dead_code)]

use serde_json::{json, Map, Value};

use crate::error::ApiError;
use crate::opencode::session_client::{build, OpenCodeClient};
use crate::state::AppState;

/// 读取 session 的 `metadata.gridforge` 子对象 (完整 namespace)。
///
/// 返回 None 表示 session 不存在或 metadata 不存在。
pub async fn read_session_gridforge_metadata(
    state: &AppState,
    session_id: &str,
    directory: Option<&str>,
) -> Result<Option<Value>, ApiError> {
    let client = build(state);
    let Some(session) = client.fetch_session(session_id, directory).await? else {
        return Ok(None);
    };
    Ok(session
        .get("metadata")
        .and_then(|m| m.get("gridforge"))
        .cloned())
}

/// 把 `key: value` 合并写入 session `metadata.gridforge`。
///
/// 保留现有 `openchamber.*` 其他子字段。Session 不存在 → 返回 Ok(false)。
///
/// 返回值: Ok(true) = 写入成功, Ok(false) = session 不存在, Err = IO/HTTP 错误。
pub async fn patch_session_gridforge_metadata(
    state: &AppState,
    session_id: &str,
    directory: Option<&str>,
    key: &str,
    value: &Value,
) -> Result<bool, ApiError> {
    let client = build(state);
    let Some(session) = client.fetch_session(session_id, directory).await? else {
        return Ok(false);
    };
    let merged = merge_key_into_gridforge(&session, key, value);
    client
        .patch_session_metadata(session_id, directory, &merged)
        .await?;
    Ok(true)
}

/// 把多个 key-value 合并写入 session `metadata.gridforge` (一次性)。
///
/// 行为与 `patch_session_gridforge_metadata` 类似, 但接受 map 而非单一 key。
pub async fn patch_session_gridforge_metadata_map(
    state: &AppState,
    session_id: &str,
    directory: Option<&str>,
    updates: &Map<String, Value>,
) -> Result<bool, ApiError> {
    let client = build(state);
    let Some(session) = client.fetch_session(session_id, directory).await? else {
        return Ok(false);
    };
    let merged = merge_map_into_gridforge(&session, updates);
    client
        .patch_session_metadata(session_id, directory, &merged)
        .await?;
    Ok(true)
}

/// 在 session metadata.gridforge 上合并一个 key。
///
/// 暴露 helper 供 `session-goal/mod.rs` 的 `writeGoal` 复用 (避免重复 IO)。
pub fn merge_key_into_gridforge(session: &Value, key: &str, value: &Value) -> Value {
    let mut updates = Map::new();
    updates.insert(key.to_string(), value.clone());
    merge_map_into_gridforge(session, &updates)
}

/// 在 session metadata.gridforge 上合并多个 key-value。
pub fn merge_map_into_gridforge(session: &Value, updates: &Map<String, Value>) -> Value {
    let metadata = session
        .get("metadata")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_else(Map::new);
    let mut metadata = metadata;

    let namespace = metadata
        .get("gridforge")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_else(Map::new);
    let mut namespace = namespace;
    for (k, v) in updates {
        namespace.insert(k.clone(), v.clone());
    }

    metadata.insert("gridforge".to_string(), Value::Object(namespace));
    json!({ "metadata": Value::Object(metadata) })
}

/// Read-only helper: 从 session 直接读取 `openchamber.<key>` 子对象 (单 key)。
pub async fn read_session_gridforge_key(
    state: &AppState,
    session_id: &str,
    directory: Option<&str>,
    key: &str,
) -> Result<Option<Value>, ApiError> {
    let Some(namespace) = read_session_gridforge_metadata(state, session_id, directory).await? else {
        return Ok(None);
    };
    Ok(namespace.get(key).cloned())
}

/// 兼容 helper — 复用现有 `OpenCodeClient` 命名空间, 供 caller 直接调用。
#[allow(dead_code)]
pub fn opencode_client<'a>(state: &'a AppState) -> OpenCodeClient<'a> {
    build(state)
}

// =========================================================================
// 测试 (合并 helper 不需 IO)
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn merge_preserves_existing_gridforge_fields() {
        let session = json!({
            "id": "s1",
            "metadata": {
                "gridforge": {
                    "assist": {"recap": "x"},
                    "dismissals": ["msg_1"]
                },
                "other": "v"
            }
        });
        let merged = merge_key_into_gridforge(&session, "goal", &json!({"id": "g1"}));
        assert_eq!(merged["metadata"]["gridforge"]["goal"], json!({"id": "g1"}));
        assert_eq!(merged["metadata"]["gridforge"]["assist"]["recap"], "x");
        assert_eq!(merged["metadata"]["gridforge"]["dismissals"][0], "msg_1");
        assert_eq!(merged["metadata"]["other"], "v");
    }

    #[test]
    fn merge_creates_namespace_when_missing() {
        let session = json!({"id": "s2"});
        let merged = merge_key_into_gridforge(&session, "goal", &json!({"id": "g"}));
        assert_eq!(merged["metadata"]["gridforge"]["goal"], json!({"id": "g"}));
    }

    #[test]
    fn merge_multiple_keys_at_once() {
        let session = json!({"metadata": {"gridforge": {"assist": {"recap": "x"}}}});
        let mut updates = Map::new();
        updates.insert("goal".to_string(), json!({"id": "g"}));
        updates.insert("review".to_string(), json!({"rating": 5}));
        let merged = merge_map_into_gridforge(&session, &updates);
        assert_eq!(merged["metadata"]["gridforge"]["goal"], json!({"id": "g"}));
        assert_eq!(merged["metadata"]["gridforge"]["review"], json!({"rating": 5}));
        assert_eq!(merged["metadata"]["gridforge"]["assist"]["recap"], "x");
    }

    #[test]
    fn merge_overwrites_same_key() {
        let session = json!({"metadata": {"gridforge": {"goal": {"id": "old"}}}});
        let merged = merge_key_into_gridforge(&session, "goal", &json!({"id": "new"}));
        assert_eq!(merged["metadata"]["gridforge"]["goal"], json!({"id": "new"}));
    }
}