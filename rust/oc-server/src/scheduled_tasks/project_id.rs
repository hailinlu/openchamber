//! Project path → stable ID — 移植自 Node `projects/project-id.js`。
//!
//! 把 project 路径转换为 `path_<base64url>` 形式的稳定 ID。
//! 供 opencode routes/settings-runtime 用于从 project 路径推导 projectId。

use base64::Engine;

/// 归一化 project 路径: `\` → `/`, 去尾 `/`。
fn normalize_project_path_for_id(value: &str) -> String {
    let normalized = value.replace('\\', "/");
    let trimmed = normalized.trim_end_matches('/');
    trimmed.to_string()
}

/// 从 project 路径生成稳定 ID: `path_<base64url(utf8-normalized-path)>`。
///
/// 空/空白路径 → 空字符串。
pub fn create_project_id_from_path(project_path: &str) -> String {
    let normalized = normalize_project_path_for_id(project_path.trim());
    if normalized.is_empty() {
        return String::new();
    }
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(normalized.as_bytes());
    format!("path_{encoded}")
}

// =========================================================================
// 测试
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_path() {
        let id = create_project_id_from_path("/home/user/myproject");
        assert!(id.starts_with("path_"));
        assert!(id.len() > 5);
    }

    #[test]
    fn test_empty_path() {
        assert_eq!(create_project_id_from_path(""), "");
        assert_eq!(create_project_id_from_path("   "), "");
    }

    #[test]
    fn test_backslash_normalized() {
        let win = create_project_id_from_path("C:\\Users\\dev\\project");
        let expected_content = "C:/Users/dev/project";
        let expected = format!(
            "path_{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(expected_content.as_bytes())
        );
        assert_eq!(win, expected);
    }

    #[test]
    fn test_trailing_slash_stripped() {
        let with_slash = create_project_id_from_path("/home/user/myproject/");
        let without_slash = create_project_id_from_path("/home/user/myproject");
        assert_eq!(with_slash, without_slash);
    }

    #[test]
    fn test_deterministic() {
        let path = "/var/www/app";
        let id1 = create_project_id_from_path(path);
        let id2 = create_project_id_from_path(path);
        assert_eq!(id1, id2);
    }
}
