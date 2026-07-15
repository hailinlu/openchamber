//! OpenCode HTTP 客户端 helper — 供 session-assist / session-goal 等
//! metadata-driven runtime 共用。
//!
//! 设计原则 (与 `permission_auto_accept::fetch_session_info` 一致):
//! - 直接 `reqwest` + `opencode_base_url` + `auth_header`, 不引入 SDK
//! - directory 通过 query string 传递 (`?directory=...`)
//! - 5-10s 超时 (per-call, 与 permission_auto_accept 一致)
//!
//! 公开 API:
//! - `fetch_session(session_id, directory)` → `Value` (含 `parentID` / `metadata` 等)
//! - `fetch_session_messages(session_id, limit, directory)` → `Vec<Value>`
//! - `patch_session_metadata(session_id, directory, metadata)` → 合并写 metadata
//! - `prompt_async(session_id, directory, body)` → `Value` (continuation prompt)
//! - `create_session(directory, body)` → `Value`

#![allow(dead_code)] // 模块大部分方法由后续 group 使用; 当前 G3 只用 fetch + patch + prompt_async

use serde_json::{json, Value};

use crate::error::ApiError;
use crate::state::AppState;

/// 单次请求超时。
const REQUEST_TIMEOUT_MS: u64 = 10_000;

/// 构造 OpenCode HTTP 客户端。
///
/// 复用 `state.http_client` 连接池 + `state.opencode_base_url` + `state.opencode_auth_header`。
pub fn build(state: &AppState) -> OpenCodeClient<'_> {
    OpenCodeClient {
        http: &state.http_client,
        base_url: state.opencode_base_url.trim_end_matches('/'),
        auth_header: state.opencode_auth_header.as_str(),
    }
}

/// OpenCode HTTP 客户端 (借用 AppState 字段, 零拷贝)。
pub struct OpenCodeClient<'a> {
    http: &'a reqwest::Client,
    base_url: &'a str,
    auth_header: &'a str,
}

impl<'a> OpenCodeClient<'a> {
    /// `GET /session/{id}?directory=...` → 返回 session object (可能含 `data` envelope)。
    ///
    /// 失败返回 `None` (与 permission_auto_accept::fetch_session_info 行为一致)。
    pub async fn fetch_session(
        &self,
        session_id: &str,
        directory: Option<&str>,
    ) -> Result<Option<Value>, ApiError> {
        let url = format!("{}/session/{}", self.base_url, session_id);
        let mut req = self
            .http
            .get(&url)
            .header("accept", "application/json")
            .header("authorization", self.auth_header)
            .timeout(std::time::Duration::from_millis(REQUEST_TIMEOUT_MS));
        if let Some(dir) = directory.filter(|d| !d.is_empty()) {
            req = req.query(&[("directory", dir)]);
        }

        match req.send().await {
            Ok(resp) if resp.status().is_success() => {
                let data: Value = resp.json().await.unwrap_or(Value::Null);
                // 兼容 `{data: ...}` envelope 与扁平 object 两种返回
                Ok(Some(data.get("data").cloned().unwrap_or(data)))
            }
            _ => Ok(None),
        }
    }

    /// `GET /session/{id}/message?limit=N&directory=...` → 返回 message 数组。
    pub async fn fetch_session_messages(
        &self,
        session_id: &str,
        limit: u32,
        directory: Option<&str>,
    ) -> Result<Option<Vec<Value>>, ApiError> {
        let url = format!("{}/session/{}/message", self.base_url, session_id);
        let mut req = self
            .http
            .get(&url)
            .header("accept", "application/json")
            .header("authorization", self.auth_header)
            .timeout(std::time::Duration::from_millis(REQUEST_TIMEOUT_MS))
            .query(&[("limit", limit.to_string())]);
        if let Some(dir) = directory.filter(|d| !d.is_empty()) {
            req = req.query(&[("directory", dir)]);
        }

        match req.send().await {
            Ok(resp) if resp.status().is_success() => {
                let data: Value = resp.json().await.unwrap_or(Value::Null);
                let arr = data.get("data").cloned().unwrap_or(data);
                Ok(arr.as_array().cloned())
            }
            _ => Ok(None),
        }
    }

    /// `PATCH /session/{id}?directory=...` body `{metadata: ...}` → 覆盖 session metadata。
    ///
    /// 注意: OpenCode PATCH 行为 — 实际行为以 OpenCode 后端为准; 当前移植假设
    /// 顶层 metadata 是浅合并 (同 Node 端). 若 OpenCode 端实现不同需调整。
    pub async fn patch_session_metadata(
        &self,
        session_id: &str,
        directory: Option<&str>,
        metadata: &Value,
    ) -> Result<(), ApiError> {
        let url = format!("{}/session/{}", self.base_url, session_id);
        let mut req = self
            .http
            .patch(&url)
            .header("accept", "application/json")
            .header("content-type", "application/json")
            .header("authorization", self.auth_header)
            .timeout(std::time::Duration::from_millis(REQUEST_TIMEOUT_MS))
            .json(&json!({ "metadata": metadata }));
        if let Some(dir) = directory.filter(|d| !d.is_empty()) {
            req = req.query(&[("directory", dir)]);
        }

        let resp = req.send().await.map_err(|e| ApiError(oc_core::Error::Internal(format!(
            "opencode patch_session_metadata failed: {}",
            e
        ))))?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(ApiError(oc_core::Error::Upstream { status, body }));
        }
        Ok(())
    }

    /// `POST /session/{id}/prompt_async?directory=...` body `{...}` → 提交 continuation prompt。
    ///
    /// 失败抛 ApiError (供 caller 单飞重试或放弃)。
    pub async fn prompt_async(
        &self,
        session_id: &str,
        directory: Option<&str>,
        body: &Value,
    ) -> Result<Option<Value>, ApiError> {
        let url = format!("{}/session/{}/prompt_async", self.base_url, session_id);
        let mut req = self
            .http
            .post(&url)
            .header("accept", "application/json")
            .header("content-type", "application/json")
            .header("authorization", self.auth_header)
            .timeout(std::time::Duration::from_millis(REQUEST_TIMEOUT_MS))
            .json(body);
        if let Some(dir) = directory.filter(|d| !d.is_empty()) {
            req = req.query(&[("directory", dir)]);
        }

        let resp = req.send().await.map_err(|e| ApiError(oc_core::Error::Internal(format!(
            "opencode prompt_async failed: {}",
            e
        ))))?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(ApiError(oc_core::Error::Upstream { status, body }));
        }
        let data: Value = resp.json().await.unwrap_or(Value::Null);
        Ok(Some(data))
    }

    /// `POST /session?directory=...` body `{...}` → 创建新 session。
    #[allow(dead_code)] // 后续 G4 scheduled-tasks 用
    pub async fn create_session(
        &self,
        directory: Option<&str>,
        body: &Value,
    ) -> Result<Option<Value>, ApiError> {
        let url = format!("{}/session", self.base_url);
        let mut req = self
            .http
            .post(&url)
            .header("accept", "application/json")
            .header("content-type", "application/json")
            .header("authorization", self.auth_header)
            .timeout(std::time::Duration::from_millis(REQUEST_TIMEOUT_MS))
            .json(body);
        if let Some(dir) = directory.filter(|d| !d.is_empty()) {
            req = req.query(&[("directory", dir)]);
        }

        let resp = req.send().await.map_err(|e| ApiError(oc_core::Error::Internal(format!(
            "opencode create_session failed: {}",
            e
        ))))?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(ApiError(oc_core::Error::Upstream { status, body }));
        }
        let data: Value = resp.json().await.unwrap_or(Value::Null);
        Ok(Some(data))
    }
}

// =========================================================================
// 测试 (mock HTTP server, 同 opencode::models_metadata 测试模式)
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicU16, Ordering};
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// 启动 mock HTTP server, 根据 path 匹配返回固定 JSON。
    /// 返回 `(port, count, handle)`。
    async fn start_mock_server(handlers: Vec<(&'static str, u16, &'static str)>) -> (u16, Arc<AtomicU16>, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let request_count = Arc::new(AtomicU16::new(0));
        let count_clone = request_count.clone();

        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { break };
                let count = count_clone.clone();
                let handlers = handlers.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    if n == 0 { return; }
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let first_line = req.lines().next().unwrap_or("");
                    let method_path = first_line.split_whitespace().take(2).collect::<Vec<_>>().join(" ");
                    let parts: Vec<&str> = method_path.split_whitespace().collect();
                    let method = parts.first().copied().unwrap_or("");
                    let path = parts.get(1).copied().unwrap_or("");
                    count.fetch_add(1, Ordering::SeqCst);

                    let mut matched: Option<&(&'static str, u16, &'static str)> = None;
                    for h in &handlers {
                        // handler format: "METHOD /path"
                        let parts_h: Vec<&str> = h.0.split_whitespace().collect();
                        let hm = parts_h.first().copied().unwrap_or("");
                        let hp = parts_h.get(1).copied().unwrap_or("");
                        if hm == method && path.starts_with(hp) {
                            matched = Some(h);
                            break;
                        }
                    }

                    let (status, body) = if let Some(h) = matched {
                        (h.1, h.2.to_string())
                    } else {
                        (404u16, r#"{"error":"not found"}"#.to_string())
                    };

                    let response = format!(
                        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        status,
                        if status == 200 { "OK" } else { "ERROR" },
                        body.len(),
                        body,
                    );
                    let _ = sock.write_all(response.as_bytes()).await;
                });
            }
        });

        (port, request_count, handle)
    }

    fn client_for_port(port: u16) -> (reqwest::Client, String) {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(REQUEST_TIMEOUT_MS))
            .build()
            .unwrap();
        (http, format!("http://127.0.0.1:{port}"))
    }

    #[tokio::test]
    async fn fetch_session_returns_data_envelope() {
        let body = json!({ "data": { "id": "sess_1", "parentID": null, "directory": "/tmp" } }).to_string();
        let (port, _count, h) = start_mock_server(vec![
            ("GET /session/", 200, Box::leak(body.into_boxed_str())),
        ]).await;

        let (http, base) = client_for_port(port);
        let c = OpenCodeClient { http: &http, base_url: &base, auth_header: "Basic xyz" };
        let s = c.fetch_session("sess_1", None).await.unwrap();
        assert!(s.is_some());
        assert_eq!(s.unwrap()["id"], "sess_1");
        h.abort();
    }

    #[tokio::test]
    async fn fetch_session_returns_none_on_404() {
        let (port, _count, h) = start_mock_server(vec![
            ("GET /session/", 404, r#"{"error":"not found"}"#),
        ]).await;

        let (http, base) = client_for_port(port);
        let c = OpenCodeClient { http: &http, base_url: &base, auth_header: "Basic xyz" };
        let s = c.fetch_session("sess_1", None).await.unwrap();
        assert!(s.is_none());
        h.abort();
    }

    #[tokio::test]
    async fn fetch_session_messages_returns_array() {
        let body = json!({ "data": [{ "info": { "id": "m1", "role": "user" } }] }).to_string();
        let (port, _count, h) = start_mock_server(vec![
            ("GET /session/", 200, Box::leak(body.into_boxed_str())),
        ]).await;

        let (http, base) = client_for_port(port);
        let c = OpenCodeClient { http: &http, base_url: &base, auth_header: "Basic xyz" };
        let msgs = c.fetch_session_messages("sess_1", 12, None).await.unwrap();
        assert!(msgs.is_some());
        assert_eq!(msgs.unwrap().len(), 1);
        h.abort();
    }

    #[tokio::test]
    async fn patch_session_metadata_success() {
        let (port, count, h) = start_mock_server(vec![
            ("PATCH /session/", 200, r#"{"ok":true}"#),
        ]).await;

        let (http, base) = client_for_port(port);
        let c = OpenCodeClient { http: &http, base_url: &base, auth_header: "Basic xyz" };
        c.patch_session_metadata("sess_1", Some("/tmp"), &json!({"openchamber": {"goal": {"id": "g1"}}}))
            .await
            .unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 1);
        h.abort();
    }

    #[tokio::test]
    async fn patch_session_metadata_error_returns_upstream() {
        let (port, _count, h) = start_mock_server(vec![
            ("PATCH /session/", 500, r#"{"error":"server"}"#),
        ]).await;

        let (http, base) = client_for_port(port);
        let c = OpenCodeClient { http: &http, base_url: &base, auth_header: "Basic xyz" };
        let result = c.patch_session_metadata("sess_1", None, &json!({})).await;
        assert!(result.is_err());
        if let Err(ApiError(oc_core::Error::Upstream { status, .. })) = result {
            assert_eq!(status, 500);
        } else {
            panic!("expected Upstream error");
        }
        h.abort();
    }

    #[tokio::test]
    async fn prompt_async_success() {
        let body = json!({ "ok": true }).to_string();
        let (port, _count, h) = start_mock_server(vec![
            ("POST /session/", 200, Box::leak(body.into_boxed_str())),
        ]).await;

        let (http, base) = client_for_port(port);
        let c = OpenCodeClient { http: &http, base_url: &base, auth_header: "Basic xyz" };
        let res = c.prompt_async("sess_1", None, &json!({"parts": []})).await.unwrap();
        assert!(res.is_some());
        h.abort();
    }

    #[tokio::test]
    async fn create_session_success() {
        let body = json!({ "data": { "id": "sess_new" } }).to_string();
        let (port, _count, h) = start_mock_server(vec![
            ("POST /session", 200, Box::leak(body.into_boxed_str())),
        ]).await;

        let (http, base) = client_for_port(port);
        let c = OpenCodeClient { http: &http, base_url: &base, auth_header: "Basic xyz" };
        let res = c.create_session(Some("/work"), &json!({"title": "test"})).await.unwrap();
        assert_eq!(res.unwrap()["data"]["id"], "sess_new");
        h.abort();
    }
}