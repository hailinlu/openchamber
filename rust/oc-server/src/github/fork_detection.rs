//! GitHub fork 检测 + repo network 解析。
//!
//! 移植自 `packages/web/server/lib/github/repo/fork-detection.js` (102 行)。
//!
//! 解析 repo 的 fork 网络: origin + parent + source (upstream)。
//! repo 元数据缓存: TTL 5min, max 200 entries。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use once_cell::sync::Lazy;
use serde_json::{json, Value};

use crate::github::client::{GitHubApiError, GitHubClient};
use crate::github::repo::GitHubRepo;

const REPO_METADATA_TTL: Duration = Duration::from_secs(5 * 60);
const REPO_METADATA_MAX_ENTRIES: usize = 200;

/// repo network 条目 (origin + upstream parent/source)。
#[derive(Debug, Clone)]
pub struct RepoNetworkEntry {
    pub owner: String,
    pub repo: String,
    /// 仓库 HTML URL (保留用于调试/扩展; 当前路由响应不包含此字段)。
    #[allow(dead_code)]
    pub url: String,
    pub source: String, // "origin" 或 "upstream"
}

// ============================================================
// Repo metadata cache (进程全局)
// ============================================================

struct RepoMetadataCacheEntry {
    data: Option<Value>,
    fetched_at: Instant,
}

static REPO_METADATA_CACHE: Lazy<Mutex<HashMap<String, RepoMetadataCacheEntry>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// `normalizeRepoKey` — owner/repo 小写规范化。
pub fn normalize_repo_key(owner: &str, repo: &str) -> String {
    let o = owner.trim().to_lowercase();
    let r = repo.trim().to_lowercase();
    if o.is_empty() || r.is_empty() {
        return String::new();
    }
    format!("{}/{}", o, r)
}

/// 从缓存设置 repo 元数据 (LRU eviction)。
fn set_repo_metadata_cache(repo_key: &str, data: Option<Value>) {
    let mut cache = REPO_METADATA_CACHE.lock().unwrap();
    if cache.len() >= REPO_METADATA_MAX_ENTRIES && !cache.contains_key(repo_key) {
        // evict 最旧的 entry
        if let Some((oldest_key, _)) = cache
            .iter()
            .min_by_key(|(_, entry)| entry.fetched_at)
            .map(|(k, v)| (k.clone(), v.fetched_at))
        {
            cache.remove(&oldest_key);
        }
    }
    cache.insert(
        repo_key.to_string(),
        RepoMetadataCacheEntry {
            data,
            fetched_at: Instant::now(),
        },
    );
}

/// 获取 repo 元数据 (带缓存), 对应 Node `getRepoMetadata`。
pub async fn get_repo_metadata(
    client: &GitHubClient,
    repo: &GitHubRepo,
) -> Result<Option<Value>, GitHubApiError> {
    let repo_key = normalize_repo_key(&repo.owner, &repo.repo);
    if repo_key.is_empty() {
        return Ok(None);
    }

    // 检查缓存
    {
        let cache = REPO_METADATA_CACHE.lock().unwrap();
        if let Some(entry) = cache.get(&repo_key) {
            if Instant::now().duration_since(entry.fetched_at) < REPO_METADATA_TTL {
                return Ok(entry.data.clone());
            }
        }
    }

    // 缓存 miss → fetch
    match client.repos_get(&repo.owner, &repo.repo).await {
        Ok(data) => {
            set_repo_metadata_cache(&repo_key, Some(data.clone()));
            Ok(Some(data))
        }
        Err(error) => {
            if error.status == 403 || error.status == 404 {
                set_repo_metadata_cache(&repo_key, None);
                return Ok(None);
            }
            Err(error)
        }
    }
}

/// `resolveRepoNetwork` — 解析 repo network (origin + parent/source)。
///
/// 返回:
/// - `None`: 不是 fork (或无法解析 repo)
/// - `Some(vec)`: fork 网络 (origin 在前, upstream 在后)
pub async fn resolve_repo_network(
    client: &GitHubClient,
    directory: &str,
    remote_name: &str,
) -> Result<Option<Vec<RepoNetworkEntry>>, GitHubApiError> {
    let (repo, _) = crate::github::repo::resolve_repo_from_directory(directory, remote_name).await;
    let Some(repo) = repo else {
        return Ok(None);
    };

    let metadata = get_repo_metadata(client, &repo).await?;
    let Some(metadata) = metadata else {
        // 无法获取元数据 → 返回只有 origin 的列表
        return Ok(Some(vec![RepoNetworkEntry {
            owner: repo.owner.clone(),
            repo: repo.repo.clone(),
            url: repo.url.clone(),
            source: "origin".to_string(),
        }]));
    };

    let mut result = vec![RepoNetworkEntry {
        owner: repo.owner.clone(),
        repo: repo.repo.clone(),
        url: repo.url.clone(),
        source: "origin".to_string(),
    }];
    let mut seen_keys: std::collections::HashSet<String> =
        std::collections::HashSet::from([normalize_repo_key(&repo.owner, &repo.repo)]);

    // parent
    if let Some(parent) = metadata.get("parent") {
        if let (Some(owner_login), Some(name)) = (
            parent.get("owner").and_then(|o| o.get("login")).and_then(|v| v.as_str()),
            parent.get("name").and_then(|v| v.as_str()),
        ) {
            let key = normalize_repo_key(owner_login, name);
            if !key.is_empty() && !seen_keys.contains(&key) {
                seen_keys.insert(key);
                result.push(RepoNetworkEntry {
                    owner: owner_login.to_string(),
                    repo: name.to_string(),
                    url: parent
                        .get("html_url")
                        .and_then(|v| v.as_str())
                        .map(String::from)
                        .unwrap_or_else(|| format!("https://github.com/{}/{}", owner_login, name)),
                    source: "upstream".to_string(),
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
                result.push(RepoNetworkEntry {
                    owner: owner_login.to_string(),
                    repo: name.to_string(),
                    url: source
                        .get("html_url")
                        .and_then(|v| v.as_str())
                        .map(String::from)
                        .unwrap_or_else(|| format!("https://github.com/{}/{}", owner_login, name)),
                    source: "upstream".to_string(),
                });
            }
        }
    }

    // 如果只有 origin (没有 parent/source), 返回 None
    if result.len() == 1 {
        return Ok(None);
    }

    Ok(Some(result))
}

/// 将 GitHubRepo 转换为 JSON (owner/repo/url)。
pub fn repo_to_json(repo: &GitHubRepo) -> Value {
    json!({
        "owner": repo.owner,
        "repo": repo.repo,
        "url": repo.url,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_repo_key_basic() {
        assert_eq!(normalize_repo_key("Octocat", "Hello-World"), "octocat/hello-world");
    }

    #[test]
    fn normalize_repo_key_empty() {
        assert_eq!(normalize_repo_key("", "repo"), "");
        assert_eq!(normalize_repo_key("owner", ""), "");
        assert_eq!(normalize_repo_key("  ", "  "), "");
    }

    #[test]
    fn normalize_repo_key_trims() {
        assert_eq!(normalize_repo_key("  Owner  ", "  Repo  "), "owner/repo");
    }
}
