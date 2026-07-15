//! ECDSA P-256 relay 签名身份 — getOrCreateKeypair + signRelayMessage + serverId。
//!
//! 对应 Node `relay/signing-key.js`。
//!
//! 密钥格式: `settings.relaySigningKey = { privateJwk, publicJwk }`。
//! serverId = `base64url(SHA-256(canonical public JWK))`。
//! 签名: ECDSA-SHA256 IEEE-P1363 (raw r||s), base64url 编码。
//!
//! **跨 Node/Rust 兼容**: p256 crate 的 `Signature` 默认就是 IEEE-P1363 64-byte 格式,
//! 与 Node 的 `crypto.sign(..., {dsaEncoding:'ieee-p1363'})` 字节对齐。

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use p256::ecdsa::signature::RandomizedSigner;
use p256::ecdsa::{Signature, SigningKey};
use p256::elliptic_curve::sec1::{Coordinates, ToEncodedPoint};
use p256::pkcs8::DecodePrivateKey;
use p256::{PublicKey, SecretKey};
use serde_json::{json, Value};

use crate::github::settings;

/// 从 public key 提取 JWK 格式。
fn public_key_to_jwk_coords(public: &PublicKey) -> Option<(String, String)> {
    let point = public.to_encoded_point(false);
    match point.coordinates() {
        Coordinates::Uncompressed { x, y } => {
            let x_b64 = URL_SAFE_NO_PAD.encode(x);
            let y_b64 = URL_SAFE_NO_PAD.encode(y);
            Some((x_b64, y_b64))
        }
        _ => None,
    }
}

/// 规范化 public JWK 字符串 (固定字段顺序)。
///
/// 对应 Node `canonicalPublicJwkString`: `JSON.stringify({crv, kty, x, y})`。
pub fn canonical_public_jwk_string(jwk: &Value) -> String {
    let crv = jwk.get("crv").cloned().unwrap_or(Value::Null);
    let kty = jwk.get("kty").cloned().unwrap_or(Value::Null);
    let x = jwk.get("x").cloned().unwrap_or(Value::Null);
    let y = jwk.get("y").cloned().unwrap_or(Value::Null);
    serde_json::to_string(&json!({ "crv": crv, "kty": kty, "x": x, "y": y }))
        .unwrap_or_default()
}

/// 派生 serverId: `base64url(SHA-256(canonical public JWK))`。
///
/// 对应 Node `deriveServerId`。
pub fn derive_server_id(public_jwk: &Value) -> String {
    use sha2::Digest;
    let canonical = canonical_public_jwk_string(public_jwk);
    let hash = sha2::Sha256::digest(canonical.as_bytes());
    URL_SAFE_NO_PAD.encode(hash)
}

/// 从 JWK 提取 public key。
fn jwk_to_public_key(jwk: &Value) -> Option<PublicKey> {
    let x_b64 = jwk.get("x")?.as_str()?;
    let y_b64 = jwk.get("y")?.as_str()?;
    let x_bytes = URL_SAFE_NO_PAD.decode(x_b64).ok()?;
    let y_bytes = URL_SAFE_NO_PAD.decode(y_b64).ok()?;
    if x_bytes.len() != 32 || y_bytes.len() != 32 {
        return None;
    }
    let mut sec1 = Vec::with_capacity(65);
    sec1.push(0x04); // uncompressed
    sec1.extend_from_slice(&x_bytes);
    sec1.extend_from_slice(&y_bytes);
    PublicKey::from_sec1_bytes(&sec1).ok()
}

/// 从 JWK 提取 secret key。
fn jwk_to_secret_key(private_jwk: &Value, public_jwk: &Value) -> Option<SecretKey> {
    let d_b64 = private_jwk.get("d")?.as_str()?;
    let d_bytes = URL_SAFE_NO_PAD.decode(d_b64).ok()?;
    if d_bytes.len() != 32 {
        return None;
    }
    let _ = public_jwk; // public key 可以从 secret key 恢复, 不需要单独解析
    SecretKey::from_slice(&d_bytes).ok()
}

/// 将 secret key 导出为 JWK (d 字段)。
fn secret_key_to_private_jwk(secret: &SecretKey) -> Value {
    let d_bytes = secret.to_bytes();
    let d = URL_SAFE_NO_PAD.encode(d_bytes);
    json!({ "kty": "EC", "crv": "P-256", "d": d })
}

/// 将 public key 导出为完整 JWK (crv, kty, x, y)。
fn public_key_to_jwk(public: &PublicKey) -> Value {
    match public_key_to_jwk_coords(public) {
        Some((x, y)) => json!({ "kty": "EC", "crv": "P-256", "x": x, "y": y }),
        None => json!({ "kty": "EC", "crv": "P-256", "x": "", "y": "" }),
    }
}

/// 获取或创建 relay 签名 keypair。
///
/// 对应 Node `getOrCreateRelaySigningKeypair`。
/// 从 settings.relaySigningKey 读取; 不存在时生成新 P-256 keypair 并持久化。
///
/// 返回 `(SigningKey, public_jwk)`。
pub fn get_or_create_relay_keypair() -> (SigningKey, Value) {
    let settings_val = settings::read_settings();

    if let Some(existing) = settings_val.get("relaySigningKey") {
        if let (Some(private_jwk), Some(public_jwk)) =
            (existing.get("privateJwk"), existing.get("publicJwk"))
        {
            if let Some(secret) = jwk_to_secret_key(private_jwk, public_jwk) {
                let signing_key = SigningKey::from(&secret);
                return (signing_key, public_jwk.clone());
            }
        }
    }

    // 生成新 keypair (首次运行)
    tracing::warn!(
        "[relay-identity] Generating NEW relay signing keypair (serverId changes; \
         previously paired devices must re-pair)"
    );

    let mut rng = rand::thread_rng();
    let signing_key = SigningKey::random(&mut rng);
    let public_key: PublicKey = signing_key.verifying_key().into();

    let private_jwk = secret_key_to_private_jwk(&SecretKey::from(&signing_key));
    let public_jwk = public_key_to_jwk(&public_key);

    // 持久化
    let mut next = settings_val;
    if let Value::Object(ref mut map) = next {
        map.insert(
            "relaySigningKey".to_string(),
            json!({ "privateJwk": private_jwk, "publicJwk": public_jwk }),
        );
    }
    let _ = settings::write_settings(&next);

    (signing_key, public_jwk)
}

/// 用 relay 私钥签名消息: ECDSA-SHA256 IEEE-P1363, base64url。
///
/// 对应 Node `signRelayMessage`。
pub fn sign_relay_message(signing_key: &SigningKey, message: &str) -> String {
    let mut rng = rand::thread_rng();
    let sig: Signature = signing_key.sign_with_rng(&mut rng, message.as_bytes());
    let der_bytes = sig.to_bytes();
    // p256 Signature::to_bytes() 返回 IEEE-P1363 fixed-size 64-byte (r||s)
    URL_SAFE_NO_PAD.encode(der_bytes)
}

/// 生成 VAPID 密钥对 (P-256)。
///
/// 返回 `(public_key_base64url, private_key_base64url)`。
/// web-push crate 不提供 keygen, 用 p256 生成 32-byte scalar。
pub fn generate_vapid_keys() -> (String, String) {
    let mut rng = rand::thread_rng();
    let secret = SecretKey::random(&mut rng);
    let public = secret.public_key();

    let private_b64 = URL_SAFE_NO_PAD.encode(secret.to_bytes());

    // public key: uncompressed SEC1 (65 bytes: 0x04 + x + y)
    let point = public.to_encoded_point(false);
    let public_b64 = match point.coordinates() {
        Coordinates::Uncompressed { x, y } => {
            let mut sec1 = Vec::with_capacity(65);
            sec1.push(0x04);
            sec1.extend_from_slice(x);
            sec1.extend_from_slice(y);
            URL_SAFE_NO_PAD.encode(&sec1)
        }
        _ => String::new(),
    };

    (public_b64, private_b64)
}

/// 从 settings 读取或创建 VAPID 密钥。
///
/// 对应 Node `getOrCreateVapidKeys`。
/// 返回 `(public_key_base64url, private_key_base64url)`。
pub fn get_or_create_vapid_keys() -> (String, String) {
    let settings_val = settings::read_settings();

    if let Some(existing) = settings_val.get("vapidKeys") {
        let public = existing.get("publicKey").and_then(|v| v.as_str()).unwrap_or("");
        let private = existing.get("privateKey").and_then(|v| v.as_str()).unwrap_or("");
        if !public.is_empty() && !private.is_empty() {
            return (public.to_string(), private.to_string());
        }
    }

    // 生成新密钥
    let (public, private) = generate_vapid_keys();

    let mut next = settings_val;
    if let Value::Object(ref mut map) = next {
        map.insert(
            "vapidKeys".to_string(),
            json!({ "publicKey": public, "privateKey": private }),
        );
    }
    let _ = settings::write_settings(&next);

    (public, private)
}

/// 从 ES256 .p8 PEM 内容构建 APNs ES256 签名密钥。
///
/// 用于 APNs direct 模式的 JWT 签名。
pub fn p8_to_signing_key(p8_pem: &str) -> Option<SigningKey> {
    // 解析 PEM
    let pem_content = pem::parse(p8_pem).ok()?;
    if pem_content.tag() != "PRIVATE KEY" {
        return None;
    }
    // PKCS#8 DER → p256 SecretKey → SigningKey
    let secret = SecretKey::from_pkcs8_der(pem_content.contents()).ok()?;
    Some(SigningKey::from(&secret))
}

// =========================================================================
// 测试
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_jwk_fixed_order() {
        let jwk = json!({ "y": "yval", "x": "xval", "kty": "EC", "crv": "P-256", "extra": "ignored" });
        let result = canonical_public_jwk_string(&jwk);
        // 字段顺序: crv, kty, x, y
        assert_eq!(result, r#"{"crv":"P-256","kty":"EC","x":"xval","y":"yval"}"#);
    }

    #[test]
    fn derive_server_id_deterministic() {
        let jwk = json!({ "crv": "P-256", "kty": "EC", "x": "abc", "y": "def" });
        let id1 = derive_server_id(&jwk);
        let id2 = derive_server_id(&jwk);
        assert_eq!(id1, id2);
        assert!(!id1.is_empty());
    }

    #[test]
    fn vapid_keys_roundtrip() {
        let (public, private) = generate_vapid_keys();
        // public = 65 bytes uncompressed → 87 chars base64url-no-pad
        // private = 32 bytes → 43 chars base64url-no-pad
        assert!(!public.is_empty());
        assert!(!private.is_empty());
        let pub_bytes = URL_SAFE_NO_PAD.decode(&public).unwrap();
        assert_eq!(pub_bytes.len(), 65);
        assert_eq!(pub_bytes[0], 0x04); // uncompressed prefix
        let priv_bytes = URL_SAFE_NO_PAD.decode(&private).unwrap();
        assert_eq!(priv_bytes.len(), 32);
    }

    #[test]
    fn sign_and_verify_relay_message() {
        use p256::ecdsa::signature::Verifier;

    let mut rng = rand::thread_rng();
    let signing_key = SigningKey::random(&mut rng);
    let verifying_key = signing_key.verifying_key();

    let message = "test message 12345";
    let sig_b64 = sign_relay_message(&signing_key, message);

    let sig_bytes = URL_SAFE_NO_PAD.decode(&sig_b64).unwrap();
    assert_eq!(sig_bytes.len(), 64); // IEEE-P1363 r||s
    let sig = Signature::from_slice(&sig_bytes).unwrap();
    assert!(
        verifying_key.verify(message.as_bytes(), &sig).is_ok(),
        "signature must verify"
    );
    }
}
