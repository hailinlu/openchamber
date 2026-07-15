//! 路径与验证工具函数。
//!
//! 移植自 `packages/web/server/lib/git/service.js` 的路径辅助函数。

use std::path::{Path, PathBuf};

/// 展开 `~` 前缀并去除首尾空白, 与 Node `normalizeDirectoryPath` 对齐。
pub fn normalize_directory_path(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    if trimmed == "~" {
        return home_dir_string();
    }

    if let Some(rest) = trimmed.strip_prefix("~/").or_else(|| trimmed.strip_prefix("~/")) {
        return join_home(rest);
    }

    trimmed.to_string()
}

/// `normalizeDirectoryPath` + 反斜杠转正斜杠, 与 Node `normalizePath` 对齐。
pub fn normalize_path(value: &str) -> String {
    let normalized = normalize_directory_path(value);
    if normalized.is_empty() {
        return normalized;
    }
    normalized.replace('\\', "/")
}

/// home 目录字符串 (fallback ".")。
pub fn home_dir_string() -> String {
    std::env::var("HOME").unwrap_or_else(|_| ".".to_string())
}

/// home 目录 PathBuf。
pub fn home_dir() -> PathBuf {
    PathBuf::from(home_dir_string())
}

fn join_home(rest: &str) -> String {
    let home = home_dir_string();
    // 模拟 path.join(home, rest) — 用 / 拼接然后 normalize
    let joined = format!("{home}/{rest}");
    joined
}

/// 验证 commit hash 格式: 7-40 个十六进制字符, 与 Node `isValidCommitHash` 对齐。
pub fn is_valid_commit_hash(hash: &str) -> bool {
    let len = hash.len();
    (7..=40).contains(&len) && hash.chars().all(|c| c.is_ascii_hexdigit())
}

/// 验证 commit SHA 格式 (更宽松): 4-64 个十六进制字符, 与 Node `getCommitSummaries` 对齐。
pub fn is_valid_commit_sha(sha: &str) -> bool {
    let len = sha.len();
    (4..=64).contains(&len) && sha.chars().all(|c| c.is_ascii_hexdigit())
}

/// 去重并过滤文件路径列表, 与 Node `normalizeFilePathList` 对齐。
pub fn normalize_file_path_list(paths: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut result = Vec::new();
    for path in paths {
        let trimmed = path.trim();
        if trimmed.is_empty() {
            continue;
        }
        if seen.insert(trimmed.to_string()) {
            result.push(trimmed.to_string());
        }
    }
    result
}

/// 检查 `child` 路径是否在 `parent` 目录内 (或等于 parent), 词法比较不碰文件系统。
/// 与 Node `isInsideOrSameDirectory` 对齐。
pub fn is_inside_or_same_directory(child: &Path, parent: &Path) -> bool {
    let child_normalized = normalize_lexical(child);
    let parent_normalized = normalize_lexical(parent);

    if child_normalized == parent_normalized {
        return true;
    }

    let prefix = format!("{}/", parent_normalized);
    child_normalized.starts_with(&prefix)
}

/// 词法 normalize: 合并 `.` 和 `..` (不碰文件系统), 统一为正斜杠。
fn normalize_lexical(path: &Path) -> String {
    let mut components: Vec<&str> = Vec::new();

    let path_str = path.to_string_lossy().replace('\\', "/");
    let is_absolute = path_str.starts_with('/');

    for component in path_str.split('/') {
        if component.is_empty() || component == "." {
            continue;
        }
        if component == ".." {
            // 只在非根目录下弹出
            if !components.is_empty() {
                components.pop();
            }
            continue;
        }
        components.push(component);
    }

    let joined = components.join("/");
    if is_absolute {
        format!("/{joined}")
    } else {
        joined
    }
}

/// 清理分支名: 去掉 `refs/heads/` / `refs/remotes/` 前缀, 与 Node `cleanBranchName` 对齐。
pub fn clean_branch_name(value: &str) -> String {
    let trimmed = value.trim();
    let result = trimmed
        .strip_prefix("refs/heads/")
        .or_else(|| trimmed.strip_prefix("refs/remotes/"))
        .unwrap_or(trimmed);
    result.to_string()
}

/// Slug 化 worktree 名称, 与 Node `slugWorktreeName` 对齐。
pub fn slug_worktree_name(value: &str) -> String {
    let mut result: String = value
        .trim()
        .strip_prefix("refs/heads/")
        .or_else(|| value.trim().strip_prefix("heads/"))
        .unwrap_or(value.trim())
        .replace(|c: char| c.is_whitespace(), "-");

    // 去掉首尾 /
    while result.starts_with('/') || result.ends_with('/') {
        result = result.trim_matches('/').to_string();
    }

    // / → -
    result = result.replace('/', "-");

    // 非 [A-Za-z0-9._-] → -
    result = result
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();

    // 合并连续 -
    while result.contains("--") {
        result = result.replace("--", "-");
    }

    // 去首尾 -
    result = result.trim_matches('-').to_string();

    // 截断到 80 字符
    result.chars().take(80).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_directory_path_tilde() {
    std::env::set_var("HOME", "/home/user");
        assert_eq!(normalize_directory_path("~/foo"), "/home/user/foo");
        assert_eq!(normalize_directory_path("~"), "/home/user");
        assert_eq!(normalize_directory_path("  /abs/path  "), "/abs/path");
        assert_eq!(normalize_directory_path(""), "");
    }

    #[test]
    fn test_normalize_path_backslash() {
        std::env::set_var("HOME", "/home/user");
        assert_eq!(normalize_path("C:\\Users\\foo"), "C:/Users/foo");
        assert_eq!(normalize_path("~/proj"), "/home/user/proj");
    }

    #[test]
    fn test_is_valid_commit_hash() {
        assert!(is_valid_commit_hash("abc1234"));
        assert!(is_valid_commit_hash("abcdef1234567890abcdef1234567890abcdef12"));
        assert!(!is_valid_commit_hash("abc123")); // 6 chars
        assert!(!is_valid_commit_hash("abcdef1234567890abcdef1234567890abcdef123")); // 41 chars
        assert!(!is_valid_commit_hash("xyz1234"));
    }

    #[test]
    fn test_normalize_file_path_list() {
        let result = normalize_file_path_list(&[
            "foo.txt".to_string(),
            "  bar.txt  ".to_string(),
            "".to_string(),
            "foo.txt".to_string(),
        ]);
        assert_eq!(result, vec!["foo.txt", "bar.txt"]);
    }

    #[test]
    fn test_is_inside_or_same_directory() {
        assert!(is_inside_or_same_directory(
            Path::new("/home/user/proj/src"),
            Path::new("/home/user/proj"),
        ));
        assert!(is_inside_or_same_directory(
            Path::new("/home/user/proj"),
            Path::new("/home/user/proj"),
        ));
        assert!(!is_inside_or_same_directory(
            Path::new("/home/user/other"),
            Path::new("/home/user/proj"),
        ));
        assert!(!is_inside_or_same_directory(
            Path::new("/home/user/proj-evil/hack"),
            Path::new("/home/user/proj"),
        ));
    }

    #[test]
    fn test_clean_branch_name() {
        assert_eq!(clean_branch_name("refs/heads/main"), "main");
        assert_eq!(clean_branch_name("refs/remotes/origin/main"), "origin/main");
        assert_eq!(clean_branch_name("main"), "main");
        assert_eq!(clean_branch_name("  refs/heads/feature/x  "), "feature/x");
    }

    #[test]
    fn test_slug_worktree_name() {
        assert_eq!(slug_worktree_name("refs/heads/my branch!"), "my-branch");
        assert_eq!(slug_worktree_name("heads/foo"), "foo");
        assert_eq!(slug_worktree_name("feature/x/y"), "feature-x-y");
        assert_eq!(slug_worktree_name("---leading---"), "leading");
    }
}
