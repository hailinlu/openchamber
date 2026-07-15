//! Integrate 操作 — worktree 集成工作流 (cherry-pick based)。
//!
//! 移植自 Node `service.js` 的 integrate 相关函数 (简化版)。

use serde_json::{json, Value};

use crate::git::runner::GitRunner;

/// 计算 integrate plan, 与 Node `computeIntegratePlan` 对齐 (简化版)。
pub async fn compute_integrate_plan(input: &Value) -> oc_core::Result<Value> {
    let source_branch = input
        .get("sourceBranch")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let target_branch = input
        .get("targetBranch")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let directory = input
        .get("directory")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if source_branch.is_empty() {
        return Err(oc_core::Error::BadRequest(
            "sourceBranch is required".to_string(),
        ));
    }
    if target_branch.is_empty() {
        return Err(oc_core::Error::BadRequest(
            "targetBranch is required".to_string(),
        ));
    }

    let dir_path = std::path::PathBuf::from(crate::git::paths::normalize_directory_path(directory));

    // 获取 source 分支的 commits
    let log_result = GitRunner::run(
        &dir_path,
        &[
            "log",
            "--pretty=format:%H",
            &format!("origin/{target_branch}..{source_branch}"),
        ],
    )
    .await;

    let commits: Vec<String> = log_result
        .stdout
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();

    Ok(json!({
        "sourceBranch": source_branch,
        "targetBranch": target_branch,
        "commits": commits,
        "commitCount": commits.len(),
    }))
}

/// 执行 integrate, 与 Node `integrateWorktreeCommits` 对齐 (简化版)。
pub async fn integrate_worktree_commits(plan: &Value) -> oc_core::Result<Value> {
    let source_branch = plan
        .get("sourceBranch")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let target_branch = plan
        .get("targetBranch")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let directory = plan
        .get("directory")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if source_branch.is_empty() || target_branch.is_empty() {
        return Err(oc_core::Error::BadRequest(
            "sourceBranch and targetBranch are required".to_string(),
        ));
    }

    let dir_path = std::path::PathBuf::from(crate::git::paths::normalize_directory_path(directory));

    // 创建临时 worktree
    let tmp_dir = std::env::temp_dir().join(format!(
        "oc-integrate-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_millis()
    ));

    // git worktree add <tmp> <target_branch>
    let add_result = GitRunner::run(
        &dir_path,
        &["worktree", "add", tmp_dir.to_string_lossy().as_ref(), target_branch],
    )
    .await;

    if !add_result.success {
        return Err(oc_core::Error::Internal(format!(
            "Failed to create temp worktree: {}",
            add_result.stderr_text()
        )));
    }

    // cherry-pick source commits
    let log_result = GitRunner::run(
        &tmp_dir,
        &["log", "--pretty=format:%H", &format!("origin/{target_branch}..{source_branch}")],
    )
    .await;

    let commits: Vec<String> = log_result
        .stdout
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();

    let mut cherry_picked = 0;
    let mut conflict = false;

    for commit in commits.iter().rev() {
        let cp_result = GitRunner::run(&tmp_dir, &["cherry-pick", commit]).await;
        if cp_result.success {
            cherry_picked += 1;
        } else {
            let error_text = cp_result.stderr_text().to_lowercase();
            if error_text.contains("conflict") {
                conflict = true;
            }
            break;
        }
    }

    if !conflict {
        // push back to target
        let _ = GitRunner::run(&tmp_dir, &["push", "origin", target_branch]).await;
    }

    // 清理临时 worktree
    let _ = GitRunner::run(&dir_path, &["worktree", "remove", "--force", tmp_dir.to_string_lossy().as_ref()]).await;

    Ok(json!({
        "success": !conflict,
        "conflict": conflict,
        "cherryPicked": cherry_picked,
        "totalCommits": commits.len(),
        "tempWorktreePath": tmp_dir.to_string_lossy(),
    }))
}

/// 获取 integrate 冲突详情, 与 Node `getIntegrateConflictDetails` 对齐。
pub async fn get_integrate_conflict_details(temp_worktree_path: &str) -> oc_core::Result<Value> {
    let dir_path = std::path::PathBuf::from(temp_worktree_path);

    let status_result = GitRunner::run(&dir_path, &["status", "--porcelain"]).await;
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

    Ok(json!({
        "statusPorcelain": status_result.stdout,
        "conflictedFiles": conflicted,
    }))
}

/// 检测 cherry-pick 是否在进行中, 与 Node `isCherryPickInProgress` 对齐。
pub async fn is_cherry_pick_in_progress(temp_worktree_path: &str) -> oc_core::Result<bool> {
    let dir_path = std::path::PathBuf::from(temp_worktree_path);
    let result = GitRunner::run(
        &dir_path,
        &["rev-parse", "--verify", "--quiet", "CHERRY_PICK_HEAD"],
    )
    .await;
    Ok(result.success)
}

/// Abort integrate, 与 Node `abortIntegrate` 对齐。
pub async fn abort_integrate(state: &Value) -> oc_core::Result<Value> {
    let temp_dir = state
        .get("tempWorktreePath")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if !temp_dir.is_empty() {
        let dir_path = std::path::PathBuf::from(temp_dir);
        let _ = GitRunner::run(&dir_path, &["cherry-pick", "--abort"]).await;
    }

    Ok(json!({ "success": true }))
}

/// Continue integrate, 与 Node `continueIntegrate` 对齐。
pub async fn continue_integrate(state: &Value) -> oc_core::Result<Value> {
    let temp_dir = state
        .get("tempWorktreePath")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if temp_dir.is_empty() {
        return Err(oc_core::Error::BadRequest(
            "tempWorktreePath is required".to_string(),
        ));
    }

    let dir_path = std::path::PathBuf::from(temp_dir);

    let mut env = GitRunner::build_env().await;
    env.insert("GIT_EDITOR".to_string(), "true".to_string());

    let result = GitRunner::run(&dir_path, &["cherry-pick", "--continue"]).await;

    Ok(json!({
        "success": result.success,
        "output": result.stdout_text(),
    }))
}
