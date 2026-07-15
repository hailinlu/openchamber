//! 扫描 git 仓库中的 SKILL.md 文件 — 对应 Node `scan.js`。
//!
//! 核心逻辑:
//! 1. 用 sparse checkout 克隆仓库到临时目录
//! 2. 递归查找所有 SKILL.md 文件
//! 3. 解析 YAML frontmatter 提取 name / description
//! 4. 返回 SkillsCatalogItem 列表

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::cache;
use super::git::{looks_like_auth_error, DefaultGitRunner, GitRunner};
use super::source::parse_skill_repo_source;

/// 扫描结果中的单个 skill item。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillsCatalogItem {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_id: Option<String>,
    pub repo_source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_subpath: Option<String>,
    pub skill_dir: String,
    pub skill_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frontmatter_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub installable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warnings: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clawdhub: Option<ClawdhubMeta>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installed: Option<InstalledInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClawdhubMeta {
    pub slug: String,
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    pub downloads: i64,
    pub stars: i64,
    pub versions_count: i64,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledInfo {
    pub is_installed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// 扫描结果(成功或带 error)。
#[derive(Debug, Clone, Serialize)]
pub struct ScanResult {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub normalized_repo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effective_subpath: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub items: Option<Vec<SkillsCatalogItem>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ScanError>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScanError {
    pub kind: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssh_only: Option<bool>,
}

/// Skill 名称校验: ^[a-z0-9][a-z0-9-]*[a-z0-9]$|^[a-z0-9]$
pub fn is_valid_skill_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 64 {
        return false;
    }
    let bytes = name.as_bytes();
    let first = bytes[0];
    let last = bytes[bytes.len() - 1];
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    if !(last.is_ascii_lowercase() || last.is_ascii_digit()) {
        return false;
    }
    if bytes.len() == 1 {
        return first.is_ascii_lowercase() || first.is_ascii_digit();
    }
    bytes[1..bytes.len() - 1].iter().all(|&b| {
        b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'
    })
}

/// 扫描一个 git 仓库, 返回发现的 skill items。
///
/// `runner` 参数用于测试时注入 mock。
pub fn scan_skills_repository(
    source: &str,
    subpath: Option<&str>,
    default_subpath: Option<&str>,
    runner: &dyn GitRunner,
) -> ScanResult {
    let parsed = parse_skill_repo_source(source);
    if !parsed.ok {
        return ScanResult {
            ok: false,
            normalized_repo: None,
            effective_subpath: None,
            items: None,
            error: Some(ScanError {
                kind: "invalidSource".to_string(),
                message: parsed
                    .error
                    .map(|e| e.message)
                    .unwrap_or_else(|| "Invalid source".to_string()),
                ssh_only: None,
            }),
        };
    }

    let normalized_repo = parsed.normalized_repo.clone().unwrap_or_default();
    let effective_subpath = subpath
        .or(parsed.effective_subpath.as_deref())
        .or(default_subpath)
        .map(|s| s.to_string());

    // 尝试缓存
    let cache_key = cache::get_cache_key(&normalized_repo, effective_subpath.as_deref(), None);
    if let Some(cached) = cache::get_cached_scan(&cache_key) {
        if let Ok(items) = serde_json::from_value::<Vec<SkillsCatalogItem>>(cached) {
            return ScanResult {
                ok: true,
                normalized_repo: Some(normalized_repo),
                effective_subpath,
                items: Some(items),
                error: None,
            };
        }
    }

    // 确定 clone URL
    let clone_url = parsed
        .clone_url_https
        .or(parsed.clone_url_ssh)
        .unwrap_or_default();

    // 创建临时目录
    let tmp_dir = match std::env::temp_dir().join(format!(
        "sc-skills-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    )) {
        p => {
            let _ = std::fs::remove_dir_all(&p);
            p
        }
    };

    // 浅克隆 (depth 1)
    let clone_result = runner.run(
        &["clone", "--depth", "1", "--filter", "blob:none", "--sparse", &clone_url, tmp_dir.to_str().unwrap_or("/tmp/sc-skills")],
        None,
        Some(120_000),
    );

    if !clone_result.ok {
        let msg = clone_result.message.unwrap_or_default();
        let kind = if looks_like_auth_error(&msg) {
            "authRequired"
        } else if msg.contains("unable to access") || msg.contains("Could not resolve") {
            "networkError"
        } else {
            "unknown"
        };
        let _ = std::fs::remove_dir_all(&tmp_dir);
        return ScanResult {
            ok: false,
            normalized_repo: Some(normalized_repo),
            effective_subpath,
            items: None,
            error: Some(ScanError {
                kind: kind.to_string(),
                message: msg,
                ssh_only: Some(clone_url.starts_with("git@")),
            }),
        };
    }

    // 尝试 sparse checkout subpath
    if let Some(ref sub) = effective_subpath {
        runner.run(
            &[
                "sparse-checkout",
                "set",
                "--no-cone",
                &format!("{}/**", sub),
            ],
            Some(tmp_dir.to_str().unwrap()),
            Some(30_000),
        );
        runner.run(
            &["checkout"],
            Some(tmp_dir.to_str().unwrap()),
            Some(30_000),
        );
    }

    // 递归查找 SKILL.md
    let items = find_skill_md_files(&tmp_dir, &normalized_repo, effective_subpath.as_deref());

    // 写入缓存
    if let Ok(val) = serde_json::to_value(&items) {
        cache::set_cached_scan(&cache_key, val, None);
    }

    // 清理临时目录
    let _ = std::fs::remove_dir_all(&tmp_dir);

    ScanResult {
        ok: true,
        normalized_repo: Some(normalized_repo),
        effective_subpath,
        items: Some(items),
        error: None,
    }
}

/// 递归查找 SKILL.md 文件并解析。
fn find_skill_md_files(
    dir: &Path,
    repo_source: &str,
    subpath: Option<&str>,
) -> Vec<SkillsCatalogItem> {
    let mut items = Vec::new();

    // 读取 SKILL.md 文件列表并查找 skill 子目录
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return items,
    };

    // 如果工具目录本身就是 SKILL.md → 特殊处理
    let self_skill_md = dir.join("SKILL.md");
    if self_skill_md.exists() {
        let skill_dir_name = dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();
        let (frontmatter_name, description) = parse_frontmatter(&self_skill_md);
        items.push(SkillsCatalogItem {
            source_id: None,
            repo_source: repo_source.to_string(),
            repo_subpath: subpath.map(|s| s.to_string()),
            skill_dir: skill_dir_name.clone(),
            skill_name: skill_dir_name,
            frontmatter_name,
            description,
            installable: true,
            warnings: None,
            clawdhub: None,
            installed: None,
        });
        return items;
    }

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let skill_md = path.join("SKILL.md");
        if !skill_md.exists() {
            continue;
        }
        let skill_dir_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();

        let is_installable = is_valid_skill_name(&skill_dir_name);
        let mut warnings = Vec::new();
        if !is_installable {
            warnings.push(format!(
                "Invalid skill directory name '{}'; must match ^[a-z0-9][a-z0-9-]*[a-z0-9]$",
                skill_dir_name
            ));
        }

        let (frontmatter_name, description) = parse_frontmatter(&skill_md);

        items.push(SkillsCatalogItem {
            source_id: None,
            repo_source: repo_source.to_string(),
            repo_subpath: subpath.map(|s| s.to_string()),
            skill_dir: skill_dir_name.clone(),
            skill_name: skill_dir_name,
            frontmatter_name,
            description,
            installable: is_installable,
            warnings: if warnings.is_empty() { None } else { Some(warnings) },
            clawdhub: None,
            installed: None,
        });
    }
    items
}

/// 简单解析 SKILL.md 的 YAML frontmatter (--- 分隔)。
/// 提取 `name` 和 `description` 字段。
pub fn parse_frontmatter(path: &Path) -> (Option<String>, Option<String>) {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return (None, None),
    };

    let mut lines = content.lines();
    // 第一行必须是 ---
    if lines.next() != Some("---") {
        return (None, None);
    }

    let mut frontmatter_lines = Vec::new();
    for line in lines {
        if line == "---" {
            break;
        }
        frontmatter_lines.push(line.to_string());
    }

    let mut name = None;
    let mut description = None;
    for line in &frontmatter_lines {
        if let Some(val) = line.strip_prefix("name:") {
            name = Some(val.trim().to_string());
        } else if let Some(val) = line.strip_prefix("description:") {
            description = Some(val.trim().to_string());
        }
    }

    (name, description)
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_skill_names() {
        assert!(is_valid_skill_name("my-skill"));
        assert!(is_valid_skill_name("a"));
        assert!(is_valid_skill_name("skill1"));
        assert!(is_valid_skill_name("my-skill-v2"));
    }

    #[test]
    fn invalid_skill_names() {
        assert!(!is_valid_skill_name(""));
        assert!(!is_valid_skill_name("-skill"));
        assert!(!is_valid_skill_name("skill-"));
        assert!(!is_valid_skill_name("SKILL"));
        assert!(!is_valid_skill_name("my skill"));
        assert!(!is_valid_skill_name(&"a".repeat(65)));
    }

    #[test]
    fn parse_frontmatter_basic() {
        let dir = std::env::temp_dir().join(format!("fm-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("SKILL.md");
        std::fs::write(&path, "---\nname: test-skill\ndescription: A test\n---\n\nBody here").unwrap();

        let (name, desc) = parse_frontmatter(&path);
        assert_eq!(name, Some("test-skill".to_string()));
        assert_eq!(desc, Some("A test".to_string()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_frontmatter_no_frontmatter() {
        let dir = std::env::temp_dir().join(format!("fm-test2-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("SKILL.md");
        std::fs::write(&path, "Just body text").unwrap();

        let (name, desc) = parse_frontmatter(&path);
        assert!(name.is_none());
        assert!(desc.is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
