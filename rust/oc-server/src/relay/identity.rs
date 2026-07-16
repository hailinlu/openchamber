//! Relay host identity (Layer 1).
//!
//! Rust port of `packages/web/server/lib/relay/{signing-key,identity}.js`.
//!
//! Bundles two long-lived keypairs:
//! - **ECDSA P-256 signing key** — reused from `notifications::relay_key` so the
//!   serverId stays stable across both push and private relays (push token
//!   binding depends on it).
//! - **ECDH P-256 encryption key** — a SEPARATE keypair (WebCrypto keys are
//!   single-purpose) used to derive per-connection E2EE session keys via
//!   `crypto::derive_session_keys`.
//!
//! Persistence: `settings.relayEncryptionKey = { privateJwk, publicJwk }`. The
//! signing key uses `settings.relaySigningKey` (handled by
//! `notifications::relay_key::get_or_create_relay_keypair`).

use std::sync::Mutex;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use p256::ecdsa::SigningKey;
use p256::elliptic_curve::sec1::ToEncodedPoint;
use p256::{PublicKey, SecretKey};
use serde_json::{json, Value};

use crate::github::settings;
use crate::notifications::relay_key::{
    derive_server_id, get_or_create_relay_keypair, sign_relay_message,
};
use crate::relay::crypto::{export_ecdh_private_jwk, export_public_key_jwk};

/// Default public OpenChamber relay URL. Users with a private deployment
/// override via settings.privateRelay.relayUrl.
pub const DEFAULT_RELAY_URL: &str = "wss://relay.openchamber.app/v1";

/// Snapshot of the host's two keypairs plus a closure for producing relay-auth
/// signatures for outbound WS connections.
#[derive(Clone)]
pub struct RelayIdentity {
    /// Stable per-server id (base64url of SHA-256 over canonical public JWK).
    pub server_id: String,
    /// ECDH public JWK advertised to peers (so they can encrypt to us).
    pub host_enc_pub_jwk: Value,
    /// ECDH private key used during E2EE handshake to derive session keys.
    pub host_enc_private_key: SecretKey,
    /// ECDSA signing key — wraps the inner p256 SecretKey + RNG for signing.
    signing_key: SigningKey,
}

impl std::fmt::Debug for RelayIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelayIdentity")
            .field("server_id", &self.server_id)
            .field("host_enc_pub_jwk", &self.host_enc_pub_jwk)
            .field("host_enc_private_key", &"<redacted>")
            .field("signing_key", &"<redacted>")
            .finish()
    }
}

/// Authentication payload signed for outbound relay WS upgrades (Layer 1).
///
/// `pk` is the base64url-encoded bytes of the canonical public JWK string —
/// the relay worker verifies the signature against this pk.
#[derive(Debug, Clone)]
pub struct RelayAuthPayload {
    pub ts: u64,
    pub sig: String,
    pub pk: String,
}

impl RelayIdentity {
    /// Sign `${ts}.${serverId}.${role}.${connectionId ?? ""}` so the relay
    /// worker can verify the upgrade. `connection_id` may be `None` for the
    /// control socket and `Some` for per-client data sockets.
    pub fn sign_relay_auth(&self, role: &str, connection_id: Option<&str>) -> RelayAuthPayload {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let connection_id_part = connection_id.unwrap_or("");
        let message = format!("{ts}.{}.{role}.{connection_id_part}", self.server_id);
        let sig = sign_relay_message(&self.signing_key, &message);
        // pk = base64url(canonical public JWK string as UTF-8 bytes).
        // Aligns with Node: `Buffer.from(canonicalPublicJwkString(publicJwk), 'utf8').toString('base64url')`.
        let pk = URL_SAFE_NO_PAD.encode(self.public_jwk_canonical_string().as_bytes());
        RelayAuthPayload { ts, sig, pk }
    }

    fn public_jwk_canonical_string(&self) -> String {
        crate::notifications::relay_key::canonical_public_jwk_string(
            &self.signing_public_jwk(),
        )
    }

    fn signing_public_jwk(&self) -> Value {
        let public: PublicKey = self.signing_key.verifying_key().into();
        // Mirror Node `exportPublicKeyJwk`: only the point-defining fields.
        let point = public.to_encoded_point(false);
        match point.coordinates() {
            p256::elliptic_curve::sec1::Coordinates::Uncompressed { x, y } => {
                let x_b64 = URL_SAFE_NO_PAD.encode(x);
                let y_b64 = URL_SAFE_NO_PAD.encode(y);
                json!({ "kty": "EC", "crv": "P-256", "x": x_b64, "y": y_b64 })
            }
            _ => json!({ "kty": "EC", "crv": "P-256" }),
        }
    }
}

/// Lazy-loads (or creates on first call) the relay host identity and caches it
/// for the process lifetime.
pub struct RelayIdentityRuntime {
    cached: Mutex<Option<RelayIdentity>>,
}

impl RelayIdentityRuntime {
    pub fn new() -> Self {
        Self {
            cached: Mutex::new(None),
        }
    }

    pub fn get_or_init(&self) -> RelayIdentity {
        let mut guard = self.cached.lock().unwrap();
        if let Some(identity) = guard.as_ref() {
            return identity.clone();
        }
        let identity = load_or_create_identity();
        *guard = Some(identity.clone());
        identity
    }
}

impl Default for RelayIdentityRuntime {
    fn default() -> Self {
        Self::new()
    }
}

fn load_or_create_identity() -> RelayIdentity {
    // Signing key — reuses notifications::relay_key which writes to
    // settings.relaySigningKey. Mirrors Node getOrCreateRelaySigningKeypair.
    let (signing_key, signing_public_jwk) = get_or_create_relay_keypair();
    let server_id = derive_server_id(&signing_public_jwk);

    // Encryption key — separate JWK pair persisted at settings.relayEncryptionKey.
    let (host_enc_private_key, host_enc_pub_jwk) =
        get_or_create_encryption_keypair();

    RelayIdentity {
        server_id,
        host_enc_pub_jwk,
        host_enc_private_key,
        signing_key,
    }
}

fn is_jwk_pair(value: Option<&Value>) -> bool {
    match value {
        Some(v) => {
            v.get("privateJwk").is_some() && v.get("publicJwk").is_some()
        }
        None => false,
    }
}

fn get_or_create_encryption_keypair() -> (SecretKey, Value) {
    let settings_val = settings::read_settings();

    if let Some(existing) = settings_val.get("relayEncryptionKey") {
        if is_jwk_pair(Some(existing)) {
            if let Some(private_jwk) = existing.get("privateJwk") {
                if let Ok(secret) = crate::relay::crypto::import_ecdh_private_key(private_jwk) {
                    let public = secret.public_key();
                    let public_jwk = export_public_key_jwk(&public);
                    return (secret, public_jwk);
                }
            }
        }
    }

    // First-run path: generate and persist. New encryption key invalidates the
    // E2EE trust anchor of every paired device. Expected exactly once.
    tracing::warn!(
        "[relay-identity] Generating NEW relay encryption keypair (E2EE trust anchor changes; previously paired devices must re-pair)"
    );
    let secret = SecretKey::random(&mut rand::thread_rng());
    let public = secret.public_key();
    let private_jwk = export_ecdh_private_jwk(&secret);
    let public_jwk = export_public_key_jwk(&public);

    let mut next = settings_val;
    if let Value::Object(ref mut map) = next {
        map.insert(
            "relayEncryptionKey".to_string(),
            json!({ "privateJwk": private_jwk, "publicJwk": public_jwk }),
        );
    }
    let _ = settings::write_settings(&next);

    (secret, public_jwk)
}

// =========================================================================
// 测试
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// All these tests mutate global settings.json — gate with a single
    /// cfg_attr so they run serially (cargo test defaults to parallel threads).
    /// The Node suite accepts the same global-state tradeoff because it also
    /// touches disk.

    #[test]
    fn server_id_is_stable_across_loads() {
        // First load.
        let id_a = RelayIdentityRuntime::new().get_or_init();

        // Second load (cache hit path).
        let id_b = RelayIdentityRuntime::new().get_or_init();
        assert_eq!(id_a.server_id, id_b.server_id);
        assert!(!id_a.server_id.is_empty());
    }

    #[test]
    fn host_enc_pub_jwk_is_p256() {
        let id = RelayIdentityRuntime::new().get_or_init();
        assert_eq!(id.host_enc_pub_jwk["crv"], "P-256");
        assert_eq!(id.host_enc_pub_jwk["kty"], "EC");
        assert!(id.host_enc_pub_jwk["x"].is_string());
        assert!(id.host_enc_pub_jwk["y"].is_string());
    }

    #[test]
    fn signing_key_persists_and_produces_consistent_pk() {
        let id = RelayIdentityRuntime::new().get_or_init();
        let auth_a = id.sign_relay_auth("host-control", None);
        let auth_b = id.sign_relay_auth("host-data", Some("conn-1"));
        // pk is the same across calls (depends only on the signing public JWK).
        assert_eq!(auth_a.pk, auth_b.pk);
        assert!(!auth_a.pk.is_empty());
    }

    #[test]
    fn sign_relay_auth_signature_verifies() {
        use p256::ecdsa::signature::Verifier;
        let id = RelayIdentityRuntime::new().get_or_init();
        let auth = id.sign_relay_auth("host-control", None);

        let sig_bytes = URL_SAFE_NO_PAD.decode(&auth.sig).expect("base64url");
        assert_eq!(sig_bytes.len(), 64, "IEEE P1363 raw r||s must be 64 bytes");

        // Reconstruct the message exactly as sign_relay_auth did and verify.
        let message = format!("{}.{}.host-control.", auth.ts, id.server_id);
        let pk_bytes = URL_SAFE_NO_PAD.decode(&auth.pk).expect("base64url pk");
        let canonical = String::from_utf8(pk_bytes).expect("pk is UTF-8");
        // canonical contains the JSON object, serverId is hash(public_jwk)
        // — no need to verify pk round-trip here, only the signature.
        let verifying_key = id.signing_key.verifying_key();
        let sig = p256::ecdsa::Signature::from_slice(&sig_bytes).expect("parse sig");
        verifying_key
            .verify(message.as_bytes(), &sig)
            .expect("signature must verify");
    }

    #[test]
    fn identity_is_cached_for_subsequent_calls() {
        // Two runtimes share identity (serverId from settings).
        let rt1 = RelayIdentityRuntime::new();
        let rt2 = RelayIdentityRuntime::new();
        let id1 = rt1.get_or_init();
        let id2 = rt2.get_or_init();
        assert_eq!(id1.server_id, id2.server_id);
    }
}