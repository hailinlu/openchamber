//! 持久化 + 序列化/反序列化 scheduled-tasks 配置 — 移植自 Node
//! `projects/project-config.js`。
//!
//! 持久化格式: `$OPENCHAMBER_DATA_DIR/projects/{project_id}/scheduled-tasks.json`,
//! atomic write (`.tmp → rename`, 0o600)。
//!
//! 上层 scheduled-tasks runtime 不直接读 `tasksByProject` 而是通过此 runtime
//! 完成 list/upsert/delete/update_state。

#![allow(dead_code)] // 部分 helper 由 runtime 调用, 当前路由只暴露 list/upsert/delete/update_state

use std::path::PathBuf;
use std::sync::Mutex;

use oc_core::Error;
use serde_json::{json, Value};

use crate::github::settings::data_dir;

/// ScheduledTask 类型别名 (即 `Value`, 整层 JSON)。
pub type ScheduledTask = Value;

#[derive(Debug, Clone)]
pub struct UpsertResult {
    pub task: ScheduledTask,
    pub created: bool,
}

#[derive(Debug, Clone)]
pub struct UpdateResult {
    pub task: Option<ScheduledTask>,
}

#[derive(Debug, Clone)]
pub struct DeleteResult {
    pub deleted: bool,
}

/// project scheduled-tasks runtime — 单实例, hold per-project write lock to
/// serialize concurrent writes.
pub struct ProjectConfigRuntime {
    base_dir: PathBuf,
    write_locks: Mutex<std::collections::HashMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>>,
}

impl ProjectConfigRuntime {
    pub fn new(base_dir: PathBuf) -> Self {
        Self {
            base_dir,
            write_locks: Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// `$OPENCHAMBER_DATA_DIR/projects/{project_id}/scheduled-tasks.json`
    fn config_file_path(&self, project_id: &str) -> PathBuf {
        self.base_dir
            .join(sanitize_id(project_id))
            .join("scheduled-tasks.json")
    }

    fn lock_for(&self, project_id: &str) -> std::sync::Arc<tokio::sync::Mutex<()>> {
        let mut map = self.write_locks.lock().unwrap();
        map.entry(project_id.to_string())
            .or_insert_with(|| std::sync::Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    async fn read_raw(&self, project_id: &str) -> Value {
        let path = self.config_file_path(project_id);
        match tokio::fs::read_to_string(&path).await {
            Ok(s) => serde_json::from_str(&s).unwrap_or_else(|_| json!({})),
            Err(_) => json!({}),
        }
    }

    async fn write_raw(&self, project_id: &str, config: &Value) -> Result<(), Error> {
        let path = self.config_file_path(project_id);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| {
                Error::Internal(format!(
                    "create_dir_all({}): {}",
                    parent.display(),
                    e
                ))
            })?;
        }
        let content = serde_json::to_string_pretty(config)
            .map_err(|e| Error::Internal(format!("serialize: {}", e)))?;

        // atomic write: .tmp → rename
        let tmp = path.with_extension(format!(
            "json.{}.{}.tmp",
            std::process::id(),
            chrono::Utc::now().timestamp_millis()
        ));
        tokio::fs::write(&tmp, &content).await.map_err(|e| {
            Error::Internal(format!("write tmp {}: {}", tmp.display(), e))
        })?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = tokio::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).await;
        }

        tokio::fs::rename(&tmp, &path).await.map_err(|e| {
            Error::Internal(format!(
                "rename {} → {}: {}",
                tmp.display(),
                path.display(),
                e
            ))
        })?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).await;
        }

        Ok(())
    }

    /// 列出 project 的所有 scheduled tasks。
    pub async fn list_scheduled_tasks(&self, project_id: &str) -> Result<Vec<ScheduledTask>, Error> {
        if !is_valid_project_id(project_id) {
            return Err(Error::BadRequest("projectId contains unsupported characters".into()));
        }
        let cfg = self.read_raw(project_id).await;
        let tasks = cfg
            .get("scheduledTasks")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(tasks)
    }

    /// Upsert (create or replace by id).
    pub async fn upsert_scheduled_task(
        &self,
        project_id: &str,
        task_input: Value,
    ) -> Result<UpsertResult, Error> {
        if !is_valid_project_id(project_id) {
            return Err(Error::BadRequest("projectId contains unsupported characters".into()));
        }
        if !task_input.is_object() {
            return Err(Error::BadRequest("task is required".into()));
        }

        let lock = self.lock_for(project_id);
        let _guard = lock.lock().await;

        let cfg = self.read_raw(project_id).await;
        let mut tasks: Vec<Value> = cfg
            .get("scheduledTasks")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let input_id = task_input.get("id").and_then(Value::as_str).map(str::to_string);
        let existing_idx = input_id
            .as_ref()
            .and_then(|id| tasks.iter().position(|t| t.get("id").and_then(Value::as_str) == Some(id.as_str())))
            .unwrap_or(usize::MAX);
        let created = existing_idx == usize::MAX;

        if !created {
            tasks[existing_idx] = task_input.clone();
        } else {
            tasks.push(task_input.clone());
        }

        let next = json!({ "version": 1, "scheduledTasks": tasks });
        self.write_raw(project_id, &next).await?;

        Ok(UpsertResult {
            task: task_input,
            created,
        })
    }

    /// Patch task state (merges top-level state object).
    pub async fn update_scheduled_task_state(
        &self,
        project_id: &str,
        task_id: &str,
        patch: Value,
    ) -> Result<UpdateResult, Error> {
        if !is_valid_project_id(project_id) {
            return Err(Error::BadRequest("projectId contains unsupported characters".into()));
        }
        if task_id.is_empty() {
            return Err(Error::BadRequest("taskId is required".into()));
        }

        let lock = self.lock_for(project_id);
        let _guard = lock.lock().await;

        let cfg = self.read_raw(project_id).await;
        let mut tasks: Vec<Value> = cfg
            .get("scheduledTasks")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let pos = match tasks
            .iter()
            .position(|t| t.get("id").and_then(Value::as_str) == Some(task_id))
        {
            Some(p) => p,
            None => {
                return Ok(UpdateResult { task: None });
            }
        };

        // merge state object
        let current = tasks[pos].clone();
        let merged_state = match (current.get("state"), patch.get("state")) {
            (Some(existing), Some(state_patch)) => {
                let mut map = existing.as_object().cloned().unwrap_or_default();
                if let Some(patch_obj) = state_patch.as_object() {
                    for (k, v) in patch_obj {
                        map.insert(k.clone(), v.clone());
                    }
                }
                json!(map)
            }
            _ => patch.get("state").cloned().unwrap_or(json!({})),
        };

        let mut updated = current.clone();
        if let Some(obj) = updated.as_object_mut() {
            // 顶层字段覆盖 (nextRunAt / lastStatus 等)
            if let Some(patch_obj) = patch.as_object() {
                for (k, v) in patch_obj {
                    if k != "state" {
                        obj.insert(k.clone(), v.clone());
                    }
                }
            }
            obj.insert("state".to_string(), merged_state.clone());
            obj.insert("updatedAt".to_string(), json!(chrono::Utc::now().timestamp_millis()));
        }

        tasks[pos] = updated.clone();
        let next = json!({ "version": 1, "scheduledTasks": tasks });
        self.write_raw(project_id, &next).await?;

        Ok(UpdateResult {
            task: Some(updated),
        })
    }

    /// 删除 task。返回 deleted 是否真删除了东西。
    pub async fn delete_scheduled_task(
        &self,
        project_id: &str,
        task_id: &str,
    ) -> Result<DeleteResult, Error> {
        if !is_valid_project_id(project_id) {
            return Err(Error::BadRequest("projectId contains unsupported characters".into()));
        }
        if task_id.is_empty() {
            return Err(Error::BadRequest("taskId is required".into()));
        }

        let lock = self.lock_for(project_id);
        let _guard = lock.lock().await;

        let cfg = self.read_raw(project_id).await;
        let tasks: Vec<Value> = cfg
            .get("scheduledTasks")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let next_tasks: Vec<Value> = tasks
            .into_iter()
            .filter(|t| t.get("id").and_then(Value::as_str) != Some(task_id))
            .collect();

        let deleted = next_tasks.len()
            != cfg
                .get("scheduledTasks")
                .and_then(Value::as_array)
                .map(|a| a.len())
                .unwrap_or(0);

        if deleted {
            let next = json!({ "version": 1, "scheduledTasks": next_tasks });
            self.write_raw(project_id, &next).await?;
        }

        Ok(DeleteResult { deleted })
    }
}

/// 默认 base dir: `$OPENCHAMBER_DATA_DIR/projects` 或 `~/.config/openchamber/projects`。
pub fn default_projects_dir() -> PathBuf {
    data_dir().join("projects")
}

/// project_id sanitization — 限制为 URL-safe 字符。
fn is_valid_project_id(project_id: &str) -> bool {
    if project_id.is_empty() {
        return false;
    }
    project_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-'))
}

fn sanitize_id(project_id: &str) -> String {
    project_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-') { c } else { '_' })
        .collect()
}

// =========================================================================
// 测试
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir() -> PathBuf {
        std::env::temp_dir().join(format!(
            "oc-scheduled-config-test-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ))
    }

    #[tokio::test]
    async fn upsert_list_update_delete_roundtrip() {
        let dir = test_dir();
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let rt = ProjectConfigRuntime::new(dir.clone());

        // 1. 初始 list 为空
        let list = rt.list_scheduled_tasks("proj-1").await.unwrap();
        assert!(list.is_empty());

        // 2. upsert (create)
        let task = json!({
            "id": "task-1",
            "name": "demo",
            "enabled": true,
            "schedule": { "kind": "daily", "times": ["09:00"] },
            "execution": { "providerID": "openai", "modelID": "gpt-4o", "prompt": "hello" },
            "state": {}
        });
        let res = rt.upsert_scheduled_task("proj-1", task.clone()).await.unwrap();
        assert!(res.created);
        assert_eq!(res.task, task);

        let list = rt.list_scheduled_tasks("proj-1").await.unwrap();
        assert_eq!(list.len(), 1);

        // 3. update state
        let patch = json!({
            "nextRunAt": 12345,
            "lastStatus": "success",
            "state": { "lastDurationMs": 1000 }
        });
        let updated = rt
            .update_scheduled_task_state("proj-1", "task-1", patch)
            .await
            .unwrap()
            .task
            .expect("expected task to exist");
        assert_eq!(updated["state"]["lastStatus"], "success");
        assert_eq!(updated["state"]["nextRunAt"], 12345);

        // 4. update non-existent → task: None
        let missing = rt
            .update_scheduled_task_state("proj-1", "no-such", json!({}))
            .await
            .unwrap();
        assert!(missing.task.is_none());

        // 5. delete
        let del = rt.delete_scheduled_task("proj-1", "task-1").await.unwrap();
        assert!(del.deleted);
        let list = rt.list_scheduled_tasks("proj-1").await.unwrap();
        assert!(list.is_empty());

        // delete again → not deleted
        let del2 = rt.delete_scheduled_task("proj-1", "task-1").await.unwrap();
        assert!(!del2.deleted);

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn enoent_returns_empty_list() {
        let dir = test_dir();
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let rt = ProjectConfigRuntime::new(dir.clone());
        // 不存在的 project
        let list = rt.list_scheduled_tasks("no-such").await.unwrap();
        assert!(list.is_empty());
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn invalid_project_id_rejected() {
        let dir = test_dir();
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let rt = ProjectConfigRuntime::new(dir.clone());
        let bad = rt.list_scheduled_tasks("../etc/passwd").await;
        assert!(matches!(bad, Err(Error::BadRequest(_))));
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn upsert_replace_existing() {
        let dir = test_dir();
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let rt = ProjectConfigRuntime::new(dir.clone());

        let v1 = json!({
            "id": "task-1",
            "name": "v1",
            "enabled": true,
            "schedule": { "kind": "daily", "times": ["09:00"] },
            "execution": { "providerID": "openai", "modelID": "gpt-4o", "prompt": "x" },
            "state": {}
        });
        let _ = rt.upsert_scheduled_task("proj", v1.clone()).await.unwrap();

        let v2 = json!({
            "id": "task-1",
            "name": "v2",
            "enabled": false,
            "schedule": { "kind": "daily", "times": ["10:00"] },
            "execution": { "providerID": "openai", "modelID": "gpt-4o", "prompt": "y" },
            "state": {}
        });
        let res = rt.upsert_scheduled_task("proj", v2.clone()).await.unwrap();
        assert!(!res.created); // 已存在, 不是 create

        let list = rt.list_scheduled_tasks("proj").await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["name"], "v2");
        assert_eq!(list[0]["enabled"], false);
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn write_invalidates_id_rejects() {
        let dir = test_dir();
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let rt = ProjectConfigRuntime::new(dir.clone());
        let res = rt.delete_scheduled_task("..", "task").await;
        assert!(matches!(res, Err(Error::BadRequest(_))));
        let res = rt.delete_scheduled_task("proj", "").await;
        assert!(matches!(res, Err(Error::BadRequest(_))));
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
