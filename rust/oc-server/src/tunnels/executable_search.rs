//! 跨平台可执行文件搜索。
//!
//! 移植自 `packages/web/server/lib/tunnels/executable-search.js` (140 行)。
//!
//! 功能:
//!   - PATH 搜索 (含 Windows Store apps 目录)
//!   - Windows PATHEXT 扩展名处理
//!   - 构建 spawn 用的 env (含搜索路径)

use std::collections::HashMap;
use std::path::PathBuf;

#[cfg(target_os = "windows")]
fn is_windows() -> bool {
    true
}
#[cfg(not(target_os = "windows"))]
fn is_windows() -> bool {
    false
}

fn path_delimiter() -> char {
    if is_windows() {
        ';'
    } else {
        ':'
    }
}

fn get_env_value(env: &HashMap<String, String>, keys: &[&str]) -> String {
    for key in keys {
        if let Some(value) = env.get(*key) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    String::new()
}

/// WindowsApps 目录 (仅 Windows 有效)。
fn get_windows_apps_directory(env: &HashMap<String, String>) -> PathBuf {
    let local_app_data = get_env_value(env, &["LOCALAPPDATA", "LocalAppData", "localappdata"]);
    if !local_app_data.is_empty() {
        return PathBuf::from(local_app_data)
            .join("Microsoft")
            .join("WindowsApps");
    }
    let user_profile = get_env_value(env, &["USERPROFILE", "UserProfile", "userprofile"]);
    if !user_profile.is_empty() {
        return PathBuf::from(user_profile)
            .join("AppData")
            .join("Local")
            .join("Microsoft")
            .join("WindowsApps");
    }
    // fallback: home dir
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home)
        .join("AppData")
        .join("Local")
        .join("Microsoft")
        .join("WindowsApps")
}

/// 获取可执行文件搜索目录列表 (去重)。
pub fn get_executable_search_directories(env: &HashMap<String, String>) -> Vec<PathBuf> {
    let path_value = get_env_value(env, &["PATH", "Path", "path"]);
    let delimiter = path_delimiter();

    let mut directories: Vec<PathBuf> = path_value
        .split(delimiter)
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .collect();

    if is_windows() {
        directories.push(get_windows_apps_directory(env));
    }

    // 去重 (Windows 大小写不敏感)
    let mut seen = std::collections::HashSet::new();
    let mut unique = vec![];
    for dir in directories {
        let key = if is_windows() {
            dir.to_string_lossy().to_lowercase()
        } else {
            dir.to_string_lossy().to_string()
        };
        if key.is_empty() || seen.contains(&key) {
            continue;
        }
        seen.insert(key);
        unique.push(dir);
    }
    unique
}

/// 构建 spawn 用的 env (含搜索路径)。对应 Node `createExecutableSearchEnv`。
pub fn create_executable_search_env() -> HashMap<String, String> {
    let env: HashMap<String, String> = std::env::vars().collect();
    create_executable_search_env_from(&env)
}

fn create_executable_search_env_from(env: &HashMap<String, String>) -> HashMap<String, String> {
    let delimiter = path_delimiter();
    let directories = get_executable_search_directories(env);
    let path_value = directories
        .iter()
        .map(|d| d.to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join(&delimiter.to_string());

    let mut next_env = env.clone();
    if is_windows() {
        next_env.insert("PATH".to_string(), path_value.clone());
        next_env.insert("Path".to_string(), path_value.clone());
        next_env.insert("path".to_string(), path_value);
    } else {
        next_env.insert("PATH".to_string(), path_value);
    }
    next_env
}

/// Windows 可执行文件扩展名列表。
fn get_executable_extensions(env: &HashMap<String, String>) -> Vec<String> {
    if !is_windows() {
        return vec![String::new()];
    }
    let raw = get_env_value(env, &["PATHEXT", "PathExt", "pathext"]);
    let default = ".EXE;.CMD;.BAT;.COM";
    let source = if raw.is_empty() { default } else { &raw };
    source
        .split(';')
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .map(|s| if s.starts_with('.') { s } else { format!(".{}", s) })
        .collect()
}

/// 在 PATH 中搜索可执行文件。返回绝对路径或 None。
/// 对应 Node `findExecutableOnPath`。
#[allow(dead_code)]
pub fn find_executable_on_path(command: &str) -> Option<PathBuf> {
    let env: HashMap<String, String> = std::env::vars().collect();
    find_executable_on_path_with_env(command, &env)
}

fn find_executable_on_path_with_env(
    command: &str,
    env: &HashMap<String, String>,
) -> Option<PathBuf> {
    let command_name = command.trim();
    if command_name.is_empty() {
        return None;
    }

    let directories = get_executable_search_directories(env);
    let extensions = get_executable_extensions(env);

    for directory in &directories {
        for extension in &extensions {
            let file_name = if is_windows() {
                format!("{}{}", command_name, extension)
            } else {
                command_name.to_string()
            };
            let candidate = directory.join(&file_name);

            match std::fs::metadata(&candidate) {
                Ok(meta) => {
                    if !meta.is_file() {
                        continue;
                    }
                    // Unix 检查可执行权限
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let perms = std::fs::metadata(&candidate).ok()?.permissions();
                        if perms.mode() & 0o111 == 0 {
                            continue;
                        }
                    }
                    return Some(candidate);
                }
                Err(_) => continue,
            }
        }
    }
    None
}

/// 返回用于 spawn 的绝对路径 + env。
/// Windows Store alias fallback: 找不到绝对路径时返回原名 (让 CreateProcess 决定)。
/// 对应 Node `resolveExecutableLaunchTarget`。
pub fn resolve_executable_launch_target(command: &str) -> Option<(String, HashMap<String, String>)> {
    let env: HashMap<String, String> = std::env::vars().collect();
    let resolved = find_executable_on_path_with_env(command, &env);
    let search_env = create_executable_search_env_from(&env);

    if let Some(path) = resolved {
        return Some((path.to_string_lossy().to_string(), search_env));
    }

    // Windows Store alias fallback
    if is_windows() {
        let trimmed = command.trim();
        if !trimmed.is_empty() {
            return Some((trimmed.to_string(), search_env));
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_empty_returns_none() {
        assert!(find_executable_on_path("").is_none());
    }

    #[test]
    fn resolve_empty_returns_none() {
        assert!(resolve_executable_launch_target("").is_none());
    }

    #[test]
    fn search_env_contains_path() {
        let env = create_executable_search_env();
        assert!(env.contains_key("PATH") || env.contains_key("Path"));
    }
}
