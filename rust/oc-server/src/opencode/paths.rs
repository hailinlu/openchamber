//! OpenCode 路径常量与解析函数。
//!
//! 对应 Node `shared.js` line 9-16 的路径常量 + `auth.js` line 5-6。
//!
//! 所有路径函数返回 `PathBuf`,Unix/Windows 行为差异由 `home_dir()` 处理。

#![allow(dead_code)]

use std::path::{Path, PathBuf};

use crate::git::paths::home_dir;

/// OpenCode config 目录名(`~/.config/opencode`)。
pub const OPENCODE_CONFIG_DIR_NAME: &str = ".config/opencode";

/// OpenCode data 目录名(`~/.local/share/opencode`)。
pub const OPENCODE_DATA_DIR_NAME: &str = ".local/share/opencode";

/// auth.json 文件名。
pub const AUTH_FILE_NAME: &str = "auth.json";

/// config.json 文件名。
pub const CONFIG_FILE_NAME: &str = "config.json";

/// agents 子目录名。
pub const AGENT_DIR_NAME: &str = "agents";

/// commands 子目录名。
pub const COMMAND_DIR_NAME: &str = "commands";

/// skills 子目录名。
pub const SKILL_DIR_NAME: &str = "skills";

/// `~/.config/opencode` 的绝对路径。
pub fn opencode_config_dir() -> PathBuf {
    home_dir().join(OPENCODE_CONFIG_DIR_NAME)
}

/// `~/.local/share/opencode` 的绝对路径。
pub fn opencode_data_dir() -> PathBuf {
    home_dir().join(OPENCODE_DATA_DIR_NAME)
}

/// `~/.local/share/opencode/auth.json`。
pub fn auth_file() -> PathBuf {
    opencode_data_dir().join(AUTH_FILE_NAME)
}

/// `~/.config/opencode/config.json`(若 `OPENCODE_CONFIG` 设置则用之)。
pub fn config_file() -> PathBuf {
    if let Some(custom) = custom_config_file() {
        custom
    } else {
        opencode_config_dir().join(CONFIG_FILE_NAME)
    }
}

/// `~/.config/opencode/agents`。
pub fn agent_dir() -> PathBuf {
    opencode_config_dir().join(AGENT_DIR_NAME)
}

/// `~/.config/opencode/commands`。
pub fn command_dir() -> PathBuf {
    opencode_config_dir().join(COMMAND_DIR_NAME)
}

/// `~/.config/opencode/skills`。
pub fn skill_dir() -> PathBuf {
    opencode_config_dir().join(SKILL_DIR_NAME)
}

/// 读取 `OPENCODE_CONFIG` 环境变量并解析为绝对路径(若设置且非空)。
pub fn custom_config_file() -> Option<PathBuf> {
    std::env::var("OPENCODE_CONFIG")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|s| Path::new(s.trim()).to_path_buf())
}
