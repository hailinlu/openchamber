//! 配置加载 (阶段 0 骨架)。
//!
//! 对应现有: `packages/web/server/lib/opencode/cli-options.js`。
//! 阶段 1 会完整实现 CLI (clap) + env 解析, 并复刻绑定地址安全检查
//! (`lib/security/bind-host.js`: 拒绝未认证 LAN 绑定)。

use clap::Parser;
use std::net::IpAddr;

/// OpenChamber Rust 后端配置。
///
/// 阶段 0: 仅最小字段 (host/port)。阶段 1 扩展为完整 CLI parity。
#[derive(Debug, Clone, Parser)]
#[command(name = "oc-server", about = "OpenChamber Rust 后端 (迁移中)")]
pub struct Config {
    /// 绑定地址。默认 loopback (安全默认)。
    #[arg(long, env = "OPENCHAMBER_HOST", default_value = "127.0.0.1")]
    pub host: IpAddr,

    /// 绑定端口。0 = 自动分配。
    #[arg(long, env = "OPENCHAMBER_PORT", default_value_t = 0)]
    pub port: u16,
}

impl Config {
    pub fn load() -> anyhow::Result<Self> {
        Ok(Self::parse())
    }
}
