//! 持久化 + 序列化/反序列化 scheduled-tasks 配置 — 移植自 Node
//! `projects/project-config.js`。
//!
//! 持久化格式: `$OPENCHAMBER_USER_CONFIG_ROOT/projects/{project_id}.json`
//! (flat 文件, 与 Node 布局完全一致), atomic write (`.tmp → rename`, 0o600)。
//!
//! 写入时 merge 现有 sibling keys (projectNotes/projectTodos 等),
//! 不会 clobber 非 scheduledTasks 字段。
//!
//! 所有 task 在 upsert 前经过 `normalize::normalize_task_for_storage` 验证:
//! name/prompt/cron 长度 clamp, providerID/modelID 必填, schedule 归一化等。

#![allow(dead_code)] // 部分 helper 由 runtime 调用, 当前路由只暴露 list/upsert/delete/update_state

use std::path::PathBuf;
use std::sync::Mutex;

use oc_core::Error;
use serde_json::{json, Value};

use crate::github::settings::user_config_root;
use crate::scheduled_tasks::normalize::{
    self, normalize_task_for_read, normalize_task_for_storage, NormalizeOptions,
    PROJECT_CONFIG_VERSION,
};

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

    /// `$BASE_DIR/{project_id}.json` (flat, 与 Node 一致)。
    fn config_file_path(&self, project_id: &str) -> PathBuf {
        self.base_dir.join(format!("{}.json", sanitize_id(project_id)))
    }

    fn lock_for(&self, project_id: &str) -> std::sync::Arc<tokio::sync::Mutex<()>> {
        let mut map = self.write_locks.lock().unwrap();
        map.entry(project_id.to_string())
            .or_insert_with(|| std::sync::Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// 读 raw config (包含所有 sibling keys), 不存在返回 `{}`。
    async fn read_raw(&self, project_id: &str) -> Value {
        let path = self.config_file_path(project_id);
        match tokio::fs::read_to_string(&path).await {
            Ok(s) => serde_json::from_str(&s).unwrap_or_else(|_| json!({})),
            Err(_) => json!({}),
        }
    }

    /// 写 config — **merge** 模式: 保留现有 sibling keys, 仅覆盖 version + scheduledTasks。
    /// 与 Node `writeProjectConfigToDisk` 一致。
    async fn write_merged(&self, project_id: &str, tasks: &[Value]) -> Result<(), Error> {
        let path = self.config_file_path(project_id);

        // 读现有 raw config (含 sibling keys)
        let existing = self.read_raw(project_id).await;
        let mut merged = if existing.is_object() {
            existing
        } else {
            json!({})
        };
        if let Some(obj) = merged.as_object_mut() {
            obj.insert("version".into(), json!(PROJECT_CONFIG_VERSION));
            obj.insert("scheduledTasks".into(), json!(tasks));
        }

        let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
        tokio::fs::create_dir_all(parent).await.map_err(|e| {
            Error::Internal(format!("create_dir_all({}): {}", parent.display(), e))
        })?;

        let content = serde_json::to_string_pretty(&merged)
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

    /// 读 config 并对每个 task 做 normalize (refreshUpdatedAt=false)。
    /// normalize 失败的 task 被静默跳过 (与 Node `readProjectConfigFromDisk` 一致)。
    async fn read_normalized(&self, project_id: &str) -> Vec<Value> {
        let parsed = self.read_raw(project_id).await;
        let tasks_raw = parsed.get("scheduledTasks").and_then(Value::as_array);
        let now_ms = chrono::Utc::now().timestamp_millis();
        match tasks_raw {
            Some(arr) => arr
                .iter()
                .filter_map(|t| normalize_task_for_read(t, now_ms))
                .collect(),
            None => Vec::new(),
        }
    }

    /// 列出 project 的所有 scheduled tasks。
    pub async fn list_scheduled_tasks(&self, project_id: &str) -> Result<Vec<ScheduledTask>, Error> {
        if !is_valid_project_id(project_id) {
            return Err(Error::BadRequest("projectId contains unsupported characters".into()));
        }
        Ok(self.read_normalized(project_id).await)
    }

    /// Upsert (create or replace by id)。
    /// 经 `normalize_task_for_storage` 完整验证 + ID 生成。
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

        let tasks = self.read_normalized(project_id).await;
        let now_ms = chrono::Utc::now().timestamp_millis();

        let incoming_id = task_input.get("id").and_then(Value::as_str).map(str::to_string);
        let existing_idx = incoming_id
            .as_ref()
            .and_then(|id| tasks.iter().position(|t| t.get("id").and_then(Value::as_str) == Some(id.as_str())));
        let existing_task = existing_idx.map(|i| &tasks[i]);

        let created = existing_idx.is_none();

        let normalized_task = normalize_task_for_storage(
            &task_input,
            NormalizeOptions {
                now: now_ms,
                existing_task,
                allow_create: true,
                refresh_updated_at: true,
            },
        )
        .map_err(Error::BadRequest)?;

        let mut next_tasks = tasks.clone();
        if let Some(idx) = existing_idx {
            next_tasks[idx] = normalized_task.clone();
        } else {
            next_tasks.push(normalized_task.clone());
        }

        self.write_merged(project_id, &next_tasks).await?;

        Ok(UpsertResult {
            task: normalized_task,
            created,
        })
    }

    /// Patch task state — 合并 state object, `updatedAt` 写在 `task.state.updatedAt` 下。
    /// 经 `normalize_state` 归一化 (timestamp round, lastError clamp, lastStatus 归一化)。
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

        let mut tasks = self.read_normalized(project_id).await;

        let pos = match tasks
            .iter()
            .position(|t| t.get("id").and_then(Value::as_str) == Some(task_id))
        {
            Some(p) => p,
            None => return Ok(UpdateResult { task: None }),
        };

        let current = tasks[pos].clone();
        let current_state = current.get("state").cloned().unwrap_or(json!({}));
        let patch_obj = if patch.is_object() { &patch } else { &Value::Null };

        // merge state: { ...current.state, ...patch }
        let mut merged_input = current_state.clone();
        if let (Some(merged_map), Some(patch_map)) = (merged_input.as_object_mut(), patch_obj.as_object()) {
            for (k, v) in patch_map {
                if k != "state" {
                    merged_map.insert(k.clone(), v.clone());
                }
            }
        }
        // 如果 patch 本身有 state 子对象, 也 merge 进去
        if let Some(patch_state) = patch_obj.get("state").and_then(Value::as_object) {
            if let Some(merged_map) = merged_input.as_object_mut() {
                for (k, v) in patch_state {
                    merged_map.insert(k.clone(), v.clone());
                }
            }
        }
        // updatedAt 刷新为 now
        if let Some(map) = merged_input.as_object_mut() {
            map.insert("updatedAt".into(), json!(chrono::Utc::now().timestamp_millis()));
        }

        // normalize_state: timestamp round + lastError clamp + lastStatus 归一化
        let new_state = normalize::normalize_state(&merged_input, &current_state);

        let mut updated = current.clone();
        if let Some(obj) = updated.as_object_mut() {
            // 顶层字段覆盖 (nextRunAt 等 patch 中的非 state 字段)
            if let Some(patch_map) = patch_obj.as_object() {
                for (k, v) in patch_map {
                    if k != "state" {
                        obj.insert(k.clone(), v.clone());
                    }
                }
            }
            obj.insert("state".to_string(), new_state);
        }

        tasks[pos] = updated.clone();
        self.write_merged(project_id, &tasks).await?;

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

        let tasks = self.read_normalized(project_id).await;
        let before_len = tasks.len();
        let next_tasks: Vec<Value> = tasks
            .into_iter()
            .filter(|t| t.get("id").and_then(Value::as_str) != Some(task_id))
            .collect();

        let deleted = next_tasks.len() != before_len;

        if deleted {
            self.write_merged(project_id, &next_tasks).await?;
        }

        Ok(DeleteResult { deleted })
    }
}

/// 默认 base dir: `$OPENCHAMBER_USER_CONFIG_ROOT/projects` 或 `~/.config/openchamber/projects`。
/// 与 Node 一致, 用 user_config_root (不读 OPENCHAMBER_DATA_DIR)。
pub fn default_projects_dir() -> PathBuf {
    user_config_root().join("projects")
}

/// project_id sanitization — 限制为 URL-safe 字符。
/// 拒绝 `.` 和 `..` (路径遍历防护)。
fn is_valid_project_id(project_id: &str) -> bool {
    if project_id.is_empty() || project_id == "." || project_id == ".." {
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

        // 2. upsert (create) — 带完整必填字段
        let task = json!({
            "name": "demo",
            "enabled": true,
            "schedule": { "kind": "daily", "times": ["09:00"], "timezone": "UTC" },
            "execution": { "providerID": "openai", "modelID": "gpt-4o", "prompt": "hello" },
        });
        let res = rt.upsert_scheduled_task("proj-1", task).await.unwrap();
        assert!(res.created);
        // normalize 后 task 应有 id (uuid 生成)
        assert!(res.task.get("id").is_some());
        assert_eq!(res.task["name"], "demo");

        let list = rt.list_scheduled_tasks("proj-1").await.unwrap();
        assert_eq!(list.len(), 1);
        let task_id = list[0]["id"].as_str().unwrap().to_string();

        // 3. update state
        let patch = json!({
            "nextRunAt": 12345,
            "lastStatus": "success",
            "state": { "lastDurationMs": 1000 }
        });
        let updated = rt
            .update_scheduled_task_state("proj-1", &task_id, patch)
            .await
            .unwrap()
            .task
            .expect("expected task to exist");
        // updatedAt 应在 state 下 (不是顶层)
        assert!(updated["state"]["updatedAt"].is_i64());
        // 顶层不应有 updatedAt (Node 写在 state.updatedAt)
        assert!(updated.get("updatedAt").is_none() || updated["updatedAt"].is_null());
        assert_eq!(updated["state"]["lastStatus"], "success");
        assert_eq!(updated["state"]["nextRunAt"], 12345);
        assert_eq!(updated["state"]["lastDurationMs"], 1000);

        // 4. update non-existent → task: None
        let missing = rt
            .update_scheduled_task_state("proj-1", "no-such", json!({}))
            .await
            .unwrap();
        assert!(missing.task.is_none());

        // 5. delete
        let del = rt.delete_scheduled_task("proj-1", &task_id).await.unwrap();
        assert!(del.deleted);
        let list = rt.list_scheduled_tasks("proj-1").await.unwrap();
        assert!(list.is_empty());

        // delete again → not deleted
        let del2 = rt.delete_scheduled_task("proj-1", &task_id).await.unwrap();
        assert!(!del2.deleted);

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn enoent_returns_empty_list() {
        let dir = test_dir();
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let rt = ProjectConfigRuntime::new(dir.clone());
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
            "schedule": { "kind": "daily", "times": ["09:00"], "timezone": "UTC" },
            "execution": { "providerID": "openai", "modelID": "gpt-4o", "prompt": "x" },
        });
        let _ = rt.upsert_scheduled_task("proj", v1).await.unwrap();

        let v2 = json!({
            "id": "task-1",
            "name": "v2",
            "enabled": false,
            "schedule": { "kind": "daily", "times": ["10:00"], "timezone": "UTC" },
            "execution": { "providerID": "openai", "modelID": "gpt-4o", "prompt": "y" },
        });
        let res = rt.upsert_scheduled_task("proj", v2).await.unwrap();
        assert!(!res.created); // 已存在, 不是 create

        let list = rt.list_scheduled_tasks("proj").await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["name"], "v2");
        assert_eq!(list[0]["enabled"], false);
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn upsert_generates_id_when_missing() {
        let dir = test_dir();
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let rt = ProjectConfigRuntime::new(dir.clone());

        let task = json!({
            "name": "no-id-task",
            "schedule": { "kind": "daily", "times": ["09:00"], "timezone": "UTC" },
            "execution": { "providerID": "openai", "modelID": "gpt-4o", "prompt": "hi" },
        });
        let res = rt.upsert_scheduled_task("proj", task).await.unwrap();
        assert!(res.created);
        // id 应自动生成
        let id = res.task.get("id").and_then(Value::as_str).expect("id should be generated");
        assert!(!id.is_empty());

        let list = rt.list_scheduled_tasks("proj").await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["id"], id);
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn upsert_rejects_missing_name() {
        let dir = test_dir();
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let rt = ProjectConfigRuntime::new(dir.clone());

        let task = json!({
            "schedule": { "kind": "daily", "times": ["09:00"], "timezone": "UTC" },
            "execution": { "providerID": "openai", "modelID": "gpt-4o", "prompt": "hi" },
        });
        let res = rt.upsert_scheduled_task("proj", task).await;
        assert!(res.is_err());
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn upsert_rejects_missing_provider() {
        let dir = test_dir();
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let rt = ProjectConfigRuntime::new(dir.clone());

        let task = json!({
            "name": "task",
            "schedule": { "kind": "daily", "times": ["09:00"], "timezone": "UTC" },
            "execution": { "prompt": "hi" }, // missing providerID + modelID
        });
        let res = rt.upsert_scheduled_task("proj", task).await;
        assert!(res.is_err());
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn write_preserves_sibling_keys() {
        let dir = test_dir();
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let rt = ProjectConfigRuntime::new(dir.clone());

        // 先写一个带 sibling key 的 config
        let path = rt.config_file_path("proj");
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.unwrap();
        }
        let initial = json!({
            "version": 1,
            "projectNotes": "important notes",
            "projectTodos": ["todo1", "todo2"],
            "scheduledTasks": []
        });
        tokio::fs::write(&path, serde_json::to_string_pretty(&initial).unwrap())
            .await
            .unwrap();

        // upsert 一个 task — 不应 clobber sibling keys
        let task = json!({
            "id": "task-1",
            "name": "demo",
            "schedule": { "kind": "daily", "times": ["09:00"], "timezone": "UTC" },
            "execution": { "providerID": "openai", "modelID": "gpt-4o", "prompt": "x" },
        });
        let _ = rt.upsert_scheduled_task("proj", task).await.unwrap();

        // 读 raw 文件验证 sibling keys 保留
        let raw: Value = serde_json::from_str(
            &tokio::fs::read_to_string(&path).await.unwrap()
        ).unwrap();
        assert_eq!(raw["projectNotes"], "important notes");
        assert_eq!(raw["projectTodos"], json!(["todo1", "todo2"]));
        assert!(raw["scheduledTasks"].is_array());

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn flat_file_layout_matches_node() {
        let dir = test_dir();
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let rt = ProjectConfigRuntime::new(dir.clone());

        let task = json!({
            "name": "demo",
            "schedule": { "kind": "daily", "times": ["09:00"], "timezone": "UTC" },
            "execution": { "providerID": "openai", "modelID": "gpt-4o", "prompt": "x" },
        });
        let _ = rt.upsert_scheduled_task("myproj", task).await.unwrap();

        // 文件应是 {dir}/myproj.json (flat, 不是 {dir}/myproj/scheduled-tasks.json)
        let flat_path = dir.join("myproj.json");
        assert!(flat_path.exists(), "flat file should exist at {}", flat_path.display());

        // 不应有子目录
        let nested_path = dir.join("myproj").join("scheduled-tasks.json");
        assert!(!nested_path.exists(), "nested path should NOT exist");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn delete_invalid_id_rejects() {
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
