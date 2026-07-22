//! 资源错误分类 + 导航策略。
//!
//! 对应 `classifyPreviewResourceError` + `classifyPreviewNavigation`
//! + `previewResourceNoiseRuleSets` (proxy-runtime.js:58-195)。

use url::Url;

/// 从 preview 资源 URL 提取 path+search (剥离 `/api/preview/proxy/<id>` 前缀)。
///
/// 对应 `parsePreviewResourcePath` (proxy-runtime.js:19-28)。
#[allow(dead_code)]
fn parse_preview_resource_path(url: &str) -> String {
    let parsed = match Url::parse(url).or_else(|_| Url::parse(&format!("http://localhost{}", url))) {
        Ok(p) => p,
        Err(_) => return url.to_string(),
    };
    let path = parsed.path();
    let re = regex::Regex::new(r"(?i)^/api/preview/proxy/[a-f0-9]{16,64}(/.*)?$").unwrap();
    let stripped = if re.is_match(path) {
        // 取 `/api/preview/proxy/<id>` 之后的部分
        let segments: Vec<&str> = path.splitn(5, '/').collect();
        // path = /api/preview/proxy/<hex>/<rest>
        // splitn(5, '/') → ["", "api", "preview", "proxy", "<hex>/<rest>"]
        if segments.len() >= 5 {
            let remainder = segments[4];
            // remainder 可能是 "<hex>/foo" 或 "<hex>"
            let after_hex = remainder.find('/').map(|i| &remainder[i..]).unwrap_or("");
            if after_hex.is_empty() {
                "/"
            } else {
                after_hex
            }
        } else {
            "/"
        }
    } else {
        path
    };
    let search = parsed
        .query()
        .map(|q| format!("?{}", q))
        .unwrap_or_default();
    format!("{}{}", stripped, search)
}

/// 判断 dev-server 资源加载失败是否属于噪音 (应抑制而非上报)。
///
/// 对应 `classifyPreviewResourceError` (proxy-runtime.js:129-141)。
/// 返回 `"suppress"` (Vite/Astro/Next/SvelteKit/Remix/Nuxt/Webpack dev-server 噪音)
/// 或 `"report"` (普通应用资源)。
#[allow(dead_code)]
pub fn classify_preview_resource_error(tag_name: &str, url: &str) -> &'static str {
    let tag = tag_name.to_lowercase();
    if tag != "script" && tag != "link" {
        return "report";
    }

    let path_and_search = parse_preview_resource_path(url);
    let lower = path_and_search.to_lowercase();
    let path: &str = path_and_search.split('?').next().unwrap_or("");

    if is_dev_server_noise(&tag, path, &lower) {
        return "suppress";
    }
    "report"
}

/// 噪音规则集 (对齐 `previewResourceNoiseRuleSets` proxy-runtime.js:58-127)。
fn is_dev_server_noise(tag: &str, path: &str, lower: &str) -> bool {
    // Vite
    if path == "/@vite/client"
        || path == "/@react-refresh"
        || path.starts_with("/@id/__x00__vite/")
        || lower.contains("/node_modules/.vite/")
        || lower.contains("/vite/dist/client/")
        || (tag == "script" && lower.contains("/@id/"))
    {
        return true;
    }
    // Astro
    if path.starts_with("/@id/astro:")
        || lower.contains("/astro/dist/runtime/client/dev-toolbar/")
        || (tag == "script" && lower.contains(".astro?") && lower.contains("type=script"))
        || (tag == "script"
            && (lower.ends_with(".css")
                || lower.contains(".css?")
                || lower.contains("type=style")
                || lower.contains("lang.css")))
    {
        return true;
    }
    // Next
    if tag == "script"
        && (path == "/_next/webpack-hmr"
            || lower.contains("/_next/static/webpack/")
            || lower.contains("/_next/static/chunks/webpack")
            || lower.contains("/_next/static/chunks/react-refresh")
            || lower.contains("/_next/static/development/"))
    {
        return true;
    }
    // SvelteKit
    if tag == "script"
        && (lower.contains("/@id/__x00__virtual:")
            || lower.contains("/@id/virtual:")
            || lower.contains("/.svelte-kit/generated/")
            || lower.contains("/node_modules/.vite/deps/"))
    {
        return true;
    }
    // Remix
    if tag == "script"
        && (lower.contains("/@remix-run/dev/")
            || lower.contains("/__manifest")
            || lower.contains("/__hmr"))
    {
        return true;
    }
    // Nuxt
    if tag == "script"
        && (lower.contains("/_nuxt/@vite/client")
            || lower.contains("/_nuxt/@id/")
            || lower.contains("/_nuxt/node_modules/.vite/")
            || lower.contains("/__nuxt_error")
            || lower.contains("/__nuxt_vite_node__"))
    {
        return true;
    }
    // Webpack
    if tag == "script"
        && (path == "/sockjs-node/info"
            || lower.contains("/webpack-dev-server/")
            || lower.contains("/webpack/hot/")
            || lower.contains("/__webpack_hmr")
            || (lower.contains("/ws") && lower.contains("webpack")))
    {
        return true;
    }
    false
}

/// 导航分类结果。
#[derive(Debug, Clone, PartialEq)]
pub struct NavigationDecision {
    pub action: String,
    pub url: String,
}

/// 判断 iframe 内导航应如何处理。
///
/// 对应 `classifyPreviewNavigation` (proxy-runtime.js:143-195)。
/// - `"allow"`: 同页 hash / 已代理链接 → 浏览器默认
/// - `"proxy"`: loopback / 同源根路径 → 通过代理
/// - `"external"`: 外部 http(s) → 外部打开
#[allow(dead_code)]
pub fn classify_preview_navigation(
    url: &str,
    current_url: &str,
    target_origin: Option<&str>,
) -> NavigationDecision {
    let current = Url::parse(current_url)
        .or_else(|_| Url::parse("http://localhost/"))
        .ok();

    let parsed = match Url::parse(url) {
        Ok(p) => p,
        Err(_) => return NavigationDecision { action: "allow".to_string(), url: url.to_string() },
    };

    // 非 http(s) → allow
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return NavigationDecision {
            action: "allow".to_string(),
            url: parsed.to_string(),
        };
    }

    // 同页 hash → allow
    if let Some(cur) = &current {
        if parsed.origin() == cur.origin()
            && parsed.path() == cur.path()
            && parsed.query() == cur.query()
            && parsed.fragment().is_some()
        {
            return NavigationDecision {
                action: "allow".to_string(),
                url: parsed.to_string(),
            };
        }
    }

    let path = if parsed.path().is_empty() { "/" } else { parsed.path() };

    // 已是代理路径 → allow
    if let Some(cur) = &current {
        if parsed.origin() == cur.origin() && path.starts_with("/api/preview/proxy/") {
            return NavigationDecision {
                action: "allow".to_string(),
                url: parsed.to_string(),
            };
        }
    }

    // app-origin 根路径 → 映射回上游 origin
    let proxy_match = current.as_ref().and_then(|cur| {
        let re = regex::Regex::new(r"(?i)^(/api/preview/proxy/[a-f0-9]{16,64})(?:/|$)").ok()?;
        re.captures(cur.path()).and_then(|c| c.get(1).map(|m| m.as_str().to_string()))
    });
    if let (Some(cur), Some(proxy_base)) = (&current, &proxy_match) {
        if parsed.origin() == cur.origin()
            && path.starts_with('/')
            && !path.starts_with(proxy_base.as_str())
        {
            if let Some(target_orig) = target_origin {
                if let Ok(upstream) = Url::parse(&format!(
                    "{}{}{}{}",
                    target_orig,
                    parsed.path(),
                    parsed
                        .query()
                        .map(|q| format!("?{}", q))
                        .unwrap_or_default(),
                    parsed
                        .fragment()
                        .map(|f| format!("#{}", f))
                        .unwrap_or_default()
                )) {
                    return NavigationDecision {
                        action: "proxy".to_string(),
                        url: upstream.to_string(),
                    };
                }
            }
        }
    }

    // loopback 或同源根路径 → proxy
    let host = parsed.host_str().unwrap_or("");
    let is_loopback = host == "localhost"
        || host == "127.0.0.1"
        || host == "0.0.0.0"
        || host == "::1"
        || host == "[::1]";
    if is_loopback
        || (current.as_ref().is_some_and(|cur| parsed.origin() == cur.origin()) && path.starts_with('/'))
    {
        return NavigationDecision {
            action: "proxy".to_string(),
            url: parsed.to_string(),
        };
    }

    NavigationDecision {
        action: "external".to_string(),
        url: parsed.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ============================================================
    // 资源错误分类
    // ============================================================

    #[test]
    fn suppresses_astro_vite_stylesheet_modules() {
        assert_eq!(
            classify_preview_resource_error(
                "script",
                "http://127.0.0.1:57123/api/preview/proxy/f4af70b4261d77706743959516f9cecc/src/styles/global.css"
            ),
            "suppress"
        );
        assert_eq!(
            classify_preview_resource_error(
                "script",
                "http://127.0.0.1:57123/api/preview/proxy/f4af70b4261d77706743959516f9cecc/src/pages/support.astro?astro&type=style&index=0&lang.css"
            ),
            "suppress"
        );
    }

    #[test]
    fn suppresses_framework_virtual_modules() {
        assert_eq!(
            classify_preview_resource_error(
                "script",
                "http://127.0.0.1:57123/api/preview/proxy/f4af70b4261d77706743959516f9cecc/src/layouts/BaseLayout.astro?astro&type=script&index=0&lang.ts"
            ),
            "suppress"
        );
        assert_eq!(
            classify_preview_resource_error(
                "script",
                "http://127.0.0.1:57123/api/preview/proxy/f4af70b4261d77706743959516f9cecc/@vite/client"
            ),
            "suppress"
        );
        assert_eq!(
            classify_preview_resource_error(
                "link",
                "http://127.0.0.1:57123/api/preview/proxy/f4af70b4261d77706743959516f9cecc/@id/astro:scripts/page.js"
            ),
            "suppress"
        );
    }

    #[test]
    fn suppresses_ecosystem_dev_runtime_resources() {
        let noisy = [
            "/_next/static/chunks/webpack.js",
            "/_next/static/chunks/react-refresh.js",
            "/.svelte-kit/generated/client/app.js",
            "/@id/__x00__virtual:sveltekit:browser",
            "/@remix-run/dev/dist/browser.js",
            "/__hmr?runtime=remix",
            "/_nuxt/@vite/client",
            "/_nuxt/@id/virtual:nuxt:%2FUsers%2Fapp",
            "/webpack-dev-server/client/index.js",
            "/webpack/hot/dev-server.js",
            "/__webpack_hmr",
        ];
        for resource in noisy {
            assert_eq!(
                classify_preview_resource_error(
                    "script",
                    &format!(
                        "http://127.0.0.1:57123/api/preview/proxy/f4af70b4261d77706743959516f9cecc{}",
                        resource
                    )
                ),
                "suppress",
                "should suppress: {}",
                resource
            );
        }
    }

    #[test]
    fn reports_ordinary_resource_failures() {
        assert_eq!(
            classify_preview_resource_error(
                "script",
                "http://127.0.0.1:57123/api/preview/proxy/f4af70b4261d77706743959516f9cecc/assets/app.js"
            ),
            "report"
        );
        assert_eq!(
            classify_preview_resource_error(
                "img",
                "http://127.0.0.1:57123/api/preview/proxy/f4af70b4261d77706743959516f9cecc/missing.png"
            ),
            "report"
        );
        assert_eq!(
            classify_preview_resource_error(
                "link",
                "http://127.0.0.1:57123/api/preview/proxy/f4af70b4261d77706743959516f9cecc/styles/missing.css"
            ),
            "report"
        );
    }

    // ============================================================
    // 导航策略
    // ============================================================

    const CURRENT: &str = "http://127.0.0.1:57123/api/preview/proxy/f4af70b4261d77706743959516f9cecc/docs";

    #[test]
    fn allows_same_page_hash_and_proxied_links() {
        let d = classify_preview_navigation("#section", CURRENT, None);
        assert_eq!(d.action, "allow");
        let d = classify_preview_navigation(
            "http://127.0.0.1:57123/api/preview/proxy/f4af70b4261d77706743959516f9cecc/roadmap",
            CURRENT,
            None,
        );
        assert_eq!(d.action, "allow");
    }

    #[test]
    fn proxies_loopback_absolute_links() {
        let d = classify_preview_navigation("http://localhost:3000/roadmap", CURRENT, None);
        assert_eq!(d.action, "proxy");
        assert_eq!(d.url, "http://localhost:3000/roadmap");
    }

    #[test]
    fn maps_app_origin_root_to_upstream_origin() {
        let d = classify_preview_navigation(
            "http://127.0.0.1:57123/support",
            CURRENT,
            Some("https://gridforge.dev"),
        );
        assert_eq!(d.action, "proxy");
        assert_eq!(d.url, "https://gridforge.dev/support");
    }

    #[test]
    fn external_non_loopback_links() {
        let d = classify_preview_navigation("https://example.com/docs", CURRENT, None);
        assert_eq!(d.action, "external");
    }

    #[test]
    fn allows_non_http_links() {
        let d = classify_preview_navigation("mailto:test@example.com", CURRENT, None);
        assert_eq!(d.action, "allow");
    }
}
