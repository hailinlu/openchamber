//! Trigger fanout orchestrator — cooldown/debounce/badge/suppression/goal。
//!
//! 对应 Node `notifications/runtime.js` (`createNotificationTriggerRuntime`, 755 行)。
//!
//! 这不是 axum handler, 而是 async orchestrator。从 GlobalHub 订阅 SSE 事件,
//! 对每个事件调用 `maybe_send_push_for_trigger` 做通知决策。
//!
//! **核心逻辑**:
//! - `session.idle`/`session.error` → 重写为 `message.updated` 递归
//! - `message.updated` + assistant finish=stop → ready 通知 (cooldown 5s, subtask/goal/focus gate)
//! - `message.updated` + assistant finish=error → error 通知 (cooldown)
//! - `question.asked` → debounce 500ms → 通知
//! - `permission.asked`/`permission.replied` → debounce 500ms, auto-accept suppression

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;

use regex::Regex;
use serde_json::{json, Value};
use tokio::task::JoinHandle;

use super::apns_send::{ApnsPayload, ApnsSendRuntime};
use super::emitter::NotificationEmitter;
use super::message::prepare_notification_last_message;
use super::push_send::{PushSendOptions, PushSendRuntime};
use super::push_store::PushStore;
use super::session_state::SessionStateRuntime;
use super::template::NotificationTemplateRuntime;
use super::types::{extract_directory_from_payload, extract_session_id_from_payload, get_parent_id_from_payload};
use super::{
    now_millis, PUSH_PERMISSION_DEBOUNCE_MS, PUSH_QUESTION_DEBOUNCE_MS, PUSH_READY_COOLDOWN_MS,
};

/// 缓存的 `(?i)plan\s*mode` 正则。
fn re_plan_mode() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"(?i)plan\s*mode").unwrap())
}

/// 缓存的 `(?i)build\s*agent` 正则。
fn re_build_agent() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"(?i)build\s*agent").unwrap())
}

/// APNs 标题 by type (对应 Node `APNS_TITLE_BY_TYPE`)。
fn apns_title_by_type(type_str: &str) -> &'static str {
    match type_str {
        "ready" => "Agent response is ready",
        "error" => "Agent hit an error",
        "question" => "Agent needs your input",
        "permission" => "Agent needs permission",
        "goal_complete" => "Goal complete",
        "goal_blocked" => "Goal blocked",
        "goal_budget" => "Goal reached its token budget",
        _ => "Agent update",
    }
}

/// Notification trigger fanout 运行时。
///
/// 始终以 `Arc<NotificationTrigger>` 形式使用, 这样 debounce timer 的 spawned task
/// 可以持有 clone。
pub struct NotificationTrigger {
    push_store: Arc<PushStore>,
    emitter: Arc<NotificationEmitter>,
    template: Arc<NotificationTemplateRuntime>,
    push_send: Arc<PushSendRuntime>,
    apns_send: Arc<ApnsSendRuntime>,
    #[allow(dead_code)]
    session_state: Arc<SessionStateRuntime>,

    opencode_base_url: String,
    auth_header: String,

    // cooldown maps
    last_ready: Mutex<HashMap<String, i64>>,
    last_error: Mutex<HashMap<String, i64>>,

    // suppression
    notified_permissions: Mutex<HashSet<String>>,
    auto_accept_sessions: Mutex<HashSet<String>>,

    // badge tracking (pending push tags)
    pending_push_tags: Mutex<HashSet<String>>,

    // debounce timers
    question_timers: Mutex<HashMap<String, JoinHandle<()>>>,
    #[allow(clippy::type_complexity)]
    permission_timers: Mutex<HashMap<String, (JoinHandle<()>, Option<String>)>>,
}

impl NotificationTrigger {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        push_store: Arc<PushStore>,
        emitter: Arc<NotificationEmitter>,
        template: Arc<NotificationTemplateRuntime>,
        push_send: Arc<PushSendRuntime>,
        apns_send: Arc<ApnsSendRuntime>,
        session_state: Arc<SessionStateRuntime>,
        opencode_base_url: String,
        auth_header: String,
    ) -> Self {
        Self {
            push_store,
            emitter,
            template,
            push_send,
            apns_send,
            session_state,
            opencode_base_url,
            auth_header,
            last_ready: Mutex::new(HashMap::new()),
            last_error: Mutex::new(HashMap::new()),
            notified_permissions: Mutex::new(HashSet::new()),
            auto_accept_sessions: Mutex::new(HashSet::new()),
            pending_push_tags: Mutex::new(HashSet::new()),
            question_timers: Mutex::new(HashMap::new()),
            permission_timers: Mutex::new(HashMap::new()),
        }
    }

    // -----------------------------------------------------------------------
    // Badge
    // -----------------------------------------------------------------------

    /// 清除 pending push badge。
    pub fn clear_pending_push_badge(&self) {
        self.pending_push_tags.lock().unwrap().clear();
    }

    /// 记录 push tag 并返回当前 badge count。
    fn track_push_and_count_badge(&self, tag: Option<&str>) -> usize {
        let mut tags = self.pending_push_tags.lock().unwrap();
        if let Some(t) = tag {
            if !t.is_empty() {
                tags.insert(t.to_string());
            }
        }
        tags.len()
    }

    // -----------------------------------------------------------------------
    // Auto-accept
    // -----------------------------------------------------------------------

    /// 设置 session auto-accept 状态。
    pub fn set_auto_accept_session(&self, session_id: &str, enabled: bool) {
        if session_id.is_empty() {
            return;
        }
        let mut sessions = self.auto_accept_sessions.lock().unwrap();
        if enabled {
            sessions.insert(session_id.to_string());
        } else {
            sessions.remove(session_id);
        }
    }

    /// session 是否 auto-accepting。
    ///
    /// 对应 Node `isSessionAutoAccepting`。简化版: 只检查当前 session,
    /// parent chain 需要网络请求, 这里降级为仅当前 session 检查。
    async fn is_session_auto_accepting(&self, session_id: &str, _directory: Option<&str>) -> bool {
        if session_id.is_empty() {
            return false;
        }
        let sessions = self.auto_accept_sessions.lock().unwrap();
        if sessions.is_empty() {
            return false;
        }
        sessions.contains(session_id)
    }

    // -----------------------------------------------------------------------
    // APNs generic payload
    // -----------------------------------------------------------------------

    /// 构建 APNs generic payload (无内容泄露)。
    ///
    /// 对应 Node `toApnsGenericPayload`。
    fn to_apns_generic_payload(&self, payload: &Value) -> ApnsPayload {
        let data = payload
            .get("data")
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();

        let session_name = data
            .get("sessionName")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "Session".to_string());

        let notif_type = data.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let title = apns_title_by_type(notif_type).to_string();

        let tag = payload.get("tag").and_then(|v| v.as_str()).map(|s| s.to_string());
        let badge = self.track_push_and_count_badge(tag.as_deref());

        let apns_data = data
            .get("sessionId")
            .and_then(|v| v.as_str())
            .map(|sid| json!({ "sessionId": sid }));

        ApnsPayload {
            title,
            body: session_name,
            badge: Some(badge as i64),
            tag,
            data: apns_data,
        }
    }

    // -----------------------------------------------------------------------
    // Fanout
    // -----------------------------------------------------------------------

    /// 扇出通知到 web-push + APNs (fire-and-forget)。
    ///
    /// 对应 Node `fanoutPush`。web-push 完整 payload, APNs generic payload。
    /// APNs 门控: 如果有 interactive client visible 则跳过。
    async fn fanout_push(&self, payload: Value, options: PushSendOptions) {
        let interactive_visible = self.push_store.is_any_interactive_client_visible();

        // web-push (fire-and-forget)
        let push_send = self.push_send.clone();
        let web_payload = payload.clone();
        let web_options = options.clone();
        tokio::spawn(async move {
            push_send.send_to_all_ui_sessions(&web_payload, &web_options).await;
        });

        // APNs (门控 interactive visible)
        if !interactive_visible {
            let apns_payload = self.to_apns_generic_payload(&payload);
            let apns_send = self.apns_send.clone();
            tokio::spawn(async move {
                apns_send.send_to_all_ui_sessions(&apns_payload).await;
            });
        }
    }

    // -----------------------------------------------------------------------
    // Main trigger entry
    // -----------------------------------------------------------------------

    /// 处理 SSE payload, 做通知决策。
    ///
    /// 对应 Node `maybeSendPushForTrigger`。
    /// `session.idle` / `session.error` 会被重写为 `message.updated` 再处理 (非递归)。
    pub async fn maybe_send_push_for_trigger(self: Arc<Self>, payload: Value) {
        if !payload.is_object() {
            return;
        }

        let mut effective_payload = payload;

        // session.idle / session.error → 重写为 message.updated (仅一层)
        let payload_type = effective_payload.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if payload_type == "session.idle" || payload_type == "session.error" {
            let session_id = extract_session_id_from_payload(&effective_payload);
            if let Some(ref sid) = session_id {
                effective_payload = Self::rewrite_to_message_updated(&effective_payload, sid, payload_type);
            }
        }

        self.process_payload(effective_payload).await;
    }

    /// 实际处理单个 payload (非递归)。
    async fn process_payload(self: Arc<Self>, payload: Value) {
        let session_id = extract_session_id_from_payload(&payload);
        let notification_directory = extract_directory_from_payload(&payload);
        let payload_type = payload.get("type").and_then(|v| v.as_str()).unwrap_or("");

        if payload_type == "message.updated" {
            self.handle_message_updated(&payload, session_id.as_deref(), notification_directory.as_deref())
                .await;
            return;
        }

        if payload_type == "question.asked" {
            if let Some(ref sid) = session_id {
                self.clone().handle_question_asked_arc(
                    payload,
                    sid.clone(),
                    notification_directory.clone(),
                );
            }
            return;
        }

        if payload_type == "permission.replied" {
            if let Some(ref sid) = session_id {
                self.handle_permission_replied(&payload, sid);
            }
            return;
        }

        if payload_type == "permission.asked" {
            if let Some(ref sid) = session_id {
                self.clone().handle_permission_asked_arc(
                    payload,
                    sid.clone(),
                    notification_directory.clone(),
                )
                .await;
            }
        }
    }

    /// 将 session.idle/session.error 重写为 message.updated。
    fn rewrite_to_message_updated(payload: &Value, session_id: &str, original_type: &str) -> Value {
        let error = payload.get("properties").and_then(|p| p.get("error"));
        let error_text = error
            .and_then(|e| {
                e.get("message")
                    .and_then(|v| v.as_str())
                    .or_else(|| e.as_str())
            })
            .unwrap_or("");

        let finish = if original_type == "session.error" {
            "error"
        } else {
            "stop"
        };

        let mut info = json!({
            "sessionID": session_id,
            "role": "assistant",
            "finish": finish,
        });
        if !error_text.is_empty() {
            info["parts"] = json!([{ "type": "text", "text": error_text }]);
        }

        let mut rewritten = payload.clone();
        rewritten["type"] = json!("message.updated");
        rewritten["properties"]["info"] = info;
        rewritten
    }

    /// 处理 message.updated (ready/error 通知)。
    async fn handle_message_updated(
        &self,
        payload: &Value,
        session_id: Option<&str>,
        directory: Option<&str>,
    ) {
        let info = match payload.get("properties").and_then(|p| p.get("info")) {
            Some(i) => i,
            None => return,
        };
        let role = info.get("role").and_then(|v| v.as_str());
        let finish = info.get("finish").and_then(|v| v.as_str());

        let session_id = match session_id {
            Some(s) if !s.is_empty() => s,
            _ => return,
        };

        // ready notification
        if role == Some("assistant") && finish == Some("stop") {
            self.handle_ready_notification(payload, session_id, directory, info)
                .await;
        }

        // error notification
        if role == Some("assistant") && finish == Some("error") {
            self.handle_error_notification(payload, session_id, directory, info)
                .await;
        }
    }

    /// 处理 ready 通知 (cooldown + subtask/goal/focus gate + template + fanout)。
    async fn handle_ready_notification(
        &self,
        payload: &Value,
        session_id: &str,
        directory: Option<&str>,
        info: &Value,
    ) {
        let settings = crate::github::settings::read_settings();

        // subtask gate
        if settings.get("notifyOnSubtasks").and_then(|v| v.as_bool()) == Some(false) {
            let parent_id = get_parent_id_from_payload(payload);
            let has_parent = match parent_id {
                Some(Some(_)) => true,
                Some(None) => false,
                None => {
                    self.template
                        .fetch_session_parent_id(
                            session_id,
                            directory,
                            &self.opencode_base_url,
                            &self.auth_header,
                        )
                        .await
                        .map(|p| p.is_some())
                        .unwrap_or(false)
                }
            };
            if has_parent {
                return;
            }
        }

        // completion gate
        if settings.get("notifyOnCompletion").and_then(|v| v.as_bool()) == Some(false) {
            return;
        }

        // window focus gate (getIsWindowFocused — Tauri/Electron 注入; 当前无注入 → 不跳过)
        let notification_mode = settings.get("notificationMode").and_then(|v| v.as_str());

        // cooldown
        let now = now_millis();
        {
            let last_ready = self.last_ready.lock().unwrap();
            if let Some(&last_at) = last_ready.get(session_id) {
                if now - last_at < PUSH_READY_COOLDOWN_MS {
                    return;
                }
            }
        }
        self.last_ready.lock().unwrap().insert(session_id.to_string(), now);

        // 默认标题/正文
        let mode_str = info.get("mode").and_then(|v| v.as_str()).unwrap_or("");
        let model_id = info.get("modelID").and_then(|v| v.as_str()).unwrap_or("");
        let mut title = format!("{} agent is ready", super::types::format_mode(mode_str));
        let mut body = format!(
            "{} completed the task",
            super::types::format_model_id(model_id)
        );
        let mut session_name = String::new();

        // template resolve
        let mut variables = self
            .template
            .build_template_variables(payload, session_id, &self.opencode_base_url, &self.auth_header)
            .await;
        session_name.clone_from(&variables.session_name);

        let last_message = NotificationTemplateRuntime::extract_last_message_text(payload);
        variables.last_message = prepare_notification_last_message(
            Some(&last_message),
            settings.get("maxLastMessageLength").and_then(|v| v.as_i64()),
        );

        // 模板
        if let Some(templates) = settings.get("notificationTemplates").and_then(|v| v.as_object()) {
            let completion_template =
                templates.get("completion").cloned().unwrap_or_else(|| {
                    json!({ "title": "{agent_name} is ready", "message": "{model_name} completed the task" })
                });
            let resolved_title = NotificationTemplateRuntime::resolve_template(
                completion_template
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("{agent_name} is ready"),
                &variables,
            );
            let resolved_body = NotificationTemplateRuntime::resolve_template(
                completion_template
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("{model_name} completed the task"),
                &variables,
            );
            if !resolved_title.is_empty() {
                title = resolved_title;
            }
            if NotificationTemplateRuntime::should_apply_resolved_message(
                completion_template
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or(""),
                &resolved_body,
                &variables,
            ) {
                body = resolved_body;
            }
        }

        // native desktop notification
        if settings.get("nativeNotificationsEnabled").and_then(|v| v.as_bool()) == Some(true) {
            let notification_payload = json!({
                "title": title,
                "body": body,
                "tag": format!("ready-{}", session_id),
                "kind": "ready",
                "sessionId": session_id,
                "directory": directory,
                "requireHidden": notification_mode != Some("always"),
            });
            let desktop_delivered = self.emitter.emit_desktop_notification(&notification_payload);
            self.emitter
                .broadcast_ui_notification(&notification_payload, desktop_delivered);
        }

        // fanout push
        let push_payload = json!({
            "title": title,
            "body": body,
            "tag": format!("ready-{}", session_id),
            "data": {
                "url": format!("/?session={}", url_encode_path(session_id)),
                "sessionId": session_id,
                "sessionName": session_name,
                "type": "ready",
            }
        });
        self.fanout_push(push_payload, PushSendOptions { require_no_sse: true })
            .await;
    }

    /// 处理 error 通知。
    async fn handle_error_notification(
        &self,
        payload: &Value,
        session_id: &str,
        directory: Option<&str>,
        _info: &Value,
    ) {
        let settings = crate::github::settings::read_settings();

        if settings.get("notifyOnError").and_then(|v| v.as_bool()) == Some(false) {
            return;
        }

        // cooldown
        let now = now_millis();
        {
            let last_error = self.last_error.lock().unwrap();
            if let Some(&last_at) = last_error.get(session_id) {
                if now - last_at < PUSH_READY_COOLDOWN_MS {
                    return;
                }
            }
        }
        self.last_error.lock().unwrap().insert(session_id.to_string(), now);

        let notification_mode = settings.get("notificationMode").and_then(|v| v.as_str());

        let mut title = "Tool error".to_string();
        let mut body = "An error occurred".to_string();
        let mut session_name = String::new();

        let mut variables = self
            .template
            .build_template_variables(payload, session_id, &self.opencode_base_url, &self.auth_header)
            .await;
        session_name.clone_from(&variables.session_name);

        let last_message = NotificationTemplateRuntime::extract_last_message_text(payload);
        variables.last_message = prepare_notification_last_message(
            Some(&last_message),
            settings.get("maxLastMessageLength").and_then(|v| v.as_i64()),
        );

        if let Some(templates) = settings.get("notificationTemplates").and_then(|v| v.as_object()) {
            let error_template = templates.get("error").cloned().unwrap_or_else(|| {
                json!({ "title": "Tool error", "message": "{last_message}" })
            });
            let resolved_title = NotificationTemplateRuntime::resolve_template(
                error_template
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Tool error"),
                &variables,
            );
            let resolved_body = NotificationTemplateRuntime::resolve_template(
                error_template
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("{last_message}"),
                &variables,
            );
            if !resolved_title.is_empty() {
                title = resolved_title;
            }
            if NotificationTemplateRuntime::should_apply_resolved_message(
                error_template.get("message").and_then(|v| v.as_str()).unwrap_or(""),
                &resolved_body,
                &variables,
            ) {
                body = resolved_body;
            }
        }

        if settings.get("nativeNotificationsEnabled").and_then(|v| v.as_bool()) == Some(true) {
            let notification_payload = json!({
                "title": title,
                "body": body,
                "tag": format!("error-{}", session_id),
                "kind": "error",
                "sessionId": session_id,
                "directory": directory,
                "requireHidden": notification_mode != Some("always"),
            });
            let desktop_delivered = self.emitter.emit_desktop_notification(&notification_payload);
            self.emitter
                .broadcast_ui_notification(&notification_payload, desktop_delivered);
        }

        let push_payload = json!({
            "title": title,
            "body": body,
            "tag": format!("error-{}", session_id),
            "data": {
                "url": format!("/?session={}", url_encode_path(session_id)),
                "sessionId": session_id,
                "sessionName": session_name,
                "type": "error",
            }
        });
        self.fanout_push(push_payload, PushSendOptions { require_no_sse: true })
            .await;
    }

    /// 处理 permission.replied (取消 pending debounce)。
    fn handle_permission_replied(&self, payload: &Value, session_id: &str) {
        let request_id = payload
            .get("properties")
            .and_then(|p| {
                p.get("requestID")
                    .or_else(|| p.get("requestId"))
                    .or_else(|| p.get("id"))
            })
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let request_key = request_id.map(|rid| format!("{}:{}", session_id, rid));

        let mut timers = self.permission_timers.lock().unwrap();
        if let Some((handle, pending_rk)) = timers.get(session_id) {
            if request_key.is_none()
                || pending_rk.is_none()
                || *pending_rk == request_key
            {
                handle.abort();
                timers.remove(session_id);
            }
        }
    }

    /// 处理 permission.asked (debounce 500ms + auto-accept suppression) — Arc<Self> 版本。
    ///
    /// 对应 Node `handlePermissionAsked`。
    pub async fn handle_permission_asked_arc(
        self: Arc<Self>,
        payload: Value,
        session_id: String,
        directory: Option<String>,
    ) {
        let request_id = payload
            .get("properties")
            .and_then(|p| {
                p.get("id")
                    .or_else(|| p.get("requestID"))
                    .or_else(|| p.get("requestId"))
            })
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let request_key = request_id
            .as_ref()
            .map(|rid| format!("{}:{}", session_id, rid));

        // 已通知过 → 跳过
        if let Some(ref rk) = request_key {
            if self.notified_permissions.lock().unwrap().contains(rk) {
                return;
            }
        }

        // auto-accept 检查
        if self.is_session_auto_accepting(&session_id, directory.as_deref()).await {
            if let Some(ref rk) = request_key {
                self.notified_permissions.lock().unwrap().insert(rk.clone());
            }
            return;
        }

        // 取消已有 timer
        {
            let mut timers = self.permission_timers.lock().unwrap();
            if let Some((handle, _)) = timers.remove(&session_id) {
                handle.abort();
            }
        }

        let self_clone = self.clone();
        let sid = session_id.clone();
        let rk_clone = request_key.clone();

        let handle = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(PUSH_PERMISSION_DEBOUNCE_MS)).await;

            self_clone.permission_timers.lock().unwrap().remove(&sid);

            let settings = crate::github::settings::read_settings();
            if settings.get("notifyOnPermission").and_then(|v| v.as_bool()) == Some(false) {
                return;
            }

            let notification_mode = settings.get("notificationMode").and_then(|v| v.as_str());

            let permission = payload
                .get("properties")
                .and_then(|p| p.get("permission"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let tool_name = payload
                .get("properties")
                .and_then(|p| p.get("tool"))
                .and_then(|v| v.as_str())
                .unwrap_or("");

            let mut title = if permission == "write" || permission == "execute" {
                "Permission needed".to_string()
            } else {
                "Approval needed".to_string()
            };
            let mut body = if !tool_name.is_empty() {
                format!("Agent wants to run {}", tool_name)
            } else {
                "Agent needs your approval".to_string()
            };
            let mut session_name = String::new();

            let mut variables = self_clone
                .template
                .build_template_variables(
                    &payload,
                    &sid,
                    &self_clone.opencode_base_url,
                    &self_clone.auth_header,
                )
                .await;
            session_name.clone_from(&variables.session_name);
            variables.last_message = if !tool_name.is_empty() {
                tool_name.to_string()
            } else {
                permission.to_string()
            };

            let templates = settings.get("notificationTemplates").and_then(|v| v.as_object());
            if let Some(templates) = templates {
                let p_template = templates.get("permission").cloned().unwrap_or_else(|| {
                    json!({ "title": "Permission needed", "message": "{last_message}" })
                });
                let resolved_title = NotificationTemplateRuntime::resolve_template(
                    p_template
                        .get("title")
                        .and_then(|v| v.as_str())
                        .unwrap_or("Permission needed"),
                    &variables,
                );
                let resolved_body = NotificationTemplateRuntime::resolve_template(
                    p_template
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("{last_message}"),
                    &variables,
                );
                if !resolved_title.is_empty() {
                    title = resolved_title;
                }
                if NotificationTemplateRuntime::should_apply_resolved_message(
                    p_template
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or(""),
                    &resolved_body,
                    &variables,
                ) {
                    body = resolved_body;
                }
            }

            // 标记为已通知
            if let Some(ref rk) = rk_clone {
                self_clone.notified_permissions.lock().unwrap().insert(rk.clone());
            }

            if settings.get("nativeNotificationsEnabled").and_then(|v| v.as_bool()) == Some(true) {
                let notification_payload = json!({
                    "kind": "permission",
                    "title": title,
                    "body": body,
                    "tag": format!("permission-{}", sid),
                    "sessionId": sid,
                    "directory": directory,
                    "requireHidden": notification_mode != Some("always"),
                });
                let desktop_delivered =
                    self_clone.emitter.emit_desktop_notification(&notification_payload);
                self_clone
                    .emitter
                    .broadcast_ui_notification(&notification_payload, desktop_delivered);
            }

            let push_payload = json!({
                "title": title,
                "body": body,
                "tag": format!("permission-{}", sid),
                "data": {
                    "url": format!("/?session={}", url_encode_path(&sid)),
                    "sessionId": sid,
                    "sessionName": session_name,
                    "type": "permission",
                }
            });
            self_clone
                .fanout_push(push_payload, PushSendOptions { require_no_sse: true })
                .await;
        });

        self.permission_timers.lock().unwrap().insert(
            session_id,
            (handle, request_key),
        );
    }
}

// 这些方法在 Arc<Self> 上下文实现, 用于 debounce timer spawn。
impl NotificationTrigger {
    /// 处理 question.asked (debounce 500ms) — Arc<Self> 版本。
    pub fn handle_question_asked_arc(
        self: Arc<Self>,
        payload: Value,
        session_id: String,
        directory: Option<String>,
    ) {
        // 取消已有 timer
        let mut timers = self.question_timers.lock().unwrap();
        if let Some(handle) = timers.remove(&session_id) {
            handle.abort();
        }
        drop(timers);

        let self_clone = self.clone();
        let sid = session_id.clone();

        let handle = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(PUSH_QUESTION_DEBOUNCE_MS)).await;

            self_clone.question_timers.lock().unwrap().remove(&sid);

            let settings = crate::github::settings::read_settings();
            if settings.get("notifyOnQuestion").and_then(|v| v.as_bool()) == Some(false) {
                return;
            }

            let notification_mode = settings.get("notificationMode").and_then(|v| v.as_str());

            let first_question = payload
                .get("properties")
                .and_then(|p| p.get("questions"))
                .and_then(|q| q.as_array())
                .and_then(|arr| arr.first());
            let header = first_question
                .and_then(|q| q.get("header"))
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .unwrap_or_default();
            let question_text = first_question
                .and_then(|q| q.get("question"))
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .unwrap_or_default();

            let mut title = if re_plan_mode().is_match(&header) {
                "Switch to plan mode".to_string()
            } else if re_build_agent().is_match(&header) {
                "Switch to build mode".to_string()
            } else if !header.is_empty() {
                header.clone()
            } else {
                "Input needed".to_string()
            };
            let mut body = if !question_text.is_empty() {
                question_text.clone()
            } else {
                "Agent is waiting for your response".to_string()
            };
            let mut session_name = String::new();

            let mut variables = self_clone
                .template
                .build_template_variables(
                    &payload,
                    &sid,
                    &self_clone.opencode_base_url,
                    &self_clone.auth_header,
                )
                .await;
            session_name.clone_from(&variables.session_name);
            variables.last_message = if !question_text.is_empty() {
                question_text.clone()
            } else {
                header.clone()
            };

            let templates = settings.get("notificationTemplates").and_then(|v| v.as_object());
            if let Some(templates) = templates {
                let q_template = templates.get("question").cloned().unwrap_or_else(|| {
                    json!({ "title": "Input needed", "message": "{last_message}" })
                });
                let resolved_title = NotificationTemplateRuntime::resolve_template(
                    q_template.get("title").and_then(|v| v.as_str()).unwrap_or("Input needed"),
                    &variables,
                );
                let resolved_body = NotificationTemplateRuntime::resolve_template(
                    q_template.get("message").and_then(|v| v.as_str()).unwrap_or("{last_message}"),
                    &variables,
                );
                if !resolved_title.is_empty() {
                    title = resolved_title;
                }
                if NotificationTemplateRuntime::should_apply_resolved_message(
                    q_template.get("message").and_then(|v| v.as_str()).unwrap_or(""),
                    &resolved_body,
                    &variables,
                ) {
                    body = resolved_body;
                }
            }

            if settings.get("nativeNotificationsEnabled").and_then(|v| v.as_bool()) == Some(true) {
                let notification_payload = json!({
                    "kind": "question",
                    "title": title,
                    "body": body,
                    "tag": format!("question-{}", sid),
                    "sessionId": sid,
                    "directory": directory,
                    "requireHidden": notification_mode != Some("always"),
                });
                let desktop_delivered =
                    self_clone.emitter.emit_desktop_notification(&notification_payload);
                self_clone
                    .emitter
                    .broadcast_ui_notification(&notification_payload, desktop_delivered);
            }

            let push_payload = json!({
                "title": title,
                "body": body,
                "tag": format!("question-{}", sid),
                "data": {
                    "url": format!("/?session={}", url_encode_path(&sid)),
                    "sessionId": sid,
                    "sessionName": session_name,
                    "type": "question",
                }
            });
            self_clone
                .fanout_push(push_payload, PushSendOptions { require_no_sse: true })
                .await;
        });

        self.question_timers.lock().unwrap().insert(session_id, handle);
    }
}

/// 简单的 URL path segment 编码 (session ID 通常已经是安全的)。
fn url_encode_path(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' || c == '~' {
                c.to_string()
            } else {
                format!("%{:02X}", c as u8)
            }
        })
        .collect()
}
