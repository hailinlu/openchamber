//! OpenCode 子进程管理。
//!
//! 对应现有:
//!   - `packages/web/server/lib/opencode/lifecycle.js` (spawn, readiness, shutdown)
//!   - `packages/web/server/lib/opencode/auth-state-runtime.js` (managed password)
//!   - `packages/web/server/lib/opencode/network-runtime.js` (waitForReady, buildOpenCodeUrl)
//!
//! 职责:
//!   1. 生成 managed password (32 byte URL-safe base64, 同 Node 侧)
//!   2. spawn `opencode serve --hostname <h> --port <p>`
//!   3. 解析 stdout 就绪行 `opencode server listening on <url>`
//!   4. 轮询 `/global/health` 就绪门 (100ms interval, 10s timeout)
//!   5. 优雅关闭 (SIGTERM → 2.5s → SIGKILL, 进程组整杀)
//!
//! 外部模式: 当 OPENCODE_HOST/OPENCODE_SKIP_START 设置时, 不 spawn,
//! 直接用外部 URL + 用户提供的密码。

use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpListener;
use tokio::process::{Child, Command};
use tokio::time;

use crate::config::Config;

/// spawn 超时 (等待 stdout 就绪行)。
const SPAWN_TIMEOUT: Duration = Duration::from_secs(30);
/// /global/health 就绪门超时。
const HEALTH_READY_TIMEOUT: Duration = Duration::from_secs(10);
/// /global/health 轮询间隔。
const HEALTH_POLL_INTERVAL: Duration = Duration::from_millis(100);
/// /global/health 单次请求超时。
const HEALTH_REQ_TIMEOUT: Duration = Duration::from_secs(3);
/// SIGTERM 后等 SIGKILL 的宽限期。
const KILL_GRACE: Duration = Duration::from_millis(2500);

/// OpenCode 进程句柄 (managed 模式) 或外部连接信息 (external 模式)。
pub struct OpenCodeHandle {
    /// managed 模式下的子进程; external 模式为 None。
    child: Option<Child>,
    #[cfg(windows)]
    job: Option<win::JobGuard>,
}

impl OpenCodeHandle {
    /// 优雅关闭: SIGTERM 进程组 → 等待 → SIGKILL 兜底。
    /// external 模式无子进程, 直接返回。
    pub async fn shutdown(&mut self) {
        if let Some(mut child) = self.child.take() {
            #[cfg(unix)]
            {
                let pid = child.id();
                if let Some(pid) = pid {
                    tracing::info!(pid, "sending SIGTERM to opencode process group");
                    unsafe {
                        let r = libc::killpg(pid as i32, libc::SIGTERM);
                        if r != 0 {
                            tracing::warn!(pid, "killpg SIGTERM failed, trying SIGKILL");
                            let _ = libc::killpg(pid as i32, libc::SIGKILL);
                        }
                    }
                }
            }
            #[cfg(windows)]
            {
                if let Some(job) = self.job.take() {
                    job.terminate();
                }
            }

            // 等待 KILL_GRACE, 如果还没退出则 start_kill (SIGKILL child)。
            match time::timeout(KILL_GRACE, child.wait()).await {
                Ok(_) => tracing::info!("opencode process exited"),
                Err(_) => {
                    tracing::warn!("opencode did not exit in {:?}, force killing", KILL_GRACE);
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                }
            }
        }
    }
}

impl Drop for OpenCodeHandle {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            #[cfg(unix)]
            {
                if let Some(pid) = child.id() {
                    unsafe {
                        let _ = libc::killpg(pid as i32, libc::SIGKILL);
                    }
                }
            }
            #[cfg(windows)]
            {
                if let Some(job) = self.job.take() {
                    job.terminate();
                }
            }
            let _ = child.start_kill();
        }
    }
}

/// 启动 OpenCode: managed spawn 或 external attach。
///
/// 返回 `(base_url, auth_header, handle)`。
/// handle 用于后续 shutdown; base_url/auth_header 供 proxy 使用。
pub async fn start(config: &Config) -> Result<(String, String, OpenCodeHandle)> {
    if config.is_external_opencode() {
        start_external(config).await
    } else {
        start_managed(config).await
    }
}

/// managed 模式: spawn opencode 二进制作为子进程。
async fn start_managed(config: &Config) -> Result<(String, String, OpenCodeHandle)> {
    // 1. 分配端口
    let port = allocate_port(&config.opencode_hostname)
        .await
        .with_context(|| format!("allocate port for opencode on {}", config.opencode_hostname))?;

    // 2. 生成 managed password
    let password = generate_secure_password();
    let auth_header = build_auth_header(&config.opencode_username, &password);

    tracing::info!(port, hostname = %config.opencode_hostname, "spawning opencode");

    // 3. 构造命令: opencode serve --hostname <h> --port <p>
    let mut cmd = Command::new(&config.opencode_binary);
    cmd.args(["serve", "--hostname", &config.opencode_hostname, "--port", &port.to_string()]);

    // Windows: 隐藏控制台窗口 (AGENTS.md Windows 规则)。
    #[cfg(windows)]
    {
        // CREATE_NO_WINDOW = 0x08000000
        cmd.creation_flags(0x08000000);
    }

    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.stdin(Stdio::null());

    // 注入 managed password 到子进程 env。
    cmd.env("OPENCODE_SERVER_PASSWORD", &password);

    // 4. spawn + 进程树归组 (Unix: setpgid)。
    #[cfg(unix)]
    unsafe {
        cmd.pre_exec(|| {
            libc::setpgid(0, 0);
            Ok(())
        });
    }

    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn opencode binary `{}`", config.opencode_binary))?;

    #[cfg(windows)]
    let job = {
        let pid = child
            .id()
            .ok_or_else(|| anyhow!("opencode child has no pid"))?;
        let job = win::JobGuard::create().context("failed to create Job Object for opencode")?;
        job.assign_pid(pid)
            .with_context(|| format!("failed to assign opencode pid {} to job", pid))?;
        Some(job)
    };

    // 5. 解析 stdout 就绪行: "opencode server listening on <url>"
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("opencode stdout not captured"))?;
    let base_url = match wait_listening_url(stdout, SPAWN_TIMEOUT).await {
        Ok(url) => url,
        Err(e) => {
            // 就绪失败: 清理已 spawn 的进程。
            #[cfg(unix)]
            {
                if let Some(pid) = child.id() {
                    unsafe {
                        let _ = libc::killpg(pid as i32, libc::SIGKILL);
                    }
                }
            }
            #[cfg(windows)]
            {
                if let Some(ref job) = job {
                    job.terminate();
                }
            }
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Err(e.context("opencode readiness (stdout) failed"));
        }
    };

    tracing::info!(%base_url, "opencode listening, waiting for health");

    // 6. 轮询 /global/health 就绪门
    let health_url = format!("{}/global/health", base_url.trim_end_matches('/'));
    if !wait_health(&health_url, &auth_header, HEALTH_READY_TIMEOUT).await {
        // 健康检查失败: 清理。
        #[cfg(unix)]
        {
            if let Some(pid) = child.id() {
                unsafe {
                    let _ = libc::killpg(pid as i32, libc::SIGKILL);
                }
            }
        }
        #[cfg(windows)]
        {
            if let Some(ref job) = job {
                job.terminate();
            }
        }
        let _ = child.start_kill();
        let _ = child.wait().await;
        return Err(anyhow!("opencode did not become healthy within {:?}", HEALTH_READY_TIMEOUT));
    }

    tracing::info!(%base_url, "opencode is healthy and ready");

    let handle = OpenCodeHandle {
        child: Some(child),
        #[cfg(windows)]
        job,
    };

    Ok((base_url, auth_header, handle))
}

/// external 模式: 连接到已有的 OpenCode 实例。
async fn start_external(config: &Config) -> Result<(String, String, OpenCodeHandle)> {
    let base_url = if let Some(ref host) = config.opencode_host {
        // OPENCODE_HOST 是完整 base URL
        host.trim_end_matches('/').to_string()
    } else if let Some(port) = config.opencode_port {
        format!("http://127.0.0.1:{}", port)
    } else {
        return Err(anyhow!(
            "external opencode mode requires OPENCODE_HOST or OPENCODE_PORT"
        ));
    };

    // 外部模式使用用户提供的密码 (如果有的话)。
    let auth_header = if let Some(ref pw) = config.opencode_password {
        build_auth_header(&config.opencode_username, pw)
    } else {
        String::new()
    };

    tracing::info!(%base_url, "external opencode mode (no spawn)");

    // 探测健康 (best-effort, 不阻塞启动)。
    let health_url = format!("{}/global/health", base_url.trim_end_matches('/'));
    if wait_health(&health_url, &auth_header, HEALTH_READY_TIMEOUT).await {
        tracing::info!(%base_url, "external opencode is healthy");
    } else {
        tracing::warn!(%base_url, "external opencode health check failed (continuing anyway)");
    }

    let handle = OpenCodeHandle {
        child: None,
        #[cfg(windows)]
        job: None,
    };

    Ok((base_url, auth_header, handle))
}

/// 生成 32 byte URL-safe base64 密码 (无 padding)。
///
/// 对应 Node 侧 `generateSecureOpenCodePassword`:
/// `crypto.randomBytes(32).toString('base64')` with `+`→`-`, `/`→`_`, `=` stripped。
fn generate_secure_password() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// 构建 Basic auth header: `Basic <base64("username:password")>`。
fn build_auth_header(username: &str, password: &str) -> String {
    let credentials = format!("{}:{}", username, password);
    format!("Basic {}", STANDARD.encode(credentials))
}

/// 绑定 `host:0` 拿到 OS 分配的空闲端口, 然后立即 drop listener。
async fn allocate_port(host: &str) -> Result<u16> {
    let listener = TcpListener::bind((host, 0u16))
        .await
        .with_context(|| format!("bind {}:0 for port allocation", host))?;
    let addr = listener.local_addr().context("get bound local addr")?;
    drop(listener);
    Ok(addr.port())
}

/// 读 stdout, 等待 `opencode server listening on <url>` 行, 解析出 base URL。
///
/// 对应 Node 侧 `lifecycle.js` line 304-305 的正则匹配。
async fn wait_listening_url(
    stdout: tokio::process::ChildStdout,
    timeout: Duration,
) -> Result<String> {
    let mut reader = BufReader::new(stdout).lines();
    let deadline = time::Instant::now() + timeout;

    loop {
        let remaining = deadline
            .checked_duration_since(time::Instant::now())
            .ok_or_else(|| anyhow!("opencode did not print listening line within {:?}", timeout))?;

        match time::timeout(remaining, reader.next_line()).await {
            Ok(Ok(Some(line))) => {
                tracing::debug!(line = %line, "opencode stdout");
                if line.starts_with("opencode server listening") {
                    // 解析 URL: "opencode server listening on http://127.0.0.1:4096"
                    if let Some(url) = line
                        .split(" on ")
                        .nth(1)
                        .map(|s| s.trim().to_string())
                    {
                        return Ok(url);
                    }
                }
            }
            Ok(Ok(None)) => {
                return Err(anyhow!("opencode stdout closed before listening line"));
            }
            Ok(Err(e)) => {
                return Err(anyhow!("opencode stdout read error: {}", e));
            }
            Err(_) => {
                return Err(anyhow!("opencode did not print listening line within {:?}", timeout));
            }
        }
    }
}

/// 轮询 /global/health, 要求返回 2xx 且 body.healthy == true。
///
/// 对应 Node 侧 `network-runtime.js` 的 `waitForReady`。
async fn wait_health(health_url: &str, auth_header: &str, timeout: Duration) -> bool {
    let client = reqwest::Client::builder()
        .timeout(HEALTH_REQ_TIMEOUT)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    let deadline = time::Instant::now() + timeout;
    loop {
        if time::Instant::now() >= deadline {
            return false;
        }

        let mut req = client
            .get(health_url)
            .header("Accept", "application/json");
        if !auth_header.is_empty() {
            req = req.header("Authorization", auth_header);
        }

        match req.send().await {
            Ok(resp) if resp.status().is_success() => {
                if let Ok(body) = resp.json::<serde_json::Value>().await {
                    if body.get("healthy").and_then(|v| v.as_bool()).unwrap_or(false) {
                        return true;
                    }
                }
            }
            _ => {}
        }

        time::sleep(HEALTH_POLL_INTERVAL).await;
    }
}

// =========================================================================
// Windows: Job Object 整树杀 (同 sidecar.rs 的实现)。
// =========================================================================
#[cfg(windows)]
mod win {
    use anyhow::{Result, anyhow};
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    pub struct JobGuard(HANDLE);
    unsafe impl Send for JobGuard {}

    impl JobGuard {
        pub fn create() -> Result<Self> {
            let h = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if h.is_null() {
                return Err(anyhow!("CreateJobObjectW returned null"));
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            unsafe {
                let r = SetInformationJobObject(
                    h,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const _,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                if r == 0 {
                    CloseHandle(h);
                    return Err(anyhow!("SetInformationJobObject failed"));
                }
            }
            Ok(Self(h))
        }

        pub fn assign_pid(&self, pid: u32) -> Result<()> {
            use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE};
            let desired = PROCESS_SET_QUOTA | PROCESS_TERMINATE;
            let proc_h = unsafe { OpenProcess(desired, 1, pid) };
            if proc_h.is_null() {
                return Err(anyhow!("OpenProcess(pid={}) failed", pid));
            }
            let r = unsafe { AssignProcessToJobObject(self.0, proc_h) };
            unsafe { CloseHandle(proc_h) };
            if r == 0 {
                return Err(anyhow!("AssignProcessToJobObject(pid={}) failed", pid));
            }
            Ok(())
        }

        pub fn terminate(&self) {
            unsafe { TerminateJobObject(self.0, 1) };
        }
    }

    impl Drop for JobGuard {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }
}

// =========================================================================
// 测试
// =========================================================================
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_is_url_safe_base64_no_pad() {
        let pw = generate_secure_password();
        // 32 bytes → 43 chars URL-safe base64 (no padding).
        assert_eq!(pw.len(), 43);
        assert!(!pw.contains('+'));
        assert!(!pw.contains('/'));
        assert!(!pw.contains('='));
    }

    #[test]
    fn password_is_unique() {
        let pw1 = generate_secure_password();
        let pw2 = generate_secure_password();
        assert_ne!(pw1, pw2);
    }

    #[test]
    fn auth_header_format() {
        let header = build_auth_header("opencode", "test123");
        assert!(header.starts_with("Basic "));
        let encoded = &header[6..];
        let decoded = STANDARD.decode(encoded).unwrap();
        let decoded_str = String::from_utf8(decoded).unwrap();
        assert_eq!(decoded_str, "opencode:test123");
    }

    #[test]
    fn auth_header_custom_username() {
        let header = build_auth_header("admin", "secret");
        let encoded = &header[6..];
        let decoded = STANDARD.decode(encoded).unwrap();
        assert_eq!(String::from_utf8(decoded).unwrap(), "admin:secret");
    }

    #[tokio::test]
    async fn allocate_port_returns_valid_port() {
        let p = allocate_port("127.0.0.1").await.unwrap();
        assert!((1..=65535).contains(&p));
    }
}
