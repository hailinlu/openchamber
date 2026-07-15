//! OpenCode 配置层 + markdown 文件 + worktree + prompts + skills 全套。
//!
//! 对应 Node `opencode/shared.js` (536 行) 的 1:1 移植。
//!
//! 模块结构(行号对齐 Node 源):
//!   1. PATH CONSTANTS              (paths.rs 中)
//!   2. SCOPE TYPE CONSTANTS
//!   3. DIRECTORY OPERATIONS
//!   4. MARKDOWN FILE OPERATIONS
//!   5. CONFIG FILE OPERATIONS

#![allow(dead_code)]
#![allow(unused_imports)]

//!   6. GIT/WORKTREE HELPERS
//!   7. PROMPT FILE HELPERS
//!   8. SKILL FILE OPERATIONS

use std::path::{Path, PathBuf};

use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Value};

use crate::git::paths::home_dir;

// ============================================================================
// 1. PATH CONSTANTS(从 paths.rs re-export,保持 Node 命名)
// ============================================================================

pub use crate::opencode::paths::{
    opencode_config_dir as OPENCODE_CONFIG_DIR,
    agent_dir as AGENT_DIR,
    command_dir as COMMAND_DIR,
    skill_dir as SKILL_DIR,
    config_file as CONFIG_FILE,
    custom_config_file as CUSTOM_CONFIG_FILE,
};

/// Prompt 文件引用模式:`{file:path}`(大小写不敏感,对齐 Node line 17)。
static PROMPT_FILE_PATTERN: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)^\{file:(.+)\}$").expect("PROMPT_FILE_PATTERN regex"));

// ============================================================================
// 2. SCOPE TYPE CONSTANTS(对齐 Node line 21-34)
// ============================================================================

/// Agent 来源作用域。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentScope {
    User,
    Project,
}

impl AgentScope {
    pub fn as_str(&self) -> &'static str {
        match self {
            AgentScope::User => "user",
            AgentScope::Project => "project",
        }
    }
}

/// Command 来源作用域。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandScope {
    User,
    Project,
}

impl CommandScope {
    pub fn as_str(&self) -> &'static str {
        match self {
            CommandScope::User => "user",
            CommandScope::Project => "project",
        }
    }
}

/// Skill 来源作用域。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillScope {
    User,
    Project,
}

impl SkillScope {
    pub fn as_str(&self) -> &'static str {
        match self {
            SkillScope::User => "user",
            SkillScope::Project => "project",
        }
    }
}

// ============================================================================
// 3. DIRECTORY OPERATIONS(对齐 Node line 38-51)
// ============================================================================

/// 确保 OpenCode config dir + agents/commands/skills 子目录存在。
pub fn ensure_dirs() -> std::io::Result<()> {
    for dir in [OPENCODE_CONFIG_DIR(), AGENT_DIR(), COMMAND_DIR(), SKILL_DIR()] {
        std::fs::create_dir_all(&dir)?;
    }
    Ok(())
}

// ============================================================================
// 4. MARKDOWN FILE OPERATIONS(对齐 Node line 55-88)
// ============================================================================

/// 解析后的 markdown 文件(frontmatter + body)。
#[derive(Debug, Clone)]
pub struct MdFile {
    pub frontmatter: Value,
    pub body: String,
}

/// 解析 markdown 文件的 YAML frontmatter。
///
/// 对应 Node `parseMdFile` (`shared.js` line 55-73)。
pub fn parse_md_file(path: &Path) -> Result<MdFile, std::io::Error> {
    let content = std::fs::read_to_string(path)?;
    let re = Regex::new(r"^---\r?\n([\s\S]*?)\r?\n---\r?\n([\s\S]*)$").unwrap();
    let caps = match re.captures(&content) {
        Some(c) => c,
        None => {
            return Ok(MdFile {
                frontmatter: Value::Object(Default::default()),
                body: content.trim().to_string(),
            });
        }
    };
    let yaml_str = caps.get(1).map(|m| m.as_str()).unwrap_or("");
    let body = caps.get(2).map(|m| m.as_str()).unwrap_or("").trim().to_string();

    let frontmatter: Value =
        serde_yaml::from_str(yaml_str).unwrap_or_else(|_| Value::Object(Default::default()));

    Ok(MdFile { frontmatter, body })
}

/// 写 markdown 文件(frontmatter 序列化为 YAML,body 跟在 `---` 后)。
///
/// 对应 Node `writeMdFile` (`shared.js` line 75-88)。
pub fn write_md_file(path: &Path, frontmatter: &Value, body: &str) -> Result<(), std::io::Error> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // 过滤 null values
    let mut cleaned = serde_yaml::Mapping::new();
    if let Some(obj) = frontmatter.as_object() {
        for (k, v) in obj {
            if !v.is_null() {
                cleaned.insert(
                    serde_yaml::Value::String(k.clone()),
                    serde_yaml::to_value(v).unwrap_or(serde_yaml::Value::Null),
                );
            }
        }
    }
    let yaml_str =
        serde_yaml::to_string(&serde_yaml::Value::Mapping(cleaned)).unwrap_or_else(|_| "{}".to_string());
    let content = format!("---\n{}---\n\n{}", yaml_str, body);
    std::fs::write(path, content)?;
    Ok(())
}

// ============================================================================
// 5. CONFIG FILE OPERATIONS(对齐 Node line 92-228)
// ============================================================================

/// 项目 config 候选路径(4 个)。
///
/// 对应 Node `getProjectConfigCandidates` (`shared.js` line 92-100)。
pub fn get_project_config_candidates(working_directory: Option<&str>) -> Vec<PathBuf> {
    let Some(wd) = working_directory else { return vec![] };
    vec![
        PathBuf::from(wd).join("opencode.json"),
        PathBuf::from(wd).join("opencode.jsonc"),
        PathBuf::from(wd).join(".opencode").join("opencode.json"),
        PathBuf::from(wd).join(".opencode").join("opencode.jsonc"),
    ]
}

/// 取第一个存在的项目 config;若都不存在,返回 `candidates[0]`(对齐 Node line 102-114)。
pub fn get_project_config_path(working_directory: Option<&str>) -> Option<PathBuf> {
    let candidates = get_project_config_candidates(working_directory);
    for c in &candidates {
        if c.exists() {
            return Some(c.clone());
        }
    }
    candidates.into_iter().next()
}

/// 收集 user/project/custom 三层 config 路径。
///
/// 对应 Node `getConfigPaths` (`shared.js` line 116-126)。
pub fn get_config_paths(working_directory: Option<&str>) -> ConfigPaths {
    let user_paths = vec![
        OPENCODE_CONFIG_DIR().join("config.json"),
        OPENCODE_CONFIG_DIR().join("opencode.json"),
        OPENCODE_CONFIG_DIR().join("opencode.jsonc"),
    ];
    let project_path = get_project_config_path(working_directory);
    let custom_path = CUSTOM_CONFIG_FILE();
    ConfigPaths { user_paths, project_path, custom_path }
}

/// 三层 config 路径。
#[derive(Debug, Clone)]
pub struct ConfigPaths {
    pub user_paths: Vec<PathBuf>,
    pub project_path: Option<PathBuf>,
    pub custom_path: Option<PathBuf>,
}

/// 取第一个存在的 user config 路径;否则返回 `config.json`(对齐 Node line 128-136)。
pub fn get_primary_user_config_path(user_paths: &[PathBuf]) -> PathBuf {
    for p in user_paths {
        if p.exists() {
            return p.clone();
        }
    }
    CONFIG_FILE()
}

/// config 读取错误。
#[derive(Debug)]
pub enum ConfigError {
    Read(String),
    Write(String),
    Parse(String),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Read(m) => write!(f, "failed to read OpenCode configuration: {}", m),
            ConfigError::Write(m) => write!(f, "failed to write OpenCode configuration: {}", m),
            ConfigError::Parse(m) => write!(f, "failed to parse OpenCode configuration: {}", m),
        }
    }
}
impl std::error::Error for ConfigError {}

/// 读 JSONC 文件。ENOENT → 空对象;解析失败 → 错误。
///
/// 对应 Node `readConfigFile` (`shared.js` line 138-153)。
pub fn read_config_file(path: &Path) -> Result<Value, ConfigError> {
    if !path.exists() {
        return Ok(json!({}));
    }
    let content = std::fs::read_to_string(path)
        .map_err(|e| ConfigError::Read(format!("{}: {}", path.display(), e)))?;
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Ok(json!({}));
    }
    // ParseOptions default 已经允许 trailing commas + comments
    let options = jsonc_parser::ParseOptions::default();
    let value: Option<Value> =
        jsonc_parser::parse_to_serde_value(trimmed, &options).map_err(|e| {
            ConfigError::Parse(format!("{}: {}", path.display(), e))
        })?;
    Ok(value.unwrap_or_else(|| json!({})))
}

/// 是否 plain object(非 array,非 null)。
pub fn is_plain_object(value: &Value) -> bool {
    value.is_object()
}

/// 递归合并 config(浅数组覆盖,null 覆盖)。
///
/// 对应 Node `mergeConfigs` (`shared.js` line 159-177)。
pub fn merge_configs(base: &Value, override_value: &Value) -> Value {
    if !is_plain_object(base) || !is_plain_object(override_value) {
        return override_value.clone();
    }
    let mut result = base.clone();
    if let (Some(base_obj), Some(override_obj)) =
        (result.as_object_mut(), override_value.as_object())
    {
        for (k, v) in override_obj {
            if let Some(base_v) = base_obj.get(k) {
                if is_plain_object(base_v) && is_plain_object(v) {
                    base_obj.insert(k.clone(), merge_configs(base_v, v));
                    continue;
                }
            }
            base_obj.insert(k.clone(), v.clone());
        }
    }
    result
}

/// 三层 config 的合并结果。
#[derive(Debug, Clone)]
pub struct ConfigLayers {
    pub user_config: Value,
    pub project_config: Value,
    pub custom_config: Value,
    pub merged_config: Value,
    pub paths: ConfigPathsInternal,
}

/// `ConfigPaths` 加上 `user_path`(已选定的)便于 `get_config_for_path` 查表。
#[derive(Debug, Clone)]
pub struct ConfigPathsInternal {
    pub user_path: PathBuf,
    pub project_path: Option<PathBuf>,
    pub custom_path: Option<PathBuf>,
}

/// 读取并合并 user → project → custom 三层 config。
///
/// 对应 Node `readConfigLayers` (`shared.js` line 179-194)。
pub fn read_config_layers(working_directory: Option<&str>) -> Result<ConfigLayers, ConfigError> {
    let paths = get_config_paths(working_directory);
    let user_path = get_primary_user_config_path(&paths.user_paths);
    let user_config = read_config_file(&user_path)?;
    let project_config = match &paths.project_path {
        Some(p) => read_config_file(p)?,
        None => json!({}),
    };
    let custom_config = match &paths.custom_path {
        Some(p) => read_config_file(p)?,
        None => json!({}),
    };
    let merged_config =
        merge_configs(&merge_configs(&user_config, &project_config), &custom_config);
    Ok(ConfigLayers {
        user_config,
        project_config,
        custom_config,
        merged_config,
        paths: ConfigPathsInternal {
            user_path,
            project_path: paths.project_path,
            custom_path: paths.custom_path,
        },
    })
}

/// 直接读 merged config(短手版)。
///
/// 对应 Node `readConfig` (`shared.js` line 196-198)。
pub fn read_config(working_directory: Option<&str>) -> Result<Value, ConfigError> {
    Ok(read_config_layers(working_directory)?.merged_config)
}

/// 根据 path 查 layers 内对应的 sub-config。
///
/// 对应 Node `getConfigForPath` (`shared.js` line 200-211)。
pub fn get_config_for_path<'a>(
    layers: &'a ConfigLayers,
    target_path: Option<&Path>,
) -> &'a Value {
    let Some(t) = target_path else { return &layers.user_config };
    if let Some(c) = &layers.paths.custom_path {
        if t == c.as_path() {
            return &layers.custom_config;
        }
    }
    if let Some(p) = &layers.paths.project_path {
        if t == p.as_path() {
            return &layers.project_config;
        }
    }
    &layers.user_config
}

/// 原子写 config: 备份 + `.tmp → rename`(对齐 Node `writeConfig` line 213-228 + Rust 强化)。
pub fn write_config(config: &Value, file_path: &Path) -> Result<(), ConfigError> {
    if let Some(parent) = file_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| ConfigError::Write(format!("create_dir_all: {}", e)))?;
    }
    if file_path.exists() {
        let backup = format!("{}.openchamber.backup", file_path.display());
        std::fs::copy(file_path, &backup)
            .map_err(|e| ConfigError::Write(format!("backup to {}: {}", backup, e)))?;
    }
    let content = serde_json::to_string_pretty(config)
        .map_err(|e| ConfigError::Write(format!("serialize: {}", e)))?;
    let tmp_path = format!(
        "{}.tmp-{}-{}-{}",
        file_path.display(),
        std::process::id(),
        chrono::Utc::now().timestamp_millis(),
        rand::random::<u32>(),
    );
    let tmp = PathBuf::from(&tmp_path);
    if let Err(e) = std::fs::write(&tmp, &content) {
        let _ = std::fs::remove_file(&tmp);
        return Err(ConfigError::Write(format!("write tmp: {}", e)));
    }
    if let Err(e) = std::fs::rename(&tmp, file_path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(ConfigError::Write(format!("rename: {}", e)));
    }
    Ok(())
}

// ============================================================================
// 6. GIT/WORKTREE HELPERS(对齐 Node line 248-273)
// ============================================================================

/// 取 `start_dir` 的祖先路径直到 `stop_dir`(不含 stop_dir 之后)。
///
/// 对应 Node `getAncestors` (`shared.js` line 248-256)。
pub fn get_ancestors(start_dir: &Path, stop_dir: Option<&Path>) -> Vec<PathBuf> {
    let mut result = vec![];
    let mut current = start_dir.to_path_buf();
    loop {
        result.push(current.clone());
        if Some(current.as_path()) == stop_dir {
            break;
        }
        match current.parent() {
            Some(p) if !p.as_os_str().is_empty() => current = p.to_path_buf(),
            _ => break,
        }
    }
    result
}

/// 找 `.git` 所在目录(向上遍历)。
///
/// 对应 Node `findWorktreeRoot` (`shared.js` line 258-273)。
pub fn find_worktree_root(start_dir: &Path) -> Option<PathBuf> {
    let mut current = start_dir.to_path_buf();
    loop {
        if current.join(".git").exists() {
            return Some(current);
        }
        match current.parent() {
            Some(p) if !p.as_os_str().is_empty() => current = p.to_path_buf(),
            _ => return None,
        }
    }
}

// ============================================================================
// 7. PROMPT FILE HELPERS(对齐 Node line 318-334)
// ============================================================================

/// 是否 prompt 引用文件格式(`{file:...}`)。
///
/// 对应 Node `isPromptFileReference` (`shared.js` line 320-322)。
pub fn is_prompt_file_reference(value: &str) -> bool {
    PROMPT_FILE_PATTERN.is_match(value)
}

/// 解析 `{file:path}` 引用为绝对路径。
///
/// 对应 Node `resolvePromptFilePath` (`shared.js` line 324-326)。
pub fn resolve_prompt_file_path(reference: &str) -> Option<PathBuf> {
    let caps = PROMPT_FILE_PATTERN.captures(reference)?;
    let raw = caps.get(1)?.as_str();
    let expanded = if let Some(stripped) = raw.strip_prefix("~/") {
        home_dir().join(stripped)
    } else if raw == "~" {
        home_dir()
    } else {
        PathBuf::from(raw)
    };
    Some(expanded)
}

/// 写 prompt 文件(content 为空字符串也允许)。
///
/// 对应 Node `writePromptFile` (`shared.js` line 329-334)。
pub fn write_prompt_file(file_path: &Path, content: Option<&str>) -> std::io::Result<()> {
    if let Some(parent) = file_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(file_path, content.unwrap_or(""))?;
    Ok(())
}

// ============================================================================
// 8. SKILL FILE OPERATIONS(对齐 Node line 338-502)
// ============================================================================

/// Skill 元数据。
#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub path: PathBuf,
    pub scope: SkillScope,
    pub source: String,
    pub description: String,
}

/// 递归 walk 找所有 SKILL.md 文件。
///
/// 对应 Node `walkSkillMdFiles` (`shared.js` line 338-364)。
pub fn walk_skill_md_files(root_dir: &Path) -> Vec<PathBuf> {
    let mut results = vec![];
    walk_recursive(root_dir, &mut results);
    results
}

fn walk_recursive(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_recursive(&path, out);
        } else if path.is_file() && path.file_name().map(|n| n == "SKILL.md").unwrap_or(false) {
            out.push(path);
        }
    }
}

/// 从 SKILL.md 路径加载 skill,加入 `skills_map`。无 frontmatter.name → 跳过。
///
/// 对应 Node `addSkillFromMdFile` (`shared.js` line 366-392)。
/// 返回是否成功添加。
pub fn add_skill_from_md_file(
    skills_map: &mut std::collections::HashMap<String, Skill>,
    skill_md_path: &Path,
    scope: SkillScope,
    source: &str,
) -> bool {
    let Ok(parsed) = parse_md_file(skill_md_path) else { return false };
    let name = parsed
        .frontmatter
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    if name.is_empty() {
        return false;
    }
    let description = parsed
        .frontmatter
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    skills_map.insert(
        name.to_string(),
        Skill {
            name: name.to_string(),
            path: skill_md_path.to_path_buf(),
            scope,
            source: source.to_string(),
            description,
        },
    );
    true
}

/// 解析 skill 搜索目录列表(对齐 Node `resolveSkillSearchDirectories` line 394-421)。
pub fn resolve_skill_search_directories(working_directory: Option<&str>) -> Vec<PathBuf> {
    let mut dirs = vec![];
    let mut push = |p: PathBuf| {
        if !p.as_os_str().is_empty() && !dirs.contains(&p) {
            dirs.push(p);
        }
    };

    push(OPENCODE_CONFIG_DIR());

    if let Some(wd) = working_directory {
        let wd_buf = PathBuf::from(wd);
        let worktree_root = find_worktree_root(&wd_buf).unwrap_or_else(|| wd_buf.clone());
        let worktree_root_opt = if worktree_root == wd_buf {
            None
        } else {
            Some(worktree_root.as_path())
        };
        for ancestor in get_ancestors(&wd_buf, worktree_root_opt) {
            push(ancestor.join(".opencode"));
        }
    }

    push(home_dir().join(".opencode"));

    if let Ok(env) = std::env::var("OPENCODE_CONFIG_DIR") {
        if !env.trim().is_empty() {
            push(PathBuf::from(env.trim()));
        }
    }

    dirs
}

/// 列出 skill 目录下除 SKILL.md 外的所有支持文件。
#[derive(Debug, Clone)]
pub struct SkillSupportingFile {
    pub name: String,
    pub path: String,
    pub full_path: PathBuf,
}

pub fn list_skill_supporting_files(skill_dir: &Path) -> std::io::Result<Vec<SkillSupportingFile>> {
    let mut out = vec![];
    if !skill_dir.exists() {
        return Ok(out);
    }
    walk_supporting(skill_dir, "", &mut out)?;
    Ok(out)
}

fn walk_supporting(
    dir: &Path,
    rel: &str,
    out: &mut Vec<SkillSupportingFile>,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        let rel_path = if rel.is_empty() { name.clone() } else { format!("{}/{}", rel, name) };
        if path.is_dir() {
            walk_supporting(&path, &rel_path, out)?;
        } else if name != "SKILL.md" {
            out.push(SkillSupportingFile { name, path: rel_path, full_path: path });
        }
    }
    Ok(())
}

/// 校验 `relative_path` 在 `skill_dir` 内,返回绝对路径(防止越界)。
pub fn assert_path_within_skill_dir(
    skill_dir: &Path,
    relative_path: &Path,
) -> std::io::Result<PathBuf> {
    let root = std::fs::canonicalize(skill_dir)?;
    let target = root.join(relative_path);
    let canonical = if target.exists() {
        std::fs::canonicalize(&target)?
    } else {
        // 不存在的路径用 "logical" 合并后检测,防止 prefix-attack
        let joined = root.join(relative_path);
        let joined_str = joined.to_string_lossy();
        let root_str = root.to_string_lossy();
        if !joined_str.starts_with(&*root_str) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "Access to file denied",
            ));
        }
        joined
    };
    let relative = canonical.strip_prefix(&root).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::PermissionDenied, "Access to file denied")
    })?;
    if relative.as_os_str().is_empty()
        || (!relative.to_string_lossy().starts_with("..") && !relative.is_absolute())
    {
        Ok(canonical)
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "Access to file denied",
        ))
    }
}

pub fn read_skill_supporting_file(
    skill_dir: &Path,
    relative_path: &Path,
) -> std::io::Result<Option<String>> {
    let full = assert_path_within_skill_dir(skill_dir, relative_path)?;
    if !full.exists() {
        return Ok(None);
    }
    Ok(Some(std::fs::read_to_string(full)?))
}

pub fn write_skill_supporting_file(
    skill_dir: &Path,
    relative_path: &Path,
    content: &str,
) -> std::io::Result<()> {
    let full = assert_path_within_skill_dir(skill_dir, relative_path)?;
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(full, content)
}

pub fn delete_skill_supporting_file(
    skill_dir: &Path,
    relative_path: &Path,
) -> std::io::Result<()> {
    let full = assert_path_within_skill_dir(skill_dir, relative_path)?;
    if full.exists() {
        std::fs::remove_file(&full)?;
        // 清理空目录
        let mut parent = full.parent().map(|p| p.to_path_buf());
        let root = std::fs::canonicalize(skill_dir)?;
        while let Some(p) = parent.take() {
            if p == root {
                break;
            }
            let is_empty = std::fs::read_dir(&p)?.next().is_none();
            if is_empty {
                std::fs::remove_dir(&p)?;
                parent = p.parent().map(|x| x.to_path_buf());
            } else {
                break;
            }
        }
    }
    Ok(())
}

// ============================================================================
// 测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opencode::auth::tests as auth_tests;
    use std::env;

    /// 各测试隔离的临时 HOME(共享 auth::TEST_LOCK 以跨模块串行)。
    fn with_temp_home<F: FnOnce(PathBuf)>(f: F) {
        let _guard = auth_tests::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // 进入时清理可能 leak 的 OPENCODE_CONFIG
        env::remove_var("OPENCODE_CONFIG");
        let temp = std::env::temp_dir().join(format!(
            "oc-config-test-{}-{}",
            std::process::id(),
            rand::random::<u32>()
        ));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let prev = env::var("HOME").ok();
        env::set_var("HOME", &temp);
        f(temp.clone());
        env::remove_var("OPENCODE_CONFIG");
        match prev {
            Some(v) => env::set_var("HOME", v),
            None => env::remove_var("HOME"),
        }
        let _ = std::fs::remove_dir_all(&temp);
    }

    // ---- markdown 测试 (4 个) ----

    #[test]
    fn parse_md_file_without_frontmatter_returns_empty_fm() {
        with_temp_home(|dir| {
            let path = dir.join("plain.md");
            std::fs::write(&path, "hello world").unwrap();
            let parsed = parse_md_file(&path).unwrap();
            assert_eq!(parsed.frontmatter, json!({}));
            assert_eq!(parsed.body, "hello world");
        });
    }

    #[test]
    fn parse_md_file_with_frontmatter() {
        with_temp_home(|dir| {
            let path = dir.join("agent.md");
            std::fs::write(&path, "---\nname: foo\ndescription: bar\n---\nbody here").unwrap();
            let parsed = parse_md_file(&path).unwrap();
            assert_eq!(parsed.frontmatter["name"], json!("foo"));
            assert_eq!(parsed.frontmatter["description"], json!("bar"));
            assert_eq!(parsed.body, "body here");
        });
    }

    #[test]
    fn write_then_parse_md_round_trip() {
        with_temp_home(|dir| {
            let path = dir.join("rt.md");
            write_md_file(&path, &json!({"name": "x"}), "body").unwrap();
            let parsed = parse_md_file(&path).unwrap();
            assert_eq!(parsed.frontmatter["name"], json!("x"));
            assert_eq!(parsed.body, "body");
        });
    }

    #[test]
    fn write_md_filters_null_values() {
        with_temp_home(|dir| {
            let path = dir.join("null.md");
            write_md_file(&path, &json!({"name": "x", "dropped": null}), "body").unwrap();
            let content = std::fs::read_to_string(&path).unwrap();
            assert!(content.contains("name: x"));
            assert!(!content.contains("dropped"));
        });
    }

    // ---- config 层测试 (8 个) ----

    #[test]
    fn read_config_file_empty_returns_empty_object() {
        with_temp_home(|dir| {
            let path = dir.join("config.json");
            assert_eq!(read_config_file(&path).unwrap(), json!({}));
        });
    }

    #[test]
    fn read_config_file_valid_json() {
        with_temp_home(|dir| {
            let path = dir.join("c.json");
            std::fs::write(&path, r#"{"a":1,"b":{"c":2}}"#).unwrap();
            assert_eq!(read_config_file(&path).unwrap(), json!({"a":1,"b":{"c":2}}));
        });
    }

    #[test]
    fn read_config_file_valid_jsonc_with_trailing_comma() {
        with_temp_home(|dir| {
            let path = dir.join("c.jsonc");
            std::fs::write(&path, r#"{"a":1,"b":[1,2,3,]}"#).unwrap();
            let parsed = read_config_file(&path).unwrap();
            assert_eq!(parsed["a"], json!(1));
            assert_eq!(parsed["b"], json!([1, 2, 3]));
        });
    }

    #[test]
    fn read_config_file_invalid_returns_error() {
        with_temp_home(|dir| {
            let path = dir.join("bad.json");
            std::fs::write(&path, "{ broken").unwrap();
            assert!(matches!(read_config_file(&path), Err(ConfigError::Parse(_))));
        });
    }

    #[test]
    fn merge_configs_shallow_override() {
        let base = json!({"a":1,"b":2});
        let over = json!({"b":99,"c":3});
        assert_eq!(merge_configs(&base, &over), json!({"a":1,"b":99,"c":3}));
    }

    #[test]
    fn merge_configs_deep_nested() {
        let base = json!({"a":{"x":1,"y":2},"z":0});
        let over = json!({"a":{"y":99,"z":3}});
        assert_eq!(
            merge_configs(&base, &over),
            json!({"a":{"x":1,"y":99,"z":3},"z":0})
        );
    }

    #[test]
    fn merge_configs_array_overrides_not_merges() {
        let base = json!({"a":[1,2,3]});
        let over = json!({"a":[9]});
        assert_eq!(merge_configs(&base, &over), json!({"a":[9]}));
    }

    #[test]
    fn merge_configs_null_overrides() {
        let base = json!({"a":1});
        let over = json!({"a":null});
        assert_eq!(merge_configs(&base, &over), json!({"a":null}));
    }

    #[test]
    fn read_config_layers_three_layer_merge_order() {
        with_temp_home(|home| {
            // user config
            let user_cfg = home.join(".config").join("opencode").join("config.json");
            std::fs::create_dir_all(user_cfg.parent().unwrap()).unwrap();
            std::fs::write(&user_cfg, r#"{"u":1,"shared":"u"}"#).unwrap();

            // project config (custom)
            env::set_var("OPENCODE_CONFIG", home.join("custom.json").to_str().unwrap());
            std::fs::write(home.join("custom.json"), r#"{"c":3,"shared":"c"}"#).unwrap();

            let layers = read_config_layers(None).unwrap();
            // merged = user → custom(custom overrides shared)
            assert_eq!(layers.merged_config["u"], json!(1));
            assert_eq!(layers.merged_config["c"], json!(3));
            assert_eq!(layers.merged_config["shared"], json!("c"));

            env::remove_var("OPENCODE_CONFIG");
        });
    }

    #[test]
    fn get_project_config_path_returns_first_existing() {
        let dir = std::env::temp_dir().join(format!("oc-proj-{}", rand::random::<u32>()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("opencode.json"), "{}").unwrap();
        assert_eq!(
            get_project_config_path(Some(dir.to_str().unwrap())),
            Some(dir.join("opencode.json"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn get_primary_user_config_path_returns_first_existing() {
        with_temp_home(|home| {
            let cfg = home.join(".config").join("opencode").join("config.json");
            std::fs::create_dir_all(cfg.parent().unwrap()).unwrap();
            std::fs::write(&cfg, "{}").unwrap();
            let paths = vec![
                home.join(".config").join("opencode").join("opencode.json"),
                cfg.clone(),
            ];
            assert_eq!(get_primary_user_config_path(&paths), cfg);
        });
    }

    #[test]
    fn write_config_creates_backup_and_atomic() {
        with_temp_home(|dir| {
            let path = dir.join("c.json");
            write_config(&json!({"v":1}), &path).unwrap();
            write_config(&json!({"v":2}), &path).unwrap();
            let backup = format!("{}.openchamber.backup", path.display());
            assert!(std::path::PathBuf::from(&backup).exists());
            assert_eq!(read_config_file(&path).unwrap(), json!({"v":2}));
            let backup_content = std::fs::read_to_string(&backup).unwrap();
            assert!(backup_content.contains("\"v\": 1"));
        });
    }

    // ---- worktree / git helpers ----

    #[test]
    fn get_ancestors_inclusive() {
        let dir = std::env::temp_dir().join(format!("oc-anc-{}", rand::random::<u32>()));
        let sub = dir.join("a").join("b");
        std::fs::create_dir_all(&sub).unwrap();
        let ancestors = get_ancestors(&sub, None);
        assert_eq!(ancestors[0], sub);
        assert_eq!(ancestors[1], dir.join("a"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_worktree_root_walks_up_to_dot_git() {
        let dir = std::env::temp_dir().join(format!("oc-wt-{}", rand::random::<u32>()));
        let sub = dir.join("a").join("b");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::create_dir(dir.join(".git")).unwrap();
        assert_eq!(find_worktree_root(&sub), Some(dir.clone()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn is_prompt_file_reference_matches() {
        assert!(is_prompt_file_reference("{file:foo.txt}"));
        assert!(is_prompt_file_reference("{FILE:foo.txt}"));
        assert!(!is_prompt_file_reference("foo.txt"));
    }

    #[test]
    fn resolve_prompt_file_path_expands_tilde() {
        let resolved = resolve_prompt_file_path("{file:~/x.txt}").unwrap();
        assert!(resolved.ends_with("x.txt"));
    }

    #[test]
    fn walk_skill_md_files_finds_only_skill_md() {
        let dir = std::env::temp_dir().join(format!("oc-sk-{}", rand::random::<u32>()));
        let s1 = dir.join("skills").join("a");
        let s2 = dir.join("skills").join("b");
        std::fs::create_dir_all(&s1).unwrap();
        std::fs::create_dir_all(&s2).unwrap();
        std::fs::write(s1.join("SKILL.md"), "---\nname: a\n---\n").unwrap();
        std::fs::write(s2.join("SKILL.md"), "---\nname: b\n---\n").unwrap();
        std::fs::write(s1.join("README.md"), "ignore").unwrap();

        let found = walk_skill_md_files(&dir.join("skills"));
        assert_eq!(found.len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn assert_path_within_skill_dir_rejects_traversal() {
        let dir = std::env::temp_dir().join(format!("oc-trav-{}", rand::random::<u32>()));
        let skill = dir.join("skill");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(dir.join("secret.txt"), "no").unwrap();
        assert!(assert_path_within_skill_dir(&skill, Path::new("../secret.txt")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
