//! Session goal — persisted self-continuing objective attached to a session
//! (`metadata.gridforge.goal`). Active goals are ticked after each busy→idle
//! transition: token usage is accounted, the small model is asked to audit
//! progress (`continue` / `complete` / `blocked`), and either a continuation
//! prompt is re-sent to the session's own model or the goal is settled.
//!
//! 对应 Node `session-goal/runtime.js` (764 行) + `session-goal/routes.js` (36 行) +
//! `session-goal/objectives.js` (61 行)。
//!
//! 事件驱动, 纯后台运行: 只在 server 运行期间发生 busy→idle 转移的 session 才会
//! tick; 无 polling / backfill / session 扫描。
//!
//! 子模块:
//! - `audit` — verdict parser + script sanitize
//! - `continuation` — continuation prompt 构造 + XML escape
//! - `metadata` — GoalMetadata 解析 / merge
//! - `objectives` — file-backed objective CRUD
//! - `persistence` — OpenCode session metadata 合并读写 helper
//! - `routes` — 3 个 axum handler

#![allow(dead_code)] // 多数 helper 由 runtime 间接调用, 此处保留以备未来 group 使用

pub mod audit;
pub mod continuation;
pub mod metadata;
pub mod objectives;
pub mod persistence;
pub mod routes;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use tokio::sync::broadcast::error::RecvError;
use tokio::task::JoinHandle;

use crate::opencode::session_client::{build, OpenCodeClient};
use crate::small_model::index::{generate_small_model_text, GenerateArgs};
use crate::state::AppState;

pub use continuation::GoalSnapshot;
pub use metadata::{
    goal_to_value, is_active as is_goal_active, parse_goal_metadata, GoalMetadata,
};

// =========================================================================
// 常量 — 与 Node `session-goal/runtime.js` 严格对齐
// =========================================================================

/// Idle 后等待 tick 的默认时长 (毫秒)。
pub const IDLE_QUIET_MS: u64 = 15_000;
/// Goal 在已 idle 的 session 上首次启动的快速 kickoff。
pub const KICKOFF_QUIET_MS: u64 = 3_000;
/// 显式 Resume 的即时 kickoff。
pub const RESUME_KICKOFF_MS: u64 = 250;
/// OpenCode HTTP 请求超时 (毫秒)。
pub const FETCH_TIMEOUT_MS: u64 = 10_000;
/// 每次 tick 抓取的最近消息数。
pub const MESSAGE_FETCH_LIMIT: u32 = 40;
/// transcript 单段字符上限。
pub const TRANSCRIPT_PART_CHAR_LIMIT: usize = 6_000;
/// note 字符上限。
pub const NOTE_CHAR_LIMIT: usize = 280;
/// statusReason 字符上限。
pub const REASON_CHAR_LIMIT: usize = 200;
/// Auto-continuation 硬上限 (safety cap)。
pub const MAX_AUTO_TURNS: u32 = 20;
/// 连续 audit `blocked` 次数达到后 settle 为 blocked。
pub const BLOCKED_STREAK_LIMIT: u32 = 3;
/// 连续 audit 失败次数达到后 settle 为 blocked。
pub const AUDIT_FAIL_LIMIT: u32 = 2;

/// Settings key — 控制 goal 是否启用 (默认 true)。
pub const SETTINGS_KEY_GOAL_ENABLED: &str = "sessionGoalEnabled";

// =========================================================================
// Timer 状态
// =========================================================================

struct GoalTimer {
    handle: JoinHandle<()>,
    #[allow(dead_code)]
    armed_at: i64,
}

/// Session-goal runtime — 持有 timers/inflight/stopped 状态 + AppState weak ref。
pub struct SessionGoalRuntime {
    timers: Mutex<HashMap<String, GoalTimer>>,
    inflight: Mutex<HashSet<String>>,
    stopped: AtomicBool,
    /// AppState 弱引用 — 由 `start()` 注入, 供 timer 在 spawn 的 task 中使用。
    app_state: std::sync::RwLock<Weak<AppState>>,
}

impl SessionGoalRuntime {
    pub fn new() -> Self {
        Self {
            timers: Mutex::new(HashMap::new()),
            inflight: Mutex::new(HashSet::new()),
            stopped: AtomicBool::new(false),
            app_state: std::sync::RwLock::new(Weak::new()),
        }
    }

    /// 注入 AppState weak ref — 必须在 `start()` 前调用。
    pub fn set_app_state(&self, state: &Arc<AppState>) {
        *self.app_state.write().unwrap() = Arc::downgrade(state);
    }

    /// 启动 GlobalHub 事件消费 task。
    pub fn start(self: Arc<Self>, state: Arc<AppState>) {
        self.set_app_state(&state);

        let mut rx = state.global_hub.subscribe_event();
        let rt = self.clone();
        tokio::spawn(async move {
            tracing::info!("[session-goal] event consumer started");
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        let rt = rt.clone();
                        let payload = event.payload.clone();
                        let directory = event.directory.clone();
                        tokio::spawn(async move {
                            rt.process_payload(&payload, &directory).await;
                        });
                    }
                    Err(RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "[session-goal] consumer lagged");
                        continue;
                    }
                    Err(RecvError::Closed) => {
                        tracing::info!("[session-goal] consumer stopped (hub closed)");
                        break;
                    }
                }
            }
        });
    }

    /// 停止 runtime (abort 所有 timer)。
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Relaxed);
        if let Ok(mut timers) = self.timers.lock() {
            for (_, t) in timers.drain() {
                t.handle.abort();
            }
        }
    }

    fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Relaxed)
    }

    fn clear_timer(&self, session_id: &str) {
        if let Ok(mut timers) = self.timers.lock() {
            if let Some(t) = timers.remove(session_id) {
                t.handle.abort();
            }
        }
    }

    /// 取消旧 timer, 启动新 timer (fire after `delay_ms`)。
    fn arm_timer(self: &Arc<Self>, session_id: String, directory: String, delay_ms: u64) {
        self.clear_timer(&session_id);
        let armed_at = now_millis();
        let rt = self.clone();
        let session_id_for_task = session_id.clone();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            {
                let mut timers = rt.timers.lock().unwrap();
                timers.remove(&session_id_for_task);
            }
            if rt.is_stopped() {
                return;
            }
            {
                let mut inflight = rt.inflight.lock().unwrap();
                if !inflight.insert(session_id_for_task.clone()) {
                    return; // 单飞
                }
            }
            let state = match rt.app_state.read().unwrap().upgrade() {
                Some(s) => s,
                None => {
                    tracing::warn!("[session-goal] app_state dropped, skipping tick");
                    rt.inflight.lock().unwrap().remove(&session_id_for_task);
                    return;
                }
            };
            if let Err(e) = rt
                .clone()
                .tick(&state, &session_id_for_task, &directory)
                .await
            {
                tracing::warn!(session_id = %session_id_for_task, "[session-goal] tick failed: {e}");
            }
            rt.inflight.lock().unwrap().remove(&session_id_for_task);
        });
        self.timers
            .lock()
            .unwrap()
            .insert(session_id, GoalTimer { handle, armed_at });
    }

    /// 处理 GlobalHub 事件 payload。
    pub async fn process_payload(self: &Arc<Self>, payload: &Value, directory_hint: &str) {
        if self.is_stopped() {
            return;
        }

        // 1. User abort → pause_after_abort
        if let Some(aborted) = extract_aborted_assistant(payload) {
            self.clear_timer(&aborted.session_id);
            {
                let inflight = self.inflight.lock().unwrap();
                if inflight.contains(&aborted.session_id) {
                    return;
                }
            }
            let rt = self.clone();
            let session_id = aborted.session_id.clone();
            let directory = directory_hint.to_string();
            {
                let mut inflight = self.inflight.lock().unwrap();
                inflight.insert(session_id.clone());
            }
            tokio::spawn(async move {
                let state = match rt.app_state.read().unwrap().upgrade() {
                    Some(s) => s,
                    None => return,
                };
                if let Err(e) = rt.clone().pause_after_abort(&state, &session_id, &directory).await {
                    tracing::warn!(session_id = %session_id, "[session-goal] pause_after_abort failed: {e}");
                }
                rt.inflight.lock().unwrap().remove(&session_id);
            });
            return;
        }

        // 2. session.status: idle → arm; 其他 → clear
        if let Some(status) = extract_session_status(payload) {
            let dir = if status.directory.is_empty() {
                directory_hint.to_string()
            } else {
                status.directory
            };
            if status.status_type == "idle" {
                self.arm_timer(status.session_id, dir, IDLE_QUIET_MS);
            } else {
                self.clear_timer(&status.session_id);
            }
            return;
        }

        // 3. session.updated kickoff
        if let Some(update) = extract_session_update(payload) {
            if update.parent_id.is_empty() {
                if let Some(g) = &update.goal {
                    if g.status == "active" && (g.turns_used == 0 || g.status_reason == "resumed") {
                        let has_timer = self.timers.lock().unwrap().contains_key(&update.session_id);
                        let has_inflight = self.inflight.lock().unwrap().contains(&update.session_id);
                        if !has_timer && !has_inflight {
                            let delay = if g.status_reason == "resumed" { RESUME_KICKOFF_MS } else { KICKOFF_QUIET_MS };
                            self.arm_timer(update.session_id, update.directory, delay);
                        }
                    }
                }
            }
        }
    }

    /// pause_after_abort — 用户 abort 时立即标记 paused。
    async fn pause_after_abort(self: Arc<Self>, state: &AppState, session_id: &str, directory: &str) -> Result<(), String> {
        let client = build(state);
        let session = client
            .fetch_session(session_id, Some(directory))
            .await
            .map_err(|e| e.to_string())?;
        let Some(session) = session else { return Ok(()) };
        let Some(goal) = parse_goal_metadata(&session) else { return Ok(()) };
        if !is_goal_active(&goal) {
            return Ok(());
        }
        let goal_id = goal.id.clone();
        let _ = self
            .persist_goal_meta(state, session_id, directory, &goal_id, |_| {
                json!({
                    "status": "paused",
                    "statusReason": "paused after abort",
                })
            })
            .await;
        tracing::info!(session_id, "[session-goal] paused after user abort");
        Ok(())
    }

    /// 核心 tick — 状态机 + audit + continuation。
    async fn tick(
        self: Arc<Self>,
        state: &AppState,
        session_id: &str,
        directory: &str,
    ) -> Result<(), String> {
        if !objectives::is_session_goal_enabled() {
            return Ok(());
        }

        let client = build(state);

        let session = match client.fetch_session(session_id, Some(directory)).await {
            Ok(Some(s)) => s,
            Ok(None) => return Ok(()),
            Err(e) => {
                tracing::warn!(session_id, "[session-goal] session fetch failed: {e}");
                return Ok(());
            }
        };

        if session.get("parentID").and_then(Value::as_str).map(|s| !s.is_empty()).unwrap_or(false) {
            return Ok(());
        }

        let goal = match parse_goal_metadata(&session) {
            Some(g) if is_goal_active(&g) => g,
            _ => return Ok(()),
        };

        // File-backed objective
        let mut effective_objective = goal.objective.clone();
        if goal.objective_file {
            match objectives::read_objective(session_id).await {
                Some(file_obj) => effective_objective = file_obj,
                None if effective_objective.is_empty() => {
                    tracing::warn!(session_id, "[session-goal] objective file unreadable and no inline fallback");
                    return Ok(());
                }
                None => {
                    tracing::warn!(session_id, "[session-goal] objective file unreadable, using inline fallback");
                }
            }
        }

        let messages = match fetch_recent_messages(&client, session_id, Some(directory)).await {
            Some(m) => m,
            None => return Ok(()),
        };

        let (last_assistant, last_assistant_info, execution_info, last_message_info) =
            find_last_assistant(&messages);

        // Quiescence 检查
        if last_message_info.as_ref().and_then(|i| i.get("role")).and_then(Value::as_str) == Some("user") {
            return Ok(());
        }
        if let Some(li) = &last_assistant_info {
            let completed = li.get("time").and_then(|t| t.get("completed")).and_then(Value::as_f64).unwrap_or(0.0);
            let has_error = li.get("error").map(|v| v.is_object()).unwrap_or(false);
            if completed <= 0.0 && !has_error {
                return Ok(());
            }
        }

        let last_assistant_info = match last_assistant_info {
            Some(i) => i,
            None => return Ok(()),
        };

        // Token accounting
        let (tokens_baseline, tokens_committed, tokens_used, last_accounted_message_id, _saw_new) =
            account_tokens(&goal, &messages);

        // Terminal: aborted tail (除非 resumed)
        let aborted_tail = last_assistant_info.get("error").and_then(|e| e.get("name")).and_then(Value::as_str) == Some("MessageAbortedError");
        if aborted_tail && goal.status_reason != "resumed" {
            self.persist_goal_meta(state, session_id, directory, &goal.id, |_| json!({
                "status": "paused",
                "statusReason": "paused after abort",
                "tokensUsed": tokens_used,
                "tokensBaseline": tokens_baseline,
                "tokensCommitted": tokens_committed,
                "lastAccountedMessageID": last_accounted_message_id,
            })).await;
            tracing::info!(session_id, "[session-goal] paused after user abort");
            return Ok(());
        }

        // Terminal: turn error
        if !aborted_tail {
            if let Some(err) = last_assistant_info.get("error").filter(|v| v.is_object()) {
                let reason = err.get("name").and_then(Value::as_str).unwrap_or("assistant turn failed");
                self.settle_goal(state, session_id, directory, &goal, "blocked", reason, tokens_used, tokens_baseline, tokens_committed, last_accounted_message_id.clone()).await;
                return Ok(());
            }
        }

        // Terminal: token budget
        if let Some(budget) = goal.token_budget {
            if tokens_used >= budget {
                self.settle_goal(state, session_id, directory, &goal, "budgetLimited", "token budget reached", tokens_used, tokens_baseline, tokens_committed, last_accounted_message_id.clone()).await;
                return Ok(());
            }
        }

        // Terminal: continuation cap
        if goal.turns_used >= MAX_AUTO_TURNS {
            self.settle_goal(state, session_id, directory, &goal, "blocked", "auto-continuation limit reached", tokens_used, tokens_baseline, tokens_committed, last_accounted_message_id.clone()).await;
            return Ok(());
        }

        // Audit
        let (audit_outcome, blocked_streak, audit_fail_streak) =
            if last_assistant_info.get("summary").and_then(Value::as_bool) == Some(true) || aborted_tail {
                (None, goal.blocked_streak, goal.audit_fail_streak)
            } else {
                let execution = execution_info.as_ref().unwrap_or(&last_assistant_info);
                let assistant_text = message_parts_to_text(last_assistant.as_ref().unwrap_or(&Value::Null));
                let audit = run_audit(
                    &goal,
                    &effective_objective,
                    &assistant_text,
                    Some(directory),
                    execution,
                )
                .await;

                let mut blocked_streak = goal.blocked_streak;
                let mut audit_fail_streak = goal.audit_fail_streak;
                let outcome = if audit.is_none() {
                    audit_fail_streak += 1;
                    if audit_fail_streak >= AUDIT_FAIL_LIMIT {
                        self.settle_goal(state, session_id, directory, &goal, "blocked", "progress audit unavailable", tokens_used, tokens_baseline, tokens_committed, last_accounted_message_id.clone()).await;
                        return Ok(());
                    }
                    tracing::warn!(session_id, audit_fail_streak, "[session-goal] audit unavailable, continuing unaudited");
                    None
                } else {
                    audit_fail_streak = 0;
                    audit
                };

                if let Some(out) = &outcome {
                    if out.verdict == audit::Verdict::Complete {
                        self.settle_goal_with_note(
                            state, session_id, directory, &goal,
                            "complete", "verified by audit", &out.note,
                            tokens_used, tokens_baseline, tokens_committed, last_accounted_message_id.clone(),
                        ).await;
                        return Ok(());
                    }
                    if out.verdict == audit::Verdict::Blocked {
                        blocked_streak += 1;
                        if blocked_streak >= BLOCKED_STREAK_LIMIT {
                            let reason = if out.note.is_empty() { "blocked per audit" } else { out.note.as_str() };
                            self.settle_goal_with_note(
                                state, session_id, directory, &goal,
                                "blocked", reason, &out.note,
                                tokens_used, tokens_baseline, tokens_committed, last_accounted_message_id.clone(),
                            ).await;
                            return Ok(());
                        }
                    }
                }
                (outcome, blocked_streak, audit_fail_streak)
            };

        // Continue: persist accounting first
        let written = self
            .persist_goal_meta(state, session_id, directory, &goal.id, |current| {
                let turns_used = current.get("turnsUsed").and_then(Value::as_u64).unwrap_or(0) + 1;
                let mut updates = json!({
                    "tokensUsed": tokens_used,
                    "tokensBaseline": tokens_baseline,
                    "tokensCommitted": tokens_committed,
                    "lastAccountedMessageID": last_accounted_message_id,
                    "turnsUsed": turns_used,
                    "blockedStreak": blocked_streak,
                    "auditFailStreak": audit_fail_streak,
                    "statusReason": "",
                });
                if let Some(out) = &audit_outcome {
                    if !out.note.is_empty() {
                        updates["note"] = json!(&out.note);
                    }
                }
                updates
            })
            .await;
        let written = match written {
            Some(w) => w,
            None => {
                tracing::info!(session_id, "[session-goal] goal changed during tick, dropping continuation");
                return Ok(());
            }
        };

        // Tail-moved-on 检查
        let latest = fetch_recent_messages(&client, session_id, Some(directory)).await;
        let latest_last_id = latest
            .as_ref()
            .and_then(|m| m.last())
            .and_then(|m| m.get("info"))
            .and_then(|i| i.get("id"))
            .and_then(Value::as_str);
        let last_message_id = last_message_info.as_ref().and_then(|i| i.get("id")).and_then(Value::as_str);
        if latest_last_id != last_message_id {
            tracing::info!(session_id, "[session-goal] tail moved on, dropping continuation");
            return Ok(());
        }

        let turns = written.get("turnsUsed").and_then(Value::as_u64).unwrap_or(0);
        let tokens = written.get("tokensUsed").and_then(Value::as_u64).unwrap_or(0);
        tracing::info!(session_id, turns, tokens, "[session-goal] continuing");

        // 提交 continuation
        let execution = execution_info.as_ref().unwrap_or(&last_assistant_info);
        let snapshot = GoalSnapshot {
            objective: effective_objective,
            tokens_used,
            token_budget: goal.token_budget,
            turns_used: turns as u32,
        };
        let body = build_continuation_body(execution, &snapshot);
        if let Err(e) = client.prompt_async(session_id, Some(directory), &body).await {
            tracing::warn!(session_id, "[session-goal] prompt_async failed: {e}");
        }

        Ok(())
    }

    /// Merge-write goal metadata — 重新读取 session + 在 openchamber.goal 上 mutate。
    async fn persist_goal_meta<F>(
        self: &Arc<Self>,
        state: &AppState,
        session_id: &str,
        directory: &str,
        expected_goal_id: &str,
        mutate: F,
    ) -> Option<Value>
    where
        F: FnOnce(&Value) -> Value,
    {
        let client = build(state);
        let session = client.fetch_session(session_id, Some(directory)).await.ok().flatten()?;
        let current_goal = parse_goal_metadata(&session)?;
        if current_goal.id != expected_goal_id {
            return None;
        }
        let current_goal_value = goal_to_value(&current_goal);
        let mut next_goal_value = current_goal_value.clone();
        if let (Some(existing_obj), Some(muts_obj)) = (
            next_goal_value.as_object_mut(),
            mutate(&current_goal_value).as_object(),
        ) {
            for (k, v) in muts_obj {
                existing_obj.insert(k.clone(), v.clone());
            }
        }
        if let Some(obj) = next_goal_value.as_object_mut() {
            obj.insert("updatedAt".to_string(), json!(now_millis()));
        }

        let merged = persistence::merge_key_into_gridforge(&session, "goal", &next_goal_value);
        let _ = client.patch_session_metadata(session_id, Some(directory), &merged).await;
        Some(next_goal_value)
    }

    #[allow(clippy::too_many_arguments)]
    async fn settle_goal(
        self: &Arc<Self>,
        state: &AppState,
        session_id: &str,
        directory: &str,
        goal: &GoalMetadata,
        status: &str,
        status_reason: &str,
        tokens_used: u64,
        tokens_baseline: u64,
        tokens_committed: u64,
        last_accounted_message_id: String,
    ) {
        self.settle_goal_with_note(
            state, session_id, directory, goal,
            status, status_reason, "",
            tokens_used, tokens_baseline, tokens_committed, last_accounted_message_id,
        )
        .await;
    }

    #[allow(clippy::too_many_arguments)]
    async fn settle_goal_with_note(
        self: &Arc<Self>,
        state: &AppState,
        session_id: &str,
        directory: &str,
        goal: &GoalMetadata,
        status: &str,
        status_reason: &str,
        note: &str,
        tokens_used: u64,
        tokens_baseline: u64,
        tokens_committed: u64,
        last_accounted_message_id: String,
    ) {
        let mut updates = json!({
            "status": status,
            "statusReason": status_reason.chars().take(REASON_CHAR_LIMIT).collect::<String>(),
            "blockedStreak": 0,
            "auditFailStreak": 0,
            "tokensUsed": tokens_used,
            "tokensBaseline": tokens_baseline,
            "tokensCommitted": tokens_committed,
            "lastAccountedMessageID": last_accounted_message_id,
        });
        if !note.is_empty() {
            updates["note"] = json!(note.chars().take(NOTE_CHAR_LIMIT).collect::<String>());
        }
        let goal_id = goal.id.clone();
        let written = self.persist_goal_meta(state, session_id, directory, &goal_id, |_| updates).await;
        tracing::info!(session_id, status, "[session-goal] settled");

        if written.is_some() {
            let payload = json!({
                "kind": "goal",
                "sessionID": session_id,
                "status": status,
                "statusReason": status_reason,
                "note": note,
                "title": format!("goal-{}", session_id),
                "body": format!("Goal {} as {}", goal.id, status),
            });
            state.emitter.broadcast_ui_notification(&payload, false);
        }
    }
}

// =========================================================================
// Pure helpers (no IO) — for testing
// =========================================================================

/// 当前时间 (millis since UNIX_EPOCH)。
pub fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 从 session.status event 提取 (sessionId, type, directory)。
pub fn extract_session_status(payload: &Value) -> Option<SessionStatus> {
    if payload.get("type")?.as_str()? != "session.status" {
        return None;
    }
    let properties = payload.get("properties")?;
    let status = properties.get("status").and_then(Value::as_object);
    let info = properties.get("info").and_then(Value::as_object);
    let session_id = properties.get("sessionID").and_then(Value::as_str)?.trim();
    if session_id.is_empty() {
        return None;
    }
    let status_type = status
        .and_then(|s| s.get("type"))
        .and_then(Value::as_str)
        .or_else(|| info.and_then(|i| i.get("type")).and_then(Value::as_str))
        .map(str::trim)?
        .to_string();
    if status_type.is_empty() {
        return None;
    }
    let directory = properties
        .get("directory")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| info.and_then(|i| i.get("directory")).and_then(Value::as_str))
        .unwrap_or("")
        .to_string();
    Some(SessionStatus {
        session_id: session_id.to_string(),
        status_type,
        directory,
    })
}

/// 从 message.updated event 提取 aborted assistant message。
pub fn extract_aborted_assistant(payload: &Value) -> Option<AbortedAssistant> {
    if payload.get("type")?.as_str()? != "message.updated" {
        return None;
    }
    let info = payload.get("properties")?.get("info")?;
    if info.get("role")?.as_str()? != "assistant" {
        return None;
    }
    let error_name = info.get("error")?.get("name")?.as_str()?;
    if error_name != "MessageAbortedError" {
        return None;
    }
    let session_id = info.get("sessionID")?.as_str()?;
    if session_id.is_empty() {
        return None;
    }
    Some(AbortedAssistant { session_id: session_id.to_string() })
}

/// 从 session.updated event 提取 top-level session update。
pub fn extract_session_update(payload: &Value) -> Option<SessionUpdate> {
    if payload.get("type")?.as_str()? != "session.updated" {
        return None;
    }
    let info = payload.get("properties")?.get("info")?;
    let id = info.get("id")?.as_str()?;
    if id.is_empty() {
        return None;
    }
    let directory = info.get("directory").and_then(Value::as_str).unwrap_or("").to_string();
    let parent_id = info.get("parentID").and_then(Value::as_str).unwrap_or("").to_string();

    let goal_value = info.get("metadata").and_then(|m| m.get("gridforge")).and_then(|oc| oc.get("goal")).cloned();
    let goal = goal_value.and_then(|gv| {
        let mut session = serde_json::Map::new();
        let mut metadata = serde_json::Map::new();
        let mut gridforge_namespace = serde_json::Map::new();
        gridforge_namespace.insert("goal".to_string(), gv);
        metadata.insert("gridforge".to_string(), Value::Object(gridforge_namespace));
        session.insert("metadata".to_string(), Value::Object(metadata));
        parse_goal_metadata(&Value::Object(session))
    });

    Some(SessionUpdate { session_id: id.to_string(), directory, parent_id, goal })
}

/// 把消息 parts 转成纯文本。
pub fn message_parts_to_text(message: &Value) -> String {
    let Some(parts) = message.get("parts").and_then(Value::as_array) else {
        return String::new();
    };
    parts
        .iter()
        .filter_map(|p| {
            if p.get("type").and_then(Value::as_str) == Some("text") {
                p.get("text").and_then(Value::as_str)
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        .chars()
        .take(TRANSCRIPT_PART_CHAR_LIMIT)
        .collect()
}

/// 计算单条消息 token 总量: input + cache.read + output。
pub fn message_token_total(info: &Value) -> u64 {
    let Some(tokens) = info.get("tokens").and_then(Value::as_object) else {
        return 0;
    };
    let input = tokens.get("input").and_then(Value::as_f64).map(|n| n.max(0.0) as u64).unwrap_or(0);
    let output = tokens.get("output").and_then(Value::as_f64).map(|n| n.max(0.0) as u64).unwrap_or(0);
    let cached_read = tokens
        .get("cache")
        .and_then(|c| c.get("read"))
        .and_then(Value::as_f64)
        .map(|n| n.max(0.0) as u64)
        .unwrap_or(0);
    input + cached_read + output
}

/// Token accounting — 计算 (tokensBaseline, tokensCommitted, tokensUsed,
/// lastAccountedMessageID, sawNewMessages)。
pub fn account_tokens(
    goal: &GoalMetadata,
    messages: &[Value],
) -> (u64, u64, u64, String, bool) {
    let mut tokens_baseline = goal.tokens_baseline;
    if goal.last_accounted_message_id.is_empty() && tokens_baseline == 0 {
        tokens_baseline = 0;
        for message in messages {
            let info = message.get("info");
            if info.and_then(|i| i.get("role")).and_then(Value::as_str) != Some("assistant") {
                continue;
            }
            let completed = info
                .and_then(|i| i.get("time"))
                .and_then(|t| t.get("completed"))
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            if completed <= 0.0 || completed > goal.created_at as f64 {
                continue;
            }
            tokens_baseline = tokens_baseline.max(message_token_total(info.unwrap_or(&Value::Null)));
        }
    }

    let mut tokens_committed = goal.tokens_committed;
    let mut tokens_used = goal.tokens_used;
    let mut last_accounted_message_id = goal.last_accounted_message_id.clone();
    let mut segment_snapshot: Option<u64> = None;
    let mut saw_new_messages = false;

    for message in messages {
        let info = match message.get("info") {
            Some(i) => i,
            None => continue,
        };
        if info.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let info_id = match info.get("id").and_then(Value::as_str) {
            Some(s) => s.to_string(),
            None => continue,
        };
        if !last_accounted_message_id.is_empty() && info_id.as_str() <= last_accounted_message_id.as_str() {
            continue;
        }
        let completed = info.get("time").and_then(|t| t.get("completed")).and_then(Value::as_f64).unwrap_or(0.0);
        if completed <= 0.0 {
            continue;
        }
        saw_new_messages = true;
        let total = message_token_total(info);
        if info.get("summary").and_then(Value::as_bool) == Some(true) {
            tokens_committed = tokens_committed
                .max(goal.tokens_used)
                .saturating_add(segment_snapshot.unwrap_or(0).saturating_sub(tokens_baseline));
            tokens_baseline = 0;
            segment_snapshot = None;
        } else {
            segment_snapshot = Some(total);
        }
        if last_accounted_message_id.is_empty() || info_id.as_str() > last_accounted_message_id.as_str() {
            last_accounted_message_id = info_id;
        }
    }

    if saw_new_messages {
        let segment_current = segment_snapshot
            .map(|s| s.saturating_sub(tokens_baseline))
            .unwrap_or(0);
        tokens_used = (tokens_committed + segment_current).max(goal.tokens_used);
    }

    (tokens_baseline, tokens_committed, tokens_used, last_accounted_message_id, saw_new_messages)
}

/// 找最后一个 assistant 消息 (含 lastAssistantInfo / executionInfo / lastMessageInfo)。
pub fn find_last_assistant(messages: &[Value]) -> (Option<Value>, Option<Value>, Option<Value>, Option<Value>) {
    let mut last_assistant: Option<Value> = None;
    let mut execution_info: Option<Value> = None;
    for m in messages.iter().rev() {
        let info = m.get("info");
        if let Some(info) = info {
            if info.get("role").and_then(Value::as_str) == Some("assistant") {
                if last_assistant.is_none() {
                    last_assistant = Some(m.clone());
                }
                if execution_info.is_none() && info.get("summary").and_then(Value::as_bool) != Some(true) {
                    execution_info = Some(info.clone());
                }
                if last_assistant.is_some() && execution_info.is_some() {
                    break;
                }
            }
        }
    }
    let last_assistant_info = last_assistant.as_ref().and_then(|m| m.get("info").cloned());
    let last_message_info = messages.last().and_then(|m| m.get("info").cloned());
    (last_assistant, last_assistant_info, execution_info, last_message_info)
}

/// 构造 continuation prompt_async body。
pub fn build_continuation_body(execution_info: &Value, snapshot: &GoalSnapshot) -> Value {
    let provider_id = execution_info.get("providerID").and_then(Value::as_str).unwrap_or("");
    let model_id = execution_info.get("modelID").and_then(Value::as_str).unwrap_or("");
    let agent = execution_info.get("agent").and_then(Value::as_str);
    let variant = execution_info.get("variant").and_then(Value::as_str);
    let prompt_text = continuation::build_continuation_prompt(snapshot);

    let mut body = json!({
        "model": { "providerID": provider_id, "modelID": model_id },
        "parts": [{ "type": "text", "text": prompt_text, "synthetic": true }],
    });
    if let Some(a) = agent.filter(|s| !s.is_empty()) {
        body["agent"] = json!(a);
    }
    if let Some(v) = variant.filter(|s| !s.is_empty()) {
        body["variant"] = json!(v);
    }
    body
}

async fn fetch_recent_messages(
    client: &OpenCodeClient<'_>,
    session_id: &str,
    directory: Option<&str>,
) -> Option<Vec<Value>> {
    client
        .fetch_session_messages(session_id, MESSAGE_FETCH_LIMIT, directory)
        .await
        .ok()
        .flatten()
}

async fn run_audit(
    _goal: &GoalMetadata,
    effective_objective: &str,
    assistant_text: &str,
    directory: Option<&str>,
    last_assistant_info: &Value,
) -> Option<audit::AuditOutcome> {
    let sample = effective_objective
        .chars()
        .take(200)
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let args = GenerateArgs {
        prompt: format!(
            "The goal objective:\n\n<objective>\n{}\n</objective>\n\nThe agent's latest turn:\n\n{}\n\nReturn the verdict JSON. Write the note in the SAME language as this sample from the objective: \"{}\"",
            effective_objective,
            assistant_text,
            sample
        ),
        system: Some(audit::build_audit_system_prompt()),
        directory: directory.map(String::from),
        preferred_provider_id: last_assistant_info.get("providerID").and_then(Value::as_str).map(String::from),
        preferred_model_id: last_assistant_info.get("modelID").and_then(Value::as_str).map(String::from),
        restrict_to_preferred_provider: true,
        ..Default::default()
    };
    let result = match generate_small_model_text(args).await {
        Ok(r) => r,
        Err(e) if e.status_code == 404 => return None,
        Err(e) => {
            tracing::warn!("[session-goal] audit failed: {}", e);
            return None;
        }
    };
    audit::parse_audit_outcome(&result.text, effective_objective, assistant_text)
}

// =========================================================================
// 内部类型 — 事件提取返回结构
// =========================================================================

pub struct SessionStatus {
    pub session_id: String,
    pub status_type: String,
    pub directory: String,
}

pub struct AbortedAssistant {
    pub session_id: String,
}

pub struct SessionUpdate {
    pub session_id: String,
    pub directory: String,
    pub parent_id: String,
    pub goal: Option<GoalMetadata>,
}

// =========================================================================
// 测试 (纯 helpers; 不需要 IO)
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn now_millis_is_positive() {
        assert!(now_millis() > 1_700_000_000_000);
    }

    #[test]
    fn extract_session_status_idle() {
        let payload = json!({
            "type": "session.status",
            "properties": {
                "sessionID": "sess_1",
                "directory": "/work",
                "status": { "type": "idle" }
            }
        });
        let s = extract_session_status(&payload).unwrap();
        assert_eq!(s.session_id, "sess_1");
        assert_eq!(s.status_type, "idle");
        assert_eq!(s.directory, "/work");
    }

    #[test]
    fn extract_session_status_falls_back_to_info_type() {
        let payload = json!({
            "type": "session.status",
            "properties": {
                "sessionID": "sess_2",
                "info": { "type": "busy", "directory": "/d" }
            }
        });
        let s = extract_session_status(&payload).unwrap();
        assert_eq!(s.status_type, "busy");
        assert_eq!(s.directory, "/d");
    }

    #[test]
    fn extract_session_status_returns_none_on_missing() {
        assert!(extract_session_status(&json!({"type": "session.created"})).is_none());
        assert!(extract_session_status(&json!({})).is_none());
    }

    #[test]
    fn extract_aborted_assistant_matches() {
        let payload = json!({
            "type": "message.updated",
            "properties": {
                "info": {
                    "id": "m1",
                    "role": "assistant",
                    "sessionID": "s1",
                    "error": { "name": "MessageAbortedError" }
                }
            }
        });
        let r = extract_aborted_assistant(&payload).unwrap();
        assert_eq!(r.session_id, "s1");
    }

    #[test]
    fn extract_aborted_assistant_ignores_other_errors() {
        let payload = json!({
            "type": "message.updated",
            "properties": {
                "info": {
                    "role": "assistant",
                    "sessionID": "s1",
                    "error": { "name": "OtherError" }
                }
            }
        });
        assert!(extract_aborted_assistant(&payload).is_none());
    }

    #[test]
    fn extract_aborted_assistant_ignores_user_role() {
        let payload = json!({
            "type": "message.updated",
            "properties": {
                "info": { "role": "user", "sessionID": "s1", "error": { "name": "MessageAbortedError" } }
            }
        });
        assert!(extract_aborted_assistant(&payload).is_none());
    }

    #[test]
    fn extract_session_update_top_level() {
        let payload = json!({
            "type": "session.updated",
            "properties": {
                "info": {
                    "id": "s1",
                    "directory": "/d",
                    "metadata": {
                        "gridforge": {
                            "goal": { "id": "g1", "objective": "x", "status": "active" }
                        }
                    }
                }
            }
        });
        let r = extract_session_update(&payload).unwrap();
        assert_eq!(r.session_id, "s1");
        assert_eq!(r.directory, "/d");
        assert!(r.parent_id.is_empty());
        assert!(r.goal.is_some());
        assert_eq!(r.goal.unwrap().id, "g1");
    }

    #[test]
    fn extract_session_update_skips_subagent() {
        let payload = json!({
            "type": "session.updated",
            "properties": {
                "info": { "id": "s1", "parentID": "parent_1", "directory": "/d" }
            }
        });
        let r = extract_session_update(&payload).unwrap();
        assert_eq!(r.parent_id, "parent_1");
        assert!(r.goal.is_none());
    }

    #[test]
    fn message_parts_to_text_basic() {
        let m = json!({
            "info": { "role": "assistant" },
            "parts": [
                { "type": "text", "text": "hello" },
                { "type": "tool", "id": "x" },
                { "type": "text", "text": "world" }
            ]
        });
        assert_eq!(message_parts_to_text(&m), "hello\nworld");
    }

    #[test]
    fn message_parts_to_text_respects_char_limit() {
        let long = "x".repeat(TRANSCRIPT_PART_CHAR_LIMIT + 100);
        let m = json!({
            "parts": [{ "type": "text", "text": long }]
        });
        let result = message_parts_to_text(&m);
        assert_eq!(result.len(), TRANSCRIPT_PART_CHAR_LIMIT);
    }

    #[test]
    fn message_token_total_basic() {
        let info = json!({
            "tokens": {
                "input": 100,
                "output": 50,
                "cache": { "read": 25 }
            }
        });
        assert_eq!(message_token_total(&info), 175);
    }

    #[test]
    fn message_token_total_handles_missing() {
        assert_eq!(message_token_total(&json!({})), 0);
        assert_eq!(message_token_total(&json!({"tokens": {}})), 0);
    }

    #[test]
    fn find_last_assistant_basic() {
        let messages = vec![
            json!({ "info": { "id": "u1", "role": "user" } }),
            json!({ "info": { "id": "a1", "role": "assistant", "summary": false, "providerID": "openai" } }),
        ];
        let (last_a, last_info, exec, last_m) = find_last_assistant(&messages);
        assert!(last_a.is_some());
        assert_eq!(last_info.unwrap()["id"], "a1");
        assert_eq!(exec.unwrap()["providerID"], "openai");
        assert_eq!(last_m.unwrap()["id"], "a1");
    }

    #[test]
    fn find_last_assistant_skips_summary_for_execution() {
        let messages = vec![
            json!({ "info": { "id": "a1", "role": "assistant", "summary": true } }),
            json!({ "info": { "id": "a2", "role": "assistant", "summary": false, "providerID": "openai" } }),
        ];
        let (_last_a, last_info, exec, _) = find_last_assistant(&messages);
        // lastAssistantInfo 是 messages 最后一条 (a2), 跳过 summary 应得 a2 的 executionInfo
        assert_eq!(last_info.unwrap()["id"], "a2");
        assert_eq!(exec.unwrap()["id"], "a2");
    }

    #[test]
    fn account_tokens_initial() {
        let goal = GoalMetadata {
            id: "g".into(),
            objective: "x".into(),
            status: "active".into(),
            created_at: 200,
            ..Default::default()
        };
        let messages = vec![
            json!({
                "info": { "id": "a1", "role": "assistant", "time": { "completed": 100.0 }, "tokens": { "input": 50 } }
            }),
        ];
        let (baseline, committed, used, last_id, saw_new) = account_tokens(&goal, &messages);
        // baseline 锁定为 message_token_total (50)
        assert_eq!(baseline, 50);
        assert_eq!(committed, 0);
        assert_eq!(used, 0);
        assert_eq!(last_id, "a1");
        assert!(saw_new);
    }

    #[test]
    fn account_tokens_skips_already_accounted() {
        let goal = GoalMetadata {
            id: "g".into(),
            objective: "x".into(),
            status: "active".into(),
            last_accounted_message_id: "a2".into(),
            ..Default::default()
        };
        let messages = vec![
            json!({ "info": { "id": "a1", "role": "assistant", "time": { "completed": 100.0 } } }),
            json!({ "info": { "id": "a2", "role": "assistant", "time": { "completed": 200.0 } } }),
            json!({ "info": { "id": "a3", "role": "assistant", "time": { "completed": 300.0 }, "tokens": { "input": 80 } } }),
        ];
        let (_b, _c, _u, last_id, saw_new) = account_tokens(&goal, &messages);
        assert_eq!(last_id, "a3");
        assert!(saw_new);
    }

    #[test]
    fn build_continuation_body_basic() {
        let exec = json!({
            "providerID": "openai",
            "modelID": "gpt-4",
            "agent": "build",
            "variant": "v1"
        });
        let snap = GoalSnapshot {
            objective: "x".into(),
            tokens_used: 100,
            token_budget: Some(1000),
            turns_used: 1,
        };
        let body = build_continuation_body(&exec, &snap);
        assert_eq!(body["model"]["providerID"], "openai");
        assert_eq!(body["model"]["modelID"], "gpt-4");
        assert_eq!(body["agent"], "build");
        assert_eq!(body["variant"], "v1");
        assert_eq!(body["parts"][0]["synthetic"], true);
    }

    #[test]
    fn build_continuation_body_skips_empty_agent_variant() {
        let exec = json!({ "providerID": "openai", "modelID": "gpt-4" });
        let snap = GoalSnapshot {
            objective: "x".into(),
            tokens_used: 0,
            token_budget: None,
            turns_used: 0,
        };
        let body = build_continuation_body(&exec, &snap);
        assert!(body.get("agent").is_none());
        assert!(body.get("variant").is_none());
    }
}