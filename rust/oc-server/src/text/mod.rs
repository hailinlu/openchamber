//! 文本处理模块。
//!
//! 对应现有: `packages/web/server/lib/text/`。
//!
//! 移植 `summarization.js` 的全部正则管道 + 蒸馏逻辑。
//! Zen 模型摘要已退役, 所有 mode 返回本地净化/蒸馏文本。

pub mod routes;
pub mod summarization;
