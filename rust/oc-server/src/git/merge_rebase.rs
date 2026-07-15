//! Merge / Rebase 操作。
//!
//! 移植自 Node `service.js` 的 merge/rebase 相关函数。

use serde_json::{json, Value};

use crate::git::context::RepoContext;
use crate::git::runner::GitRunner;

/// Merge branch, 与 Node `merge` 对齐。
pub async fn merge(directory: &str, branch: &str) -> oc_core::Result<Value> {
    let repo_ctx = RepoContext::create(directory).await?;
    let repo_root = &repo_ctx.repo_root;

    let result = GitRunner::run(repo_root, &["merge", branch]).await;

    if result.success {
        return Ok(json!({ "success": true, "conflict": false }));
    }

    // 检查是否冲突
    let error_text = result.stderr_text().to_lowercase();
    let is_conflict = error_text.contains("conflict") || error_text.contains("automatic merge failed");

    if is_conflict {
        // 获取冲突文件列表
        let status_result = GitRunner::run(repo_root, &["status", "--porcelain"]).await;
        let conflicted: Vec<String> = status_result
            .stdout
            .lines()
            .filter(|l| {
                let bytes = l.as_bytes();
                bytes.len() >= 2
                    && (bytes[0] == b'U' || bytes[1] == b'U'
                        || (bytes[0] == b'D' && bytes[1] == b'D')
                        || (bytes[0] == b'A' && bytes[1] == b'A'))
            })
            .map(|l| if l.len() >= 3 { l[3..].trim().to_string() } else { String::new() })
            .filter(|s| !s.is_empty())
            .collect();

        return Ok(json!({
            "success": false,
            "conflict": true,
            "conflictFiles": conflicted,
        }));
    }

    Err(oc_core::Error::Internal(format!(
        "Merge failed: {}",
        result.stderr_text()
    )))
}

/// Abort merge, 与 Node `abortMerge` 对齐。
pub async fn abort_merge(directory: &str) -> oc_core::Result<Value> {
    let repo_ctx = RepoContext::create(directory).await?;
    let repo_root = &repo_ctx.repo_root;

    let result = GitRunner::run(repo_root, &["merge", "--abort"]).await;

    if !result.success {
        return Err(oc_core::Error::Internal(format!(
            "Failed to abort merge: {}",
            result.stderr_text()
        )));
    }

    Ok(json!({ "success": true }))
}

/// Continue merge, 与 Node `continueMerge` 对齐。
pub async fn continue_merge(directory: &str) -> oc_core::Result<Value> {
    let repo_ctx = RepoContext::create(directory).await?;
    let repo_root = &repo_ctx.repo_root;

    // commit --no-edit (GIT_EDITOR=true 跳过编辑器)
    let mut env = GitRunner::build_env().await;
    env.insert("GIT_EDITOR".to_string(), "true".to_string());

    let result = GitRunner::run(repo_root, &["commit", "--no-edit"]).await;

    if result.success {
        return Ok(json!({ "success": true, "conflict": false }));
    }

    // 检查是否仍有冲突
    let status_result = GitRunner::run(repo_root, &["status", "--porcelain"]).await;
    let has_conflicts = status_result.stdout.lines().any(|l| {
        let bytes = l.as_bytes();
        bytes.len() >= 2
            && (bytes[0] == b'U' || bytes[1] == b'U'
                || (bytes[0] == b'D' && bytes[1] == b'D')
                || (bytes[0] == b'A' && bytes[1] == b'A'))
    });

    if has_conflicts {
        return Ok(json!({ "success": false, "conflict": true }));
    }

    Err(oc_core::Error::Internal(format!(
        "Failed to continue merge: {}",
        result.stderr_text()
    )))
}

/// Rebase onto target, 与 Node `rebase` 对齐。
pub async fn rebase(directory: &str, onto: &str) -> oc_core::Result<Value> {
    let repo_ctx = RepoContext::create(directory).await?;
    let repo_root = &repo_ctx.repo_root;

    let result = GitRunner::run(repo_root, &["rebase", onto]).await;

    if result.success {
        return Ok(json!({ "success": true, "conflict": false }));
    }

    // 检查是否冲突
    let error_text = result.stderr_text().to_lowercase();
    let is_conflict = error_text.contains("conflict") || error_text.contains("could not apply");

    if is_conflict {
        let status_result = GitRunner::run(repo_root, &["status", "--porcelain"]).await;
        let conflicted: Vec<String> = status_result
            .stdout
            .lines()
            .filter(|l| {
                let bytes = l.as_bytes();
                bytes.len() >= 2
                    && (bytes[0] == b'U' || bytes[1] == b'U'
                        || (bytes[0] == b'D' && bytes[1] == b'D')
                        || (bytes[0] == b'A' && bytes[1] == b'A'))
            })
            .map(|l| if l.len() >= 3 { l[3..].trim().to_string() } else { String::new() })
            .filter(|s| !s.is_empty())
            .collect();

        return Ok(json!({
            "success": false,
            "conflict": true,
            "conflictFiles": conflicted,
        }));
    }

    Err(oc_core::Error::Internal(format!(
        "Rebase failed: {}",
        result.stderr_text()
    )))
}

/// Abort rebase, 与 Node `abortRebase` 对齐。
pub async fn abort_rebase(directory: &str) -> oc_core::Result<Value> {
    let repo_ctx = RepoContext::create(directory).await?;
    let repo_root = &repo_ctx.repo_root;

    let result = GitRunner::run(repo_root, &["rebase", "--abort"]).await;

    if !result.success {
        return Err(oc_core::Error::Internal(format!(
            "Failed to abort rebase: {}",
            result.stderr_text()
        )));
    }

    Ok(json!({ "success": true }))
}

/// Continue rebase, 与 Node `continueRebase` 对齐。
pub async fn continue_rebase(directory: &str) -> oc_core::Result<Value> {
    let repo_ctx = RepoContext::create(directory).await?;
    let repo_root = &repo_ctx.repo_root;

    let mut env = GitRunner::build_env().await;
    env.insert("GIT_EDITOR".to_string(), "true".to_string());

    let result = GitRunner::run(repo_root, &["rebase", "--continue"]).await;

    if result.success {
        return Ok(json!({ "success": true, "conflict": false }));
    }

    // 检查是否需要 --skip (no changes)
    let error_text = result.stderr_text().to_lowercase();
    if error_text.contains("no changes") || error_text.contains("nothing to commit") {
        let skip_result = GitRunner::run(repo_root, &["rebase", "--skip"]).await;
        if skip_result.success {
            return Ok(json!({ "success": true, "conflict": false }));
        }
    }

    // 检查冲突
    let status_result = GitRunner::run(repo_root, &["status", "--porcelain"]).await;
    let has_conflicts = status_result.stdout.lines().any(|l| {
        let bytes = l.as_bytes();
        bytes.len() >= 2
            && (bytes[0] == b'U' || bytes[1] == b'U'
                || (bytes[0] == b'D' && bytes[1] == b'D')
                || (bytes[0] == b'A' && bytes[1] == b'A'))
    });

    if has_conflicts {
        return Ok(json!({ "success": false, "conflict": true }));
    }

    Err(oc_core::Error::Internal(format!(
        "Failed to continue rebase: {}",
        result.stderr_text()
    )))
}

/// 获取冲突详情, 与 Node `getConflictDetails` 对齐。
pub async fn get_conflict_details(directory: &str) -> oc_core::Result<Value> {
    let repo_ctx = RepoContext::create(directory).await?;
    let repo_root = &repo_ctx.repo_root;

    // porcelain status
    let status_result = GitRunner::run(repo_root, &["status", "--porcelain"]).await;

    // unmerged files
    let unmerged_result = GitRunner::run(repo_root, &["diff", "--name-only", "--diff-filter=U"]).await;
    let unmerged_files: Vec<String> = unmerged_result
        .stdout
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();

    // diff
    let diff_result = GitRunner::run(repo_root, &["diff"]).await;

    // 检测操作类型 (merge / rebase / cherry-pick)
    let mut operation = None;
    let git_dir = GitRunner::run(repo_root, &["rev-parse", "--git-dir"])
        .await
        .stdout_text();

    let git_dir_path = if git_dir.starts_with('/') {
        std::path::PathBuf::from(&git_dir)
    } else {
        repo_root.join(&git_dir)
    };

    if git_dir_path.join("MERGE_HEAD").exists() {
        operation = Some("merge".to_string());
    } else if git_dir_path.join("rebase-merge").exists() || git_dir_path.join("rebase-apply").exists() {
        operation = Some("rebase".to_string());
    } else if git_dir_path.join("CHERRY_PICK_HEAD").exists() {
        operation = Some("cherry-pick".to_string());
    }

    // head info
    let head_sha = GitRunner::run(repo_root, &["rev-parse", "HEAD"])
        .await
        .stdout_text();
    let head_short: String = head_sha.chars().take(7).collect();

    Ok(json!({
        "statusPorcelain": status_result.stdout,
        "unmergedFiles": unmerged_files,
        "diff": diff_result.stdout,
        "headInfo": if head_short.is_empty() { Value::Null } else { json!({ "short": head_short }) },
        "operation": operation,
    }))
}
