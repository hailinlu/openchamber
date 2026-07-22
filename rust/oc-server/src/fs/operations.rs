//! 文件操作: read / write / delete / rename / mkdir / stat / home / list / reveal。
//!
//! 对应 `packages/web/server/lib/fs/routes.js` 各 handler 中的文件操作逻辑。

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{json, Value};

/// 把路径转成返回给客户端的字符串, 剥离 Windows verbatim 前缀
/// (`\\?\` / `\\.\`)。
///
/// 背景: `std::fs::canonicalize` 在 Windows 上会返回带 `\\?\` 前缀的
/// "verbatim" 路径。Node 的 `fs.realpath` 不会加这个前缀。如果把这个
/// 前缀泄漏给客户端 (例如 `/api/fs/list` 的 `entries[].path`), UI 的
/// `isPathWithinRoot` 比对会失败 —— `\\?\E:\...` 永远不会被认为在
/// `E:\...` 工作区内, 于是被误判为 "工作区外", 触发
/// `allowOutsideWorkspace=true` 但没有 grant →
/// "Outside file grant is required"。
///
/// 仅用于 *输出给客户端* 的路径; 内部边界校验仍用 canonical 路径。
fn to_client_path(path: &Path) -> String {
    let s = path.to_string_lossy().to_string();
    strip_verbatim_prefix(&s)
}

/// 剥离 Windows verbatim/设备命名空间前缀。
/// 在非 Windows 平台是 no-op (编译期消除)。
fn strip_verbatim_prefix(s: &str) -> String {
    #[cfg(windows)]
    {
        // verbatim 前缀 `\\?\` (含 UNC 形式 `\\?\UNC\`) 与设备命名空间 `\\.\`
        // Path::strip_prefix 无法处理这些 (它们不是合法的 Path 组件前缀),
        // 所以用字符串匹配。
        if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
            return format!(r"\\{rest}");
        }
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            return rest.to_string();
        }
        if let Some(rest) = s.strip_prefix(r"\\.\") {
            return rest.to_string();
        }
        s.to_string()
    }
    #[cfg(not(windows))]
    {
        s.to_string()
    }
}

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
                path: to_client_path(path),
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
///
/// 编码策略与 Node 版 `packages/web/server/lib/fs/routes.js` 一致:
/// 先按 UTF-8 (`fatal`) 解码, 失败再回退到 GBK (CJK, Windows 常见)。
/// 这避免了 `read_to_string` 对非 UTF-8 文件直接抛 "stream did not
/// contain valid UTF-8" → 500 "Failed to open file" 的问题。
pub async fn read(path: &Path) -> Result<String, oc_core::Error> {
    match tokio::fs::read(path).await {
        Ok(bytes) => Ok(decode_text(&bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Err(oc_core::Error::NotFound(format!(
                "File not found: {}",
                path.display()
            )))
        }
        Err(e) => Err(oc_core::Error::Io(e)),
    }
}

/// 解码文件字节: UTF-8 优先, 失败回退 GBK (对应 Node 的两次 TextDecoder)。
fn decode_text(bytes: &[u8]) -> String {
    // 先试 UTF-8 strict — 纯 UTF-8 文件零开销命中此路径。
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => {
            // 回退 GBK。encoding_rs 的 decode 无致命错误,
            // 无法识别的字节会被替换为 U+FFFD (与 Node TextDecoder 行为一致)。
            let (cow, _, _) = encoding_rs::GBK.decode(bytes);
            cow.into_owned()
        }
    }
}

/// `POST /api/fs/write` — 原子写入。
///
/// 步骤:
///   1. 如果内容相同, 跳过 (no-op success)
///   2. 写入 .tmp 文件
///   3. rename 到目标
pub async fn write(path: &Path, content: &str) -> Result<Value, oc_core::Error> {
    // 检查现有内容 (按字节比较, 避免非 UTF-8 文件触发 read_to_string 失败)
    if let Ok(existing) = tokio::fs::read(path).await {
        if existing.as_slice() == content.as_bytes() {
            return Ok(json!({ "success": true, "path": to_client_path(path) }));
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

    Ok(json!({ "success": true, "path": to_client_path(path) }))
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
    Ok(json!({ "success": true, "path": to_client_path(path) }))
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
    Ok(json!({ "success": true, "path": to_client_path(new_path) }))
}

/// `POST /api/fs/mkdir`
pub async fn mkdir(path: &Path) -> Result<Value, oc_core::Error> {
    tokio::fs::create_dir_all(path)
        .await
        .map_err(oc_core::Error::Io)?;
    Ok(json!({ "success": true, "path": to_client_path(path) }))
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
            path: to_client_path(&entry_path),
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
        "path": to_client_path(path),
        "entries": entries,
    }))
}

/// `POST /api/fs/reveal` — 在文件管理器中显示文件。
pub async fn reveal(path: &Path) -> Result<Value, oc_core::Error> {
    let result = reveal_in_file_manager(path).await;
    match result {
        Ok(()) => Ok(json!({ "success": true, "path": to_client_path(path) })),
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

    /// GBK 编码的中文文件必须能被读取, 而不是抛 "stream did not contain
    /// valid UTF-8"。这是 Windows 文件预览 "Failed to open file" 的根因之一。
    #[tokio::test]
    async fn read_gbk_encoded_file_decodes_without_error() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("oc-test-gbk-{}.txt", rand::random::<u32>()));

        // 用 encoding_rs 把 "中文测试" 编码成 GBK 字节, 避免手写字节错误。
        // 这些字节不是合法 UTF-8, 会触发旧 read_to_string 的 InvalidData。
        let gbk_bytes = encoding_rs::GBK.encode("中文测试").0;
        let gbk_bytes: &[u8] = &gbk_bytes;
        // 确认它确实不是合法 UTF-8 (否则测试无意义)。
        assert!(
            std::str::from_utf8(gbk_bytes).is_err(),
            "test fixture must be non-UTF-8 to exercise the fallback"
        );
        tokio::fs::write(&path, gbk_bytes)
            .await
            .expect("write gbk file");

        let content = read(&path).await.expect("gbk read should succeed");
        assert_eq!(content, "中文测试");

        tokio::fs::remove_file(&path).await.ok();
    }

    /// 纯 UTF-8 文件走 fast path, 内容原样返回。
    #[tokio::test]
    async fn read_utf8_file_decodes_verbatim() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("oc-test-utf8-{}.txt", rand::random::<u32>()));
        let content = "hello 世界 🌍";
        tokio::fs::write(&path, content)
            .await
            .expect("write utf8 file");

        let read_content = read(&path).await.expect("utf8 read");
        assert_eq!(read_content, content);

        tokio::fs::remove_file(&path).await.ok();
    }

    /// decode_text 单元测试: 确认回退逻辑不依赖文件系统。
    #[test]
    fn decode_text_gbk_bytes() {
        // GBK 编码 "A中" → 非合法 UTF-8, 走 GBK 回退。
        let gbk_bytes = encoding_rs::GBK.encode("A中").0;
        assert_eq!(decode_text(&gbk_bytes), "A中");
    }

    #[test]
    fn decode_text_utf8_bytes() {
        assert_eq!(decode_text(b"plain ascii"), "plain ascii");
        assert_eq!(decode_text("utf8 中文".as_bytes()), "utf8 中文");
    }

    /// Windows 上 `std::fs::canonicalize` 会给路径加 `\\?\` 前缀。
    /// 返回给客户端的路径必须剥离这个前缀, 否则 UI 的 `isPathWithinRoot`
    /// 会把工作区内文件误判为 "工作区外" → "Outside file grant is required"。
    #[test]
    fn strip_verbatim_prefix_removes_windows_prefixes() {
        assert_eq!(
            strip_verbatim_prefix(r"\\?\C:\foo\bar"),
            r"C:\foo\bar"
        );
        // UNC verbatim 形式 → 还原为普通 UNC
        assert_eq!(
            strip_verbatim_prefix(r"\\?\UNC\server\share\foo"),
            r"\\server\share\foo"
        );
        // 设备命名空间
        assert_eq!(
            strip_verbatim_prefix(r"\\.\COM1"),
            r"COM1"
        );
        // 没有前缀的路径原样返回
        assert_eq!(
            strip_verbatim_prefix(r"C:\foo\bar"),
            r"C:\foo\bar"
        );
    }

    #[test]
    fn to_client_path_strips_verbatim() {
        let p = Path::new(r"\\?\C:\proj\src\main.rs");
        assert_eq!(to_client_path(p), r"C:\proj\src\main.rs");
    }
}
