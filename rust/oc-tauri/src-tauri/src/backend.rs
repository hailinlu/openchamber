//! 统一后端句柄: 进程内嵌入 (默认) 或 sidecar 回退。
//!
//! Phase 4B: Tauri setup 按 `OPENCHAMBER_SIDECAR` env 选择后端路径。
//! 两条路径通过 `BackendHandle` 枚举暴露统一的 `base_url()` + `shutdown()`。

use crate::sidecar::SidecarHandle;
use oc_server::OcServer;

/// 后端句柄 — 封装进程内嵌入或 sidecar 两种实现。
pub enum BackendHandle {
    /// 进程内嵌入的 oc-server (默认路径)。
    InProcess(OcServer),
    /// sidecar 子进程 (OPENCHAMBER_SIDECAR=1 回退路径)。
    Sidecar(SidecarHandle),
}

impl BackendHandle {
    /// 返回 `http://127.0.0.1:<port>`, 供 WebView 加载和 IPC 命令使用。
    pub fn base_url(&self) -> String {
        match self {
            Self::InProcess(s) => s.base_url().to_string(),
            Self::Sidecar(h) => h.base_url(),
        }
    }

    /// 优雅关闭 — 按各自路径执行 shutdown/kill。
    pub async fn shutdown(self) {
        match self {
            Self::InProcess(s) => s.shutdown().await,
            Self::Sidecar(mut h) => {
                let _ = h.kill().await;
            }
        }
    }
}

/// 决策: 是否走 sidecar 回退路径。
///
/// 环境变量 `OPENCHAMBER_SIDECAR` 为 `1`/`true`/`TRUE` 时返回 true,
/// 未设置或其他值返回 false (默认进程内嵌入)。
pub fn use_sidecar() -> bool {
    std::env::var("OPENCHAMBER_SIDECAR")
        .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE"))
        .unwrap_or(false)
}

/// 从 `http://host:port` 提取端口号。
pub fn parse_port(base_url: &str) -> u16 {
    base_url
        .rsplit(':')
        .next()
        .and_then(|s| s.parse().ok())
        .expect("base_url should contain port")
}

// =========================================================================
// 测试
// =========================================================================
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn use_sidecar_defaults_false() {
        // 注意: env 测试需串行 (--test-threads=1 已是 oc-tauri 惯例)。
        std::env::remove_var("OPENCHAMBER_SIDECAR");
        assert!(!use_sidecar());
    }

    #[test]
    fn use_sidecar_truthy_values() {
        for v in ["1", "true", "TRUE"] {
            std::env::set_var("OPENCHAMBER_SIDECAR", v);
            assert!(use_sidecar(), "value {} should be truthy", v);
        }
        std::env::remove_var("OPENCHAMBER_SIDECAR");
    }

    #[test]
    fn use_sidecar_falsy_values() {
        for v in ["0", "false", "", "no"] {
            std::env::set_var("OPENCHAMBER_SIDECAR", v);
            assert!(!use_sidecar(), "value {:?} should be falsy", v);
        }
        std::env::remove_var("OPENCHAMBER_SIDECAR");
    }

    #[test]
    fn parse_port_extracts_correctly() {
        assert_eq!(parse_port("http://127.0.0.1:8080"), 8080);
        assert_eq!(parse_port("http://127.0.0.1:1"), 1);
        assert_eq!(parse_port("http://127.0.0.1:65535"), 65535);
    }
}
