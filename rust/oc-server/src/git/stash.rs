//! Stash 操作。
//!
//! 移植自 Node `service.js` 的 stash 相关函数。

use serde_json::{json, Value};

use crate::git::context::RepoContext;
use crate::git::parsing::parse_stash_list;
use crate::git::runner::GitRunner;

/// 列出 stash, 与 Node `listStashes` 对齐。
pub async fn list_stashes(directory: &str) -> oc_core::Result<Value> {
    let repo_ctx = RepoContext::create(directory).await?;
    let repo_root = &repo_ctx.repo_root;

    let result = GitRunner::run(
        repo_root,
        &[
            "stash",
            "list",
            "--format=%gd\x1f%gs\x1f%cr\x1f%H",
        ],
    )
    .await;

    let stashes: Vec<Value> = parse_stash_list(&result.stdout)
        .into_iter()
        .map(|s| {
            json!({
                "ref": s.r#ref,
                "message": s.message,
                "relativeDate": s.relative_date,
                "hash": s.hash,
            })
        })
        .collect();

    Ok(json!({ "stashes": stashes }))
}

/// 批量统计 stash 变更文件数, 与 Node `countStashFiles` 对齐。
pub async fn count_stash_files(directory: &str, refs: &[String]) -> oc_core::Result<Value> {
    let repo_ctx = RepoContext::create(directory).await?;
    let repo_root = &repo_ctx.repo_root;

    let mut counts = Vec::new();
    for r#ref in refs {
        let result = GitRunner::run(repo_root, &["stash", "show", "--name-only", r#ref]).await;
        let count = if result.success {
            result.stdout.lines().filter(|l| !l.trim().is_empty()).count() as u64
        } else {
            0
        };
        counts.push(json!({
            "ref": r#ref,
            "count": count,
        }));
    }

    Ok(json!({ "counts": counts }))
}

/// Stash push, 与 Node `stashPush` 对齐。
pub async fn stash_push(directory: &str, message: Option<&str>) -> oc_core::Result<Value> {
    let repo_ctx = RepoContext::create(directory).await?;
    let repo_root = &repo_ctx.repo_root;

    let result = match message {
        Some(msg) => GitRunner::run(
            repo_root,
            &["stash", "push", "--include-untracked", "-m", msg],
        )
        .await,
        None => GitRunner::run(repo_root, &["stash", "push", "--include-untracked"]).await,
    };

    if !result.success {
        return Err(oc_core::Error::Internal(format!(
            "Failed to stash push: {}",
            result.stderr_text()
        )));
    }

    Ok(json!({ "success": true, "output": result.stdout_text() }))
}

/// Stash apply, 与 Node `stashApply` 对齐。
pub async fn stash_apply(directory: &str, r#ref: &str) -> oc_core::Result<Value> {
    let repo_ctx = RepoContext::create(directory).await?;
    let repo_root = &repo_ctx.repo_root;

    // 尝试 --index, fallback 不带 --index
    let result = GitRunner::run(repo_root, &["stash", "apply", "--index", r#ref]).await;

    if !result.success {
        // fallback: git stash apply <ref>
        let fallback = GitRunner::run(repo_root, &["stash", "apply", r#ref]).await;
        if !fallback.success {
            return Err(oc_core::Error::Internal(format!(
                "Failed to apply stash: {}",
                fallback.stderr_text()
            )));
        }
    }

    Ok(json!({ "success": true }))
}

/// Stash pop (apply + drop), 与 Node `stashPop` 对齐。
pub async fn stash_pop(directory: &str, r#ref: &str) -> oc_core::Result<Value> {
    let _repo_ctx = RepoContext::create(directory).await?;

    // 先 apply
    let apply_result = stash_apply(directory, r#ref).await?;
    if apply_result.get("success") != Some(&json!(true)) {
        return Ok(apply_result);
    }

    // 成功后 drop
    let _ = stash_drop(directory, r#ref).await;

    Ok(json!({ "success": true }))
}

/// Stash drop, 与 Node `stashDrop` 对齐。
pub async fn stash_drop(directory: &str, r#ref: &str) -> oc_core::Result<Value> {
    let repo_ctx = RepoContext::create(directory).await?;
    let repo_root = &repo_ctx.repo_root;

    let result = GitRunner::run(repo_root, &["stash", "drop", r#ref]).await;

    if !result.success {
        return Err(oc_core::Error::Internal(format!(
            "Failed to drop stash: {}",
            result.stderr_text()
        )));
    }

    Ok(json!({ "success": true }))
}
