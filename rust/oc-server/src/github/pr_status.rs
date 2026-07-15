//! GitHub PR status 解析 (最复杂的 github 函数)。
//!
//! 移植自 `packages/web/server/lib/github/pr-status.js` (567 行)。
//!
//! 解析流程:
//! 1. directory 存在检查
//! 2. 获取 git status + remotes
//! 3. 解析 tracking remote/branch
//! 4. 排序 remote 候选 [explicit, tracking, origin, upstream, rest]
//! 5. 并发解析 remote 候选
//! 6. 展开 repo network (parent/source)
//! 7. 按优先级遍历: 对每个 target × branch candidate 找 PR
//! 8. search fallback

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use once_cell::sync::Lazy;
use serde_json::Value;

use crate::github::client::{GitHubApiError, GitHubClient};
use crate::github::fork_detection::{get_repo_metadata, normalize_repo_key};
use crate::github::rate_limit::RateLimitState;
use crate::github::repo::GitHubRepo;

const SEARCH_API_RETRY: Duration = Duration::from_secs(5 * 60);

// ============================================================
// Text helpers (与 pr-status.js 对齐)
// ============================================================

fn normalize_text(value: Option<&str>) -> String {
    value.unwrap_or("").trim().to_string()
}

fn normalize_lower(value: Option<&str>) -> String {
    normalize_text(value).to_lowercase()
}

fn parse_tracking_remote_name(tracking: Option<&str>) -> String {
    let normalized = normalize_text(tracking);
    if normalized.is_empty() {
        return String::new();
    }
    match normalized.find('/') {
        Some(idx) if idx > 0 => normalized[..idx].trim().to_string(),
        _ => String::new(),
    }
}

fn parse_tracking_branch_name(tracking: Option<&str>) -> String {
    let normalized = normalize_text(tracking);
    if normalized.is_empty() {
        return String::new();
    }
    match normalized.find('/') {
        Some(idx) if idx > 0 && idx < normalized.len() - 1 => normalized[idx + 1..].trim().to_string(),
        _ => String::new(),
    }
}

/// 向 collection 推入唯一值 (按 lowercase key 去重)。
fn push_unique(collection: &mut Vec<String>, value: &str) {
    let normalized = normalize_text(Some(value));
    if normalized.is_empty() {
        return;
    }
    let key = normalized.to_lowercase();
    if collection.iter().any(|item| item.to_lowercase() == key) {
        return;
    }
    collection.push(normalized);
}

/// 排序 remote 候选: [explicit, tracking, origin, upstream, rest]。
fn rank_remote_names(
    remote_names: &[String],
    explicit: &str,
    tracking: &str,
) -> Vec<String> {
    let mut ranked = Vec::new();
    push_unique(&mut ranked, explicit);
    if !tracking.is_empty() {
        push_unique(&mut ranked, tracking);
    }
    push_unique(&mut ranked, "origin");
    push_unique(&mut ranked, "upstream");
    for name in remote_names {
        push_unique(&mut ranked, name);
    }
    ranked
}

// ============================================================
// Source matcher (PR 候选排序)
// ============================================================

struct SourceMatcher {
    repo_rank: HashMap<String, usize>,
    owner_rank: HashMap<String, usize>,
}

impl SourceMatcher {
    fn build(source_candidates: &[RepoCandidate]) -> Self {
        let mut repo_rank = HashMap::new();
        let mut owner_rank = HashMap::new();

        for (index, candidate) in source_candidates.iter().enumerate() {
            let repo_key = normalize_repo_key(&candidate.repo.owner, &candidate.repo.repo);
            if !repo_key.is_empty() && !repo_rank.contains_key(&repo_key) {
                repo_rank.insert(repo_key, index);
            }
            let owner = normalize_lower(Some(&candidate.repo.owner));
            if !owner.is_empty() && !owner_rank.contains_key(&owner) {
                owner_rank.insert(owner, index);
            }
        }

        Self {
            repo_rank,
            owner_rank,
        }
    }

    fn get_head_owner(pr: &Value) -> String {
        // pr.head.repo.owner.login
        let repo_owner = pr
            .get("head")
            .and_then(|h| h.get("repo"))
            .and_then(|r| r.get("owner"))
            .and_then(|o| o.get("login"))
            .and_then(|v| v.as_str());
        if let Some(owner) = repo_owner {
            if !owner.is_empty() {
                return owner.to_string();
            }
        }
        // pr.head.user.login
        let user_owner = pr
            .get("head")
            .and_then(|h| h.get("user"))
            .and_then(|u| u.get("login"))
            .and_then(|v| v.as_str());
        if let Some(owner) = user_owner {
            if !owner.is_empty() {
                return owner.to_string();
            }
        }
        // pr.head.label → "owner:branch"
        let head_label = normalize_text(pr.get("head").and_then(|h| h.get("label")).and_then(|v| v.as_str()));
        if let Some(idx) = head_label.find(':') {
            if idx > 0 {
                return head_label[..idx].trim().to_string();
            }
        }
        String::new()
    }

    fn get_head_repo_key(pr: &Value, fallback_repo_name: &str) -> String {
        let repo_owner = pr
            .get("head")
            .and_then(|h| h.get("repo"))
            .and_then(|r| r.get("owner"))
            .and_then(|o| o.get("login"))
            .and_then(|v| v.as_str());
        let repo_name = pr
            .get("head")
            .and_then(|h| h.get("repo"))
            .and_then(|r| r.get("name"))
            .and_then(|v| v.as_str());
        if let (Some(owner), Some(name)) = (repo_owner, repo_name) {
            if !owner.is_empty() && !name.is_empty() {
                return normalize_repo_key(owner, name);
            }
        }
        // head.label fallback
        let head_label = normalize_text(pr.get("head").and_then(|h| h.get("label")).and_then(|v| v.as_str()));
        if let Some(idx) = head_label.find(':') {
            if idx > 0 {
                let label_owner = head_label[..idx].trim();
                if !label_owner.is_empty() && !fallback_repo_name.is_empty() {
                    return normalize_repo_key(label_owner, fallback_repo_name);
                }
            }
        }
        String::new()
    }

    fn matches(&self, pr: &Value, fallback_repo_name: &str) -> bool {
        let repo_key = Self::get_head_repo_key(pr, fallback_repo_name);
        if !repo_key.is_empty() && self.repo_rank.contains_key(&repo_key) {
            return true;
        }
        let owner = normalize_lower(Some(&Self::get_head_owner(pr)));
        !owner.is_empty() && self.owner_rank.contains_key(&owner)
    }

    /// 比较两个 PR 的 source ranking (越小越优)。
    fn compare(&self, left: &Value, right: &Value, fallback_repo_name: &str) -> i64 {
        let left_repo_key = Self::get_head_repo_key(left, fallback_repo_name);
        let right_repo_key = Self::get_head_repo_key(right, fallback_repo_name);
        let left_repo_score = self.repo_rank.get(&left_repo_key).copied().unwrap_or(usize::MAX);
        let right_repo_score = self.repo_rank.get(&right_repo_key).copied().unwrap_or(usize::MAX);
        if left_repo_score != right_repo_score {
            return left_repo_score as i64 - right_repo_score as i64;
        }

        let left_owner = normalize_lower(Some(&Self::get_head_owner(left)));
        let right_owner = normalize_lower(Some(&Self::get_head_owner(right)));
        let left_owner_score = self.owner_rank.get(&left_owner).copied().unwrap_or(usize::MAX);
        let right_owner_score = self.owner_rank.get(&right_owner).copied().unwrap_or(usize::MAX);
        left_owner_score as i64 - right_owner_score as i64
    }
}

// ============================================================
// Resolved target + candidate
// ============================================================

#[derive(Debug, Clone)]
struct RepoCandidate {
    repo: GitHubRepo,
    remote_name: String,
    priority: usize,
}

/// 解析结果。
pub struct PrStatusResult {
    pub repo: Option<GitHubRepo>,
    pub pr: Option<Value>,
    pub default_branch: Option<String>,
    pub resolved_remote_name: Option<String>,
}

// ============================================================
// Default branch cache (进程全局, 复用 repo metadata)
// ============================================================

async fn get_repo_default_branch(
    client: &GitHubClient,
    repo: &GitHubRepo,
) -> Result<Option<String>, GitHubApiError> {
    let repo_key = normalize_repo_key(&repo.owner, &repo.repo);
    if repo_key.is_empty() {
        return Ok(None);
    }

    // 先检查 repo metadata cache (避免重复 repos.get)
    let metadata = get_repo_metadata(client, repo).await?;
    if let Some(meta) = &metadata {
        let default_branch = meta
            .get("default_branch")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from);
        return Ok(default_branch);
    }

    Ok(None)
}

// ============================================================
// Search API disabled repos cache
// ============================================================

static SEARCH_API_DISABLED: Lazy<Mutex<HashMap<String, Instant>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// `searchFallbackPr` — 用 GitHub search API 查找 PR。
async fn search_fallback_pr(
    client: &GitHubClient,
    rate_limit: &RateLimitState,
    branch: &str,
    repo_names: &[String],
) -> Result<Option<(GitHubRepo, Value)>, GitHubApiError> {
    let repo_key = {
        let mut sorted: Vec<String> = repo_names.iter().map(|n| n.to_lowercase()).collect();
        sorted.sort();
        sorted.join(",")
    };

    // 检查 search API 是否最近被禁用
    {
        let cache = SEARCH_API_DISABLED.lock().unwrap();
        if let Some(&disabled_at) = cache.get(&repo_key) {
            if Instant::now().duration_since(disabled_at) < SEARCH_API_RETRY {
                return Ok(None);
            }
        }
    }

    let normalized_repo_names: HashSet<String> = repo_names
        .iter()
        .map(|n| normalize_lower(Some(n)))
        .filter(|s| !s.is_empty())
        .collect();

    for state in &["open", "closed"] {
        let query = format!("is:pr state:{} head:{}", state, branch);
        let response = match client.search_issues(&query, 20, 1).await {
            Ok(resp) => resp,
            Err(error) => {
                rate_limit.note_if_rate_limit(&error);
                if error.status == 403 {
                    SEARCH_API_DISABLED
                        .lock()
                        .unwrap()
                        .insert(repo_key.clone(), Instant::now());
                    return Ok(None);
                }
                if error.status == 404 {
                    continue;
                }
                return Err(error);
            }
        };

        // search 成功 → 清除禁用标志
        SEARCH_API_DISABLED.lock().unwrap().remove(&repo_key);

        let items = response
            .get("items")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        for item in &items {
            let parsed_repo = parse_repo_from_api_url(
                item.get("repository_url").and_then(|v| v.as_str()).unwrap_or(""),
            );
            let Some((repo_owner, repo_name)) = parsed_repo else { continue };

            if !normalized_repo_names.is_empty()
                && !normalized_repo_names.contains(&normalize_lower(Some(&repo_name)))
            {
                continue;
            }

            // 获取完整 PR 数据
            match client.pulls_get(&repo_owner, &repo_name, item.get("number").and_then(|v| v.as_u64()).unwrap_or(0)).await {
                Ok(pr) => {
                    let head_ref = normalize_text(pr.get("head").and_then(|h| h.get("ref")).and_then(|v| v.as_str()));
                    if head_ref != branch {
                        continue;
                    }
                    return Ok(Some((
                        GitHubRepo {
                            owner: repo_owner.clone(),
                            repo: repo_name.clone(),
                            url: format!("https://github.com/{}/{}", repo_owner, repo_name),
                        },
                        pr,
                    )));
                }
                Err(error) => {
                    if error.status == 403 || error.status == 404 {
                        continue;
                    }
                    return Err(error);
                }
            }
        }
    }

    Ok(None)
}

/// 解析 `https://api.github.com/repos/OWNER/REPO` → `{owner, repo}`。
fn parse_repo_from_api_url(value: &str) -> Option<(String, String)> {
    let normalized = normalize_text(Some(value));
    if normalized.is_empty() {
        return None;
    }
    // 简单解析: 找 /repos/OWNER/REPO
    let parts: Vec<&str> = normalized.split('/').filter(|s| !s.is_empty()).collect();
    // 期望: ["api.github.com", "repos", "OWNER", "REPO"]
    let repos_idx = parts.iter().position(|s| *s == "repos")?;
    if parts.len() < repos_idx + 3 {
        return None;
    }
    let owner = parts[repos_idx + 1];
    let repo = parts[repos_idx + 2];
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some((owner.to_string(), repo.to_string()))
}

/// `findFirstMatchingPr` — 在 target repo 中找到匹配的 PR。
async fn find_first_matching_pr(
    client: &GitHubClient,
    rate_limit: &RateLimitState,
    target: &RepoCandidate,
    branch: &str,
    source_candidates: &[RepoCandidate],
) -> Result<Option<Value>, GitHubApiError> {
    let matcher = SourceMatcher::build(source_candidates);
    let source_owners: Vec<String> = {
        let mut owners = Vec::new();
        for candidate in source_candidates {
            push_unique(&mut owners, &candidate.repo.owner);
        }
        owners
    };

    for state in &["open", "closed"] {
        // 1. 按 owner:branch 查找 cross-repo PR
        for owner in &source_owners {
            let head = format!("{}:{}", owner, branch);
            let candidates = safe_list_pulls(client, rate_limit, &target.repo.owner, &target.repo.repo, state, Some(&head)).await?;
            let pr = pick_preferred(&candidates, branch, &matcher, &target.repo.repo);
            if let Some(pr) = pr {
                return Ok(Some(pr));
            }
        }

        // 2. fallback: 列出所有 PR
        let candidates = safe_list_pulls(client, rate_limit, &target.repo.owner, &target.repo.repo, state, None).await?;
        let pr = pick_preferred(&candidates, branch, &matcher, &target.repo.repo);
        if let Some(pr) = pr {
            return Ok(Some(pr));
        }
    }

    Ok(None)
}

/// 从 PR 候选列表中选出最优匹配。
fn pick_preferred(
    prs: &[Value],
    branch: &str,
    matcher: &SourceMatcher,
    fallback_repo_name: &str,
) -> Option<Value> {
    let mut filtered: Vec<&Value> = prs
        .iter()
        .filter(|pr| {
            let head_ref = normalize_text(pr.get("head").and_then(|h| h.get("ref")).and_then(|v| v.as_str()));
            head_ref == branch
        })
        .filter(|pr| matcher.matches(pr, fallback_repo_name))
        .collect();

    if filtered.is_empty() {
        return None;
    }

    filtered.sort_by(|a, b| {
        let cmp = matcher.compare(a, b, fallback_repo_name);
        cmp.cmp(&0)
    });

    filtered.first().cloned().cloned()
}

/// `safeListPulls` — 安全列出 PRs (403/404 返回空)。
async fn safe_list_pulls(
    client: &GitHubClient,
    rate_limit: &RateLimitState,
    owner: &str,
    repo: &str,
    state: &str,
    head: Option<&str>,
) -> Result<Vec<Value>, GitHubApiError> {
    match client.pulls_list(owner, repo, state, head).await {
        Ok(prs) => Ok(prs),
        Err(error) => {
            rate_limit.note_if_rate_limit(&error);
            if error.status == 404 || error.status == 403 {
                return Ok(Vec::new());
            }
            Err(error)
        }
    }
}

/// 展开候选 remote 列表为 repo network (parent/source)。
async fn expand_repo_network(
    client: &GitHubClient,
    candidates: &[RepoCandidate],
) -> Result<Vec<RepoCandidate>, GitHubApiError> {
    let mut expanded: Vec<RepoCandidate> = Vec::new();
    let mut seen_keys: HashSet<String> = HashSet::new();

    // 并发获取 metadata
    let mut metadata_results = Vec::new();
    for candidate in candidates {
        let metadata = get_repo_metadata(client, &candidate.repo).await?;
        metadata_results.push((candidate.clone(), metadata));
    }

    for (candidate, metadata) in metadata_results {
        let Some(metadata) = metadata else { continue };

        let repo_key = normalize_repo_key(&candidate.repo.owner, &candidate.repo.repo);
        if !repo_key.is_empty() && !seen_keys.contains(&repo_key) {
            seen_keys.insert(repo_key.clone());
            expanded.push(candidate.clone());
        }

        // parent
        if let Some(parent) = metadata.get("parent") {
            if let (Some(owner_login), Some(name)) = (
                parent.get("owner").and_then(|o| o.get("login")).and_then(|v| v.as_str()),
                parent.get("name").and_then(|v| v.as_str()),
            ) {
                let key = normalize_repo_key(owner_login, name);
                if !key.is_empty() && !seen_keys.contains(&key) {
                    seen_keys.insert(key);
                    expanded.push(RepoCandidate {
                        repo: GitHubRepo {
                            owner: owner_login.to_string(),
                            repo: name.to_string(),
                            url: parent
                                .get("html_url")
                                .and_then(|v| v.as_str())
                                .map(String::from)
                                .unwrap_or_else(|| format!("https://github.com/{}/{}", owner_login, name)),
                        },
                        remote_name: candidate.remote_name.clone(),
                        priority: candidate.priority + 1,
                    });
                }
            }
        }

        // source
        if let Some(source) = metadata.get("source") {
            if let (Some(owner_login), Some(name)) = (
                source.get("owner").and_then(|o| o.get("login")).and_then(|v| v.as_str()),
                source.get("name").and_then(|v| v.as_str()),
            ) {
                let key = normalize_repo_key(owner_login, name);
                if !key.is_empty() && !seen_keys.contains(&key) {
                    seen_keys.insert(key);
                    expanded.push(RepoCandidate {
                        repo: GitHubRepo {
                            owner: owner_login.to_string(),
                            repo: name.to_string(),
                            url: source
                                .get("html_url")
                                .and_then(|v| v.as_str())
                                .map(String::from)
                                .unwrap_or_else(|| format!("https://github.com/{}/{}", owner_login, name)),
                        },
                        remote_name: candidate.remote_name.clone(),
                        priority: candidate.priority + 2,
                    });
                }
            }
        }
    }

    expanded.sort_by_key(|c| c.priority);
    Ok(expanded)
}

/// `resolveGitHubPrStatus` — 主入口。
pub async fn resolve_github_pr_status(
    client: &GitHubClient,
    rate_limit: &RateLimitState,
    directory: &str,
    branch: &str,
    remote_name: &str,
) -> Result<PrStatusResult, GitHubApiError> {
    // 1. directory 存在检查
    if !std::path::Path::new(directory).exists() {
        return Ok(PrStatusResult {
            repo: None,
            pr: None,
            default_branch: None,
            resolved_remote_name: None,
        });
    }

    let normalized_branch = normalize_text(Some(branch));
    let normalized_remote_name = normalize_text(Some(remote_name));
    let normalized_remote_name = if normalized_remote_name.is_empty() {
        "origin".to_string()
    } else {
        normalized_remote_name
    };

    // 2. 获取 git status + remotes
    let status = crate::git::status::get_status(directory, Default::default())
        .await
        .ok();
    let remotes = crate::git::remote::get_remotes(directory)
        .await
        .ok();

    let tracking = status
        .as_ref()
        .and_then(|s| s.get("tracking"))
        .and_then(|v| v.as_str());
    let tracking_remote_name = parse_tracking_remote_name(tracking);
    let tracking_branch_name = parse_tracking_branch_name(tracking);

    let mut branch_candidates = Vec::new();
    push_unique(&mut branch_candidates, &normalized_branch);
    if !tracking_branch_name.is_empty() {
        push_unique(&mut branch_candidates, &tracking_branch_name);
    }

    let remote_name_list: Vec<String> = remotes
        .as_ref()
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|r| r.get("name").and_then(|v| v.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let ranked_remote_names = rank_remote_names(&remote_name_list, &normalized_remote_name, &tracking_remote_name);

    // 3. 并发解析 remote 候选
    let mut resolved_targets: Vec<RepoCandidate> = Vec::new();
    let mut seen_repo_keys: HashSet<String> = HashSet::new();
    for (index, remote_name) in ranked_remote_names.iter().enumerate() {
        let (repo, _) = crate::github::repo::resolve_repo_from_directory(directory, remote_name).await;
        if let Some(repo) = repo {
            let repo_key = normalize_repo_key(&repo.owner, &repo.repo);
            if !repo_key.is_empty() && !seen_repo_keys.contains(&repo_key) {
                seen_repo_keys.insert(repo_key);
                resolved_targets.push(RepoCandidate {
                    repo,
                    remote_name: remote_name.clone(),
                    priority: index,
                });
            }
        }
    }

    if resolved_targets.is_empty() {
        return Ok(PrStatusResult {
            repo: None,
            pr: None,
            default_branch: None,
            resolved_remote_name: None,
        });
    }

    // 4. 展开 repo network
    let expanded_targets = expand_repo_network(client, &resolved_targets).await?;
    if expanded_targets.is_empty() {
        return Ok(PrStatusResult {
            repo: None,
            pr: None,
            default_branch: None,
            resolved_remote_name: None,
        });
    }

    let source_candidates = expanded_targets.clone();
    let mut fallback_repo = expanded_targets[0].repo.clone();
    let mut fallback_remote_name = expanded_targets[0].remote_name.clone();
    let mut fallback_default_branch = get_repo_default_branch(client, &fallback_repo).await?;

    // 5. 按优先级遍历
    for target in &expanded_targets {
        let default_branch = get_repo_default_branch(client, &target.repo).await?;
        if fallback_repo.owner.is_empty() {
            fallback_repo = target.repo.clone();
            fallback_remote_name = target.remote_name.clone();
            fallback_default_branch = default_branch.clone();
        }

        let has_cross_repo_source = source_candidates.iter().any(|c| {
            normalize_repo_key(&c.repo.owner, &c.repo.repo)
                != normalize_repo_key(&target.repo.owner, &target.repo.repo)
        });

        for candidate_branch in &branch_candidates {
            // 跳过 default branch (除非有 cross-repo source)
            if let Some(ref db) = default_branch {
                if db == candidate_branch && !has_cross_repo_source {
                    continue;
                }
            }

            let pr = find_first_matching_pr(
                client,
                rate_limit,
                target,
                candidate_branch,
                &source_candidates,
            )
            .await?;

            if let Some(pr) = pr {
                return Ok(PrStatusResult {
                    repo: Some(target.repo.clone()),
                    pr: Some(pr),
                    default_branch,
                    resolved_remote_name: Some(target.remote_name.clone()),
                });
            }
        }
    }

    // 6. search fallback
    let repo_names: Vec<String> = expanded_targets
        .iter()
        .map(|t| t.repo.repo.clone())
        .collect();
    for candidate_branch in &branch_candidates {
        let fallback_search =
            search_fallback_pr(client, rate_limit, candidate_branch, &repo_names).await?;
        if let Some((repo, pr)) = fallback_search {
            let default_branch = get_repo_default_branch(client, &repo).await?;
            return Ok(PrStatusResult {
                repo: Some(repo),
                pr: Some(pr),
                default_branch,
                resolved_remote_name: None,
            });
        }
    }

    Ok(PrStatusResult {
        repo: Some(fallback_repo),
        pr: None,
        default_branch: fallback_default_branch,
        resolved_remote_name: Some(fallback_remote_name),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tracking_remote_basic() {
        assert_eq!(parse_tracking_remote_name(Some("origin/main")), "origin");
        assert_eq!(parse_tracking_remote_name(Some("upstream/feature/x")), "upstream");
    }

    #[test]
    fn parse_tracking_remote_empty() {
        assert_eq!(parse_tracking_remote_name(None), "");
        assert_eq!(parse_tracking_remote_name(Some("")), "");
        assert_eq!(parse_tracking_remote_name(Some("nonsense")), "");
    }

    #[test]
    fn parse_tracking_branch_basic() {
        assert_eq!(parse_tracking_branch_name(Some("origin/main")), "main");
        assert_eq!(parse_tracking_branch_name(Some("upstream/feature/x")), "feature/x");
    }

    #[test]
    fn parse_tracking_branch_empty() {
        assert_eq!(parse_tracking_branch_name(None), "");
        assert_eq!(parse_tracking_branch_name(Some("origin")), "");
    }

    #[test]
    fn rank_remote_names_basic() {
        let remotes = vec!["origin".to_string(), "upstream".to_string(), "fork".to_string()];
        let ranked = rank_remote_names(&remotes, "explicit", "tracking");
        assert_eq!(ranked[0], "explicit");
        assert_eq!(ranked[1], "tracking");
        assert_eq!(ranked[2], "origin");
        assert_eq!(ranked[3], "upstream");
        assert!(ranked.contains(&"fork".to_string()));
    }

    #[test]
    fn rank_remote_names_no_tracking() {
        let remotes = vec!["origin".to_string()];
        let ranked = rank_remote_names(&remotes, "origin", "");
        assert_eq!(ranked[0], "origin");
        // tracking 空, 不推入
        assert_eq!(ranked.len(), 2); // origin + upstream
    }

    #[test]
    fn push_unique_dedup() {
        let mut collection = Vec::new();
        push_unique(&mut collection, "origin");
        push_unique(&mut collection, "Origin"); // 不同大小写 → 去重
        push_unique(&mut collection, "upstream");
        assert_eq!(collection, vec!["origin", "upstream"]);
    }

    #[test]
    fn parse_repo_from_api_url_basic() {
        let (owner, repo) = parse_repo_from_api_url(
            "https://api.github.com/repos/octocat/hello-world",
        )
        .unwrap();
        assert_eq!(owner, "octocat");
        assert_eq!(repo, "hello-world");
    }

    #[test]
    fn parse_repo_from_api_url_invalid() {
        assert!(parse_repo_from_api_url("").is_none());
        assert!(parse_repo_from_api_url("https://example.com/foo/bar").is_none());
    }
}
