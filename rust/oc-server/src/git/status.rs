//! `get_status` — 最复杂的 git 函数。
//!
//! 移植自 Node `service.js getStatus` (~265 行)。
//! 输出格式与 `simple-git` 的 `git.status()` 返回值对齐。

use std::collections::HashMap;
use std::path::Path;

use serde_json::{json, Value};

use crate::git::context::RepoContext;
use crate::git::parsing::{parse_numstat, parse_status_porcelain, DiffStat};
use crate::git::runner::{is_missing_directory_error, is_not_git_repository_error, GitRunner};

/// Status 查询选项。
#[derive(Default)]
pub struct StatusOptions {
    pub mode: Option<String>, // "light" 或 None
}


/// 获取仓库状态, 与 Node `getStatus` 对齐。
pub async fn get_status(directory: &str, options: StatusOptions) -> oc_core::Result<Value> {
    let light_mode = options.mode.as_deref() == Some("light");

    let repo_ctx = RepoContext::create(directory).await?;
    let repo_root = &repo_ctx.repo_root;

    // 1. git status -uall --porcelain=v1 -b
    let status_result = GitRunner::run(repo_root, &["status", "-uall", "--porcelain=v1", "-b"]).await;

    if !status_result.success {
        let error_text = status_result.stderr_text();
        if is_not_git_repository_error(&error_text) || is_missing_directory_error(&error_text) {
            // 返回空状态
            return Ok(json!({
                "current": null,
                "tracking": null,
                "ahead": 0,
                "behind": 0,
                "files": [],
                "isClean": true,
            }));
        }
        return Err(oc_core::Error::Internal(format!(
            "failed to get status: {error_text}"
        )));
    }

    let parsed_status = parse_status_porcelain(&status_result.stdout);

    // 2. numstat (light mode 跳过)
    let mut diff_stats: HashMap<String, DiffStat> = HashMap::new();

    if !light_mode {
        let staged_stats = GitRunner::run(repo_root, &["diff", "--cached", "--numstat"]).await;
        let working_stats = GitRunner::run(repo_root, &["diff", "--numstat"]).await;

        // 合并 staged + working stats
        for raw in [staged_stats.stdout.as_str(), working_stats.stdout.as_str()] {
            for (path, stat) in parse_numstat(raw) {
                let entry = diff_stats.entry(path).or_default();
                entry.insertions += stat.insertions;
                entry.deletions += stat.deletions;
            }
        }
    }

    // 3. 新文件行数统计 (light mode 跳过)
    if !light_mode {
        count_new_file_stats(repo_root, &parsed_status, &mut diff_stats).await;
    }

    // 4. ahead/behind fallback (无 tracking 时)
    let tracking = parsed_status.tracking.clone();
    let mut ahead = parsed_status.ahead;
    let mut behind = parsed_status.behind;

    if !light_mode && tracking.is_none() && parsed_status.current.is_some() {
        if let Some(base_ref) = select_base_ref_for_unpublished(repo_root).await {
            let count_raw = GitRunner::run(
                repo_root,
                &["rev-list", "--count", &format!("{base_ref}..HEAD")],
            )
            .await
            .stdout_text();

            if let Ok(count) = count_raw.trim().parse::<u32>() {
                ahead = count;
                behind = 0;
            }
        }
    }

    // 5. upstream remote 比较
    let upstream_comparison = if !light_mode
        && parsed_status.current.is_some()
        && tracking
            .as_ref()
            .map(|t| !t.starts_with("upstream/"))
            .unwrap_or(true)
        && has_remote(repo_root, "upstream").await
    {
        get_remote_branch_comparison(
            repo_root,
            "upstream",
            parsed_status.current.as_deref().unwrap_or(""),
        )
        .await
    } else {
        None
    };

    // 6. merge/rebase 检测
    let merge_in_progress = detect_merge_in_progress(repo_root).await;
    let rebase_in_progress = detect_rebase_in_progress(repo_root).await;

    // 构建 files 数组
    let files: Vec<Value> = parsed_status
        .files
        .iter()
        .map(|f| {
            json!({
                "path": f.path,
                "index": f.index,
                "working_dir": f.working_dir,
            })
        })
        .collect();

    let is_clean = files.is_empty();

    // 构建 diffStats (light mode 为 null)
    let diff_stats_json = if light_mode {
        Value::Null
    } else {
        let mut map = serde_json::Map::new();
        for (path, stat) in &diff_stats {
            map.insert(
                path.clone(),
                json!({
                    "insertions": stat.insertions,
                    "deletions": stat.deletions,
                }),
            );
        }
        Value::Object(map)
    };

    let mut result = json!({
        "current": parsed_status.current,
        "tracking": tracking,
        "ahead": ahead,
        "behind": behind,
        "files": files,
        "isClean": is_clean,
        "diffStats": diff_stats_json,
    });

    if let Some(mi) = merge_in_progress {
        result["mergeInProgress"] = mi;
    }
    if let Some(ri) = rebase_in_progress {
        result["rebaseInProgress"] = ri;
    }
    if let Some(uc) = upstream_comparison {
        result["upstreamComparison"] = uc;
    }

    Ok(result)
}

/// 为新文件 (untracked/new) 统计行数, 与 Node getStatus 内联逻辑对齐。
///
/// 对于 status 为 `?` 或 `A` 且 numstat 没有数据的文件, 读文件内容计算行数。
async fn count_new_file_stats(
    repo_root: &Path,
    parsed_status: &crate::git::parsing::ParsedStatus,
    diff_stats: &mut HashMap<String, DiffStat>,
) {
    use crate::git::MAX_NEW_FILE_STATS;
    use crate::git::MAX_NEW_FILE_STAT_SIZE;

    let mut count = 0usize;

    for file in &parsed_status.files {
        if count >= MAX_NEW_FILE_STATS {
            break;
        }

        let working = file.working_dir.trim();
        let index_status = file.index.trim();
        let status_code = if !working.is_empty() { working } else { index_status };

        if status_code != "?" && status_code != "A" {
            continue;
        }

        // 如果已有 numstat 数据, 跳过
        if let Some(existing) = diff_stats.get(&file.path) {
            if existing.insertions > 0 {
                continue;
            }
        }

        let absolute_path = repo_root.join(&file.path);

        let stat = match tokio::fs::metadata(&absolute_path).await {
            Ok(s) => s,
            Err(_) => continue,
        };

        if !stat.is_file() || stat.len() > MAX_NEW_FILE_STAT_SIZE {
            continue;
        }

        count += 1;

        let buffer = match tokio::fs::read(&absolute_path).await {
            Ok(b) => b,
            Err(_) => continue,
        };

        // 检测 binary (NUL 字节)
        if buffer.contains(&0u8) {
            let _entry = diff_stats.entry(file.path.clone()).or_default();
            // 已有的 insertions/deletions 保留 (从 numstat 可能拿到 binary 的 0/0)
            continue;
        }

        let content = String::from_utf8_lossy(&buffer);
        let normalized = content.replace("\r\n", "\n");

        if normalized.is_empty() {
            let entry = diff_stats.entry(file.path.clone()).or_default();
            entry.insertions = 0;
            entry.deletions = 0;
            continue;
        }

        let mut segments: Vec<&str> = normalized.split('\n').collect();
        if normalized.ends_with('\n') {
            segments.pop();
        }

        let line_count = segments.len() as u64;
        let entry = diff_stats.entry(file.path.clone()).or_default();
        entry.insertions = line_count;
        entry.deletions = 0;
    }
}

/// 选择未发布提交的 base ref, 与 Node `selectBaseRefForUnpublished` 对齐。
///
/// 优先级: origin/HEAD → origin/main → origin/master → main → master
async fn select_base_ref_for_unpublished(repo_root: &Path) -> Option<String> {
    let mut candidates = Vec::new();

    // 尝试 origin/HEAD
    let origin_head = GitRunner::run(
        repo_root,
        &["symbolic-ref", "-q", "refs/remotes/origin/HEAD"],
    )
    .await
    .stdout_text();

    if !origin_head.is_empty() {
        let trimmed = origin_head.trim();
        let stripped = trimmed
            .strip_prefix("refs/remotes/")
            .unwrap_or(trimmed)
            .to_string();
        candidates.push(stripped);
    }

    candidates.push("origin/main".to_string());
    candidates.push("origin/master".to_string());
    candidates.push("main".to_string());
    candidates.push("master".to_string());

    for ref_name in &candidates {
        let result = GitRunner::run(repo_root, &["rev-parse", "--verify", ref_name]).await;
        if result.success && !result.stdout_text().is_empty() {
            return Some(ref_name.clone());
        }
    }

    None
}

/// 检查 remote 是否存在, 与 Node `hasRemote` 对齐 (带缓存)。
pub async fn has_remote(repo_root: &Path, remote_name: &str) -> bool {
    let result = GitRunner::run(
        repo_root,
        &["remote", "get-url", remote_name],
    )
    .await;
    result.success
}

/// 获取远程分支比较, 与 Node `getRemoteBranchComparison` 对齐。
pub async fn get_remote_branch_comparison(
    repo_root: &Path,
    remote: &str,
    branch: &str,
) -> Option<Value> {
    let remote_ref = format!("{remote}/{branch}");

    // 验证 remote ref 存在
    let verify = GitRunner::run(repo_root, &["rev-parse", "--verify", &remote_ref]).await;
    if !verify.success {
        return None;
    }

    // ahead/behind: rev-list --left-right --count HEAD...remoteRef
    let count_raw = GitRunner::run(
        repo_root,
        &["rev-list", "--left-right", "--count", &format!("HEAD...{remote_ref}")],
    )
    .await
    .stdout_text();

    let (ahead, behind) = if count_raw.is_empty() {
        (0, 0)
    } else {
        let parts: Vec<&str> = count_raw.split_whitespace().collect();
        if parts.len() >= 2 {
            (
                parts[0].parse::<u32>().unwrap_or(0),
                parts[1].parse::<u32>().unwrap_or(0),
            )
        } else {
            (0, 0)
        }
    };

    Some(json!({
        "remote": remote,
        "branch": branch,
        "ahead": ahead,
        "behind": behind,
    }))
}

/// 检测 merge in progress, 与 Node getStatus 内联逻辑对齐。
async fn detect_merge_in_progress(repo_root: &Path) -> Option<Value> {
    // 检查 MERGE_HEAD
    let merge_head_exists = GitRunner::run(
        repo_root,
        &["rev-parse", "--verify", "--quiet", "MERGE_HEAD"],
    )
    .await;

    if !merge_head_exists.success {
        return None;
    }

    let merge_head = GitRunner::run(repo_root, &["rev-parse", "MERGE_HEAD"])
        .await
        .stdout_text();
    let head_sha = merge_head.trim().chars().take(7).collect::<String>();

    if head_sha.is_empty() {
        return None;
    }

    // 读取 MERGE_MSG
    let git_dir = GitRunner::run(repo_root, &["rev-parse", "--git-dir"])
        .await
        .stdout_text();
    let merge_msg = if !git_dir.is_empty() {
        let merge_msg_path = if Path::new(&git_dir).is_absolute() {
            Path::new(&git_dir).join("MERGE_MSG")
        } else {
            repo_root.join(&git_dir).join("MERGE_MSG")
        };
        tokio::fs::read_to_string(&merge_msg_path)
            .await
            .unwrap_or_default()
            .lines()
            .next()
            .unwrap_or("")
            .to_string()
    } else {
        String::new()
    };

    Some(json!({
        "head": head_sha,
        "message": merge_msg,
    }))
}

/// 检测 rebase in progress, 与 Node getStatus 内联逻辑对齐。
async fn detect_rebase_in_progress(repo_root: &Path) -> Option<Value> {
    // 获取 git-dir
    let git_dir_raw = GitRunner::run(repo_root, &["rev-parse", "--git-dir"])
        .await
        .stdout_text();
    if git_dir_raw.is_empty() {
        return None;
    }

    let git_dir = if Path::new(&git_dir_raw).is_absolute() {
        Path::new(&git_dir_raw).to_path_buf()
    } else {
        repo_root.join(&git_dir_raw)
    };

    // 检查 rebase-merge 或 rebase-apply
    let rebase_merge = git_dir.join("rebase-merge");
    let rebase_apply = git_dir.join("rebase-apply");

    let rebase_path = if rebase_merge.exists() {
        rebase_merge
    } else if rebase_apply.exists() {
        rebase_apply
    } else {
        return None;
    };

    let head_name = tokio::fs::read_to_string(rebase_path.join("head-name"))
        .await
        .unwrap_or_default()
        .trim()
        .replace("refs/heads/", "");

    let onto = tokio::fs::read_to_string(rebase_path.join("onto"))
        .await
        .unwrap_or_default()
        .trim()
        .chars()
        .take(7)
        .collect::<String>();

    if head_name.is_empty() && onto.is_empty() {
        return None;
    }

    Some(json!({
        "headName": head_name,
        "onto": onto,
    }))
}

/// 检查目录是否是 git 仓库, 与 Node `isGitRepository` 对齐。
pub async fn is_git_repository(directory: &str) -> bool {
    let dir = crate::git::paths::normalize_directory_path(directory);
    if dir.is_empty() {
        return false;
    }
    GitRunner::is_git_repo(Path::new(&dir)).await
}
