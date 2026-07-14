//! 静态 dist 托管 + SPA fallback。
//!
//! 对应现有: `packages/web/server/lib/opencode/static-routes-runtime.js`。
//!
//! 职责:
//!   1. 托管 React dist 目录 (`tower_http::services::ServeDir`)
//!   2. `sw.js` → `Cache-Control: no-store`
//!   3. SPA fallback: 非 `/api`、非静态资源扩展名 → `index.html`
//!   4. `--api-only` 模式或 dist 不存在 → headless JSON 响应

use std::path::Path;
use std::path::PathBuf;

use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::json;
use tower::Service;
use tower_http::services::ServeDir;

/// 静态资源扩展名 (这些路径不会被 SPA fallback 拦截)。
const STATIC_EXTENSIONS: &[&str] = &[
    "js", "css", "svg", "png", "jpg", "jpeg", "gif", "ico", "woff", "woff2", "ttf", "eot", "map",
];

/// 判断路径是否指向静态资源文件 (有已知扩展名)。
fn is_static_asset(path: &str) -> bool {
    let path = path.split('?').next().unwrap_or(path);
    STATIC_EXTENSIONS.iter().any(|ext| path.ends_with(&format!(".{}", ext)))
}

/// 判断路径是否以 `/api` 开头 (不应被 SPA fallback 拦截)。
fn is_api_path(path: &str) -> bool {
    path == "/api" || path.starts_with("/api/")
}

/// 构建 dist 托管服务。
///
/// 如果 dist 目录存在, 返回 `ServeDir` 用于 axum 路由。
/// 如果不存在或 api_only, 返回 None (调用方走 headless fallback)。
pub fn build_dist_service(dist_dir: &Path) -> Option<ServeDir> {
    if !dist_dir.exists() || !dist_dir.is_dir() {
        return None;
    }
    Some(ServeDir::new(dist_dir))
}

/// SPA fallback service: 尝试返回 `index.html`。
///
/// 这是一个 tower Service (不使用 axum extractor), 用于 `ServeDir::fallback()`。
/// 它直接读取 `index.html` 文件返回, 不需要 AppState。
#[derive(Clone)]
pub struct SpaFallback {
    index_html: Option<PathBuf>,
}

impl SpaFallback {
    pub fn new(dist_dir: &Path) -> Self {
        let index = dist_dir.join("index.html");
        if index.exists() {
            Self { index_html: Some(index) }
        } else {
            Self { index_html: None }
        }
    }
}

impl Service<Request> for SpaFallback {
    type Response = axum::response::Response;
    type Error = std::convert::Infallible;
    type Future = std::pin::Pin<Box<dyn std::future::Future<Output = std::result::Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut std::task::Context<'_>) -> std::task::Poll<std::result::Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request) -> Self::Future {
        let index_html = self.index_html.clone();
        Box::pin(async move {
            let path = req.uri().path();

            // 非 SPA 路径 (API 或静态资源) → 404
            if is_api_path(path) || is_static_asset(path) {
                return Ok((StatusCode::NOT_FOUND, "Not found").into_response());
            }

            // 尝试返回 index.html
            if let Some(ref index) = index_html {
                if let Ok(content) = tokio::fs::read(index).await {
                    return Ok((
                        StatusCode::OK,
                        [("content-type", "text/html; charset=utf-8")],
                        content,
                    )
                        .into_response());
                }
            }

            // dist 不存在 → headless 模式
            Ok((
                StatusCode::NOT_FOUND,
                axum::Json(json!({
                    "ok": false,
                    "mode": "api-only",
                    "error": "Static files not found.",
                })),
            )
                .into_response())
        })
    }
}

/// headless 模式 fallback (当 api_only 或 dist 不存在时)。
pub async fn headless_fallback() -> impl IntoResponse {
    (
        StatusCode::OK,
        axum::Json(json!({
            "ok": true,
            "mode": "api-only",
            "message": "OpenChamber is running in API-only mode. No static files served.",
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_asset_detection() {
        assert!(is_static_asset("/assets/index.js"));
        assert!(is_static_asset("/assets/index.css"));
        assert!(is_static_asset("/favicon.ico"));
        assert!(is_static_asset("/sw.js"));
    }

    #[test]
    fn non_static_asset() {
        assert!(!is_static_asset("/"));
        assert!(!is_static_asset("/dashboard"));
        assert!(!is_static_asset("/session/abc123"));
        assert!(!is_static_asset("/api/health"));
    }

    #[test]
    fn api_path_detection() {
        assert!(is_api_path("/api"));
        assert!(is_api_path("/api/session"));
        assert!(is_api_path("/api/global/event"));
        assert!(!is_api_path("/dashboard"));
        assert!(!is_api_path("/health"));
    }

    #[test]
    fn build_dist_service_returns_none_for_missing_dir() {
        let result = build_dist_service(Path::new("/nonexistent/path/that/does/not/exist"));
        assert!(result.is_none());
    }
}
