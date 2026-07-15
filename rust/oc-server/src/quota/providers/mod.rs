//! Quota providers — 各 provider 的 fetch_quota 实现 + 全局 registry。
//!
//! 与 Node `quota/providers/index.js` 行为字节对齐:
//!   - 17 providers 注册到静态 registry
//!   - `list_configured_quota_providers` 返回已配置 provider ID 列表
//!   - `fetch_quota_for_provider` 走 registry + 错误 catch-all

pub mod claude;
pub mod codex;
pub mod copilot;
pub mod cursor;
pub mod google;
pub mod kimi;
pub mod minimax_cn_coding_plan;
pub mod minimax_coding_plan;
pub mod minimax_shared;
pub mod nanogpt;
pub mod ollama_cloud;
pub mod openai;
pub mod opencode_go;
pub mod openrouter;
pub mod wafer;
pub mod zai;
pub mod zhipuai_coding_plan;

use serde_json::{json, Value};

use crate::quota::utils::formatters::build_result;

/// 单个 provider 的元信息 (id, name, isConfigured, fetchQuota)。
pub struct QuotaProvider {
    pub provider_id: &'static str,
    pub provider_name: &'static str,
    pub is_configured: fn() -> bool,
    pub fetch_quota: fn() -> Value,
}

/// Registry: provider ID → entry (与 Node `registry` 字节对齐)。
///
/// 顺序与 Node 一致 — 影响 `list_configured_quota_providers` 返回顺序。
pub static REGISTRY: &[(&str, QuotaProvider)] = &[
    (
        "claude",
        QuotaProvider {
            provider_id: claude::PROVIDER_ID,
            provider_name: claude::PROVIDER_NAME,
            is_configured: claude::is_configured,
            fetch_quota: claude::fetch_quota_sync,
        },
    ),
    (
        "codex",
        QuotaProvider {
            provider_id: codex::PROVIDER_ID,
            provider_name: codex::PROVIDER_NAME,
            is_configured: codex::is_configured,
            fetch_quota: codex::fetch_quota_sync,
        },
    ),
    (
        "cursor",
        QuotaProvider {
            provider_id: cursor::PROVIDER_ID,
            provider_name: cursor::PROVIDER_NAME,
            is_configured: cursor::is_configured,
            fetch_quota: cursor::fetch_quota_sync,
        },
    ),
    (
        "google",
        QuotaProvider {
            provider_id: google::PROVIDER_ID,
            provider_name: google::PROVIDER_NAME,
            is_configured: google::is_configured,
            fetch_quota: google::fetch_google_quota_sync,
        },
    ),
    (
        "zai-coding-plan",
        QuotaProvider {
            provider_id: zai::PROVIDER_ID,
            provider_name: zai::PROVIDER_NAME,
            is_configured: zai::is_configured,
            fetch_quota: zai::fetch_quota_sync,
        },
    ),
    (
        "zhipuai-coding-plan",
        QuotaProvider {
            provider_id: zhipuai_coding_plan::PROVIDER_ID,
            provider_name: zhipuai_coding_plan::PROVIDER_NAME,
            is_configured: zhipuai_coding_plan::is_configured,
            fetch_quota: zhipuai_coding_plan::fetch_quota_sync,
        },
    ),
    (
        "kimi-for-coding",
        QuotaProvider {
            provider_id: kimi::PROVIDER_ID,
            provider_name: kimi::PROVIDER_NAME,
            is_configured: kimi::is_configured,
            fetch_quota: kimi::fetch_quota_sync,
        },
    ),
    (
        "openrouter",
        QuotaProvider {
            provider_id: openrouter::PROVIDER_ID,
            provider_name: openrouter::PROVIDER_NAME,
            is_configured: openrouter::is_configured,
            fetch_quota: openrouter::fetch_quota_sync,
        },
    ),
    (
        "nano-gpt",
        QuotaProvider {
            provider_id: nanogpt::PROVIDER_ID,
            provider_name: nanogpt::PROVIDER_NAME,
            is_configured: nanogpt::is_configured,
            fetch_quota: nanogpt::fetch_quota_sync,
        },
    ),
    (
        "github-copilot",
        QuotaProvider {
            provider_id: copilot::PROVIDER_ID,
            provider_name: copilot::PROVIDER_NAME,
            is_configured: copilot::is_configured,
            fetch_quota: copilot::fetch_quota_sync,
        },
    ),
    (
        "github-copilot-addon",
        QuotaProvider {
            provider_id: copilot::PROVIDER_ID_ADDON,
            provider_name: copilot::PROVIDER_NAME_ADDON,
            is_configured: copilot::is_configured,
            fetch_quota: copilot::fetch_quota_addon_sync,
        },
    ),
    (
        "minimax-coding-plan",
        QuotaProvider {
            provider_id: minimax_coding_plan::PROVIDER_ID,
            provider_name: minimax_coding_plan::PROVIDER_NAME,
            is_configured: minimax_coding_plan::is_configured,
            fetch_quota: minimax_coding_plan::fetch_quota_sync,
        },
    ),
    (
        "minimax-cn-coding-plan",
        QuotaProvider {
            provider_id: minimax_cn_coding_plan::PROVIDER_ID,
            provider_name: minimax_cn_coding_plan::PROVIDER_NAME,
            is_configured: minimax_cn_coding_plan::is_configured,
            fetch_quota: minimax_cn_coding_plan::fetch_quota_sync,
        },
    ),
    (
        "ollama-cloud",
        QuotaProvider {
            provider_id: ollama_cloud::PROVIDER_ID,
            provider_name: ollama_cloud::PROVIDER_NAME,
            is_configured: ollama_cloud::is_configured,
            fetch_quota: ollama_cloud::fetch_quota_sync,
        },
    ),
    (
        "wafer",
        QuotaProvider {
            provider_id: wafer::PROVIDER_ID,
            provider_name: wafer::PROVIDER_NAME,
            is_configured: wafer::is_configured,
            fetch_quota: wafer::fetch_quota_sync,
        },
    ),
    (
        "opencode-go",
        QuotaProvider {
            provider_id: opencode_go::PROVIDER_ID,
            provider_name: opencode_go::PROVIDER_NAME,
            is_configured: opencode_go::is_configured,
            fetch_quota: opencode_go::fetch_quota_sync,
        },
    ),
];

/// 列出所有已配置的 provider ID。
///
/// 对应 Node `listConfiguredQuotaProviders` — 单 provider 报错时 catch-ignore。
pub fn list_configured_quota_providers() -> Vec<String> {
    let mut out = Vec::new();
    for (id, p) in REGISTRY {
        let f = p.is_configured;
        // 同 Node: try/catch 隔离单个 provider 异常
        let configured = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f()))
            .ok()
            .unwrap_or(false);
        if configured {
            out.push((*id).to_string());
        }
    }
    out
}

/// 同步 driver: 因为 fn pointer 不能直接 await,我们这里实现轻量级同步 wrapper。
///
/// 每个 provider 的 `fetch_*` 是 `tokio::runtime::Handle::block_on`-friendly 同步 fn。
pub fn fetch_quota_for_provider(provider_id: &str) -> Value {
    for (id, p) in REGISTRY {
        if *id == provider_id {
            return (p.fetch_quota)();
        }
    }
    build_result(crate::quota::utils::formatters::BuildResultArgs {
        provider_id,
        provider_name: provider_id,
        ok: false,
        configured: false,
        usage: None,
        error: Some("Unsupported provider"),
    })
}

/// 给出公开的 fetch fns (与 Node `fetchXxxQuota` 对齐,作为模块顶层 re-export)。
pub use claude::fetch_claude_quota;
pub use codex::fetch_codex_quota;
pub use copilot::{fetch_copilot_addon_quota, fetch_copilot_quota};
pub use cursor::fetch_cursor_quota;
pub use google::fetch_google_quota;
pub use kimi::fetch_kimi_quota;
pub use minimax_cn_coding_plan::fetch_minimax_cn_coding_plan_quota;
pub use minimax_coding_plan::fetch_minimax_coding_plan_quota;
pub use nanogpt::fetch_nanogpt_quota;
pub use ollama_cloud::fetch_ollama_cloud_quota;
pub use openai::fetch_openai_quota;
pub use openrouter::fetch_openrouter_quota;
pub use wafer::fetch_wafer_quota;
pub use zai::fetch_zai_quota;
pub use zhipuai_coding_plan::fetch_zhipuai_quota;

/// 通用 Helper: 标准 "not configured" 响应。
pub(crate) fn not_configured(provider_id: &'static str, provider_name: &'static str) -> Value {
    build_result(crate::quota::utils::formatters::BuildResultArgs {
        provider_id,
        provider_name,
        ok: false,
        configured: false,
        usage: None,
        error: Some("Not configured"),
    })
}

/// 通用 Helper: `fetch_quota` 抛错时返回的错误响应。
pub(crate) fn fetch_error(provider_id: &'static str, provider_name: &'static str, msg: &str) -> Value {
    build_result(crate::quota::utils::formatters::BuildResultArgs {
        provider_id,
        provider_name,
        ok: false,
        configured: true,
        usage: None,
        error: Some(msg),
    })
}

/// 通用 Helper: `API error` 响应。
pub(crate) fn api_error(provider_id: &'static str, provider_name: &'static str, status: u16) -> Value {
    build_result(crate::quota::utils::formatters::BuildResultArgs {
        provider_id,
        provider_name,
        ok: false,
        configured: true,
        usage: None,
        error: Some(&format!("API error: {status}")[..]),
    })
}
