//! 纯解析函数 — 不执行任何 IO。
//!
//! 移植自 `packages/web/server/lib/git/service.js` 中的各种解析逻辑。
//! 所有函数接收 git CLI 原始输出文本, 返回结构化数据。

use std::collections::HashMap;

use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::git::paths::clean_branch_name;

/// Diff 统计 (insertions / deletions)。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DiffStat {
    pub insertions: u64,
    pub deletions: u64,
}

/// `git status` 文件条目 (porcelain v1 格式)。
#[derive(Debug, Clone, Default)]
pub struct StatusFile {
    pub path: String,
    pub index: String,       // 暂存区状态码 (XY 中的 X)
    pub working_dir: String, // 工作区状态码 (XY 中的 Y)
    pub orig_path: Option<String>,
}

/// `git status -uall --porcelain=v1 -b` 解析结果。
#[derive(Debug, Clone, Default)]
pub struct ParsedStatus {
    /// 当前分支名 (无 refs/heads/ 前缀)。
    pub current: Option<String>,
    /// tracking 分支 (例如 origin/main)。
    pub tracking: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub files: Vec<StatusFile>,
    /// 是否有冲突文件 (状态码 U)。
    pub conflicted: Vec<String>,
}

/// `git log` 单条记录。
#[derive(Debug, Clone, Default, Serialize)]
pub struct LogEntry {
    pub hash: String,
    pub date: String,
    pub message: String,
    pub refs: String,
    pub body: String,
    pub author_name: String,
    pub author_email: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files_changed: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub insertions: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deletions: Option<u64>,
    pub parents: Vec<String>,
}

/// Worktree 条目 (porcelain 格式)。
#[derive(Debug, Clone, Default, Serialize)]
pub struct WorktreeEntry {
    pub head: String,
    pub name: String,
    pub branch: String,
    pub path: String,
}

// 用于 shortstat 解析的 regex
static RE_FILES_CHANGED: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(\d+)\s+files?\s+changed").unwrap());
static RE_INSERTIONS: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(\d+)\s+insertions?\(\+\)").unwrap());
static RE_DELETIONS: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(\d+)\s+deletions?\(-\)").unwrap());

/// shortstat 解析结果。
#[derive(Debug, Clone, Default)]
pub struct ShortStat {
    pub files_changed: u64,
    pub insertions: u64,
    pub deletions: u64,
    pub has_data: bool,
}

/// 解析 shortstat 行, 与 Node regex `/(\d+)\s+files?\s+changed/` 等对齐。
///
/// 输入例如: `" 3 files changed, 10 insertions(+), 2 deletions(-)"` 或空行。
pub fn parse_shortstat_line(line: &str) -> ShortStat {
    let files_changed = RE_FILES_CHANGED
        .captures(line)
        .and_then(|c| c.get(1).unwrap().as_str().parse::<u64>().ok())
        .unwrap_or(0);

    if files_changed == 0 {
        return ShortStat {
            files_changed: 0,
            insertions: 0,
            deletions: 0,
            has_data: false,
        };
    }

    let insertions = RE_INSERTIONS
        .captures(line)
        .and_then(|c| c.get(1).unwrap().as_str().parse::<u64>().ok())
        .unwrap_or(0);

    let deletions = RE_DELETIONS
        .captures(line)
        .and_then(|c| c.get(1).unwrap().as_str().parse::<u64>().ok())
        .unwrap_or(0);

    ShortStat {
        files_changed,
        insertions,
        deletions,
        has_data: true,
    }
}

/// 解析 `git diff --numstat` 输出。
///
/// 每行格式: `insertions\tdeletions\tpath` (binary 文件 insertions/deletions 为 `-`)。
/// 重命名行可能包含 `=>`。
pub fn parse_numstat(raw: &str) -> HashMap<String, DiffStat> {
    let mut map = HashMap::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let parts: Vec<&str> = trimmed.split('\t').collect();
        if parts.len() < 3 {
            continue;
        }
        let insertions_raw = parts[0];
        let deletions_raw = parts[1];
        let path_parts = &parts[2..];
        let path = path_parts.join("\t");
        if path.is_empty() {
            continue;
        }

        let insertions = if insertions_raw == "-" {
            0
        } else {
            insertions_raw.parse::<u64>().unwrap_or(0)
        };
        let deletions = if deletions_raw == "-" {
            0
        } else {
            deletions_raw.parse::<u64>().unwrap_or(0)
        };

        // 处理重命名: "old => new" 或 "prefix/{old => new}/suffix"
        let resolved_path = extract_numstat_destination_path(&path);

        let entry = map.entry(resolved_path).or_insert_with(DiffStat::default);
        entry.insertions += insertions;
        entry.deletions += deletions;
    }
    map
}

/// 从 numstat 路径中提取目标路径 (处理 `{old => new}` 重命名), 与 Node `extractGitNumstatDestinationPath` 对齐。
fn extract_numstat_destination_path(path: &str) -> String {
    // 查找 `{... => ...}` 模式
    if let Some(brace_start) = path.find('{') {
        if let Some(arrow) = path.find("=>") {
            if arrow > brace_start {
                if let Some(brace_end) = path[brace_start..].find('}') {
                    let brace_end_abs = brace_start + brace_end;
                    let prefix = &path[..brace_start];
                    let inner = &path[brace_start + 1..brace_end_abs];
                    let suffix = &path[brace_end_abs + 1..];

                    // inner 格式: "old => new"
                    let new_part = inner
                        .split("=>")
                        .nth(1)
                        .unwrap_or("")
                        .trim();

                    return format!("{prefix}{new_part}{suffix}");
                }
            }
        }
    }

    // 简单 "old => new" 格式
    if let Some(idx) = path.find("=>") {
        let new_part = path[idx + 2..].trim();
        return new_part.to_string();
    }

    path.to_string()
}

/// 解析 `git status -uall --porcelain=v1 -b` 输出。
///
/// `-b` 在第一行添加 branch 信息。后续行是文件状态。
pub fn parse_status_porcelain(raw: &str) -> ParsedStatus {
    let mut result = ParsedStatus::default();
    let lines: Vec<&str> = raw.lines().collect();

    let mut start_idx = 0;

    // 第一行可能是 branch 头信息 (## branch...)
    if !lines.is_empty() {
        let first = lines[0];
        if first.starts_with("## ") {
            parse_branch_header(first.trim_start_matches("## "), &mut result);
            start_idx = 1;
        }
    }

    for line in &lines[start_idx..] {
        if line.len() < 3 {
            continue;
        }

        let bytes = line.as_bytes();
        let index_code = bytes[0] as char;
        let work_code = bytes[1] as char;

        // porcelain v1: XY <space> path
        // 路径从第 3 字节开始 (索引 3, 因为 XY 占 2 字节, 空格占 1 字节)
        let path_raw = &line[3..];

        // 检查是否有 rename: "old -> new" 或 orig path (porcelain v1 中, R 状态用 \0 分隔 orig path)
        let (path, orig_path) = if let Some(sep) = path_raw.find(" -> ") {
            // rename: "old -> new"
            let old = path_raw[..sep].to_string();
            let new = path_raw[sep + 4..].to_string();
            // git 报告的是 new path, 但我们也保留 old
            (new, Some(old))
        } else {
            (path_raw.to_string(), None)
        };

        let status_file = StatusFile {
            path: path.clone(),
            index: index_code.to_string(),
            working_dir: work_code.to_string(),
            orig_path,
        };

        // 冲突检测: X 或 Y 是 U, 或 D DU/UD/AU/UA/AA/DD
        let is_conflict = is_conflict_status(index_code, work_code);
        if is_conflict {
            result.conflicted.push(path.clone());
        }

        result.files.push(status_file);
    }

    result
}

/// 检查 XY 状态码是否表示冲突。
fn is_conflict_status(x: char, y: char) -> bool {
    matches!(x, 'U') || matches!(y, 'U') ||
    (x == 'D' && y == 'D') || // DD
    (x == 'A' && y == 'A') || // AA
    (x == 'D' && y == 'U') || // DU
    (x == 'U' && y == 'D') || // UD
    (x == 'A' && y == 'U') || // AU
    (x == 'U' && y == 'A')    // UA
}

/// 解析 branch header 行 (## 后面的部分)。
///
/// 格式:
///   `main` — 当前分支, 无 upstream
///   `main...origin/main` — 有 tracking
///   `main...origin/main [ahead 2]` — ahead
///   `main...origin/main [ahead 2, behind 1]` — ahead + behind
///   `HEAD (no branch)` — detached HEAD
///   `No commits yet on main` — 新仓库
fn parse_branch_header(header: &str, result: &mut ParsedStatus) {
    let header = header.trim();

    // detached HEAD
    if header.starts_with("HEAD (no branch)") || header.starts_with("HEAD (detached") {
        return;
    }

    // "No commits yet on main"
    if let Some(rest) = header.strip_prefix("No commits yet on ") {
        result.current = Some(rest.trim().to_string());
        return;
    }

    // Initial commit on main
    if let Some(rest) = header.strip_prefix("Initial commit on ") {
        result.current = Some(rest.trim().to_string());
        return;
    }

    // 分割 branch...tracking [ahead/behind]
    let (branch_part, tracking_info) = match header.find('[') {
        Some(idx) => (&header[..idx], Some(&header[idx..])),
        None => (header, None),
    };

    // branch_part 可能是 "main" 或 "main...origin/main"
    if let Some(dots_idx) = branch_part.find("...") {
        let current = branch_part[..dots_idx].trim();
        let tracking = branch_part[dots_idx + 3..].trim();
        result.current = Some(current.to_string());
        result.tracking = Some(tracking.to_string());
    } else {
        result.current = Some(branch_part.trim().to_string());
    }

    // 解析 [ahead N, behind M]
    if let Some(info) = tracking_info {
        let info = info.trim_start_matches('[').trim_end_matches(']');
        for part in info.split(',') {
            let part = part.trim();
            if let Some(n) = part.strip_prefix("ahead ") {
                result.ahead = n.trim().parse::<u32>().unwrap_or(0);
            } else if let Some(n) = part.strip_prefix("behind ") {
                result.behind = n.trim().parse::<u32>().unwrap_or(0);
            }
        }
    }
}

/// 解析 `git log --pretty=format:%x1e...%x1f...` 输出。
///
/// `\x1e` (Record Separator) 分隔记录, `\x1f` (Unit Separator) 分隔字段。
/// 普通 mode 字段顺序: hash, parents, author_name, author_email, date, subject
/// all mode 字段顺序: hash, parents, author_name, author_email, date, subject, refs
pub fn parse_log_separated(raw: &str, is_all_mode: bool) -> Vec<LogEntry> {
    let mut entries = Vec::new();

    for record in raw.split('\x1e') {
        let record = record.trim_start_matches('\n');
        if record.is_empty() {
            continue;
        }

        let fields: Vec<&str> = record.split('\x1f').collect();
        if fields.len() < 6 {
            continue;
        }

        let hash = fields[0].to_string();
        let parents_str = fields.get(1).unwrap_or(&"");
        let author_name = fields.get(2).unwrap_or(&"").to_string();
        let author_email = fields.get(3).unwrap_or(&"").to_string();
        let date = fields.get(4).unwrap_or(&"").to_string();
        let message = fields.get(5).unwrap_or(&"").to_string();
        let refs = if is_all_mode { fields.get(6).unwrap_or(&"").to_string() } else { String::new() };

        let parents: Vec<String> = if parents_str.is_empty() {
            Vec::new()
        } else {
            parents_str.split_whitespace().map(|s| s.to_string()).collect()
        };

        entries.push(LogEntry {
            hash,
            date,
            message,
            refs,
            body: String::new(),
            author_name,
            author_email,
            files_changed: None,
            insertions: None,
            deletions: None,
            parents,
        });
    }

    entries
}

/// 解析 `git log` 带 shortstat 的输出。
///
/// 输出格式: 每条记录后跟一个 shortstat 行 (以空格开头的 ` N files changed, ...`)。
/// 记录之间用 `\x1e` 分隔, 字段用 `\x1f` 分隔。
pub fn parse_log_with_shortstat(raw: &str, is_all_mode: bool) -> Vec<LogEntry> {
    let mut entries = parse_log_separated(raw, is_all_mode);

    // shortstat 行嵌入在每条记录的末尾 (在最后一个字段后, 以 \n 分隔)
    // 我们需要重新解析, 因为 shortstat 行是 git --shortstat 追加的
    // 实际上 git --shortstat 会在每个 commit 的输出后追加 stat 行
    // 这些行以空格开头, 格式 " N file(s) changed, N insertions(+), N deletions(-)"
    //
    // 但由于我们用 \x1e 分隔记录, shortstat 行会出现在下一条记录的开头 (trim_start 后)
    // 所以我们在 parse_log_separated 中已经跳过了它们。
    // 需要一种不同的解析策略: 按行扫描。

    // 重新解析: 找到 shortstat 行并关联到前一条记录
    parse_log_shortstat_from_output(raw, &mut entries, is_all_mode);

    entries
}

/// 从原始输出中提取 shortstat 行并关联到 log entry。
fn parse_log_shortstat_from_output(raw: &str, entries: &mut [LogEntry], _is_all_mode: bool) {
    if entries.is_empty() {
        return;
    }

    // 按记录分隔符分割, 然后在每条记录内找 shortstat 行
    let records: Vec<&str> = raw.split('\x1e').collect();
    for (idx, record) in records.iter().enumerate() {
        if idx >= entries.len() {
            break;
        }
        // 在记录中查找 shortstat 行
        for line in record.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            // shortstat 行的特征: 以数字开头, 包含 "files? changed" 或 "insertions?" 或 "deletions?"
            if trimmed.contains("file") && trimmed.contains("changed") {
                let stat = parse_shortstat_line(trimmed);
                if stat.has_data {
                    entries[idx].files_changed = Some(stat.files_changed);
                    entries[idx].insertions = Some(stat.insertions);
                    entries[idx].deletions = Some(stat.deletions);
                }
            }
        }
    }
}

/// 解析 `git worktree list --porcelain` 输出。
///
/// 格式:
/// ```text
/// worktree /path/to/main
/// HEAD abc123...
/// branch refs/heads/main
///
/// worktree /path/to/linked
/// HEAD def456...
/// branch refs/heads/feature
/// ```
pub fn parse_worktree_porcelain(raw: &str) -> Vec<WorktreeEntry> {
    let mut entries = Vec::new();
    let mut current: Option<WorktreeEntry> = None;

    for line in raw.lines() {
        let line = line.trim();

        if line.is_empty() {
            if let Some(entry) = current.take() {
                entries.push(entry);
            }
            continue;
        }

        if let Some(rest) = line.strip_prefix("worktree ") {
            // 先保存前一个 entry
            if let Some(entry) = current.take() {
                entries.push(entry);
            }
            let path = rest.trim();
            let name = std::path::Path::new(path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            current = Some(WorktreeEntry {
                head: String::new(),
                name,
                branch: String::new(),
                path: path.to_string(),
            });
            continue;
        }

        if current.is_none() {
            continue;
        }

        if let Some(rest) = line.strip_prefix("HEAD ") {
            if let Some(ref mut entry) = current {
                entry.head = rest.trim().to_string();
            }
        } else if let Some(rest) = line.strip_prefix("branch ") {
            let branch_ref = rest.trim();
            if let Some(ref mut entry) = current {
                entry.branch = clean_branch_name(branch_ref);
            }
        }
    }

    if let Some(entry) = current {
        entries.push(entry);
    }

    // 过滤掉没有 path 的条目
    entries.into_iter().filter(|e| !e.path.is_empty()).collect()
}

/// 解析 `git rev-list --left-right --count HEAD...origin/branch` 的 "N\tM" 输出。
pub fn parse_ahead_behind(raw: &str) -> (u32, u32) {
    let parts: Vec<&str> = raw.split_whitespace().collect();
    if parts.len() < 2 {
        return (0, 0);
    }
    let ahead = parts[0].parse::<u32>().unwrap_or(0);
    let behind = parts[1].parse::<u32>().unwrap_or(0);
    (ahead, behind)
}

/// 解析 `git remote -v` 输出为 remote -> {fetch_url, push_url} 映射。
///
/// 输出格式 (TAB 分隔):
/// ```text
/// origin    git@github.com:user/repo.git (fetch)
/// origin    git@github.com:user/repo.git (push)
/// upstream  https://github.com/upstream/repo.git (fetch)
/// ```
pub fn parse_remotes_verbose(raw: &str) -> Vec<RemoteInfo> {
    let mut remote_map: HashMap<String, RemoteInfo> = HashMap::new();

    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // 格式: "name<TAB>url (type)" 或 "name url (type)"
        let parts: Vec<&str> = trimmed.split_whitespace().collect();
        if parts.len() < 2 {
            continue;
        }

        let name = parts[0].to_string();
        let url_with_type = parts[1..].join(" ");

        // 提取 URL 和类型 (fetch/push)
        let (url, rtype) = if let Some(open) = url_with_type.rfind('(') {
            let url = url_with_type[..open].trim().to_string();
            let type_str = url_with_type[open + 1..].trim_end_matches(')').trim();
            (url, type_str.to_string())
        } else {
            (url_with_type.trim().to_string(), String::new())
        };

        let entry = remote_map.entry(name.clone()).or_insert_with(|| RemoteInfo {
            name,
            fetch_url: None,
            push_url: None,
        });

        if rtype == "fetch" {
            entry.fetch_url = Some(url);
        } else if rtype == "push" {
            entry.push_url = Some(url);
        } else if entry.fetch_url.is_none() {
            entry.fetch_url = Some(url);
        }
    }

    let mut result: Vec<RemoteInfo> = remote_map.into_values().collect();
    result.sort_by(|a, b| a.name.cmp(&b.name));
    result
}

/// Remote 信息。
#[derive(Debug, Clone, Serialize)]
pub struct RemoteInfo {
    pub name: String,
    pub fetch_url: Option<String>,
    pub push_url: Option<String>,
}

/// 解析 `git show --name-status --format=` 输出为 path -> change_type 映射。
///
/// 输出格式:
/// ```text
/// M\tpath/to/modified
/// A\tpath/to/added
/// D\tpath/to/deleted
/// R100\told\tnew     (rename)
/// C100\told\tnew     (copy)
/// ```
pub fn parse_name_status(raw: &str) -> HashMap<String, String> {
    let mut result = HashMap::new();

    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // 格式: STATUS\tpath 或 STATUS\tscore\told\tnew
        let parts: Vec<&str> = trimmed.split('\t').collect();
        if parts.is_empty() {
            continue;
        }

        let status_raw = parts[0];

        // 提取基本状态码 (R100 -> R, C90 -> C)
        let status_code = status_raw
            .chars()
            .next()
            .unwrap_or('?')
            .to_string();

        let path = if (status_code == "R" || status_code == "C") && parts.len() >= 3 {
            // rename/copy: 最后一个路径是目标
            parts.last().unwrap().to_string()
        } else if parts.len() >= 2 {
            parts[1].to_string()
        } else {
            continue;
        };

        result.insert(path, status_code);
    }

    result
}

/// 解析 `git stash list --format=%gd%x1f%gs%x1f%cr%x1f%H` 输出。
pub fn parse_stash_list(raw: &str) -> Vec<StashEntry> {
    let mut entries = Vec::new();

    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let fields: Vec<&str> = trimmed.split('\x1f').collect();
        if fields.len() < 4 {
            continue;
        }

        entries.push(StashEntry {
            r#ref: fields[0].to_string(),
            message: fields[1].to_string(),
            relative_date: fields[2].to_string(),
            hash: fields[3].to_string(),
        });
    }

    entries
}

/// Stash 条目。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StashEntry {
    pub r#ref: String,
    pub message: String,
    pub relative_date: String,
    pub hash: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_shortstat_line() {
        let stat = parse_shortstat_line(" 3 files changed, 10 insertions(+), 2 deletions(-)");
        assert_eq!(stat.files_changed, 3);
        assert_eq!(stat.insertions, 10);
        assert_eq!(stat.deletions, 2);
        assert!(stat.has_data);

        let empty = parse_shortstat_line("");
        assert!(!empty.has_data);

        let only_files = parse_shortstat_line(" 1 file changed");
        assert_eq!(only_files.files_changed, 1);
        assert_eq!(only_files.insertions, 0);
        assert_eq!(only_files.deletions, 0);
        assert!(only_files.has_data);
    }

    #[test]
    fn test_parse_numstat() {
        let raw = "10\t2\tsrc/main.rs\n-\t-\timage.png\n5\t0\tsrc/lib.rs";
        let map = parse_numstat(raw);
        assert_eq!(map.get("src/main.rs").unwrap().insertions, 10);
        assert_eq!(map.get("src/main.rs").unwrap().deletions, 2);
        assert_eq!(map.get("image.png").unwrap().insertions, 0); // binary
        assert_eq!(map.get("src/lib.rs").unwrap().insertions, 5);
    }

    #[test]
    fn test_parse_numstat_rename() {
        let raw = "5\t2\t{old => new}/file.rs";
        let map = parse_numstat(raw);
        assert!(map.contains_key("new/file.rs"));
        assert_eq!(map.get("new/file.rs").unwrap().insertions, 5);
    }

    #[test]
    fn test_parse_status_porcelain_basic() {
        let raw = "## main...origin/main [ahead 2]\n\
                   M  staged_file.txt\n\
                   M  modified_file.txt\n\
                   ?? untracked.txt\n\
                   A  new_file.txt";
        let status = parse_status_porcelain(raw);
        assert_eq!(status.current.as_deref(), Some("main"));
        assert_eq!(status.tracking.as_deref(), Some("origin/main"));
        assert_eq!(status.ahead, 2);
        assert_eq!(status.behind, 0);
        assert_eq!(status.files.len(), 4);
        assert_eq!(status.files[0].path, "staged_file.txt");
        assert_eq!(status.files[0].index, "M");
        assert_eq!(status.files[0].working_dir, " ");
        assert_eq!(status.files[2].index, "?");
        assert_eq!(status.files[2].working_dir, "?");
    }

    #[test]
    fn test_parse_status_porcelain_conflict() {
        let raw = "## main\nUU conflict.txt\nDD both_deleted.txt";
        let status = parse_status_porcelain(raw);
        assert_eq!(status.current.as_deref(), Some("main"));
        assert!(status.tracking.is_none());
        assert_eq!(status.conflicted.len(), 2);
        assert!(status.conflicted.contains(&"conflict.txt".to_string()));
    }

    #[test]
    fn test_parse_status_porcelain_detached() {
        let raw = "## HEAD (no branch)\nM  file.txt";
        let status = parse_status_porcelain(raw);
        assert!(status.current.is_none());
        assert_eq!(status.files.len(), 1);
    }

    #[test]
    fn test_parse_branch_header_ahead_behind() {
        let mut result = ParsedStatus::default();
        parse_branch_header("main...origin/main [ahead 2, behind 1]", &mut result);
        assert_eq!(result.current.as_deref(), Some("main"));
        assert_eq!(result.tracking.as_deref(), Some("origin/main"));
        assert_eq!(result.ahead, 2);
        assert_eq!(result.behind, 1);
    }

    #[test]
    fn test_parse_log_separated() {
        let record_sep = '\x1e'.to_string();
        let field_sep = '\x1f'.to_string();
        let raw = format!(
            "{rs}abc1234{fs}parent1 parent2{fs}Alice{fs}alice@example.com{fs}2024-01-01{fs}Fix bug{rs}def5678{fs}{fs}Bob{fs}bob@example.com{fs}2024-01-02{fs}Add feature",
            rs = record_sep,
            fs = field_sep
        );
        let entries = parse_log_separated(&raw, false);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].hash, "abc1234");
        assert_eq!(entries[0].parents, vec!["parent1", "parent2"]);
        assert_eq!(entries[0].author_name, "Alice");
        assert_eq!(entries[0].message, "Fix bug");
        assert_eq!(entries[1].hash, "def5678");
        assert!(entries[1].parents.is_empty());
    }

    #[test]
    fn test_parse_worktree_porcelain() {
        let raw = "worktree /home/user/main\n\
                   HEAD abc1234567890abcdef\n\
                   branch refs/heads/main\n\
                   \n\
                   worktree /home/user/feature\n\
                   HEAD def456789012345678\n\
                   branch refs/heads/feature/x";
        let entries = parse_worktree_porcelain(raw);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].path, "/home/user/main");
        assert_eq!(entries[0].name, "main");
        assert_eq!(entries[0].branch, "main");
        assert_eq!(entries[0].head, "abc1234567890abcdef");
        assert_eq!(entries[1].path, "/home/user/feature");
        assert_eq!(entries[1].branch, "feature/x");
    }

    #[test]
    fn test_parse_ahead_behind() {
        assert_eq!(parse_ahead_behind("2\t3"), (2, 3));
        assert_eq!(parse_ahead_behind("0\t0"), (0, 0));
        assert_eq!(parse_ahead_behind("invalid"), (0, 0));
    }

    #[test]
    fn test_parse_remotes_verbose() {
        let raw = "origin\tgit@github.com:user/repo.git (fetch)\n\
                   origin\tgit@github.com:user/repo.git (push)\n\
                   upstream\thttps://github.com/up/repo.git (fetch)";
        let remotes = parse_remotes_verbose(raw);
        assert_eq!(remotes.len(), 2);
        assert_eq!(remotes[0].name, "origin");
        assert_eq!(
            remotes[0].fetch_url.as_deref(),
            Some("git@github.com:user/repo.git")
        );
        assert_eq!(remotes[1].name, "upstream");
    }

    #[test]
    fn test_parse_name_status() {
        let raw = "M\tsrc/main.rs\nA\tsrc/new.rs\nD\tsrc/old.rs";
        let map = parse_name_status(raw);
        assert_eq!(map.get("src/main.rs"), Some(&"M".to_string()));
        assert_eq!(map.get("src/new.rs"), Some(&"A".to_string()));
        assert_eq!(map.get("src/old.rs"), Some(&"D".to_string()));
    }

    #[test]
    fn test_parse_stash_list() {
        let raw = "stash@{0}\x1fWIP on main: abc1234\x1f2 hours ago\x1fdef5678";
        let entries = parse_stash_list(raw);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].r#ref, "stash@{0}");
        assert_eq!(entries[0].message, "WIP on main: abc1234");
        assert_eq!(entries[0].relative_date, "2 hours ago");
        assert_eq!(entries[0].hash, "def5678");
    }
}
