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

/// home 目录字符串。
///
/// 对齐 Node `os.homedir()` 的平台语义:
/// - **Windows**: 优先 `USERPROFILE`(Windows 不会为从 explorer.exe 双击启动的 GUI 进程
///   设置 `HOME`;只读 `HOME` 会 fallback 到 `"."`,导致 `opencode_config_dir()` 等解析到
///   `<CWD>/.config/opencode` 这类不存在的路径)。与本库 `behavior.rs`、`fs/operations.rs`、
///   `resolution_routes.rs` 已有的正确模式一致。
/// - **POSIX**: 读 `HOME`。
/// - 任一平台都设置不了时 fallback `"."`(与原行为一致,保留向后兼容)。
pub fn home_dir_string() -> String {
    std::env::var(home_env_var_name())
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| ".".to_string())
}

/// [`home_dir_string()`] 实际读取的环境变量名。
///
/// - Windows: `USERPROFILE`
/// - 其他:   `HOME`
///
/// 测试 helper 用它来设置/还原正确的 env var,避免在 Windows 上错误地只设 `HOME`
/// (那不会影响 [`home_dir_string()`] 的结果)。
pub fn home_env_var_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "USERPROFILE"
    } else {
        "HOME"
    }
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
    use std::sync::Mutex;

    /// 串行化所有 mutate 进程 env 的测试 —— env 是全局共享状态,
    /// `cargo test` 默认并行,不加锁会互相覆盖(参考 `backend.rs::ENV_LOCK`)。
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// 测试期间临时设置 home 相关 env 并在退出时还原的平台抽象。
    /// - Windows: 设置 `USERPROFILE`
    /// - 其他:   设置 `HOME`
    struct HomeEnvGuard {
        #[cfg(target_os = "windows")]
        original: Option<std::ffi::OsString>,
        #[cfg(not(target_os = "windows"))]
        original: Option<std::ffi::OsString>,
    }

    impl HomeEnvGuard {
        fn set_home(value: &str) -> Self {
            #[cfg(target_os = "windows")]
            {
                let original = std::env::var_os("USERPROFILE");
                std::env::set_var("USERPROFILE", value);
                Self { original }
            }
            #[cfg(not(target_os = "windows"))]
            {
                let original = std::env::var_os("HOME");
                std::env::set_var("HOME", value);
                Self { original }
            }
        }
    }

    impl Drop for HomeEnvGuard {
        fn drop(&mut self) {
            #[cfg(target_os = "windows")]
            {
                match self.original.take() {
                    Some(v) => std::env::set_var("USERPROFILE", &v),
                    None => std::env::remove_var("USERPROFILE"),
                }
            }
            #[cfg(not(target_os = "windows"))]
            {
                match self.original.take() {
                    Some(v) => std::env::set_var("HOME", &v),
                    None => std::env::remove_var("HOME"),
                }
            }
        }
    }

    #[test]
    fn home_dir_string_resolves_correct_env_var() {
        let _lock = ENV_LOCK.lock().expect("ENV_LOCK poisoned");
        // Windows 读 USERPROFILE;POSIX 读 HOME。两者都应返回设置的非空值。
        let _g = HomeEnvGuard::set_home("/home/user");
        assert_eq!(home_dir_string(), "/home/user");
    }

    #[test]
    fn home_dir_string_fallback_on_missing_env() {
        let _lock = ENV_LOCK.lock().expect("ENV_LOCK poisoned");
        #[cfg(target_os = "windows")]
        let _g = HomeEnvGuard::set_home("");
        #[cfg(not(target_os = "windows"))]
        let _g = HomeEnvGuard::set_home("");
        // 空串视为未设置 → fallback "."
        // (Windows 下 set_var("") 的值经 trim 判空后也走 fallback)
        // 注意: POSIX 下 var() 对空串返回 Ok(""),这里测试 fallback 行为需要先 remove。
        #[cfg(target_os = "windows")]
        {
            std::env::remove_var("USERPROFILE");
            assert_eq!(home_dir_string(), ".");
        }
        #[cfg(not(target_os = "windows"))]
        {
            std::env::remove_var("HOME");
            assert_eq!(home_dir_string(), ".");
        }
        drop(_g);
    }

    #[test]
    fn test_normalize_directory_path_tilde() {
        let _lock = ENV_LOCK.lock().expect("ENV_LOCK poisoned");
        let _g = HomeEnvGuard::set_home("/home/user");
        assert_eq!(normalize_directory_path("~/foo"), "/home/user/foo");
        assert_eq!(normalize_directory_path("~"), "/home/user");
        assert_eq!(normalize_directory_path("  /abs/path  "), "/abs/path");
        assert_eq!(normalize_directory_path(""), "");
    }

    #[test]
    fn test_normalize_path_backslash() {
        let _lock = ENV_LOCK.lock().expect("ENV_LOCK poisoned");
        let _g = HomeEnvGuard::set_home("/home/user");
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
