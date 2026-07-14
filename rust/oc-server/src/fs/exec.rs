//! 后台命令执行系统。
//!
//! 移植 `packages/web/server/lib/fs/routes.js` 的 exec job 系统。
//!
//! 注意: Node 版本中 `background=true` 被始终拒绝 (返回 400)。
//! 所有命令同步执行, 结果存入 job Map (TTL 30 分钟)。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{json, Value};
use tokio::process::Command;

use super::DEFAULT_EXEC_TIMEOUT_SECS;

/// 命令执行结果。
#[derive(Serialize, Clone)]
pub struct CommandResult {
    pub command: String,
    pub success: bool,
    #[serde(rename = "exitCode", skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Exec job 状态。
#[derive(Serialize, Clone)]
pub struct ExecJob {
    #[serde(rename = "jobId")]
    pub job_id: String,
    pub status: String, // "completed"
    pub success: bool,
    pub results: Vec<CommandResult>,
}

/// Exec job 存储 — 进程内单例。
pub struct ExecJobStore {
    jobs: Mutex<HashMap<String, (ExecJob, Instant)>>,
    ttl: Duration,
}

impl ExecJobStore {
    pub fn new(ttl: Duration) -> Self {
        Self {
            jobs: Mutex::new(HashMap::new()),
            ttl,
        }
    }

    /// 执行命令序列并存储结果。
    ///
    /// 对应 Node `runExecJob`。
    pub async fn execute(
        &self,
        commands: Vec<String>,
        cwd: &std::path::Path,
        timeout_secs: Option<u64>,
    ) -> Result<Value, oc_core::Error> {
        if commands.is_empty() {
            return Err(oc_core::Error::BadRequest("Commands are required".into()));
        }

        let job_id = uuid::Uuid::new_v4().to_string();
        let timeout = Duration::from_secs(timeout_secs.unwrap_or(DEFAULT_EXEC_TIMEOUT_SECS));
        let mut results = Vec::with_capacity(commands.len());

        for cmd_str in &commands {
            let result = execute_single_command(cmd_str, cwd, timeout).await;
            results.push(result);
        }

        let all_success = results.iter().all(|r| r.success);
        let job = ExecJob {
            job_id: job_id.clone(),
            status: "completed".to_string(),
            success: all_success,
            results: results.clone(),
        };

        // 存储
        {
            let mut jobs = self.jobs.lock().unwrap();
            self.prune_locked(&mut jobs);
            jobs.insert(job_id.clone(), (job.clone(), Instant::now() + self.ttl));
        }

        Ok(json!(job))
    }

    /// 查询 job 状态。
    pub fn get(&self, job_id: &str) -> Option<ExecJob> {
        let mut jobs = self.jobs.lock().unwrap();
        self.prune_locked(&mut jobs);
        jobs.get(job_id).map(|(job, _)| job.clone())
    }

    /// 清理过期 job。
    fn prune_locked(&self, jobs: &mut HashMap<String, (ExecJob, Instant)>) {
        let now = Instant::now();
        jobs.retain(|_, (_, expires_at)| *expires_at > now);
    }
}

/// 执行单个命令。
///
/// 使用平台默认 shell:
///   - Unix: `/bin/sh -c "<command>"`
///   - Windows: `cmd.exe /C "<command>"` (但 AGENTS.md 说避免 cmd.exe /c 管道)
///
/// 注意: AGENTS.md 警告 Windows 上避免 cmd.exe /c 管道。
/// fs/exec 是用户显式请求的命令执行 (非系统探测), 允许使用 shell。
async fn execute_single_command(
    command: &str,
    cwd: &std::path::Path,
    timeout: Duration,
) -> CommandResult {
    let normalized = normalize_command(command);

    #[cfg(unix)]
    let mut cmd = {
        let mut c = Command::new("/bin/sh");
        c.arg("-c").arg(&normalized);
        c
    };

    #[cfg(windows)]
    let mut cmd = {
        let mut c = Command::new("cmd.exe");
        c.arg("/C").arg(&normalized);
        c
    };

    cmd.current_dir(cwd);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.stdin(std::process::Stdio::null());

    // windowsHide: 隐藏控制台窗口
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    match tokio::time::timeout(timeout, cmd.output()).await {
        Ok(Ok(output)) => CommandResult {
            command: normalized.clone(),
            success: output.status.success(),
            exit_code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            error: None,
        },
        Ok(Err(e)) => CommandResult {
            command: normalized.clone(),
            success: false,
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            error: Some(e.to_string()),
        },
        Err(_) => CommandResult {
            command: normalized.clone(),
            success: false,
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            error: Some(format!("Command timed out after {} seconds", timeout.as_secs())),
        },
    }
}

/// 规范化命令字符串 (trim + 合并空白)。
fn normalize_command(command: &str) -> String {
    command.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_collapses_whitespace() {
        assert_eq!(normalize_command("  echo   hello  "), "echo hello");
        assert_eq!(normalize_command("ls -la"), "ls -la");
    }

    #[tokio::test]
    async fn execute_success() {
        let store = ExecJobStore::new(Duration::from_secs(1800));
        let dir = std::env::temp_dir();
        let result = store
            .execute(vec!["echo hello".to_string()], &dir, Some(10))
            .await
            .unwrap();

        assert_eq!(result["status"], "completed");
        assert_eq!(result["success"], true);
        assert_eq!(result["results"][0]["success"], true);
        assert!(result["results"][0]["stdout"].as_str().unwrap().contains("hello"));
    }

    #[tokio::test]
    async fn execute_failure() {
        let store = ExecJobStore::new(Duration::from_secs(1800));
        let dir = std::env::temp_dir();
        let result = store
            .execute(vec!["exit 1".to_string()], &dir, Some(10))
            .await
            .unwrap();

        assert_eq!(result["status"], "completed");
        assert_eq!(result["success"], false);
        assert_eq!(result["results"][0]["success"], false);
        assert_eq!(result["results"][0]["exitCode"], 1);
    }

    #[tokio::test]
    async fn execute_empty_commands_fails() {
        let store = ExecJobStore::new(Duration::from_secs(1800));
        let dir = std::env::temp_dir();
        let result = store.execute(vec![], &dir, Some(10)).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn job_stored_and_retrievable() {
        let store = ExecJobStore::new(Duration::from_secs(1800));
        let dir = std::env::temp_dir();
        let result = store
            .execute(vec!["echo test".to_string()], &dir, Some(10))
            .await
            .unwrap();

        let job_id = result["jobId"].as_str().unwrap();
        let job = store.get(job_id);
        assert!(job.is_some());
        assert_eq!(job.unwrap().job_id, job_id);
    }
}
