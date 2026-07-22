//! Session-assist — busy→idle 后等待 60s 静默期, 调用 small model 生成
//! recap + suggestion, 写入 `metadata.gridforge.assist`。
//!
//! 对应 Node `session-assist/runtime.js` (394 行) + DOCUMENTATION.md。
//!
//! 纯事件驱动: 只在 server 运行期间发生 busy→idle 转移的 session 才会生成。
//! 无 polling / backfill / session 扫描。
//!
//! 关键 invariant (与 Node 一致):
//! - session.status.type === "idle" → arm 60s timer
//! - 其他 session.status → clear timer
//! - message.updated user.createdAt >= timer.armedAt → clear timer (用户换话题)
//! - 生成期间 inflight 单飞
//! - sub-agent session (parentID truthy) → skip
//! - 生成后 tail-moved-on 检查 (re-fetch + 比 lastAssistantInfo.id)
//! - 生成 strict-JSON system prompt, 字段长度 clamp, Cyrillic/CJK sanitize
//! - 语言由 conversation sample 决定 (账户侧个性化会泄露, 用示例控制)
//! - merge-write metadata.gridforge (保留 dismissals/goal/review 等)

#![allow(dead_code)] // 部分 helper 暂未直接调用

pub mod metadata;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use serde_json::Value;
use tokio::sync::broadcast::error::RecvError;
use tokio::task::JoinHandle;

use crate::opencode::session_client::build;
use crate::small_model::index::{generate_small_model_text, GenerateArgs};
use crate::state::AppState;

pub use metadata::{clamp_recap, clamp_suggestion, merge_assist_into_gridforge, AssistMetadata};

// =========================================================================
// 常量 — 与 Node `session-assist/runtime.js` 严格对齐
// =========================================================================

/// Idle 后等待生成 recap 的时长 (毫秒)。
pub const IDLE_QUIET_MS: u64 = 60_000;
/// 抓取最近消息数。
pub const TRANSCRIPT_MESSAGE_LIMIT: u32 = 12;
/// transcript 单段字符上限。
pub const TRANSCRIPT_PART_CHAR_LIMIT: usize = 6_000;
/// OpenCode HTTP 请求超时。
pub const FETCH_TIMEOUT_MS: u64 = 5_000;
/// Settings key — recap 开关 (默认 true)。
pub const SETTINGS_KEY_RECAP: &str = "sessionRecapEnabled";
/// Settings key — suggestion 开关 (默认 true)。
pub const SETTINGS_KEY_SUGGESTION: &str = "sessionSuggestionEnabled";

// =========================================================================
// Timer 状态
// =========================================================================

struct AssistTimer {
    handle: JoinHandle<()>,
    armed_at: i64,
}

/// Session-assist runtime — 持有 timers/inflight/stopped 状态 + AppState weak ref。
pub struct SessionAssistRuntime {
    timers: Mutex<HashMap<String, AssistTimer>>,
    inflight: Mutex<HashSet<String>>,
    stopped: AtomicBool,
    app_state: std::sync::RwLock<Weak<AppState>>,
}

impl SessionAssistRuntime {
    pub fn new() -> Self {
        Self {
            timers: Mutex::new(HashMap::new()),
            inflight: Mutex::new(HashSet::new()),
            stopped: AtomicBool::new(false),
            app_state: std::sync::RwLock::new(Weak::new()),
        }
    }

    fn set_app_state(&self, state: &Arc<AppState>) {
        *self.app_state.write().unwrap() = Arc::downgrade(state);
    }

    /// 启动 GlobalHub 事件消费 task。
    pub fn start(self: Arc<Self>, state: Arc<AppState>) {
        self.set_app_state(&state);

        let mut rx = state.global_hub.subscribe_event();
        let rt = self.clone();
        tokio::spawn(async move {
            tracing::info!("[session-assist] event consumer started");
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
                        tracing::warn!(skipped = n, "[session-assist] consumer lagged");
                        continue;
                    }
                    Err(RecvError::Closed) => {
                        tracing::info!("[session-assist] consumer stopped (hub closed)");
                        break;
                    }
                }
            }
        });
    }

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

    fn arm_timer(self: &Arc<Self>, session_id: String, directory: String, delay_ms: u64) {
        self.clear_timer(&session_id);
        let armed_at = crate::session_goal::now_millis();
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
                    rt.inflight.lock().unwrap().remove(&session_id_for_task);
                    return;
                }
            };
            if let Err(e) = rt.generate_assist(&state, &session_id_for_task, &directory).await {
                tracing::warn!(session_id = %session_id_for_task, "[session-assist] generate failed: {e}");
            }
            rt.inflight.lock().unwrap().remove(&session_id_for_task);
        });
        self.timers
            .lock()
            .unwrap()
            .insert(session_id, AssistTimer { handle, armed_at });
    }

    /// 处理 GlobalHub 事件 payload。
    pub async fn process_payload(self: &Arc<Self>, payload: &Value, directory_hint: &str) {
        if self.is_stopped() {
            return;
        }

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

        if let Some(user_msg) = extract_user_message(payload) {
            // OpenCode 在 session settle 后会 re-emit 旧 message.updated;
            // 只有 createdAt >= armedAt 的消息才是用户真正继续
            let armed = self.timers.lock().unwrap().get(&user_msg.session_id).map(|t| t.armed_at);
            if let Some(armed_at) = armed {
                if user_msg.created_at >= armed_at {
                    self.clear_timer(&user_msg.session_id);
                }
            }
        }
    }

    /// 生成 recap + suggestion + 写入 metadata。
    async fn generate_assist(self: &Arc<Self>, state: &AppState, session_id: &str, directory: &str) -> Result<(), String> {
        // 1. settings 开关 — 两者都关闭 → 直接返回 (已有 payload 不动)
        let targets = read_session_assist_targets();
        if !targets.recap && !targets.suggestion {
            return Ok(());
        }

        let client = build(state);

        // 2. 读 session + 检查 sub-agent
        let session = client
            .fetch_session(session_id, Some(directory))
            .await
            .map_err(|e| e.to_string())?;
        let Some(session) = session else { return Ok(()) };
        if session.get("parentID").and_then(Value::as_str).map(|s: &str| !s.is_empty()).unwrap_or(false) {
            return Ok(()); // sub-agent skip
        }

        // 3. 抓最近消息 + 找最后一个 assistant
        let messages = match client
            .fetch_session_messages(session_id, TRANSCRIPT_MESSAGE_LIMIT, Some(directory))
            .await
        {
            Ok(Some(m)) => m,
            _ => {
                tracing::warn!(session_id, "[session-assist] no messages fetched");
                return Ok(());
            }
        };

        let last_assistant = messages.iter().rev().find_map(|m| {
            let info = m.get("info")?;
            if info.get("role").and_then(Value::as_str) == Some("assistant") {
                Some(m.clone())
            } else {
                None
            }
        });
        let last_assistant_info = last_assistant.as_ref().and_then(|m| m.get("info"));
        let last_assistant_id = last_assistant_info.and_then(|i| i.get("id")).and_then(Value::as_str);
        if last_assistant_id.is_none() {
            return Ok(());
        }
        let last_assistant_id = last_assistant_id.unwrap().to_string();

        // 4. 构造 transcript — 只取最后 user → assistant 一组 (lastAssistantInfo.parentID)
        let parent_user_message = last_assistant_info
            .and_then(|i| i.get("parentID"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .and_then(|parent_id| {
                messages.iter().find(|m| {
                    m.get("info")
                        .and_then(|i| i.get("id"))
                        .and_then(Value::as_str) == Some(parent_id)
                        && m.get("info").and_then(|i| i.get("role")).and_then(Value::as_str) == Some("user")
                }).cloned()
            });
        let user_text = parent_user_message
            .as_ref()
            .map(message_parts_to_text)
            .unwrap_or_default();
        let assistant_text = message_parts_to_text(last_assistant.as_ref().unwrap_or(&Value::Null));
        let transcript = [
            if !user_text.is_empty() { format!("User:\n{user_text}") } else { String::new() },
            if !assistant_text.is_empty() { format!("Assistant:\n{assistant_text}") } else { String::new() },
        ]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
        if transcript.is_empty() {
            return Ok(());
        }

        // 5. 调 small model
        let language_sample = (user_text.clone() + &assistant_text)
            .chars()
            .take(200)
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let requested_fields = match (targets.recap, targets.suggestion) {
            (true, true) => "recap and suggestion",
            (true, false) => "recap",
            (false, true) => "suggestion",
            (false, false) => unreachable!(),
        };
        let preferred_provider = last_assistant_info.and_then(|i| i.get("providerID")).and_then(Value::as_str).map(String::from);
        let preferred_model = last_assistant_info.and_then(|i| i.get("modelID")).and_then(Value::as_str).map(String::from);

        let prompt = format!(
            "The latest exchange in the conversation:\n\n{transcript}\n\nWrite {requested_fields} in the SAME language as this sample from the conversation: \"{language_sample}\""
        );

        let generated = match generate_small_model_text(GenerateArgs {
            prompt,
            system: Some(build_assist_system_prompt(targets)),
            directory: Some(directory.to_string()),
            preferred_provider_id: preferred_provider,
            preferred_model_id: preferred_model,
            restrict_to_preferred_provider: true,
            ..Default::default()
        })
        .await
        {
            Ok(r) => r,
            Err(e) if e.status_code == 404 => return Ok(()), // 无 provider, 静默
            Err(e) => {
                tracing::warn!("[session-assist] generation failed: {}", e);
                return Ok(());
            }
        };

        // 6. 解析 trailing JSON
        let structured = extract_json_object(&generated.text);

        let mut recap = if targets.recap {
            structured
                .as_ref()
                .and_then(|s| s.get("recap"))
                .and_then(Value::as_str)
                .map(clamp_recap)
                .unwrap_or_default()
        } else {
            String::new()
        };
        let mut suggestion = if targets.suggestion {
            structured
                .as_ref()
                .and_then(|s| s.get("suggestion"))
                .and_then(Value::as_str)
                .map(clamp_suggestion)
                .unwrap_or_default()
        } else {
            String::new()
        };

        // 7. Cyrillic/CJK sanitize
        let input_text = format!("{user_text}\n{assistant_text}");
        if !recap.is_empty() && script_mismatch(&recap, &input_text) {
            tracing::warn!(session_id, "[session-assist] dropped recap: language mismatch");
            recap.clear();
        }
        if !suggestion.is_empty() && script_mismatch(&suggestion, &input_text) {
            tracing::warn!(session_id, "[session-assist] dropped suggestion: language mismatch");
            suggestion.clear();
        }
        if recap.is_empty() && suggestion.is_empty() {
            return Ok(());
        }

        // 8. Tail-moved-on 检查 — 重新 fetch 后比 lastAssistantId
        let latest = client
            .fetch_session_messages(session_id, TRANSCRIPT_MESSAGE_LIMIT, Some(directory))
            .await
            .ok()
            .flatten();
        let latest_assistant_id = latest
            .as_ref()
            .and_then(|m| {
                for msg in m.iter().rev() {
                    let info = msg.get("info");
                    if let Some(info) = info {
                        if info.get("role").and_then(Value::as_str) == Some("assistant") {
                            return info.get("id").and_then(Value::as_str);
                        }
                        if info.get("role").and_then(Value::as_str) == Some("user") {
                            return None;
                        }
                    }
                }
                None
            });
        if latest_assistant_id != Some(last_assistant_id.as_str()) {
            tracing::info!(session_id, "[session-assist] tail moved on, dropping result");
            return Ok(());
        }

        // 9. 写入 metadata (fresh merge)
        let fresh_session = client
            .fetch_session(session_id, Some(directory))
            .await
            .ok()
            .flatten()
            .unwrap_or(session.clone());
        let assist = AssistMetadata {
            recap: recap.clone(),
            suggestion: suggestion.clone(),
            for_message_id: last_assistant_id.clone(),
            generated_at: crate::session_goal::now_millis(),
        };
        let merged = merge_assist_into_gridforge(&fresh_session, &assist);
        let _ = client.patch_session_metadata(session_id, Some(directory), &merged).await;

        tracing::info!(session_id, provider = %generated.provider_id, model = %generated.model_id, "[session-assist] generated");
        Ok(())
    }
}

// =========================================================================
// Pure helpers (无 IO, 便于测试)
// =========================================================================

/// Settings target: recap/suggestion 开关。
#[derive(Debug, Clone, Copy)]
pub struct AssistTargets {
    pub recap: bool,
    pub suggestion: bool,
}

/// 从 settings.json 读 `sessionRecapEnabled` / `sessionSuggestionEnabled` (默认 true)。
pub fn read_session_assist_targets() -> AssistTargets {
    let v = crate::github::settings::read_settings();
    AssistTargets {
        recap: v.get(SETTINGS_KEY_RECAP).and_then(Value::as_bool).unwrap_or(true),
        suggestion: v.get(SETTINGS_KEY_SUGGESTION).and_then(Value::as_bool).unwrap_or(true),
    }
}

/// 构造 strict-JSON system prompt — Node `buildAssistSystemPrompt({recap, suggestion})`。
pub fn build_assist_system_prompt(targets: AssistTargets) -> String {
    let shape = match (targets.recap, targets.suggestion) {
        (true, true) => r#"Shape: {"recap": string, "suggestion": string}"#,
        (true, false) => r#"Shape: {"recap": string}"#,
        (false, true) => r#"Shape: {"suggestion": string}"#,
        (false, false) => r#"Shape: {}"#,
    };
    let mut parts: Vec<String> = vec![
        "You assist a user who chats with a coding agent. Based on the conversation transcript, return exactly one JSON object and nothing else — no prose, no markdown, no code fences.".into(),
        shape.to_string(),
    ];
    if targets.recap {
        parts.push("recap: at most 20 words. State the substance directly — the facts, result, or conclusion, plus the next move if there is one. NEVER narrate (\"The assistant explained…\", \"The agent did…\") — write the content itself, like a note the user jotted down.".into());
    }
    if targets.suggestion {
        parts.extend([
            "suggestion: write ONE immediately sendable next user message addressed TO the coding agent.".into(),
            "The suggestion should be the most useful next step after the assistant's latest reply. It should help the user continue productively, not inspect already-known details.".into(),
            "Prefer suggestions that ask the agent to make a concrete improvement, implement something specific, validate the latest change, explain tradeoffs, improve the current approach, or continue from the current result.".into(),
            "Rules for suggestion:".into(),
            "- Output exactly one message the user could click and send without editing.".into(),
            "- Pick one best next action yourself.".into(),
            "- Do not include alternatives, choices, slash-separated options, or \"or\".".into(),
            "- Do not write \"Do X or Y\", \"Ask whether...\", \"Maybe...\", or \"You could...\".".into(),
            "- Do not ask for information the assistant already provided.".into(),
            "- Do not ask to see exact code, file paths, prompt locations, or implementation internals unless the assistant did not provide them and they are necessary for the next step.".into(),
            "- Do not produce generic workflow commands like \"Run tests\" unless testing is clearly the next unresolved step.".into(),
            "- Do not produce meta/debug requests that merely inspect the implementation.".into(),
            "- Use imperative or question form.".into(),
            "- Keep it concise.".into(),
            "Use these examples to understand how to choose the suggestion. Do not copy their topic or wording unless the current conversation is about the same thing.".into(),
            "Example 1:".into(),
            "Assistant reply summary:".into(),
            "The assistant already identified the file where the feature is implemented, explained what context is sent to the small model, and summarized the current prompt.".into(),
            "Bad suggestion:".into(),
            "\"Show me the exact runtime.js code and where the prompt is built.\"".into(),
            "Why bad:".into(),
            "It asks for information the assistant already provided. It repeats inspection instead of moving to an improvement or decision.".into(),
            "Good suggestion:".into(),
            "\"Suggest how to improve the prompt and context so the generated suggestion is more useful.\"".into(),
            "Why good:".into(),
            "It naturally continues from the analysis and asks for a concrete improvement.".into(),
            "Example 2:".into(),
            "Assistant reply summary:".into(),
            "The assistant implemented a timeline dialog redesign, listed concrete UI changes, and reported that type-check and lint passed.".into(),
            "Bad suggestion:".into(),
            "\"Check whether scrolling or loading older messages works without jumps.\"".into(),
            "Why bad:".into(),
            "It contains an alternative. A suggestion chip must be one sendable message, not a choice the user has to edit.".into(),
            "Good suggestion:".into(),
            "\"Check whether scrolling and loading older messages work without jumps.\"".into(),
            "Why good:".into(),
            "It picks a single validation request that the user can send immediately.".into(),
        ]);
    }
    parts.push("All requested values MUST be written in the same language as the conversation text itself. Ignore any other language preferences or personalization you may have — only the conversation text decides the language.".into());
    parts.push("Use double quotes for JSON strings, no trailing commas.".into());
    parts.join("\n")
}

/// 从模型输出中提取最尾部 JSON 对象 (同 session_goal::audit::extract_json_object)。
pub fn extract_json_object(value: &str) -> Option<Value> {
    crate::session_goal::audit::extract_json_object(value)
}

/// Cyrillic / CJK script mismatch 检测。
pub fn script_mismatch(text: &str, input_text: &str) -> bool {
    let has_cyrillic = |t: &str| t.chars().any(|c| matches!(c, '\u{0400}'..='\u{04FF}'));
    let has_cjk = |t: &str| {
        t.chars().any(|c| {
            matches!(c,
                '\u{3040}'..='\u{30FF}' |
                '\u{4E00}'..='\u{9FFF}' |
                '\u{AC00}'..='\u{D7AF}'
            )
        })
    };
    (has_cyrillic(text) && !has_cyrillic(input_text))
        || (has_cjk(text) && !has_cjk(input_text))
}

/// 消息 parts → 纯文本。
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

/// 从 session.status event 提取 (sessionId, type, directory)。
pub fn extract_session_status(payload: &Value) -> Option<AssistSessionStatus> {
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
    Some(AssistSessionStatus {
        session_id: session_id.to_string(),
        status_type,
        directory,
    })
}

/// 从 message.updated event 提取 user message (用于 tail-moved-on 检查)。
pub fn extract_user_message(payload: &Value) -> Option<AssistUserMessage> {
    if payload.get("type")?.as_str()? != "message.updated" {
        return None;
    }
    let info = payload.get("properties")?.get("info")?;
    if info.get("role")?.as_str()? != "user" {
        return None;
    }
    let session_id = info.get("sessionID")?.as_str()?;
    if session_id.is_empty() {
        return None;
    }
    let created_at = info.get("time").and_then(|t| t.get("created")).and_then(Value::as_i64).unwrap_or(0);
    Some(AssistUserMessage {
        session_id: session_id.to_string(),
        created_at,
    })
}

pub struct AssistSessionStatus {
    pub session_id: String,
    pub status_type: String,
    pub directory: String,
}

pub struct AssistUserMessage {
    pub session_id: String,
    pub created_at: i64,
}

// =========================================================================
// 测试
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn build_system_prompt_full() {
        let p = build_assist_system_prompt(AssistTargets { recap: true, suggestion: true });
        assert!(p.contains(r#"Shape: {"recap": string, "suggestion": string}"#));
        assert!(p.contains("recap: at most 20 words"));
        assert!(p.contains("Rules for suggestion"));
        assert!(p.contains("Example 1"));
        assert!(p.contains("Example 2"));
        assert!(p.contains("All requested values MUST be written"));
    }

    #[test]
    fn build_system_prompt_recap_only() {
        let p = build_assist_system_prompt(AssistTargets { recap: true, suggestion: false });
        assert!(p.contains(r#"Shape: {"recap": string}"#));
        assert!(p.contains("recap: at most 20 words"));
        assert!(!p.contains("Rules for suggestion"));
    }

    #[test]
    fn build_system_prompt_suggestion_only() {
        let p = build_assist_system_prompt(AssistTargets { recap: false, suggestion: true });
        assert!(p.contains(r#"Shape: {"suggestion": string}"#));
        assert!(!p.contains("recap: at most 20 words"));
        assert!(p.contains("Rules for suggestion"));
    }

    #[test]
    fn extract_json_object_simple_and_prose() {
        let v = extract_json_object(r#"{"recap":"X","suggestion":"Y"}"#).unwrap();
        assert_eq!(v["recap"], "X");
        assert_eq!(v["suggestion"], "Y");

        let v2 = extract_json_object("Some prose\n\n{\"recap\":\"A\"}").unwrap();
        assert_eq!(v2["recap"], "A");
    }

    #[test]
    fn extract_json_object_handles_fence() {
        let v = extract_json_object("```json\n{\"recap\":\"A\",\"suggestion\":\"B\"}\n```").unwrap();
        assert_eq!(v["recap"], "A");
    }

    #[test]
    fn script_mismatch_detects_hallucination() {
        // 对话无 Cyrillic, 但 text 有 → mismatch
        assert!(script_mismatch("продолжаем", "agent did X"));
        // 对话无 CJK, 但 text 有 → mismatch
        assert!(script_mismatch("继续工作", "agent did X"));
        // 匹配 (input 也有) → no mismatch
        assert!(!script_mismatch("продолжаем", "собрать виджет"));
        assert!(!script_mismatch("继续工作", "构建 widget"));
        // 都没有 → no mismatch
        assert!(!script_mismatch("continue", "agent did X"));
    }

    #[test]
    fn message_parts_to_text_basic() {
        let m = json!({
            "parts": [
                { "type": "text", "text": "hello" },
                { "type": "tool" },
                { "type": "text", "text": "world" }
            ]
        });
        assert_eq!(message_parts_to_text(&m), "hello\nworld");
    }

    #[test]
    fn extract_session_status_idle() {
        let payload = json!({
            "type": "session.status",
            "properties": {
                "sessionID": "s1",
                "directory": "/d",
                "status": { "type": "idle" }
            }
        });
        let s = extract_session_status(&payload).unwrap();
        assert_eq!(s.status_type, "idle");
        assert_eq!(s.directory, "/d");
    }

    #[test]
    fn extract_user_message_basic() {
        let payload = json!({
            "type": "message.updated",
            "properties": {
                "info": { "role": "user", "sessionID": "s1", "time": { "created": 100 } }
            }
        });
        let m = extract_user_message(&payload).unwrap();
        assert_eq!(m.session_id, "s1");
        assert_eq!(m.created_at, 100);
    }

    #[test]
    fn extract_user_message_ignores_assistant() {
        let payload = json!({
            "type": "message.updated",
            "properties": {
                "info": { "role": "assistant", "sessionID": "s1" }
            }
        });
        assert!(extract_user_message(&payload).is_none());
    }
}