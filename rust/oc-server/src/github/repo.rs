//! GitHub remote URL 解析 + directory → repo 解析。
//!
//! 移植自 `packages/web/server/lib/github/repo/index.js` (55 行)。

use serde::{Deserialize, Serialize};

/// 解析后的 GitHub repo 信息。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitHubRepo {
    pub owner: String,
    pub repo: String,
    pub url: String,
}

/// `parseGitHubRemoteUrl` — 解析 3 种 GitHub remote URL 格式。
///
/// 支持格式:
/// 1. `git@github.com:OWNER/REPO(.git)` (SSH)
/// 2. `ssh://git@github.com/OWNER/REPO(.git)` (SSH protocol)
/// 3. `https://github.com/OWNER/REPO(.git)` (HTTPS, 须 hostname === github.com)
///
/// 返回 `Some(GitHubRepo)` 或 `None` (无效 URL)。
/// `url` 始终规范化为 `https://github.com/{owner}/{repo}`。
pub fn parse_github_remote_url(raw: &str) -> Option<GitHubRepo> {
    let value = raw.trim();
    if value.is_empty() {
        return None;
    }

    // git@github.com:OWNER/REPO.git
    if let Some(rest) = value.strip_prefix("git@github.com:") {
        return parse_owner_repo(rest);
    }

    // ssh://git@github.com/OWNER/REPO.git
    if let Some(rest) = value.strip_prefix("ssh://git@github.com/") {
        return parse_owner_repo(rest);
    }

    // https://github.com/OWNER/REPO(.git) 或其他 URL
    parse_url_form(value)
}

/// 解析 `OWNER/REPO(.git)` 片段。
fn parse_owner_repo(rest: &str) -> Option<GitHubRepo> {
    let cleaned = rest.strip_suffix(".git").unwrap_or(rest);
    let mut parts = cleaned.splitn(2, '/');
    let owner = parts.next().unwrap_or("");
    let repo = parts.next().unwrap_or("");
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some(GitHubRepo {
        owner: owner.to_string(),
        repo: repo.to_string(),
        url: format!("https://github.com/{}/{}", owner, repo),
    })
}

/// 用 URL 解析 HTTPS 格式。
fn parse_url_form(value: &str) -> Option<GitHubRepo> {
    let parsed = url::Url::parse(value).ok()?;
    if parsed.host_str() != Some("github.com") {
        return None;
    }

    let path = parsed.path();
    let trimmed = path.trim_matches('/');
    let cleaned = trimmed.strip_suffix(".git").unwrap_or(trimmed);
    let mut parts = cleaned.splitn(2, '/');
    let owner = parts.next().unwrap_or("");
    let repo = parts.next().unwrap_or("");
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some(GitHubRepo {
        owner: owner.to_string(),
        repo: repo.to_string(),
        url: format!("https://github.com/{}/{}", owner, repo),
    })
}

/// `resolveGitHubRepoFromDirectory` — 从 git remote 解析 repo。
///
/// 调用 `git remote get-url <remoteName>` → `parse_github_remote_url`。
/// 返回 `(Option<GitHubRepo>, Option<remote_url_string>)`。
pub async fn resolve_repo_from_directory(
    directory: &str,
    remote_name: &str,
) -> (Option<GitHubRepo>, Option<String>) {
    let remote_url = match crate::git::remote::get_remote_url(directory, remote_name).await {
        Ok(url) if !url.is_empty() => Some(url),
        _ => None,
    };

    let repo = remote_url
        .as_deref()
        .and_then(parse_github_remote_url);

    (repo, remote_url)
}

mod url {
    /// 极简 URL 解析 (仅用于 github remote URL, 避免引入 url crate)。
    pub struct Url {
        host: String,
        path: String,
    }

    impl Url {
        pub fn parse(input: &str) -> Result<Self, ()> {
            // 跳过 scheme://
            let after_scheme = if let Some(pos) = input.find("://") {
                &input[pos + 3..]
            } else {
                return Err(()); // 无 scheme 的不是 HTTPS URL
            };

            // host = 第一个 / 之前的部分
            let (host_part, path_part) = match after_scheme.find('/') {
                Some(pos) => (&after_scheme[..pos], &after_scheme[pos..]),
                None => (after_scheme, ""),
            };

            // host 可能包含 userinfo@ (虽然 GitHub HTTPS 一般没有)
            let host = host_part.rsplit('@').next().unwrap_or(host_part);

            Ok(Self {
                host: host.to_string(),
                path: path_part.to_string(),
            })
        }

        pub fn host_str(&self) -> Option<&str> {
            Some(&self.host)
        }

        pub fn path(&self) -> &str {
            &self.path
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ssh_format() {
        let repo = parse_github_remote_url("git@github.com:octocat/hello-world.git").unwrap();
        assert_eq!(repo.owner, "octocat");
        assert_eq!(repo.repo, "hello-world");
        assert_eq!(repo.url, "https://github.com/octocat/hello-world");
    }

    #[test]
    fn parse_ssh_no_git_suffix() {
        let repo = parse_github_remote_url("git@github.com:octocat/hello-world").unwrap();
        assert_eq!(repo.owner, "octocat");
        assert_eq!(repo.repo, "hello-world");
    }

    #[test]
    fn parse_ssh_protocol_format() {
        let repo =
            parse_github_remote_url("ssh://git@github.com/octocat/hello-world.git").unwrap();
        assert_eq!(repo.owner, "octocat");
        assert_eq!(repo.repo, "hello-world");
    }

    #[test]
    fn parse_https_format() {
        let repo =
            parse_github_remote_url("https://github.com/octocat/hello-world.git").unwrap();
        assert_eq!(repo.owner, "octocat");
        assert_eq!(repo.repo, "hello-world");
    }

    #[test]
    fn parse_https_no_git_suffix() {
        let repo = parse_github_remote_url("https://github.com/octocat/hello-world").unwrap();
        assert_eq!(repo.owner, "octocat");
        assert_eq!(repo.repo, "hello-world");
    }

    #[test]
    fn parse_https_trailing_slash() {
        let repo = parse_github_remote_url("https://github.com/octocat/hello-world/").unwrap();
        assert_eq!(repo.owner, "octocat");
        assert_eq!(repo.repo, "hello-world");
    }

    #[test]
    fn parse_non_github_returns_none() {
        assert!(parse_github_remote_url("https://gitlab.com/octocat/hello-world").is_none());
    }

    #[test]
    fn parse_invalid_returns_none() {
        assert!(parse_github_remote_url("").is_none());
        assert!(parse_github_remote_url("not a url").is_none());
        assert!(parse_github_remote_url("git@github.com:").is_none());
        assert!(parse_github_remote_url("git@github.com:owner").is_none());
    }

    #[test]
    fn parse_owner_with_dots() {
        let repo =
            parse_github_remote_url("git@github.com:my-org.my-team/my.repo.name.git").unwrap();
        assert_eq!(repo.owner, "my-org.my-team");
        assert_eq!(repo.repo, "my.repo.name");
    }
}
