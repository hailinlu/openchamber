//! `/api/config/settings` + `/api/behavior/agents-md` 路由。
//!
//! Tauri 模式下 `__GRIDFORGE_API_BASE_URL__` 指向 Rust server,
//! 但 settings 相关的路由原本只在 Node 后端实现。
//! 此处提供简单的文件读写实现，覆盖 BehaviorPage 所需字段。
//!
//! 注意: 这是前端调用 settings 端点的最小实现，不包含 Node 端的迁移/
//! 校验逻辑。如需完整功能，后续可考虑 proxy 到 Node 后端或完善此模块。

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde_json::{json, Value};

use crate::state::AppState;

/// 主 settings 路径的品牌目录名。
const SETTINGS_BRAND_DIR: &str = "gridforge";
/// 旧品牌目录名(Node/Electron 仍在用 `~/.config/openchamber/settings.json`)。
///
/// 品牌从 `openchamber` 重命名为 `gridforge` 时,Node 路径未跟进,历史用户数据
/// (含 `projects` / `activeProjectId` 等 UI 关键字段)仍留在 openchamber 目录。
/// [`legacy_settings_path`] 读它作为回退,合并进 [`resolve_settings_path`] 的结果。
const LEGACY_SETTINGS_BRAND_DIR: &str = "openchamber";

#[cfg(not(target_os = "windows"))]
fn home_brand_dir(brand: &str) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".config").join(brand)
}

#[cfg(target_os = "windows")]
fn home_brand_dir(brand: &str) -> PathBuf {
    let home = std::env::var("USERPROFILE").unwrap_or_else(|_| "C:\\".to_string());
    PathBuf::from(home).join(".config").join(brand)
}

fn default_settings_path() -> PathBuf {
    home_brand_dir(SETTINGS_BRAND_DIR).join("settings.json")
}

fn resolve_settings_path() -> PathBuf {
    if let Ok(dir) = std::env::var("GRIDFORGE_DATA_DIR") {
        PathBuf::from(dir).join("settings.json")
    } else {
        default_settings_path()
    }
}

/// 旧品牌 `~/.config/openchamber/settings.json`。
///
/// 与主路径走同一 home 解析([`home_dir_string`],Windows 读 `USERPROFILE`),
/// 保证双击启动的 GUI 进程也能找到历史数据。
fn legacy_settings_path() -> PathBuf {
    home_brand_dir(LEGACY_SETTINGS_BRAND_DIR).join("settings.json")
}

#[cfg(not(target_os = "windows"))]
fn agents_md_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".config/opencode/AGENTS.md")
}

#[cfg(target_os = "windows")]
fn agents_md_path() -> PathBuf {
    let home = std::env::var("USERPROFILE").unwrap_or_else(|_| "C:\\".to_string());
    PathBuf::from(home).join(".config/opencode/AGENTS.md")
}

/// 读取并合并 settings: 旧品牌 `openchamber/settings.json` 作基础,
/// 主品牌 `gridforge/settings.json` 覆盖。
///
/// 这样历史用户数据(`projects` 等)在品牌重命名后仍能被 UI 读到,
/// 同时尊重 gridforge 路径下更新的设置。
pub(crate) async fn read_merged_settings() -> Value {
    // 1. 基础: 旧品牌 settings (历史数据)
    let legacy_path = legacy_settings_path();
    let base = match tokio::fs::read_to_string(&legacy_path).await {
        Ok(content) => serde_json::from_str::<Value>(&content).unwrap_or(json!({})),
        Err(_) => json!({}),
    };

    // 2. 覆盖: 主品牌 settings (gridforge)
    let path = resolve_settings_path();
    match tokio::fs::read_to_string(&path).await {
        Ok(content) => match serde_json::from_str::<Value>(&content) {
            Ok(current) => deep_merge(base, current),
            Err(e) => {
                tracing::error!(path = %path.display(), error = %e, "failed to parse settings");
                // 主品牌解析失败: 用基础(旧品牌)兜底, 不丢数据
                base
            }
        },
        Err(_) => {
            // 主品牌文件不存在: 仅用基础(旧品牌)
            base
        }
    }
}

/// GET /api/config/settings
///
/// 读取策略见 [`read_merged_settings`]。写入仍走主品牌路径([`put_settings`])。
pub async fn get_settings(
    State(_state): State<Arc<AppState>>,
) -> impl IntoResponse {
    Json(read_merged_settings().await).into_response()
}

/// PUT /api/config/settings
pub async fn put_settings(
    State(_state): State<Arc<AppState>>,
    Json(changes): Json<Value>,
) -> impl IntoResponse {
    let path = resolve_settings_path();

    // 读取已有 settings
    let existing = match tokio::fs::read_to_string(&path).await {
        Ok(content) => serde_json::from_str::<Value>(&content).unwrap_or(json!({})),
        Err(_) => json!({}),
    };

    // 合并: changes 的属性覆盖到 existing
    let merged = deep_merge(existing, changes);

    // 确保父目录存在
    if let Some(parent) = path.parent() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            tracing::error!(path = %parent.display(), error = %e, "failed to create settings dir");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Failed to create settings directory" })),
            )
                .into_response();
        }
    }

    // 以 tmp + rename 方式原子写入
    let tmp_path = {
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let suffix = format!("{}-{}", pid, nanos);
        path.with_extension(format!("json.tmp-{}", suffix))
    };

    let content = serde_json::to_string_pretty(&merged).unwrap_or_default();
    match tokio::fs::write(&tmp_path, &content).await {
        Ok(_) => {
            match tokio::fs::rename(&tmp_path, &path).await {
                Ok(_) => Json(merged).into_response(),
                Err(e) => {
                    tracing::error!(from = %tmp_path.display(), to = %path.display(), error = %e, "rename failed");
                    let _ = tokio::fs::remove_file(&tmp_path).await;
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({ "error": "Failed to write settings" })),
                    )
                        .into_response()
                }
            }
        }
        Err(e) => {
            tracing::error!(path = %tmp_path.display(), error = %e, "write failed");
            let _ = tokio::fs::remove_file(&tmp_path).await;
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Failed to write settings" })),
            )
                .into_response()
        }
    }
}

/// GET /api/behavior/agents-md
pub async fn get_agents_md(
    State(_state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let path = agents_md_path();
    match tokio::fs::read_to_string(&path).await {
        Ok(content) => Json(json!({
            "content": content,
            "exists": true,
        }))
        .into_response(),
        Err(_) => Json(json!({
            "content": "",
            "exists": false,
        }))
        .into_response(),
    }
}

/// PUT /api/behavior/agents-md
pub async fn put_agents_md(
    State(_state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    const MAX_SIZE: usize = 256 * 1024; // 256KB

    let content = body
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    if content.len() > MAX_SIZE {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({ "error": "Content exceeds maximum size" })),
        )
            .into_response();
    }

    let path = agents_md_path();

    // 确保父目录存在
    if let Some(parent) = path.parent() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            tracing::error!(path = %parent.display(), error = %e, "failed to create agents-md dir");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Failed to create directory" })),
            )
                .into_response();
        }
    }

    match tokio::fs::write(&path, &content).await {
        Ok(_) => Json(json!({ "success": true })).into_response(),
        Err(e) => {
            tracing::error!(path = %path.display(), error = %e, "failed to write AGENTS.md");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Failed to write AGENTS.md" })),
            )
                .into_response()
        }
    }
}

/// 深度合并: b 中的字段覆盖到 a。
/// 遇到嵌套对象时递归合并 (非对象字段直接覆盖)。
fn deep_merge(a: Value, b: Value) -> Value {
    match (a, b) {
        (Value::Object(mut a_map), Value::Object(b_map)) => {
            for (k, v) in b_map {
                if let Some(existing) = a_map.remove(&k) {
                    a_map.insert(k, deep_merge(existing, v));
                } else {
                    a_map.insert(k, v);
                }
            }
            Value::Object(a_map)
        }
        (_a, b) => b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::paths::home_env_var_name;
    use serde_json::json;

    /// 与其它模块一致的 home env 测试互斥锁 + 跨平台 env var 设置。
    ///
    /// `home_dir_string()` 在 Windows 读 `USERPROFILE`、其他平台读 `HOME`,
    /// 必须按 [`home_env_var_name`] 设置/还原,否则 Windows 上只设 `HOME` 不生效。
    struct HomeGuard {
        prev: Option<String>,
        var: &'static str,
    }
    impl Drop for HomeGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var(self.var, v),
                None => std::env::remove_var(self.var),
            }
        }
    }

    static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn set_temp_home() -> (HomeGuard, std::sync::MutexGuard<'static, ()>) {
        let guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let var = home_env_var_name();
        let prev = std::env::var(var).ok();
        let tmp = std::env::temp_dir().join(format!(
            "oc-behavior-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        std::env::set_var(var, tmp.to_string_lossy().to_string());
        (HomeGuard { prev, var }, guard)
    }

    #[test]
    fn resolve_and_legacy_paths_use_distinct_brand_dirs() {
        let (_guard, _lock) = set_temp_home();
        let main = resolve_settings_path();
        let legacy = legacy_settings_path();
        let main_s = main.to_string_lossy().to_lowercase().replace('\\', "/");
        let legacy_s = legacy.to_string_lossy().to_lowercase().replace('\\', "/");
        assert!(main_s.contains("gridforge/settings.json"), "main = {main_s}");
        assert!(
            legacy_s.contains("openchamber/settings.json"),
            "legacy = {legacy_s}"
        );
    }

    #[test]
    fn gridforge_data_dir_overrides_default_path() {
        let (_guard, _lock) = set_temp_home();
        std::env::set_var("GRIDFORGE_DATA_DIR", "/custom/data");
        let path = resolve_settings_path();
        std::env::remove_var("GRIDFORGE_DATA_DIR");
        let s = path.to_string_lossy().to_lowercase().replace('\\', "/");
        assert!(s.ends_with("/custom/data/settings.json"), "path = {s}");
    }

    #[test]
    fn deep_merge_preserves_base_fields_and_overrides_from_b() {
        // a (旧品牌) 有 projects; b (主品牌) 有 theme 但无 projects
        let a = json!({
            "projects": [{"id": "p1", "path": "/x"}],
            "themeId": "old",
            "shared": { "x": 1, "y": 2 },
        });
        let b = json!({
            "themeId": "new",
            "splashBgDark": "#000",
            "shared": { "y": 20, "z": 30 },
        });
        let merged = deep_merge(a, b);
        // b 覆盖 a 的同名字段
        assert_eq!(merged["themeId"], "new");
        assert_eq!(merged["splashBgDark"], "#000");
        // a 独有字段保留
        assert!(merged["projects"].is_array());
        assert_eq!(merged["projects"][0]["id"], "p1");
        // 嵌套对象递归合并
        assert_eq!(merged["shared"]["x"], 1);
        assert_eq!(merged["shared"]["y"], 20);
        assert_eq!(merged["shared"]["z"], 30);
    }

    #[tokio::test]
    async fn get_settings_merges_legacy_projects_with_current_brand() {
        let (_guard, _lock) = set_temp_home();
        let home = PathBuf::from(std::env::var(home_env_var_name()).unwrap());
        let legacy_dir = home.join(".config").join("openchamber");
        let main_dir = home.join(".config").join("gridforge");
        std::fs::create_dir_all(&legacy_dir).unwrap();
        std::fs::create_dir_all(&main_dir).unwrap();

        // 旧品牌: 含 projects (历史用户数据)
        std::fs::write(
            legacy_dir.join("settings.json"),
            json!({
                "projects": [{"id": "p1", "path": "E:/demo"}],
                "activeProjectId": "p1",
                "themeId": "legacy-theme",
            })
            .to_string(),
        )
        .unwrap();
        // 主品牌: 不含 projects, 但 themeId 更新
        std::fs::write(
            main_dir.join("settings.json"),
            json!({ "themeId": "gridforge-theme", "splashBgDark": "#111" })
                .to_string(),
        )
        .unwrap();

        let merged = read_merged_settings().await;

        // 旧品牌的 projects 被保留
        assert!(merged["projects"].is_array(), "merged = {merged}");
        assert_eq!(merged["projects"][0]["id"], "p1");
        assert_eq!(merged["activeProjectId"], "p1");
        // 主品牌覆盖 themeId, 并贡献 splashBgDark
        assert_eq!(merged["themeId"], "gridforge-theme");
        assert_eq!(merged["splashBgDark"], "#111");
    }

    #[tokio::test]
    async fn get_settings_falls_back_to_legacy_when_main_missing() {
        let (_guard, _lock) = set_temp_home();
        let home = PathBuf::from(std::env::var(home_env_var_name()).unwrap());
        let legacy_dir = home.join(".config").join("openchamber");
        std::fs::create_dir_all(&legacy_dir).unwrap();
        std::fs::write(
            legacy_dir.join("settings.json"),
            json!({ "projects": [{"id": "only"}] }).to_string(),
        )
        .unwrap();
        // 不创建 gridforge/settings.json

        let merged = read_merged_settings().await;
        assert_eq!(merged["projects"][0]["id"], "only");
    }
}
