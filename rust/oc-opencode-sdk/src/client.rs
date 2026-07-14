//! OpenCode 客户端。
//!
//! 对应现有: `packages/ui/src/lib/opencode/client.ts` +
//! `@opencode-ai/sdk/v2` 的服务端调用面。
//!
//! 阶段 1: 基础 GET/POST + 认证头 + 健康探测 (`/global/health`)。

use std::time::Duration;

use oc_core::{Error, Result};
use reqwest::Client as HttpClient;

/// OpenCode 服务端客户端。
///
/// 持有 OpenCode server 的 base URL 与认证信息 (managed-password bearer)。
#[derive(Debug, Clone)]
pub struct OpencodeClient {
    http: HttpClient,
    base_url: String,
    auth_header: Option<String>,
}

impl OpencodeClient {
    /// 创建客户端。`base_url` 例如 `http://127.0.0.1:4096`。
    pub fn new(base_url: impl Into<String>) -> Self {
        let http = HttpClient::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| HttpClient::new());
        Self {
            http,
            base_url: base_url.into(),
            auth_header: None,
        }
    }

    /// 设置 Basic auth header (例如 `Basic <base64("opencode:<password>")>`)。
    pub fn with_auth_header(mut self, header: impl Into<String>) -> Self {
        self.auth_header = Some(header.into());
        self
    }

    /// OpenCode server base URL (例如 `http://127.0.0.1:4096`)。
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// 构建完整 URL: `{base_url}/global/health`。
    fn health_url(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        format!("{}/global/health", base)
    }

    /// 探测 OpenCode 健康状态。对应 `/global/health`。
    ///
    /// 返回 `Ok(())` 当且仅当上游返回 2xx 且 JSON body 中 `healthy == true`。
    /// 对应 Node 侧 `network-runtime.js` 的 `waitForReady` 逻辑。
    pub async fn health(&self) -> Result<()> {
        let mut req = self.http.get(self.health_url()).header("Accept", "application/json");
        if let Some(ref auth) = self.auth_header {
            req = req.header("Authorization", auth);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| Error::Internal(format!("opencode health request failed: {}", e)))?;

        if !resp.status().is_success() {
            return Err(Error::Upstream {
                status: resp.status().as_u16(),
                body: format!("health check returned {}", resp.status()),
            });
        }

        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| Error::Internal(format!("failed to parse health response: {}", e)))?;

        if body.get("healthy").and_then(|v| v.as_bool()).unwrap_or(false) {
            Ok(())
        } else {
            Err(Error::Internal("opencode health check: healthy != true".into()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_url_strips_trailing_slash() {
        let c = OpencodeClient::new("http://127.0.0.1:4096/");
        assert_eq!(c.health_url(), "http://127.0.0.1:4096/global/health");
    }

    #[test]
    fn health_url_no_trailing_slash() {
        let c = OpencodeClient::new("http://127.0.0.1:4096");
        assert_eq!(c.health_url(), "http://127.0.0.1:4096/global/health");
    }

    #[test]
    fn auth_header_stored() {
        let c = OpencodeClient::new("http://localhost:1").with_auth_header("Basic abc123");
        assert_eq!(c.auth_header.as_deref(), Some("Basic abc123"));
    }
}
