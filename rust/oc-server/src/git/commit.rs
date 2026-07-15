//! Commit 操作 — stage/unstage/commit/revert/hunk + index mutation queue。
//!
//! 移植自 Node `service.js` 的 commit/stage/revert 相关函数。

use std::path::PathBuf;

use serde_json::{json, Value};

use crate::git::context::RepoContext;
use crate::git::paths::normalize_file_path_list;
use crate::git::runner::GitRunner;

/// Stage 文件, 与 Node `stageFiles` 对齐。
pub async fn stage_files(directory: &str, paths: &[String]) -> oc_core::Result<Value> {
    if directory.trim().is_empty() {
        return Err(oc_core::Error::BadRequest(
            "directory and path are required for stageFile".to_string(),
        ));
    }

    let file_paths = normalize_file_path_list(paths);
    if file_paths.is_empty() {
        return Err(oc_core::Error::BadRequest(
            "directory and path are required for stageFile".to_string(),
        ));
    }

    let repo_ctx = RepoContext::create(directory).await?;
    let repo_root = &repo_ctx.repo_root;

    // 构建命令: git add -- <paths...>
    let mut args: Vec<String> = vec!["add".to_string(), "--".to_string()];
    args.extend(file_paths.iter().cloned());

    let args_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let result = GitRunner::run(repo_root, &args_refs).await;

    if !result.success {
        // fallback: 逐个 add
        for path in &file_paths {
            let _ = GitRunner::run(repo_root, &["add", "--", path]).await;
        }
    }

    Ok(json!({ "success": true }))
}

/// Unstage 文件, 与 Node `unstageFiles` 对齐。
pub async fn unstage_files(directory: &str, paths: &[String]) -> oc_core::Result<Value> {
    let file_paths = normalize_file_path_list(paths);
    if file_paths.is_empty() {
        return Err(oc_core::Error::BadRequest(
            "directory and path are required for unstageFile".to_string(),
        ));
    }

    let repo_ctx = RepoContext::create(directory).await?;
    let repo_root = &repo_ctx.repo_root;

    // 尝试 restore --staged, fallback reset HEAD
    let mut args: Vec<String> = vec!["restore".to_string(), "--staged".to_string(), "--".to_string()];
    args.extend(file_paths.iter().cloned());

    let args_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let result = GitRunner::run(repo_root, &args_refs).await;

    if !result.success {
        // fallback: git reset HEAD -- <paths...>
        let mut fallback_args: Vec<String> = vec!["reset".to_string(), "HEAD".to_string(), "--".to_string()];
        fallback_args.extend(file_paths.iter().cloned());
        let fallback_refs: Vec<&str> = fallback_args.iter().map(|s| s.as_str()).collect();
        let _ = GitRunner::run(repo_root, &fallback_refs).await;
    }

    Ok(json!({ "success": true }))
}

/// 创建 commit, 与 Node `commit` 对齐。
pub async fn commit(
    directory: &str,
    message: &str,
    add_all: bool,
    files: Option<&[String]>,
) -> oc_core::Result<Value> {
    let repo_ctx = RepoContext::create(directory).await?;
    let repo_root = &repo_ctx.repo_root;

    if message.trim().is_empty() {
        return Err(oc_core::Error::BadRequest("message is required".to_string()));
    }

    // 暂存文件
    if add_all {
        let _ = GitRunner::run(repo_root, &["add", "."]).await;
    } else if let Some(files) = files {
        if !files.is_empty() {
            let mut args: Vec<String> = vec!["add".to_string(), "--".to_string()];
            args.extend(files.iter().cloned());
            let args_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
            let _ = GitRunner::run(repo_root, &args_refs).await;
        }
    }

    // commit
    let result = GitRunner::run(repo_root, &["commit", "-m", message]).await;

    if !result.success {
        return Err(oc_core::Error::Internal(format!(
            "Failed to commit: {}",
            result.stderr_text()
        )));
    }

    // 获取 commit hash
    let rev_result = GitRunner::run(repo_root, &["rev-parse", "HEAD"]).await;
    let commit_hash = rev_result.stdout_text();
    let short_hash: String = commit_hash.chars().take(7).collect();

    Ok(json!({
        "success": true,
        "commit": commit_hash,
        "shortHash": short_hash,
        "branch": null, // 可选, 调用方不依赖此字段
    }))
}

/// Revert 文件 (撤销变更), 与 Node `revertFile` 对齐。
pub async fn revert_file(
    directory: &str,
    file_path: &str,
    scope: &str, // "all" (默认) 或 "working"
) -> oc_core::Result<Value> {
    let repo_ctx = RepoContext::create(directory).await?;
    let repo_root = &repo_ctx.repo_root;

    // 检查文件是否被跟踪
    let ls_result = GitRunner::run(
        repo_root,
        &["ls-files", "--error-unmatch", file_path],
    )
    .await;

    let is_tracked = ls_result.success;

    if scope == "working" {
        // 只恢复工作区 (保留暂存区)
        if is_tracked {
            let result = GitRunner::run(repo_root, &["restore", file_path]).await;
            if !result.success {
                return Err(oc_core::Error::Internal(format!(
                    "Failed to restore file: {}",
                    result.stderr_text()
                )));
            }
        }
    } else {
        // scope == "all": 恢复暂存 + 工作区
        if is_tracked {
            // restore --staged + restore
            let _ = GitRunner::run(repo_root, &["restore", "--staged", file_path]).await;
            let result = GitRunner::run(repo_root, &["restore", file_path]).await;
            if !result.success {
                return Err(oc_core::Error::Internal(format!(
                    "Failed to restore file: {}",
                    result.stderr_text()
                )));
            }
        } else {
            // 未跟踪文件: git clean
            let _ = GitRunner::run(repo_root, &["clean", "-f", "-d", file_path]).await;
        }
    }

    Ok(json!({ "success": true }))
}

/// Apply hunk patch, 与 Node `applyHunk` 对齐。
pub async fn apply_hunk(
    directory: &str,
    _file_path: &str,
    patch: &str,
    action: &str, // "stage" | "unstage" | "discard"
) -> oc_core::Result<Value> {
    let repo_ctx = RepoContext::create(directory).await?;
    let repo_root = &repo_ctx.repo_root;

    // 写 patch 到临时文件
    let tmp_dir = std::env::temp_dir();
    let tmp_path = tmp_dir.join(format!(
        "oc-hunk-{}-{}.patch",
        std::process::id(),
        chrono::Utc::now().timestamp_millis()
    ));
    tokio::fs::write(&tmp_path, patch).await?;

    let tmp_str = tmp_path.to_string_lossy().to_string();

    let result = match action {
        "stage" => {
            // git apply --cached <patch>
            GitRunner::run(repo_root, &["apply", "--cached", &tmp_str]).await
        }
        "unstage" => {
            // git apply --cached --reverse <patch>
            GitRunner::run(repo_root, &["apply", "--cached", "--reverse", &tmp_str]).await
        }
        "discard" => {
            // git apply --reverse <patch>
            GitRunner::run(repo_root, &["apply", "--reverse", &tmp_str]).await
        }
        _ => {
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return Err(oc_core::Error::BadRequest(format!(
                "Invalid action: {action}"
            )));
        }
    };

    let _ = tokio::fs::remove_file(&tmp_path).await;

    if !result.success {
        return Err(oc_core::Error::BadRequest(format!(
            "Failed to apply hunk: {}",
            result.stderr_text()
        )));
    }

    Ok(json!({ "success": true }))
}

/// Cherry-pick, 与 Node `cherryPick` 对齐。
pub async fn cherry_pick(directory: &str, hash: &str) -> oc_core::Result<Value> {
    if !crate::git::paths::is_valid_commit_hash(hash) {
        return Err(oc_core::Error::BadRequest("Invalid commit hash".to_string()));
    }

    let repo_ctx = RepoContext::create(directory).await?;
    let repo_root = &repo_ctx.repo_root;

    let result = GitRunner::run(repo_root, &["cherry-pick", hash]).await;

    if result.success {
        return Ok(json!({ "success": true, "conflict": false }));
    }

    // 检查是否是冲突
    let error_text = result.stderr_text().to_lowercase();
    let is_conflict = error_text.contains("conflict") || error_text.contains("patch does not apply");

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
            .map(|l| {
                if l.len() >= 3 {
                    l[3..].trim().to_string()
                } else {
                    String::new()
                }
            })
            .filter(|s| !s.is_empty())
            .collect();

        return Ok(json!({
            "success": false,
            "conflict": true,
            "conflictFiles": conflicted,
        }));
    }

    Err(oc_core::Error::Internal(format!(
        "Cherry-pick failed: {}",
        result.stderr_text()
    )))
}

/// Revert commit (创建一个反向 commit), 与 Node `revertCommit` 对齐。
pub async fn revert_commit(directory: &str, hash: &str) -> oc_core::Result<Value> {
    if !crate::git::paths::is_valid_commit_hash(hash) {
        return Err(oc_core::Error::BadRequest("Invalid commit hash".to_string()));
    }

    let repo_ctx = RepoContext::create(directory).await?;
    let repo_root = &repo_ctx.repo_root;

    let result = GitRunner::run(repo_root, &["revert", "--no-commit", hash]).await;

    if result.success {
        return Ok(json!({ "success": true, "conflict": false }));
    }

    let error_text = result.stderr_text().to_lowercase();
    let is_conflict = error_text.contains("conflict") || error_text.contains("patch does not apply");

    if is_conflict {
        return Ok(json!({
            "success": false,
            "conflict": true,
        }));
    }

    Err(oc_core::Error::Internal(format!(
        "Revert commit failed: {}",
        result.stderr_text()
    )))
}

/// Reset 到 commit, 与 Node `resetToCommit` 对齐。
pub async fn reset_to_commit(
    directory: &str,
    hash: &str,
    mode: &str,  // "soft" | "mixed" | "hard"
    force: bool,
) -> oc_core::Result<Value> {
    if !crate::git::paths::is_valid_commit_hash(hash) {
        return Err(oc_core::Error::BadRequest("Invalid commit hash".to_string()));
    }

    let repo_ctx = RepoContext::create(directory).await?;
    let repo_root = &repo_ctx.repo_root;

    // hard reset 需要检查工作区是否干净
    if mode == "hard" && !force {
        let status_result = GitRunner::run(repo_root, &["status", "--porcelain"]).await;
        if !status_result.stdout.trim().is_empty() {
            return Err(oc_core::Error::BadRequest(
                "Cannot hard reset: uncommitted changes present. Use force=true to override.".to_string(),
            ));
        }
    }

    let result = GitRunner::run(repo_root, &["reset", &format!("--{mode}"), hash]).await;

    if !result.success {
        return Err(oc_core::Error::Internal(format!(
            "Reset failed: {}",
            result.stderr_text()
        )));
    }

    Ok(json!({ "success": true }))
}

/// 保留 `_` 以匹配签名约定。
#[allow(dead_code)]
fn _repo_root_marker() -> PathBuf {
    PathBuf::new()
}
