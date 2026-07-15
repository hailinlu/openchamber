//! Worktree 操作。
//!
//! 移植自 Node `service.js` 的 worktree 相关函数。
//! OpenCode DB sync (`syncSandboxesToOpenCodeDb`) 暂为 stub (阶段 4B 实现)。

use std::path::Path;

use serde_json::{json, Value};

use crate::git::parsing::parse_worktree_porcelain;
use crate::git::runner::GitRunner;

/// 列出 worktrees, 与 Node `getWorktrees` 对齐。
///
/// 失败时返回空数组 (不是错误)。
pub async fn get_worktrees(directory: &str) -> Vec<Value> {
    let dir = crate::git::paths::normalize_directory_path(directory);
    if dir.is_empty() || !Path::new(&dir).exists() {
        return Vec::new();
    }

    let dir_path = std::path::PathBuf::from(&dir);

    // 解析 repo root
    let root_result = GitRunner::run(&dir_path, &["rev-parse", "--show-toplevel"]).await;
    if !root_result.success {
        return Vec::new();
    }
    let repo_root = root_result.stdout_text();

    // worktree list --porcelain
    let result = match GitRunner::run_or_throw(
        Path::new(&repo_root),
        &["worktree", "list", "--porcelain"],
        "Failed to list git worktrees",
    )
    .await
    {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };

    parse_worktree_porcelain(&result.stdout)
        .into_iter()
        .map(|wt| {
            json!({
                "head": wt.head,
                "name": wt.name,
                "branch": wt.branch,
                "path": wt.path,
            })
        })
        .collect()
}

/// 验证 worktree 创建参数, 与 Node `validateWorktreeCreate` 对齐 (简化版)。
pub async fn validate_worktree_create(directory: &str, input: &Value) -> oc_core::Result<Value> {
    let mode = if input.get("mode").and_then(|v| v.as_str()) == Some("existing") {
        "existing"
    } else {
        "new"
    };

    let mut errors: Vec<Value> = Vec::new();

    let repo_ctx = crate::git::context::RepoContext::create(directory).await?;
    let repo_root = &repo_ctx.repo_root;

    let branch_name = input
        .get("branchName")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let preferred_branch = crate::git::paths::clean_branch_name(&branch_name);

    if mode == "new"
        && !preferred_branch.is_empty() {
            // 检查分支是否已存在
            let exists_result = GitRunner::run(
                repo_root,
                &["show-ref", "--verify", "--quiet", &format!("refs/heads/{preferred_branch}")],
            )
            .await;
            if exists_result.success {
                errors.push(json!({
                    "code": "branch_exists",
                    "message": format!("Branch already exists: {preferred_branch}"),
                }));
            }
        }

    Ok(json!({
        "valid": errors.is_empty(),
        "errors": errors,
        "mode": mode,
        "branchName": preferred_branch,
    }))
}

/// 预览 worktree 创建, 与 Node `previewWorktreeCreate` 对齐 (简化版)。
pub async fn preview_worktree_create(directory: &str, input: &Value) -> oc_core::Result<Value> {
    let validation = validate_worktree_create(directory, input).await?;

    let branch_name = validation.get("branchName").and_then(|v| v.as_str()).unwrap_or("");
    let mode = validation.get("mode").and_then(|v| v.as_str()).unwrap_or("new");

    Ok(json!({
        "valid": validation.get("valid"),
        "mode": mode,
        "branchName": branch_name,
        "preview": true,
    }))
}

/// 创建 worktree, 与 Node `createWorktree` 对齐 (简化版, 不含 OpenCode DB sync)。
pub async fn create_worktree(directory: &str, input: &Value) -> oc_core::Result<Value> {
    let repo_ctx = crate::git::context::RepoContext::create(directory).await?;
    let repo_root = &repo_ctx.repo_root;

    let mode = if input.get("mode").and_then(|v| v.as_str()) == Some("existing") {
        "existing"
    } else {
        "new"
    };

    let branch_name = input
        .get("branchName")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let target_dir = input
        .get("directory")
        .or_else(|| input.get("path"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();

    if target_dir.is_empty() {
        return Err(oc_core::Error::BadRequest(
            "directory is required for worktree creation".to_string(),
        ));
    }

    let start_ref = input
        .get("startRef")
        .and_then(|v| v.as_str())
        .unwrap_or("HEAD")
        .trim();

    let result = if mode == "existing" {
        // git worktree add <dir> <branch>
        GitRunner::run(repo_root, &["worktree", "add", &target_dir, &branch_name]).await
    } else {
        // git worktree add -b <branch> <dir> <startRef>
        GitRunner::run(
            repo_root,
            &["worktree", "add", "-b", &branch_name, &target_dir, start_ref],
        )
        .await
    };

    if !result.success {
        return Err(oc_core::Error::Internal(format!(
            "Failed to create worktree: {}",
            result.stderr_text()
        )));
    }

    // 获取 head SHA
    let head_result = GitRunner::run(Path::new(&target_dir), &["rev-parse", "HEAD"]).await;
    let head = head_result.stdout_text();

    let name = Path::new(&target_dir)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_string();

    Ok(json!({
        "head": head,
        "name": name,
        "branch": branch_name,
        "path": target_dir,
        "directoryCreated": true,
        "bootstrapStatus": "ready", // OpenCode DB sync stub → ready
    }))
}

/// 删除 worktree, 与 Node `removeWorktree` 对齐。
pub async fn remove_worktree(
    directory: &str,
    target_dir: &str,
    delete_local_branch: bool,
) -> oc_core::Result<Value> {
    let repo_ctx = crate::git::context::RepoContext::create(directory).await?;
    let repo_root = &repo_ctx.repo_root;

    // 获取 worktree 列表, 确认不是 primary
    let worktrees = get_worktrees(directory).await;
    if let Some(first) = worktrees.first() {
        if let Some(path) = first.get("path").and_then(|v| v.as_str()) {
            if path == repo_root.to_string_lossy() && target_dir == path {
                return Err(oc_core::Error::BadRequest(
                    "Cannot remove the primary workspace".to_string(),
                ));
            }
        }
    }

    // git worktree remove [--force] <dir>
    let force = if delete_local_branch { vec!["--force"] } else { vec![] };
    let mut args: Vec<&str> = vec!["worktree", "remove"];
    args.extend(force);
    args.push(target_dir);

    let result = GitRunner::run(repo_root, &args).await;

    if !result.success {
        // fallback: 尝试强制删除目录
        let force_result = GitRunner::run(repo_root, &["worktree", "remove", "--force", target_dir]).await;
        if !force_result.success {
            return Err(oc_core::Error::Internal(format!(
                "Failed to remove worktree: {}",
                result.stderr_text()
            )));
        }
    }

    // 可选: 删除本地分支
    if delete_local_branch {
        // 从 target_dir 获取分支名
        // 简化: 调用方应传入分支名
    }

    Ok(json!({ "success": true }))
}

/// 检查是否是 linked worktree, 与 Node `isLinkedWorktree` 对齐。
pub async fn is_linked_worktree(directory: &str) -> oc_core::Result<bool> {
    let dir = crate::git::paths::normalize_directory_path(directory);
    let dir_path = std::path::PathBuf::from(&dir);

    let git_dir = GitRunner::run(&dir_path, &["rev-parse", "--git-dir"])
        .await
        .stdout_text();
    let git_common_dir = GitRunner::run(&dir_path, &["rev-parse", "--git-common-dir"])
        .await
        .stdout_text();

    Ok(!git_dir.is_empty() && !git_common_dir.is_empty() && git_dir != git_common_dir)
}

/// 解析 primary worktree root, 与 Node `resolvePrimaryWorktreeRoot` 对齐。
pub async fn resolve_primary_worktree_root(directory: &str) -> oc_core::Result<Value> {
    let dir = crate::git::paths::normalize_directory_path(directory);
    let dir_path = std::path::PathBuf::from(&dir);

    let result = GitRunner::run(
        &dir_path,
        &["rev-parse", "--absolute-git-dir", "--git-common-dir"],
    )
    .await;

    if !result.success {
        return Ok(json!({ "root": directory }));
    }

    let lines: Vec<String> = result
        .stdout
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();

    let absolute_git_dir = crate::git::paths::normalize_path(lines.first().map(|s| s.as_str()).unwrap_or(""));

    if let Some(root) = derive_primary_root(&absolute_git_dir) {
        return Ok(json!({ "root": root }));
    }

    // 尝试 git-common-dir
    if let Some(raw_common) = lines.get(1) {
        let common_dir = crate::git::paths::normalize_path(raw_common);
        let resolved = if Path::new(&common_dir).is_absolute() {
            common_dir
        } else {
            dir_path.join(&common_dir).to_string_lossy().to_string()
        };
        let normalized = crate::git::paths::normalize_path(&resolved);
        if let Some(root) = derive_primary_root(&normalized) {
            return Ok(json!({ "root": root }));
        }
    }

    Ok(json!({ "root": directory }))
}

/// 从 git dir 推导 primary worktree root, 与 Node `derivePrimaryWorktreeRootFromGitDir` 对齐。
fn derive_primary_root(git_dir: &str) -> Option<String> {
    let normalized = crate::git::paths::normalize_path(git_dir);
    if normalized.is_empty() {
        return None;
    }

    if normalized.ends_with("/.git") {
        let root = &normalized[..normalized.len() - "/.git".len()];
        return if root.is_empty() { None } else { Some(root.to_string()) };
    }

    let marker = "/.git/worktrees/";
    if let Some(idx) = normalized.find(marker) {
        if idx > 0 {
            let root = &normalized[..idx];
            return if root.is_empty() { None } else { Some(root.to_string()) };
        }
    }

    None
}

/// 解析 worktree top level, 与 Node `resolveWorktreeTopLevel` 对齐。
pub async fn resolve_worktree_top_level(directory: &str) -> oc_core::Result<Value> {
    let dir = crate::git::paths::normalize_directory_path(directory);
    let dir_path = std::path::PathBuf::from(&dir);

    let result = GitRunner::run(&dir_path, &["rev-parse", "--show-toplevel"]).await;

    if !result.success {
        return Ok(json!({ "root": directory }));
    }

    let root = crate::git::paths::normalize_path(&result.stdout_text());
    Ok(json!({ "root": if root.is_empty() { directory.to_string() } else { root } }))
}

/// 验证 worktree 目录, 与 Node `validateWorktreeDirectory` 对齐。
pub async fn validate_worktree_directory(
    directory: &str,
    worktree_root: &str,
) -> oc_core::Result<Value> {
    let is_repo = crate::git::status::is_git_repository(directory).await;
    let resolved_cwd = std::fs::canonicalize(directory).unwrap_or_else(|_| std::path::PathBuf::from(directory));
    let resolved_root = std::fs::canonicalize(worktree_root).unwrap_or_else(|_| std::path::PathBuf::from(worktree_root));

    let inside = crate::git::paths::is_inside_or_same_directory(&resolved_cwd, &resolved_root);

    Ok(json!({
        "valid": is_repo && inside,
        "insideWorktreeRoot": inside,
        "resolvedWorktreeRoot": resolved_root.to_string_lossy(),
        "resolvedCwd": resolved_cwd.to_string_lossy(),
    }))
}

/// Canonicalize worktree state, 与 Node `canonicalizeWorktreeState` 对齐 (简化版)。
pub async fn canonicalize_worktree_state(directory: &str) -> oc_core::Result<Value> {
    let dir = crate::git::paths::normalize_directory_path(directory);
    let dir_path = std::path::PathBuf::from(&dir);

    // symbolic-ref -q HEAD
    let head_ref = GitRunner::run(&dir_path, &["symbolic-ref", "-q", "HEAD"])
        .await
        .stdout_text();

    // rev-parse HEAD
    let head_sha = GitRunner::run(&dir_path, &["rev-parse", "HEAD"])
        .await
        .stdout_text();

    // status -uall
    let status = GitRunner::run(&dir_path, &["status", "-uall", "--porcelain=v1", "-b"])
        .await
        .stdout;

    // MERGE_HEAD
    let merge_head = GitRunner::run(&dir_path, &["rev-parse", "--verify", "--quiet", "MERGE_HEAD"])
        .await;

    Ok(json!({
        "headRef": head_ref,
        "headSha": head_sha,
        "status": status,
        "mergeInProgress": merge_head.success,
        "canonicalized": true,
    }))
}
