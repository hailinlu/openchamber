//! File-backed goal objectives — 移植 Node `session-goal/objectives.js`。
//!
//! Session metadata 必须保持轻量 (它跟随每个 session.updated 事件广播),
//! 所以 objective TEXT 落在 OpenChamber data dir 下的文件, 用 session id 做 key:
//! session 是全局唯一的, 一个 session 最多同时持有一个 goal, 映射完全确定。
//! metadata 只携带 `objectiveFile: true` 标记, 从不携带路径 — 用户可写的
//! metadata 不可能成为文件读向量。
//!
//! 文件路径: `$GRIDFORGE_DATA_DIR/goals/<session_id>.md` 或
//! `~/.config/gridforge/goals/<session_id>.md`。
//!
//! 对应 Node `session-goal/objectives.js` (61 行)。

#![allow(dead_code)] // 部分 helper 由 session-goal runtime 调用, 当前路由只暴露 read/write/delete

use std::path::PathBuf;

use oc_core::Error;
use serde_json::Value;

/// Objective 字符上限 (5000)。
pub const GOAL_OBJECTIVE_CHAR_LIMIT: usize = 5_000;

/// OpenCode session id 校验正则 (URL-safe token, 4-128 chars)。
///
/// 对应 Node `SESSION_ID_PATTERN = /^[A-Za-z0-9_-]{4,128}$/`。
fn is_valid_objective_key(session_id: &str) -> bool {
    if session_id.len() < 4 || session_id.len() > 128 {
        return false;
    }
    session_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// `GRIDFORGE_DATA_DIR` 或 `~/.config/gridforge` 下的 `goals` 目录。
pub fn goals_dir() -> PathBuf {
    crate::github::settings::data_dir().join("goals")
}

/// 单个 objective 文件路径。
fn objective_file_path(session_id: &str) -> PathBuf {
    goals_dir().join(format!("{session_id}.md"))
}

/// Trim + 字符 clamp。
fn clamp_content(content: &str) -> String {
    content.trim().chars().take(GOAL_OBJECTIVE_CHAR_LIMIT).collect()
}

/// 写入 (或覆盖 — 新 goal 替换旧文件) session 的 objective。
///
/// 对应 Node `writeObjective(sessionId, content)`。
/// 返回写入后的 trimmed 文本。
pub async fn write_objective(session_id: &str, content: &str) -> Result<String, Error> {
    if !is_valid_objective_key(session_id) {
        return Err(Error::BadRequest("invalid session id".into()));
    }
    let text = clamp_content(content);
    if text.is_empty() {
        return Err(Error::BadRequest("objective content is required".into()));
    }
    let dir = goals_dir();
    tokio::fs::create_dir_all(&dir).await?;
    tokio::fs::write(objective_file_path(session_id), &text).await?;
    Ok(text)
}

/// 读取 session 的 objective, 缺失/无效时返回 None。
///
/// 对应 Node `readObjective(sessionId)`。
pub async fn read_objective(session_id: &str) -> Option<String> {
    if !is_valid_objective_key(session_id) {
        return None;
    }
    let raw = tokio::fs::read_to_string(objective_file_path(session_id))
        .await
        .ok()?;
    Some(clamp_content(&raw))
}

/// Best-effort 删除 (缺失文件不报错)。
///
/// 对应 Node `deleteObjective(sessionId)`。
pub async fn delete_objective(session_id: &str) {
    if !is_valid_objective_key(session_id) {
        return;
    }
    let _ = tokio::fs::remove_file(objective_file_path(session_id)).await;
}

/// 从 settings.json 读 `sessionGoalEnabled` (默认 true)。
///
/// 对应 Node `isSessionGoalEnabled()`。
pub fn is_session_goal_enabled() -> bool {
    crate::github::settings::read_settings()
        .get("sessionGoalEnabled")
        .and_then(Value::as_bool)
        .unwrap_or(true)
}

// =========================================================================
// 测试
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// 设置临时 GRIDFORGE_DATA_DIR 并运行 async 测试体。
    /// 注意: env 是进程级, 测试需串行化。
    /// 使用 `auth::TEST_LOCK` 与 `config::with_temp_home` 共享锁, 跨模块防止 HOME/
    /// GRIDFORGE_DATA_DIR 互相污染。
    #[allow(clippy::await_holding_lock)]
    async fn with_temp_data_dir<F: FnOnce(&PathBuf) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>>(f: F) {
        use crate::opencode::auth::tests as auth_tests;
        let _guard = auth_tests::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("GRIDFORGE_DATA_DIR").ok();
        let tmp = std::env::temp_dir().join(format!(
            "oc-server-objectives-test-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        std::env::set_var("GRIDFORGE_DATA_DIR", &tmp);
        f(&tmp).await;
        if let Some(p) = prev {
            std::env::set_var("GRIDFORGE_DATA_DIR", p);
        } else {
            std::env::remove_var("GRIDFORGE_DATA_DIR");
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn is_valid_objective_key_accepts_url_safe_token() {
        assert!(is_valid_objective_key("sess_abc123"));
        assert!(is_valid_objective_key("ABC-def-123"));
        assert!(is_valid_objective_key("12345678"));
    }

    #[test]
    fn is_valid_objective_key_rejects_invalid() {
        assert!(!is_valid_objective_key(""));
        assert!(!is_valid_objective_key("ab"));        // too short
        assert!(!is_valid_objective_key("a".repeat(129).as_str())); // too long
        assert!(!is_valid_objective_key("../etc/passwd"));
        assert!(!is_valid_objective_key("with/slash"));
        assert!(!is_valid_objective_key("with.dot"));
        assert!(!is_valid_objective_key("with space"));
    }

    #[test]
    fn clamp_content_trims_and_limits() {
        assert_eq!(clamp_content("  hello  "), "hello");
        let long = "x".repeat(GOAL_OBJECTIVE_CHAR_LIMIT + 100);
        let clamped = clamp_content(&long);
        assert_eq!(clamped.len(), GOAL_OBJECTIVE_CHAR_LIMIT);
    }

#[tokio::test]
    async fn write_then_read_roundtrip() {
        with_temp_data_dir(|_| Box::pin(async {
            let written = write_objective("sess_abc1", "  build the widget  ").await.unwrap();
            assert_eq!(written, "build the widget");
            let read = read_objective("sess_abc1").await;
            assert_eq!(read, Some("build the widget".to_string()));
        })).await;
    }

    #[tokio::test]
    async fn write_empty_content_returns_bad_request() {
        with_temp_data_dir(|_| Box::pin(async {
            let result = write_objective("sess_abc1", "   ").await;
            assert!(matches!(result, Err(Error::BadRequest(_))));
        })).await;
    }

    #[tokio::test]
    async fn write_rejects_invalid_session_id() {
        with_temp_data_dir(|_| Box::pin(async {
            let result = write_objective("../bad", "content").await;
            assert!(matches!(result, Err(Error::BadRequest(_))));
        })).await;
    }

    #[tokio::test]
    async fn read_missing_returns_none() {
        with_temp_data_dir(|_| Box::pin(async {
            let result = read_objective("sess_does_not_exist_1234").await;
            assert!(result.is_none());
        })).await;
    }

    #[tokio::test]
    async fn read_invalid_session_id_returns_none() {
        let result = read_objective("").await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn delete_missing_is_noop() {
        with_temp_data_dir(|_| Box::pin(async {
            delete_objective("sess_nonexistent_12345").await; // should not panic
        })).await;
    }

    #[tokio::test]
    async fn write_then_delete_then_read_returns_none() {
        with_temp_data_dir(|_| Box::pin(async {
            write_objective("sess_xyz1", "objective content").await.unwrap();
            delete_objective("sess_xyz1").await;
            let read = read_objective("sess_xyz1").await;
            assert!(read.is_none());
        })).await;
    }
}