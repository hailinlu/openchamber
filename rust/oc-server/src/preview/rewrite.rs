//! Body / CSP / redirect 重写 + preview bridge 注入。
//!
//! 对应 `rewritePreviewBody`, `rewritePreviewCspHeader`, `rewritePreviewRedirectLocation`,
//! `injectPreviewBridge`, `rewriteViteClientHmr`, `stripFrameBustingHeaders`
//! (`proxy-runtime.js:1040-1349, 1279-1304`)。

use once_cell::sync::Lazy;
use url::Url;

use super::{PREVIEW_BRIDGE_SCRIPT_ID, TOKEN_QUERY_PARAM, URL_AUTH_TOKEN_QUERY_PARAM};

/// Preview bridge 脚本 (浏览器端, ~700 行 JS, 原样嵌入不移植)。
///
/// 对应 `PREVIEW_BRIDGE_SCRIPT` (proxy-runtime.js:197-904)。
pub const PREVIEW_BRIDGE_SCRIPT: &str = include_str!("preview_bridge.js");

/// body 重写种类。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RewriteKind {
    Html,
    Css,
    JavaScript,
}

/// body 重写参数。
pub struct RewriteParams<'a> {
    pub body_text: &'a str,
    pub proxy_base_path: &'a str,
    pub target_origin: &'a str,
    pub kind: RewriteKind,
    pub preview_token: &'a str,
    pub url_auth_token: &'a str,
}

/// 向代理 URL 追加 auth token 参数。
///
/// 对应 `appendProxyAuthToProxyUrl` (proxy-runtime.js:1019-1036)。
/// 删除 `oc_client_token` / `oc_url_token`, 追加 `oc_preview_token` / `oc_url_token`。
fn append_proxy_auth_to_proxy_url(value: &str, preview_token: &str, url_auth_token: &str) -> String {
    if value.is_empty() {
        return value.to_string();
    }
    let needs_rewrite = !preview_token.is_empty()
        || !url_auth_token.is_empty()
        || value.contains(super::CLIENT_TOKEN_QUERY_PARAM)
        || value.contains(URL_AUTH_TOKEN_QUERY_PARAM);
    if !needs_rewrite {
        return value.to_string();
    }
    // 用占位 origin 解析相对路径
    let base = Url::parse("http://openchamber-preview.local").unwrap();
    let mut parsed = match base.join(value) {
        Ok(u) => u,
        Err(_) => return value.to_string(),
    };
    // 删除旧 token 参数
    let query_pairs: Vec<(String, String)> = parsed
        .query_pairs()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .filter(|(k, _)| {
            k != super::CLIENT_TOKEN_QUERY_PARAM && k != URL_AUTH_TOKEN_QUERY_PARAM
        })
        .collect();
    parsed.query_pairs_mut().clear();
    for (k, v) in &query_pairs {
        parsed.query_pairs_mut().append_pair(k, v);
    }
    if !preview_token.is_empty() {
        parsed
            .query_pairs_mut()
            .append_pair(TOKEN_QUERY_PARAM, preview_token);
    }
    if !url_auth_token.is_empty() {
        parsed
            .query_pairs_mut()
            .append_pair(URL_AUTH_TOKEN_QUERY_PARAM, url_auth_token);
    }
    let path = parsed.path();
    let search = parsed
        .query()
        .map(|q| format!("?{}", q))
        .unwrap_or_default();
    let hash = parsed
        .fragment()
        .map(|f| format!("#{}", f))
        .unwrap_or_default();
    format!("{}{}{}", path, search, hash)
}

/// 判断 URL 是否与目标 origin 同源 (含 loopback 端口匹配)。
///
/// 对应 `isSameTargetOrigin` (proxy-runtime.js:1047-1057)。
fn is_same_target_origin(parsed: &Url, target: Option<&Url>) -> bool {
    let target = match target {
        Some(t) => t,
        None => return false,
    };
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return false;
    }
    if parsed.origin() == target.origin() {
        return true;
    }
    let host = parsed.host_str().unwrap_or("");
    if !["localhost", "127.0.0.1", "0.0.0.0", "::1", "[::1]"].contains(&host) {
        return false;
    }
    parsed.port() == target.port()
}

/// 重写资源 URL (相对路径加前缀, 同源绝对路径转代理路径)。
///
/// 对应 `rewriteResourceUrl` (proxy-runtime.js:1058-1073)。
fn rewrite_resource_url(
    value: &str,
    prefix: &str,
    target: Option<&Url>,
    preview_token: &str,
    url_auth_token: &str,
) -> String {
    if value.is_empty() {
        return value.to_string();
    }
    // 绝对路径 (不以 // 开头)
    if value.starts_with('/') && !value.starts_with("//") {
        if value.starts_with("/api/preview/proxy/") {
            return append_proxy_auth_to_proxy_url(value, preview_token, url_auth_token);
        }
        return append_proxy_auth_to_proxy_url(
            &format!("{}{}", prefix, value),
            preview_token,
            url_auth_token,
        );
    }
    // 尝试解析为绝对 URL, 判断是否同源
    if let Ok(parsed) = Url::parse(value) {
        if is_same_target_origin(&parsed, target) {
            let path = parsed.path();
            let search = parsed
                .query()
                .map(|q| format!("?{}", q))
                .unwrap_or_default();
            let hash = parsed
                .fragment()
                .map(|f| format!("#{}", f))
                .unwrap_or_default();
            return append_proxy_auth_to_proxy_url(
                &format!("{}{}{}{}", prefix, path, search, hash),
                preview_token,
                url_auth_token,
            );
        }
    }
    value.to_string()
}

// ============================================================
// 正则 (fancy-regex 用于 lookahead)
// ============================================================

/// CSP meta strip — 用 lookahead + backref, 需要 fancy-regex。
/// 两个模式分别匹配带引号和不带引号的 http-equiv=content-security-policy。
static RE_CSP_META_QUOTED: Lazy<fancy_regex::Regex> = Lazy::new(|| {
    fancy_regex::Regex::new(
        r#"(?i)<meta\b(?=[^>]*\bhttp-equiv\s*=\s*(['"])content-security-policy\1)[^>]*>"#,
    )
    .expect("invalid RE_CSP_META_QUOTED")
});
static RE_CSP_META_UNQUOTED: Lazy<fancy_regex::Regex> = Lazy::new(|| {
    fancy_regex::Regex::new(
        r#"(?i)<meta\b(?=[^>]*\bhttp-equiv\s*=\s*content-security-policy\b)[^>]*>"#,
    )
    .expect("invalid RE_CSP_META_UNQUOTED")
});

/// CSS url() — `url(['"]?...['"]?)` 引号匹配。回引 `\1` 需 fancy-regex。
static RE_CSS_URL: Lazy<fancy_regex::Regex> = Lazy::new(|| {
    fancy_regex::Regex::new(r#"(?i)url\((['"]?)([^)'"]*)\1\)"#).expect("invalid RE_CSS_URL")
});

/// CSS @import — negative lookahead `(?!//)` 需要 fancy-regex。
static RE_CSS_IMPORT: Lazy<fancy_regex::Regex> = Lazy::new(|| {
    fancy_regex::Regex::new(r#"(?i)@import\s+(['"])\/(?!//)([^'"]*)\1"#)
        .expect("invalid RE_CSS_IMPORT")
});

/// JS from/import/dynamic import — negative lookahead `(?!//)` 需要 fancy-regex。
static RE_JS_FROM: Lazy<fancy_regex::Regex> = Lazy::new(|| {
    fancy_regex::Regex::new(r#"(?i)\bfrom\s+(['"])\/(?!//)([^'"]*)\1"#).expect("invalid RE_JS_FROM")
});
static RE_JS_IMPORT: Lazy<fancy_regex::Regex> = Lazy::new(|| {
    fancy_regex::Regex::new(r#"(?i)\bimport\s+(['"])\/(?!//)([^'"]*)\1"#)
        .expect("invalid RE_JS_IMPORT")
});
static RE_JS_DYNAMIC_IMPORT: Lazy<fancy_regex::Regex> = Lazy::new(|| {
    fancy_regex::Regex::new(r#"(?i)\bimport\(\s*(['"])\/(?!//)([^'"]*)\1\s*\)"#)
        .expect("invalid RE_JS_DYNAMIC_IMPORT")
});

/// HTML 属性 (src/href/action) — 回引 `\2` 需 fancy-regex。
static RE_HTML_ATTR: Lazy<fancy_regex::Regex> = Lazy::new(|| {
    fancy_regex::Regex::new(r#"(?i)\b(src|href|action)=(['"])([^'"]*)\2"#).expect("invalid RE_HTML_ATTR")
});

/// HTML srcset — 回引 `\1` 需 fancy-regex。
static RE_HTML_SRCSET: Lazy<fancy_regex::Regex> = Lazy::new(|| {
    fancy_regex::Regex::new(r#"(?i)\bsrcset=(['"])([^'"]*)\1"#).expect("invalid RE_HTML_SRCSET")
});

/// inline module script — 标准 regex (捕获 attrs + body)。
static RE_INLINE_MODULE_SCRIPT: Lazy<regex::Regex> = Lazy::new(|| {
    regex::Regex::new(r"(?is)<script\b([^>]*)>([\s\S]*?)</script>").expect("invalid RE_INLINE_MODULE_SCRIPT")
});

/// script type 提取。
static RE_SCRIPT_TYPE: Lazy<regex::Regex> = Lazy::new(|| {
    regex::Regex::new(r#"(?i)\btype\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+))"#)
        .expect("invalid RE_SCRIPT_TYPE")
});

/// <head> 开标签。
static RE_HEAD_OPEN: Lazy<regex::Regex> =
    Lazy::new(|| regex::Regex::new(r"(?i)<head(?:\s[^>]*)?>").expect("invalid RE_HEAD_OPEN"));

/// Vite HMR 常量替换。
static RE_VITE_BASE1: Lazy<regex::Regex> =
    Lazy::new(|| regex::Regex::new(r"const base\$1 = [^;]+;").expect("invalid RE_VITE_BASE1"));
static RE_VITE_BASE: Lazy<regex::Regex> =
    Lazy::new(|| regex::Regex::new(r"const base = [^;]+;").expect("invalid RE_VITE_BASE"));
static RE_VITE_HMR_PORT: Lazy<regex::Regex> =
    Lazy::new(|| regex::Regex::new(r"const hmrPort = [^;]+;").expect("invalid RE_VITE_HMR_PORT"));
static RE_VITE_SOCKET_HOST: Lazy<regex::Regex> =
    Lazy::new(|| regex::Regex::new(r"const socketHost = [^;]+;").expect("invalid RE_VITE_SOCKET_HOST"));
static RE_VITE_DIRECT_SOCKET: Lazy<regex::Regex> = Lazy::new(|| {
    regex::Regex::new(r"const directSocketHost = [^;]+;").expect("invalid RE_VITE_DIRECT_SOCKET")
});

// ============================================================
// CSS 重写
// ============================================================

fn rewrite_css(
    text: &str,
    prefix: &str,
    target: Option<&Url>,
    preview_token: &str,
    url_auth_token: &str,
) -> String {
    // url() — fancy-regex 回引需手动遍历
    let mut out = String::with_capacity(text.len());
    let mut last_end = 0;
    let mut iter = RE_CSS_URL.captures_iter(text);
    while let Some(Ok(caps)) = iter.next() {
        let m = caps.get(0).unwrap();
        let quote = caps.get(1).map(|c| c.as_str()).unwrap_or("");
        let value = caps.get(2).map(|c| c.as_str()).unwrap_or("");
        out.push_str(&text[last_end..m.start()]);
        let rewritten = rewrite_resource_url(value, prefix, target, preview_token, url_auth_token);
        out.push_str(&format!("url({}{}{})", quote, rewritten, quote));
        last_end = m.end();
    }
    out.push_str(&text[last_end..]);

    // @import (fancy-regex, 需手动遍历)
    rewrite_css_imports(&out, prefix, target, preview_token, url_auth_token)
}

fn rewrite_css_imports(
    text: &str,
    prefix: &str,
    target: Option<&Url>,
    preview_token: &str,
    url_auth_token: &str,
) -> String {
    let mut result = String::with_capacity(text.len());
    let mut last_end = 0;
    let mut iter = RE_CSS_IMPORT.captures_iter(text);
    while let Some(Ok(caps)) = iter.next() {
        let m = caps.get(0).unwrap();
        let quote = caps.get(1).map(|c| c.as_str()).unwrap_or("'");
        let path = caps.get(2).map(|c| c.as_str()).unwrap_or("");
        result.push_str(&text[last_end..m.start()]);
        let rewritten = rewrite_resource_url(
            &format!("/{}", path),
            prefix,
            target,
            preview_token,
            url_auth_token,
        );
        result.push_str(&format!("@import {}{}{}", quote, rewritten, quote));
        last_end = m.end();
    }
    result.push_str(&text[last_end..]);
    result
}

// ============================================================
// JS 重写
// ============================================================

fn rewrite_javascript(
    text: &str,
    prefix: &str,
    target: Option<&Url>,
    preview_token: &str,
    url_auth_token: &str,
) -> String {
    let out = rewrite_js_single(text, &RE_JS_FROM, prefix, target, preview_token, url_auth_token);
    let out = rewrite_js_single(&out, &RE_JS_IMPORT, prefix, target, preview_token, url_auth_token);
    rewrite_js_single(
        &out,
        &RE_JS_DYNAMIC_IMPORT,
        prefix,
        target,
        preview_token,
        url_auth_token,
    )
}

/// 对单个 fancy-regex 模式做 from/import 替换。
fn rewrite_js_single(
    text: &str,
    re: &fancy_regex::Regex,
    prefix: &str,
    target: Option<&Url>,
    preview_token: &str,
    url_auth_token: &str,
) -> String {
    let mut result = String::with_capacity(text.len());
    let mut last_end = 0;
    let mut iter = re.captures_iter(text);
    while let Some(Ok(caps)) = iter.next() {
        let m = caps.get(0).unwrap();
        let full_keyword = m.as_str();
        // 找到关键字部分 (from/import/import() 已在 pattern 中)
        let quote = caps.get(1).map(|c| c.as_str()).unwrap_or("'");
        let path = caps.get(2).map(|c| c.as_str()).unwrap_or("");
        result.push_str(&text[last_end..m.start()]);
        let rewritten = rewrite_resource_url(
            &format!("/{}", path),
            prefix,
            target,
            preview_token,
            url_auth_token,
        );
        // 重建: 保留关键字前缀
        // from → "from", import → "import", import() → "import("
        // pattern 以 \bfrom / \bimport / \bimport\( 开头, full_keyword 含完整匹配
        let keyword_prefix = if full_keyword.starts_with("import(") || full_keyword.contains("import(") {
            // 动态 import: 重建 import("rewritten")
            format!("import({}{}{})", quote, rewritten, quote)
        } else if full_keyword.trim_start().to_lowercase().starts_with("from") {
            format!("from {}{}{}", quote, rewritten, quote)
        } else {
            format!("import {}{}{}", quote, rewritten, quote)
        };
        result.push_str(&keyword_prefix);
        last_end = m.end();
    }
    result.push_str(&text[last_end..]);
    result
}

// ============================================================
// HTML 重写
// ============================================================

/// 重写 inline module script 的 JS。
///
/// 对应 `rewriteInlineModuleScripts` (proxy-runtime.js:1095-1108)。
fn rewrite_inline_module_scripts(
    text: &str,
    prefix: &str,
    target: Option<&Url>,
    preview_token: &str,
    url_auth_token: &str,
) -> String {
    let mut result = String::with_capacity(text.len());
    let mut last_end = 0;
    for caps in RE_INLINE_MODULE_SCRIPT.captures_iter(text) {
        let m = caps.get(0).unwrap();
        let attrs = caps.get(1).map(|c| c.as_str()).unwrap_or("");
        let script_body = caps.get(2).map(|c| c.as_str()).unwrap_or("");
        // 有 src 的不处理
        if attrs.to_lowercase().contains("src") {
            continue;
        }
        // 只处理 type="module"
        let script_type = RE_SCRIPT_TYPE
            .captures(attrs)
            .and_then(|c| {
                c.get(1)
                    .or_else(|| c.get(2))
                    .or_else(|| c.get(3))
                    .map(|m| m.as_str().trim().to_lowercase())
            })
            .unwrap_or_default();
        if script_type != "module" {
            // 不改写, 原样保留
            result.push_str(&text[last_end..m.end()]);
            last_end = m.end();
            continue;
        }
        let rewritten = rewrite_javascript(script_body, prefix, target, preview_token, url_auth_token);
        if rewritten == script_body {
            result.push_str(&text[last_end..m.end()]);
            last_end = m.end();
            continue;
        }
        result.push_str(&text[last_end..m.start()]);
        result.push_str(&format!("<script{}>{}</script>", attrs, rewritten));
        last_end = m.end();
    }
    result.push_str(&text[last_end..]);
    result
}

fn rewrite_html(
    text: &str,
    prefix: &str,
    target: Option<&Url>,
    preview_token: &str,
    url_auth_token: &str,
) -> String {
    // 1. 属性 src/href/action — fancy-regex 回引需手动遍历
    let mut out = String::with_capacity(text.len());
    let mut last_end = 0;
    let mut iter = RE_HTML_ATTR.captures_iter(text);
    while let Some(Ok(caps)) = iter.next() {
        let m = caps.get(0).unwrap();
        let attr = caps.get(1).map(|c| c.as_str()).unwrap_or("");
        let quote = caps.get(2).map(|c| c.as_str()).unwrap_or("\"");
        let value = caps.get(3).map(|c| c.as_str()).unwrap_or("");
        out.push_str(&text[last_end..m.start()]);
        let rewritten = rewrite_resource_url(value, prefix, target, preview_token, url_auth_token);
        out.push_str(&format!("{}={}{}{}", attr, quote, rewritten, quote));
        last_end = m.end();
    }
    out.push_str(&text[last_end..]);

    // 2. srcset — fancy-regex 回引需手动遍历
    let mut final_out = String::with_capacity(out.len());
    let mut last_end = 0;
    let mut iter = RE_HTML_SRCSET.captures_iter(&out);
    while let Some(Ok(caps)) = iter.next() {
        let m = caps.get(0).unwrap();
        let quote = caps.get(1).map(|c| c.as_str()).unwrap_or("\"");
        let value = caps.get(2).map(|c| c.as_str()).unwrap_or("");
        final_out.push_str(&out[last_end..m.start()]);
        let rewritten: String = value
            .split(',')
            .map(|part| {
                let trimmed = part.trim();
                if trimmed.is_empty() {
                    return trimmed.to_string();
                }
                let mut segments: Vec<&str> = trimmed.split_whitespace().collect();
                if !segments.is_empty() {
                    let url = segments[0].to_string();
                    let rewritten_url =
                        rewrite_resource_url(&url, prefix, target, preview_token, url_auth_token);
                    segments[0] = ""; // placeholder, 将在下面替换
                    let descriptor = if segments.len() > 1 {
                        segments[1..].join(" ")
                    } else {
                        String::new()
                    };
                    if descriptor.is_empty() {
                        return rewritten_url;
                    }
                    return format!("{} {}", rewritten_url, descriptor);
                }
                trimmed.to_string()
            })
            .collect::<Vec<_>>()
            .join(", ");
        final_out.push_str(&format!("srcset={}{}{}", quote, rewritten, quote));
        last_end = m.end();
    }
    final_out.push_str(&out[last_end..]);

    // 3. inline module scripts
    rewrite_inline_module_scripts(&final_out, prefix, target, preview_token, url_auth_token)
}

/// 剥离 CSP meta 标签。
///
/// 对应 `stripPreviewCspMeta` (proxy-runtime.js:1074-1076)。
fn strip_preview_csp_meta(text: &str) -> String {
    let out = RE_CSP_META_QUOTED.replace_all(text, "");
    let out = RE_CSP_META_UNQUOTED.replace_all(&out, "");
    out.to_string()
}

// ============================================================
// 公开 API
// ============================================================

/// 重写 preview body (HTML/CSS/JavaScript)。
///
/// 对应 `rewritePreviewBody` (proxy-runtime.js:1040-1129)。
pub fn rewrite_preview_body(params: &RewriteParams) -> String {
    let body = params.body_text;
    if body.is_empty() {
        return body.to_string();
    }
    let prefix = if params.proxy_base_path.ends_with('/') {
        &params.proxy_base_path[..params.proxy_base_path.len() - 1]
    } else {
        params.proxy_base_path
    };
    let target = Url::parse(params.target_origin).ok();

    match params.kind {
        RewriteKind::Html => {
            let rewritten = rewrite_html(
                body,
                prefix,
                target.as_ref(),
                params.preview_token,
                params.url_auth_token,
            );
            strip_preview_csp_meta(&rewritten)
        }
        RewriteKind::Css => rewrite_css(
            body,
            prefix,
            target.as_ref(),
            params.preview_token,
            params.url_auth_token,
        ),
        RewriteKind::JavaScript => rewrite_javascript(
            body,
            prefix,
            target.as_ref(),
            params.preview_token,
            params.url_auth_token,
        ),
    }
}

/// 重写 CSP header 值。
///
/// 对应 `rewritePreviewCspHeader` (proxy-runtime.js:1135-1167)。
/// - 删除 `frame-ancestors` 和 `require-trusted-types-for` (阻止 framing / bridge DOM)
/// - 向 `script-src` / `script-src-elem` 追加 nonce
/// - 无 script 指令时从 `default-src` 合成 `script-src`
/// - 删除 lone `'none'` 让 nonce 生效
///
/// 返回 `None` 表示 CSP 应完全移除 (空/无有效指令)。
pub fn rewrite_preview_csp_header(csp_value: &str, nonce: &str) -> Option<String> {
    if csp_value.is_empty() {
        return Some(csp_value.to_string());
    }
    let nonce_source = if nonce.is_empty() {
        String::new()
    } else {
        format!("'nonce-{}'", nonce)
    };

    let mut directives: Vec<(String, Vec<String>)> = csp_value
        .split(';')
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .map(|p| {
            let tokens: Vec<String> = p.split_whitespace().map(String::from).collect();
            let name = tokens
                .first()
                .map(|t| t.to_lowercase())
                .unwrap_or_default();
            (name, tokens)
        })
        .filter(|(name, _)| name != "frame-ancestors" && name != "require-trusted-types-for")
        .collect();

    if !nonce_source.is_empty() {
        // 收集指令索引 (owns 数据, 避免 borrow 冲突)
        let by_name: std::collections::HashMap<String, usize> = directives
            .iter()
            .enumerate()
            .map(|(i, (name, _))| (name.clone(), i))
            .collect();
        let allow_nonce = |directive: &mut (String, Vec<String>)| {
            directive
                .1
                .retain(|t| t.to_lowercase() != "'none'");
            if !directive.1.contains(&nonce_source) {
                directive.1.push(nonce_source.clone());
            }
        };
        let has_script_elem = by_name.contains_key("script-src-elem");
        let has_script_src = by_name.contains_key("script-src");

        // 提取索引后 by_name 不再需要 (其 borrow 结束)
        let script_elem_idx = by_name.get("script-src-elem").copied();
        let script_src_idx = by_name.get("script-src").copied();
        let default_src_base: Option<Vec<String>> = if !has_script_elem
            && !has_script_src
            && by_name.contains_key("default-src")
        {
            let i = by_name["default-src"];
            Some(
                directives[i]
                    .1
                    .iter()
                    .skip(1)
                    .filter(|t| t.to_lowercase() != "'none'")
                    .cloned()
                    .collect(),
            )
        } else {
            None
        };

        if let Some(i) = script_elem_idx {
            allow_nonce(&mut directives[i]);
        }
        if let Some(i) = script_src_idx {
            allow_nonce(&mut directives[i]);
        }
        if let Some(base) = default_src_base {
            let mut new_dir = vec!["script-src".to_string()];
            new_dir.extend(base);
            new_dir.push(nonce_source.clone());
            directives.push(("script-src".to_string(), new_dir));
        }
    }

    let rebuilt: Vec<String> = directives
        .into_iter()
        .map(|(_, tokens)| tokens.join(" "))
        .collect();
    if rebuilt.is_empty() {
        None
    } else {
        Some(rebuilt.join("; "))
    }
}

/// 重写 redirect Location header。
///
/// 对应 `rewritePreviewRedirectLocation` (proxy-runtime.js:1169-1184)。
/// 仅 loopback + 同端口的重定向才转代理路径; 外部重定向原样返回。
pub fn rewrite_preview_redirect_location(
    location: &str,
    proxy_base_path: &str,
    target_origin: &str,
    preview_token: &str,
    url_auth_token: &str,
) -> String {
    if location.is_empty() {
        return location.to_string();
    }
    let prefix = proxy_base_path
        .strip_suffix('/')
        .unwrap_or(proxy_base_path);
    let target = match Url::parse(target_origin) {
        Ok(t) => t,
        Err(_) => return location.to_string(),
    };
    let parsed = match target.join(location) {
        Ok(p) => p,
        Err(_) => return location.to_string(),
    };
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return location.to_string();
    }
    let host = parsed.host_str().unwrap_or("");
    let is_loopback = ["localhost", "127.0.0.1", "0.0.0.0", "::1", "[::1]"].contains(&host);
    if !is_loopback || parsed.port() != target.port() {
        return location.to_string();
    }
    let path = parsed.path();
    let search = parsed
        .query()
        .map(|q| format!("?{}", q))
        .unwrap_or_default();
    let hash = parsed
        .fragment()
        .map(|f| format!("#{}", f))
        .unwrap_or_default();
    append_proxy_auth_to_proxy_url(
        &format!("{}{}{}{}", prefix, path, search, hash),
        preview_token,
        url_auth_token,
    )
}

/// 注入 preview bridge 脚本到 HTML body。
///
/// 对应 `injectPreviewBridge` (proxy-runtime.js:1315-1330)。
pub fn inject_preview_bridge(body_text: &str, target_origin: &str, bridge_nonce: &str) -> String {
    if body_text.contains(PREVIEW_BRIDGE_SCRIPT_ID) {
        return body_text.to_string();
    }
    let nonce_attr = if bridge_nonce.is_empty() {
        String::new()
    } else {
        format!(" nonce=\"{}\"", bridge_nonce)
    };
    let target_origin_script = format!(
        "<script{}>window.__openchamberPreviewTargetOrigin={};</script>",
        nonce_attr,
        serde_json::to_string(target_origin).unwrap_or_else(|_| "\"\"".to_string())
    );
    let script = format!(
        "{}<script id=\"{}\"{}>{}</script>",
        target_origin_script, PREVIEW_BRIDGE_SCRIPT_ID, nonce_attr, PREVIEW_BRIDGE_SCRIPT
    );
    // 插入到 <head> 之后
    if let Some(m) = RE_HEAD_OPEN.find(body_text) {
        let mut result = String::with_capacity(body_text.len() + script.len());
        result.push_str(&body_text[..m.end()]);
        result.push_str(&script);
        result.push_str(&body_text[m.end()..]);
        return result;
    }
    // 插入到 </body> 之前
    if let Some(idx) = body_text.to_lowercase().find("</body>") {
        let mut result = String::with_capacity(body_text.len() + script.len());
        result.push_str(&body_text[..idx]);
        result.push_str(&script);
        result.push_str(&body_text[idx..]);
        return result;
    }
    // 追加到末尾
    format!("{}{}", body_text, script)
}

/// 重写 Vite client HMR 代码。
///
/// 对应 `rewriteViteClientHmr` (proxy-runtime.js:1332-1349)。
pub fn rewrite_vite_client_hmr(body_text: &str, proxy_base_path: &str) -> String {
    if !body_text.contains("vite-hmr") {
        return body_text.to_string();
    }
    let base = if proxy_base_path.ends_with('/') {
        proxy_base_path.to_string()
    } else {
        format!("{}/", proxy_base_path)
    };
    let base_json = serde_json::to_string(&base).unwrap_or_default();
    let escaped_base = &base_json[1..base_json.len() - 1];

    let out = RE_VITE_BASE1
        .replace_all(body_text, format!("const base$1 = {};", base_json));
    let out = RE_VITE_BASE.replace_all(&out, format!("const base = {};", base_json));
    let out = RE_VITE_HMR_PORT.replace_all(&out, "const hmrPort = importMetaUrl.port;");
    let out = RE_VITE_SOCKET_HOST.replace_all(
        &out,
        format!(
            "const socketHost = `${{importMetaUrl.hostname}}${{importMetaUrl.port ? ':' + importMetaUrl.port : ''}}{}`;",
            escaped_base
        ),
    );
    let out = RE_VITE_DIRECT_SOCKET.replace_all(&out, "const directSocketHost = socketHost;");
    out.to_string()
}

/// 剥离 frame-busting headers (X-Frame-Options / CSP frame-ancestors)。
///
/// 对应 `stripFrameBustingHeaders` (proxy-runtime.js:1279-1304)。
/// 返回 (csp_value_or_none, should_delete_xfo): CSP 改写后可能为 None (应删除)。
#[allow(dead_code)]
pub fn strip_frame_busting_csp(csp_value: &str, bridge_nonce: &str) -> Option<String> {
    rewrite_preview_csp_header(csp_value, bridge_nonce)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rewrite(body: &str, kind: RewriteKind) -> String {
        rewrite_preview_body(&RewriteParams {
            body_text: body,
            proxy_base_path: "/api/preview/proxy/abc123",
            target_origin: "http://127.0.0.1:3000",
            kind,
            preview_token: "",
            url_auth_token: "",
        })
    }

    // ============================================================
    // HTML 重写
    // ============================================================

    #[test]
    fn rewrites_html_resource_attributes() {
        let input = r#"<img src="/logo.png"><a href="/docs">Docs</a><script>const url = "/api/data";</script>"#;
        let out = rewrite(input, RewriteKind::Html);
        assert!(out.contains(r#"src="/api/preview/proxy/abc123/logo.png""#));
        assert!(out.contains(r#"href="/api/preview/proxy/abc123/docs""#));
        assert!(out.contains(r#"const url = "/api/data";"#));
    }

    #[test]
    fn rewrites_inline_module_imports() {
        let input = [
            r#"<script type="module">"#,
            r#"import RefreshRuntime from "/@react-refresh";"#,
            r#"window.__vite_plugin_react_preamble_installed__ = true;"#,
            r#"</script>"#,
            r#"<script type=module>"#,
            r#"import { injectIntoGlobalHook } from "/@react-refresh";"#,
            r#"import value from "/module.js";"#,
            r#"const url = "/api/data";"#,
            r#"</script>"#,
            r#"<script type='module'>"#,
            r#"import "/entry.js";"#,
            r#"</script>"#,
            r#"<script type="text/javascript">"#,
            r#"import "/not-rewritten.js";"#,
            r#"</script>"#,
            r#"<script>const refreshUrl = "/@react-refresh";</script>"#,
        ]
        .join("");
        let out = rewrite(&input, RewriteKind::Html);
        assert!(out.contains(r#"from "/api/preview/proxy/abc123/@react-refresh""#));
        assert!(out.contains(r#"import { injectIntoGlobalHook } from "/api/preview/proxy/abc123/@react-refresh";"#));
        assert!(out.contains(r#"import value from "/api/preview/proxy/abc123/module.js";"#));
        assert!(out.contains(r#"const url = "/api/data";"#));
        assert!(out.contains(r#"import "/api/preview/proxy/abc123/entry.js";"#));
        // 非 module 的 script 不改写
        assert!(out.contains(r#"import "/not-rewritten.js";"#));
        assert!(out.contains(r#"const refreshUrl = "/@react-refresh";"#));
        assert!(out.contains("window.__vite_plugin_react_preamble_installed__ = true;"));
    }

    #[test]
    fn removes_csp_meta_tags() {
        let input = r#"<meta http-equiv="Content-Security-Policy" content="script-src 'self'"><div>Preview</div>"#;
        let out = rewrite(input, RewriteKind::Html);
        assert!(!out.contains("Content-Security-Policy"));
        assert!(out.contains("<div>Preview</div>"));
    }

    // ============================================================
    // token 追加
    // ============================================================

    #[test]
    fn adds_tokens_to_rewritten_resources() {
        let out = rewrite_preview_body(&RewriteParams {
            body_text: r#"<script src="/entry.js"></script><script type="module">import RefreshRuntime from "/@react-refresh";</script><a href="http://localhost:3000/docs?x=1&oc_client_token=legacy">Docs</a>"#,
            kind: RewriteKind::Html,
            proxy_base_path: "/api/preview/proxy/abc123",
            target_origin: "http://127.0.0.1:3000",
            preview_token: "preview-secret",
            url_auth_token: "url-secret",
        });
        assert!(out.contains(r#"src="/api/preview/proxy/abc123/entry.js?oc_preview_token=preview-secret&oc_url_token=url-secret""#));
        assert!(out.contains(r#"from "/api/preview/proxy/abc123/@react-refresh?oc_preview_token=preview-secret&oc_url_token=url-secret""#));
        assert!(out.contains(r#"href="/api/preview/proxy/abc123/docs?x=1&oc_preview_token=preview-secret&oc_url_token=url-secret""#));
        assert!(!out.contains("oc_client_token"));
    }

    // ============================================================
    // CSS 重写
    // ============================================================

    #[test]
    fn rewrites_css_imports_and_urls() {
        let input = r#"@import "/theme.css"; .hero { background: url(/hero.png); } .copy::after { content: "/not-a-url"; }"#;
        let out = rewrite(input, RewriteKind::Css);
        assert!(out.contains(r#"@import "/api/preview/proxy/abc123/theme.css""#));
        assert!(out.contains("url(/api/preview/proxy/abc123/hero.png)"));
        assert!(out.contains(r#"content: "/not-a-url";"#));
    }

    #[test]
    fn rewrites_css_with_tokens() {
        let out = rewrite_preview_body(&RewriteParams {
            body_text: r#"@import "/theme.css"; .hero { background: url(/hero.png); }"#,
            kind: RewriteKind::Css,
            proxy_base_path: "/api/preview/proxy/abc123",
            target_origin: "http://127.0.0.1:3000",
            preview_token: "preview-secret",
            url_auth_token: "url-secret",
        });
        assert!(out.contains(r#"@import "/api/preview/proxy/abc123/theme.css?oc_preview_token=preview-secret&oc_url_token=url-secret""#));
        assert!(out.contains("url(/api/preview/proxy/abc123/hero.png?oc_preview_token=preview-secret&oc_url_token=url-secret)"));
    }

    // ============================================================
    // JS 重写
    // ============================================================

    #[test]
    fn rewrites_javascript_static_imports() {
        let input = r#"import "/entry.js"; import value from "/module.js"; const url = "/api/data"; fetch("/api/data");"#;
        let out = rewrite(input, RewriteKind::JavaScript);
        assert!(out.contains(r#"import "/api/preview/proxy/abc123/entry.js""#));
        assert!(out.contains(r#"from "/api/preview/proxy/abc123/module.js""#));
        assert!(out.contains(r#"const url = "/api/data""#));
        assert!(out.contains(r#"fetch("/api/data")"#));
    }

    #[test]
    fn rewrites_javascript_with_tokens() {
        let out = rewrite_preview_body(&RewriteParams {
            body_text: r#"import("/entry.js"); import value from "/module.js";"#,
            kind: RewriteKind::JavaScript,
            proxy_base_path: "/api/preview/proxy/abc123",
            target_origin: "http://127.0.0.1:3000",
            preview_token: "preview-secret",
            url_auth_token: "url-secret",
        });
        assert!(out.contains(r#"import("/api/preview/proxy/abc123/entry.js?oc_preview_token=preview-secret&oc_url_token=url-secret")"#));
        assert!(out.contains(r#"from "/api/preview/proxy/abc123/module.js?oc_preview_token=preview-secret&oc_url_token=url-secret""#));
    }

    // ============================================================
    // Redirect 重写
    // ============================================================

    #[test]
    fn rewrites_loopback_redirect() {
        let result = rewrite_preview_redirect_location(
            "http://localhost:3000/login?next=%2F#top",
            "/api/preview/proxy/abc123",
            "http://127.0.0.1:3000",
            "",
            "",
        );
        assert_eq!(result, "/api/preview/proxy/abc123/login?next=%2F#top");
    }

    #[test]
    fn leaves_external_redirect_unchanged() {
        let result = rewrite_preview_redirect_location(
            "https://example.com/login",
            "/api/preview/proxy/abc123",
            "http://127.0.0.1:3000",
            "",
            "",
        );
        assert_eq!(result, "https://example.com/login");
    }

    #[test]
    fn adds_tokens_to_loopback_redirect() {
        let result = rewrite_preview_redirect_location(
            "http://localhost:3000/login?next=%2F#top",
            "/api/preview/proxy/abc123",
            "http://127.0.0.1:3000",
            "preview-secret",
            "url-secret",
        );
        assert_eq!(
            result,
            "/api/preview/proxy/abc123/login?next=%2F&oc_preview_token=preview-secret&oc_url_token=url-secret#top"
        );
    }

    #[test]
    fn leaves_redirect_unchanged_no_target_origin() {
        let result = rewrite_preview_redirect_location(
            "http://localhost:5174/callback",
            "/api/preview/proxy/abc123",
            "",
            "preview-secret",
            "",
        );
        assert_eq!(result, "http://localhost:5174/callback");
    }

    // ============================================================
    // CSP 重写
    // ============================================================

    #[test]
    fn csp_drops_frame_ancestors_and_trusted_types() {
        let result = rewrite_preview_csp_header(
            "default-src 'self'; frame-ancestors 'none'; require-trusted-types-for 'script'",
            "abc123",
        )
        .unwrap();
        assert!(!result.contains("frame-ancestors"));
        assert!(!result.contains("require-trusted-types-for"));
        assert!(result.contains("default-src 'self'"));
    }

    #[test]
    fn csp_adds_nonce_to_script_src() {
        let result = rewrite_preview_csp_header("script-src 'self'", "abc123").unwrap();
        assert!(result.contains("script-src 'self' 'nonce-abc123'"));
    }

    #[test]
    fn csp_adds_nonce_to_script_src_elem() {
        let result = rewrite_preview_csp_header("script-src-elem 'self'", "abc123").unwrap();
        assert!(result.contains("script-src-elem 'self' 'nonce-abc123'"));
    }

    #[test]
    fn csp_synthesizes_script_src_from_default_src() {
        let result =
            rewrite_preview_csp_header("default-src 'self' https://cdn.example.com", "abc123").unwrap();
        assert!(result.contains("default-src 'self' https://cdn.example.com"));
        assert!(result.contains("script-src 'self' https://cdn.example.com 'nonce-abc123'"));
    }

    #[test]
    fn csp_drops_lone_none() {
        let result = rewrite_preview_csp_header("script-src 'none'", "abc123").unwrap();
        assert_eq!(result, "script-src 'nonce-abc123'");
    }

    #[test]
    fn csp_empty_returns_empty() {
        assert_eq!(rewrite_preview_csp_header("", "abc123").unwrap(), "");
    }

    // ============================================================
    // Bridge 注入
    // ============================================================

    #[test]
    fn inject_bridge_into_head() {
        let html = "<html><head><title>Test</title></head><body></body></html>";
        let result = inject_preview_bridge(html, "http://127.0.0.1:3000", "nonce123");
        assert!(result.contains(PREVIEW_BRIDGE_SCRIPT_ID));
        assert!(result.contains("nonce=\"nonce123\""));
        // bridge 在 <head> 之后
        let head_end = result.find("</head>").unwrap();
        let bridge_pos = result.find(PREVIEW_BRIDGE_SCRIPT_ID).unwrap();
        assert!(bridge_pos < head_end);
    }

    #[test]
    fn inject_bridge_does_not_double_inject() {
        let html = "<head></head>";
        let once = inject_preview_bridge(html, "", "");
        let twice = inject_preview_bridge(&once, "", "");
        assert_eq!(once, twice);
    }
}
