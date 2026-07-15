//! 仓库上下文工厂。
//!
//! 移植自 Node `createRepositoryGitContext` / `resolveGitRepositoryRoot` / `resolveGitFileContext`。
//! 提供仓库根目录解析和文件路径安全检查。

use std::path::{Path, PathBuf};

use crate::git::paths::{is_inside_or_same_directory, normalize_directory_path};
use crate::git::runner::GitRunner;

/// 仓库上下文, 与 Node `createRepositoryGitContext` 返回值对齐。
#[derive(Debug, Clone)]
pub struct RepoContext {
    /// normalizeDirectoryPath(directory) — 调用者传入的目录 (normalized)。
    pub directory_path: PathBuf,
    /// git rev-parse --show-toplevel — 仓库根目录。
    pub repo_root: PathBuf,
}

impl RepoContext {
    /// 创建仓库上下文: 解析仓库根目录。
    ///
    /// 与 Node `createRepositoryGitContext` 对齐。
    pub async fn create(directory: &str) -> oc_core::Result<Self> {
        let dir_str = normalize_directory_path(directory);
        if dir_str.is_empty() {
            return Err(oc_core::Error::BadRequest(
                "directory parameter is required".to_string(),
            ));
        }

        let directory_path = PathBuf::from(&dir_str);

        // git rev-parse --show-toplevel
        let result = GitRunner::run(&directory_path, &["rev-parse", "--show-toplevel"]).await;

        if !result.success {
            let msg = result.stderr_text();
            return Err(oc_core::Error::Internal(format!(
                "failed to resolve repository root: {msg}"
            )));
        }

        let root = result.stdout_text();
        if root.is_empty() {
            return Err(oc_core::Error::Internal(
                "failed to resolve repository root: empty output".to_string(),
            ));
        }

        Ok(Self {
            directory_path,
            repo_root: PathBuf::from(&root),
        })
    }
}

/// 文件上下文, 与 Node `resolveGitFileContext` 对齐。
#[derive(Debug, Clone)]
pub struct GitFileContext {
    /// 文件在文件系统中的绝对路径。
    pub absolute_path: PathBuf,
    /// 文件在仓库中的相对路径 (repo-relative)。
    pub repo_path: String,
    /// 仓库根目录。
    pub repo_root: PathBuf,
}

impl GitFileContext {
    /// 解析文件路径, 确保在仓库内。
    ///
    /// 与 Node `resolveGitFileContext` 对齐。
    pub fn resolve(repo_ctx: &RepoContext, file_path: &str) -> oc_core::Result<Self> {
        let trimmed = file_path.trim();
        if trimmed.is_empty() {
            return Err(oc_core::Error::BadRequest(
                "file path is required".to_string(),
            ));
        }

        // 解析为绝对路径
        let absolute = if Path::new(trimmed).is_absolute() {
            PathBuf::from(trimmed)
        } else {
            repo_ctx.repo_root.join(trimmed)
        };

        // 规范化 (词法)
        let absolute_path = lexical_normalize(&absolute);

        // 检查是否在仓库内
        if !is_inside_or_same_directory(&absolute_path, &repo_ctx.repo_root) {
            return Err(oc_core::Error::BadRequest(
                "Path is outside repository".to_string(),
            ));
        }

        // 计算 repo-relative 路径
        let repo_path = path_relative_to(&absolute_path, &repo_ctx.repo_root);

        Ok(Self {
            absolute_path,
            repo_path,
            repo_root: repo_ctx.repo_root.clone(),
        })
    }
}

/// 词法 normalize: 去除 `.` 和 `..` (不碰文件系统)。
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut components: Vec<String> = Vec::new();
    let path_str = path.to_string_lossy();
    let is_absolute = path_str.starts_with('/');

    for component in path_str.split('/') {
        match component {
            "" | "." => continue,
            ".." => {
                components.pop();
            }
            other => components.push(other.to_string()),
        }
    }

    let joined = components.join("/");
    PathBuf::from(if is_absolute {
        format!("/{joined}")
    } else {
        joined
    })
}

/// 计算 `path` 相对于 `base` 的路径。
fn path_relative_to(path: &Path, base: &Path) -> String {
    let path_str = path.to_string_lossy();
    let base_str = base.to_string_lossy();

    let path_normalized = path_str.trim_end_matches('/');
    let base_normalized = base_str.trim_end_matches('/');

    if path_normalized == base_normalized {
        return String::new();
    }

    // 如果 path 以 base/ 开头, 去掉 base/ 前缀
    let prefix = format!("{base_normalized}/");
    if let Some(rest) = path_normalized.strip_prefix(&prefix) {
        rest.to_string()
    } else {
        path_normalized.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lexical_normalize() {
        assert_eq!(
            lexical_normalize(Path::new("/home/user/./proj/../proj/src")).to_string_lossy(),
            "/home/user/proj/src"
        );
        assert_eq!(
            lexical_normalize(Path::new("/home/user/../../etc")).to_string_lossy(),
            "/etc"
        );
    }

    #[test]
    fn test_path_relative_to() {
        assert_eq!(
            path_relative_to(
                Path::new("/home/user/proj/src/main.rs"),
                Path::new("/home/user/proj")
            ),
            "src/main.rs"
        );
        assert_eq!(
            path_relative_to(Path::new("/home/user/proj"), Path::new("/home/user/proj")),
            ""
        );
    }
}
