//! Quota utilities — 共享 helper 模块。
//!
//! 对应 Node `quota/utils/{auth,transformers,formatters,index}.js`。

pub mod auth;
pub mod formatters;
pub mod transformers;

// 重新导出常用符号,保持内部调用简洁。
pub use auth::{get_auth_entry, normalize_auth_entry, read_json_file, ANTIGRAVITY_ACCOUNTS_PATHS};
pub use formatters::{
    build_result, calculate_reset_after_seconds, duration_to_label, duration_to_seconds,
    format_money, format_reset_time, to_usage_window,
};
pub use transformers::{
    as_non_empty_string, as_object, normalize_timestamp, resolve_window_label,
    resolve_window_seconds, to_number, to_timestamp,
};
