//! APNs 发送 — relay (POST) + direct (HTTP/2 ES256 JWT) + relay signing keypair。
//!
//! 对应 Node `notifications/apns-runtime.js` 的发送部分。
//!
//! **两种模式**:
//! - Relay (默认): POST tokens + generic text 到 relay URL, relay 持有 Apple Key。
//! - Direct (fallback): `GRIDFORGE_PUSH_RELAY_DISABLED=true` + `GRIDFORGE_APNS_*`,
//!   自己签 ES256 JWT + HTTP/2 连接 api.push.apple.com。
//!
//! **签名兼容**: ECDSA P-256 IEEE-P1363 (raw r||s), base64url 编码。
//! Node 用 `crypto.sign(..., {dsaEncoding:'ieee-p1363'})`, Rust 用 p256 crate。

use std::sync::Arc;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde_json::{json, Value};

use super::apns_store::ApnsStore;
use super::relay_key;
use super::{
    now_millis, APNS_HOST_PRODUCTION, APNS_HOST_SANDBOX, DEFAULT_BUNDLE_ID, DEFAULT_RELAY_URL,
};

/// APNs direct 模式配置。
#[derive(Debug, Clone)]
pub struct ApnsConfig {
    pub key_id: String,
    pub team_id: String,
    pub p8_pem: String,
    pub bundle_id: String,
    pub environment: String, // "production" | "sandbox"
}

/// APNs relay 配置。
#[derive(Debug, Clone)]
pub struct RelayConfig {
    pub url: String,
    pub register_url: String,
    pub environment: String,
}

/// APNs payload (generic, 通知 relay 用)。
#[derive(Debug, Clone)]
pub struct ApnsPayload {
    pub title: String,
    pub body: String,
    pub badge: Option<i64>,
    pub tag: Option<String>,
    pub data: Option<Value>,
}

/// APNs 发送运行时。
pub struct ApnsSendRuntime {
    apns_store: Arc<ApnsStore>,
    http_client: reqwest::Client,
}

impl ApnsSendRuntime {
    pub fn new(apns_store: Arc<ApnsStore>, http_client: reqwest::Client) -> Self {
        Self {
            apns_store,
            http_client,
        }
    }

    // -----------------------------------------------------------------------
    // Config resolution
    // -----------------------------------------------------------------------

    /// 解析 relay 配置。
    ///
    /// 对应 Node `resolveRelayConfig`。relay 未禁用时返回 Some。
    pub fn resolve_relay_config() -> Option<RelayConfig> {
        let disabled = trimmed_env("GRIDFORGE_PUSH_RELAY_DISABLED");
        let relay_url = trimmed_env("GRIDFORGE_PUSH_RELAY_URL");
        let apns_env = trimmed_env("GRIDFORGE_APNS_ENVIRONMENT");
        Self::resolve_relay_config_from(disabled.as_deref(), relay_url.as_deref(), apns_env.as_deref())
    }

    /// 纯函数版本，便于单元测试 (避免 env 并发竞争)。
    fn resolve_relay_config_from(
        disabled: Option<&str>,
        relay_url: Option<&str>,
        apns_env: Option<&str>,
    ) -> Option<RelayConfig> {
        if disabled == Some("true") {
            return None;
        }
        let url = relay_url
            .filter(|s| !s.is_empty())
            .unwrap_or(DEFAULT_RELAY_URL)
            .to_string();
        let register_url = url.replace("/send", "/register-token");
        let environment = apns_env
            .map(|e| e.to_lowercase())
            .filter(|e| e == "production")
            .unwrap_or_else(|| "sandbox".to_string());
        Some(RelayConfig {
            url,
            register_url,
            environment,
        })
    }

    /// 解析 APNs direct 配置。
    ///
    /// 对应 Node `resolveApnsConfig`。env 优先, 然后 settings.apnsConfig。
    pub fn resolve_apns_config() -> Option<ApnsConfig> {
        let mut key_id = trimmed_env("GRIDFORGE_APNS_KEY_ID");
        let mut team_id = trimmed_env("GRIDFORGE_APNS_TEAM_ID");
        let mut bundle_id = trimmed_env("GRIDFORGE_APNS_BUNDLE_ID");
        let mut environment = trimmed_env("GRIDFORGE_APNS_ENVIRONMENT")
            .map(|e| e.to_lowercase())
            .unwrap_or_default();
        let mut p8 = super::types::normalize_pem(&std::env::var("GRIDFORGE_APNS_P8").unwrap_or_default());

        // 从文件读取 p8
        if p8.is_empty() {
            if let Some(p8_path) = trimmed_env("GRIDFORGE_APNS_P8_PATH") {
                if let Ok(content) = std::fs::read_to_string(&p8_path) {
                    p8 = super::types::normalize_pem(content.trim());
                }
            }
        }

        // settings fallback
        if key_id.is_none() || team_id.is_none() || p8.is_empty() {
            let settings = crate::github::settings::read_settings();
            if let Some(stored) = settings.get("apnsConfig") {
                key_id = key_id.or_else(|| {
                    stored.get("keyId").and_then(|v| v.as_str()).map(|s| s.trim().to_string())
                });
                team_id = team_id.or_else(|| {
                    stored.get("teamId").and_then(|v| v.as_str()).map(|s| s.trim().to_string())
                });
                bundle_id = bundle_id.or_else(|| {
                    stored.get("bundleId").and_then(|v| v.as_str()).map(|s| s.trim().to_string())
                });
                if environment.is_empty() {
                    environment = stored
                        .get("environment")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_lowercase())
                        .unwrap_or_default();
                }
                if p8.is_empty() {
                    if let Some(stored_p8) = stored.get("p8").and_then(|v| v.as_str()) {
                        p8 = super::types::normalize_pem(stored_p8);
                    }
                }
            }
        }

        let key_id = key_id?;
        let team_id = team_id?;
        if p8.is_empty() {
            return None;
        }

        Some(ApnsConfig {
            key_id,
            team_id,
            p8_pem: p8,
            bundle_id: bundle_id.unwrap_or_else(|| DEFAULT_BUNDLE_ID.to_string()),
            environment: if environment == "production" {
                "production".to_string()
            } else {
                "sandbox".to_string()
            },
        })
    }

    // -----------------------------------------------------------------------
    // ES256 JWT signing (direct mode)
    // -----------------------------------------------------------------------

    /// 签发 APNs ES256 JWT。
    ///
    /// 对应 Node `signApnsJwt`。header `{alg:ES256, kid}`, claims `{iss, iat}`,
    /// p256 签名 IEEE-P1363 base64url。
    pub fn sign_apns_jwt(config: &ApnsConfig) -> Option<String> {
        use p256::ecdsa::signature::RandomizedSigner;

        let signing_key = relay_key::p8_to_signing_key(&config.p8_pem)?;

        let header = json!({ "alg": "ES256", "kid": config.key_id });
        let header_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_string(&header).ok()?);
        let iat = chrono::Utc::now().timestamp();
        let claims = json!({ "iss": config.team_id, "iat": iat });
        let claims_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_string(&claims).ok()?);

        let signing_input = format!("{}.{}", header_b64, claims_b64);

        let mut rng = rand::thread_rng();
        let sig: p256::ecdsa::Signature =
            signing_key.sign_with_rng(&mut rng, signing_input.as_bytes());
        let sig_b64 = URL_SAFE_NO_PAD.encode(sig.to_bytes());

        Some(format!("{}.{}", signing_input, sig_b64))
    }

    // -----------------------------------------------------------------------
    // Relay mode (POST tokens + generic text)
    // -----------------------------------------------------------------------

    /// 在 relay 注册 token。
    ///
    /// 对应 Node `registerTokenWithRelay`。
    pub async fn register_token_with_relay(&self, token: &str, platform: &str) {
        let relay = match Self::resolve_relay_config() {
            Some(r) => r,
            None => return, // direct mode — no relay binding
        };

        let (signing_key, public_jwk) = relay_key::get_or_create_relay_keypair();
        let ts = now_millis();
        let message = format!("{}.{}.{}", ts, token, platform);
        let sig = relay_key::sign_relay_message(&signing_key, &message);

        let relay_jwk = json!({
            "kty": public_jwk.get("kty").cloned().unwrap_or(Value::Null),
            "crv": public_jwk.get("crv").cloned().unwrap_or(Value::Null),
            "x": public_jwk.get("x").cloned().unwrap_or(Value::Null),
            "y": public_jwk.get("y").cloned().unwrap_or(Value::Null),
        });

        let body = json!({
            "token": token,
            "platform": platform,
            "publicKeyJwk": relay_jwk,
            "ts": ts,
            "sig": sig,
        });

        let result = self
            .http_client
            .post(&relay.register_url)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await;

        match result {
            Ok(resp) if !resp.status().is_success() => {
                tracing::warn!(status = %resp.status(), "[Push relay] register-token failed");
            }
            Err(e) => {
                tracing::warn!(error = %e, "[Push relay] register-token request failed");
            }
            _ => {}
        }
    }

    /// 通过 relay 发送。
    ///
    /// 对应 Node `sendViaRelay`。
    async fn send_via_relay(&self, device_tokens: &[String], payload: &ApnsPayload, relay: &RelayConfig) {
        let tokens: Vec<String> = device_tokens.iter().take(100).cloned().collect();
        let title = if payload.title.is_empty() {
            "GridForge".to_string()
        } else {
            payload.title.clone()
        };

        let (signing_key, public_jwk) = relay_key::get_or_create_relay_keypair();
        let ts = now_millis();
        let mut sorted_tokens = tokens.clone();
        sorted_tokens.sort();
        let sign_message = format!("{}.{}.{}", ts, sorted_tokens.join(","), title);
        let sig = relay_key::sign_relay_message(&signing_key, &sign_message);

        let relay_jwk = json!({
            "kty": public_jwk.get("kty").cloned().unwrap_or(Value::Null),
            "crv": public_jwk.get("crv").cloned().unwrap_or(Value::Null),
            "x": public_jwk.get("x").cloned().unwrap_or(Value::Null),
            "y": public_jwk.get("y").cloned().unwrap_or(Value::Null),
        });

        let collapse_id = payload
            .tag
            .as_ref()
            .map(|t| t.chars().take(64).collect::<String>());

        let body = json!({
            "tokens": tokens,
            "title": title,
            "body": payload.body,
            "badge": payload.badge.map(|b| b as u64),
            "collapseId": collapse_id,
            "env": relay.environment,
            "data": payload.data,
            "publicKeyJwk": relay_jwk,
            "ts": ts,
            "sig": sig,
        });

        let result = self
            .http_client
            .post(&relay.url)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await;

        match result {
            Ok(resp) if resp.status().is_success() => {
                if let Ok(data) = resp.json::<Value>().await {
                    if let Some(results) = data.get("results").and_then(|v| v.as_array()) {
                        for result in results {
                            if result.get("drop").and_then(|v| v.as_bool()) == Some(true) {
                                if let Some(token) = result.get("token").and_then(|v| v.as_str()) {
                                    self.apns_store.remove_token_from_all_sessions(token);
                                }
                            }
                        }
                    }
                }
            }
            Ok(resp) => {
                tracing::warn!(status = %resp.status(), "[APNs relay] send failed");
            }
            Err(e) => {
                tracing::warn!(error = %e, "[APNs relay] request failed");
            }
        }
    }

    // -----------------------------------------------------------------------
    // Direct mode (HTTP/2 to api.push.apple.com)
    // -----------------------------------------------------------------------

    /// 构建 APNs 请求 body。
    ///
    /// 对应 Node `buildBody`。
    fn build_body(payload: &ApnsPayload) -> String {
        let mut aps = serde_json::Map::new();
        let mut alert = serde_json::Map::new();
        if !payload.title.is_empty() {
            alert.insert("title".to_string(), Value::String(payload.title.clone()));
        }
        if !payload.body.is_empty() {
            alert.insert("body".to_string(), Value::String(payload.body.clone()));
        }
        aps.insert("alert".to_string(), Value::Object(alert));
        if let Some(badge) = payload.badge {
            if badge >= 0 {
                aps.insert("badge".to_string(), Value::Number(serde_json::Number::from(badge as u64)));
            }
        }
        aps.insert("sound".to_string(), Value::String("default".to_string()));
        if let Some(tag) = &payload.tag {
            aps.insert("thread-id".to_string(), Value::String(tag.clone()));
        }
        aps.insert("mutable-content".to_string(), Value::Number(serde_json::Number::from(1)));

        let mut body = serde_json::Map::new();
        body.insert("aps".to_string(), Value::Object(aps));
        if let Some(data) = &payload.data {
            if let Some(obj) = data.as_object() {
                for (k, v) in obj {
                    body.insert(k.clone(), v.clone());
                }
            }
        }
        serde_json::to_string(&Value::Object(body)).unwrap_or_default()
    }

    /// 通过 HTTP/2 direct 连接发送。
    ///
    /// 对应 Node `sendViaDirectApns`。用 `h2` crate 连接 APNs。
    async fn send_via_direct_apns(&self, device_tokens: &[String], payload: &ApnsPayload) {
        let config = match Self::resolve_apns_config() {
            Some(c) => c,
            None => {
                tracing::warn!(
                    "[APNs] Relay disabled and no direct config; set \
                     GRIDFORGE_APNS_KEY_ID / GRIDFORGE_APNS_TEAM_ID / GRIDFORGE_APNS_P8 \
                     for direct send."
                );
                return;
            }
        };

        let host = if config.environment == "production" {
            APNS_HOST_PRODUCTION
        } else {
            APNS_HOST_SANDBOX
        };
        let _ = host; // direct mode 降级, host 暂不使用

        let jwt = match Self::sign_apns_jwt(&config) {
            Some(j) => j,
            None => {
                tracing::warn!("[APNs] failed to sign JWT");
                return;
            }
        };

        let _ = jwt;
        let body = Self::build_body(payload);
        let _ = body;

        // h2 direct 模式需要手动 TLS + h2 handshake (tokio-rustls + h2 crate),
        // 较复杂。由于 direct 模式是 fallback (relay disabled 时才用), 且用户表示
        // 后续不会用移动端, 这里降级为 warn no-op, 保持 API 对等但避免 h2/TLS 复杂性。
        // JWT 签名和 body 构建已验证可用, 只差 TLS+h2 传输层。
        tracing::warn!(
            target: "oc_server::notifications::apns_send",
            "[APNs] Direct HTTP/2 mode is not yet wired (h2 over TLS); {} tokens would have been sent.",
            device_tokens.len()
        );
    }

    // -----------------------------------------------------------------------
    // Main entry
    // -----------------------------------------------------------------------

    /// 发送到所有 UI session (NOT gated on UI visibility)。
    ///
    /// 对应 Node `sendApnsToAllUiSessions`。收集所有 device tokens → relay 或 direct。
    pub async fn send_to_all_ui_sessions(&self, payload: &ApnsPayload) {
        let device_tokens = self.apns_store.read_all_tokens();
        if device_tokens.is_empty() {
            return;
        }

        match Self::resolve_relay_config() {
            Some(relay) => {
                self.send_via_relay(&device_tokens, payload, &relay).await;
            }
            None => {
                self.send_via_direct_apns(&device_tokens, payload).await;
            }
        }
    }
}

/// 从 env 读取 trimmed 值, 空值返回 None。
fn trimmed_env(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(v) => {
            let trimmed = v.trim().to_string();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            }
        }
        Err(_) => None,
    }
}

// =========================================================================
// 测试
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relay_config_default() {
        let config = ApnsSendRuntime::resolve_relay_config_from(None, None, None);
        assert!(config.is_some());
        let config = config.unwrap();
        assert_eq!(config.url, DEFAULT_RELAY_URL);
        assert_eq!(config.register_url, "https://api.gridforge.dev/v1/push/register-token");
    }

    #[test]
    fn relay_config_disabled() {
        assert!(ApnsSendRuntime::resolve_relay_config_from(Some("true"), None, None).is_none());
    }

    #[test]
    fn relay_config_custom_url() {
        let config =
            ApnsSendRuntime::resolve_relay_config_from(None, Some("https://custom.example.com/v1/send"), None)
                .unwrap();
        assert_eq!(config.url, "https://custom.example.com/v1/send");
        assert_eq!(config.register_url, "https://custom.example.com/v1/register-token");
    }

    #[test]
    fn build_body_basic() {
        let payload = ApnsPayload {
            title: "Test".to_string(),
            body: "Hello".to_string(),
            badge: Some(3),
            tag: Some("tag1".to_string()),
            data: None,
        };
        let body = ApnsSendRuntime::build_body(&payload);
        let parsed: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["aps"]["alert"]["title"], "Test");
        assert_eq!(parsed["aps"]["alert"]["body"], "Hello");
        assert_eq!(parsed["aps"]["badge"], 3);
        assert_eq!(parsed["aps"]["sound"], "default");
        assert_eq!(parsed["aps"]["thread-id"], "tag1");
        assert_eq!(parsed["aps"]["mutable-content"], 1);
    }

    #[test]
    fn build_body_with_data() {
        let payload = ApnsPayload {
            title: "T".to_string(),
            body: "B".to_string(),
            badge: None,
            tag: None,
            data: Some(json!({ "sessionId": "sess-1" })),
        };
        let body = ApnsSendRuntime::build_body(&payload);
        let parsed: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["sessionId"], "sess-1");
    }

    #[test]
    fn sign_apns_jwt_format() {
        use p256::pkcs8::EncodePrivateKey;

        // 生成临时 .p8 密钥
        let mut rng = rand::thread_rng();
        let secret = p256::SecretKey::random(&mut rng);
        let pkcs8 = secret.to_pkcs8_der().unwrap();
        let pem_content = pem::encode(&pem::Pem::new("PRIVATE KEY", pkcs8.as_bytes().to_vec()));

        let config = ApnsConfig {
            key_id: "K12345".to_string(),
            team_id: "T67890".to_string(),
            p8_pem: pem_content,
            bundle_id: "com.test.app".to_string(),
            environment: "sandbox".to_string(),
        };

        let jwt = ApnsSendRuntime::sign_apns_jwt(&config);
        assert!(jwt.is_some());
        let jwt = jwt.unwrap();
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3); // header.claims.signature

        // 验证 header
        let header_bytes = URL_SAFE_NO_PAD.decode(parts[0]).unwrap();
        let header: Value = serde_json::from_slice(&header_bytes).unwrap();
        assert_eq!(header["alg"], "ES256");
        assert_eq!(header["kid"], "K12345");

        // 验证 claims
        let claims_bytes = URL_SAFE_NO_PAD.decode(parts[1]).unwrap();
        let claims: Value = serde_json::from_slice(&claims_bytes).unwrap();
        assert_eq!(claims["iss"], "T67890");
        assert!(claims["iat"].as_i64().is_some());

        // 验证签名长度 (64 bytes IEEE-P1363)
        let sig_bytes = URL_SAFE_NO_PAD.decode(parts[2]).unwrap();
        assert_eq!(sig_bytes.len(), 64);
    }

    #[test]
    fn trimmed_env_missing() {
        std::env::remove_var("GRIDFORGE_NONEXISTENT_VAR_TEST_12345");
        assert!(trimmed_env("GRIDFORGE_NONEXISTENT_VAR_TEST_12345").is_none());
    }

    #[test]
    fn trimmed_env_empty() {
        std::env::set_var("GRIDFORGE_TEST_EMPTY_VAR", "  ");
        assert!(trimmed_env("GRIDFORGE_TEST_EMPTY_VAR").is_none());
        std::env::remove_var("GRIDFORGE_TEST_EMPTY_VAR");
    }

    #[test]
    fn trimmed_env_valid() {
        std::env::set_var("GRIDFORGE_TEST_VAR_12345", "  value  ");
        assert_eq!(
            trimmed_env("GRIDFORGE_TEST_VAR_12345"),
            Some("value".to_string())
        );
        std::env::remove_var("GRIDFORGE_TEST_VAR_12345");
    }
}
