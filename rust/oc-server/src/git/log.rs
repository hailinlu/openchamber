//! Log 操作 — get_log, get_commit_summaries, get_commit_files。
//!
//! 移植自 Node `service.js` 的 log 相关函数。

use serde_json::{json, Value};

use crate::git::context::RepoContext;
use crate::git::parsing::{parse_log_with_shortstat, LogEntry};
use crate::git::runner::GitRunner;

/// Log 查询选项。
pub struct LogOptions {
    pub max_count: Option<u32>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub file: Option<String>,
    pub all: bool,
}

impl Default for LogOptions {
    fn default() -> Self {
        Self {
            max_count: Some(50),
            from: None,
            to: None,
            file: None,
            all: false,
        }
    }
}

/// 获取 commit 历史, 与 Node `getLog` 对齐。
///
/// 返回 `{ all: [...], latest: {...}, total: N }`。
pub async fn get_log(directory: &str, options: LogOptions) -> oc_core::Result<Value> {
    let repo_ctx = RepoContext::create(directory).await?;
    let repo_root = &repo_ctx.repo_root;

    let record_sep = "\x1e";
    let field_sep = "\x1f";

    let mut args: Vec<String> = vec![
        "log".to_string(),
        format!(
            "--pretty=format:{}%H{}%P{}%an{}%ae{}%ad{}%s",
            record_sep, field_sep, field_sep, field_sep, field_sep, field_sep
        ),
    ];

    if options.all {
        args[1] = format!(
            "--pretty=format:{}%H{}%P{}%an{}%ae{}%ad{}%s{}%D",
            record_sep, field_sep, field_sep, field_sep, field_sep, field_sep, field_sep
        );
        args.push("--all".to_string());
        args.push("--topo-order".to_string());
    }

    // max_count
    let max_count = options.max_count.unwrap_or(50);
    args.push(format!("-n{max_count}"));

    // from..to range
    if let (Some(from), Some(to)) = (&options.from, &options.to) {
        args.push(format!("{from}..{to}"));
    } else if let Some(from) = &options.from {
        // 只有 from
        let _ = from; // from 参数保留但不加范围 (与 Node 行为一致)
    }

    // file path
    if let Some(file) = &options.file {
        args.push("--".to_string());
        args.push(file.clone());
    }

    // shortstat
    args.push("--shortstat".to_string());

    let args_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let result = GitRunner::run(repo_root, &args_refs).await;

    if !result.success {
        // 空 repo 返回空
        return Ok(json!({
            "all": [],
            "latest": null,
            "total": 0,
        }));
    }

    let entries: Vec<LogEntry> = parse_log_with_shortstat(&result.stdout, options.all);

    let total = entries.len();
    let latest = entries.first().map(log_entry_to_json);
    let all: Vec<Value> = entries.iter().map(log_entry_to_json).collect();

    Ok(json!({
        "all": all,
        "latest": latest,
        "total": total,
    }))
}

fn log_entry_to_json(entry: &LogEntry) -> Value {
    json!({
        "hash": entry.hash,
        "date": entry.date,
        "message": entry.message,
        "refs": entry.refs,
        "body": entry.body,
        "author_name": entry.author_name,
        "author_email": entry.author_email,
        "files_changed": entry.files_changed,
        "insertions": entry.insertions,
        "deletions": entry.deletions,
        "parents": entry.parents,
    })
}

/// 获取 commit summaries (批量), 与 Node `getCommitSummaries` 对齐。
pub async fn get_commit_summaries(
    directory: &str,
    shas: &[String],
) -> oc_core::Result<Value> {
    let repo_ctx = RepoContext::create(directory).await?;
    let repo_root = &repo_ctx.repo_root;

    let commits: Vec<String> = shas
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    if commits.is_empty() {
        return Ok(json!({ "commits": [] }));
    }

    // 验证 SHA 格式
    for sha in &commits {
        if !crate::git::paths::is_valid_commit_sha(sha) {
            return Err(oc_core::Error::BadRequest("Invalid commit SHA".to_string()));
        }
    }

    let mut args: Vec<String> = vec![
        "show".to_string(),
        "-s".to_string(),
        "--format=%H%x09%h%x09%s".to_string(),
    ];
    args.extend(commits.iter().cloned());
    args.push("--".to_string());

    let args_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let result = GitRunner::run_or_throw(repo_root, &args_refs, "Failed to get commit summaries").await?;

    let parsed: Vec<Value> = result
        .stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|line| {
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() >= 3 {
                Some(json!({
                    "sha": parts[0],
                    "short": parts[1],
                    "subject": parts[2],
                }))
            } else {
                None
            }
        })
        .filter(|entry| {
            entry["sha"].as_str().map(|s| !s.is_empty()).unwrap_or(false)
                && entry["short"].as_str().map(|s| !s.is_empty()).unwrap_or(false)
        })
        .collect();

    Ok(json!({ "commits": parsed }))
}

/// 获取 commit 变更文件列表, 与 Node `getCommitFiles` 对齐。
pub async fn get_commit_files(
    directory: &str,
    commit_hash: &str,
) -> oc_core::Result<Value> {
    let repo_ctx = RepoContext::create(directory).await?;
    let repo_root = &repo_ctx.repo_root;

    // numstat
    let numstat_result = GitRunner::run(
        repo_root,
        &["show", "--numstat", "--format=", commit_hash],
    )
    .await;

    // name-status
    let name_status_result = GitRunner::run(
        repo_root,
        &["show", "--name-status", "--format=", commit_hash],
    )
    .await;

    // 解析 name-status → path → change_type
    let change_types = crate::git::parsing::parse_name_status(&name_status_result.stdout);

    // 解析 numstat → path → stats
    let numstats = crate::git::parsing::parse_numstat(&numstat_result.stdout);

    let mut files = Vec::new();

    for (path, stat) in &numstats {
        let change_type = change_types.get(path).cloned().unwrap_or_default();
        let is_binary = numstat_result
            .stdout
            .lines()
            .any(|line| {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    return false;
                }
                let parts: Vec<&str> = trimmed.split('\t').collect();
                parts.len() >= 3
                    && parts[0] == "-"
                    && parts[1] == "-"
                    && parts[2..].join("\t") == *path
            });

        files.push(json!({
            "path": path,
            "insertions": stat.insertions,
            "deletions": stat.deletions,
            "isBinary": is_binary,
            "changeType": change_type,
        }));
    }

    Ok(json!({ "files": files }))
}
