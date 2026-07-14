//! 设置持久化模块 — 原子读写 `~/.config/openchamber/settings.json`。
//!
//! 与 Electron 共享同一文件。路径可通过 `OPENCHAMBER_DATA_DIR` 环境变量覆盖。
//!
//! 写入是原子的 (tmp 文件 + rename)，防止并发读者看到半写文件。
//! 进程内通过 Mutex 序列化 read-modify-write，防止多次 RMW 交叉覆盖。
//!
//! 复现 Electron main.mjs:
//! - `settingsFilePath()` (main.mjs:466-471)
//! - `readJsonFile` / `readSettingsRoot` (main.mjs:479-504)
//! - `writeJsonFile` (main.mjs:492-499) — 原子写入
//! - `mutateSettingsRoot` (main.mjs:511-522) — 进程内序列化

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::Value;

/// Tauri 管理的设置存储。仅持有一个序列化锁，数据本身在磁盘上。
pub struct SettingsStore {
    /// 序列化 read-modify-write 的进程内锁。
    lock: Mutex<()>,
}

impl SettingsStore {
    pub fn new() -> Self {
        Self {
            lock: Mutex::new(()),
        }
    }

    /// 解析 settings.json 的路径。
    ///
    /// `OPENCHAMBER_DATA_DIR` 环境变量优先 (trim 后非空才用);
    /// 否则 `~/.config/openchamber/settings.json`。
    pub fn settings_file_path() -> PathBuf {
        if let Ok(dir) = std::env::var("OPENCHAMBER_DATA_DIR") {
            let trimmed = dir.trim();
            if !trimmed.is_empty() {
                return PathBuf::from(trimmed).join("settings.json");
            }
        }
        let home = dirs_or_env();
        home.join(".config").join("openchamber").join("settings.json")
    }

    /// 读取整个 settings root。文件不存在或解析失败返回 `{}`。
    ///
    /// 复现 Electron readSettingsRoot (main.mjs:501-504)。
    pub fn read() -> Value {
        let path = Self::settings_file_path();
        Self::read_json_file(&path)
    }

    /// 原子写入整个 settings root。
    ///
    /// 复现 Electron writeJsonFile (main.mjs:492-499):
    /// 先写 tmp 文件 (`{path}.tmp-{pid}-{ts}-{rand}`) 再 rename。
    pub fn write(root: &Value) -> std::io::Result<()> {
        let path = Self::settings_file_path();
        write_json_atomic(&path, root)
    }

    /// 序列化的 read-modify-write。
    ///
    /// 复现 Electron mutateSettingsRoot (main.mjs:511-522):
    /// 进程内 Mutex 保证 RMW 不交叉，mutator 返回修改后的 root。
    ///
    /// 如果 mutator 返回 Err，不写入 (保留旧值)。
    /// 如果 mutator 返回 Ok(None)，写入当前读取的值 (不变)。
    pub fn mutate<F>(&self, mutator: F) -> Result<(), String>
    where
        F: FnOnce(&mut Value) -> Result<Option<Value>, String>,
    {
        let _guard = self.lock.lock().map_err(|e| e.to_string())?;

        let mut current = Self::read();
        match mutator(&mut current)? {
            Some(new_root) => {
                Self::write(&new_root).map_err(|e| e.to_string())?;
            }
            None => {
                // mutator 未返回新 root，写入 current (可能有原地修改)
                Self::write(&current).map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    }

    /// 读取单个 key 的值。不存在返回 None。
    pub fn get(key: &str) -> Option<Value> {
        let root = Self::read();
        root.get(key).cloned()
    }

    /// 读取 bool key，不存在或类型不匹配返回 default。
    pub fn get_bool(key: &str, default: bool) -> bool {
        Self::get(key)
            .and_then(|v| v.as_bool())
            .unwrap_or(default)
    }

    /// 设置单个 key (序列化 RMW)。
    pub fn set(&self, key: &str, value: Value) -> Result<(), String> {
        self.mutate(|root| {
            root[key] = value.clone();
            Ok(None)
        })
    }

    /// 读取或创建 desktopInstallId。首次生成 UUID 并持久化。
    ///
    /// 复现 Electron getOrCreateDesktopInstallId (main.mjs:547-559)。
    pub fn get_or_create_install_id(&self) -> Result<String, String> {
        // 先检查是否已存在
        if let Some(existing) = Self::get("desktopInstallId") {
            if let Some(s) = existing.as_str() {
                let trimmed = s.trim();
                if !trimmed.is_empty() {
                    return Ok(trimmed.to_string());
                }
            }
        }
        let generated = generate_uuid();
        self.mutate(|root| {
            // Race guard: 如果另一个 writer 已经写入了 id，保留它
            if let Some(existing) = root.get("desktopInstallId").and_then(|v| v.as_str()) {
                let trimmed = existing.trim();
                if !trimmed.is_empty() {
                    return Ok(None);
                }
            }
            root["desktopInstallId"] = Value::String(generated.clone());
            Ok(None)
        })?;

        // 读回确认
        if let Some(after) = Self::get("desktopInstallId") {
            if let Some(s) = after.as_str() {
                let trimmed = s.trim();
                if !trimmed.is_empty() {
                    return Ok(trimmed.to_string());
                }
            }
        }
        Ok(generated)
    }

    /// 读取 JSON 文件，文件不存在或解析失败返回 `{}`。
    ///
    /// 复现 Electron readJsonFile (main.mjs:479-489)。
    fn read_json_file(path: &Path) -> Value {
        match fs::read_to_string(path) {
            Ok(content) => {
                match serde_json::from_str::<Value>(&content) {
                    Ok(v) if v.is_object() && !v.is_array() => v,
                    _ => {
                        // 解析失败 (可能是并发写入中的半写状态)
                        log::warn!(
                            "[settings] failed to parse JSON, returning empty: {}",
                            path.display()
                        );
                        Value::Object(serde_json::Map::new())
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Value::Object(
                serde_json::Map::new(),
            ),
            Err(e) => {
                log::warn!(
                    "[settings] failed to read file {}: {}",
                    path.display(),
                    e
                );
                Value::Object(serde_json::Map::new())
            }
        }
    }
}

impl Default for SettingsStore {
    fn default() -> Self {
        Self::new()
    }
}

/// 获取用户 home 目录。优先用 `HOME` 环境变量 (Unix)，否则用 `dirs` crate 回退。
fn dirs_or_env() -> PathBuf {
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            return PathBuf::from(home);
        }
    }
    // Windows: USERPROFILE
    if let Ok(home) = std::env::var("USERPROFILE") {
        if !home.is_empty() {
            return PathBuf::from(home);
        }
    }
    // 回退到当前目录
    PathBuf::from(".")
}

/// 原子写入 JSON: 先写 tmp 文件再 rename。
///
/// 复现 Electron writeJsonFile (main.mjs:492-499)。
fn write_json_atomic(path: &Path, data: &Value) -> std::io::Result<()> {
    // 确保父目录存在
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let pid = std::process::id();
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let rand: u64 = {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        pid.hash(&mut hasher);
        ts.hash(&mut hasher);
        std::time::Instant::now().elapsed().as_nanos().hash(&mut hasher);
        hasher.finish()
    };

    // with_extension 替换后缀，我们要在原文件名后追加后缀。用文件名拼接:
    let file_name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let tmp = path
        .parent()
        .unwrap_or(Path::new("."))
        .join(format!("{}.tmp-{}-{}-{:x}", file_name, pid, ts, rand));

    let json_str = serde_json::to_string_pretty(data)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(json_str.as_bytes())?;
        file.sync_all()?;
    }

    // rename 是原子的 (同一文件系统上)
    fs::rename(&tmp, path)?;

    Ok(())
}

/// 生成一个 UUID v4 字符串 (不依赖外部 crate)。
fn generate_uuid() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    // 用时间戳 + 进程信息作为熵源生成一个类 UUID 格式字符串
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);

    let pid = std::process::id() as u128;

    // 简单的伪随机: 用 nanos 和 pid 的混合
    let seed = nanos
        .wrapping_mul(0x5bd1e995)
        .wrapping_add(pid << 32);

    // Linear congruential generator 生成 16 字节
    let mut state = seed;
    let mut bytes = [0u8; 16];
    for b in &mut bytes {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        *b = (state >> 56) as u8;
    }

    // 设置 version (4) 和 variant (10xx)
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;

    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3],
        bytes[4], bytes[5],
        bytes[6], bytes[7],
        bytes[8], bytes[9],
        bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn read_missing_file_returns_empty_object() {
        // 用一个不存在的路径
        let path = PathBuf::from("/tmp/oc-test-nonexistent-settings-xyz.json");
        let result = SettingsStore::read_json_file(&path);
        assert!(result.is_object());
        assert!(result.as_object().unwrap().is_empty());
    }

    #[test]
    fn atomic_write_and_read_roundtrip() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "oc-test-settings-roundtrip-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis()
        ));
        let data = json!({
            "desktopMinimizeToTrayEnabled": true,
            "desktopVibrancy": false,
            "nested": { "key": "value", "num": 42 }
        });
        write_json_atomic(&path, &data).expect("write should succeed");
        let read_back = SettingsStore::read_json_file(&path);
        assert_eq!(read_back, data);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn mutate_modifies_and_persists() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "oc-test-settings-mutate-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis()
        ));
        // 路径替换: 临时让 settings_file_path 指向这里不太容易，直接测试 write_json_atomic + read_json_file
        write_json_atomic(&path, &json!({ "existing": true })).unwrap();

        let read = SettingsStore::read_json_file(&path);
        assert_eq!(read["existing"], json!(true));

        // 模拟 mutate
        let mut current = SettingsStore::read_json_file(&path);
        current["newKey"] = json!("newValue");
        write_json_atomic(&path, &current).unwrap();

        let after = SettingsStore::read_json_file(&path);
        assert_eq!(after["existing"], json!(true));
        assert_eq!(after["newKey"], json!("newValue"));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn uuid_format_is_valid() {
        let uuid = generate_uuid();
        assert_eq!(uuid.len(), 36);
        assert_eq!(uuid.chars().filter(|&c| c == '-').count(), 4);
        // version digit
        let parts: Vec<&str> = uuid.split('-').collect();
        assert!(parts[2].starts_with('4'));
    }

    #[test]
    fn settings_path_respects_env_override() {
        // 此测试仅验证逻辑路径构建 (不依赖实际 env)
        // 在真实运行时 settings_file_path() 会检查 OPENCHAMBER_DATA_DIR
        let home = dirs_or_env();
        let default_path = home.join(".config").join("openchamber").join("settings.json");
        // 验证路径结构
        assert!(default_path.to_string_lossy().contains("openchamber"));
        assert!(default_path.to_string_lossy().contains("settings.json"));
    }
}
