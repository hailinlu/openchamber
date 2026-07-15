//! 配置加载 (CLI + env)。
//!
//! 对应现有: `packages/web/server/lib/opencode/cli-options.js` +
//! `packages/web/bin/lib/cli-args.js`。
//!
//! 环境变量与 Node 侧保持一致:
//!   - OPENCHAMBER_HOST / OPENCHAMBER_PORT
//!   - OPENCHAMBER_API_ONLY / OPENCHAMBER_UI_PASSWORD
//!   - OPENCHAMBER_DIST_DIR
//!   - OPENCODE_BINARY / OPENCODE_HOST / OPENCODE_PORT / OPENCODE_SKIP_START
//!   - OPENCHAMBER_OPENCODE_HOSTNAME
//!   - OPENCHAMBER_ALLOW_UNAUTHENTICATED_LAN

use std::net::IpAddr;
use std::path::PathBuf;

use clap::Parser;

/// OpenChamber Rust 后端配置。
#[derive(Debug, Clone, Parser)]
#[command(name = "oc-server", about = "OpenChamber Rust 后端 (迁移中)")]
pub struct Config {
    /// 绑定地址。默认 loopback (安全默认)。
    #[arg(long, env = "OPENCHAMBER_HOST", default_value = "127.0.0.1")]
    pub host: IpAddr,

    /// 绑定端口。0 = 自动分配。
    #[arg(long, env = "OPENCHAMBER_PORT", default_value_t = 0)]
    pub port: u16,

    /// 前台运行 (oc-server 本身就是前台进程, 此 flag 仅为 CLI parity)。
    #[arg(long)]
    pub foreground: bool,

    /// 仅 API 模式 (不托管静态 dist)。
    #[arg(long, env = "OPENCHAMBER_API_ONLY")]
    pub api_only: bool,

    /// UI 认证密码 (LAN 绑定时必需)。
    #[arg(long, env = "OPENCHAMBER_UI_PASSWORD")]
    pub ui_password: Option<String>,

    /// 静态 dist 目录 (默认: packages/web/dist)。
    #[arg(long, env = "OPENCHAMBER_DIST_DIR")]
    pub dist_dir: Option<PathBuf>,

    /// OpenCode 二进制路径。
    #[arg(long, env = "OPENCODE_BINARY", default_value = "opencode")]
    pub opencode_binary: String,

    /// 外部 OpenCode URL (跳过 spawn, 例如 http://hostname:4096)。
    #[arg(long, env = "OPENCODE_HOST")]
    pub opencode_host: Option<String>,

    /// 外部 OpenCode 端口。
    #[arg(long, env = "OPENCODE_PORT")]
    pub opencode_port: Option<u16>,

    /// 跳过启动 OpenCode (配合 OPENCODE_HOST 使用)。
    #[arg(long, env = "OPENCODE_SKIP_START")]
    pub opencode_skip_start: bool,

    /// OpenCode 绑定主机名 (managed spawn 用)。
    #[arg(long, env = "OPENCHAMBER_OPENCODE_HOSTNAME", default_value = "127.0.0.1")]
    pub opencode_hostname: String,

    /// 允许未认证 LAN 绑定 (危险, 显式 opt-in)。
    #[arg(long, env = "OPENCHAMBER_ALLOW_UNAUTHENTICATED_LAN")]
    pub allow_unauthenticated_lan: bool,

    /// OpenCode 服务器用户名 (Basic auth, 默认 "opencode")。
    #[arg(long, env = "OPENCODE_SERVER_USERNAME", default_value = "opencode")]
    pub opencode_username: String,

    /// 用户提供的 OpenCode 服务器密码 (不生成 managed password)。
    #[arg(long, env = "OPENCODE_SERVER_PASSWORD")]
    pub opencode_password: Option<String>,

    /// 要求 client 认证 (无密码模式下的增强门)。
    #[arg(long, env = "OPENCHAMBER_REQUIRE_CLIENT_AUTH")]
    pub require_client_auth: bool,
}

impl Config {
    pub fn load() -> anyhow::Result<Self> {
        Ok(Self::parse())
    }

    /// 是否使用外部 OpenCode (不 spawn)。
    pub fn is_external_opencode(&self) -> bool {
        self.opencode_skip_start || self.opencode_host.is_some()
    }

    /// 解析 dist 目录: 优先 OPENCHAMBER_DIST_DIR, 否则 fallback 到
    /// packages/web/dist (相对于 CARGO_MANIFEST_DIR / 运行目录)。
    pub fn resolve_dist_dir(&self) -> Option<PathBuf> {
        if let Some(ref d) = self.dist_dir {
            return Some(d.clone());
        }
        // 阶段 1: 不硬编码路径, 返回 None → static_files 走 api-only fallback。
        // 生产部署时通过 OPENCHAMBER_DIST_DIR 指定。
        None
    }
}
