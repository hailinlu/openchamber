//! Git CLI 执行核心。
//!
//! 移植自 Node `runGitCommand` / `runGitCommandOrThrow` / `buildGitEnv` / `resolveGitBinary`。
//! 所有 git 调用通过 `tokio::process::Command` spawn `git` 二进制。

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::OnceLock;

use tokio::process::Command;

/// git 命令执行结果, 与 Node `runGitCommand` 返回值对齐。
#[derive(Debug, Clone)]
pub struct GitCommandResult {
    pub success: bool,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub message: Option<String>,
}

impl GitCommandResult {
    /// stdout 去首尾空白。
    pub fn stdout_text(&self) -> String {
        self.stdout.trim().to_string()
    }

    /// stderr 或 message 去首尾空白, 与 Node `gitStderrText` 对齐。
    pub fn stderr_text(&self) -> String {
        let stderr = self.stderr.trim();
        if !stderr.is_empty() {
            return stderr.to_string();
        }
        self.message
            .as_deref()
            .unwrap_or("")
            .trim()
            .to_string()
    }

    /// success 布尔值, 与 Node `runGitOk` 对齐。
    pub fn ok(&self) -> bool {
        self.success
    }
}

/// 解析 git 错误文本: 合并 stderr/stdout/message, 与 Node `parseGitErrorText` 对齐。
pub fn parse_git_error_text(stderr: &str, stdout: &str, message: Option<&str>) -> String {
    let mut parts: Vec<&str> = Vec::new();
    let stderr_trimmed = stderr.trim();
    let stdout_trimmed = stdout.trim();
    let message_trimmed = message.unwrap_or("").trim();

    if !message_trimmed.is_empty() {
        parts.push(message_trimmed);
    }
    if !stderr_trimmed.is_empty() {
        parts.push(stderr_trimmed);
    }
    if !stdout_trimmed.is_empty() {
        parts.push(stdout_trimmed);
    }
    parts.join("\n")
}

/// 检测 "not a git repository" 错误, 与 Node `isNotGitRepositoryError` 对齐。
pub fn is_not_git_repository_error(text: &str) -> bool {
    text.to_lowercase().contains("not a git repository")
}

/// 检测目录缺失错误, 与 Node `isMissingDirectoryError` 对齐。
pub fn is_missing_directory_error(text: &str) -> bool {
    let lower = text.to_lowercase();
    lower.contains("directory that does not exist")
        || lower.contains("does not exist")
        || lower.contains("no such file or directory")
}

/// git 二进制解析 (线程安全缓存)。
static GIT_BINARY: OnceLock<String> = OnceLock::new();

/// gpgconf 候选路径。
const GPGCONF_CANDIDATES: &[&str] = &["gpgconf", "/opt/homebrew/bin/gpgconf", "/usr/local/bin/gpgconf"];

pub struct GitRunner;

impl GitRunner {
    /// 返回 git 二进制路径。Unix 直接返回 "git"; Windows 探测已知安装目录。
    pub fn git_binary() -> String {
        // 非 Windows 直接返回 "git"
        if !cfg!(target_os = "windows") {
            return "git".to_string();
        }

        // Windows: 检查缓存
        if let Some(cached) = GIT_BINARY.get() {
            return cached.clone();
        }

        let resolved = resolve_git_binary_windows().unwrap_or_else(|| "git.exe".to_string());
        // 第一次 set 赢, 后续直接 get
        let _ = GIT_BINARY.set(resolved.clone());
        GIT_BINARY.get().cloned().unwrap_or_else(|| "git.exe".to_string())
    }

    /// 构建 git 环境变量 (继承 process env + SSH_AUTH_SOCK 探测)。
    pub async fn build_env() -> HashMap<String, String> {
        let mut env: HashMap<String, String> = std::env::vars().collect();

        // SSH_AUTH_SOCK 探测
        let needs_ssh_sock = env
            .get("SSH_AUTH_SOCK")
            .map(|v| v.trim().is_empty())
            .unwrap_or(true);

        if needs_ssh_sock {
            if let Some(sock) = resolve_ssh_auth_sock().await {
                env.insert("SSH_AUTH_SOCK".to_string(), sock);
            }
        }

        env
    }

    /// 在 cwd 下执行 git 命令, 永不返回 Err (失败体现在 GitCommandResult 中)。
    /// 与 Node `runGitCommand` 对齐。
    pub async fn run(cwd: &Path, args: &[&str]) -> GitCommandResult {
        let binary = Self::git_binary();
        let env = Self::build_env().await;

        let mut cmd = Command::new(&binary);
        cmd.args(args)
            .current_dir(cwd)
            .envs(env.iter())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // Windows: 隐藏控制台窗口
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x08000000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }

        match cmd.output().await {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                let code = output.status.code().unwrap_or(1);
                let success = output.status.success();

                // maxBuffer 检查: Node 在超过 20MB 时报错, 我们截断
                if stdout.len() > crate::git::MAX_GIT_STDOUT_BYTES {
                    return GitCommandResult {
                        success: false,
                        exit_code: code,
                        stdout: stdout[..crate::git::MAX_GIT_STDOUT_BYTES].to_string(),
                        stderr,
                        message: Some("maxBuffer exceeded".to_string()),
                    };
                }

                let message = if !success {
                    let parsed = parse_git_error_text(&stderr, &stdout, None);
                    if !parsed.is_empty() {
                        Some(parsed)
                    } else {
                        None
                    }
                } else {
                    None
                };

                GitCommandResult {
                    success,
                    exit_code: code,
                    stdout,
                    stderr,
                    message,
                }
            }
            Err(e) => GitCommandResult {
                success: false,
                exit_code: 1,
                stdout: String::new(),
                stderr: e.to_string(),
                message: Some(e.to_string()),
            },
        }
    }

    /// run() + 失败时返回 oc_core::Error, 与 Node `runGitCommandOrThrow` 对齐。
    pub async fn run_or_throw(
        cwd: &Path,
        args: &[&str],
        fallback: &str,
    ) -> oc_core::Result<GitCommandResult> {
        let result = Self::run(cwd, args).await;
        if !result.success {
            let msg = result
                .message
                .as_deref()
                .filter(|m| !m.is_empty())
                .unwrap_or(fallback);
            return Err(oc_core::Error::Internal(msg.to_string()));
        }
        Ok(result)
    }

    /// run() + 如果 stderr 包含 "not a git repository" 返回 false。
    pub async fn is_git_repo(cwd: &Path) -> bool {
        let result = Self::run(cwd, &["rev-parse", "--git-dir"]).await;
        result.success
    }
}

/// 解析 git 二进制路径 (Windows only)。
#[cfg(target_os = "windows")]
fn resolve_git_binary_windows() -> Option<String> {
    use std::path::Path;

    // 1. 环境变量 GIT_BINARY / GRIDFORGE_GIT_BINARY
    for var in &["GIT_BINARY", "GRIDFORGE_GIT_BINARY"] {
        if let Ok(val) = std::env::var(var) {
            let trimmed = val.trim();
            if !trimmed.is_empty() && is_executable_file(Path::new(trimmed)) {
                return Some(trimmed.to_string());
            }
        }
    }

    // 2. PATH 探测
    let path = std::env::var("PATH").unwrap_or_default();
    let seen_dirs: std::collections::HashSet<&str> = path.split(';').filter(|s| !s.is_empty()).collect();
    for dir in &seen_dirs {
        for name in &["git.exe", "git"] {
            let candidate = Path::new(dir).join(name);
            if is_executable_file(&candidate) {
                return Some("git".to_string());
            }
        }
    }

    // 3. 已知安装目录
    let mut candidates = Vec::new();
    for root_var in &["ProgramFiles", "ProgramFiles(x86)", "LocalAppData"] {
        if let Ok(root) = std::env::var(root_var) {
            let root = root.trim();
            if root.is_empty() {
                continue;
            }
            candidates.push(format!("{root}\\Git\\cmd\\git.exe"));
            candidates.push(format!("{root}\\Git\\bin\\git.exe"));
            candidates.push(format!("{root}\\Git\\mingw64\\bin\\git.exe"));
            candidates.push(format!("{root}\\Programs\\Git\\cmd\\git.exe"));
            candidates.push(format!("{root}\\Programs\\Git\\bin\\git.exe"));
        }
    }

    for candidate in &candidates {
        let path = Path::new(candidate);
        if is_executable_file(path) {
            return Some(candidate.clone());
        }
    }

    None
}

#[cfg(not(target_os = "windows"))]
fn resolve_git_binary_windows() -> Option<String> {
    None
}

/// 检查路径是否是可执行文件。
fn is_executable_file(path: &Path) -> bool {
    use std::fs;
    match fs::metadata(path) {
        Ok(meta) => {
            if !meta.is_file() {
                return false;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                meta.permissions().mode() & 0o111 != 0
            }
            #[cfg(not(unix))]
            {
                let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                ext == "exe" || ext == "cmd" || ext == "bat" || ext.is_empty()
            }
        }
        Err(_) => false,
    }
}

/// 检查路径是否是 Unix socket (用于 SSH_AUTH_SOCK 探测)。
#[cfg(unix)]
async fn is_socket_path(candidate: &str) -> bool {
    use std::os::unix::fs::FileTypeExt;
    match tokio::fs::metadata(candidate).await {
        Ok(meta) => meta.file_type().is_socket(),
        Err(_) => false,
    }
}

#[cfg(not(unix))]
async fn is_socket_path(_candidate: &str) -> bool {
    false
}

/// 解析 SSH_AUTH_SOCK, 与 Node `resolveSshAuthSock` 对齐。
async fn resolve_ssh_auth_sock() -> Option<String> {
    // Windows 不需要 SSH_AUTH_SOCK
    if cfg!(target_os = "windows") {
        return None;
    }

    // 1. 检查 ~/.gnupg/S.gpg-agent.ssh
    let home = crate::git::paths::home_dir_string();
    let gpg_sock = format!("{home}/.gnupg/S.gpg-agent.ssh");
    if is_socket_path(&gpg_sock).await {
        return Some(gpg_sock);
    }

    // 2. gpgconf --list-dirs agent-ssh-socket
    let candidate = run_gpgconf(&["--list-dirs", "agent-ssh-socket"]).await;
    if let Some(ref c) = candidate {
        if is_socket_path(c).await {
            return Some(c.clone());
        }
    }

    // 3. 尝试启动 gpg-agent 后重试
    if candidate.is_some() {
        let _ = run_gpgconf(&["--launch", "gpg-agent"]).await;
        let retried = run_gpgconf(&["--list-dirs", "agent-ssh-socket"]).await;
        if let Some(ref r) = retried {
            if is_socket_path(r).await {
                return Some(r.clone());
            }
        }
    }

    None
}

/// 尝试 gpgconf 候选, 返回 stdout trim。
async fn run_gpgconf(args: &[&str]) -> Option<String> {
    for candidate in GPGCONF_CANDIDATES {
        let result = Command::new(candidate)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .await;

        if let Ok(output) = result {
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !stdout.is_empty() {
                    return Some(stdout);
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_git_error_text() {
        assert_eq!(
            parse_git_error_text("err", "out", Some("msg")),
            "msg\nerr\nout"
        );
        assert_eq!(parse_git_error_text("  ", "out", None), "out");
        assert_eq!(parse_git_error_text("", "", None), "");
    }

    #[test]
    fn test_is_not_git_repository_error() {
        assert!(is_not_git_repository_error(
            "fatal: not a git repository (or any of the parent directories): .git"
        ));
        assert!(!is_not_git_repository_error("some other error"));
    }

    #[test]
    fn test_is_missing_directory_error() {
        assert!(is_missing_directory_error(
            "directory that does not exist"
        ));
        assert!(is_missing_directory_error("no such file or directory"));
        assert!(!is_missing_directory_error("some other error"));
    }

    #[test]
    fn test_git_command_result_helpers() {
        let result = GitCommandResult {
            success: true,
            exit_code: 0,
            stdout: "  hello  \n".to_string(),
            stderr: "  ".to_string(),
            message: None,
        };
        assert_eq!(result.stdout_text(), "hello");
        assert_eq!(result.stderr_text(), "");
        assert!(result.ok());

        let failed = GitCommandResult {
            success: false,
            exit_code: 1,
            stdout: "".to_string(),
            stderr: "error msg".to_string(),
            message: Some("wrapped".to_string()),
        };
        assert_eq!(failed.stderr_text(), "error msg");
        assert!(!failed.ok());
    }

    #[tokio::test]
    async fn test_git_runner_version() {
        // 这是一个集成测试, 需要 git 在 PATH 中
        let result = GitRunner::run(Path::new("."), &["--version"]).await;
        assert!(result.success, "git --version should succeed");
        assert!(result.stdout.contains("git version"));
    }

    #[tokio::test]
    async fn test_git_runner_not_a_repo() {
        let tmp = std::env::temp_dir();
        let result = GitRunner::run(&tmp, &["rev-parse", "--git-dir"]).await;
        assert!(!result.success);
        assert!(result
            .message
            .as_ref()
            .map(|m| m.contains("not a git repository"))
            .unwrap_or(false)
            || result.stderr.contains("not a git repository"));
    }
}
