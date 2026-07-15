//! Small-model 模块 — OpenAI-compatible / Anthropic / Google / OpenAI-codex-SSE provider 派发。
//!
//! 对应 Node `small-model/` (1307 行) 的 1:1 移植:
//!   - `routes.js`        — 2 HTTP handler (GET /api/small-model + POST /api/small-model/generate)
//!   - `resolve.js`       — resolve_small_model + family priority
//!   - `call.js`          — call_small_model + 4 provider dispatch
//!   - `index.js`         — generate / describe / list authenticated
//!   - `catalog.js`       — delegate to opencode/models_metadata
//!
//! 外部依赖:
//!   - `crate::opencode::auth` — provider auth (api key / oauth credentials)
//!   - `crate::opencode::models_metadata` — models.dev catalog
//!   - `crate::opencode::config` — provider config + working directory

pub mod call;
pub mod index;
pub mod resolve;
pub mod routes;

/// small-model 服务占位 (unit struct,stateless)。
///
/// 当前不持有任何状态(所有调用都走 module-level static + 临时 fetch)。
/// 保留为 `Arc<SmallModelService>` 字段便于未来添加 per-session 缓存或后端路由选择。
pub struct SmallModelService;

impl SmallModelService {
    pub fn new() -> Self {
        Self
    }
}
