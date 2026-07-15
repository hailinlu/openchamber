//! MiniMax Coding Plan (minimax.io)。

use crate::quota::providers::minimax_shared::{create_minimax_coding_plan_provider, MiniMaxProviderConfig};

pub const PROVIDER_ID: &str = "minimax-coding-plan";
pub const PROVIDER_NAME: &str = "MiniMax Coding Plan (minimax.io)";
pub const ALIASES: &[&str] = &["minimax-coding-plan"];

static PROVIDER: std::sync::OnceLock<crate::quota::providers::minimax_shared::MiniMaxProvider> =
    std::sync::OnceLock::new();

fn provider() -> &'static crate::quota::providers::minimax_shared::MiniMaxProvider {
    PROVIDER.get_or_init(|| {
        create_minimax_coding_plan_provider(MiniMaxProviderConfig {
            provider_id: PROVIDER_ID,
            provider_name: PROVIDER_NAME,
            aliases: ALIASES,
            token_plan_url: "https://api.minimax.io/v1/token_plan/remains",
            coding_plan_url: "https://api.minimax.io/v1/api/openplatform/coding_plan/remains",
        })
    })
}

pub fn is_configured() -> bool {
    (provider().is_configured)()
}

pub fn fetch_quota() -> serde_json::Value {
    (provider().fetch_quota)()
}

pub fn fetch_quota_sync() -> serde_json::Value {
    fetch_quota()
}

pub use fetch_quota as fetch_minimax_coding_plan_quota;
