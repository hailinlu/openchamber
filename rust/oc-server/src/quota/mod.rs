//! Quota module — usage/quota providers for various AI services.
//!
//! 移植自 `packages/web/server/lib/quota/`:
//!   - `utils/` — 共享 helper (auth resolution, transformers, formatters)
//!   - `credentials/` — 受管的 quota credential 读写 (atomic write, mode 0o600)
//!   - `providers/` — 各 provider 的 fetch_quota 实现
//!   - `routes.rs` — 7 个 HTTP 端点
//!
//! 与 Node 行为字节对齐 (provider ID, response shape, status code)。

pub mod credentials;
pub mod providers;
pub mod routes;
pub mod utils;

/// 在当前 tokio runtime 内 block-on 给定 future。
///
/// 调用方必须在 tokio runtime 内(例如 axum handler 或 `#[tokio::test]`)。
pub fn block_on<F: std::future::Future>(future: F) -> F::Output {
    let handle = tokio::runtime::Handle::try_current()
        .expect("quota::block_on called outside tokio runtime");
    tokio::task::block_in_place(|| handle.block_on(future))
}

/// 简单的 reqwest client (15s timeout),各 provider 复用。
pub(crate) fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}
