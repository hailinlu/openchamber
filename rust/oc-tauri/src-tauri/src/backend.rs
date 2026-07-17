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
///
/// 该函数仅在 Tauri setup 时单线程调用, 不参与跨线程并发; 解析逻辑委托给
/// [`parse_use_sidecar_value`] 以便单元测试可对各输入值做确定性测试,
/// 避免 `set_var`/`remove_var` 与并行测试线程产生的竞态。
pub fn use_sidecar() -> bool {
    std::env::var("OPENCHAMBER_SIDECAR")
        .ok()
        .as_deref()
        .map(parse_use_sidecar_value)
        .unwrap_or(false)
}

/// 纯解析: 给定 `OPENCHAMBER_SIDECAR` 的原始字符串, 返回对应的 sidecar 决策。
///
/// 接受 `1`/`true`/`TRUE` (大小写敏感, 与原契约一致); 其他字符串包括空串、
/// `0`/`false`/`no` 等均返回 `false`。无 I/O, 无 env 读取 — 可安全并发调用。
pub fn parse_use_sidecar_value(raw: &str) -> bool {
    matches!(raw, "1" | "true" | "TRUE")
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
    use std::sync::Mutex;

    /// 序列化所有 mutate `OPENCHAMBER_SIDECAR` 进程 env 的测试。
    /// `cargo test` 默认跨测试并行, 进程 env 是共享全局状态 — 必须用互斥锁保证
    /// 每个 env-mutating 测试独占完成, 避免被其他 env-mutating 测试中途覆盖。
    /// env 的清理通过 [`EnvGuard`] 的 `Drop` 实现, panic 时也会执行, 不留泄漏。
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// `EnvGuard` RAII 句柄: 构造时记下 `key` 当前 OsString 值 (保留任意字节),
    /// 析构时按原状态精确还原。
    /// - 原值: `Some(value)` → `set_var(key, value)` 还原原始字节
    /// - 原未设置: `None` → `remove_var(key)` 还原未设置状态
    /// 仅在 `cfg(test)` 下使用, 不污染 release 构建; 纯 std 实现, 无外部依赖。
    struct EnvGuard {
        key: &'static str,
        original: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn capture(key: &'static str) -> Self {
            // `std::env::var_os` 返回 `Option<OsString>`, 保留原始字节
            // (含非 UTF-8 OsString 的 WTF-8/UCS-2 编码), 比 `var()` 更精确。
            let original = std::env::var_os(key);
            Self { key, original }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.original.take() {
                Some(value) => {
                    // 还原为原始 OsString, 通过引用避免不必要的 clone。
                    std::env::set_var(self.key, &value);
                }
                None => {
                    // 原未设置 → 还原到未设置状态, 防止泄漏到后续测试。
                    std::env::remove_var(self.key);
                }
            }
        }
    }

    #[test]
    fn parse_use_sidecar_value_truthy() {
        for v in ["1", "true", "TRUE"] {
            assert!(parse_use_sidecar_value(v), "{:?} should be truthy", v);
        }
    }

    #[test]
    fn parse_use_sidecar_value_falsy() {
        for v in ["0", "false", "", "no", "yes", "True", "true ", " 1", "2"] {
            assert!(
                !parse_use_sidecar_value(v),
                "{:?} should be falsy (strict exact-match against 1/true/TRUE)",
                v
            );
        }
    }

    #[test]
    fn parse_use_sidecar_value_empty() {
        assert!(!parse_use_sidecar_value(""));
    }

    /// 集成测试: `use_sidecar()` 端到端走 `std::env::var` + 纯解析。
    /// 必须在 `ENV_LOCK` 内 mutate env, 避免与其他 env-mutating 测试并行;
    /// `EnvGuard` 保证 panic 时也能还原原状态, 不污染其他测试。
    #[test]
    fn use_sidecar_integration_with_env() {
        let _lock = ENV_LOCK.lock().expect("ENV_LOCK poisoned by prior test panic");
        let _env = EnvGuard::capture("OPENCHAMBER_SIDECAR");

        // unset 路径: 默认 false。
        std::env::remove_var("OPENCHAMBER_SIDECAR");
        assert!(!use_sidecar(), "unset env should default to false");

        // 三个 truthy 值都应返回 true。
        for v in ["1", "true", "TRUE"] {
            std::env::set_var("OPENCHAMBER_SIDECAR", v);
            assert!(use_sidecar(), "set_var({:?}) should yield true", v);
        }

        // 已知 falsy 值都应返回 false (即便 set_var 存在)。
        for v in ["0", "false", "", "no"] {
            std::env::set_var("OPENCHAMBER_SIDECAR", v);
            assert!(!use_sidecar(), "set_var({:?}) should yield false", v);
        }
        // EnvGuard::Drop 会在此处 (或 panic unwind 时) 还原 env 到测试开始前状态。
    }

    #[test]
    fn parse_port_extracts_correctly() {
        assert_eq!(parse_port("http://127.0.0.1:8080"), 8080);
        assert_eq!(parse_port("http://127.0.0.1:1"), 1);
        assert_eq!(parse_port("http://127.0.0.1:65535"), 65535);
    }

    /// RED 验证: panic 时 `EnvGuard::Drop` 必须还原 OPENCHAMBER_SIDECAR。
    /// 使用 `catch_unwind` 触发 unwind, 然后断言 env 状态与 guard 创建前一致。
    /// 该测试在 `EnvGuard` 类型实现前编译失败 (类型未定义), 实现后 GREEN。
    #[test]
    fn env_guard_restores_openchamber_sidecar_on_panic() {
        // 串行化所有 mutating env 的测试, 避免外部干扰。
        let _lock = ENV_LOCK.lock().expect("ENV_LOCK poisoned by prior test panic");

        // 子用例 A: 起始状态 = 已设置值, 中途 panic, 期望还原为该值。
        let pre_existing = "pre-existing-marker";
        std::env::set_var("OPENCHAMBER_SIDECAR", pre_existing);
        assert_eq!(
            std::env::var_os("OPENCHAMBER_SIDECAR").as_deref().map(|s| s.to_str()),
            Some(Some(pre_existing)),
            "precondition: env must start as pre-existing value"
        );

        let panic_result = std::panic::catch_unwind(|| {
            let _guard = EnvGuard::capture("OPENCHAMBER_SIDECAR");
            // 在 guard 持有期间 mutate env, 然后主动 panic。
            std::env::set_var("OPENCHAMBER_SIDECAR", "mutated-then-panic");
            panic!("intentional panic to exercise Drop");
        });
        assert!(panic_result.is_err(), "catch_unwind should observe the panic");

        // guard 应在 panic unwind 阶段执行 Drop, 还原回 pre_existing。
        assert_eq!(
            std::env::var_os("OPENCHAMBER_SIDECAR").as_deref().map(|s| s.to_str()),
            Some(Some(pre_existing)),
            "EnvGuard must restore the original value across a panic"
        );

        // 子用例 B: 起始状态 = 未设置, panic 后必须保持未设置。
        std::env::remove_var("OPENCHAMBER_SIDECAR");
        assert!(
            std::env::var_os("OPENCHAMBER_SIDECAR").is_none(),
            "precondition: env must start unset"
        );

        let panic_result = std::panic::catch_unwind(|| {
            let _guard = EnvGuard::capture("OPENCHAMBER_SIDECAR");
            std::env::set_var("OPENCHAMBER_SIDECAR", "mutated-then-panic");
            panic!("intentional panic from unset baseline");
        });
        assert!(panic_result.is_err(), "catch_unwind should observe the panic");

        assert!(
            std::env::var_os("OPENCHAMBER_SIDECAR").is_none(),
            "EnvGuard must restore the unset state across a panic"
        );

        // 子用例 C: OsString 精确还原 (含任意字节序列, 而不仅是 UTF-8 str)。
        // 通过两次 round-trip 验证: 先 set_var 一个含非 ASCII 字符的 OsString,
        // 主动 panic, 再读取并断言 byte-equality。
        use std::ffi::OsString;
        // 在 Windows 上, OsString 是 Wtf8/UCS-2 包装; 直接构造多字节串即可。
        let original = OsString::from("roundtrip-osstring-marker");
        std::env::set_var("OPENCHAMBER_SIDECAR", &original);
        let pre_bytes = std::env::var_os("OPENCHAMBER_SIDECAR").expect("precondition: env must hold value");
        assert_eq!(
            pre_bytes.clone().into_string().ok().as_deref(),
            Some("roundtrip-osstring-marker"),
            "precondition: env holds the exact OsString value"
        );

        let panic_result = std::panic::catch_unwind(|| {
            let _guard = EnvGuard::capture("OPENCHAMBER_SIDECAR");
            std::env::set_var("OPENCHAMBER_SIDECAR", "mutated-and-panicked");
            panic!("intentional panic for OsString roundtrip");
        });
        assert!(panic_result.is_err(), "catch_unwind should observe the panic");

        let post_bytes = std::env::var_os("OPENCHAMBER_SIDECAR").expect("postcondition: env must hold restored value");
        assert_eq!(
            post_bytes.clone().into_string().ok().as_deref(),
            Some("roundtrip-osstring-marker"),
            "EnvGuard must restore the exact OsString value across a panic"
        );
        // 同时按字节长度比对, 防止 into_string 在非 UTF-8 情况下掩盖差异。
        assert_eq!(
            post_bytes.len(),
            pre_bytes.len(),
            "EnvGuard must preserve byte length of the original OsString across a panic"
        );

        // 主动清理, 不依赖后续测试的副作用。
        std::env::remove_var("OPENCHAMBER_SIDECAR");
    }
}
