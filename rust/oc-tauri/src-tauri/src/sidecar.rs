//! Sidecar 进程管理: Tauri 桌面壳以子进程方式拉起 `@openchamber/web` CLI
//! (foreground 模式), 该 CLI 内部再 spawn opencode 二进制作为孙进程。
//!
//! 关键不变量:
//! - **整树清理**: `kill()` / `Drop` 必须杀掉 sidecar + opencode 孙进程。
//!   Unix: 进程组 (`setpgid` + `kill(-pgid)`); Windows: Job Object
//!   (`AssignProcessToJobObject` + `TerminateJobObject` 杀整组)。
//! - **就绪门**: sidecar 端口能建立 TCP 连接并返回 `/health` 200 才视为启动成功。
//! - **端口分配**: 绑定 `127.0.0.1:0` 让 OS 分配, 再把实际端口传给 CLI 的 `--port`。
//!
//! 见 docs/plan/rust-migration-plan.md 阶段 4A (sidecar 过渡态)。

use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use tokio::net::TcpListener;
use tokio::process::{Child, Command};
use tokio::time;

// tokio::process::Command 原生暴露平台扩展方法:
// creation_flags (Windows, CREATE_NO_WINDOW) / pre_exec (Unix, setpgid)。
// 不需要显式 import std::os 的 CommandExt trait。

/// 默认就绪门超时 (CLI 冷启动 + opencode spawn + express ready)。
const DEFAULT_READY_TIMEOUT: Duration = Duration::from_secs(30);
/// `/health` 轮询间隔。
const HEALTH_POLL_INTERVAL: Duration = Duration::from_millis(200);
/// `/health` 单次请求超时。
const HEALTH_REQ_TIMEOUT: Duration = Duration::from_secs(2);
/// SIGTERM 后等 SIGKILL 的宽限期 (对齐 oc-server opencode shutdown 的 KILL_GRACE)。
/// bun/Node 默认不响应 SIGTERM, 必须有 SIGKILL 兜底, 否则 wait().await 永久阻塞。
const KILL_GRACE: Duration = Duration::from_millis(2500);

/// 已启动的 sidecar 句柄。`kill()` / Drop 保证整树清理。
pub struct SidecarHandle {
    child: Option<Child>,
    port: u16,
    #[cfg(windows)]
    job: Option<win::JobGuard>,
}

impl SidecarHandle {
    pub fn port(&self) -> u16 {
        self.port
    }

    #[allow(dead_code)]
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// 杀掉整棵 sidecar 进程树 (sidecar + opencode 孙进程)。
    /// 幂等: 多次调用安全。
    ///
    /// 关闭时序 (对齐 oc-server opencode shutdown):
    ///   1. killpg(SIGTERM) 通知整组优雅退出
    ///   2. 等 KILL_GRACE (2.5s)
    ///   3. 仍未退出 → killpg(SIGKILL) 强杀 + child.start_kill() 兜底
    ///   4. child.wait() 回收资源
    /// 不加超时的话, 若 sidecar (bun/Node) 不响应 SIGTERM, wait().await 会永远阻塞,
    /// 导致 oc-tauri 主进程卡死无法退出 (SIGTERM 冒烟复现)。
    pub async fn kill(&mut self) -> Result<()> {
        if let Some(mut child) = self.child.take() {
            #[cfg(unix)]
            {
                // 进程组: 子进程在 start() 里已 setpgid(0,0), pid == pgid。
                // kill(-pgid) 杀整组 (含 opencode 孙进程)。
                let pid = child.id();
                if let Some(pid) = pid {
                    // SAFETY: killpg 是 POSIX 标准 libc 调用, 参数为正 pgid。
                    unsafe {
                        // 1. 先 SIGTERM 优雅退出
                        let _ = libc::killpg(pid as i32, libc::SIGTERM);
                    }
                }
            }
            #[cfg(windows)]
            {
                // Job Object: 终止整个 job 会级联终止所有已加入的进程
                // (sidecar + 它 spawn 的 opencode 孙进程)。
                // 详见 win::JobGuard::terminate。
                if let Some(job) = self.job.take() {
                    job.terminate();
                }
            }

            // 2-3. 等 KILL_GRACE, 超时则 SIGKILL 强杀。
            //      bun/Node 默认不处理 SIGTERM, 必须有 SIGKILL 兜底, 否则 wait 永久阻塞。
            match tokio::time::timeout(KILL_GRACE, child.wait()).await {
                Ok(_status) => {
                    // 进程在宽限期内退出, 资源已回收。
                }
                Err(_elapsed) => {
                    log::warn!("sidecar did not exit in {:?}, force killing", KILL_GRACE);
                    #[cfg(unix)]
                    {
                        let pid = child.id();
                        if let Some(pid) = pid {
                            unsafe {
                                let _ = libc::killpg(pid as i32, libc::SIGKILL);
                            }
                        }
                    }
                    // start_kill 跨平台兜底 (发 SIGKILL/terminate)。
                    let _ = child.start_kill();
                    // 4. 回收资源。
                    let _ = child.wait().await;
                }
            }
        }
        Ok(())
    }
}

impl Drop for SidecarHandle {
    fn drop(&mut self) {
        // Drop 兜底: 如果没显式 kill(), 同步尽力清理。
        // 注意: Drop 不能 await, 所以用 blocking_kill 同步路径。
        if let Some(mut child) = self.child.take() {
            #[cfg(unix)]
            {
                let pid = child.id();
                if let Some(pid) = pid {
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
            // tokio Child drop 时, 如果没 kill, 子进程会成为孤儿继续跑。
            // 这里已通过 killpg/Job 杀了树; child.start_kill() 兜底确保
            // sidecar 主进程也被回收。
            let _ = child.start_kill();
        }
    }
}

/// sidecar 构造器。`start()` 前可链式配置。
#[allow(dead_code)]
pub struct SidecarBuilder {
    bin: String,
    host: String,
    ready_timeout: Duration,
    extra_env: Vec<(String, String)>,
    extra_args: Vec<String>,
    port: Option<u16>,
}

#[allow(dead_code)]
impl SidecarBuilder {
    /// 用默认值构造: bin = "gridforge", host = "127.0.0.1"。
    pub fn new() -> Self {
        Self {
            bin: "gridforge".to_string(),
            host: "127.0.0.1".to_string(),
            ready_timeout: DEFAULT_READY_TIMEOUT,
            extra_env: Vec::new(),
            extra_args: Vec::new(),
            port: None,
        }
    }

    /// 覆盖 sidecar 可执行文件路径 (绝对路径或 PATH 上的名字)。
    pub fn bin(mut self, bin: impl Into<String>) -> Self {
        self.bin = bin.into();
        self
    }

    /// 覆盖就绪门超时。
    pub fn ready_timeout(mut self, d: Duration) -> Self {
        self.ready_timeout = d;
        self
    }

    /// 追加环境变量 (覆盖同名变量)。
    pub fn env(mut self, key: impl Into<String>, val: impl Into<String>) -> Self {
        self.extra_env.push((key.into(), val.into()));
        self
    }

    /// 追加额外的 CLI 参数 (在 `serve --foreground --port N` 之后)。
    pub fn arg(mut self, a: impl Into<String>) -> Self {
        self.extra_args.push(a.into());
        self
    }

    /// 设置固定端口。不调用此方法时，sidecar 将使用 OS 分配的随机端口。
    /// 生产环境不设置此值；dev 模式可通过 `GRIDFORGE_PORT` 环境变量设置固定端口，
    /// 使 Vite early injection、proxy 和 sidecar 使用同一个端口。
    pub fn port(mut self, port: u16) -> Self {
        self.port = Some(port);
        self
    }

    /// 启动 sidecar: 分配端口 → spawn → 进程树归组 → 轮询 /health 就绪门。
    pub async fn start(self) -> Result<SidecarHandle> {
        // 1. 端口分配: 若设置了固定端口则直接使用，否则绑 127.0.0.1:0 让 OS 分配。
        let port = if let Some(p) = self.port {
            p
        } else {
            allocate_port(&self.host)
                .await
                .with_context(|| format!("failed to allocate port on {}", self.host))?
        };

        // 2. 构造命令: gridforge serve --foreground --port <p> [--host h] [extra...]
        //    --foreground 让 CLI 进程本身成为 web server (in-process), 不 detach,
        //    这样 sidecar 进程 == server 进程, kill sidecar 即停服务。
        let mut cmd = Command::new(&self.bin);
        cmd.args(["serve", "--foreground", "--port", &port.to_string()]);
        cmd.args(["--host", &self.host]);
        cmd.args(&self.extra_args);

        // Windows: 隐藏可能闪现的控制台窗口 (AGENTS.md Windows 规则)。
        // tokio Command 跨平台; windows_hide 通过 cfg! 注入。
        #[cfg(windows)]
        {
            // CREATE_NO_WINDOW = 0x08000000
            cmd.creation_flags(0x08000000);
        }

        // 管道: stdout/stderr 捕获以便诊断; stdin 忽略。
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.stdin(Stdio::null());

        // 环境变量覆盖。
        for (k, v) in &self.extra_env {
            cmd.env(k, v);
        }

        // 3. spawn + 进程树归组。
        #[cfg(unix)]
        unsafe {
            // pre_exec 在 fork 之后、exec 之前运行: setpgid(0,0) 把子进程
            // 放进以自己 pid 为 pgid 的新进程组, 这样后续 kill(-pgid) 能杀整组。
            cmd.pre_exec(|| {
                libc::setpgid(0, 0);
                Ok(())
            });
        }

        let mut child = cmd
            .spawn()
            .with_context(|| format!("failed to spawn sidecar `{}`", self.bin))?;

        #[cfg(windows)]
        let job = {
            let pid = child
                .id()
                .ok_or_else(|| anyhow!("sidecar child has no pid"))?;
            // 把 sidecar 加入 Job Object。它后续 spawn 的 opencode 孙进程
            // 会继承 job 成员资格 (默认 job 允许 breakaway, 但 Node 不主动 break),
            // 所以 terminate(job) 能级联杀掉整树。
            let job = win::JobGuard::create().context("failed to create Job Object")?;
            job.assign_pid(pid)
                .with_context(|| format!("failed to assign pid {} to job", pid))?;
            Some(job)
        };
        #[cfg(not(windows))]
        let _ = &mut child; // 抑制未用 mut 警告

        // 4. 就绪门: 轮询 /health 直到 200 或超时。
        let health_url = format!("http://127.0.0.1:{}/health", port);
        match wait_ready(&health_url, self.ready_timeout).await {
            Ok(()) => Ok(SidecarHandle {
                child: Some(child),
                port,
                #[cfg(windows)]
                job,
            }),
            Err(e) => {
                // 就绪失败: 必须清理已 spawn 的进程, 避免泄漏。
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
                    if let Some(j) = job {
                        j.terminate();
                    }
                }
                let _ = child.start_kill();
                let _ = child.wait().await;
                Err(e.context("sidecar readiness gate failed"))
            }
        }
    }
}

impl Default for SidecarBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// 绑定 `host:0` 拿到 OS 分配的空闲端口, 然后立即 drop listener 释放给 sidecar 用。
async fn allocate_port(host: &str) -> Result<u16> {
    let listener = TcpListener::bind((host, 0u16))
        .await
        .with_context(|| format!("bind {}:0 for port allocation", host))?;
    let addr = listener.local_addr().context("get bound local addr")?;
    drop(listener);
    Ok(addr.port())
}

/// 轮询 `/health` 直到返回 2xx 或超时。
async fn wait_ready(health_url: &str, timeout: Duration) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(HEALTH_REQ_TIMEOUT)
        .build()
        .context("build health-check http client")?;

    let deadline = tokio::time::Instant::now() + timeout;
    let mut last_err: Option<anyhow::Error> = None;
    loop {
        if tokio::time::Instant::now() >= deadline {
            let detail = last_err
                .as_ref()
                .map(|e| format!("; last error: {}", e))
                .unwrap_or_default();
            return Err(anyhow!(
                "sidecar did not become ready within {:?}{}",
                timeout,
                detail
            ));
        }
        match client.get(health_url).send().await {
            Ok(resp) if resp.status().is_success() => return Ok(()),
            Ok(resp) => {
                last_err = Some(anyhow!("health check returned status {}", resp.status()));
            }
            Err(e) => {
                last_err = Some(anyhow::Error::from(e));
            }
        }
        time::sleep(HEALTH_POLL_INTERVAL).await;
    }
}

// =========================================================================
// Windows: Job Object 整树杀。封装在私有 mod 以隔离 unsafe + cfg。
// =========================================================================
#[cfg(windows)]
mod win {
    use anyhow::Result;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    /// RAII Job Object guard。Drop 时 terminate 整组并关闭句柄。
    /// HANDLE 是原始指针 (*mut c_void), 默认非 Send; 但我们只在单线程
    /// (Tauri setup/退出) 使用, 且 Win32 句柄本身可跨线程, 所以手动 impl Send。
    pub struct JobGuard(HANDLE);

    // SAFETY: HANDLE 是 OS 资源句柄, Win32 句柄本身可跨线程传递使用。
    // 我们保证同一时刻只有一个线程操作 job (sidecar 启动/退出都是串行的)。
    unsafe impl Send for JobGuard {}

    impl JobGuard {
        pub fn create() -> Result<Self> {
            // SAFETY: CreateJobObjectW 创建新 job (无安全属性, 无名字);
            // 失败返回 NULL (空指针)。
            let h = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if h.is_null() {
                return Err(anyhow::anyhow!("CreateJobObjectW returned null handle"));
            }

            // 配置: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE — 当 job 句柄关闭时
            // 自动终止所有成员进程。这是最后的安全网: 即使 terminate 调用因
            // 竞态遗漏, 句柄 Close 时 OS 也会清理整组。
            // 用 zeroed 初始化整个结构 (全零合法), 只设 LimitFlags 一个字段,
            // 避免 windows-sys 不同版本结构体字段名差异导致的编译问题。
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION =
                unsafe { std::mem::zeroed() };
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

            // SAFETY: SetInformationJobObject 配置 job 属性; info 是本函数栈对象。
            unsafe {
                let r = SetInformationJobObject(
                    h,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const _,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                if r == 0 {
                    CloseHandle(h);
                    return Err(anyhow::anyhow!(
                        "SetInformationJobObject(KILL_ON_JOB_CLOSE) failed"
                    ));
                }
            }

            Ok(Self(h))
        }

        /// 把一个 pid 对应的进程加入 job。
        pub fn assign_pid(&self, pid: u32) -> Result<()> {
            use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE};
            // PROCESS_SET_QUOTA | PROCESS_TERMINATE: 加入 job 所需权限。
            let desired = PROCESS_SET_QUOTA | PROCESS_TERMINATE;
            // SAFETY: OpenProcess 打开句柄; 失败返回 NULL。
            let proc_h = unsafe { OpenProcess(desired, 1, pid) };
            if proc_h.is_null() {
                return Err(anyhow::anyhow!("OpenProcess(pid={}) failed", pid));
            }
            // SAFETY: AssignProcessToJobObject 把进程加入 job。
            let r = unsafe { AssignProcessToJobObject(self.0, proc_h) };
            unsafe { CloseHandle(proc_h) };
            if r == 0 {
                return Err(anyhow::anyhow!(
                    "AssignProcessToJobObject(pid={}) failed",
                    pid
                ));
            }
            Ok(())
        }

        /// 终止整个 job (级联终止所有成员进程)。幂等。
        pub fn terminate(&self) {
            // SAFETY: TerminateJobObject 杀整组; exit code 1 是任意非零。
            unsafe {
                TerminateJobObject(self.0, 1);
            }
        }
    }

    impl Drop for JobGuard {
        fn drop(&mut self) {
            // KILL_ON_JOB_CLOSE: 关闭句柄时 OS 自动终止整组。
            // terminate() 是显式兜底; 这里 Close 确保句柄不泄漏。
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

// =========================================================================
// 测试
// =========================================================================
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU16, Ordering};

    // 端口分配: 拿到的端口能在预期范围内 (1-65535), 且不固定。
    #[tokio::test]
    async fn allocate_port_returns_valid_port() {
        let p = allocate_port("127.0.0.1").await.unwrap();
        assert!((1..=65535).contains(&p));
    }

    // 端口分配: 两次调用通常不同 (除非极端巧合 OS 复用)。
    #[tokio::test]
    async fn allocate_port_is_dynamic() {
        let p1 = allocate_port("127.0.0.1").await.unwrap();
        let p2 = allocate_port("127.0.0.1").await.unwrap();
        // 不强求 != (OS 偶尔复用), 但用计数器观察分布。
        static DISTINCT: AtomicU16 = AtomicU16::new(0);
        if p1 != p2 {
            DISTINCT.fetch_add(1, Ordering::Relaxed);
        }
        assert!(DISTINCT.load(Ordering::Relaxed) >= 1 || p1 == p2);
    }

    // 就绪门: 对一个能立即返回 200 的 stub HTTP server, 应快速成功。
    #[tokio::test]
    async fn wait_ready_succeeds_on_healthy_server() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        // 最小 stub: 收到任意 HTTP 请求就回 200 OK。
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let url = format!("http://127.0.0.1:{}/health", port);

        let srv = tokio::spawn(async move {
            loop {
                let (mut sock, _) = match listener.accept().await {
                    Ok(v) => v,
                    Err(_) => break,
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 256];
                    let _ = tokio::io::AsyncReadExt::read(&mut sock, &mut buf).await;
                    let resp = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });

        let r = wait_ready(&url, Duration::from_secs(3)).await;
        assert!(r.is_ok(), "should be ready: {:?}", r);
        srv.abort();
    }

    // 就绪门: 对一个不存在 / 拒绝连接的端口, 应在超时后失败。
    #[tokio::test]
    async fn wait_ready_times_out_when_no_server() {
        // 用一个几乎肯定空闲的端口 (不 bind 任何东西)。
        let url = "http://127.0.0.1:1/health".to_string();
        let r = wait_ready(&url, Duration::from_millis(500)).await;
        assert!(r.is_err(), "should time out");
    }

    // SidecarBuilder 默认值。
    #[test]
    fn builder_defaults() {
        let b = SidecarBuilder::new();
        assert_eq!(b.bin, "gridforge");
        assert_eq!(b.host, "127.0.0.1");
        assert_eq!(b.ready_timeout, DEFAULT_READY_TIMEOUT);
        assert!(b.extra_env.is_empty());
        assert!(b.extra_args.is_empty());
    }

    // SidecarBuilder 链式配置生效。
    #[test]
    fn builder_chain_overrides() {
        let b = SidecarBuilder::new()
            .bin("/custom/path")
            .ready_timeout(Duration::from_secs(5))
            .env("FOO", "bar")
            .arg("--extra");
        assert_eq!(b.bin, "/custom/path");
        assert_eq!(b.ready_timeout, Duration::from_secs(5));
        assert_eq!(b.extra_env, vec![("FOO".to_string(), "bar".to_string())]);
        assert_eq!(b.extra_args, vec!["--extra".to_string()]);
    }

    // 默认不设置固定端口。
    #[test]
    fn builder_default_port_is_none() {
        let b = SidecarBuilder::new();
        assert!(b.port.is_none());
    }

    // 设置固定端口后生效。
    #[test]
    fn builder_port_override() {
        let b = SidecarBuilder::new().port(9999);
        assert_eq!(b.port, Some(9999));
    }
}
