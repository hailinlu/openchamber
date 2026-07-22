//! 工作区目录解析。
//!
//! 移植 `packages/web/server/lib/opencode/project-directory-runtime.js`。
//!
//! 解析优先级 (与 Node 完全一致):
//!   1. `x-opencode-directory` 请求头 (仅当 `x-opencode-directory-encoding: uri` 时 URI 解码)
//!   2. `?directory` query 参数
//!   3. `settings.json` 的 `lastDirectory` (优先于 activeProjectId)
//!   4. `settings.json` 的 `activeProjectId` → projects[id].path
//!   5. projects[0].path

use std::path::{Path, PathBuf};

use axum::http::HeaderMap;
use serde::Deserialize;

/// 解析工作区目录的结果。
#[allow(dead_code)]
#[derive(Debug)]
pub struct ResolvedDirectory {
    pub directory: PathBuf,
}

/// 从请求上下文解析项目目录。
///
/// 检查 header/query hint → settings.json fallback。
/// 返回 `None` 当没有可用目录时 (handler 应返回 400)。
pub async fn resolve_project_directory(
    headers: &HeaderMap,
    query_directory: Option<&str>,
    settings_path: &Path,
) -> Option<PathBuf> {
    // 1. 收集候选目录 (header + query)
    let mut candidates: Vec<PathBuf> = Vec::new();

    // x-opencode-directory header
    if let Some(dir_header) = headers.get("x-opencode-directory") {
        if let Ok(dir_str) = dir_header.to_str() {
            let encoding_header = headers.get("x-opencode-directory-encoding");
            let is_uri_encoded = encoding_header
                .and_then(|v| v.to_str().ok())
                .map(|v| v.eq_ignore_ascii_case("uri"))
                .unwrap_or(false);

            let decoded = if is_uri_encoded {
                percent_encoding::percent_decode_str(dir_str)
                    .decode_utf8_lossy()
                    .to_string()
            } else {
                dir_str.to_string()
            };

            let normalized = normalize_directory_path(&decoded);
            if !normalized.is_empty() {
                candidates.push(PathBuf::from(normalized));
            }
        }
    }

    // ?directory query
    if let Some(dir) = query_directory {
        let normalized = normalize_directory_path(dir);
        if !normalized.is_empty() {
            candidates.push(PathBuf::from(normalized));
        }
    }

    // 2. 验证候选: 第一个存在的目录
    for candidate in &candidates {
        if tokio::fs::metadata(candidate).await.is_ok() {
            return Some(candidate.canonicalize().unwrap_or_else(|_| candidate.clone()));
        }
    }

    // 如果有候选但都不存在, 返回 None (handler 应 400)
    if !candidates.is_empty() {
        return None;
    }

    // 3. 无 hint → 读 settings.json fallback
    let settings = read_settings(settings_path).await;

    // lastDirectory (优先于 activeProjectId)
    if let Some(last_dir) = settings.last_directory.as_ref() {
        let normalized = normalize_directory_path(last_dir);
        if !normalized.is_empty() && tokio::fs::metadata(&normalized).await.is_ok() {
            return Some(PathBuf::from(normalized));
        }
    }

    // activeProjectId → projects[id].path
    if let Some(active_id) = settings.active_project_id.as_ref() {
        if let Some(project) = settings.projects.iter().find(|p| &p.id == active_id) {
            if tokio::fs::metadata(&project.path).await.is_ok() {
                return Some(PathBuf::from(&project.path));
            }
        }
    }

    // projects[0].path
    if let Some(project) = settings.projects.first() {
        if tokio::fs::metadata(&project.path).await.is_ok() {
            return Some(PathBuf::from(&project.path));
        }
    }

    None
}

/// `~`/`~/` 展开为 home 目录 + trim。
///
/// 对应 `git/service.js` 的 `normalizeDirectoryPath` (git-local 版, 不剥引号)。
pub fn normalize_directory_path(input: &str) -> String {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    // ~ 展开
    if trimmed == "~" {
        return home_dir().unwrap_or_default();
    }
    if let Some(rest) = trimmed.strip_prefix("~/").or_else(|| trimmed.strip_prefix("~\\")) {
        if let Some(home) = home_dir() {
            return format!("{}/{}", home, rest);
        }
    }

    trimmed.to_string()
}

/// 获取 home 目录。
fn home_dir() -> Option<String> {
    std::env::var("HOME").ok().filter(|s| !s.is_empty())
}

/// settings.json 结构 (只读所需字段)。
#[derive(Deserialize, Default, Debug)]
struct Settings {
    #[serde(default, rename = "lastDirectory")]
    last_directory: Option<String>,
    #[serde(default, rename = "activeProjectId")]
    active_project_id: Option<String>,
    #[serde(default)]
    projects: Vec<ProjectEntry>,
}

#[derive(Deserialize, Default, Debug)]
struct ProjectEntry {
    id: String,
    path: String,
}

/// 读取 settings.json, 文件不存在/解析失败返回空 Settings。
async fn read_settings(settings_path: &Path) -> Settings {
    match tokio::fs::read_to_string(settings_path).await {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => Settings::default(),
    }
}

mod percent_encoding {
    /// 简化的 percent-decode (不需要完整 crate)。
    pub fn percent_decode_str<'a>(input: &'a str) -> PercentDecoded<'a> {
        PercentDecoded { input: input.as_bytes(), pos: 0 }
    }

    pub struct PercentDecoded<'a> {
        input: &'a [u8],
        pos: usize,
    }

    impl<'a> PercentDecoded<'a> {
        pub fn decode_utf8_lossy(self) -> std::borrow::Cow<'a, str> {
            let decoded = self.collect::<Vec<u8>>();
            String::from_utf8_lossy(&decoded).into_owned().into()
        }
    }

    impl<'a> Iterator for PercentDecoded<'a> {
        type Item = u8;

        fn next(&mut self) -> Option<u8> {
            if self.pos >= self.input.len() {
                return None;
            }
            let b = self.input[self.pos];
            self.pos += 1;

            if b == b'%' && self.pos + 1 < self.input.len() {
                let h = hex_val(self.input[self.pos]);
                let l = hex_val(self.input[self.pos + 1]);
                if let (Some(hi), Some(lo)) = (h, l) {
                    self.pos += 2;
                    return Some(hi * 16 + lo);
                }
            }
            Some(b)
        }
    }

    fn hex_val(b: u8) -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderValue};
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_test_dir(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "openchamber-project-dir-{label}-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create temp project directory");
        path
    }

    fn percent_encode_path(path: &str) -> String {
        path.bytes()
            .map(|byte| match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    (byte as char).to_string()
                }
                _ => format!("%{byte:02X}"),
            })
            .collect()
    }

    #[tokio::test]
    async fn request_header_directory_wins_over_settings() {
        let request_dir = temp_test_dir("request");
        let settings_dir = temp_test_dir("settings");
        let settings_path = settings_dir.join("settings.json");
        std::fs::write(
            &settings_path,
            serde_json::json!({ "lastDirectory": settings_dir }).to_string(),
        )
        .expect("write settings");

        let mut headers = HeaderMap::new();
        headers.insert(
            "x-opencode-directory",
            HeaderValue::from_str(request_dir.to_str().expect("utf-8 path"))
                .expect("valid header path"),
        );

        let resolved = resolve_project_directory(&headers, None, &settings_path).await;
        assert_eq!(resolved, request_dir.canonicalize().ok());

        std::fs::remove_dir_all(request_dir).ok();
        std::fs::remove_dir_all(settings_dir).ok();
    }

    #[tokio::test]
    async fn uri_encoded_request_header_directory_is_decoded() {
        let request_dir = temp_test_dir("encoded request");
        let settings_path = request_dir.join("missing-settings.json");
        let encoded = percent_encode_path(request_dir.to_str().expect("utf-8 path"));

        let mut headers = HeaderMap::new();
        headers.insert(
            "x-opencode-directory",
            HeaderValue::from_str(&encoded).expect("valid encoded header"),
        );
        headers.insert(
            "x-opencode-directory-encoding",
            HeaderValue::from_static("uri"),
        );

        let resolved = resolve_project_directory(&headers, None, &settings_path).await;
        assert_eq!(resolved, request_dir.canonicalize().ok());

        std::fs::remove_dir_all(request_dir).ok();
    }

    #[test]
    fn normalize_empty() {
        assert_eq!(normalize_directory_path(""), "");
        assert_eq!(normalize_directory_path("   "), "");
    }

    #[test]
    fn normalize_plain() {
        assert_eq!(normalize_directory_path("/usr/local"), "/usr/local");
    }

    #[test]
    fn normalize_tilde_only() {
        // 取决于 HOME 环境变量
        let result = normalize_directory_path("~");
        // 在不同平台上 home 可能是盘符根目录，例如 `C:`；只验证结果不是未展开的 `~`。
        assert_ne!(result, "~");
    }

    #[test]
    fn normalize_tilde_slash() {
        std::env::set_var("HOME", "/testhome");
        assert_eq!(normalize_directory_path("~/projects"), "/testhome/projects");
        std::env::remove_var("HOME");
    }

    #[test]
    fn settings_default() {
        let s = Settings::default();
        assert!(s.last_directory.is_none());
        assert!(s.projects.is_empty());
    }

    #[test]
    fn settings_parse() {
        let json = r#"{
            "lastDirectory": "/home/user/project",
            "activeProjectId": "path_abc",
            "projects": [{"id": "path_abc", "path": "/home/user/project"}]
        }"#;
        let s: Settings = serde_json::from_str(json).unwrap();
        assert_eq!(s.last_directory.as_deref(), Some("/home/user/project"));
        assert_eq!(s.active_project_id.as_deref(), Some("path_abc"));
        assert_eq!(s.projects.len(), 1);
        assert_eq!(s.projects[0].path, "/home/user/project");
    }

    #[test]
    fn percent_decode_basic() {
        let decoded = percent_encoding::percent_decode_str("/foo%20bar/baz")
            .decode_utf8_lossy();
        assert_eq!(decoded, "/foo bar/baz");
    }
}
