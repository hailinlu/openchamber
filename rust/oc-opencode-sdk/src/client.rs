//! OpenCode 客户端骨架。
//!
//! 对应现有: `packages/ui/src/lib/opencode/client.ts` +
//! `@opencode-ai/sdk/v2` 的服务端调用面。
//!
//! 阶段 1 会实现: 基础 GET/POST + 认证头 + 健康探测
//! (`/global/health`, `/global/config`).

use oc_core::Result;
use reqwest::Client as HttpClient;

/// OpenCode 服务端客户端。
///
/// 持有 OpenCode server 的 base URL 与认证信息 (managed-password bearer)。
#[derive(Debug, Clone)]
pub struct OpencodeClient {
    http: HttpClient,
    base_url: String,
    auth_token: Option<String>,
}

impl OpencodeClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            http: HttpClient::new(),
            base_url: base_url.into(),
            auth_token: None,
        }
    }

    pub fn with_auth_token(mut self, token: impl Into<String>) -> Self {
        self.auth_token = Some(token.into());
        self
    }

    /// OpenCode server base URL (例如 `http://127.0.0.1:4096`)。
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// 探测 OpenCode 健康状态。对应 `/global/health`。
    ///
    /// 阶段 1 实现。
    pub async fn health(&self) -> Result<()> {
        // TODO 阶段 1: GET {base_url}/global/health, 携带 auth header.
        let _ = (&self.http, &self.base_url, &self.auth_token);
        Ok(())
    }
}
