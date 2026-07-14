//! 文件操作: read / write / delete / rename / mkdir / stat / home / list / reveal。
//!
//! 对应 `packages/web/server/lib/fs/routes.js` 各 handler 中的文件操作逻辑。

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{json, Value};

/// `GET /api/fs/home` → `{ home }`
pub fn home_dir() -> Value {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| "/".to_string());
    json!({ "home": home })
}

/// stat 结果。
#[derive(Serialize)]
pub struct StatResult {
    pub path: String,
    #[serde(rename = "isFile")]
    pub is_file: bool,
    pub size: u64,
    #[serde(rename = "mtimeMs")]
    pub mtime_ms: f64,
}

/// stat 可选结果 (optional 模式, 文件不存在时)。
#[allow(dead_code)]
#[derive(Serialize)]
pub struct StatOptional {
    pub path: String,
    pub exists: bool,
}

/// `GET /api/fs/stat` — 文件元信息。
pub async fn stat(path: &Path) -> Result<Value, oc_core::Error> {
    match tokio::fs::metadata(path).await {
        Ok(meta) => {
            let mtime_ms = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as f64)
                .unwrap_or(0.0);

            Ok(json!(StatResult {
                path: path.to_string_lossy().to_string(),
                is_file: meta.is_file(),
                size: meta.len(),
                mtime_ms,
            }))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Err(oc_core::Error::NotFound(format!(
                "File not found: {}",
                path.display()
            )))
        }
        Err(e) => Err(oc_core::Error::Io(e)),
    }
}

/// `GET /api/fs/read` — 读取文本文件。
pub async fn read(path: &Path) -> Result<String, oc_core::Error> {
    match tokio::fs::read_to_string(path).await {
        Ok(content) => Ok(content),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Err(oc_core::Error::NotFound(format!(
                "File not found: {}",
                path.display()
            )))
        }
        Err(e) => Err(oc_core::Error::Io(e)),
    }
}

/// `POST /api/fs/write` — 原子写入。
///
/// 步骤:
///   1. 如果内容相同, 跳过 (no-op success)
///   2. 写入 .tmp 文件
///   3. rename 到目标
pub async fn write(path: &Path, content: &str) -> Result<Value, oc_core::Error> {
    // 检查现有内容
    if let Ok(existing) = tokio::fs::read_to_string(path).await {
        if existing == content {
            return Ok(json!({ "success": true, "path": path.to_string_lossy() }));
        }
    }

    // 原子写: .tmp → rename
    let tmp_path = format!(
        "{}.tmp-{}-{}-{}",
        path.to_string_lossy(),
        std::process::id(),
        chrono::Utc::now().timestamp_millis(),
        rand::random::<u32>()
    );
    let tmp = PathBuf::from(&tmp_path);

    tokio::fs::write(&tmp, content)
        .await
        .map_err(oc_core::Error::Io)?;

    tokio::fs::rename(&tmp, path)
        .await
        .map_err(|e| {
            // 清理 tmp 文件
            let tmp_clone = tmp.clone();
            tokio::spawn(async move {
                tokio::fs::remove_file(&tmp_clone).await.ok();
            });
            oc_core::Error::Io(e)
        })?;

    Ok(json!({ "success": true, "path": path.to_string_lossy() }))
}

/// `POST /api/fs/delete`
pub async fn delete(path: &Path) -> Result<Value, oc_core::Error> {
    let meta = tokio::fs::metadata(path).await;
    match meta {
        Ok(m) if m.is_dir() => {
            tokio::fs::remove_dir_all(path).await.map_err(oc_core::Error::Io)?;
        }
        Ok(_) => {
            tokio::fs::remove_file(path).await.map_err(oc_core::Error::Io)?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(oc_core::Error::NotFound(format!(
                "File not found: {}",
                path.display()
            )));
        }
        Err(e) => return Err(oc_core::Error::Io(e)),
    }
    Ok(json!({ "success": true, "path": path.to_string_lossy() }))
}

/// `POST /api/fs/rename`
pub async fn rename(old_path: &Path, new_path: &Path) -> Result<Value, oc_core::Error> {
    tokio::fs::rename(old_path, new_path)
        .await
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                oc_core::Error::NotFound(format!("File not found: {}", old_path.display()))
            } else {
                oc_core::Error::Io(e)
            }
        })?;
    Ok(json!({ "success": true, "path": new_path.to_string_lossy() }))
}

/// `POST /api/fs/mkdir`
pub async fn mkdir(path: &Path) -> Result<Value, oc_core::Error> {
    tokio::fs::create_dir_all(path)
        .await
        .map_err(oc_core::Error::Io)?;
    Ok(json!({ "success": true, "path": path.to_string_lossy() }))
}

/// 目录条目。
#[derive(Serialize)]
pub struct DirEntry {
    pub name: String,
    pub path: String,
    #[serde(rename = "isDirectory")]
    pub is_directory: bool,
    #[serde(rename = "isFile")]
    pub is_file: bool,
    #[serde(rename = "isSymbolicLink")]
    pub is_symbolic_link: bool,
}

/// `GET /api/fs/list` — 列出目录内容。
pub async fn list(path: &Path) -> Result<Value, oc_core::Error> {
    let mut entries = Vec::new();

    let mut reader = tokio::fs::read_dir(path).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            oc_core::Error::NotFound(format!("Directory not found: {}", path.display()))
        } else {
            oc_core::Error::Io(e)
        }
    })?;

    while let Some(entry) = reader.next_entry().await.map_err(oc_core::Error::Io)? {
        let file_type = match entry.file_type().await {
            Ok(ft) => ft,
            Err(_) => {
                let meta = match tokio::fs::metadata(entry.path()).await {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                meta.file_type()
            }
        };
        let name = entry.file_name().to_string_lossy().to_string();
        let entry_path = entry.path();

        entries.push(DirEntry {
            name: name.clone(),
            path: entry_path.to_string_lossy().to_string(),
            is_directory: file_type.is_dir(),
            is_file: file_type.is_file(),
            is_symbolic_link: file_type.is_symlink(),
        });
    }

    // 排序: 目录在前, 然后按名称
    entries.sort_by(|a, b| {
        match (a.is_directory, b.is_directory) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
        }
    });

    Ok(json!({
        "path": path.to_string_lossy(),
        "entries": entries,
    }))
}

/// `POST /api/fs/reveal` — 在文件管理器中显示文件。
pub async fn reveal(path: &Path) -> Result<Value, oc_core::Error> {
    let result = reveal_in_file_manager(path).await;
    match result {
        Ok(()) => Ok(json!({ "success": true, "path": path.to_string_lossy() })),
        Err(e) => Err(oc_core::Error::Internal(format!(
            "Failed to reveal: {}",
            e
        ))),
    }
}

/// 平台特定的 reveal 实现。
async fn reveal_in_file_manager(path: &Path) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        use tokio::process::Command;
        // 如果是目录, 用 open; 如果是文件, 用 open -R
        let is_dir = tokio::fs::metadata(path)
            .await
            .map(|m| m.is_dir())
            .unwrap_or(false);
        let mut cmd = Command::new("open");
        if is_dir {
            cmd.arg(path);
        } else {
            cmd.arg("-R").arg(path);
        }
        cmd.output()
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    #[cfg(target_os = "windows")]
    {
        use tokio::process::Command;
        Command::new("explorer.exe")
            .arg(path)
            .output()
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        use tokio::process::Command;
        Command::new("xdg-open")
            .arg(path)
            .output()
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", unix)))]
    {
        Err("unsupported platform".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn write_and_read() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("oc-test-write-{}", rand::random::<u32>()));
        let content = "hello world";

        // write
        let result = write(&path, content).await.unwrap();
        assert_eq!(result["success"], true);

        // read back
        let read_content = read(&path).await.unwrap();
        assert_eq!(read_content, content);

        // cleanup
        tokio::fs::remove_file(&path).await.ok();
    }

    #[tokio::test]
    async fn write_noop_same_content() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("oc-test-noop-{}", rand::random::<u32>()));
        let content = "same content";

        // 第一次写
        write(&path, content).await.unwrap();

        // 第二次写相同内容 — 也应该成功
        let result = write(&path, content).await.unwrap();
        assert_eq!(result["success"], true);

        // cleanup
        tokio::fs::remove_file(&path).await.ok();
    }

    #[tokio::test]
    async fn read_nonexistent() {
        let path = Path::new("/nonexistent/file/that/does/not/exist.txt");
        let result = read(path).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn mkdir_and_delete() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("oc-test-mkdir-{}", rand::random::<u32>()));

        mkdir(&path).await.unwrap();
        assert!(path.exists());

        delete(&path).await.unwrap();
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn list_directory() {
        let dir = std::env::temp_dir();
        let result = list(&dir).await.unwrap();
        assert_eq!(result["path"], dir.to_string_lossy().to_string());
        assert!(result["entries"].is_array());
    }

    #[test]
    fn home_returns_value() {
        let result = home_dir();
        assert!(result["home"].is_string());
    }
}
