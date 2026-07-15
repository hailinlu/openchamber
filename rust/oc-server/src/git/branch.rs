//! Branch 操作 — list/create/delete/rename/checkout。
//!
//! 移植自 Node `service.js` 的 branch 相关函数。

use std::path::Path;

use serde_json::{json, Value};

use crate::git::context::RepoContext;
use crate::git::runner::GitRunner;

/// 获取分支列表, 与 Node `getBranches` 对齐。
pub async fn get_branches(directory: &str) -> oc_core::Result<Value> {
    let repo_ctx = RepoContext::create(directory).await?;
    let repo_root = &repo_ctx.repo_root;

    // 本地分支列表
    let local_result = GitRunner::run(repo_root, &["branch", "--list"]).await;

    let mut all_branches = Vec::new();
    let mut current: Option<String> = None;

    for line in local_result.stdout.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }

        // 当前分支以 "* " 开头
        let (is_current, name) = if let Some(name) = line.strip_prefix("* ") {
            (true, name.trim().to_string())
        } else {
            // 分支名可能有前导空格 (非当前分支)
            (false, line.trim().to_string())
        };

        if name.is_empty() || name.starts_with('(') {
            // 跳过 "(detached from ...)"
            continue;
        }

        if is_current {
            current = Some(name.clone());
        }
        all_branches.push(name);
    }

    // 远程分支列表
    let remote_result = GitRunner::run(repo_root, &["branch", "--list", "-r"]).await;
    let remote_branches: Vec<String> = remote_result
        .stdout
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty() && !l.starts_with("-> "))
        .collect();

    // 过滤活跃远程分支 (通过 ls-remote 验证)
    let active_remote_branches = filter_active_remote_branches(repo_root, &remote_branches).await;

    // 合并本地 + 活跃远程
    let mut filtered_all = all_branches.clone();
    filtered_all.extend(active_remote_branches.iter().cloned());

    Ok(json!({
        "all": filtered_all,
        "current": current,
        "branches": {}, // branches map — UI 主要用 all 和 current
    }))
}

/// 过滤活跃远程分支, 与 Node `filterActiveRemoteBranches` 对齐。
///
/// 通过 `git ls-remote --heads <remote>` 验证远程分支是否仍存在。
async fn filter_active_remote_branches(
    repo_root: &Path,
    remote_branches: &[String],
) -> Vec<String> {
    // 获取所有 remote
    let remotes_result = GitRunner::run(repo_root, &["remote"]).await;
    let remotes: Vec<String> = remotes_result
        .stdout
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();

    // 对每个 remote, 查询活跃分支
    let mut branches_by_remote: std::collections::HashMap<String, std::collections::HashSet<String>> =
        std::collections::HashMap::new();

    for remote in &remotes {
        let ls_result = GitRunner::run(repo_root, &["ls-remote", "--heads", remote]).await;
        if !ls_result.success {
            continue;
        }

        let mut actual = std::collections::HashSet::new();
        for line in ls_result.stdout.lines() {
            if line.contains("\trefs/heads/") {
                if let Some(branch) = line.split('\t').nth(1) {
                    let branch_name = branch.replace("refs/heads/", "");
                    actual.insert(branch_name);
                }
            }
        }
        branches_by_remote.insert(remote.clone(), actual);
    }

    // 过滤
    let mut result = Vec::new();
    for remote_branch in remote_branches {
        // 格式: remotes/<remote>/<branch>
        let stripped = match remote_branch.strip_prefix("remotes/") {
            Some(s) => s,
            None => continue,
        };

        let parts: Vec<&str> = stripped.splitn(2, '/').collect();
        if parts.len() < 2 {
            continue;
        }

        let remote_name = parts[0];
        let branch_name = parts[1];

        if let Some(actual) = branches_by_remote.get(remote_name) {
            if actual.contains(branch_name) {
                result.push(remote_branch.clone());
            }
        }
    }

    // 如果过滤失败 (空), 返回原始列表 (与 Node 一致)
    if result.is_empty() && !remote_branches.is_empty() {
        return remote_branches.to_vec();
    }

    result
}

/// 创建并切换到新分支, 与 Node `createBranch` 对齐。
pub async fn create_branch(
    directory: &str,
    branch_name: &str,
    start_point: Option<&str>,
) -> oc_core::Result<Value> {
    let repo_ctx = RepoContext::create(directory).await?;
    let repo_root = &repo_ctx.repo_root;

    let sp = start_point.unwrap_or("HEAD");

    let result = GitRunner::run(repo_root, &["checkout", "-b", branch_name, sp]).await;

    if !result.success {
        return Err(oc_core::Error::Internal(format!(
            "Failed to create branch: {}",
            result.stderr_text()
        )));
    }

    Ok(json!({ "success": true, "branch": branch_name }))
}

/// 切换分支, 与 Node `checkoutBranch` 对齐。
pub async fn checkout_branch(directory: &str, branch_name: &str) -> oc_core::Result<Value> {
    let repo_ctx = RepoContext::create(directory).await?;
    let repo_root = &repo_ctx.repo_root;

    let result = GitRunner::run(repo_root, &["checkout", branch_name]).await;

    if !result.success {
        return Err(oc_core::Error::Internal(format!(
            "Failed to checkout branch: {}",
            result.stderr_text()
        )));
    }

    Ok(json!({ "success": true, "branch": branch_name }))
}

/// 切换到 commit (detached HEAD), 与 Node `checkoutCommit` 对齐。
pub async fn checkout_commit(directory: &str, hash: &str) -> oc_core::Result<Value> {
    if !crate::git::paths::is_valid_commit_hash(hash) {
        return Err(oc_core::Error::BadRequest("Invalid commit hash".to_string()));
    }

    let repo_ctx = RepoContext::create(directory).await?;
    let repo_root = &repo_ctx.repo_root;

    let result = GitRunner::run(repo_root, &["checkout", hash]).await;

    if !result.success {
        return Err(oc_core::Error::Internal(format!(
            "Failed to checkout commit: {}",
            result.stderr_text()
        )));
    }

    Ok(json!({ "success": true }))
}

/// 删除分支, 与 Node `deleteBranch` 对齐。
pub async fn delete_branch(
    directory: &str,
    branch: &str,
    force: bool,
) -> oc_core::Result<Value> {
    let repo_ctx = RepoContext::create(directory).await?;
    let repo_root = &repo_ctx.repo_root;

    let flag = if force { "-D" } else { "-d" };
    let result = GitRunner::run(repo_root, &["branch", flag, branch]).await;

    if !result.success {
        return Err(oc_core::Error::Internal(format!(
            "Failed to delete branch: {}",
            result.stderr_text()
        )));
    }

    Ok(json!({ "success": true }))
}

/// 重命名分支, 与 Node `renameBranch` 对齐。
pub async fn rename_branch(
    directory: &str,
    old_name: &str,
    new_name: &str,
) -> oc_core::Result<Value> {
    let repo_ctx = RepoContext::create(directory).await?;
    let repo_root = &repo_ctx.repo_root;

    // 保存 upstream tracking config
    let remote_result = GitRunner::run(
        repo_root,
        &["config", "--get", &format!("branch.{old_name}.remote")],
    )
    .await;
    let merge_result = GitRunner::run(
        repo_root,
        &["config", "--get", &format!("branch.{old_name}.merge")],
    )
    .await;

    let old_remote = remote_result.stdout_text();
    let old_merge = merge_result.stdout_text();

    // git branch -m
    let result = GitRunner::run(repo_root, &["branch", "-m", old_name, new_name]).await;

    if !result.success {
        return Err(oc_core::Error::Internal(format!(
            "Failed to rename branch: {}",
            result.stderr_text()
        )));
    }

    // 恢复 tracking config
    if !old_remote.is_empty() {
        let _ = GitRunner::run(
            repo_root,
            &[
                "config",
                &format!("branch.{new_name}.remote"),
                &old_remote,
            ],
        )
        .await;
    }
    if !old_merge.is_empty() {
        let _ = GitRunner::run(
            repo_root,
            &[
                "config",
                &format!("branch.{new_name}.merge"),
                &old_merge,
            ],
        )
        .await;
    }

    Ok(json!({ "success": true }))
}
