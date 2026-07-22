//! Permission auto-accept — 持久化策略 + session lineage 解析 + 自动回复 + reconcile。
//!
//! 对应 Node `permission-auto-accept/runtime.js` (264 行)。
//!
//! 路由:
//!  1. GET  /api/permission-auto-accept
//!  2. PUT  /api/permission-auto-accept/sessions/{sessionId}
//!
//! 运行时行为 (非路由):
//! - 订阅 GlobalHub 事件: `session.created/updated` → rememberSession;
//!   `permission.asked` → processPermission (去重 + retry + auto-reply)
//! - 订阅 GlobalHub 状态: `connect` → reconcilePending (补全离线期间的 pending)
//! - session lineage: 向上遍历 parentID 链找最近显式策略; 缺失时 GET /session/{id} 补全

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use crate::github::settings::{read_settings, write_settings};
use crate::notifications::emitter::NotificationEmitter;
use crate::realtime::global_hub::{GlobalHub, HubStatus};
use crate::state::AppState;

/// settings.json 中的 key。
const SETTINGS_KEY: &str = "permissionAutoAccept";

/// retry 延迟序列 (毫秒): 立即 → 250ms → 1000ms。
const RETRY_DELAYS_MS: &[u64] = &[0, 250, 1000];

/// OpenCode API 请求超时。
const REQUEST_TIMEOUT_MS: u64 = 5000;

/// session 缓存上限。
const SESSION_CACHE_LIMIT: usize = 10_000;

/// Policy 规范化结果: `{ sessions: {id: enabled} }`。
#[derive(Clone, Debug, Default)]
pub struct Policy {
    pub sessions: HashMap<String, bool>,
}

impl Policy {
    fn snapshot(&self) -> Value {
        let mut map = serde_json::Map::new();
        for (id, enabled) in &self.sessions {
            map.insert(id.clone(), Value::Bool(*enabled));
        }
        json!({ "sessions": Value::Object(map) })
    }
}

/// 从 settings value 规范化 policy。
fn normalize_policy(value: &Value) -> Policy {
    let mut sessions = HashMap::new();
    if let Some(obj) = value.as_object() {
        if let Some(sessions_val) = obj.get("sessions") {
            if let Some(sessions_obj) = sessions_val.as_object() {
                for (session_id, enabled) in sessions_obj {
                    if let Some(b) = enabled.as_bool() {
                        sessions.insert(session_id.clone(), b);
                    }
                }
            }
        }
    }
    Policy { sessions }
}

/// session lineage 缓存条目。
#[derive(Clone, Debug)]
struct SessionInfo {
    parent_id: Option<String>,
    directory: Option<String>,
}

/// Permission auto-accept 运行时。
pub struct PermissionAutoAcceptRuntime {
    /// 当前策略 (内存)。
    policy: std::sync::Mutex<Policy>,
    /// 是否已从 settings.json 加载。
    loaded: std::sync::atomic::AtomicBool,
    /// session lineage 缓存。
    sessions: std::sync::Mutex<HashMap<String, SessionInfo>>,
    /// permission reply 去重 (permission_id → 是否处理中)。
    in_flight: tokio::sync::Mutex<HashMap<String, bool>>,
}

impl PermissionAutoAcceptRuntime {
    pub fn new() -> Self {
        Self {
            policy: std::sync::Mutex::new(Policy::default()),
            loaded: std::sync::atomic::AtomicBool::new(false),
            sessions: std::sync::Mutex::new(HashMap::new()),
            in_flight: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    /// 从 settings.json 加载 policy (仅首次)。
    fn ensure_loaded(&self) {
        use std::sync::atomic::Ordering;
        if self.loaded.load(Ordering::Acquire) {
            return;
        }
        let settings = read_settings();
        let policy = normalize_policy(settings.get(SETTINGS_KEY).unwrap_or(&Value::Null));
        *self.policy.lock().unwrap() = policy;
        self.loaded.store(true, Ordering::Release);
    }

    /// 返回当前 policy 快照。
    pub fn load(&self) -> Value {
        self.ensure_loaded();
        self.policy.lock().unwrap().snapshot()
    }

    /// 强制从 settings 重新加载 (用于测试)。
    #[cfg(test)]
    #[allow(dead_code)]
    fn reload(&self) {
        let settings = read_settings();
        let policy = normalize_policy(settings.get(SETTINGS_KEY).unwrap_or(&Value::Null));
        *self.policy.lock().unwrap() = policy;
        use std::sync::atomic::Ordering;
        self.loaded.store(true, Ordering::Release);
    }

    /// 设置单个 session 的 auto-accept 策略。
    ///
    /// 1. 更新 settings.json (原子写)
    /// 2. 更新内存 policy
    /// 3. 如果 enabled=true, 触发 reconcile (补全已有 pending)
    /// 4. 广播 `gridforge:permission-auto-accept.updated` SSE 事件
    pub async fn set_session_policy(
        self: Arc<Self>,
        session_id: &str,
        enabled: bool,
        directory: Option<&str>,
        opencode_base_url: &str,
        opencode_auth_header: &str,
        emitter: &NotificationEmitter,
    ) -> Result<Value, String> {
        let normalized = session_id.trim();
        if normalized.is_empty() {
            return Err("sessionId is required".to_string());
        }

        self.ensure_loaded();

        // 更新 settings.json
        let mut settings = read_settings();
        if let Some(obj) = settings.as_object_mut() {
            // 确保 permissionAutoAccept.sessions 存在
            if !obj.contains_key(SETTINGS_KEY) {
                obj.insert(
                    SETTINGS_KEY.to_string(),
                    json!({ "sessions": {} }),
                );
            }
            if let Some(paa) = obj.get_mut(SETTINGS_KEY).and_then(|v| v.as_object_mut()) {
                if !paa.contains_key("sessions") {
                    paa.insert("sessions".to_string(), json!({}));
                }
                if let Some(sessions) = paa.get_mut("sessions").and_then(|v| v.as_object_mut()) {
                    sessions.insert(normalized.to_string(), Value::Bool(enabled));
                }
            }
        }
        write_settings(&settings).map_err(|e| e.to_string())?;

        // 更新内存 policy
        {
            let mut policy = self.policy.lock().unwrap();
            policy.sessions.insert(normalized.to_string(), enabled);
        }

        // 广播 UI 事件
        let snapshot = self.policy.lock().unwrap().snapshot();
        // 事件类型: gridforge:permission-auto-accept.updated
        // (snapshot 直接喂给 broadcast_ui_notification; 标记 desktop=false 避免触发原生通知)
        emitter.broadcast_ui_notification(&snapshot, false);

        // enabled=true → reconcile pending
        if enabled {
            let dirs = directory.map(|d| vec![d.to_string()]).unwrap_or_default();
            self.clone()
                .reconcile_pending(&dirs, opencode_base_url, opencode_auth_header)
                .await;
        }

        Ok(snapshot)
    }

    /// 检查 session 是否 auto-accept。
    ///
    /// 向上遍历 parentID 链, 找到第一个显式策略即返回; 缺失 parentID 时
    /// 通过 GET /session/{id} 补全 lineage。
    pub async fn is_session_auto_accepting(
        &self,
        session_id: &str,
        directory: Option<&str>,
        opencode_base_url: &str,
        opencode_auth_header: &str,
    ) -> bool {
        self.ensure_loaded();

        let mut seen = HashSet::new();
        let mut current = session_id.to_string();
        let mut current_dir = directory.map(|d| d.to_string());

        loop {
            if current.is_empty() || seen.contains(&current) {
                return false;
            }
            seen.insert(current.clone());

            // 检查显式策略 (scoped guard, drops before any await)
            let explicit_enabled = {
                let policy = self.policy.lock().unwrap();
                policy.sessions.get(&current).copied()
            };
            if let Some(enabled) = explicit_enabled {
                return enabled;
            }

            // 查 lineage 缓存
            let cached = self.sessions.lock().unwrap().get(&current).cloned();
            let info = match cached {
                Some(info) => info,
                None => {
                    // GET /session/{id} 补全
                    match self
                        .fetch_session_info(&current, current_dir.as_deref(), opencode_base_url, opencode_auth_header)
                        .await
                    {
                        Some(info) => info,
                        None => return false,
                    }
                }
            };

            current = info.parent_id.unwrap_or_default();
            current_dir = info.directory.or(current_dir);
        }
    }

    /// 从 OpenCode GET /session/{id} 获取 session info (parentID, directory)。
    async fn fetch_session_info(
        &self,
        session_id: &str,
        directory: Option<&str>,
        opencode_base_url: &str,
        opencode_auth_header: &str,
    ) -> Option<SessionInfo> {
        let base = opencode_base_url.trim_end_matches('/');
        let url = format!("{base}/session/{session_id}");

        let client = reqwest::Client::new();
        let mut req = client
            .get(&url)
            .header("accept", "application/json")
            .header("authorization", opencode_auth_header)
            .timeout(std::time::Duration::from_millis(REQUEST_TIMEOUT_MS));

        if let Some(dir) = directory {
            if !dir.is_empty() {
                req = req.query(&[("directory", dir)]);
            }
        }

        let result = req.send().await;

        let resp = match result {
            Ok(r) if r.status().is_success() => r,
            _ => return None,
        };

        let data: Value = resp.json().await.ok()?;
        let info_obj = data.get("data").unwrap_or(&data);

        let session_info = SessionInfo {
            parent_id: info_obj
                .get("parentID")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string()),
            directory: info_obj
                .get("directory")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string()),
        };

        // 缓存
        {
            let mut sessions = self.sessions.lock().unwrap();
            if sessions.len() >= SESSION_CACHE_LIMIT {
                // 淘汰第一个 (FIFO 近似)
                if let Some(first_key) = sessions.keys().next().cloned() {
                    sessions.remove(&first_key);
                }
            }
            sessions.insert(session_id.to_string(), session_info.clone());
        }

        Some(session_info)
    }

    /// 记住 session info (从 SSE 事件中提取)。
    fn remember_session(&self, info: &Value, directory_hint: Option<&str>) {
        let id = info.get("id").and_then(|v| v.as_str());
        let id = match id {
            Some(id) if !id.is_empty() => id,
            _ => return,
        };

        let parent_id = info
            .get("parentID")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());

        let directory = info
            .get("directory")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .or_else(|| directory_hint.map(|d| d.to_string()));

        let session_info = SessionInfo { parent_id, directory };

        let mut sessions = self.sessions.lock().unwrap();
        if sessions.len() >= SESSION_CACHE_LIMIT {
            if let Some(first_key) = sessions.keys().next().cloned() {
                sessions.remove(&first_key);
            }
        }
        sessions.insert(id.to_string(), session_info);
    }

    /// 处理单个 permission: 去重 + retry + auto-reply。
    ///
    /// 返回 true 如果已 auto-reply 或 session 非 auto-accept。
    pub async fn process_permission(
        self: Arc<Self>,
        permission: &Value,
        directory: Option<&str>,
        opencode_base_url: &str,
        opencode_auth_header: &str,
    ) -> bool {
        let permission_id = permission.get("id").and_then(|v| v.as_str());
        let session_id = permission.get("sessionID").and_then(|v| v.as_str());
        let (permission_id, session_id) = match (permission_id, session_id) {
            (Some(pid), Some(sid)) if !pid.is_empty() && !sid.is_empty() => (pid, sid),
            _ => return false,
        };

        // 去重: 检查是否已有相同 permission_id 在处理
        {
            let mut in_flight = self.in_flight.lock().await;
            if in_flight.contains_key(permission_id) {
                return false;
            }
            in_flight.insert(permission_id.to_string(), true);
        }

        let result = self
            .clone()
            .reply_with_retry(
                permission_id,
                session_id,
                directory,
                opencode_base_url,
                opencode_auth_header,
            )
            .await;

        // 清除 in_flight
        self.in_flight.lock().await.remove(permission_id);

        result
    }

    /// 带重试的 permission reply。
    ///
    /// retry 序列 [0, 250, 1000ms]。404 → 返回 true (已处理)。
    async fn reply_with_retry(
        self: Arc<Self>,
        permission_id: &str,
        session_id: &str,
        directory: Option<&str>,
        opencode_base_url: &str,
        opencode_auth_header: &str,
    ) -> bool {
        for (attempt, &delay) in RETRY_DELAYS_MS.iter().enumerate() {
            if delay > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
            }

            // 检查 session 是否 auto-accept
            let auto = self
                .clone()
                .is_session_auto_accepting(session_id, directory, opencode_base_url, opencode_auth_header)
                .await;
            if !auto {
                return false;
            }

            // POST /permission/{id}/reply { reply: "once" }
            let result = self
                .clone()
                .post_permission_reply(permission_id, directory, opencode_base_url, opencode_auth_header)
                .await;

            match result {
                Ok(()) => return true,
                Err(status) => {
                    if status == 404 {
                        // permission 已不存在 → 视为已处理
                        return true;
                    }
                    // 非 404 错误 → 如果还有 retry 额度则重试
                    if attempt == RETRY_DELAYS_MS.len() - 1 {
                        return false;
                    }
                }
            }
        }
        false
    }

    /// POST /permission/{id}/reply。
    ///
    /// Ok(()) = 成功; Err(status_code) = HTTP 错误。
    async fn post_permission_reply(
        &self,
        permission_id: &str,
        directory: Option<&str>,
        opencode_base_url: &str,
        opencode_auth_header: &str,
    ) -> Result<(), u16> {
        let base = opencode_base_url.trim_end_matches('/');
        let url = format!("{base}/permission/{permission_id}/reply");

        let client = reqwest::Client::new();
        let mut req = client
            .post(&url)
            .header("accept", "application/json")
            .header("content-type", "application/json")
            .header("authorization", opencode_auth_header)
            .timeout(std::time::Duration::from_millis(REQUEST_TIMEOUT_MS))
            .body(json!({ "reply": "once" }).to_string());

        if let Some(dir) = directory {
            if !dir.is_empty() {
                req = req.query(&[("directory", dir)]);
            }
        }

        let result = req.send().await;

        match result {
            Ok(r) if r.status().is_success() => Ok(()),
            Ok(r) => Err(r.status().as_u16()),
            Err(_) => Err(500),
        }
    }

    /// Reconcile pending permissions: GET /permission 收集 pending → 对 auto-accept 的自动 reply。
    pub async fn reconcile_pending(
        self: Arc<Self>,
        directories: &[String],
        opencode_base_url: &str,
        opencode_auth_header: &str,
    ) {
        self.ensure_loaded();

        // scopes = [undefined, ...directories]
        let mut scopes: Vec<Option<&str>> = vec![None];
        for dir in directories {
            let trimmed = dir.trim();
            if !trimmed.is_empty() {
                scopes.push(Some(trimmed));
            }
        }

        // 收集 pending permissions (dedup by ID)
        let mut pending_by_id: HashMap<String, (Value, Option<String>)> = HashMap::new();

        for scope in &scopes {
            let permissions = match self
                .fetch_pending_permissions(*scope, opencode_base_url, opencode_auth_header)
                .await
            {
                Some(perms) => perms,
                None => continue,
            };

            for permission in perms_into_vec(permissions) {
                if let Some(id) = permission.get("id").and_then(|v| v.as_str()) {
                    if id.is_empty() {
                        continue;
                    }
                    let dir = permission
                        .get("directory")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                        .or_else(|| scope.map(|s| s.to_string()));
                    pending_by_id.insert(id.to_string(), (permission, dir));
                }
            }
        }

        // 对每个 pending permission 处理
        for (_id, (permission, dir)) in pending_by_id {
            self.clone()
                .process_permission(&permission, dir.as_deref(), opencode_base_url, opencode_auth_header)
                .await;
        }
    }

    /// GET /permission (optionally scoped by directory)。
    async fn fetch_pending_permissions(
        &self,
        directory: Option<&str>,
        opencode_base_url: &str,
        opencode_auth_header: &str,
    ) -> Option<Value> {
        let base = opencode_base_url.trim_end_matches('/');
        let url = format!("{base}/permission");

        let client = reqwest::Client::new();
        let mut req = client
            .get(&url)
            .header("accept", "application/json")
            .header("authorization", opencode_auth_header)
            .timeout(std::time::Duration::from_millis(REQUEST_TIMEOUT_MS));

        if let Some(dir) = directory {
            if !dir.is_empty() {
                req = req.query(&[("directory", dir)]);
            }
        }

        let result = req.send().await;

        let resp = match result {
            Ok(r) if r.status().is_success() => r,
            _ => return None,
        };

        resp.json().await.ok()
    }

    /// 处理 GlobalHub 事件。
    ///
    /// `session.created/updated` → rememberSession
    /// `permission.asked` → processPermission (fire-and-forget)
    pub fn process_event(
        self: Arc<Self>,
        event: &Value,
        opencode_base_url: &str,
        opencode_auth_header: &str,
    ) {
        let raw = event;
        // 事件可能是 { payload: { type, properties } } 或直接 { type, properties }
        let payload = raw
            .get("payload")
            .filter(|v| v.is_object())
            .unwrap_or(raw);

        let event_type = payload.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let directory = event
            .get("directory")
            .and_then(|v| v.as_str())
            .filter(|d| !d.is_empty() && *d != "global");

        match event_type {
            "session.created" | "session.updated" => {
                if let Some(info) = payload.get("properties").and_then(|p| p.get("info")) {
                    self.remember_session(info, directory);
                }
            }
            "permission.asked" => {
                if let Some(permission) = payload.get("properties") {
                    let rt = self.clone();
                    let dir = directory.map(|d| d.to_string());
                    let base = opencode_base_url.to_string();
                    let auth = opencode_auth_header.to_string();
                    let perm = permission.clone();
                    tokio::spawn(async move {
                        rt.process_permission(&perm, dir.as_deref(), &base, &auth)
                            .await;
                    });
                }
            }
            _ => {}
        }
    }

    /// 启动 GlobalHub 消费者 (后台 task)。
    ///
    /// 订阅事件 + 状态:
    /// - event: `session.created/updated` → rememberSession; `permission.asked` → processPermission
    /// - status: `connect` → reconcilePending
    pub fn start(
        self: Arc<Self>,
        global_hub: &GlobalHub,
        opencode_base_url: &str,
        opencode_auth_header: &str,
    ) {
        let mut event_rx = global_hub.subscribe_event();
        let mut status_rx = global_hub.subscribe_status();
        let rt = self.clone();
        let base = opencode_base_url.to_string();
        let auth = opencode_auth_header.to_string();

        tokio::spawn(async move {
            tracing::info!("[permission-auto-accept] consumer started");
            loop {
                tokio::select! {
                    ev = event_rx.recv() => {
                        match ev {
                            Ok(event) => {
                                rt.clone().process_event(&event.payload, &base, &auth);
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                tracing::warn!(skipped = n, "[permission-auto-accept] event consumer lagged");
                                continue;
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                tracing::info!("[permission-auto-accept] event consumer stopped");
                                break;
                            }
                        }
                    }
                    st = status_rx.recv() => {
                        match st {
                            Ok(HubStatus::Connect { .. }) => {
                                let rt2 = rt.clone();
                                let base2 = base.clone();
                                let auth2 = auth.clone();
                                tokio::spawn(async move {
                                    rt2.reconcile_pending(&[], &base2, &auth2).await;
                                });
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                tracing::warn!(skipped = n, "[permission-auto-accept] status consumer lagged");
                                continue;
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                tracing::info!("[permission-auto-accept] status consumer stopped");
                                break;
                            }
                            _ => {}
                        }
                    }
                }
            }
        });
    }
}

impl Default for PermissionAutoAcceptRuntime {
    fn default() -> Self {
        Self::new()
    }
}

/// 将 permissions 响应 (可能是数组或 `{data:[...]}`) 转为 Vec。
fn perms_into_vec(value: Value) -> Vec<Value> {
    if let Some(arr) = value.as_array() {
        return arr.clone();
    }
    if let Some(data) = value.get("data") {
        if let Some(arr) = data.as_array() {
            return arr.clone();
        }
    }
    Vec::new()
}

// ============================================================
// axum handlers
// ============================================================

/// GET /api/permission-auto-accept
pub async fn get_permission_auto_accept(
    State(state): State<Arc<AppState>>,
) -> Response {
    let snapshot = state.permission_auto_accept.load();
    Json(snapshot).into_response()
}

/// PUT /api/permission-auto-accept/sessions/{sessionId}
pub async fn put_session_policy(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let enabled = body.get("enabled").and_then(|v| v.as_bool());
    let directory = body.get("directory").and_then(|v| v.as_str());

    let enabled = match enabled {
        Some(e) => e,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "enabled must be a boolean" })),
            )
                .into_response();
        }
    };

    match state
        .permission_auto_accept
        .clone()
        .set_session_policy(
            &session_id,
            enabled,
            directory,
            &state.opencode_base_url,
            &state.opencode_auth_header,
            &state.emitter,
        )
        .await
    {
        Ok(snapshot) => Json(snapshot).into_response(),
        Err(message) => {
            let status = if message.contains("sessionId is required") {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            (status, Json(json!({ "error": message }))).into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_policy_filters_non_boolean() {
        let raw = json!({
            "sessions": {
                "root": true,
                "child": false,
                "invalid": "not-a-bool",
                "also-invalid": 42,
            }
        });
        let policy = normalize_policy(&raw);
        assert_eq!(policy.sessions.len(), 2);
        assert_eq!(policy.sessions.get("root"), Some(&true));
        assert_eq!(policy.sessions.get("child"), Some(&false));
    }

    #[test]
    fn normalize_policy_empty() {
        let policy = normalize_policy(&Value::Null);
        assert!(policy.sessions.is_empty());

        let policy = normalize_policy(&json!({}));
        assert!(policy.sessions.is_empty());

        let policy = normalize_policy(&json!({ "sessions": "not-object" }));
        assert!(policy.sessions.is_empty());
    }

    #[test]
    fn policy_snapshot_roundtrip() {
        let mut policy = Policy::default();
        policy.sessions.insert("a".to_string(), true);
        policy.sessions.insert("b".to_string(), false);

        let snap = policy.snapshot();
        assert_eq!(snap["sessions"]["a"], true);
        assert_eq!(snap["sessions"]["b"], false);
    }

    #[test]
    fn remember_session_extracts_parent_and_directory() {
        let rt = PermissionAutoAcceptRuntime::new();
        let info = json!({
            "id": "child",
            "parentID": "root",
            "directory": "/project"
        });
        rt.remember_session(&info, Some("/hint"));

        let sessions = rt.sessions.lock().unwrap();
        let cached = sessions.get("child").unwrap();
        assert_eq!(cached.parent_id.as_deref(), Some("root"));
        assert_eq!(cached.directory.as_deref(), Some("/project"));
    }

    #[test]
    fn remember_session_uses_hint_when_directory_missing() {
        let rt = PermissionAutoAcceptRuntime::new();
        let info = json!({ "id": "s1", "parentID": "p1" });
        rt.remember_session(&info, Some("/hint-dir"));

        let sessions = rt.sessions.lock().unwrap();
        let cached = sessions.get("s1").unwrap();
        assert_eq!(cached.directory.as_deref(), Some("/hint-dir"));
    }

    #[test]
    fn remember_session_ignores_empty_id() {
        let rt = PermissionAutoAcceptRuntime::new();
        rt.remember_session(&json!({ "id": "" }), None);
        assert!(rt.sessions.lock().unwrap().is_empty());

        rt.remember_session(&json!({ "id": null }), None);
        assert!(rt.sessions.lock().unwrap().is_empty());
    }

    #[test]
    fn perms_into_vec_handles_array() {
        let v = json!([{ "id": "a" }, { "id": "b" }]);
        let result = perms_into_vec(v);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn perms_into_vec_handles_data_wrapper() {
        let v = json!({ "data": [{ "id": "a" }] });
        let result = perms_into_vec(v);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn perms_into_vec_handles_empty() {
        assert!(perms_into_vec(json!(null)).is_empty());
        assert!(perms_into_vec(json!({})).is_empty());
        assert!(perms_into_vec(json!({ "data": "not-array" })).is_empty());
    }

    #[test]
    fn process_event_extracts_session_created() {
        let rt = Arc::new(PermissionAutoAcceptRuntime::new());
        let event = json!({
            "type": "session.created",
            "properties": {
                "info": { "id": "s1", "parentID": "root" }
            }
        });

        rt.clone().process_event(&event, "http://localhost", "Basic abc");
        let sessions = rt.sessions.lock().unwrap();
        assert!(sessions.contains_key("s1"));
    }

    #[test]
    fn process_event_ignores_global_directory() {
        let rt = Arc::new(PermissionAutoAcceptRuntime::new());
        let event = json!({
            "directory": "global",
            "payload": {
                "type": "session.updated",
                "properties": { "info": { "id": "s2" } }
            }
        });

        rt.clone().process_event(&event, "http://localhost", "Basic abc");
        let sessions = rt.sessions.lock().unwrap();
        assert!(sessions.contains_key("s2"));
        // directory = "global" should be filtered out
        assert!(sessions.get("s2").unwrap().directory.is_none());
    }

    #[test]
    fn process_event_handles_nested_payload() {
        let rt = Arc::new(PermissionAutoAcceptRuntime::new());
        let event = json!({
            "payload": {
                "type": "session.created",
                "properties": { "info": { "id": "nested-sess" } }
            }
        });

        rt.clone().process_event(&event, "http://localhost", "Basic abc");
        let sessions = rt.sessions.lock().unwrap();
        assert!(sessions.contains_key("nested-sess"));
    }

    #[test]
    fn normalize_policy_preserves_ownership() {
        // Object.hasOwn semantics: only own properties, not inherited
        let raw = json!({
            "sessions": {
                "explicit-true": true,
                "explicit-false": false,
            }
        });
        let policy = normalize_policy(&raw);
        // 模拟 is_session_auto_accepting: policy.sessions.get("explicit-true")
        assert!(matches!(policy.sessions.get("explicit-true"), Some(true)));
        assert!(matches!(policy.sessions.get("explicit-false"), Some(false)));
        assert!(!policy.sessions.contains_key("nonexistent"));
    }
}
