//! `utils/auth.js` 移植:
//!   - ANTIGRAVITY_ACCOUNTS_PATHS (从 opencode config/data 目录派生)
//!   - readJsonFile
//!   - getAuthEntry
//!   - normalizeAuthEntry

use std::path::PathBuf;

use serde_json::Value;

use crate::opencode::paths::{opencode_config_dir, opencode_data_dir};

/// `~/.config/opencode/antigravity-accounts.json` 与
/// `~/.local/share/opencode/antigravity-accounts.json`。
pub static ANTIGRAVITY_ACCOUNTS_PATHS: [PathBuf; 2] = [
    PathBuf::new(), // 占位 — 见 lazy build
    PathBuf::new(),
];

/// 惰性构造 antigravity 搜索路径,避免在 const 上下文使用 fn。
pub fn antigravity_accounts_paths() -> Vec<PathBuf> {
    vec![
        opencode_config_dir().join("antigravity-accounts.json"),
        opencode_data_dir().join("antigravity-accounts.json"),
    ]
}

/// 读取 JSON 文件,失败/不存在返回 `None`。
pub fn read_json_file(file_path: &std::path::Path) -> Option<Value> {
    let raw = match std::fs::read_to_string(file_path) {
        Ok(c) => c,
        Err(_) => return None,
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    match serde_json::from_str::<Value>(trimmed) {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!(path = %file_path.display(), error = %e, "failed to parse JSON file");
            None
        }
    }
}

/// 在 auth map 中按 alias 顺序查找 entry。
///
/// 对应 Node `getAuthEntry`:
/// ```js
/// for (const alias of aliases) {
///   if (auth[alias]) return auth[alias];
/// }
/// return null;
/// ```
pub fn get_auth_entry<'a>(auth: &'a Value, aliases: &[&str]) -> Option<&'a Value> {
    let obj = auth.as_object()?;
    for alias in aliases {
        if let Some(v) = obj.get(*alias) {
            return Some(v);
        }
    }
    None
}

/// 把 string / object entry 统一为可比较形状。
///
/// 对应 Node `normalizeAuthEntry`:
///   - string → `{ token: entry }`
///   - object → entry itself
///   - 其他 → null
pub fn normalize_auth_entry(entry: Option<&Value>) -> Option<Value> {
    let value = entry?;
    if let Some(s) = value.as_str() {
        Some(Value::Object({
            let mut m = serde_json::Map::new();
            m.insert("token".to_string(), Value::String(s.to_string()));
            m
        }))
    } else if value.is_object() {
        Some(value.clone())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalize_string_entry() {
        let entry = json!("sk-abc");
        let n = normalize_auth_entry(Some(&entry)).unwrap();
        assert_eq!(n, json!({"token": "sk-abc"}));
    }

    #[test]
    fn normalize_object_entry() {
        let entry = json!({"access": "tok", "refresh": "ref"});
        let n = normalize_auth_entry(Some(&entry)).unwrap();
        assert_eq!(n, entry);
    }

    #[test]
    fn normalize_null_returns_none() {
        assert!(normalize_auth_entry(None).is_none());
    }

    #[test]
    fn normalize_other_returns_none() {
        let entry = json!(42);
        assert!(normalize_auth_entry(Some(&entry)).is_none());
    }

    #[test]
    fn get_auth_entry_first_match_wins() {
        let auth = json!({
            "openai": {"access": "openai-tok"},
            "claude": {"access": "claude-tok"}
        });
        let aliases = vec!["openai", "claude"];
        let e = get_auth_entry(&auth, &aliases).unwrap();
        assert_eq!(e, &json!({"access": "openai-tok"}));
    }

    #[test]
    fn get_auth_entry_alias_miss() {
        let auth = json!({"other": {"access": "x"}});
        let aliases = vec!["openai", "claude"];
        assert!(get_auth_entry(&auth, &aliases).is_none());
    }
}
