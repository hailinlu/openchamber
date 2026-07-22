//! `ScheduledTasksRuntime` — main state machine for scheduled tasks.
//!
//! 移植自 Node `scheduled-tasks/runtime.js` (878 行)。
//!
//! 职责:
//! - 持有 per-project 的 task map + 全局 queue
//! - 按 schedule 计算 next_run_at, arm single-shot `tokio::spawn` timer
//! - jitter (JITTER_MAX_MS=2000), bounded delay (MAX_TIMER_DELAY_MS=i32::MAX)
//! - pump_queue 调度: 全局并发 4, per-project 2
//! - 单飞: queuedTaskKeys + runningTaskKeys (HashSet)
//!
//! Public API:
//! - `start(self: Arc<Self>)` — 幂等启动, 触发 `sync_all_projects`
//! - `stop()` — 清 timers + queue
//! - `sync_all_projects()` — 全量重建 (从 settings.json 读 project 列表)
//! - `sync_project(project_id)` — 单项目同步 (路由 PUT/DELETE 后调用)
//! - `run_now(project_id, task_id)` — 手动触发, 返回 `RunNowResult`
//! - `get_status()` — 全局 {hasEnabled*, hasRunning*, *Count}

#![allow(dead_code)] // 部分 helper 由 runtime 内部使用, 不一定被路由直接调用

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;
use oc_core::Error;
use rand::Rng;
use serde_json::{json, Value};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

// Note: execution helpers are referenced inline (no wildcard import to keep
// the call sites explicit). They are private to the execution module and only
// used by `run_task_with_watchdog` which itself is in `execution.rs`.
use super::execution;
use super::project_config::{ProjectConfigRuntime, ScheduledTask};
use super::schedule::{compute_next_run_at, format_scheduled_session_title};
use crate::state::AppState;

// =========================================================================
// 常量 — 与 Node `scheduled-tasks/runtime.js` 严格对齐
// =========================================================================

pub const DEFAULT_MAX_GLOBAL_CONCURRENCY: usize = 4;
pub const DEFAULT_MAX_PROJECT_CONCURRENCY: usize = 2;
pub const DEFAULT_MAX_RUN_MS: u64 = 30 * 60 * 1000;
pub const JITTER_MAX_MS: u64 = 2_000;
/// bound for the timer delay on one arm (we re-arm in the next iteration if overflow).
pub const MAX_TIMER_DELAY_MS: u64 = 2_147_483_647;

/// manual run / scheduled run distinction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunReason {
    Manual,
    Scheduled,
}

impl RunReason {
    fn as_str(&self) -> &'static str {
        match self {
            RunReason::Manual => "manual",
            RunReason::Scheduled => "scheduled",
        }
    }
}

/// Item in the run queue.
#[derive(Debug, Clone)]
pub struct TaskQueueItem {
    pub project_id: String,
    pub task_id: String,
    pub reason: RunReason,
}

/// Result of `run_now`.
#[derive(Debug, Clone)]
pub struct RunNowResult {
    pub ok: bool,
    pub running: bool,
    pub queued: bool,
    pub skipped: bool,
    pub session_id: Option<String>,
    pub task: Option<ScheduledTask>,
    pub error: Option<String>,
}

/// Status snapshot — what the routes return via `GET /api/gridforge/scheduled-tasks/status`.
#[derive(Debug, Clone, Default)]
pub struct StatusSnapshot {
    pub has_enabled_scheduled_tasks: bool,
    pub has_running_scheduled_tasks: bool,
    pub enabled_scheduled_tasks_count: usize,
    pub running_scheduled_tasks_count: usize,
}

// =========================================================================
// ScheduledTasksRuntime
// =========================================================================

pub struct ScheduledTasksRuntime {
    // ---- shared configuration / dependencies ----
    project_config: Arc<ProjectConfigRuntime>,
    state: std::sync::RwLock<Option<Arc<AppState>>>,

    // ---- in-memory state ----
    tasks_by_project: Mutex<HashMap<String, HashMap<String, Value>>>,
    timers_by_task_key: Mutex<HashMap<String, JoinHandle<()>>>,
    queued_task_keys: Mutex<HashSet<String>>,
    running_task_keys: Mutex<HashSet<String>>,
    running_count_by_project: Mutex<HashMap<String, usize>>,
    running_global_count: Mutex<usize>,
    queue: Mutex<VecDeque<TaskQueueItem>>,
    project_path_by_id: Mutex<HashMap<String, String>>,

    // ---- synchronization barrier ----
    started: AtomicBool,
    /// Notified when a new item hits the queue; pump may be in a different task.
    queue_notify: Arc<Notify>,

    // ---- knobs ----
    max_global_concurrency: usize,
    max_project_concurrency: usize,
    max_run_duration_ms: u64,
}

// =========================================================================
// 工厂
// =========================================================================

/// Factory — 构造 `ScheduledTasksRuntime`, 关联 project-config runtime。
pub fn create_scheduled_tasks_runtime(
    project_config: Arc<ProjectConfigRuntime>,
    max_global_concurrency: Option<usize>,
    max_project_concurrency: Option<usize>,
    max_run_duration_ms: Option<u64>,
) -> Arc<ScheduledTasksRuntime> {
    Arc::new(ScheduledTasksRuntime {
        project_config,
        state: std::sync::RwLock::new(None),
        tasks_by_project: Mutex::new(HashMap::new()),
        timers_by_task_key: Mutex::new(HashMap::new()),
        queued_task_keys: Mutex::new(HashSet::new()),
        running_task_keys: Mutex::new(HashSet::new()),
        running_count_by_project: Mutex::new(HashMap::new()),
        running_global_count: Mutex::new(0),
        queue: Mutex::new(VecDeque::new()),
        project_path_by_id: Mutex::new(HashMap::new()),
        started: AtomicBool::new(false),
        queue_notify: Arc::new(Notify::new()),
        max_global_concurrency: max_global_concurrency.unwrap_or(DEFAULT_MAX_GLOBAL_CONCURRENCY),
        max_project_concurrency: max_project_concurrency.unwrap_or(DEFAULT_MAX_PROJECT_CONCURRENCY),
        max_run_duration_ms: max_run_duration_ms.unwrap_or(DEFAULT_MAX_RUN_MS),
    })
}

impl ScheduledTasksRuntime {
    /// 注入 AppState, 启动 runtime。
    ///
    /// 幂等 — 多次调用只启动一次。
    pub fn start(self: Arc<Self>, state: Arc<AppState>) {
        if self.started.swap(true, Ordering::SeqCst) {
            return;
        }
        *self.state.write().unwrap() = Some(state.clone());
        let rt = self.clone();
        tokio::spawn(async move {
            // ignore errors here — they cascade to logging
            if let Err(e) = rt.sync_all_projects().await {
                tracing::warn!("[ScheduledTasks] initial sync failed: {e}");
            }
        });
    }

    /// 停止 runtime, 清空 timers + queue。
    pub fn stop(&self) {
        if !self.started.swap(false, Ordering::SeqCst) {
            return;
        }
        let mut timers = self.timers_by_task_key.lock().unwrap();
        for (_, h) in timers.drain() {
            h.abort();
        }
        drop(timers);
        self.queued_task_keys.lock().unwrap().clear();
        self.queue.lock().unwrap().clear();
    }

    /// 检查 started — 路由可能要查此状态。
    pub fn is_started(&self) -> bool {
        self.started.load(Ordering::SeqCst)
    }

    /// 注入 AppState (测试 helper)。
    pub fn set_state_for_test(&self, state: Arc<AppState>) {
        *self.state.write().unwrap() = Some(state);
    }

    fn build_task_key(project_id: &str, task_id: &str) -> String {
        format!("{}:{}", project_id, task_id)
    }

    fn clear_timer_for_key(&self, key: &str) {
        let mut timers = self.timers_by_task_key.lock().unwrap();
        if let Some(h) = timers.remove(key) {
            h.abort();
        }
    }

    fn clear_project_timers(&self, project_id: &str) {
        let tasks = self
            .tasks_by_project
            .lock()
            .unwrap()
            .get(project_id)
            .cloned();
        let Some(tasks) = tasks else { return };
        for (task_id, _) in tasks.iter() {
            let key = Self::build_task_key(project_id, task_id);
            self.clear_timer_for_key(&key);
            self.queued_task_keys.lock().unwrap().remove(&key);
        }
    }

    fn set_project_tasks(&self, project_id: &str, tasks: &[Value]) {
        self.clear_project_timers(project_id);
        let mut map: HashMap<String, Value> = HashMap::new();
        for t in tasks {
            if let Some(id) = t.get("id").and_then(Value::as_str) {
                map.insert(id.to_string(), t.clone());
            }
        }
        self.tasks_by_project
            .lock()
            .unwrap()
            .insert(project_id.to_string(), map);
    }

    /// 计算全局 status。
    pub fn get_status(&self) -> StatusSnapshot {
        let mut enabled_count = 0usize;
        let tasks_map = self.tasks_by_project.lock().unwrap();
        for task_map in tasks_map.values() {
            for t in task_map.values() {
                if t.get("enabled").and_then(Value::as_bool).unwrap_or(false) {
                    enabled_count += 1;
                }
            }
        }
        drop(tasks_map);
        let running = self.running_task_keys.lock().unwrap().len();
        StatusSnapshot {
            has_enabled_scheduled_tasks: enabled_count > 0,
            has_running_scheduled_tasks: running > 0,
            enabled_scheduled_tasks_count: enabled_count,
            running_scheduled_tasks_count: running,
        }
    }

    // -----------------------------------------------------------------------
    // 同步路径
    // -----------------------------------------------------------------------

    /// 全量同步 — 从 settings.json 读所有 project, 然后逐个 sync_project。
    pub async fn sync_all_projects(self: &Arc<Self>) -> Result<(), Error> {
        let projects = list_projects_from_settings();
        let mut active_ids: HashSet<String> = HashSet::new();
        {
            let mut path_map = self.project_path_by_id.lock().unwrap();
            path_map.clear();
        }
        for p in &projects {
            if let (Some(id), Some(path)) = (
                p.get("id").and_then(Value::as_str),
                p.get("path").and_then(Value::as_str),
            ) {
                active_ids.insert(id.to_string());
                self.project_path_by_id
                    .lock()
                    .unwrap()
                    .insert(id.to_string(), path.to_string());
            }
        }

        // Remove known-but-inactive projects.
        let removed: Vec<String> = {
            let map = self.tasks_by_project.lock().unwrap();
            map.keys()
                .filter(|k| !active_ids.contains(*k))
                .cloned()
                .collect()
        };
        for k in removed {
            self.clear_project_timers(&k);
            self.tasks_by_project.lock().unwrap().remove(&k);
        }

        for project_id in &active_ids {
            if let Err(e) = self.sync_project(project_id).await {
                tracing::warn!(project_id = %project_id, "[ScheduledTasks] sync_project failed: {e}");
            }
        }
        Ok(())
    }

    /// 同步单个 project。
    pub async fn sync_project(self: &Arc<Self>, project_id: &str) -> Result<Vec<Value>, Error> {
        let tasks = self.project_config.list_scheduled_tasks(project_id).await?;
        self.set_project_tasks(project_id, &tasks);
        for t in &tasks {
            self.sync_task_schedule(project_id, t).await;
        }
        Ok(tasks)
    }

    /// 计算 task 的 next_run_at, 写入持久化, 并 arm timer。
    async fn sync_task_schedule(self: &Arc<Self>, project_id: &str, task: &Value) {
        let task_id = match task.get("id").and_then(Value::as_str) {
            Some(s) => s.to_string(),
            None => return,
        };
        let enabled = task.get("enabled").and_then(Value::as_bool).unwrap_or(false);
        if !enabled {
            return;
        }
        let now = Utc::now().timestamp_millis();
        let next_run_at = compute_next_run_at(task, now);
        let state_patch = json!({
            "nextRunAt": match next_run_at { Some(v) => json!(v), None => Value::Null },
            "updatedAt": now,
        });
        let updated = match self
            .project_config
            .update_scheduled_task_state(project_id, &task_id, state_patch)
            .await
        {
            Ok(r) => r.task,
            Err(_) => return,
        };
        let Some(updated) = updated else { return };

        {
            let mut map = self.tasks_by_project.lock().unwrap();
            if let Some(project_map) = map.get_mut(project_id) {
                project_map.insert(task_id.clone(), updated.clone());
            }
        }

        if updated.get("enabled").and_then(Value::as_bool).unwrap_or(false) {
            if let Some(nr) = updated
                .get("state")
                .and_then(|s| s.get("nextRunAt"))
                .and_then(Value::as_i64)
                .filter(|v| *v > 0)
            {
                self.schedule_task(project_id, &task_id, nr);
            }
        }
    }

    // -----------------------------------------------------------------------
    // Timer arm
    // -----------------------------------------------------------------------

    /// (重)arm 单次 timer 在 `next_run_at` 后 fire。
    ///
    /// 与 Node `scheduleTask` 行为一致:
    ///   - delay_base = max(0, next_run_at - now)
    ///   - jitter = 0..=JITTER_MAX_MS
    ///   - delay = delay_base + jitter
    ///   - 如果 delay > MAX_TIMER_DELAY_MS: spawn 一个 task 立即重新调度 (无 sleep)
    ///     否则 sleep 后 queue + pump
    fn schedule_task(self: &Arc<Self>, project_id: &str, task_id: &str, next_run_at: i64) {
        let key = Self::build_task_key(project_id, task_id);
        self.clear_timer_for_key(&key);

        if !self.started.load(Ordering::SeqCst) {
            return;
        }
        if next_run_at <= 0 {
            return;
        }

        let now = Utc::now().timestamp_millis();
        let delay_base = (next_run_at - now).max(0) as u64;
        let jitter: u64 = {
            let mut rng = rand::thread_rng();
            rng.gen_range(0..=JITTER_MAX_MS)
        };
        let delay = delay_base.saturating_add(jitter);
        let bounded_delay = delay.min(MAX_TIMER_DELAY_MS);

        let rt = self.clone();
        let project_id_owned = project_id.to_string();
        let task_id_owned = task_id.to_string();
        let key_owned = key.clone();

        let handle = tokio::spawn(async move {
            if delay > MAX_TIMER_DELAY_MS {
                // overflow — re-arm with original next_run_at (skip sleep)
                rt.schedule_task(&project_id_owned, &task_id_owned, next_run_at);
                return;
            }
            tokio::time::sleep(Duration::from_millis(bounded_delay)).await;
            // Remove from timers map.
            rt.timers_by_task_key.lock().unwrap().remove(&key_owned);

            if !rt.started.load(Ordering::SeqCst) {
                return;
            }
            // Sanity check: still enabled in memory + has path.
            let task_opt: Option<Value> = {
                let map = rt.tasks_by_project.lock().unwrap();
                map.get(&project_id_owned)
                    .and_then(|m: &std::collections::HashMap<String, Value>| {
                        m.get(&task_id_owned).cloned()
                    })
            };
            let Some(task) = task_opt else { return };
            if !task.get("enabled").and_then(Value::as_bool).unwrap_or(false) {
                return;
            }
            // enqueue + pump
            rt.enqueue_run(&project_id_owned, &task_id_owned, RunReason::Scheduled);
            rt.pump_queue();
        });

        self.timers_by_task_key
            .lock()
            .unwrap()
            .insert(key, handle);
    }

    // -----------------------------------------------------------------------
    // Queue + Concurrency
    // -----------------------------------------------------------------------

    fn enqueue_run(&self, project_id: &str, task_id: &str, reason: RunReason) {
        let key = Self::build_task_key(project_id, task_id);
        if self.queued_task_keys.lock().unwrap().contains(&key)
            || self.running_task_keys.lock().unwrap().contains(&key)
        {
            return;
        }
        self.queued_task_keys.lock().unwrap().insert(key);
        self.queue.lock().unwrap().push_back(TaskQueueItem {
            project_id: project_id.to_string(),
            task_id: task_id.to_string(),
            reason,
        });
        self.queue_notify.notify_one();
    }

    fn can_run_task(&self, project_id: &str) -> bool {
        let global_count = *self.running_global_count.lock().unwrap();
        if global_count >= self.max_global_concurrency {
            return false;
        }
        let project_map = self.running_count_by_project.lock().unwrap();
        let project_running = project_map.get(project_id).copied().unwrap_or(0);
        project_running < self.max_project_concurrency
    }

    /// 取下一个可运行项, 启动它。返回是否真的启动了。
    fn try_dispatch_one(self: &Arc<Self>) -> bool {
        let mut queue = self.queue.lock().unwrap();
        for i in 0..queue.len() {
            let item = queue[i].clone();
            if !self.can_run_task(&item.project_id) {
                continue;
            }
            queue.remove(i);
            drop(queue);
            let key = Self::build_task_key(&item.project_id, &item.task_id);
            self.queued_task_keys.lock().unwrap().remove(&key);
            self.spawn_run(item);
            return true;
        }
        false
    }

    /// Pump — drain queue respecting concurrency limits.
    pub fn pump_queue(self: &Arc<Self>) {
        if !self.started.load(Ordering::SeqCst) {
            return;
        }
        let mut safety = 0;
        while self.try_dispatch_one() {
            safety += 1;
            if safety > 1024 {
                break;
            }
        }
    }

    /// 在后台 spawn runTask。
    fn spawn_run(self: &Arc<Self>, item: TaskQueueItem) {
        let rt = self.clone();
        tokio::spawn(async move {
            rt.clone()
                .run_task(item.project_id, item.task_id, item.reason)
                .await;
            rt.pump_queue();
        });
    }

    // -----------------------------------------------------------------------
    // runNow
    // -----------------------------------------------------------------------

    /// 手动触发一个 task run, 跳过 queue (除非已经在跑/queued)。
    pub async fn run_now(self: &Arc<Self>, project_id: &str, task_id: &str) -> RunNowResult {
        let key = Self::build_task_key(project_id, task_id);
        if self.running_task_keys.lock().unwrap().contains(&key) {
            return RunNowResult {
                ok: false,
                running: true,
                queued: false,
                skipped: false,
                session_id: None,
                task: None,
                error: Some("task is already running".into()),
            };
        }
        if self.queued_task_keys.lock().unwrap().contains(&key) {
            return RunNowResult {
                ok: false,
                running: false,
                queued: true,
                skipped: false,
                session_id: None,
                task: None,
                error: Some("task is already queued".into()),
            };
        }
        self.clone().run_task(project_id.to_string(), task_id.to_string(), RunReason::Manual)
            .await
    }

    // -----------------------------------------------------------------------
    // runTask (core)
    // -----------------------------------------------------------------------

    async fn run_task(
        self: Arc<Self>,
        project_id: String,
        task_id: String,
        reason: RunReason,
    ) -> RunNowResult {
        let key = Self::build_task_key(&project_id, &task_id);

        // look up task in memory
        let task = {
            let map = self.tasks_by_project.lock().unwrap();
            map.get(&project_id)
                .and_then(|m| m.get(&task_id))
                .cloned()
        };
        let Some(task) = task else {
            return RunNowResult {
                ok: false,
                running: false,
                queued: false,
                skipped: true,
                session_id: None,
                task: None,
                error: Some("task not found".into()),
            };
        };
        if !task.get("enabled").and_then(Value::as_bool).unwrap_or(false) {
            return RunNowResult {
                ok: false,
                running: false,
                queued: false,
                skipped: true,
                session_id: None,
                task: None,
                error: Some("task not found or disabled".into()),
            };
        }

        // reserve running slot
        {
            let mut running = self.running_task_keys.lock().unwrap();
            if running.contains(&key) {
                return RunNowResult {
                    ok: false,
                    running: true,
                    queued: false,
                    skipped: false,
                    session_id: None,
                    task: None,
                    error: Some("task is already running".into()),
                };
            }
            running.insert(key.clone());
        }
        *self.running_global_count.lock().unwrap() += 1;
        {
            let mut count = self.running_count_by_project.lock().unwrap();
            *count.entry(project_id.clone()).or_insert(0) += 1;
        }

        let run_started_at = Utc::now().timestamp_millis();
        // initial patch: lastStatus = running, lastRunAt
        let initial_patch = json!({
            "lastRunAt": run_started_at,
            "lastStatus": "running",
            "updatedAt": run_started_at,
        });
        let _ = self
            .project_config
            .update_scheduled_task_state(&project_id, &task_id, initial_patch)
            .await;

        let mut status = "success".to_string();
        let mut session_id: Option<String> = None;
        let mut duration_ms: i64 = 0;
        let mut error_message: Option<String> = None;

        let project_path = self
            .project_path_by_id
            .lock()
            .unwrap()
            .get(&project_id)
            .cloned();

        let state_opt = self.state.read().unwrap().clone();
        if let Some(state) = state_opt {
            if let Some(path) = project_path {
                let path_clone = path.clone();
                let state_clone = state.clone();
                let emit = |_status: &str| {
                    // consumed by routes / external emitter — currently a no-op
                };
                let emit_ref: &(dyn Fn(&str) + Send + Sync) = &emit;
                let task_clone = task.clone();
                let run_result = execution::run_task_with_watchdog(
                    &state_clone,
                    &path_clone,
                    &task_clone,
                    reason.as_str(),
                    emit_ref,
                    self.max_run_duration_ms,
                )
                .await;
                match run_result {
                    Ok((sid, dur, _finished)) => {
                        session_id = Some(sid);
                        duration_ms = dur;
                        status = "success".to_string();
                    }
                    Err(e) => {
                        status = "error".to_string();
                        error_message = Some(e.to_string());
                    }
                }
            } else {
                status = "error".to_string();
                error_message = Some("project path is unavailable".into());
            }
        } else {
            // No state: degraded mode (rare; shouldn't happen post-start)
            tracing::warn!(project_id=%project_id, "[ScheduledTasks] no AppState bound; treating run as no-op");
        }

        let finished_at = Utc::now().timestamp_millis();
        if duration_ms == 0 {
            duration_ms = (finished_at - run_started_at).max(0);
        }

        // consume one-time task (kind == once, reason == scheduled)
        let mut latest_task = {
            let map = self.tasks_by_project.lock().unwrap();
            map.get(&project_id)
                .and_then(|m| m.get(&task_id))
                .cloned()
                .unwrap_or_else(|| task.clone())
        };
        let schedule_kind = latest_task
            .get("schedule")
            .and_then(|s| s.get("kind"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if schedule_kind == "once" && reason == RunReason::Scheduled {
            let mut disabled = latest_task.clone();
            if let Some(obj) = disabled.as_object_mut() {
                obj.insert("enabled".into(), json!(false));
            }
            if let Ok(res) = self
                .project_config
                .upsert_scheduled_task(&project_id, disabled.clone())
                .await
            {
                if let Some(t) = res.task.as_object() {
                    latest_task = json!(t);
                }
            }
        }

        let next_run_at = compute_next_run_at(&latest_task, finished_at);
        let state_patch = json!({
            "lastStatus": status,
            "lastDurationMs": duration_ms,
            "lastError": if status == "error" {
                json!(error_message.clone().unwrap_or_default())
            } else {
                Value::Null
            },
            "lastSessionId": if status == "success" {
                json!(session_id.clone().unwrap_or_default())
            } else {
                Value::Null
            },
            "nextRunAt": match next_run_at { Some(v) => json!(v), None => Value::Null },
            "updatedAt": finished_at,
        });
        let final_state = match self
            .project_config
            .update_scheduled_task_state(&project_id, &task_id, state_patch)
            .await
        {
            Ok(r) => r.task,
            Err(_) => None,
        };

        if let Some(ref final_task) = final_state {
            let mut map = self.tasks_by_project.lock().unwrap();
            if let Some(project_map) = map.get_mut(&project_id) {
                project_map.insert(task_id.clone(), final_task.clone());
            }
            let enabled = final_task
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let nr = final_task
                .get("state")
                .and_then(|s| s.get("nextRunAt"))
                .and_then(Value::as_i64)
                .filter(|v| *v > 0);
            if enabled {
                if let Some(nr) = nr {
                    self.schedule_task(&project_id, &task_id, nr);
                }
            }
        }

        // release slot
        self.running_task_keys.lock().unwrap().remove(&key);
        let mut g = self.running_global_count.lock().unwrap();
        *g = g.saturating_sub(1);
        drop(g);
        let mut count = self.running_count_by_project.lock().unwrap();
        let cur = count.get(&project_id).copied().unwrap_or(1);
        if cur <= 1 {
            count.remove(&project_id);
        } else {
            count.insert(project_id.clone(), cur - 1);
        }

        RunNowResult {
            ok: status == "success",
            running: false,
            queued: false,
            skipped: false,
            session_id,
            task: final_state,
            error: error_message,
        }
    }

    /// 列出已知 project 的所有 task (in-memory mirror)。
    #[allow(dead_code)]
    pub fn list_tasks_in_memory(&self, project_id: &str) -> Vec<Value> {
        let map = self.tasks_by_project.lock().unwrap();
        map.get(project_id)
            .map(|m| m.values().cloned().collect())
            .unwrap_or_default()
    }
}

/// Read all projects from `settings.json` and return those with valid id + path.
fn list_projects_from_settings() -> Vec<Value> {
    let raw = crate::github::settings::read_settings();
    let projects = raw.get("projects").cloned().unwrap_or(Value::Array(vec![]));
    let arr = projects.as_array().cloned().unwrap_or_default();
    arr.into_iter()
        .filter(|p| {
            p.get("id")
                .and_then(Value::as_str)
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false)
                && p.get("path")
                    .and_then(Value::as_str)
                    .map(|s| !s.trim().is_empty())
                    .unwrap_or(false)
        })
        .collect()
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn empty_pcr() -> Arc<ProjectConfigRuntime> {
        let dir = std::env::temp_dir().join(format!(
            "oc-st-runtime-test-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Arc::new(ProjectConfigRuntime::new(dir))
    }

    #[test]
    fn factory_creates_unstarted_runtime() {
        let rt = create_scheduled_tasks_runtime(empty_pcr(), None, None, None);
        assert!(!rt.is_started());
        let status = rt.get_status();
        assert_eq!(status.enabled_scheduled_tasks_count, 0);
        assert_eq!(status.running_scheduled_tasks_count, 0);
        assert!(!status.has_enabled_scheduled_tasks);
    }

    #[test]
    fn stop_is_idempotent() {
        let rt = create_scheduled_tasks_runtime(empty_pcr(), None, None, None);
        rt.stop();
        rt.stop();
        // 不 panic
    }

    #[test]
    fn can_run_task_respects_global_limit() {
        let rt = create_scheduled_tasks_runtime(empty_pcr(), Some(2), Some(1), None);
        rt.running_task_keys.lock().unwrap().insert("p:t1".to_string());
        *rt.running_global_count.lock().unwrap() = 2;
        assert!(!rt.can_run_task("p"));
        assert!(!rt.can_run_task("p"));
    }

    #[test]
    fn can_run_task_respects_project_limit() {
        let rt = create_scheduled_tasks_runtime(empty_pcr(), Some(10), Some(1), None);
        // global limit OK but project cap reached
        rt.running_task_keys.lock().unwrap().insert("p:t1".to_string());
        *rt.running_global_count.lock().unwrap() = 1;
        rt.running_count_by_project
            .lock()
            .unwrap()
            .insert("p".to_string(), 1);
        // try a different task — same project → still blocked
        assert!(!rt.can_run_task("p"));
        // different project → ok
        rt.running_count_by_project
            .lock()
            .unwrap()
            .insert("q".to_string(), 0);
        // global still at 1, q not in running_count_by_project
        rt.running_count_by_project.lock().unwrap().remove("q");
        assert!(rt.can_run_task("q"));
    }

    #[test]
    fn enqueue_run_dedups_within_queued_set() {
        let rt = create_scheduled_tasks_runtime(empty_pcr(), None, None, None);
        rt.enqueue_run("p", "t1", RunReason::Scheduled);
        rt.enqueue_run("p", "t1", RunReason::Scheduled);
        assert_eq!(rt.queue.lock().unwrap().len(), 1);
        assert!(rt.queued_task_keys.lock().unwrap().contains("p:t1"));
    }

    #[test]
    fn enqueue_run_dedups_with_running_set() {
        let rt = create_scheduled_tasks_runtime(empty_pcr(), None, None, None);
        rt.running_task_keys.lock().unwrap().insert("p:t1".to_string());
        rt.enqueue_run("p", "t1", RunReason::Scheduled);
        assert_eq!(rt.queue.lock().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn sync_project_with_empty_pcr_is_ok() {
        let rt = create_scheduled_tasks_runtime(empty_pcr(), None, None, None);
        let tasks = rt.sync_project("ghost").await.unwrap();
        assert_eq!(tasks.len(), 0);
    }

    #[test]
    fn format_scheduled_session_title_compiles() {
        // smoke — runtime just re-exports the helper from schedule.rs
        let dt = chrono::Utc::now().timestamp_millis();
        let task = json!({ "name": "Demo", "schedule": { "timezone": "UTC" } });
        let s = format_scheduled_session_title(&task, dt);
        assert!(s.starts_with("Demo "));
    }

    #[test]
    fn build_task_key_format() {
        assert_eq!(ScheduledTasksRuntime::build_task_key("p", "t"), "p:t");
    }
}
