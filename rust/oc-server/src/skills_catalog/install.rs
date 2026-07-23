//! Skill 安装 — 对应 Node `install.js` + `clawdhub/install.js`。
//!
//! 从 git 仓库或 ClawdHub 安装 skill 到目标目录。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::git::{looks_like_auth_error, DefaultGitRunner, GitRunner};
use super::scan::is_valid_skill_name;
use super::source::parse_skill_repo_source;

/// 安装结果。
#[derive(Debug, Clone, Serialize)]
pub struct InstallResult {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installed: Option<Vec<InstalledSkill>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped: Option<Vec<SkippedSkill>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<InstallError>,
}

#[derive(Debug, Clone, Serialize)]
pub struct InstalledSkill {
    pub skill_name: String,
    pub scope: String,
    pub source: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SkippedSkill {
    pub skill_name: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct InstallError {
    pub kind: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conflicts: Option<Vec<ConflictInfo>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssh_only: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConflictInfo {
    pub skill_name: String,
    pub scope: String,
    pub source: String,
}

/// 安装请求参数字段。
#[derive(Debug, Clone, Deserialize)]
pub struct InstallRequest {
    pub source: Option<String>,
    pub subpath: Option<String>,
    pub git_identity_id: Option<String>,
    pub scope: Option<String>,
    pub target_source: Option<String>,
    pub selections: Option<Vec<Selection>>,
    pub conflict_policy: Option<String>,
    pub conflict_decisions: Option<HashMap<String, String>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Selection {
    pub skill_dir: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clawdhub: Option<ClawdhubSelection>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ClawdhubSelection {
    pub slug: String,
    pub version: Option<String>,
}

/// 获取 skill 目标目录。
pub fn get_target_skill_dir(
    scope: &str,
    target_source: &str,
    working_directory: Option<&Path>,
) -> PathBuf {
    match (scope, target_source) {
        ("user", "agents") => {
            let home = crate::git::paths::home_dir_string();
            PathBuf::from(home).join(".agents").join("skills")
        }
        ("user", _) => {
            let home = crate::git::paths::home_dir_string();
            PathBuf::from(home).join(".config").join("opencode").join("skills")
        }
        ("project", "agents") => {
            working_directory
                .map(|wd| wd.join(".agents").join("skills"))
                .unwrap_or_else(|| PathBuf::from(".agents").join("skills"))
        }
        ("project", _) => {
            working_directory
                .map(|wd| wd.join(".opencode").join("skills"))
                .unwrap_or_else(|| PathBuf::from(".opencode").join("skills"))
        }
        // fallback: treat as user + opencode
        _ => {
            let home = crate::git::paths::home_dir_string();
            PathBuf::from(home).join(".config").join("opencode").join("skills")
        }
    }
}

/// 从 git 仓库安装 skills。
pub fn install_skills_from_repository(
    source: &str,
    subpath: Option<&str>,
    scope: &str,
    target_source: &str,
    selections: &[Selection],
    conflict_policy: Option<&str>,
    conflict_decisions: &HashMap<String, String>,
    working_directory: Option<&Path>,
    runner: &dyn GitRunner,
) -> InstallResult {
    let parsed = parse_skill_repo_source(source);
    if !parsed.ok {
        return InstallResult {
            ok: false,
            installed: None,
            skipped: None,
            error: Some(InstallError {
                kind: "invalidSource".to_string(),
                message: "Unable to parse source".to_string(),
                conflicts: None,
                ssh_only: None,
            }),
        };
    }

    let target_base = get_target_skill_dir(scope, target_source, working_directory);

    // 检查冲突 (目录已存在)
    let conflicts = check_conflicts(&selections, &target_base);
    if !conflicts.is_empty() {
        let resolved = resolve_conflicts(&conflicts, conflict_policy, conflict_decisions);
        if !resolved.is_empty() {
            // 仍有未解决的冲突
            return InstallResult {
                ok: false,
                installed: None,
                skipped: None,
                error: Some(InstallError {
                    kind: "conflicts".to_string(),
                    message: "Conflicts detected".to_string(),
                    conflicts: Some(resolved),
                    ssh_only: None,
                }),
            };
        }
    }

    // Clone 仓库到临时目录
    let clone_url = parsed
        .clone_url_https
        .or(parsed.clone_url_ssh)
        .unwrap_or_default();
    let tmp_dir = std::env::temp_dir().join(format!(
        "sc-install-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));

    let effective_subpath = subpath.or(parsed.effective_subpath.as_deref());

    let clone_result = runner.run(
        &[
            "clone",
            "--depth",
            "1",
            "--filter",
            "blob:none",
            "--sparse",
            &clone_url,
            tmp_dir.to_str().unwrap_or("/tmp/sc-install"),
        ],
        None,
        Some(120_000),
    );

    if !clone_result.ok {
        let msg = clone_result.message.unwrap_or_default();
        let kind = if looks_like_auth_error(&msg) {
            "authRequired"
        } else {
            "unknown"
        };
        let _ = std::fs::remove_dir_all(&tmp_dir);
        return InstallResult {
            ok: false,
            installed: None,
            skipped: None,
            error: Some(InstallError {
                kind: kind.to_string(),
                message: msg,
                conflicts: None,
                ssh_only: Some(clone_url.starts_with("git@")),
            }),
        };
    }

    // sparse checkout 需要的子路径
    if let Some(sub) = effective_subpath {
        runner.run(
            &["sparse-checkout", "set", "--no-cone", &format!("{}/**", sub)],
            Some(tmp_dir.to_str().unwrap()),
            Some(30_000),
        );
        runner.run(&["checkout"], Some(tmp_dir.to_str().unwrap()), Some(30_000));
    }

    // 将选中的 skill 目录复制到目标位置
    let source_base = match effective_subpath {
        Some(sub) => tmp_dir.join(sub),
        None => tmp_dir.clone(),
    };

    let mut installed = Vec::new();
    let mut skipped = Vec::new();

    for selection in selections {
        let skill_dir_name = &selection.skill_dir;
        let src_path = source_base.join(skill_dir_name);
        let skill_md = src_path.join("SKILL.md");

        if !skill_md.exists() {
            skipped.push(SkippedSkill {
                skill_name: skill_dir_name.clone(),
                reason: "SKILL.md not found".to_string(),
            });
            continue;
        }

        if !is_valid_skill_name(skill_dir_name) {
            skipped.push(SkippedSkill {
                skill_name: skill_dir_name.clone(),
                reason: "Invalid skill name".to_string(),
            });
            continue;
        }

        // 检查冲突解决结果
        let decision = conflict_decisions.get(skill_dir_name).map(|s| s.as_str());
        let target_dir = target_base.join(skill_dir_name);
        if target_dir.exists() {
            match decision {
                Some("overwrite") => {
                    let _ = std::fs::remove_dir_all(&target_dir);
                }
                Some("skip") => {
                    skipped.push(SkippedSkill {
                        skill_name: skill_dir_name.clone(),
                        reason: "Skipped by user decision".to_string(),
                    });
                    continue;
                }
                _ => {
                    // 不应该发生 — 已在冲突检查中处理
                    skipped.push(SkippedSkill {
                        skill_name: skill_dir_name.clone(),
                        reason: "Target exists".to_string(),
                    });
                    continue;
                }
            }
        }

        // 复制目录
        match copy_dir(&src_path, &target_dir) {
            Ok(_) => {
                installed.push(InstalledSkill {
                    skill_name: skill_dir_name.clone(),
                    scope: scope.to_string(),
                    source: target_source.to_string(),
                });
            }
            Err(e) => {
                skipped.push(SkippedSkill {
                    skill_name: skill_dir_name.clone(),
                    reason: format!("Copy error: {}", e),
                });
            }
        }
    }

    let _ = std::fs::remove_dir_all(&tmp_dir);

    InstallResult {
        ok: true,
        installed: Some(installed),
        skipped: Some(skipped),
        error: None,
    }
}

/// 检查冲突: 目标目录中哪些 skill 已经存在。
fn check_conflicts(selections: &[Selection], target_base: &Path) -> Vec<ConflictInfo> {
    let mut conflicts = Vec::new();
    for sel in selections {
        let target_dir = target_base.join(&sel.skill_dir);
        if target_dir.exists() {
            conflicts.push(ConflictInfo {
                skill_name: sel.skill_dir.clone(),
                scope: "unknown".to_string(),
                source: "opencode".to_string(),
            });
        }
    }
    conflicts
}

/// 解析冲突: 应用 conflict_policy 和 conflict_decisions。
/// 返回仍未解决的冲突列表。
fn resolve_conflicts(
    conflicts: &[ConflictInfo],
    conflict_policy: Option<&str>,
    conflict_decisions: &HashMap<String, String>,
) -> Vec<ConflictInfo> {
    let mut unresolved = Vec::new();
    for conflict in conflicts {
        let decision = conflict_decisions
            .get(&conflict.skill_name)
            .map(|s| s.as_str());
        match decision {
            Some("overwrite") | Some("skip") => {
                // resolved
            }
            Some(_) => {
                // unknown decision value; treat as unresolved
                unresolved.push(conflict.clone());
            }
            None => match conflict_policy {
                Some("overwriteAll") | Some("skipAll") => {
                    // resolved
                }
                _ => {
                    unresolved.push(conflict.clone());
                }
            },
        }
    }
    unresolved
}

/// 递归复制目录 (简单实现)。
fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    if !src.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_get_target_skill_dir_user_opencode() {
        let dir = get_target_skill_dir("user", "opencode", None);
        // 跨平台: 用 components 检查后缀,避免 Windows 反斜杠与 Unix 正斜杠的断言分歧。
        let tail: Vec<_> = dir
            .components()
            .filter_map(|c| c.as_os_str().to_str())
            .rev()
            .take(3)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>();
        assert_eq!(tail, vec![".config", "opencode", "skills"]);
    }

    #[test]
    fn test_get_target_skill_dir_user_agents() {
        let dir = get_target_skill_dir("user", "agents", None);
        let tail: Vec<_> = dir
            .components()
            .filter_map(|c| c.as_os_str().to_str())
            .rev()
            .take(2)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>();
        assert_eq!(tail, vec![".agents", "skills"]);
    }

    #[test]
    fn test_get_target_skill_dir_project() {
        let wd = Path::new("/tmp/myproject");
        let dir = get_target_skill_dir("project", "opencode", Some(wd));
        assert_eq!(dir, PathBuf::from("/tmp/myproject/.opencode/skills"));
    }

    #[test]
    fn test_check_conflicts_empty() {
        let tmp = std::env::temp_dir().join(format!("cc-{}", std::process::id()));
        let _ = fs::create_dir_all(&tmp);
        let selections = vec![Selection {
            skill_dir: "nonexistent".to_string(),
            clawdhub: None,
        }];
        let conflicts = check_conflicts(&selections, &tmp);
        assert!(conflicts.is_empty());
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_check_conflicts_existing() {
        let tmp = std::env::temp_dir().join(format!("cc2-{}", std::process::id()));
        let _ = fs::create_dir_all(&tmp);
        let existing = tmp.join("my-skill");
        let _ = fs::create_dir_all(&existing);

        let selections = vec![Selection {
            skill_dir: "my-skill".to_string(),
            clawdhub: None,
        }];
        let conflicts = check_conflicts(&selections, &tmp);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].skill_name, "my-skill");

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_resolve_conflicts_policy_overwrite() {
        let conflicts = vec![ConflictInfo {
            skill_name: "test".to_string(),
            scope: "user".to_string(),
            source: "opencode".to_string(),
        }];
        let decisions = HashMap::new();
        let unresolved = resolve_conflicts(&conflicts, Some("overwriteAll"), &decisions);
        assert!(unresolved.is_empty());
    }

    #[test]
    fn test_resolve_conflicts_no_policy() {
        let conflicts = vec![ConflictInfo {
            skill_name: "test".to_string(),
            scope: "user".to_string(),
            source: "opencode".to_string(),
        }];
        let decisions = HashMap::new();
        let unresolved = resolve_conflicts(&conflicts, None, &decisions);
        assert_eq!(unresolved.len(), 1);
    }

    #[test]
    fn test_resolve_conflicts_with_decision() {
        let conflicts = vec![ConflictInfo {
            skill_name: "test".to_string(),
            scope: "user".to_string(),
            source: "opencode".to_string(),
        }];
        let mut decisions = HashMap::new();
        decisions.insert("test".to_string(), "overwrite".to_string());
        let unresolved = resolve_conflicts(&conflicts, None, &decisions);
        assert!(unresolved.is_empty());
    }
}
