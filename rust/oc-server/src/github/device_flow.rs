//! GitHub OAuth device flow。
//!
//! 移植自 `packages/web/server/lib/github/device-flow.js` (50 行)。
//!
//! 两个 form-encoded POST:
//! 1. `POST /login/device/code` → device code payload
//! 2. `POST /login/oauth/access_token` → access token (或 pending error)
//!
//! 关键: GitHub 对 pending 状态返回 HTTP 200 + `{error: 'authorization_pending'}`。
//! 不把 pending 当错误。

use serde_json::Value;

use crate::github::{ACCESS_TOKEN_URL, DEVICE_CODE_URL, DEVICE_GRANT_TYPE};

/// form 编码 (跳过 null/undefined 值, 对应 Node `encodeForm`)。
fn encode_form(params: &[(&str, Option<&str>)]) -> String {
    let mut pairs = Vec::new();
    for (key, value) in params {
        if let Some(v) = value {
            if !v.is_empty() {
                pairs.push(format!("{}={}", url_encode(key), url_encode(v)));
            }
        }
    }
    pairs.join("&")
}

/// 极简 URL 编码 (form-encoded)。
fn url_encode(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                result.push(byte as char);
            }
            _ => {
                result.push_str(&format!("%{:02X}", byte));
            }
        }
    }
    result
}

/// POST form-encoded 到 GitHub, 解析 JSON 响应。
async fn post_form(url: &str, body: &str) -> Result<Value, DeviceFlowError> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| DeviceFlowError(e.to_string()))?;

    let resp = client
        .post(url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .body(body.to_string())
        .send()
        .await
        .map_err(|e| DeviceFlowError(e.to_string()))?;

    let status = resp.status();
    let payload: Value = resp
        .json()
        .await
        .map_err(|e| DeviceFlowError(format!("failed to parse response: {}", e)))?;

    if !status.is_success() {
        let message = payload
            .get("error_description")
            .and_then(|v| v.as_str())
            .or_else(|| payload.get("error").and_then(|v| v.as_str()))
            .unwrap_or("GitHub request failed")
            .to_string();
        return Err(DeviceFlowError(message));
    }

    Ok(payload)
}

/// GitHub device flow 错误。
#[derive(Debug)]
pub struct DeviceFlowError(pub String);

/// 启动 device flow — `POST /login/device/code`。
pub async fn start_device_flow(
    client_id: &str,
    scope: &str,
) -> Result<Value, DeviceFlowError> {
    let body = encode_form(&[
        ("client_id", Some(client_id)),
        ("scope", Some(scope)),
    ]);
    post_form(DEVICE_CODE_URL, &body).await
}

/// 交换 device code — `POST /login/oauth/access_token`。
///
/// GitHub 返回:
/// - 成功: `{ access_token, scope, token_type }`
/// - pending: `{ error: "authorization_pending", error_description: "..." }` (HTTP 200)
pub async fn exchange_device_code(
    client_id: &str,
    device_code: &str,
) -> Result<Value, DeviceFlowError> {
    let body = encode_form(&[
        ("client_id", Some(client_id)),
        ("device_code", Some(device_code)),
        ("grant_type", Some(DEVICE_GRANT_TYPE)),
    ]);
    // 注意: access_token endpoint 对 pending 返回 200, 所以 post_form 不会报错
    post_form(ACCESS_TOKEN_URL, &body).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_form_skips_empty() {
        let body = encode_form(&[
            ("client_id", Some("abc")),
            ("scope", Some("")),
            ("device_code", None),
        ]);
        assert_eq!(body, "client_id=abc");
    }

    #[test]
    fn encode_form_all_present() {
        let body = encode_form(&[
            ("client_id", Some("abc")),
            ("scope", Some("repo user")),
        ]);
        assert_eq!(body, "client_id=abc&scope=repo%20user");
    }

    #[test]
    fn encode_form_special_chars() {
        let body = encode_form(&[("grant_type", Some("urn:ietf:params:oauth:grant-type:device_code"))]);
        assert_eq!(
            body,
            "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code"
        );
    }
}
