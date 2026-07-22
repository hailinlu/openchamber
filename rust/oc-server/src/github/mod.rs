//! GitHub 模块 — OAuth device flow + PR/issue/repo 集成。
//!
//! 移植自 `packages/web/server/lib/github/` (~3000 行)。
//!
//! 策略: 直接用 `reqwest` 调用 GitHub REST API (不引入 `octocrab`),
//! 因为 `@octokit/rest` 本质是带类型封装的 HTTP 客户端。所有调用都是
//! 标准 GitHub REST 端点, `reqwest` (workspace 已有) 完全胜任。
//! git 集成 (getRemotes/getStatus/getRemoteUrl) 复用阶段 3a (后半)
//! 已实现的 `crate::git::*` 函数。

pub mod auth;
pub mod client;
pub mod device_flow;
pub mod fork_detection;
pub mod pr_status;
pub mod rate_limit;
pub mod repo;
pub mod routes;
pub mod settings;

/// gh-CLI sentinel account ID (与 Node `GH_CLI_ACCOUNT_ID` 对齐)。
pub const GH_CLI_ACCOUNT_ID: &str = "gh-cli";

/// 默认 GitHub OAuth client ID。
pub const DEFAULT_GITHUB_CLIENT_ID: &str = "Ov23lizomPOC3eFYo56r";

/// 默认 GitHub scopes。
pub const DEFAULT_GITHUB_SCOPES: &str = "repo read:org workflow read:user user:email";

/// GitHub API base URL (可通过 env 覆盖, 测试用)。
pub fn api_base_url() -> String {
    match std::env::var("GRIDFORGE_GITHUB_API_URL") {
        Ok(url) if !url.is_empty() => url,
        _ => "https://api.github.com".to_string(),
    }
}

/// Device flow device-code URL。
pub const DEVICE_CODE_URL: &str = "https://github.com/login/device/code";

/// Device flow access-token URL。
pub const ACCESS_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";

/// Device flow grant type。
pub const DEVICE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";
