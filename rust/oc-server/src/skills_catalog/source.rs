//! Git source string parsing — 对应 Node `source.js`。
//!
//! 把 `owner/repo[/subpath]` / SSH / HTTPS 解析为标准化 clone URL + subpath。

use serde::Serialize;

/// 解析结果。
#[derive(Debug, Clone, Serialize)]
pub struct ParsedSkillRepoSource {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clone_url_ssh: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clone_url_https: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effective_subpath: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub normalized_repo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<SourceError>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceError {
    pub kind: String,
    pub message: String,
}

/// 判断 source 是否以 `clawdhub:` 开头。
pub fn is_clawdhub_source(input: &str) -> bool {
    input.trim().starts_with("clawdhub:")
}

/// 标准化 git 简写 `owner/repo[/subpath]` → 完整 URL。
///
/// 返回 ParsedSkillRepoSource, `ok=false` 时带 error 描述。
pub fn parse_skill_repo_source(input: &str) -> ParsedSkillRepoSource {
    let input = input.trim();

    // clawdhub 不走 git 解析
    if is_clawdhub_source(input) {
        return ParsedSkillRepoSource {
            ok: true,
            host: None,
            owner: None,
            repo: None,
            clone_url_ssh: None,
            clone_url_https: None,
            effective_subpath: None,
            normalized_repo: Some(input.to_string()),
            error: None,
        };
    }

    // SSH: git@host:owner/repo[.git][/subpath]
    if let Some(rest) = input.strip_prefix("git@") {
        if let Some(at_colon) = rest.find(':') {
            let host = &rest[..at_colon];
            let remainder = &rest[at_colon + 1..];
            // remainder = owner/repo[.git][/subpath]
            if let Some(slash_pos) = remainder.find('/') {
                let owner = &remainder[..slash_pos];
                let after_owner = &remainder[slash_pos + 1..];
                let (repo_name_raw, subpath) = split_first_path(after_owner);
                let repo_name = repo_name_raw.strip_suffix(".git").unwrap_or(repo_name_raw);
                let normalized = format!("{}/{}", owner, repo_name);
                return ParsedSkillRepoSource {
                    ok: true,
                    host: Some(host.to_string()),
                    owner: Some(owner.to_string()),
                    repo: Some(repo_name.to_string()),
                    clone_url_ssh: Some(format!("git@{}:{}/{}.git", host, owner, repo_name)),
                    clone_url_https: Some(format!("https://{}/{}/{}.git", host, owner, repo_name)),
                    effective_subpath: subpath.map(|s| s.to_string()),
                    normalized_repo: Some(normalized),
                    error: None,
                };
            }
        }
    }

    // HTTPS: https://host/owner/repo[.git][/subpath]
    if let Some(rest) = input.strip_prefix("https://") {
        if let Some(slash) = rest.find('/') {
            let host = &rest[..slash];
            let remainder = &rest[slash + 1..];
            let (repo_part, subpath) = split_first_path(remainder);
            let repo_part = repo_part.strip_suffix(".git").unwrap_or(repo_part);
            if let Some((owner, repo_name)) = repo_part.split_once('/') {
                let normalized = format!("{}/{}", owner, repo_name);
                return ParsedSkillRepoSource {
                    ok: true,
                    host: Some(host.to_string()),
                    owner: Some(owner.to_string()),
                    repo: Some(repo_name.to_string()),
                    clone_url_ssh: Some(format!("git@{}:{}.git", host, repo_part)),
                    clone_url_https: Some(format!("https://{}/{}.git", host, repo_part)),
                    effective_subpath: subpath.map(|s| s.to_string()),
                    normalized_repo: Some(normalized),
                    error: None,
                };
            }
        }
    }

    // shorthand: owner/repo[/subpath]
    if let Some((owner, rest)) = input.split_once('/') {
        // 不含 `://` 或 `git@` 的才是 shorthand
        if !owner.contains('.') || input.len() < input.find('.').unwrap_or(usize::MAX) {
            let default_host = "github.com";
            let (repo_part, subpath) = split_first_path(rest);
            let normalized = format!("{}/{}", owner, repo_part);
            return ParsedSkillRepoSource {
                ok: true,
                host: Some(default_host.to_string()),
                owner: Some(owner.to_string()),
                repo: Some(repo_part.to_string()),
                clone_url_ssh: Some(format!("git@{}:{}/{}.git", default_host, owner, repo_part)),
                clone_url_https: Some(format!("https://{}/{}/{}.git", default_host, owner, repo_part)),
                effective_subpath: subpath.map(|s| s.to_string()),
                normalized_repo: Some(normalized),
                error: None,
            };
        }
    }

    ParsedSkillRepoSource {
        ok: false,
        host: None,
        owner: None,
        repo: None,
        clone_url_ssh: None,
        clone_url_https: None,
        effective_subpath: None,
        normalized_repo: None,
        error: Some(SourceError {
            kind: "invalidSource".to_string(),
            message: format!("Unable to parse skill source: {}", input),
        }),
    }
}

/// 把 `path/rest` 拆成 (第一个 path segment, 剩余部分)。
fn split_first_path(input: &str) -> (&str, Option<&str>) {
    if let Some(idx) = input.find('/') {
        (&input[..idx], Some(&input[idx + 1..]))
    } else {
        (input, None)
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_shorthand() {
        let r = parse_skill_repo_source("anthropics/skills");
        assert!(r.ok);
        assert_eq!(r.owner.as_deref(), Some("anthropics"));
        assert_eq!(r.repo.as_deref(), Some("skills"));
        assert_eq!(r.normalized_repo.as_deref(), Some("anthropics/skills"));
        assert!(r.clone_url_https.unwrap().contains("github.com"));
    }

    #[test]
    fn parse_shorthand_with_subpath() {
        let r = parse_skill_repo_source("anthropics/skills/foo");
        assert!(r.ok);
        assert_eq!(r.effective_subpath.as_deref(), Some("foo"));
    }

    #[test]
    fn parse_ssh_url() {
        let r = parse_skill_repo_source("git@github.com:anthropics/skills.git");
        assert!(r.ok);
        assert_eq!(r.host.as_deref(), Some("github.com"));
        assert_eq!(r.owner.as_deref(), Some("anthropics"));
        assert!(r.clone_url_ssh.unwrap().starts_with("git@"));
    }

    #[test]
    fn parse_https_url() {
        let r = parse_skill_repo_source("https://github.com/anthropics/skills");
        assert!(r.ok);
        assert!(r.clone_url_https.unwrap().starts_with("https://"));
    }

    #[test]
    fn parse_https_with_git_suffix() {
        let r = parse_skill_repo_source("https://github.com/anthropics/skills.git");
        assert!(r.ok);
        assert!(r.clone_url_https.unwrap().ends_with(".git"));
    }

    #[test]
    fn empty_input_returns_error() {
        let r = parse_skill_repo_source("");
        assert!(!r.ok);
        assert!(r.error.is_some());
    }

    #[test]
    fn clawdhub_source() {
        assert!(is_clawdhub_source("clawdhub:registry"));
        assert!(is_clawdhub_source("clawdhub:my-skills"));
        assert!(!is_clawdhub_source("anthropics/skills"));
    }
}
