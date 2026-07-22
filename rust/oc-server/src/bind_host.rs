//! 绑定地址安全检查。
//!
//! 移植自 `packages/web/server/lib/security/bind-host.js`。
//!
//! 核心规则: 如果绑定地址不是 loopback 且没有 UI 认证密码,
//! 拒绝启动 (防止意外暴露到 LAN)。

use std::net::IpAddr;

use crate::config::Config;

/// 判断 IP 是否为 loopback。
///
/// 对应 Node 侧 `isLoopbackBindHost`:
///   - `127.x.x.x` (整个 /8) → loopback
///   - `::1` → loopback
///   - `::ffff:127.x.x.x` (IPv4-mapped) → loopback
fn is_loopback_ip(addr: &IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => {
            // ::1
            if v6.is_loopback() {
                return true;
            }
            // ::ffff:127.x.x.x (IPv4-mapped IPv6)
            if let Some(mapped) = v6.to_ipv4() {
                return mapped.is_loopback();
            }
            false
        }
    }
}

/// 判断绑定地址是否为网络暴露的 (非 loopback)。
pub fn is_network_exposed(addr: &IpAddr) -> bool {
    !is_loopback_ip(addr)
}

/// 绑定安全检查: 如果网络暴露且无认证, 返回错误。
///
/// 对应 Node 侧 `server/index.js` line 1251-1260 的检查逻辑。
pub fn enforce(config: &Config) -> anyhow::Result<()> {
    if is_network_exposed(&config.host)
        && config.ui_password.is_none()
        && !config.allow_unauthenticated_lan
    {
        anyhow::bail!(
            "GridForge refuses to bind to {} without UI authentication. \
             Set --ui-password or GRIDFORGE_UI_PASSWORD before exposing it over LAN, \
             or set GRIDFORGE_ALLOW_UNAUTHENTICATED_LAN=true to accept the risk.",
            config.host
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_v4_127_any() {
        assert!(is_loopback_ip(&"127.0.0.1".parse().unwrap()));
        assert!(is_loopback_ip(&"127.0.1.1".parse().unwrap()));
        assert!(is_loopback_ip(&"127.255.255.255".parse().unwrap()));
    }

    #[test]
    fn loopback_v6() {
        assert!(is_loopback_ip(&"::1".parse().unwrap()));
    }

    #[test]
    fn loopback_v4_mapped_v6() {
        assert!(is_loopback_ip(&"::ffff:127.0.0.1".parse().unwrap()));
    }

    #[test]
    fn non_loopback_v4() {
        assert!(!is_loopback_ip(&"0.0.0.0".parse().unwrap()));
        assert!(!is_loopback_ip(&"192.168.1.1".parse().unwrap()));
        assert!(!is_loopback_ip(&"10.0.0.1".parse().unwrap()));
    }

    #[test]
    fn non_loopback_v6() {
        assert!(!is_loopback_ip(&"::".parse().unwrap()));
        assert!(!is_loopback_ip(&"::ffff:0.0.0.0".parse().unwrap()));
    }

    #[test]
    fn network_exposed_detection() {
        assert!(!is_network_exposed(&"127.0.0.1".parse().unwrap()));
        assert!(is_network_exposed(&"0.0.0.0".parse().unwrap()));
        assert!(is_network_exposed(&"192.168.1.1".parse().unwrap()));
    }
}
