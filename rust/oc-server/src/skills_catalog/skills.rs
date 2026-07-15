//! 本地 skill 文件系统操作 — 对应 Node `skills.js`。
//!
//! 包括 discovery / create / update / delete 以及 supporting file 管理。

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::{ApiError, ApiResult};

/// 已发现的 skill 信息。
#[derive(Debug, Clone, Serialize)]
pub struct DiscoveredSkill {
    pub name: String,
    pub path: String,
    pub scope: String,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Skill sources 对象 — 对应 `getSkillSources` 返回形状。
#[derive(Debug, Clone, Serialize)]
pub struct SkillSources {
    pub md: SkillSourceEntry,
    pub project_md: SkillSourceEntry,
    pub claude_md: SkillSourceEntry,
    pub user_md: SkillSourceEntry,
    pub user_claude_md: SkillSourceEntry,
    pub user_agents_md: SkillSourceEntry,
}

#[derive(Debug, Clone, Serialize)]
pub struct SkillSourceEntry {
    pub exists: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supporting_files: Option<Vec<SupportingFile>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SupportingFile {
    pub name: String,
    pub path: String,
    pub full_path: String,
}

// ---------------------------------------------------------------------------
// 目录解析 helper
// ---------------------------------------------------------------------------

fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
}

fn user_opencode_skills_dir() -> PathBuf {
    home_dir().join(".config").join("opencode").join("skills")
}

fn user_agents_skills_dir() -> PathBuf {
    home_dir().join(".agents").join("skills")
}

fn user_claude_skills_dir() -> PathBuf {
    home_dir().join(".claude").join("skills")
}

fn project_opencode_skills_dir(working_dir: &Path) -> PathBuf {
    working_dir.join(".opencode").join("skills")
}

fn project_agents_skills_dir(working_dir: &Path) -> PathBuf {
    working_dir.join(".agents").join("skills")
}

fn project_claude_skills_dir(working_dir: &Path) -> PathBuf {
    working_dir.join(".claude").join("skills")
}

/// 获取 scope, source, 对应 skill 目录列表 (按优先级)。
fn search_paths(working_directory: Option<&str>) -> Vec<(PathBuf, &'static str, &'static str)> {
    let mut paths: Vec<(PathBuf, &str, &str)> = Vec::new();
    if let Some(wd) = working_directory {
        let work_dir = Path::new(wd);
        paths.push((
            project_opencode_skills_dir(work_dir),
            "project",
            "opencode",
        ));
        paths.push((project_claude_skills_dir(work_dir), "project", "claude"));
        paths.push((project_agents_skills_dir(work_dir), "project", "agents"));
    }
    paths.push((user_opencode_skills_dir(), "user", "opencode"));
    paths.push((user_claude_skills_dir(), "user", "claude"));
    paths.push((user_agents_skills_dir(), "user", "agents"));
    paths
}

// ---------------------------------------------------------------------------
// 发现
// ---------------------------------------------------------------------------

/// 发现本地文件系统中的所有 skill。
pub fn discover_skills(working_directory: Option<&str>) -> Vec<DiscoveredSkill> {
    let mut skills: Vec<DiscoveredSkill> = Vec::new();
    let mut seen_names = std::collections::HashSet::new();

    for (base_dir, scope, source) in search_paths(working_directory) {
        if !base_dir.exists() {
            continue;
        }
        if let Ok(entries) = std::fs::read_dir(&base_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                let name = match path.file_name().and_then(|n| n.to_str()) {
                    Some(n) => n.to_string(),
                    None => continue,
                };
                if seen_names.contains(&name) {
                    continue;
                }
                let description = parse_description(&path);
                seen_names.insert(name.clone());
                skills.push(DiscoveredSkill {
                    name,
                    path: path.to_string_lossy().to_string(),
                    scope: scope.to_string(),
                    source: source.to_string(),
                    description,
                });
            }
        }
    }
    skills
}

/// 解析 SKILL.md 中的 description。
fn parse_description(skill_dir: &Path) -> Option<String> {
    let md_path = skill_dir.join("SKILL.md");
    if !md_path.exists() {
        return None;
    }
    let content = std::fs::read_to_string(md_path).ok()?;
    let mut lines = content.lines();
    if lines.next()? != "---" {
        return None;
    }
    for line in lines {
        if line == "---" {
            break;
        }
        if let Some(val) = line.strip_prefix("description:") {
            return Some(val.trim().to_string());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Skill sources 查询
// ---------------------------------------------------------------------------

/// 获取某 skill 的 sources 信息。
pub fn get_skill_sources(skill_name: &str, working_directory: Option<&str>) -> SkillSources {
    let paths = search_paths(working_directory);

    // 按优先级查找第一个存在的
    let mut md_entry = SkillSourceEntry::empty();
    for (base_dir, scope, source) in &paths {
        let skill_dir = base_dir.join(skill_name);
        let skill_md = skill_dir.join("SKILL.md");
        if skill_md.exists() {
            md_entry = make_entry(&skill_dir, &skill_md, scope, source, skill_name);
            break;
        }
    }

    // 按 scope/source 分别查询 (可能重复查找, 但简单明了)
    let project_md = find_entry(skill_name, &paths, "project", "opencode");
    let claude_md = find_entry(skill_name, &paths, "project", "claude");
    let user_md = find_entry(skill_name, &paths, "user", "opencode");
    let user_claude_md = find_entry(skill_name, &paths, "user", "claude");
    let user_agents_md = find_entry(skill_name, &paths, "user", "agents");

    SkillSources {
        md: md_entry,
        project_md,
        claude_md,
        user_md,
        user_claude_md,
        user_agents_md,
    }
}

fn find_entry(
    skill_name: &str,
    paths: &[(PathBuf, &'static str, &'static str)],
    scope: &str,
    source: &str,
) -> SkillSourceEntry {
    for (base_dir, s, src) in paths {
        if *s != scope || *src != source {
            continue;
        }
        let skill_dir = base_dir.join(skill_name);
        let skill_md = skill_dir.join("SKILL.md");
        if skill_md.exists() {
            return make_entry(&skill_dir, &skill_md, *s, *src, skill_name);
        }
    }
    SkillSourceEntry::empty()
}

fn make_entry(
    skill_dir: &Path,
    skill_md: &Path,
    scope: &str,
    source: &str,
    skill_name: &str,
) -> SkillSourceEntry {
    SkillSourceEntry {
        exists: true,
        path: Some(skill_md.to_string_lossy().to_string()),
        dir: Some(skill_dir.to_string_lossy().to_string()),
        scope: Some(scope.to_string()),
        source: Some(source.to_string()),
        name: Some(skill_name.to_string()),
        description: parse_description(skill_dir),
        instructions: read_body(skill_md),
        supporting_files: Some(list_supporting_files(skill_dir)),
    }
}

impl SkillSourceEntry {
    fn empty() -> Self {
        SkillSourceEntry {
            exists: false,
            path: None,
            dir: None,
            scope: None,
            source: None,
            name: None,
            description: None,
            instructions: None,
            supporting_files: None,
        }
    }
}

// ---------------------------------------------------------------------------
// SKILL.md body / supporting files
// ---------------------------------------------------------------------------

/// Read the body (everything after frontmatter) of a SKILL.md.
fn read_body(path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let mut lines = content.lines();
    // 第一行必须是 ---
    if lines.next()? != "---" {
        return Some(content);
    }
    // 跳过 frontmatter 直到 ---
    let mut body_parts: Vec<&str> = Vec::new();
    let mut in_frontmatter = true;
    for line in lines {
        if in_frontmatter {
            if line == "---" {
                in_frontmatter = false;
            }
            continue;
        }
        body_parts.push(line);
    }
    if body_parts.is_empty() {
        return None;
    }
    Some(body_parts.join("\n"))
}

/// List supporting files (除 SKILL.md 外) 在 skill 目录中。
fn list_supporting_files(skill_dir: &Path) -> Vec<SupportingFile> {
    let mut files = Vec::new();
    if !skill_dir.exists() {
        return files;
    }
    if let Ok(entries) = std::fs::read_dir(skill_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                // 递归一层
                if let Ok(sub_entries) = std::fs::read_dir(&path) {
                    for sub in sub_entries.flatten() {
                        let sub_path = sub.path();
                        if sub_path.is_file() {
                            let name = sub_path
                                .file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or("")
                                .to_string();
                            let dir_name = path
                                .file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or("");
                            files.push(SupportingFile {
                                name: name.clone(),
                                path: format!("{}/{}", dir_name, name),
                                full_path: sub_path.to_string_lossy().to_string(),
                            });
                        }
                    }
                }
            } else if path.is_file() {
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_string();
                if name == "SKILL.md" {
                    continue;
                }
                files.push(SupportingFile {
                    name,
                    path: path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("")
                        .to_string(),
                    full_path: path.to_string_lossy().to_string(),
                });
            }
        }
    }
    files
}

// ---------------------------------------------------------------------------
// Supporting file CRUD (带 path traversal 防护)
// ---------------------------------------------------------------------------

/// 安全地读 supporting file。
pub fn read_skill_supporting_file(skill_dir: &str, relative_path: &str) -> ApiResult<String> {
    check_path(relative_path)?;
    let resolved = std::path::Path::new(skill_dir).join(relative_path);
    if !resolved.exists() {
        return Err(ApiError(oc_core::Error::NotFound(
            "File not found".into(),
        )));
    }
    std::fs::read_to_string(&resolved)
        .map_err(|e| ApiError(oc_core::Error::Internal(format!("read error: {}", e))))
}

/// 安全地写 supporting file。
pub fn write_skill_supporting_file(
    skill_dir: &str,
    relative_path: &str,
    content: &str,
) -> ApiResult<()> {
    check_path(relative_path)?;
    let resolved = std::path::Path::new(skill_dir).join(relative_path);
    if let Some(parent) = resolved.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| ApiError(oc_core::Error::Internal(format!("mkdir error: {}", e))))?;
    }
    std::fs::write(&resolved, content)
        .map_err(|e| ApiError(oc_core::Error::Internal(format!("write error: {}", e))))
}

/// 安全地删 supporting file。
pub fn delete_skill_supporting_file(skill_dir: &str, relative_path: &str) -> ApiResult<()> {
    check_path(relative_path)?;
    let resolved = std::path::Path::new(skill_dir).join(relative_path);
    if !resolved.exists() {
        return Err(ApiError(oc_core::Error::NotFound(
            "File not found".into(),
        )));
    }
    std::fs::remove_file(&resolved)
        .map_err(|e| ApiError(oc_core::Error::Internal(format!("delete error: {}", e))))
}

fn check_path(relative_path: &str) -> ApiResult<()> {
    if is_unsafe_relative_path(relative_path) {
        return Err(ApiError(oc_core::Error::BadRequest(
            "Invalid file path".into(),
        )));
    }
    Ok(())
}

/// 判断相对路径是否不安全。
fn is_unsafe_relative_path(path: &str) -> bool {
    path.contains("..")
        || path.starts_with('/')
        || path.contains('\0')
        || path.contains('~')
}

// ---------------------------------------------------------------------------
// SKILL.md 生成
// ---------------------------------------------------------------------------

/// 生成 SKILL.md 文件内容。
pub fn build_skill_md_content(name: &str, description: Option<&str>, instructions: &str) -> String {
    let mut frontmatter = format!("---\nname: {}\n", name);
    if let Some(desc) = description {
        frontmatter.push_str(&format!("description: {}\n", desc));
    }
    frontmatter.push_str("---\n\n");
    frontmatter.push_str(instructions);
    frontmatter
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_is_unsafe_path() {
        assert!(is_unsafe_relative_path("../foo"));
        assert!(is_unsafe_relative_path("/etc/passwd"));
        assert!(is_unsafe_relative_path("foo/../../bar"));
        assert!(is_unsafe_relative_path("~/skills"));
        assert!(!is_unsafe_relative_path("valid-file.txt"));
        assert!(!is_unsafe_relative_path("subdir/file.txt"));
    }

    #[test]
    fn test_skill_md_content() {
        let content = build_skill_md_content("test-skill", Some("A test"), "Do something");
        assert!(content.starts_with("---\nname: test-skill"));
        assert!(content.contains("description: A test"));
        assert!(content.ends_with("Do something"));
    }

    #[test]
    fn test_discover_skills_empty() {
        let skills = discover_skills(None);
        // Should not crash; may find nothing
        assert!(skills.is_empty() || skills.iter().any(|s| s.scope == "user"));
    }

    #[test]
    fn test_read_body_no_frontmatter() {
        let dir = std::env::temp_dir().join(format!("sbt-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let md_path = dir.join("SKILL.md");
        fs::write(&md_path, "Just body text").unwrap();
        assert_eq!(read_body(&md_path), Some("Just body text".to_string()));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_read_body_with_frontmatter() {
        let dir = std::env::temp_dir().join(format!("sbt2-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let md_path = dir.join("SKILL.md");
        fs::write(&md_path, "---\nname: test\ndescription: test\n---\n\nInstructions here").unwrap();
        assert_eq!(read_body(&md_path), Some("\nInstructions here".to_string()));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_list_supporting_files() {
        let dir = std::env::temp_dir().join(format!("sfs-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        fs::write(dir.join("SKILL.md"), "test").unwrap();
        fs::write(dir.join("config.json"), "{}").unwrap();
        fs::write(dir.join("prompt.txt"), "hello").unwrap();

        let files = list_supporting_files(&dir);
        assert_eq!(files.len(), 2);
        assert!(files.iter().any(|f| f.name == "config.json"));
        assert!(files.iter().any(|f| f.name == "prompt.txt"));
        assert!(!files.iter().any(|f| f.name == "SKILL.md"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_get_skill_sources_no_skill() {
        let sources = get_skill_sources("nonexistent-skill", None);
        assert!(!sources.md.exists);
    }

    #[test]
    fn test_build_skill_md_without_description() {
        let content = build_skill_md_content("bare", None, "Do it");
        assert!(!content.contains("description:"));
        assert!(content.contains("name: bare"));
    }
}
