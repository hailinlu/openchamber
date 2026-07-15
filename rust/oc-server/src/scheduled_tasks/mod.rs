//! Scheduled-tasks module — Rust port of Node `packages/web/server/lib/scheduled-tasks/`.
//!
//! 子模块:
//! - `schedule` — pure schedule parsing (`parse_scheduled_command_prompt`,
//!   `compute_next_run_at`, `format_scheduled_session_title`)
//! - `project_config` — per-project scheduled-tasks JSON persistence
//! - `execution` — runtime helpers (`build_prompt_async_payload`,
//!   `build_goal_intro_text`, `expand_snippets`, `create_task_goal`,
//!   `run_scheduled_command_if_applicable`, `run_task_with_watchdog`, ...)
//! - `runtime` — `ScheduledTasksRuntime` (state machine + timer arming +
//!   queue + concurrency limits)
//! - `routes` — HTTP handlers (`/api/projects/:id/scheduled-tasks/*`,
//!   `/api/openchamber/scheduled-tasks/status`, `/api/openchamber/events` SSE)

#![allow(dead_code)] // 部分 helper 由 runtime 间接使用

pub mod execution;
pub mod project_config;
pub mod routes;
pub mod runtime;
pub mod schedule;

use std::sync::Arc;

pub use project_config::ProjectConfigRuntime;
pub use runtime::{
    create_scheduled_tasks_runtime, ScheduledTasksRuntime,
};

/// 初始化 scheduled-tasks module 时的工厂 — 包装 `project_config::new`
/// + `runtime::create_scheduled_tasks_runtime`, 注入 AppState 后启动 runtime。
///
/// 调用方: `AppState::init_scheduled_tasks(self: &Arc<Self>)`。
pub fn build_default_runtime() -> Arc<ScheduledTasksRuntime> {
    let cfg_runtime = Arc::new(ProjectConfigRuntime::new(
        project_config::default_projects_dir(),
    ));
    create_scheduled_tasks_runtime(cfg_runtime, None, None, None)
}

#[allow(dead_code)]
fn _ensure_sync() -> Arc<ScheduledTasksRuntime> {
    build_default_runtime()
}
