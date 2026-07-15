//! 模板变量解析 + git branch + session info fetch。
//!
//! 对应 Node `notifications/template-runtime.js` (`createNotificationTemplateRuntime`)。
//!
//! 提供: 模板插值 `{key}`, 变量构建 (project/branch/session/agent/model),
//! 文本提取 (parts/content), session info fetch + cache。

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use regex::Regex;
use serde_json::Value;

use super::types::{format_model_id, format_mode, format_project_label};
use super::NOTIFICATION_BODY_MAX_CHARS;

use std::sync::OnceLock;

fn re_template_var() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"\{(\w+)\}").unwrap())
}

/// 模板变量集。
#[derive(Debug, Clone, Default)]
pub struct TemplateVars {
    pub project_name: String,
    pub worktree: String,
    pub branch: String,
    pub session_name: String,
    pub agent_name: String,
    pub model_name: String,
    pub last_message: String,
    pub session_id: String,
}

impl TemplateVars {
    /// 转为 `serde_json::Map` 用于插值。
    pub fn to_map(&self) -> serde_json::Map<String, Value> {
        let mut map = serde_json::Map::new();
        map.insert("project_name".to_string(), Value::String(self.project_name.clone()));
        map.insert("worktree".to_string(), Value::String(self.worktree.clone()));
        map.insert("branch".to_string(), Value::String(self.branch.clone()));
        map.insert("session_name".to_string(), Value::String(self.session_name.clone()));
        map.insert("agent_name".to_string(), Value::String(self.agent_name.clone()));
        map.insert("model_name".to_string(), Value::String(self.model_name.clone()));
        map.insert("last_message".to_string(), Value::String(self.last_message.clone()));
        map.insert("session_id".to_string(), Value::String(self.session_id.clone()));
        map
    }

    /// 获取变量值。
    pub fn get(&self, key: &str) -> Option<String> {
        match key {
            "project_name" => Some(self.project_name.clone()),
            "worktree" => Some(self.worktree.clone()),
            "branch" => Some(self.branch.clone()),
            "session_name" => Some(self.session_name.clone()),
            "agent_name" => Some(self.agent_name.clone()),
            "model_name" => Some(self.model_name.clone()),
            "last_message" => Some(self.last_message.clone()),
            "session_id" => Some(self.session_id.clone()),
            _ => None,
        }
    }

    /// 设置变量值。
    pub fn set(&mut self, key: &str, value: String) {
        match key {
            "project_name" => self.project_name = value,
            "worktree" => self.worktree = value,
            "branch" => self.branch = value,
            "session_name" => self.session_name = value,
            "agent_name" => self.agent_name = value,
            "model_name" => self.model_name = value,
            "last_message" => self.last_message = value,
            "session_id" => self.session_id = value,
            _ => {}
        }
    }
}

/// session info 缓存条目。
struct SessionInfoCacheEntry {
    data: Value,
    at: i64,
}

/// 通知模板运行时。
pub struct NotificationTemplateRuntime {
    /// session title 缓存 (无 TTL)。
    session_title_cache: Mutex<HashMap<String, String>>,
    /// session info 缓存 (TTL 60s)。
    session_info_cache: Mutex<HashMap<String, SessionInfoCacheEntry>>,
    http_client: reqwest::Client,
}

impl NotificationTemplateRuntime {
    pub fn new(http_client: reqwest::Client) -> Self {
        Self {
            session_title_cache: Mutex::new(HashMap::new()),
            session_info_cache: Mutex::new(HashMap::new()),
            http_client,
        }
    }

    /// 模板插值 `{key}`。
    ///
    /// 对应 Node `resolveNotificationTemplate`。
    pub fn resolve_template(template: &str, vars: &TemplateVars) -> String {
        re_template_var()
            .replace_all(template, |caps: &regex::Captures| {
                let key = &caps[1];
                vars.get(key).unwrap_or_default()
            })
            .to_string()
    }

    /// 判断是否应用解析后的 template message。
    ///
    /// 对应 Node `shouldApplyResolvedTemplateMessage`。
    pub fn should_apply_resolved_message(template: &str, resolved: &str, vars: &TemplateVars) -> bool {
        if resolved.is_empty() {
            return false;
        }
        if template.contains("{last_message}") {
            return !vars.last_message.trim().is_empty();
        }
        true
    }

    /// 缓存 session title。
    pub fn cache_session_title(&self, session_id: &str, title: &str) {
        if session_id.is_empty() || title.is_empty() {
            return;
        }
        self.session_title_cache
            .lock()
            .unwrap()
            .insert(session_id.to_string(), title.to_string());
    }

    /// 获取缓存的 session title。
    pub fn get_cached_session_title(&self, session_id: &str) -> Option<String> {
        self.session_title_cache
            .lock()
            .unwrap()
            .get(session_id)
            .cloned()
    }

    /// fetch session info (带 60s TTL 缓存)。
    ///
    /// 对应 Node `fetchSessionInfo`。
    pub async fn fetch_session_info(
        &self,
        session_id: &str,
        opencode_base_url: &str,
        auth_header: &str,
    ) -> Option<Value> {
        if session_id.is_empty() {
            return None;
        }

        let now = super::now_millis();
        {
            let cache = self.session_info_cache.lock().unwrap();
            if let Some(entry) = cache.get(session_id) {
                if now - entry.at < super::SESSION_PARENT_CACHE_TTL_MS {
                    return Some(entry.data.clone());
                }
            }
        }

        let url = format!(
            "{}/session/{}",
            opencode_base_url.trim_end_matches('/'),
            session_id // session IDs are hex/base64-safe; no encoding needed
        );

        let result = self
            .http_client
            .get(&url)
            .header("accept", "application/json")
            .header("authorization", auth_header)
            .timeout(std::time::Duration::from_millis(2000))
            .send()
            .await;

        let resp = match result {
            Ok(r) if r.status().is_success() => r,
            _ => return None,
        };

        let data: Value = resp.json().await.ok()?;
        if !data.is_object() {
            return None;
        }

        self.session_info_cache
            .lock()
            .unwrap()
            .insert(session_id.to_string(), SessionInfoCacheEntry {
                data: data.clone(),
                at: now,
            });

        Some(data)
    }

    /// 获取 session parentID (带缓存)。
    ///
    /// 对应 Node `fetchSessionParentId`。
    pub async fn fetch_session_parent_id(
        &self,
        session_id: &str,
        directory: Option<&str>,
        opencode_base_url: &str,
        auth_header: &str,
    ) -> Option<Option<String>> {
        if session_id.is_empty() {
            return None;
        }

        let info = self
            .fetch_session_info(session_id, opencode_base_url, auth_header)
            .await?;

        let _ = directory; // directory 影响 URL query 但 parentID 来自 session body
        let parent_id = info
            .get("parentID")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());

        Some(parent_id)
    }

    /// 构建 template 变量。
    ///
    /// 对应 Node `buildTemplateVariables`。
    pub async fn build_template_variables(
        &self,
        payload: &Value,
        session_id: &str,
        opencode_base_url: &str,
        auth_header: &str,
    ) -> TemplateVars {
        let info = payload
            .get("properties")
            .and_then(|p| p.get("info"))
            .cloned()
            .unwrap_or(Value::Null);
        let props = payload.get("properties");

        // session title
        let mut session_title = props
            .and_then(|p| {
                p.get("sessionTitle")
                    .and_then(|v| v.as_str())
                    .or_else(|| p.get("session").and_then(|s| s.get("title")).and_then(|v| v.as_str()))
                    .or_else(|| {
                        info.get("sessionTitle")
                            .and_then(|v| v.as_str())
                    })
            })
            .unwrap_or("")
            .to_string();

        if session_title.is_empty() && !session_id.is_empty() {
            if let Some(cached) = self.get_cached_session_title(session_id) {
                session_title = cached;
            }
        }

        if session_title.is_empty() && !session_id.is_empty() {
            if let Some(info_data) = self
                .fetch_session_info(session_id, opencode_base_url, auth_header)
                .await
            {
                if let Some(title) = info_data.get("title").and_then(|v| v.as_str()) {
                    session_title = title.trim().to_string();
                    if !session_title.is_empty() {
                        self.cache_session_title(session_id, &session_title);
                    }
                }
            }
        }

        // agent name
        let agent_name = {
            let mode = info
                .get("agent")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .or_else(|| {
                    info.get("mode")
                        .and_then(|v| v.as_str())
                        .map(|s| s.trim().to_string())
                })
                .unwrap_or_default();
            if mode.is_empty() {
                "Agent".to_string()
            } else {
                format_mode(&mode)
            }
        };

        // model name
        let model_name = {
            let raw = info
                .get("modelID")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .or_else(|| {
                    info.get("model")
                        .and_then(|m| m.get("modelID"))
                        .and_then(|v| v.as_str())
                        .map(|s| s.trim().to_string())
                })
                .unwrap_or_default();
            if raw.is_empty() {
                "Assistant".to_string()
            } else {
                format_model_id(&raw)
            }
        };

        // project name + worktree + branch
        let (project_name, worktree_dir, branch) = self
            .resolve_project_and_branch(&info)
            .await;

        TemplateVars {
            project_name,
            worktree: worktree_dir,
            branch,
            session_name: session_title,
            agent_name,
            model_name,
            last_message: String::new(),
            session_id: session_id.to_string(),
        }
    }

    /// 从 info.path + settings 解析 project name + worktree + git branch。
    async fn resolve_project_and_branch(
        &self,
        info: &Value,
    ) -> (String, String, String) {
        let info_path = info.get("path");
        let mut worktree_dir = String::new();

        if let Some(root) = info_path.and_then(|p| p.get("root")).and_then(|v| v.as_str()) {
            if !root.is_empty() {
                worktree_dir = root.to_string();
            }
        }
        if worktree_dir.is_empty() {
            if let Some(cwd) = info_path.and_then(|p| p.get("cwd")).and_then(|v| v.as_str()) {
                if !cwd.is_empty() {
                    worktree_dir = cwd.to_string();
                }
            }
        }

        let settings = crate::github::settings::read_settings();
        let projects = settings.get("projects").and_then(|v| v.as_array());
        let mut project_name = String::new();

        if !worktree_dir.is_empty() {
            let normalized_dir = worktree_dir.trim_end_matches('/').to_string();
            let matched = projects.and_then(|arr| {
                arr.iter().find_map(|p| {
                    let path = p.get("path").and_then(|v| v.as_str())?;
                    if path.trim_end_matches('/') == normalized_dir {
                        Some(p.clone())
                    } else {
                        None
                    }
                })
            });

            if let Some(ref proj) = matched {
                if let Some(label) = proj.get("label").and_then(|v| v.as_str()) {
                    let trimmed = label.trim();
                    if !trimmed.is_empty() {
                        project_name = trimmed.to_string();
                    }
                }
            }

            if project_name.is_empty() {
                project_name = normalized_dir
                    .split('/')
                    .rfind(|s| !s.is_empty())
                    .unwrap_or("")
                    .to_string();
            }
        } else {
            // 用 activeProjectId 或第一个 project
            let active_id = settings.get("activeProjectId").and_then(|v| v.as_str()).unwrap_or("");
            let active_project = if !active_id.is_empty() {
                projects.and_then(|arr| arr.iter().find(|p| p.get("id") == Some(&Value::String(active_id.to_string()))).cloned())
            } else {
                projects.and_then(|arr| arr.first().cloned())
            };

            if let Some(proj) = active_project {
                let label = proj.get("label").and_then(|v| v.as_str()).unwrap_or("");
                let trimmed = label.trim();
                if !trimmed.is_empty() {
                    project_name = trimmed.to_string();
                } else if let Some(path) = proj.get("path").and_then(|v| v.as_str()) {
                    project_name = path.split('/').rfind(|s| !s.is_empty()).unwrap_or("").to_string();
                    worktree_dir = path.to_string();
                }
            }
        }

        // git branch (spawn git, 3s timeout)
        let branch = if !worktree_dir.is_empty() {
            resolve_git_branch(&worktree_dir).await
        } else {
            String::new()
        };

        (
            format_project_label(&project_name),
            worktree_dir,
            branch,
        )
    }

    /// 从消息 parts 提取文本。
    ///
    /// 对应 Node `extractTextFromParts`。
    pub fn extract_text_from_parts(parts: Option<&Value>, max_length: usize) -> String {
        let arr = match parts.and_then(|v| v.as_array()) {
            Some(a) if !a.is_empty() => a,
            _ => return String::new(),
        };

        let mut text_parts: Vec<String> = Vec::new();
        for part in arr {
            let part_type = part.get("type").and_then(|v| v.as_str());
            if part_type != Some("text") {
                continue;
            }
            let text = part
                .get("text")
                .and_then(|v| v.as_str())
                .or_else(|| part.get("content").and_then(|v| v.as_str()))
                .unwrap_or("");
            if !text.is_empty() {
                text_parts.push(text.to_string());
            }
        }

        let mut text = text_parts.join("\n");
        let text_trimmed = text.trim().to_string();
        text = text_trimmed;

        if max_length > 0 && text.len() > max_length {
            text = text.chars().take(max_length).collect();
        }
        text
    }

    /// 从 payload 提取最后一条消息的文本。
    ///
    /// 对应 Node `extractLastMessageText`。
    pub fn extract_last_message_text(payload: &Value) -> String {
        let info = match payload.get("properties").and_then(|p| p.get("info")) {
            Some(i) => i,
            None => return String::new(),
        };

        let parts = info
            .get("parts")
            .or_else(|| payload.get("properties").and_then(|p| p.get("parts")));
        let text = Self::extract_text_from_parts(parts, NOTIFICATION_BODY_MAX_CHARS);
        if !text.is_empty() {
            return text;
        }

        // info.content 回退
        if let Some(content) = info.get("content").and_then(|v| v.as_array()) {
            let text_parts: Vec<String> = content
                .iter()
                .filter(|e| {
                    e.get("type").and_then(|v| v.as_str()) == Some("text")
                        && (e.get("text").and_then(|v| v.as_str()).is_some()
                            || e.get("content").and_then(|v| v.as_str()).is_some())
                })
                .filter_map(|e| {
                    e.get("text")
                        .and_then(|v| v.as_str())
                        .or_else(|| e.get("content").and_then(|v| v.as_str()))
                        .map(|s| s.to_string())
                })
                .filter(|s| !s.is_empty())
                .collect();

            if !text_parts.is_empty() {
                let mut result = text_parts.join("\n");
                result = result.trim().to_string();
                if NOTIFICATION_BODY_MAX_CHARS > 0 && result.len() > NOTIFICATION_BODY_MAX_CHARS {
                    result = result.chars().take(NOTIFICATION_BODY_MAX_CHARS).collect();
                }
                return result;
            }
        }

        String::new()
    }
}

/// 用 `git revparse --abbrev-ref HEAD` 获取分支名 (3s timeout)。
///
/// 对应 Node template-runtime.js 中通过 simple-git 获取 branch。
async fn resolve_git_branch(worktree_dir: &str) -> String {
    let path = Path::new(worktree_dir);
    if !path.exists() {
        return String::new();
    }

    let git_binary = crate::git::runner::GitRunner::git_binary();
    let result = tokio::time::timeout(
        std::time::Duration::from_millis(3000),
        async {
            tokio::process::Command::new(&git_binary)
                .args(["revparse", "--abbrev-ref", "HEAD"])
                .current_dir(path)
                .output()
                .await
        },
    )
    .await;

    match result {
        Ok(Ok(output)) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        }
        _ => String::new(),
    }
}

// =========================================================================
// 测试
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn resolve_template_basic() {
        let vars = TemplateVars {
            agent_name: "Claude".to_string(),
            model_name: "GPT-4".to_string(),
            ..Default::default()
        };
        let result = NotificationTemplateRuntime::resolve_template(
            "{agent_name} is ready, using {model_name}",
            &vars,
        );
        assert_eq!(result, "Claude is ready, using GPT-4");
    }

    #[test]
    fn resolve_template_missing_var_empty() {
        let vars = TemplateVars::default();
        let result =
            NotificationTemplateRuntime::resolve_template("Hello {nonexistent}", &vars);
        assert_eq!(result, "Hello ");
    }

    #[test]
    fn should_apply_resolved_message_last_message() {
        let template = "{last_message}";
        let vars = TemplateVars {
            last_message: "  ".to_string(),
            ..Default::default()
        };
        assert!(!NotificationTemplateRuntime::should_apply_resolved_message(
            template,
            "resolved",
            &vars,
        ));

        let vars2 = TemplateVars {
            last_message: "content".to_string(),
            ..Default::default()
        };
        assert!(NotificationTemplateRuntime::should_apply_resolved_message(
            template,
            "content",
            &vars2,
        ));
    }

    #[test]
    fn should_apply_resolved_message_empty() {
        let vars = TemplateVars::default();
        assert!(!NotificationTemplateRuntime::should_apply_resolved_message(
            "template",
            "",
            &vars,
        ));
    }

    #[test]
    fn extract_text_from_parts_valid() {
        let parts = json!([
            { "type": "text", "text": "Hello" },
            { "type": "tool", "text": "ignored" },
            { "type": "text", "text": "World" }
        ]);
        let result = NotificationTemplateRuntime::extract_text_from_parts(Some(&parts), 1000);
        assert_eq!(result, "Hello\nWorld");
    }

    #[test]
    fn extract_text_from_parts_empty() {
        let result =
            NotificationTemplateRuntime::extract_text_from_parts(None, 1000);
        assert_eq!(result, "");
    }

    #[test]
    fn extract_text_from_parts_truncation() {
        let parts = json!([{ "type": "text", "text": "abcdefghij" }]);
        let result = NotificationTemplateRuntime::extract_text_from_parts(Some(&parts), 5);
        assert_eq!(result, "abcde");
    }

    #[test]
    fn extract_last_message_text_from_info() {
        let payload = json!({
            "properties": {
                "info": {
                    "parts": [{ "type": "text", "text": "Response text" }]
                }
            }
        });
        let result = NotificationTemplateRuntime::extract_last_message_text(&payload);
        assert_eq!(result, "Response text");
    }

    #[test]
    fn extract_last_message_text_empty() {
        let payload = json!({ "properties": {} });
        let result = NotificationTemplateRuntime::extract_last_message_text(&payload);
        assert_eq!(result, "");
    }

    #[test]
    fn template_vars_get_set() {
        let mut vars = TemplateVars::default();
        vars.set("agent_name", "Test Agent".to_string());
        assert_eq!(vars.get("agent_name"), Some("Test Agent".to_string()));
        assert_eq!(vars.get("nonexistent"), None);
    }
}
