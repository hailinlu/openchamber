//! Magic prompts — JSON 文件原子读写 + prompt ID 校验。
//!
//! 对应 Node `magic-prompts/routes.js` (63 行) + `runtime.js` (119 行)。
//!
//! 路由:
//!  1. GET    /api/magic-prompts        — 读取 overrides
//!  2. PUT    /api/magic-prompts/{id}   — 设置 override
//!  3. DELETE /api/magic-prompts/{id}   — 重置单个 override
//!  4. DELETE /api/magic-prompts        — 重置全部 overrides

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::github::settings::data_dir;
use crate::state::AppState;

/// 文件版本。
const FILE_VERSION: u32 = 1;

/// prompt 文本最大长度。
const MAX_PROMPT_TEXT_LENGTH: usize = 200_000;

/// prompt ID 合法模式: `[a-z0-9._-]{1,160}`。
static PROMPT_ID_PATTERN: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^[a-z0-9._-]{1,160}$").unwrap());

/// `.visible` 后缀判断。
fn is_visible_prompt_id(id: &str) -> bool {
    id.ends_with(".visible")
}

/// 校验 prompt ID 是否合法。
fn is_valid_prompt_id(id: &str) -> bool {
    PROMPT_ID_PATTERN.is_match(id)
}

/// `magic-prompts.json` 文件路径。
fn file_path() -> std::path::PathBuf {
    data_dir().join("magic-prompts.json")
}

/// 从 JSON Value 规范化 overrides map (只保留合法 ID + string value)。
fn sanitize_overrides(value: &Value) -> HashMap<String, String> {
    let mut next = HashMap::new();
    if let Some(obj) = value.as_object() {
        for (key, entry) in obj {
            if !is_valid_prompt_id(key) {
                continue;
            }
            if let Some(text) = entry.as_str() {
                next.insert(key.clone(), text.to_string());
            }
        }
    }
    next
}

/// 将 overrides map 序列化为 wire JSON。
fn state_to_json(overrides: &HashMap<String, String>) -> Value {
    let mut map = serde_json::Map::new();
    for (key, text) in overrides {
        map.insert(key.clone(), Value::String(text.clone()));
    }
    json!({
        "version": FILE_VERSION,
        "overrides": Value::Object(map),
    })
}

/// 从文件读取 overrides。ENOENT / 解析失败 → 空 map。
fn read_overrides_from_file(path: &std::path::Path) -> HashMap<String, String> {
    match std::fs::read_to_string(path) {
        Ok(raw) => {
            let parsed: Value = serde_json::from_str(&raw).unwrap_or_default();
            let stored = parsed.get("overrides").cloned().unwrap_or(Value::Null);
            sanitize_overrides(&stored)
        }
        Err(_) => HashMap::new(),
    }
}

/// Magic prompts 运行时 — write_lock 串行化 read-modify-write。
pub struct MagicPromptRuntime {
    file_path: std::path::PathBuf,
    write_lock: Mutex<()>,
}

impl MagicPromptRuntime {
    pub fn new(file_path: std::path::PathBuf) -> Self {
        Self {
            file_path,
            write_lock: Mutex::new(()),
        }
    }

    /// 读取当前 prompt state。
    pub fn read_state(&self) -> Value {
        let overrides = read_overrides_from_file(&self.file_path);
        state_to_json(&overrides)
    }

    /// 设置单个 override。
    ///
    /// 返回更新后的 state。失败返回 error message。
    pub async fn set_override(
        &self,
        id: &str,
        text: &str,
    ) -> Result<Value, String> {
        let normalized_id = id.trim();
        if !is_valid_prompt_id(normalized_id) {
            return Err("Invalid prompt id".to_string());
        }
        if is_visible_prompt_id(normalized_id) && text.trim().is_empty() {
            return Err("Visible prompt text cannot be empty".to_string());
        }
        if text.len() > MAX_PROMPT_TEXT_LENGTH {
            return Err("Prompt text is too long".to_string());
        }

        let _lock = self.write_lock.lock().await;
        let mut overrides = read_overrides_from_file(&self.file_path);
        overrides.insert(normalized_id.to_string(), text.to_string());
        let state = state_to_json(&overrides);
        self.write_state(&state).map_err(|e| e.to_string())?;
        Ok(state)
    }

    /// 重置单个 override。
    pub async fn reset_override(&self, id: &str) -> Result<Value, String> {
        let normalized_id = id.trim();
        if !is_valid_prompt_id(normalized_id) {
            return Err("Invalid prompt id".to_string());
        }

        let _lock = self.write_lock.lock().await;
        let mut overrides = read_overrides_from_file(&self.file_path);
        if overrides.remove(normalized_id).is_none() {
            // ID 不在 overrides 中 — 返回当前 state (no-op)
            let state = state_to_json(&overrides);
            return Ok(state);
        }
        let state = state_to_json(&overrides);
        self.write_state(&state).map_err(|e| e.to_string())?;
        Ok(state)
    }

    /// 重置全部 overrides。
    pub async fn reset_all_overrides(&self) -> Result<Value, String> {
        let _lock = self.write_lock.lock().await;
        let state = json!({ "version": FILE_VERSION, "overrides": {} });
        self.write_state(&state).map_err(|e| e.to_string())?;
        Ok(state)
    }

    /// 原子写入 state JSON。
    fn write_state(&self, state: &Value) -> std::io::Result<()> {
        let path = &self.file_path;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = serde_json::to_string_pretty(state)?;
        let tmp_path = format!(
            "{}.tmp-{}-{}-{}",
            path.to_string_lossy(),
            std::process::id(),
            chrono::Utc::now().timestamp_millis(),
            rand::random::<u32>(),
        );
        let tmp = std::path::PathBuf::from(&tmp_path);
        if let Err(e) = std::fs::write(&tmp, &content) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        if let Err(e) = std::fs::rename(&tmp, path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        Ok(())
    }
}

// ============================================================
// axum handlers
// ============================================================

/// GET /api/magic-prompts
pub async fn get_magic_prompts(State(_state): State<Arc<AppState>>) -> Response {
    let runtime = MagicPromptRuntime::new(file_path());
    Json(runtime.read_state()).into_response()
}

/// PUT /api/magic-prompts/{id}
pub async fn put_magic_prompt(
    State(_state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let text = match body.get("text").and_then(|v| v.as_str()) {
        Some(t) => t,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "text is required" })),
            )
                .into_response();
        }
    };

    let runtime = MagicPromptRuntime::new(file_path());
    match runtime.set_override(&id, text).await {
        Ok(state) => Json(state).into_response(),
        Err(message) => {
            let status = if message.contains("Invalid prompt id")
                || message.contains("too long")
                || message.contains("cannot be empty")
            {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            (status, Json(json!({ "error": message }))).into_response()
        }
    }
}

/// DELETE /api/magic-prompts/{id}
pub async fn delete_magic_prompt(
    State(_state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    let runtime = MagicPromptRuntime::new(file_path());
    match runtime.reset_override(&id).await {
        Ok(state) => Json(state).into_response(),
        Err(message) => {
            let status = if message.contains("Invalid prompt id") {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            (status, Json(json!({ "error": message }))).into_response()
        }
    }
}

/// DELETE /api/magic-prompts
pub async fn delete_all_magic_prompts(State(_state): State<Arc<AppState>>) -> Response {
    let runtime = MagicPromptRuntime::new(file_path());
    match runtime.reset_all_overrides().await {
        Ok(state) => Json(state).into_response(),
        Err(message) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": message })),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static TEST_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn temp_path() -> std::path::PathBuf {
        let dir = std::env::temp_dir();
        let seq = TEST_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        dir.join(format!(
            "oc-test-magic-prompts-{}-{}-{}.json",
            std::process::id(),
            chrono::Utc::now().timestamp_millis(),
            seq,
        ))
    }

    fn temp_runtime() -> MagicPromptRuntime {
        let path = temp_path();
        let _ = std::fs::remove_file(&path);
        MagicPromptRuntime::new(path)
    }

    #[tokio::test]
    async fn set_and_read_override() {
        let rt = temp_runtime();
        let state = rt.set_override("my.prompt", "hello world").await.unwrap();
        assert_eq!(state["version"], FILE_VERSION);
        assert_eq!(state["overrides"]["my.prompt"], "hello world");

        let read = rt.read_state();
        assert_eq!(read["overrides"]["my.prompt"], "hello world");
    }

    #[tokio::test]
    async fn reset_override() {
        let rt = temp_runtime();
        rt.set_override("test.id", "text").await.unwrap();
        let state = rt.reset_override("test.id").await.unwrap();
        assert!(state["overrides"].as_object().unwrap().is_empty());
    }

    #[tokio::test]
    async fn reset_all_overrides() {
        let rt = temp_runtime();
        rt.set_override("a", "1").await.unwrap();
        rt.set_override("b", "2").await.unwrap();
        let state = rt.reset_all_overrides().await.unwrap();
        assert!(state["overrides"].as_object().unwrap().is_empty());
    }

    #[tokio::test]
    async fn invalid_prompt_id_rejected() {
        let rt = temp_runtime();
        let err = rt.set_override("INVALID ID!", "text").await;
        assert!(err.is_err());
        assert_eq!(err.unwrap_err(), "Invalid prompt id");
    }

    #[tokio::test]
    async fn visible_prompt_empty_rejected() {
        let rt = temp_runtime();
        let err = rt.set_override("my.visible", "  ").await;
        assert!(err.is_err());
        assert_eq!(err.unwrap_err(), "Visible prompt text cannot be empty");
    }

    #[tokio::test]
    async fn too_long_text_rejected() {
        let rt = temp_runtime();
        let long_text = "x".repeat(MAX_PROMPT_TEXT_LENGTH + 1);
        let err = rt.set_override("ok.id", &long_text).await;
        assert!(err.is_err());
        assert_eq!(err.unwrap_err(), "Prompt text is too long");
    }

    #[tokio::test]
    async fn write_lock_serializes() {
        // 并发 set_override 不会丢失 (write_lock 串行化)
        let rt = Arc::new(temp_runtime());
        let mut handles = Vec::new();
        for i in 0..5 {
            let rt_clone = rt.clone();
            handles.push(tokio::spawn(async move {
                rt_clone
                    .set_override(&format!("prompt.{i}"), &format!("text-{i}"))
                    .await
                    .unwrap();
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        let state = rt.read_state();
        for i in 0..5 {
            assert_eq!(
                state["overrides"][format!("prompt.{i}")],
                format!("text-{i}")
            );
        }
    }

    #[test]
    fn sanitize_filters_invalid_entries() {
        let raw = json!({
            "valid.id": "text1",
            "INVALID": "text2",
            "another-valid": "text3",
            "empty": "",
        });
        let result = sanitize_overrides(&raw);
        assert_eq!(result.len(), 3);
        assert!(result.contains_key("valid.id"));
        assert!(result.contains_key("another-valid"));
        assert!(result.contains_key("empty"));
        assert!(!result.contains_key("INVALID"));
    }
}
