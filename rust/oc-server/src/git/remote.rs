//! Remote 操作 — pull/push/fetch/remotes + push upstream fallback。
//!
//! 移植自 Node `service.js` 的 remote/push/pull/fetch 相关函数。

use serde_json::{json, Value};

use crate::git::context::RepoContext;
use crate::git::parsing::parse_remotes_verbose;
use crate::git::runner::GitRunner;

/// 检测 push 错误是否像 "no upstream", 与 Node `looksLikeMissingUpstream` 对齐。
pub fn looks_like_missing_upstream(message: &str) -> bool {
    let lower = message.to_lowercase();
    lower.contains("has no upstream")
        || lower.contains("no upstream")
        || lower.contains("set-upstream")
        || lower.contains("set upstream")
        || (lower.contains("upstream") && lower.contains("push") && lower.contains("-u"))
}

/// Push, 与 Node `push` 对齐 (包含 upstream fallback)。
pub async fn push(directory: &str, options: &Value) -> oc_core::Result<Value> {
    let repo_ctx = RepoContext::create(directory).await?;
    let repo_root = &repo_ctx.repo_root;

    let remote = options
        .get("remote")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let branch = options
        .get("branch")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string());

    // Case 1: 无 remote 无 branch → git push
    if remote.is_empty() && branch.is_none() {
        let result = GitRunner::run(repo_root, &["push"]).await;
        if result.success {
            return Ok(json!({
                "success": true,
                "pushed": [],
                "repo": directory,
                "ref": null,
            }));
        }

        // 检查是否是 missing upstream
        let error_text = result.stderr_text();
        if !looks_like_missing_upstream(&error_text) {
            return Err(oc_core::Error::Internal(format!(
                "Failed to push: {error_text}"
            )));
        }

        // fallback: 获取 branch 和 remote, 带 --set-upstream
        return push_with_upstream_fallback(repo_root, directory, None).await;
    }

    let remote_name = if remote.is_empty() { "origin".to_string() } else { remote };

    // Case 2: 有 remote 无 branch → 检查是否需要 set-upstream
    if branch.is_none() {
        // 读 status 获取 current branch
        let status_result = GitRunner::run(repo_root, &["status", "-uall", "--porcelain=v1", "-b"]).await;
        let parsed = crate::git::parsing::parse_status_porcelain(&status_result.stdout);

        if let Some(current_branch) = &parsed.current {
            if parsed.tracking.is_none() {
                // 分支无 upstream, 带 --set-upstream push
                let result = GitRunner::run(
                    repo_root,
                    &["push", "--set-upstream", &remote_name, current_branch],
                )
                .await;

                if result.success {
                    return Ok(json!({
                        "success": true,
                        "pushed": [{ "branch": current_branch, "remote": remote_name }],
                        "repo": directory,
                        "ref": format!("{remote_name}/{current_branch}"),
                    }));
                }

                let error_text = result.stderr_text();
                return Err(oc_core::Error::Internal(format!(
                    "Failed to push: {error_text}"
                )));
            }
        }
    }

    // Case 3: 正常 push
    let branch_str = branch.as_deref().unwrap_or("");
    let result = if branch_str.is_empty() {
        GitRunner::run(repo_root, &["push", &remote_name]).await
    } else {
        GitRunner::run(repo_root, &["push", &remote_name, branch_str]).await
    };

    if result.success {
        return Ok(json!({
            "success": true,
            "pushed": if branch_str.is_empty() {
                json!([])
            } else {
                json!([{ "branch": branch_str, "remote": remote_name }])
            },
            "repo": directory,
            "ref": if branch_str.is_empty() {
                json!(null)
            } else {
                json!(format!("{remote_name}/{branch_str}"))
            },
        }));
    }

    // Last-resort: missing upstream fallback
    let error_text = result.stderr_text();
    if !looks_like_missing_upstream(&error_text) {
        return Err(oc_core::Error::Internal(format!(
            "Failed to push: {error_text}"
        )));
    }

    push_with_upstream_fallback(repo_root, directory, Some(&remote_name)).await
}

/// Push upstream fallback, 与 Node push 内联逻辑对齐。
async fn push_with_upstream_fallback(
    repo_root: &std::path::Path,
    directory: &str,
    remote_hint: Option<&str>,
) -> oc_core::Result<Value> {
    // 读 status 获取 branch
    let status_result = GitRunner::run(repo_root, &["status", "-uall", "--porcelain=v1", "-b"]).await;
    let parsed = crate::git::parsing::parse_status_porcelain(&status_result.stdout);

    let branch = match &parsed.current {
        Some(b) if !b.is_empty() => b.clone(),
        _ => {
            return Err(oc_core::Error::Internal(
                "Failed to push: missing branch name for upstream setup".to_string(),
            ));
        }
    };

    // 获取 remotes 列表
    let remotes_result = GitRunner::run(repo_root, &["remote"]).await;
    let remotes: Vec<&str> = remotes_result
        .stdout
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect();

    let remote_name = remote_hint
        .map(|s| s.to_string())
        .or_else(|| {
            remotes
                .iter()
                .find(|&&r| r == "origin")
                .map(|s| s.to_string())
                .or_else(|| remotes.first().map(|s| s.to_string()))
        })
        .unwrap_or_else(|| "origin".to_string());

    let result = GitRunner::run(
        repo_root,
        &["push", "--set-upstream", &remote_name, &branch],
    )
    .await;

    if result.success {
        return Ok(json!({
            "success": true,
            "pushed": [{ "branch": branch, "remote": remote_name }],
            "repo": directory,
            "ref": format!("{remote_name}/{branch}"),
        }));
    }

    Err(oc_core::Error::Internal(format!(
        "Failed to push (including upstream fallback): {}",
        result.stderr_text()
    )))
}

/// Pull, 与 Node `pull` 对齐。
pub async fn pull(directory: &str, options: &Value) -> oc_core::Result<Value> {
    let repo_ctx = RepoContext::create(directory).await?;
    let repo_root = &repo_ctx.repo_root;

    let remote = options
        .get("remote")
        .and_then(|v| v.as_str())
        .unwrap_or("origin")
        .to_string();
    let branch = options
        .get("branch")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let result = match &branch {
        Some(b) => GitRunner::run(repo_root, &["pull", &remote, b]).await,
        None => GitRunner::run(repo_root, &["pull", &remote]).await,
    };

    if !result.success {
        return Err(oc_core::Error::Internal(format!(
            "Failed to pull: {}",
            result.stderr_text()
        )));
    }

    Ok(json!({ "success": true }))
}

/// Fetch, 与 Node `fetch` 对齐。
pub async fn fetch(directory: &str, options: &Value) -> oc_core::Result<Value> {
    let repo_ctx = RepoContext::create(directory).await?;
    let repo_root = &repo_ctx.repo_root;

    let remote = options
        .get("remote")
        .and_then(|v| v.as_str())
        .unwrap_or("origin")
        .to_string();
    let branch = options
        .get("branch")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let result = match &branch {
        Some(b) => GitRunner::run(repo_root, &["fetch", &remote, b]).await,
        None => GitRunner::run(repo_root, &["fetch", &remote]).await,
    };

    if !result.success {
        return Err(oc_core::Error::Internal(format!(
            "Failed to fetch: {}",
            result.stderr_text()
        )));
    }

    Ok(json!({ "success": true }))
}

/// 获取 remotes 列表, 与 Node `getRemotes` 对齐。
pub async fn get_remotes(directory: &str) -> oc_core::Result<Value> {
    let repo_ctx = RepoContext::create(directory).await?;
    let repo_root = &repo_ctx.repo_root;

    let result = GitRunner::run(repo_root, &["remote", "-v"]).await;

    let remotes = parse_remotes_verbose(&result.stdout);
    let remote_list: Vec<Value> = remotes
        .iter()
        .map(|r| {
            json!({
                "name": r.name,
                "refs": {
                    "fetch": r.fetch_url,
                    "push": r.push_url,
                },
            })
        })
        .collect();

    Ok(json!(remote_list))
}

/// 删除 remote branch, 与 Node `deleteRemoteBranch` 对齐。
pub async fn delete_remote_branch(
    directory: &str,
    branch: &str,
    remote: Option<&str>,
) -> oc_core::Result<Value> {
    let repo_ctx = RepoContext::create(directory).await?;
    let repo_root = &repo_ctx.repo_root;

    let branch_name = branch
        .strip_prefix("refs/heads/")
        .unwrap_or(branch);
    let remote_name = remote.unwrap_or("origin");

    let result = GitRunner::run(
        repo_root,
        &["push", remote_name, &format!(":{branch_name}")],
    )
    .await;

    if !result.success {
        return Err(oc_core::Error::Internal(format!(
            "Failed to delete remote branch: {}",
            result.stderr_text()
        )));
    }

    Ok(json!({ "success": true }))
}

/// 删除 remote, 与 Node `removeRemote` 对齐。
pub async fn remove_remote(directory: &str, remote_name: &str) -> oc_core::Result<Value> {
    if remote_name == "origin" {
        return Err(oc_core::Error::BadRequest(
            "Cannot remove origin remote".to_string(),
        ));
    }

    let repo_ctx = RepoContext::create(directory).await?;
    let repo_root = &repo_ctx.repo_root;

    let result = GitRunner::run(repo_root, &["remote", "remove", remote_name]).await;

    if !result.success {
        return Err(oc_core::Error::Internal(format!(
            "Failed to remove remote: {}",
            result.stderr_text()
        )));
    }

    Ok(json!({ "success": true }))
}

/// 获取 remote URL, 与 Node `getRemoteUrl` 对齐。
pub async fn get_remote_url(directory: &str, remote: &str) -> oc_core::Result<String> {
    let dir = crate::git::paths::normalize_directory_path(directory);
    let dir_path = std::path::PathBuf::from(&dir);

    let result = GitRunner::run(&dir_path, &["remote", "get-url", remote]).await;

    if !result.success {
        return Err(oc_core::Error::Internal(format!(
            "Failed to get remote url: {}",
            result.stderr_text()
        )));
    }

    Ok(result.stdout_text())
}
