//! 执行引擎 — scheduled task 真正到点时的运行路径。
//!
//! 移植自 Node `scheduled-tasks/runtime.js` 的 `runTaskWithWatchdog`,
//! `buildPromptAsyncPayload`, `buildGoalIntroText`, `createTaskGoal`,
//! `runScheduledCommandIfApplicable` 以及 `expandSnippets` (后者简化实现)。
//!
//! 公开 helper (供 runtime 调用):
//! - `build_prompt_async_payload`
//! - `build_goal_intro_text`
//! - `expand_snippets` (简化 — 无 snippet 解析, 直接返回原文)
//! - `create_task_goal` (file-backed objective + PATCH metadata + 蒸馏)
//! - `run_prompt_async`
//! - `run_scheduled_command_if_applicable`
//! - `run_task_with_watchdog`

use std::time::{Duration, Instant};

use chrono::Utc;
use oc_core::Error;
use serde_json::{json, Value};

use crate::opencode::session_client::build;
use crate::session_goal::objectives;
use crate::small_model::index::{generate_small_model_text, GenerateArgs};
use crate::state::AppState;

use super::schedule::{
    compute_next_run_at, format_scheduled_session_title, parse_scheduled_command_prompt,
};

/// Goal intro 字符上限 (与 Node 一致: 5000)。
pub const GOAL_OBJECTIVE_CHAR_LIMIT: usize = 5_000;
/// Goal 最大 token budget 默认值 (供 worker agent 参考)。
pub const TASK_DUE_SLACK_MS: i64 = 5_000;
/// 蒸馏 prompt 长度阈值 (Node: 5000 chars 以上走 small-model distill)。
const DISTILL_THRESHOLD: usize = 5_000;

// =========================================================================
// expand_snippets — 简化版: 无 hashtag/loop 解析, 直接返回原文。
// =========================================================================

/// 简易 `expandSnippets` — 无 snippet 仓库解析, 返回原文。
///
/// TODO: Node 解析 `#name` → 加载 markdown snippet 文件 (`~/.config/opencode/snippet/*.md`
/// 或 `<project>/.opencode/snippets/*.md`) 并展开。简化版对 Rust 端已足够避免新依赖。
pub fn expand_snippets(text: &str, _project_path: Option<&str>) -> String {
    text.to_string()
}

// =========================================================================
// Build helpers — pure, covered by unit tests
// =========================================================================

/// 构造 `/session/{id}/prompt_async` 请求 body。
///
/// 与 Node `buildPromptAsyncPayload` 字节对齐:
/// - `model: { providerID, modelID }`
/// - 可选 `agent: <string>`
/// - 可选 `variant: <string>`
/// - `parts[0]`: expand_snippets(prompt)
/// - `parts[1]` (可选): `buildGoalIntroText(token_budget)` with `synthetic: true`
pub fn build_prompt_async_payload(task: &Value, project_path: Option<&str>) -> Value {
    let execution = task.get("execution").cloned().unwrap_or(json!({}));
    let provider_id = execution
        .get("providerID")
        .and_then(Value::as_str)
        .unwrap_or("");
    let model_id = execution
        .get("modelID")
        .and_then(Value::as_str)
        .unwrap_or("");
    let agent = execution
        .get("agent")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let variant = execution
        .get("variant")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let goal_enabled = execution
        .get("goalEnabled")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let goal_token_budget = execution
        .get("goalTokenBudget")
        .and_then(Value::as_i64);

    let prompt_text = execution
        .get("prompt")
        .and_then(Value::as_str)
        .unwrap_or("");

    let expanded = expand_snippets(prompt_text, project_path);

    let mut parts = vec![json!({ "type": "text", "text": expanded })];
    if goal_enabled {
        parts.push(json!({
            "type": "text",
            "text": build_goal_intro_text(goal_token_budget),
            "synthetic": true
        }));
    }

    let mut body = json!({
        "model": { "providerID": provider_id, "modelID": model_id },
        "parts": parts
    });
    if let Some(a) = agent {
        body["agent"] = json!(a);
    }
    if let Some(v) = variant {
        body["variant"] = json!(v);
    }
    body
}

/// Goal mode `<system-reminder>` 文本 — Node `buildGoalIntroText`。
pub fn build_goal_intro_text(token_budget: Option<i64>) -> String {
    let budget_line = match token_budget {
        Some(b) if b > 0 => format!(" A token budget of {} tokens applies to this goal.", b),
        _ => String::new(),
    };
    format!(
        "<system-reminder>\nGoal mode is active for this session. The user message above defines the goal objective. Work toward it across turns; whenever you stop before the objective is verifiably complete, the system will automatically prompt you to continue. Progress is evaluated independently after each turn, so end every turn with a clear, factual statement of what is done, what was verified, and what remains.{}\n</system-reminder>",
        budget_line
    )
}

// =========================================================================
// create_task_goal — file-backed objective + small-model distillation + PATCH
// =========================================================================

/// Stamp goal metadata on a fresh session. Mirrors Node `createTaskGoal`.
///
/// Step:
/// 1. 准备 objectiveText (expand_snippets)
/// 2. > 5000 chars → try `generate_small_model_text` for distillation
/// 3. 失败 → head+tail excerpt with marker
/// 4. 写 `$OPENCHAMBER_DATA_DIR/goals/{session_id}.md` (objective_file = true)
/// 5. 失败 → inline clamp
/// 6. PATCH `metadata.openchamber.goal = {...}`
pub async fn create_task_goal(
    state: &AppState,
    session_id: &str,
    project_path: &str,
    task: &Value,
) -> Result<(), Error> {
    let now_ms = Utc::now().timestamp_millis();

    let mut objective_text = expand_snippets(
        task.get("execution")
            .and_then(|e| e.get("prompt"))
            .and_then(Value::as_str)
            .unwrap_or(""),
        Some(project_path),
    );

    if objective_text.len() > DISTILL_THRESHOLD {
        let distilled = try_distill_objective(state, &objective_text, project_path, task).await;
        match distilled {
            Some(d) => {
                objective_text = d;
            }
            None => {
                let marker = "\n\n[... objective trimmed for the auditor — the full prompt was delivered in the chat message ...]\n\n";
                let half = (GOAL_OBJECTIVE_CHAR_LIMIT.saturating_sub(marker.len())) / 2;
                let head: String = objective_text.chars().take(half).collect();
                let tail_chars: String = objective_text
                    .chars()
                    .rev()
                    .take(half)
                    .collect::<String>()
                    .chars()
                    .rev()
                    .collect();
                objective_text = format!("{}{}{}", head, marker, tail_chars);
            }
        }
    }

    let mut objective_file = false;
    // clamp inline just in case
    let clamped_inline: String = objective_text
        .chars()
        .take(GOAL_OBJECTIVE_CHAR_LIMIT)
        .collect();
    if let Err(e) = objectives::write_objective(session_id, &clamped_inline).await {
        tracing::warn!(session_id, "[scheduled-tasks] write objective file failed: {e}");
    } else {
        objective_file = true;
    }

    let goal_id = format!(
        "{:x}{:x}",
        now_ms,
        rand::random::<u32>()
    );
    let token_budget = task
        .get("execution")
        .and_then(|e| e.get("goalTokenBudget"))
        .and_then(Value::as_i64);

    let goal_value = json!({
        "id": goal_id,
        "objective": if objective_file { String::new() } else { clamped_inline },
        "objectiveFile": objective_file,
        "status": "active",
        "tokenBudget": token_budget,
        "tokensUsed": 0,
        "turnsUsed": 0,
        "blockedStreak": 0,
        "note": "",
        "statusReason": "",
        "lastAccountedMessageID": "",
        "createdAt": now_ms,
        "updatedAt": now_ms,
    });

    let openchamber_namespace = json!({ "goal": goal_value });
    let payload = json!({
        "metadata": { "openchamber": openchamber_namespace }
    });

    let client = build(state);
    client
        .patch_session_metadata(session_id, Some(project_path), &payload)
        .await
        .map_err(|e| Error::Internal(format!("create_task_goal patch failed: {e}")))?;
    Ok(())
}

async fn try_distill_objective(
    state: &AppState,
    text: &str,
    project_path: &str,
    task: &Value,
) -> Option<String> {
    let preferred_provider_id = task
        .get("execution")
        .and_then(|e| e.get("providerID"))
        .and_then(Value::as_str)
        .map(String::from);
    let preferred_model_id = task
        .get("execution")
        .and_then(|e| e.get("modelID"))
        .and_then(Value::as_str)
        .map(String::from);

    let system = "You distill a large task description into the COMPLETION CRITERIA a progress auditor will judge against.\n\
                   Return ONLY the criteria text — no preamble, no headers, no markdown fences.\n\
                   Capture: the end goals, what must exist and work when the task is fully done, and how each major part is verified. Omit implementation steps.\n\
                   Preserve verbatim any file paths, commands, and identifiers that define the task.\n\
                   Stay under 4000 characters.\n\
                   Write in the same language as the task text.";

    let res = generate_small_model_text(GenerateArgs {
        prompt: text.to_string(),
        system: Some(system.to_string()),
        directory: Some(project_path.to_string()),
        preferred_provider_id,
        preferred_model_id,
        restrict_to_preferred_provider: true,
        ..Default::default()
    })
    .await;

    match res {
        Ok(g) => Some(g.text.trim().to_string()),
        Err(e) => {
            tracing::warn!(
                "[scheduled-tasks] goal objective distillation failed: {}",
                e
            );
            None
        }
    }
}

// =========================================================================
// OpenCode HTTP calls — POST /session/{id}/prompt_async, POST /session/{id}/command
// =========================================================================

/// POST `/session/{id}/prompt_async` body.
pub async fn run_prompt_async(
    state: &AppState,
    session_id: &str,
    project_path: &str,
    body: &Value,
) -> Result<(), Error> {
    let client = build(state);
    client
        .prompt_async(session_id, Some(project_path), body)
        .await
        .map_err(|e| Error::Upstream {
            status: 502,
            body: format!("prompt_async failed: {e}"),
        })?;
    Ok(())
}

/// If prompt is a slash command and the command is registered, dispatch via
/// `POST /session/{id}/command`. Returns Ok(true) if executed as command.
pub async fn run_scheduled_command_if_applicable(
    state: &AppState,
    session_id: &str,
    project_path: &str,
    task: &Value,
) -> Result<bool, Error> {
    let prompt = task
        .get("execution")
        .and_then(|e| e.get("prompt"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let parsed = match parse_scheduled_command_prompt(prompt) {
        Some(p) => p,
        None => return Ok(false),
    };

    let (command_name, arguments) = parsed;

    // GET /command?directory=...
    let list_url = format!("{}/command", state.opencode_base_url.trim_end_matches('/'));
    let list_resp = state
        .http_client
        .get(&list_url)
        .query(&[("directory", project_path)])
        .header("accept", "application/json")
        .header("authorization", &state.opencode_auth_header)
        .send()
        .await
        .map_err(|e| Error::Internal(format!("command.list failed: {e}")))?;
    let matched = if list_resp.status().is_success() {
        let json: Value = list_resp
            .json()
            .await
            .unwrap_or_else(|_| Value::Array(vec![]));
        let arr = json.get("data").cloned().unwrap_or(json);
        arr.as_array()
            .map(|items| {
                items
                    .iter()
                    .any(|c| c.get("name").and_then(Value::as_str) == Some(command_name.as_str()))
            })
            .unwrap_or(false)
    } else {
        false
    };
    if !matched {
        return Ok(false);
    }

    // POST /session/{id}/command
    let execution = task.get("execution");
    let provider_id = execution
        .and_then(|e| e.get("providerID"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let model_id = execution
        .and_then(|e| e.get("modelID"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let agent = execution
        .and_then(|e| e.get("agent"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let variant = execution
        .and_then(|e| e.get("variant"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let mut body = json!({
        "directory": project_path,
        "command": command_name,
        "arguments": arguments,
        "model": format!("{}/{}", provider_id, model_id),
    });
    if let Some(a) = agent {
        body["agent"] = json!(a);
    }
    if let Some(v) = variant {
        body["variant"] = json!(v);
    }

    let url = format!(
        "{}/session/{}/command",
        state.opencode_base_url.trim_end_matches('/'),
        session_id
    );
    let resp = state
        .http_client
        .post(&url)
        .query(&[("directory", project_path)])
        .header("accept", "application/json")
        .header("content-type", "application/json")
        .header("authorization", &state.opencode_auth_header)
        .json(&body)
        .send()
        .await
        .map_err(|e| Error::Internal(format!("session.command failed: {e}")))?;
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        return Err(Error::Upstream { status, body });
    }
    Ok(true)
}

/// 创建 session 并执行 task (含 watchdog 超时)。
///
/// `reason` = "scheduled" | "manual"
/// `max_run_ms` 默认 30 分钟 (Node `DEFAULT_MAX_RUN_MS`).
///
/// 返回 `(session_id, duration_ms, finished_at)`。
pub async fn run_task_with_watchdog(
    state: &AppState,
    project_path: &str,
    task: &Value,
    reason: &str,
    emit_task_event: &(dyn Fn(&str) + Send + Sync),
    max_run_ms: u64,
) -> Result<(String, i64, i64), Error> {
    let started_at = Utc::now().timestamp_millis();
    let title = format_scheduled_session_title(task, started_at);

    let client = build(state);
    let created_session = client
        .create_session(Some(project_path), &json!({ "title": title }))
        .await
        .map_err(|e| Error::Internal(format!("create_session failed: {e}")))?;
    let session_id = created_session
        .as_ref()
        .and_then(|v| v.get("data").and_then(|d| d.get("id")).cloned())
        .or_else(|| created_session.as_ref().and_then(|v| v.get("id").cloned()))
        .and_then(|v| v.as_str().map(String::from))
        .ok_or_else(|| Error::Internal("failed to create session".into()))?;

    emit_task_event("running");

    // 1. goal mode — first stamp the goal metadata
    let goal_enabled = task
        .get("execution")
        .and_then(|e| e.get("goalEnabled"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if goal_enabled {
        create_task_goal(state, &session_id, project_path, task).await?;
    }

    // 2. decide: slash-command or plain prompt
    let started = Instant::now();
    let inner = async {
        let as_command = run_scheduled_command_if_applicable(state, &session_id, project_path, task).await?;
        if !as_command {
            let body = build_prompt_async_payload(task, Some(project_path));
            run_prompt_async(state, &session_id, project_path, &body).await?;
        }
        Ok::<_, Error>(())
    };

    let result = tokio::time::timeout(Duration::from_millis(max_run_ms), inner).await;
    let finished_at = Utc::now().timestamp_millis();
    let duration_ms = (started.elapsed().as_millis() as i64).max(0);

    match result {
        Ok(Ok(())) => {
            tracing::info!(
                session_id = %session_id,
                duration_ms,
                reason,
                "[ScheduledTasks] run completed"
            );
            Ok((session_id, duration_ms, finished_at))
        }
        Ok(Err(e)) => {
            tracing::warn!(
                session_id = %session_id,
                reason,
                "[ScheduledTasks] run failed: {e}"
            );
            Err(e)
        }
        Err(_) => {
            tracing::warn!(
                session_id = %session_id,
                reason,
                max_run_ms,
                "[ScheduledTasks] run timed out"
            );
            Err(Error::Internal(format!(
                "scheduled task run timed out after {} ms",
                max_run_ms
            )))
        }
    }
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn build_prompt_async_payload_passes_through() {
        let task = json!({
            "execution": {
                "providerID": "openai",
                "modelID": "gpt-4o",
                "agent": "build",
                "variant": "fast",
                "prompt": "do thing"
            }
        });
        let body = build_prompt_async_payload(&task, Some("/work"));
        assert_eq!(body["model"]["providerID"], "openai");
        assert_eq!(body["model"]["modelID"], "gpt-4o");
        assert_eq!(body["agent"], "build");
        assert_eq!(body["variant"], "fast");
        assert_eq!(body["parts"][0]["text"], "do thing");
        assert_eq!(body["parts"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn build_prompt_async_payload_appends_goal_intro() {
        let task = json!({
            "execution": {
                "providerID": "openai",
                "modelID": "gpt-4o",
                "prompt": "x",
                "goalEnabled": true,
                "goalTokenBudget": 1000
            }
        });
        let body = build_prompt_async_payload(&task, Some("/work"));
        assert_eq!(body["parts"].as_array().unwrap().len(), 2);
        let intro = body["parts"][1]["text"].as_str().unwrap();
        assert!(intro.starts_with("<system-reminder>"));
        assert!(intro.contains("token budget of 1000"));
        assert_eq!(body["parts"][1]["synthetic"], true);
    }

    #[test]
    fn build_prompt_async_payload_skips_empty_agent_variant() {
        let task = json!({
            "execution": { "providerID": "openai", "modelID": "gpt-4o", "prompt": "x" }
        });
        let body = build_prompt_async_payload(&task, Some("/work"));
        assert!(body.get("agent").is_none());
        assert!(body.get("variant").is_none());
    }

    #[test]
    fn build_goal_intro_no_budget() {
        let intro = build_goal_intro_text(None);
        assert!(intro.contains("Goal mode is active"));
        assert!(!intro.contains("token budget"));
    }

    #[test]
    fn build_goal_intro_with_zero_budget_skips_phrase() {
        let intro = build_goal_intro_text(Some(0));
        assert!(!intro.contains("token budget"));
    }

    #[test]
    fn build_goal_intro_with_positive_budget() {
        let intro = build_goal_intro_text(Some(500));
        assert!(intro.contains("A token budget of 500 tokens"));
    }

    #[test]
    fn expand_snippets_passthrough() {
        // 简化: 直接返回原文
        assert_eq!(expand_snippets("#hello", Some("/work")), "#hello");
        assert_eq!(expand_snippets("", Some("/work")), "");
    }
}
