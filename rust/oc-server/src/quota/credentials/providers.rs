//! Quota credentials — normalizers + managed CRUD facade。
//!
//! 对应 Node `quota/credentials/providers.js`。

use serde_json::{json, Value};

use super::store::{delete_quota_credential, read_quota_credential, write_quota_credential};

const SECRET_MASK: &str = "\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}";

/// 清剪单行字符串: 去掉首尾空白 + CR/LF 拒绝。
fn clean(value: &Value) -> String {
    if let Some(s) = value.as_str() {
        if s.contains('\r') || s.contains('\n') {
            return String::new();
        }
        return s.trim().to_string();
    }
    String::new()
}

/// `opencode-go` normalizer: `{ workspaceId, authCookie }`, authCookie 可能带 `auth=` 前缀。
fn normalize_opencode_go(value: Value) -> Option<Value> {
    let workspace_id = clean(value.get("workspaceId")?);
    let mut auth_cookie = clean(value.get("authCookie")?);
    if let Some(stripped) = auth_cookie.strip_prefix("auth=") {
        auth_cookie = stripped.trim().to_string();
    }
    if workspace_id.is_empty() || auth_cookie.is_empty() {
        return None;
    }
    Some(json!({
        "workspaceId": workspace_id,
        "authCookie": auth_cookie,
    }))
}

/// `ollama-cloud` normalizer: `{ cookie }`.
fn normalize_ollama_cloud(value: Value) -> Option<Value> {
    let cookie = clean(value.get("cookie")?);
    if cookie.is_empty() {
        return None;
    }
    Some(json!({"cookie": cookie}))
}

/// `cursor` normalizer: `{ accessToken, refreshToken }`.
/// 两个字段都是可选的 — 缺失的键视为空 (与 Node `value?.accessToken` 一致),
/// 只有两者都空才返回 None。
fn normalize_cursor(value: Value) -> Option<Value> {
    let access = value.get("accessToken").map(clean).unwrap_or_default();
    let refresh = value.get("refreshToken").map(clean).unwrap_or_default();
    if access.is_empty() && refresh.is_empty() {
        return None;
    }
    Some(json!({
        "accessToken": access,
        "refreshToken": refresh,
    }))
}

/// 公共 normalizer 注册表 (与 Node `normalizers` 形状一致,只是值是 fn pointer)。
pub static NORMALIZERS: &[(&str, fn(Value) -> Option<Value>)] = &[
    ("opencode-go", normalize_opencode_go as fn(Value) -> Option<Value>),
    ("ollama-cloud", normalize_ollama_cloud as fn(Value) -> Option<Value>),
    ("cursor", normalize_cursor as fn(Value) -> Option<Value>),
];

/// `normalizers[providerId](body)` — 同步查找 + 调用。
pub fn normalize(provider_id: &str, body: Value) -> Option<Value> {
    for (id, fn_ptr) in NORMALIZERS {
        if *id == provider_id {
            return fn_ptr(body);
        }
    }
    None
}

/// `normalizers`-表 (与 Node 对齐,导出供 routes 用)。
pub fn normalizers() -> &'static [(&'static str, fn(Value) -> Option<Value>)] {
    NORMALIZERS
}

/// 读取 managed credential。
pub fn read_managed_credential(provider_id: &str) -> Option<Value> {
    for (id, fn_ptr) in NORMALIZERS {
        if *id == provider_id {
            return read_quota_credential(provider_id, |v| fn_ptr(v));
        }
    }
    None
}

/// 写入 managed credential,返回 status JSON。
pub fn write_managed_credential(provider_id: &str, value: Value) -> Result<Value, String> {
    let cred = normalize(provider_id, value).ok_or_else(|| "Invalid credential".to_string())?;
    write_quota_credential(provider_id, &cred).map_err(|e| e.to_string())?;
    Ok(get_managed_credential_status_value(provider_id))
}

/// 删除 managed credential。
pub fn delete_managed_credential(provider_id: &str) -> Result<(), String> {
    delete_quota_credential(provider_id).map_err(|e| e.to_string())
}

/// 获取 status JSON: `{ configured: bool, ... }`。
pub fn get_managed_credential_status(provider_id: &str) -> Value {
    get_managed_credential_status_value(provider_id)
}

fn get_managed_credential_status_value(provider_id: &str) -> Value {
    let cred = read_managed_credential(provider_id);
    let Some(cred) = cred else {
        return json!({ "configured": false });
    };
    match provider_id {
        "opencode-go" => json!({
            "configured": true,
            "workspaceId": cred.get("workspaceId").cloned().unwrap_or(Value::Null),
            "secretMasked": SECRET_MASK,
        }),
        "cursor" => json!({
            "configured": true,
            "hasRefreshToken": cred.get("refreshToken")
                .and_then(|v| v.as_str())
                .map(|s| !s.is_empty())
                .unwrap_or(false),
            "secretMasked": SECRET_MASK,
        }),
        _ => json!({
            "configured": true,
            "secretMasked": SECRET_MASK,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn opencode_go_basic() {
        let v = normalize(
            "opencode-go",
            json!({"workspaceId": "ws1", "authCookie": "ck1"}),
        )
        .unwrap();
        assert_eq!(v, json!({"workspaceId": "ws1", "authCookie": "ck1"}));
    }

    #[test]
    fn opencode_go_strips_auth_prefix() {
        let v = normalize(
            "opencode-go",
            json!({"workspaceId": "ws1", "authCookie": "auth=abc"}),
        )
        .unwrap();
        assert_eq!(v["authCookie"], json!("abc"));
    }

    #[test]
    fn opencode_go_missing_field_returns_none() {
        let v = normalize("opencode-go", json!({"workspaceId": "ws1"}));
        assert!(v.is_none());
        let v = normalize("opencode-go", json!({"authCookie": "  "}));
        assert!(v.is_none());
    }

    #[test]
    fn opencode_go_rejects_newlines() {
        let v = normalize(
            "opencode-go",
            json!({"workspaceId": "ws1", "authCookie": "abc\ndef"}),
        );
        assert!(v.is_none());
    }

    #[test]
    fn ollama_cloud_basic() {
        let v = normalize("ollama-cloud", json!({"cookie": "sid=xx"})).unwrap();
        assert_eq!(v, json!({"cookie": "sid=xx"}));
    }

    #[test]
    fn ollama_cloud_empty_returns_none() {
        let v = normalize("ollama-cloud", json!({"cookie": ""}));
        assert!(v.is_none());
    }

    #[test]
    fn cursor_normalizes_both_tokens() {
        let v = normalize(
            "cursor",
            json!({"accessToken": "a", "refreshToken": "r"}),
        )
        .unwrap();
        assert_eq!(v["accessToken"], json!("a"));
        assert_eq!(v["refreshToken"], json!("r"));
    }

    #[test]
    fn cursor_accepts_only_one_token() {
        let v = normalize("cursor", json!({"refreshToken": "r"})).unwrap();
        assert_eq!(v["refreshToken"], json!("r"));
    }

    #[test]
    fn cursor_rejects_both_empty() {
        assert!(normalize("cursor", json!({"accessToken": "", "refreshToken": ""})).is_none());
    }

    #[test]
    fn unsupported_normalizer_returns_none() {
        assert!(normalize("not-a-provider", json!({})).is_none());
    }
}
