//! 工作区路径解析与边界检查。
//!
//! 移植 `packages/web/server/lib/fs/routes.js` 的
//! `resolveWorkspacePath` / `resolveWorkspacePathFromContext` / `isPathWithinRoot`。
//!
//! 职责:
//!   - 将用户输入路径解析为绝对路径
//!   - 验证路径在活动项目目录或用户配置目录内
//!   - 拒绝工作区外的路径 (除非有 grant)

use std::path::{Path, PathBuf};

/// 检查 `resolved` 路径是否在 `root` 内 (或等于 root)。
///
/// 对应 Node `isPathWithinRoot`:
/// `path.relative(resolved, root)` 不以 `..` 开头且不是绝对路径。
pub fn is_path_within_root(resolved: &Path, root: &Path) -> bool {
    // 如果路径相同, 返回 true
    if resolved == root {
        return true;
    }

    // 检查 resolved 是否是 root 的子路径
    // 使用 components 比较避免字符串路径分隔符差异
    let resolved_components: Vec<_> = resolved.components().collect();
    let root_components: Vec<_> = root.components().collect();

    if resolved_components.len() < root_components.len() {
        return false;
    }

    resolved_components[..root_components.len()] == root_components[..]
}

/// 解析工作区路径并验证在边界内。
///
/// 对应 Node `resolveWorkspacePath`。
///
/// 步骤:
///   1. resolve(target_path) against base_directory
///   2. canonicalize (或 lexical normalize 如果文件不存在)
///   3. 检查在 base_directory 内 或 user_config_root 内
pub fn resolve_workspace_path(
    target_path: &str,
    base_directory: &Path,
    user_config_root: Option<&Path>,
) -> Result<PathBuf, oc_core::Error> {
    resolve_with_extra_roots(target_path, base_directory, user_config_root, None)
}

/// 解析 list-only 路径 — 额外允许 home directory。
///
/// 用于 `GET /api/fs/list`: 列出 home 是"添加项目"对话框的合法用例,
/// 暴露目录名 (不带文件内容) 不构成敏感读取。read/write/delete 仍走
/// `resolve_workspace_path`, 不放行 home。
pub fn resolve_list_path(
    target_path: &str,
    base_directory: &Path,
    user_config_root: Option<&Path>,
    home_directory: Option<&Path>,
) -> Result<PathBuf, oc_core::Error> {
    resolve_with_extra_roots(target_path, base_directory, user_config_root, home_directory)
}

fn resolve_with_extra_roots(
    target_path: &str,
    base_directory: &Path,
    user_config_root: Option<&Path>,
    home_directory: Option<&Path>,
) -> Result<PathBuf, oc_core::Error> {
    let trimmed = target_path.trim();
    if trimmed.is_empty() {
        return Err(oc_core::Error::BadRequest("Path is required".into()));
    }

    // 解析为绝对路径
    let target = Path::new(trimmed);
    let resolved = if target.is_absolute() {
        target.to_path_buf()
    } else {
        base_directory.join(target)
    };

    // 尝试 canonicalize (文件存在时), 否则用 lexical normalize
    let canonical = resolved.canonicalize().unwrap_or_else(|_| normalize_path_lexical(&resolved));

    // 检查边界
    if is_path_within_root(&canonical, base_directory) {
        return Ok(canonical);
    }

    if let Some(config_root) = user_config_root {
        if is_path_within_root(&canonical, config_root) {
            return Ok(canonical);
        }
    }

    // List-only: 额外允许 home directory, 让"添加项目"对话框能浏览 ~/Projects 等。
    if let Some(home) = home_directory {
        if is_path_within_root(&canonical, home) {
            return Ok(canonical);
        }
    }

    Err(oc_core::Error::BadRequest(
        "Path is outside of active workspace".into(),
    ))
}

/// 词汇规范化路径 (不触及文件系统)。
///
/// 展开 `.`, `..`, 去除多余的 `/`。
fn normalize_path_lexical(path: &Path) -> PathBuf {
    let mut components = Vec::new();

    for comp in path.components() {
        use std::path::Component;
        match comp {
            Component::CurDir => {} // 跳过 "."
            Component::ParentDir => {
                // 弹出最后一个正常组件 (如果有)
                if let Some(Component::Normal(_)) = components.last() {
                    components.pop();
                } else {
                    components.push(comp);
                }
            }
            c => components.push(c),
        }
    }

    components.iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_workspace(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "openchamber-workspace-{label}-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create temp workspace");
        path
    }

    #[test]
    fn existing_workspace_root_and_child_are_allowed() {
        let root = temp_workspace("root");
        let child = root.join("src");
        std::fs::create_dir_all(&child).expect("create child");

        let canonical_root = root.canonicalize().expect("canonical root");
        assert!(resolve_workspace_path(
            canonical_root.to_str().expect("utf-8 root"),
            &canonical_root,
            None,
        )
        .is_ok());
        assert!(resolve_workspace_path(child.to_str().expect("utf-8 child"), &canonical_root, None).is_ok());

        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn existing_sibling_workspace_is_rejected() {
        let parent = temp_workspace("parent");
        let root = parent.join("active");
        let sibling = parent.join("sibling");
        std::fs::create_dir_all(&root).expect("create active root");
        std::fs::create_dir_all(&sibling).expect("create sibling");

        let result = resolve_workspace_path(
            sibling.to_str().expect("utf-8 sibling"),
            &root.canonicalize().expect("canonical root"),
            None,
        );
        assert!(matches!(result, Err(oc_core::Error::BadRequest(message)) if message == "Path is outside of active workspace"));

        std::fs::remove_dir_all(parent).ok();
    }

    #[test]
    fn within_root_same() {
        let root = Path::new("/home/user/project");
        assert!(is_path_within_root(root, root));
    }

    #[test]
    fn within_root_child() {
        let root = Path::new("/home/user/project");
        let child = Path::new("/home/user/project/src/main.rs");
        assert!(is_path_within_root(child, root));
    }

    #[test]
    fn not_within_root_sibling() {
        let root = Path::new("/home/user/project");
        let sibling = Path::new("/home/user/other-project");
        assert!(!is_path_within_root(sibling, root));
    }

    #[test]
    fn not_within_root_parent() {
        let root = Path::new("/home/user/project");
        let parent = Path::new("/home/user");
        assert!(!is_path_within_root(parent, root));
    }

    #[test]
    fn resolve_absolute_within() {
        let base = Path::new("/home/user/project");
        let result = resolve_workspace_path("/home/user/project/src/main.rs", base, None);
        assert!(result.is_ok());
    }

    #[test]
    fn resolve_absolute_outside() {
        let base = Path::new("/home/user/project");
        let result = resolve_workspace_path("/etc/passwd", base, None);
        assert!(result.is_err());
    }

    #[test]
    fn resolve_relative() {
        let base = Path::new("/home/user/project");
        let result = resolve_workspace_path("src/main.rs", base, None);
        // 文件不存在时使用 lexical normalize
        assert!(result.is_ok());
    }

    #[test]
    fn resolve_empty_fails() {
        let base = Path::new("/home/user/project");
        let result = resolve_workspace_path("", base, None);
        assert!(result.is_err());
    }

    #[test]
    fn resolve_user_config_root() {
        // 使用真实临时目录 + canonicalize: 硬编码 Unix 路径在 Windows 上会被当成盘符根相对路径,
        // 且 canonicalize() 返回 `\\?\` 前缀路径,与 lexical config_root 的 components 无法匹配。
        let config_root = temp_workspace("config-root");
        let settings = config_root.join("settings.json");
        std::fs::write(&settings, "{}").expect("write settings");

        let canonical_config_root = config_root.canonicalize().expect("canonical config root");
        let base = temp_workspace("base");

        let result = resolve_workspace_path(
            settings.to_str().expect("utf-8 settings path"),
            &base,
            Some(&canonical_config_root),
        );
        assert!(result.is_ok());

        std::fs::remove_dir_all(&config_root).ok();
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn normalize_lexical_dotdot() {
        let p = Path::new("/home/user/project/../other/file.txt");
        let normalized = normalize_path_lexical(p);
        assert_eq!(normalized, PathBuf::from("/home/user/other/file.txt"));
    }
}
