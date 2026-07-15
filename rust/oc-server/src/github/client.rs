//! GitHub REST/GraphQL API 客户端。
//!
//! 替代 `@octokit/rest`。用 `reqwest` 直接调用 GitHub REST API。
//!
//! 关键行为 (与 Node 对齐):
//! - 8s per-request timeout (对应 Node 的 `AbortSignal.timeout(8000)`)
//! - Headers: `Authorization: token <token>`, `Accept: application/vnd.github+json`,
//!   `User-Agent: openchamber`
//! - 错误携带 status + headers (rate-limit 检测用)

use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::{json, Value};

/// GitHub API 请求超时 (对应 Node `OCTOKIT_REQUEST_TIMEOUT_MS = 8000`)。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);

/// GitHub API 错误 (携带 status + headers + body)。
#[derive(Debug)]
pub struct GitHubApiError {
    pub status: u16,
    pub message: String,
    pub headers: HeaderMap,
    /// 原始响应体 (保留用于调试/扩展, 当前 rate-limit 检测仅依赖 headers)。
    #[allow(dead_code)]
    pub body: Value,
}

impl GitHubApiError {
    /// 从 reqwest Response 构建错误。
    async fn from_response(resp: reqwest::Response) -> Self {
        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        let body = resp.json::<Value>().await.unwrap_or(Value::Null);
        let message = body
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("GitHub API error")
            .to_string();
        Self {
            status,
            message,
            headers,
            body,
        }
    }
}

impl std::fmt::Display for GitHubApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "GitHub API error ({}): {}", self.status, self.message)
    }
}

impl std::error::Error for GitHubApiError {}

/// GitHub REST API 客户端。
pub struct GitHubClient {
    http: reqwest::Client,
    token: String,
    base_url: String,
}

impl GitHubClient {
    /// 用指定 token 创建客户端。
    pub fn new(token: String) -> Self {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .expect("failed to build GitHub HTTP client");

        Self {
            http,
            token,
            base_url: crate::github::api_base_url(),
        }
    }

    /// 从当前 auth + gh-CLI token 解析活跃 token, 创建客户端。
    /// 无 token 时返回 None (对应 Node `getOctokitOrNull`)。
    pub fn from_current_auth() -> Option<Self> {
        let token = resolve_active_token()?;
        Some(Self::new(token))
    }

    /// 构建 GitHub API 请求 headers。
    fn build_headers(&self, extra: Option<(&str, HeaderValue)>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            HeaderValue::from_str(&format!("token {}", self.token))
                .unwrap_or_else(|_| HeaderValue::from_static("token")),
        );
        headers.insert(
            reqwest::header::ACCEPT,
            HeaderValue::from_static("application/vnd.github+json"),
        );
        headers.insert(
            reqwest::header::USER_AGENT,
            HeaderValue::from_static("openchamber"),
        );
        if let Some((name, value)) = extra {
            if let Ok(name) = reqwest::header::HeaderName::from_bytes(name.as_bytes()) {
                headers.insert(name, value);
            }
        }
        headers
    }

    // ================================================================
    // REST: Users
    // ================================================================

    /// `GET /user` — 当前认证用户。
    pub async fn users_get_authenticated(&self) -> Result<Value, GitHubApiError> {
        self.get_json("/user").await
    }

    /// `GET /user/emails?per_page=100` — 当前用户邮箱列表。
    pub async fn users_list_emails(&self) -> Result<Vec<Value>, GitHubApiError> {
        self.get_json_vec("/user/emails?per_page=100").await
    }

    // ================================================================
    // REST: Repos
    // ================================================================

    /// `GET /repos/{owner}/{repo}` — 仓库元数据。
    pub async fn repos_get(&self, owner: &str, repo: &str) -> Result<Value, GitHubApiError> {
        self.get_json(&format!("/repos/{}/{}", owner, repo)).await
    }

    /// `GET /repos/{owner}/{repo}/branches?per_page=100&page={page}` — 分支列表 (分页)。
    pub async fn repos_list_branches(
        &self,
        owner: &str,
        repo: &str,
        page: u32,
    ) -> Result<Vec<Value>, GitHubApiError> {
        self.get_json_vec(&format!(
            "/repos/{}/{}/branches?per_page=100&page={}",
            owner, repo, page
        ))
        .await
    }

    /// `GET /repos/{owner}/{repo}/branches/{branch}` — 单个分支。
    pub async fn repos_get_branch(
        &self,
        owner: &str,
        repo: &str,
        branch: &str,
    ) -> Result<Value, GitHubApiError> {
        self.get_json(&format!("/repos/{}/branches/{}/{}", owner, repo, branch))
            .await
    }

    /// `GET /repos/{owner}/{repo}/git/refs/{ref}` — Git ref。
    pub async fn git_get_ref(
        &self,
        owner: &str,
        repo: &str,
        ref_name: &str,
    ) -> Result<Value, GitHubApiError> {
        self.get_json(&format!("/repos/{}/{}/git/refs/{}", owner, repo, ref_name))
            .await
    }

    /// `GET /repos/{owner}/{repo}/collaborators/{username}/permission`
    pub async fn repos_get_collaborator_permission(
        &self,
        owner: &str,
        repo: &str,
        username: &str,
    ) -> Result<Value, GitHubApiError> {
        self.get_json(&format!(
            "/repos/{}/{}/collaborators/{}/permission",
            owner, repo, username
        ))
        .await
    }

    // ================================================================
    // REST: Pulls
    // ================================================================

    /// `GET /repos/{owner}/{repo}/pulls?state=...&head=...&per_page=100`
    pub async fn pulls_list(
        &self,
        owner: &str,
        repo: &str,
        state: &str,
        head: Option<&str>,
    ) -> Result<Vec<Value>, GitHubApiError> {
        let mut path = format!("/repos/{}/{}/pulls?state={}&per_page=100", owner, repo, state);
        if let Some(h) = head {
            path.push_str(&format!("&head={}", h));
        }
        self.get_json_vec(&path).await
    }

    /// `POST /repos/{owner}/{repo}/pulls`
    pub async fn pulls_create(
        &self,
        owner: &str,
        repo: &str,
        body: &Value,
    ) -> Result<Value, GitHubApiError> {
        self.post_json(&format!("/repos/{}/{}/pulls", owner, repo), body)
            .await
    }

    /// `PATCH /repos/{owner}/{repo}/pulls/{number}`
    pub async fn pulls_update(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        body: &Value,
    ) -> Result<Value, GitHubApiError> {
        self.patch_json(&format!("/repos/{}/{}/pulls/{}", owner, repo, number), body)
            .await
    }

    /// `PUT /repos/{owner}/{repo}/pulls/{number}/merge`
    pub async fn pulls_merge(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        body: &Value,
    ) -> Result<Value, GitHubApiError> {
        self.put_json(&format!("/repos/{}/{}/pulls/{}/merge", owner, repo, number), body)
            .await
    }

    /// `GET /repos/{owner}/{repo}/pulls/{number}`
    pub async fn pulls_get(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<Value, GitHubApiError> {
        self.get_json(&format!("/repos/{}/{}/pulls/{}", owner, repo, number))
            .await
    }

    /// `GET /repos/{owner}/{repo}/pulls/{number}/files`
    pub async fn pulls_list_files(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<Vec<Value>, GitHubApiError> {
        self.get_json_vec(&format!(
            "/repos/{}/{}/pulls/{}/files?per_page=100",
            owner, repo, number
        ))
        .await
    }

    /// `GET /repos/{owner}/{repo}/pulls/{number}/comments`
    pub async fn pulls_list_review_comments(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<Vec<Value>, GitHubApiError> {
        self.get_json_vec(&format!(
            "/repos/{}/{}/pulls/{}/comments?per_page=100",
            owner, repo, number
        ))
        .await
    }

    /// `GET /repos/{owner}/{repo}/pulls/{number}` (diff format)
    pub async fn pulls_get_diff(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<String, GitHubApiError> {
        let url = format!("{}/repos/{}/{}/pulls/{}", self.base_url, owner, repo, number);
        let resp = self
            .http
            .get(&url)
            .headers(self.build_headers(Some((
                "Accept",
                HeaderValue::from_static("application/vnd.github.v3.diff"),
            ))))
            .send()
            .await
            .map_err(|e| GitHubApiError {
                status: 0,
                message: e.to_string(),
                headers: HeaderMap::new(),
                body: Value::Null,
            })?;

        if !resp.status().is_success() {
            return Err(GitHubApiError::from_response(resp).await);
        }
        resp.text()
            .await
            .map_err(|e| GitHubApiError {
                status: 0,
                message: e.to_string(),
                headers: HeaderMap::new(),
                body: Value::Null,
            })
    }

    // ================================================================
    // REST: Statuses / Checks / Actions
    // ================================================================

    /// `GET /repos/{owner}/{repo}/commits/{ref}/status`
    pub async fn repos_get_combined_status_for_ref(
        &self,
        owner: &str,
        repo: &str,
        ref_name: &str,
    ) -> Result<Value, GitHubApiError> {
        self.get_json(&format!(
            "/repos/{}/{}/commits/{}/status",
            owner, repo, ref_name
        ))
        .await
    }

    /// `GET /repos/{owner}/{repo}/commits/{ref}/check-runs?per_page=100`
    pub async fn checks_list_for_ref(
        &self,
        owner: &str,
        repo: &str,
        ref_name: &str,
    ) -> Result<Value, GitHubApiError> {
        self.get_json(&format!(
            "/repos/{}/{}/commits/{}/check-runs?per_page=100",
            owner, repo, ref_name
        ))
        .await
    }

    /// `GET /repos/{owner}/{repo}/check-runs/{id}/annotations?per_page=100&page={page}`
    pub async fn checks_list_annotations(
        &self,
        owner: &str,
        repo: &str,
        check_run_id: u64,
        page: u32,
    ) -> Result<Vec<Value>, GitHubApiError> {
        self.get_json_vec(&format!(
            "/repos/{}/{}/check-runs/{}/annotations?per_page=100&page={}",
            owner, repo, check_run_id, page
        ))
        .await
    }

    /// `GET /repos/{owner}/{repo}/actions/runs/{run_id}/jobs?per_page=100`
    pub async fn actions_list_jobs(
        &self,
        owner: &str,
        repo: &str,
        run_id: u64,
    ) -> Result<Value, GitHubApiError> {
        self.get_json(&format!(
            "/repos/{}/{}/actions/runs/{}/jobs?per_page=100",
            owner, repo, run_id
        ))
        .await
    }

    // ================================================================
    // REST: Issues
    // ================================================================

    /// `GET /repos/{owner}/{repo}/issues?state=...&per_page=100&page={page}`
    pub async fn issues_list_for_repo(
        &self,
        owner: &str,
        repo: &str,
        state: &str,
        per_page: u32,
        page: u32,
    ) -> Result<Vec<Value>, GitHubApiError> {
        self.get_json_vec(&format!(
            "/repos/{}/{}/issues?state={}&per_page={}&page={}",
            owner, repo, state, per_page, page
        ))
        .await
    }

    /// `GET /repos/{owner}/{repo}/issues/{number}`
    pub async fn issues_get(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<Value, GitHubApiError> {
        self.get_json(&format!("/repos/{}/{}/issues/{}", owner, repo, number))
            .await
    }

    /// `GET /repos/{owner}/{repo}/issues/{number}/comments?per_page=100`
    pub async fn issues_list_comments(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<Vec<Value>, GitHubApiError> {
        self.get_json_vec(&format!(
            "/repos/{}/{}/issues/{}/comments?per_page=100",
            owner, repo, number
        ))
        .await
    }

    // ================================================================
    // REST: Search
    // ================================================================

    /// `GET /search/issues?q=...&per_page={per_page}&page={page}`
    pub async fn search_issues(
        &self,
        q: &str,
        per_page: u32,
        page: u32,
    ) -> Result<Value, GitHubApiError> {
        let encoded_q = url_encode(q);
        self.get_json(&format!(
            "/search/issues?q={}&per_page={}&page={}",
            encoded_q, per_page, page
        ))
        .await
    }

    // ================================================================
    // GraphQL
    // ================================================================

    /// `POST /graphql`
    pub async fn graphql(
        &self,
        query: &str,
        variables: Value,
    ) -> Result<Value, GitHubApiError> {
        let body = json!({
            "query": query,
            "variables": variables,
        });
        let url = format!("{}/graphql", self.base_url);
        let resp = self
            .http
            .post(&url)
            .headers(self.build_headers(None))
            .json(&body)
            .send()
            .await
            .map_err(|e| GitHubApiError {
                status: 0,
                message: e.to_string(),
                headers: HeaderMap::new(),
                body: Value::Null,
            })?;

        if !resp.status().is_success() {
            return Err(GitHubApiError::from_response(resp).await);
        }
        resp.json::<Value>()
            .await
            .map_err(|e| GitHubApiError {
                status: 0,
                message: e.to_string(),
                headers: HeaderMap::new(),
                body: Value::Null,
            })
    }

    // ================================================================
    // 内部 HTTP helpers
    // ================================================================

    async fn get_json(&self, path: &str) -> Result<Value, GitHubApiError> {
        let url = format!("{}{}", self.base_url, path);
        let resp = self
            .http
            .get(&url)
            .headers(self.build_headers(None))
            .send()
            .await
            .map_err(|e| GitHubApiError {
                status: 0,
                message: e.to_string(),
                headers: HeaderMap::new(),
                body: Value::Null,
            })?;

        if !resp.status().is_success() {
            return Err(GitHubApiError::from_response(resp).await);
        }
        resp.json::<Value>()
            .await
            .map_err(|e| GitHubApiError {
                status: 0,
                message: e.to_string(),
                headers: HeaderMap::new(),
                body: Value::Null,
            })
    }

    async fn get_json_vec(&self, path: &str) -> Result<Vec<Value>, GitHubApiError> {
        let val = self.get_json(path).await?;
        Ok(val.as_array().cloned().unwrap_or_default())
    }

    async fn post_json(&self, path: &str, body: &Value) -> Result<Value, GitHubApiError> {
        self.send_json(reqwest::Method::POST, path, Some(body)).await
    }

    async fn patch_json(&self, path: &str, body: &Value) -> Result<Value, GitHubApiError> {
        self.send_json(reqwest::Method::PATCH, path, Some(body))
            .await
    }

    async fn put_json(&self, path: &str, body: &Value) -> Result<Value, GitHubApiError> {
        self.send_json(reqwest::Method::PUT, path, Some(body)).await
    }

    async fn send_json(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, GitHubApiError> {
        let url = format!("{}{}", self.base_url, path);
        let mut req = self
            .http
            .request(method, &url)
            .headers(self.build_headers(None));
        if let Some(b) = body {
            req = req.json(b);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| GitHubApiError {
                status: 0,
                message: e.to_string(),
                headers: HeaderMap::new(),
                body: Value::Null,
            })?;

        if !resp.status().is_success() {
            return Err(GitHubApiError::from_response(resp).await);
        }
        resp.json::<Value>()
            .await
            .map_err(|e| GitHubApiError {
                status: 0,
                message: e.to_string(),
                headers: HeaderMap::new(),
                body: Value::Null,
            })
    }
}

/// 解析当前活跃 token (对应 Node `getOctokitOrNull` 的 token 解析逻辑)。
///
/// 优先级: ghCliActive ? (ghToken || authToken) : (authToken || ghToken)
fn resolve_active_token() -> Option<String> {
    let auth_token = crate::github::auth::get_github_auth()
        .map(|e| e.access_token);
    let gh_cli_disabled = crate::github::settings::is_gh_cli_disabled();
    let gh_token = if !gh_cli_disabled {
        crate::github::auth::get_gh_cli_token()
    } else {
        None
    };
    let gh_cli_active = crate::github::settings::is_gh_cli_active();

    let token = if gh_cli_active {
        gh_token.clone().or(auth_token)
    } else {
        auth_token.or(gh_token)
    };

    token.filter(|t| !t.is_empty())
}

/// 极简 URL 编码 (仅用于 search query)。
fn url_encode(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                result.push(byte as char);
            }
            b' ' => result.push_str("%20"),
            _ => {
                result.push_str(&format!("%{:02X}", byte));
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_encode_basic() {
        assert_eq!(url_encode("hello world"), "hello%20world");
        assert_eq!(url_encode("is:pr head:feature"), "is%3Apr%20head%3Afeature");
        assert_eq!(url_encode("abc-123_test"), "abc-123_test");
    }

    #[test]
    fn github_client_constructs() {
        // 注意: base_url 受 OPENCHAMBER_GITHUB_API_URL 环境变量影响 (测试并行运行可能被
        // github_client_custom_base_url 污染), 所以这里只验证 token 正确设置。
        let client = GitHubClient::new("test_token".to_string());
        assert_eq!(client.token, "test_token");
        assert!(!client.base_url.is_empty());
    }

    #[test]
    fn github_client_custom_base_url() {
        // 注意: 环境变量是进程全局的, 测试并行运行可能相互干扰。
        // 先清除可能残留的值, 设置自定义值, 验证, 然后清除。
        std::env::remove_var("OPENCHAMBER_GITHUB_API_URL");
        std::env::set_var("OPENCHAMBER_GITHUB_API_URL", "http://localhost:9999");
        let client = GitHubClient::new("test".to_string());
        assert_eq!(client.base_url, "http://localhost:9999");
        std::env::remove_var("OPENCHAMBER_GITHUB_API_URL");
    }
}
