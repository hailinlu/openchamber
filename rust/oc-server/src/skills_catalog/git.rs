//! Git 执行工具 — 对应 Node `git.js`。
//!
//! 封装 `git` 命令调用, 提供可测试的运行器注入。

use serde::Serialize;
use std::process::Output;

/// Git 命令执行结果。
#[derive(Debug, Clone, Serialize)]
pub struct GitResult {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdout: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stderr: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<i32>,
}

/// Git 执行器 trait — 方便测试时 mock。
pub trait GitRunner: Send + Sync {
    fn run(&self, args: &[&str], cwd: Option<&str>, timeout_ms: Option<u64>) -> GitResult;
}

/// 默认 GitRunner — 调用系统的 `git`。
pub struct DefaultGitRunner;

impl GitRunner for DefaultGitRunner {
    fn run(&self, args: &[&str], cwd: Option<&str>, _timeout_ms: Option<u64>) -> GitResult {
        let mut cmd = std::process::Command::new("git");
        cmd.args(args);
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        match cmd.output() {
            Ok(output) => result_from_output(output),
            Err(e) => GitResult {
                ok: false,
                stdout: None,
                stderr: None,
                message: Some(format!("git error: {}", e)),
                code: None,
            },
        }
    }
}

fn result_from_output(output: Output) -> GitResult {
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let code = output.status.code();
    let ok = output.status.success();
    let message = if !ok && !stderr.is_empty() {
        Some(stderr.trim().to_string())
    } else if !ok {
        Some("git command failed".to_string())
    } else {
        None
    };
    GitResult {
        ok,
        stdout: Some(stdout),
        stderr: Some(stderr),
        message,
        code,
    }
}

/// 检查错误信息是否包含常见的 git 认证失败 pattern。
pub fn looks_like_auth_error(message: &str) -> bool {
    let lower = message.to_lowercase();
    lower.contains("permission denied")
        || lower.contains("authentication failed")
        || lower.contains("could not read from remote repository")
        || lower.contains("repository not found")
        || lower.contains("could not resolve host")
        || lower.contains("does not appear to be a git repository")
        || lower.contains("host key verification failed")
        || lower.contains("fatal: could not read username")
}

/// 检测 `git` 是否可用。
pub fn assert_git_available(runner: &dyn GitRunner) -> Result<(), String> {
    let result = runner.run(&["--version"], None, None);
    if result.ok {
        Ok(())
    } else {
        Err("git is not available in PATH".to_string())
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    struct MockGitRunner {
        version_ok: bool,
    }
    impl GitRunner for MockGitRunner {
        fn run(&self, args: &[&str], _cwd: Option<&str>, _timeout: Option<u64>) -> GitResult {
            if args.contains(&"--version") {
                if self.version_ok {
                    GitResult { ok: true, stdout: Some("git version 2.40.0".into()), stderr: None, message: None, code: Some(0) }
                } else {
                    GitResult { ok: false, stdout: None, stderr: Some("not found".into()), message: Some("not found".into()), code: None }
                }
            } else {
                GitResult { ok: true, stdout: Some("ok".into()), stderr: None, message: None, code: Some(0) }
            }
        }
    }

    #[test]
    fn assert_git_available_ok() {
        let runner = MockGitRunner { version_ok: true };
        assert!(assert_git_available(&runner).is_ok());
    }

    #[test]
    fn assert_git_available_fails() {
        let runner = MockGitRunner { version_ok: false };
        assert!(assert_git_available(&runner).is_err());
    }

    #[test]
    fn looks_like_auth_error_detects_patterns() {
        assert!(looks_like_auth_error("Permission denied (publickey)"));
        assert!(looks_like_auth_error("Authentication failed"));
        assert!(looks_like_auth_error("repository not found"));
        assert!(!looks_like_auth_error("everything up-to-date"));
    }
}
