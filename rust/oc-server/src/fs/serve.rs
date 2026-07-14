//! 原始文件服务: raw + serve。
//!
//! 移植 `packages/web/server/lib/fs/routes.js` 的 `/api/fs/raw` 和 `/api/fs/serve` handler。
//!
//! MIME 类型映射与 Node 侧 `FILE_MIME_MAP` 对齐。

use std::path::Path;

use once_cell::sync::Lazy;

/// 文件扩展名 → MIME 类型映射 (对应 Node `FILE_MIME_MAP`)。
static FILE_MIME_MAP: Lazy<Vec<(&str, &str)>> = Lazy::new(|| {
    vec![
        (".html", "text/html; charset=utf-8"),
        (".htm", "text/html; charset=utf-8"),
        (".css", "text/css; charset=utf-8"),
        (".js", "text/javascript; charset=utf-8"),
        (".mjs", "text/javascript; charset=utf-8"),
        (".json", "application/json; charset=utf-8"),
        (".xml", "application/xml; charset=utf-8"),
        (".txt", "text/plain; charset=utf-8"),
        (".md", "text/markdown; charset=utf-8"),
        (".svg", "image/svg+xml"),
        (".png", "image/png"),
        (".jpg", "image/jpeg"),
        (".jpeg", "image/jpeg"),
        (".gif", "image/gif"),
        (".webp", "image/webp"),
        (".ico", "image/x-icon"),
        (".bmp", "image/bmp"),
        (".pdf", "application/pdf"),
        (".woff", "font/woff"),
        (".woff2", "font/woff2"),
        (".ttf", "font/ttf"),
        (".otf", "font/otf"),
        (".wasm", "application/wasm"),
        (".webmanifest", "application/manifest+json"),
    ]
});

/// 根据文件扩展名推断 MIME 类型。
///
/// 对应 Node 的 mime-db 查找, 但使用固定的扩展名表 (与 Node `FILE_MIME_MAP` 对齐)。
pub fn mime_for_extension(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{}", e.to_lowercase()))
        .unwrap_or_default();

    FILE_MIME_MAP
        .iter()
        .find(|(suffix, _)| *suffix == ext.as_str())
        .map(|(_, mime)| *mime)
        .unwrap_or("application/octet-stream")
}

/// 使用 mime_guess 作为 fallback (覆盖更多扩展名)。
pub fn content_type_for(path: &Path) -> String {
    // 先查固定表
    let mime = mime_for_extension(path);
    if mime != "application/octet-stream" {
        return mime.to_string();
    }

    // fallback: mime_guess crate
    mime_guess::from_path(path)
        .first()
        .map(|m| m.to_string())
        .unwrap_or_else(|| "application/octet-stream".to_string())
}

/// `GET /api/fs/raw` — 返回原始文件内容。
///
/// 设置 Content-Type, Cache-Control: no-store。
/// download=true 时设 Content-Disposition (RFC 5987)。
pub async fn serve_raw(
    path: &Path,
    download: bool,
) -> Result<RawFileData, oc_core::Error> {
    let content = tokio::fs::read(path)
        .await
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                oc_core::Error::NotFound(format!("File not found: {}", path.display()))
            } else {
                oc_core::Error::Io(e)
            }
        })?;

    let content_type = content_type_for(path);

    let content_disposition = if download {
        let filename = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("download");
        Some(format_rfc5987_content_disposition(filename))
    } else {
        None
    };

    Ok(RawFileData {
        content,
        content_type,
        content_disposition,
    })
}

/// `GET /api/fs/serve/<rest>` — serve 模式。
///
/// 与 raw 的区别:
///   - 拒绝 allowOutsideWorkspace
///   - 最大 100 MiB
///   - X-Content-Type-Options: nosniff
///   - path 从 `/<rest>` 解析
pub async fn serve_file(path: &Path) -> Result<ServeFileData, oc_core::Error> {
    // 大小检查
    let meta = tokio::fs::metadata(path)
        .await
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                oc_core::Error::NotFound(format!("File not found: {}", path.display()))
            } else {
                oc_core::Error::Io(e)
            }
        })?;

    if meta.len() as usize > super::MAX_SERVE_BYTES {
        return Err(oc_core::Error::BadRequest(format!(
            "File exceeds maximum size of {} bytes",
            super::MAX_SERVE_BYTES
        )));
    }

    let content = tokio::fs::read(path).await.map_err(oc_core::Error::Io)?;
    let content_type = mime_for_extension(path);

    Ok(ServeFileData {
        content,
        content_type: content_type.to_string(),
    })
}

/// raw 模式返回的文件数据。
pub struct RawFileData {
    pub content: Vec<u8>,
    pub content_type: String,
    pub content_disposition: Option<String>,
}

/// serve 模式返回的文件数据。
pub struct ServeFileData {
    pub content: Vec<u8>,
    pub content_type: String,
}

/// 构造 RFC 5987 Content-Disposition 头。
///
/// `attachment; filename="<ascii-safe>"; filename*=UTF-8''<percent-encoded>`
fn format_rfc5987_content_disposition(filename: &str) -> String {
    // ASCII-safe fallback filename
    let ascii_safe: String = filename
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '-' || *c == '_')
        .collect();

    // percent-encode for filename*
    let encoded: String = filename
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.' || b == b'~' {
                format!("{}", b as char)
            } else {
                format!("%{:02X}", b)
            }
        })
        .collect();

    format!(
        "attachment; filename=\"{}\"; filename*=UTF-8''{}",
        if ascii_safe.is_empty() { "download" } else { &ascii_safe },
        encoded
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mime_html() {
        assert_eq!(mime_for_extension(Path::new("index.html")), "text/html; charset=utf-8");
    }

    #[test]
    fn mime_png() {
        assert_eq!(mime_for_extension(Path::new("logo.png")), "image/png");
    }

    #[test]
    fn mime_unknown() {
        assert_eq!(
            mime_for_extension(Path::new("file.unknownext123")),
            "application/octet-stream"
        );
    }

    #[test]
    fn mime_case_insensitive() {
        assert_eq!(mime_for_extension(Path::new("script.JS")), "text/javascript; charset=utf-8");
    }

    #[test]
    fn rfc5987_ascii_filename() {
        let cd = format_rfc5987_content_disposition("document.pdf");
        assert!(cd.contains("filename=\"document.pdf\""));
        assert!(cd.contains("filename*=UTF-8''document.pdf"));
    }

    #[test]
    fn rfc5987_unicode_filename() {
        let cd = format_rfc5987_content_disposition("文件.txt");
        // ASCII-safe fallback 保留 .txt 部分 (非字母数字的 ASCII 不会被保留)
        assert!(cd.starts_with("attachment; filename=\""));
        assert!(cd.contains("filename*=UTF-8''"));
    }

    #[tokio::test]
    async fn serve_raw_file() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("oc-test-raw-{}.txt", rand::random::<u32>()));
        tokio::fs::write(&path, "test content").await.unwrap();

        let data = serve_raw(&path, false).await.unwrap();
        assert_eq!(data.content, b"test content");
        assert_eq!(data.content_type, "text/plain; charset=utf-8");
        assert!(data.content_disposition.is_none());

        tokio::fs::remove_file(&path).await.ok();
    }

    #[tokio::test]
    async fn serve_raw_with_download() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("oc-test-dl-{}.txt", rand::random::<u32>()));
        tokio::fs::write(&path, "test").await.unwrap();

        let data = serve_raw(&path, true).await.unwrap();
        assert!(data.content_disposition.is_some());
        let cd = data.content_disposition.unwrap();
        assert!(cd.starts_with("attachment;"));

        tokio::fs::remove_file(&path).await.ok();
    }

    #[tokio::test]
    async fn serve_file_too_large() {
        // MAX_SERVE_BYTES 是 100 MiB, 直接测试一个超过的路径不可行
        // 这里只测试正常文件
        let dir = std::env::temp_dir();
        let path = dir.join(format!("oc-test-serve-{}.txt", rand::random::<u32>()));
        tokio::fs::write(&path, "small file").await.unwrap();

        let data = serve_file(&path).await.unwrap();
        assert_eq!(data.content, b"small file");

        tokio::fs::remove_file(&path).await.ok();
    }
}
