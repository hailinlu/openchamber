//! Diff 操作 — get_diff, get_file_diff, get_commit_file_diff。
//!
//! 移植自 Node `service.js` 的 diff 相关函数。

use serde_json::{json, Value};

use crate::git::context::{GitFileContext, RepoContext};
use crate::git::runner::GitRunner;

/// 获取 diff 文本, 与 Node `getDiff` 对齐。
pub async fn get_diff(
    directory: &str,
    file_path: Option<&str>,
    staged: bool,
    context_lines: u32,
) -> oc_core::Result<String> {
    let repo_ctx = RepoContext::create(directory).await?;
    let repo_root = &repo_ctx.repo_root;

    let mut args: Vec<String> = vec!["diff".to_string(), "--no-color".to_string()];

    // context lines
    args.push(format!("-U{}", context_lines));

    if staged {
        args.push("--cached".to_string());
    }

    let resolved_path = if let Some(fp) = file_path {
        let file_ctx = GitFileContext::resolve(&repo_ctx, fp)?;
        Some(file_ctx.repo_path)
    } else {
        None
    };

    if let Some(rp) = &resolved_path {
        args.push("--".to_string());
        args.push(rp.clone());
    }

    let args_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let result = GitRunner::run(repo_root, &args_refs).await;

    let diff = result.stdout;

    // 如果文件是新文件且 diff 为空, 尝试 --no-index fallback
    if diff.is_empty() && file_path.is_some() {
        if let Some(rp) = &resolved_path {
            let abs_path = repo_root.join(rp);
            // 对于新文件, git diff 不产生输出 (因为没有跟踪版本)
            // 使用 --no-index /dev/null <path> 作为 fallback
            let no_index_result = GitRunner::run(
                repo_root,
                &[
                    "diff",
                    "--no-color",
                    "--no-index",
                    "/dev/null",
                    &abs_path.to_string_lossy(),
                ],
            )
            .await;
            // --no-index 在有差异时退出码为 1 (success=false), 但 stdout 有内容
            return Ok(no_index_result.stdout);
        }
    }

    Ok(diff)
}

/// 获取文件的前后内容, 与 Node `getFileDiff` 对齐。
pub async fn get_file_diff(
    directory: &str,
    file_path: &str,
    staged: bool,
) -> oc_core::Result<Value> {
    let repo_ctx = RepoContext::create(directory).await?;
    let repo_root = &repo_ctx.repo_root;
    let file_ctx = GitFileContext::resolve(&repo_ctx, file_path)?;
    let repo_path = &file_ctx.repo_path;

    // original: HEAD:<path> (staged) 或 :<path> (unstaged)
    let original = if staged {
        let result = GitRunner::run(repo_root, &["show", &format!("HEAD:{repo_path}")]).await;
        if result.success {
            result.stdout
        } else {
            String::new()
        }
    } else {
        // :<path> (index version)
        let result = GitRunner::run(repo_root, &["show", &format!(":{repo_path}")]).await;
        if result.success {
            result.stdout
        } else {
            // fallback to HEAD
            let head_result =
                GitRunner::run(repo_root, &["show", &format!("HEAD:{repo_path}")]).await;
            if head_result.success {
                head_result.stdout
            } else {
                String::new()
            }
        }
    };

    // modified: 读取工作区文件
    let modified = tokio::fs::read_to_string(&file_ctx.absolute_path)
        .await
        .unwrap_or_default();

    // 检测 binary
    let is_binary = original.bytes().any(|b| b == 0) || modified.bytes().any(|b: u8| b == 0);

    Ok(json!({
        "original": original,
        "modified": modified,
        "path": repo_path,
        "isBinary": is_binary,
    }))
}

/// 获取 commit 中文件的 diff, 与 Node `getCommitFileDiff` 对齐。
pub async fn get_commit_file_diff(
    directory: &str,
    hash: &str,
    file_path: &str,
    is_binary: bool,
) -> oc_core::Result<Value> {
    let repo_ctx = RepoContext::create(directory).await?;
    let repo_root = &repo_ctx.repo_root;

    // 验证 hash 格式
    if !crate::git::paths::is_valid_commit_hash(hash) {
        return Err(oc_core::Error::BadRequest("Invalid commit hash".to_string()));
    }

    let file_ctx = GitFileContext::resolve(&repo_ctx, file_path)?;
    let repo_path = &file_ctx.repo_path;

    if is_binary {
        return Ok(json!({
            "original": "",
            "modified": "",
            "isBinary": true,
        }));
    }

    // original: <hash>^:<path>
    let original_result = GitRunner::run(
        repo_root,
        &["show", &format!("{hash}^:{repo_path}")],
    )
    .await;

    // modified: <hash>:<path>
    let modified_result = GitRunner::run(
        repo_root,
        &["show", &format!("{hash}:{repo_path}")],
    )
    .await;

    let original = if original_result.success {
        original_result.stdout
    } else {
        String::new()
    };

    let modified = if modified_result.success {
        modified_result.stdout
    } else {
        String::new()
    };

    Ok(json!({
        "original": original,
        "modified": modified,
        "isBinary": false,
    }))
}

/// 获取 range diff, 与 Node `getRangeDiff` 对齐。
pub async fn get_range_diff(
    directory: &str,
    base: &str,
    head: &str,
    file_path: Option<&str>,
    context_lines: u32,
) -> oc_core::Result<String> {
    let repo_ctx = RepoContext::create(directory).await?;
    let repo_root = &repo_ctx.repo_root;

    // 验证 base 和 head refs
    let base_check = GitRunner::run(repo_root, &["rev-parse", "--verify", base]).await;
    if !base_check.success {
        return Err(oc_core::Error::BadRequest(format!(
            "Invalid base ref: {base}"
        )));
    }

    let head_check = GitRunner::run(repo_root, &["rev-parse", "--verify", head]).await;
    if !head_check.success {
        return Err(oc_core::Error::BadRequest(format!(
            "Invalid head ref: {head}"
        )));
    }

    let mut args: Vec<String> = vec![
        "diff".to_string(),
        "--no-color".to_string(),
        format!("-U{}", context_lines),
        format!("{base}...{head}"),
    ];

    if let Some(fp) = file_path {
        let file_ctx = GitFileContext::resolve(&repo_ctx, fp)?;
        args.push("--".to_string());
        args.push(file_ctx.repo_path);
    }

    let args_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let result = GitRunner::run(repo_root, &args_refs).await;
    Ok(result.stdout)
}

/// 获取 range 内变更的文件列表, 与 Node `getRangeFiles` 对齐。
pub async fn get_range_files(
    directory: &str,
    base: &str,
    head: &str,
) -> oc_core::Result<Value> {
    let repo_ctx = RepoContext::create(directory).await?;
    let repo_root = &repo_ctx.repo_root;

    let result = GitRunner::run(
        repo_root,
        &["diff", "--name-only", &format!("{base}...{head}")],
    )
    .await;

    let files: Vec<String> = result
        .stdout
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();

    Ok(json!({ "files": files }))
}
