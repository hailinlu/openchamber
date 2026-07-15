//! Tunnel provider 子模块。
//!
//! 两个实现:
//!   - [`cloudflare`]: cloudflared 子进程 (3 种模式: quick/managed-remote/managed-local)
//!   - [`ngrok`]: ngrok 子进程 (1 种模式: quick)
//!
//! 不使用 `#[async_trait]` trait (避免引入 async-trait 依赖)。
//! 改用模块级 async 函数 + enum dispatch。

pub mod cloudflare;
pub mod ngrok;

use std::path::PathBuf;
use std::process::Stdio;

use serde_json::Value;
use tokio::process::Child;

use crate::tunnels::types::TunnelServiceError;

/// 隧道控制器: 持有子进程 + 元数据。
pub struct TunnelController {
    pub mode: String,
    pub provider: Option<String>,
    pub public_url: Option<String>,
    pub child: Option<Child>,
    pub config_path: Option<PathBuf>,
    pub resolved_hostname: Option<String>,
    /// cleanup 函数 (临时文件清理等)。
    cleanup: Option<Box<dyn FnOnce() + Send + Sync>>,
}

impl TunnelController {
    pub fn new(mode: impl Into<String>) -> Self {
        Self {
            mode: mode.into(),
            provider: None,
            public_url: None,
            child: None,
            config_path: None,
            resolved_hostname: None,
            cleanup: None,
        }
    }

    pub fn with_public_url(mut self, url: impl Into<String>) -> Self {
        self.public_url = Some(url.into());
        self
    }

    pub fn with_child(mut self, child: Child) -> Self {
        self.child = Some(child);
        self
    }

    pub fn with_cleanup<F>(mut self, cleanup: F) -> Self
    where
        F: FnOnce() + Send + Sync + 'static,
    {
        self.cleanup = Some(Box::new(cleanup));
        self
    }

    pub fn with_config_path(mut self, path: PathBuf) -> Self {
        self.config_path = Some(path);
        self
    }

    pub fn with_resolved_hostname(mut self, hostname: impl Into<String>) -> Self {
        self.resolved_hostname = Some(hostname.into());
        self
    }

    #[allow(dead_code)]
    pub fn get_public_url(&self) -> Option<&str> {
        self.public_url.as_deref()
    }

    pub fn get_effective_config_path(&self) -> Option<&str> {
        self.config_path.as_deref().and_then(|p| p.to_str())
    }

    pub fn get_resolved_hostname(&self) -> Option<&str> {
        self.resolved_hostname.as_deref()
    }

    /// 停止子进程 + 运行 cleanup。
    pub fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            // 先尝试 kill (SIGKILL 等价)
            let _ = child.start_kill();
            // 不等待 — 让进程在后台退出
        }
        if let Some(cleanup) = self.cleanup.take() {
            cleanup();
        }
    }
}

impl Drop for TunnelController {
    fn drop(&mut self) {
        // 确保 cleanup 在 drop 时运行 (即使没显式调用 stop)
        if let Some(cleanup) = self.cleanup.take() {
            cleanup();
        }
    }
}

/// provider 启动上下文。
#[derive(Debug, Clone)]
pub struct StartContext {
    pub active_port: Option<u16>,
    pub origin_url: Option<String>,
}

/// 构建 spawn Command 的公共辅助 (windowsHide 等价)。
pub(crate) fn build_spawn_command(
    program: &str,
    args: &[&str],
    env: &std::collections::HashMap<String, String>,
) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args);
    cmd.envs(env);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    // Windows: 隐藏控制台窗口
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    cmd
}

/// provider capabilities JSON (静态)。
pub fn list_capabilities() -> Vec<Value> {
    vec![
        cloudflare::capabilities(),
        ngrok::capabilities(),
    ]
}

/// provider 检查可用性。
pub async fn check_availability(provider: &str) -> Result<Value, TunnelServiceError> {
    match provider {
        "ngrok" => Ok(ngrok::check_availability().await),
        _ => Ok(cloudflare::check_availability().await),
    }
}

/// provider 诊断。
pub async fn diagnose(
    provider: &str,
    request: &Value,
) -> Result<Value, TunnelServiceError> {
    match provider {
        "ngrok" => Ok(ngrok::diagnose(request).await),
        _ => Ok(cloudflare::diagnose(request).await),
    }
}

/// provider 启动隧道。
pub async fn start(
    provider: &str,
    request: &crate::tunnels::types::NormalizedTunnelStartRequest,
    context: &StartContext,
) -> Result<TunnelController, TunnelServiceError> {
    match provider {
        "ngrok" => ngrok::start(request, context).await,
        _ => cloudflare::start(request, context).await,
    }
}
