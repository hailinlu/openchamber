//! E2EE primitives + responder handshake for the private relay (Layer 2).
//!
//! Rust port of `packages/web/server/lib/relay/e2ee.js`. Byte-compatible with
//! the normative TS implementation in `packages/ui/src/lib/relay/{protocol,
//! crypto, handshake}.ts` — verified by `tests/cross_compat_vectors.rs`.
//!
//! Layout:
//! - **Encrypted frame**: `[1B version=0x01][12B IV][ciphertext+16B GCM tag]`
//! - **IV**: `[4B random per-direction prefix][8B big-endian frame counter]`
//! - **Handshake**: JSON text frames over WS:
//!   - `hello = { t:'hello', v:1, clientPubJwk, nonce (b64url, 16B), batch?:bool }`
//!   - `ready = { t:'ready', v:1, batch?:bool }`
//!
//! Primitives: ECDH P-256, HKDF-SHA256, AES-256-GCM.

use std::convert::TryInto;

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use hkdf::Hkdf;
use p256::ecdh::diffie_hellman;
use p256::elliptic_curve::sec1::{FromEncodedPoint, ToEncodedPoint};
use p256::{EncodedPoint, PublicKey, SecretKey};
use rand::rngs::OsRng;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use thiserror::Error;

use crate::relay::tunnel_codec::MAX_PLAINTEXT_FRAME_BYTES;

pub const RELAY_PROTOCOL_VERSION: u32 = 1;
pub const RELAY_HKDF_INFO: &[u8] = b"openchamber-relay-v1";

pub const ENCRYPTED_FRAME_VERSION: u8 = 1;
pub const ENCRYPTED_FRAME_IV_BYTES: usize = 12;
pub const ENCRYPTED_FRAME_HEADER_BYTES: usize = 1 + ENCRYPTED_FRAME_IV_BYTES;

const HANDSHAKE_NONCE_BYTES: usize = 16;
const SESSION_KEY_BYTES: usize = 32;
const GCM_TAG_BYTES: usize = 16;
const IV_PREFIX_BYTES: usize = 4;
const IV_COUNTER_BYTES: usize = 8;

/// Relay-assigned WebSocket close codes (subset the host needs).
pub mod close_codes {
    pub const REKEY_MISMATCH: u16 = 1008;
    pub const CHANNEL_FAILURE: u16 = 1011;
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum RelayCryptoError {
    #[error("invalid ECDH public key JWK")]
    InvalidPublicJwk,
    #[error("invalid ECDH private key JWK")]
    InvalidPrivateJwk,
    #[error("invalid handshake nonce length")]
    InvalidNonceLength,
    #[error("plaintext frame exceeds maximum size")]
    PlaintextTooLarge,
    #[error("encrypted frame too short")]
    EncryptedFrameTooShort,
    #[error("unsupported encrypted frame version")]
    UnsupportedVersion,
    #[error("frame counter regression")]
    CounterRegression,
    #[error("frame decryption failed")]
    DecryptionFailed,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum Base64UrlError {
    #[error("invalid base64url input")]
    Invalid,
}

/// ECDH P-256 keypair (Node `generateEcdhKeyPair` equivalent).
pub struct EcdhKeyPair {
    pub private_key: SecretKey,
    pub public_key: PublicKey,
}

/// Generate an ECDH P-256 keypair using the OS RNG.
pub fn generate_ecdh_keypair() -> EcdhKeyPair {
    let private_key = SecretKey::random(&mut OsRng);
    let public_key = private_key.public_key();
    EcdhKeyPair {
        private_key,
        public_key,
    }
}

/// Public-key JWK with only the fields that define the point (Node
/// `exportPublicKeyJwk`).
pub fn export_public_key_jwk(public: &PublicKey) -> serde_json::Value {
    let point = public.to_encoded_point(false);
    let coords = point.coordinates();
    let (x, y) = match coords {
        p256::elliptic_curve::sec1::Coordinates::Uncompressed { x, y } => (x, y),
        _ => unreachable!("uncompressed point always provides x,y"),
    };
    serde_json::json!({
        "kty": "EC",
        "crv": "P-256",
        "x": URL_SAFE_NO_PAD.encode(x),
        "y": URL_SAFE_NO_PAD.encode(y),
    })
}

/// Import an ECDH public-key JWK.
pub fn import_ecdh_public_key(jwk: &serde_json::Value) -> Result<PublicKey, RelayCryptoError> {
    let kty = jwk.get("kty").and_then(|v| v.as_str()).unwrap_or("");
    let crv = jwk.get("crv").and_then(|v| v.as_str()).unwrap_or("");
    let x = jwk.get("x").and_then(|v| v.as_str()).unwrap_or("");
    let y = jwk.get("y").and_then(|v| v.as_str()).unwrap_or("");
    if kty != "EC" || crv != "P-256" || x.is_empty() || y.is_empty() {
        return Err(RelayCryptoError::InvalidPublicJwk);
    }
    let x_bytes = URL_SAFE_NO_PAD
        .decode(x)
        .map_err(|_| RelayCryptoError::InvalidPublicJwk)?;
    let y_bytes = URL_SAFE_NO_PAD
        .decode(y)
        .map_err(|_| RelayCryptoError::InvalidPublicJwk)?;
    if x_bytes.len() != 32 || y_bytes.len() != 32 {
        return Err(RelayCryptoError::InvalidPublicJwk);
    }
    let mut sec1 = Vec::with_capacity(65);
    sec1.push(0x04);
    sec1.extend_from_slice(&x_bytes);
    sec1.extend_from_slice(&y_bytes);
    let encoded = EncodedPoint::from_bytes(&sec1).map_err(|_| RelayCryptoError::InvalidPublicJwk)?;
    let opt: Option<PublicKey> = Option::from(PublicKey::from_encoded_point(&encoded));
    opt.ok_or(RelayCryptoError::InvalidPublicJwk)
}

/// Import an ECDH private-key JWK (Node `importEcdhPrivateKey`).
pub fn import_ecdh_private_key(jwk: &serde_json::Value) -> Result<SecretKey, RelayCryptoError> {
    let d = jwk
        .get("d")
        .and_then(|v| v.as_str())
        .ok_or(RelayCryptoError::InvalidPrivateJwk)?;
    let d_bytes = URL_SAFE_NO_PAD
        .decode(d)
        .map_err(|_| RelayCryptoError::InvalidPrivateJwk)?;
    if d_bytes.len() != 32 {
        return Err(RelayCryptoError::InvalidPrivateJwk);
    }
    let arr: [u8; 32] = d_bytes
        .as_slice()
        .try_into()
        .map_err(|_| RelayCryptoError::InvalidPrivateJwk)?;
    SecretKey::from_slice(&arr).map_err(|_| RelayCryptoError::InvalidPrivateJwk)
}

/// Export an ECDH private key as a JWK (with `d` field).
pub fn export_ecdh_private_jwk(private: &SecretKey) -> serde_json::Value {
    let d = URL_SAFE_NO_PAD.encode(private.to_bytes());
    serde_json::json!({
        "kty": "EC",
        "crv": "P-256",
        "d": d,
    })
}

/// Stable fingerprint of a public key, used to detect rekey attempts
/// on re-hello (Node `publicKeyJwkFingerprint`).
pub fn public_key_jwk_fingerprint(jwk: &serde_json::Value) -> String {
    serde_json::to_string(&serde_json::json!({
        "crv": jwk.get("crv").cloned().unwrap_or(serde_json::Value::Null),
        "kty": jwk.get("kty").cloned().unwrap_or(serde_json::Value::Null),
        "x": jwk.get("x").cloned().unwrap_or(serde_json::Value::Null),
        "y": jwk.get("y").cloned().unwrap_or(serde_json::Value::Null),
    }))
    .unwrap_or_default()
}

/// Generate a 16-byte handshake nonce (Node `generateHandshakeNonce`).
pub fn generate_handshake_nonce() -> [u8; HANDSHAKE_NONCE_BYTES] {
    let mut nonce = [0u8; HANDSHAKE_NONCE_BYTES];
    OsRng.fill_bytes(&mut nonce);
    nonce
}

/// Per-direction AES-256-GCM session keys (Node `deriveSessionKeys`).
pub struct SessionKeys {
    pub client_to_host: Aes256Gcm,
    pub host_to_client: Aes256Gcm,
}

impl std::fmt::Debug for SessionKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionKeys")
            .field("client_to_host", &"<aes-gcm>")
            .field("host_to_client", &"<aes-gcm>")
            .finish()
    }
}

/// Derive both AES session keys from ECDH shared secret + handshake nonce.
/// HKDF-SHA256 with salt = nonce, info = "openchamber-relay-v1", L = 64 bytes.
pub fn derive_session_keys(
    own_private_key: &SecretKey,
    peer_public_key: &PublicKey,
    handshake_nonce: &[u8],
) -> Result<SessionKeys, RelayCryptoError> {
    if handshake_nonce.len() != HANDSHAKE_NONCE_BYTES {
        return Err(RelayCryptoError::InvalidNonceLength);
    }
    let shared_secret = diffie_hellman(own_private_key.to_nonzero_scalar(), peer_public_key.as_affine());
    let hkdf = Hkdf::<Sha256>::new(Some(handshake_nonce), shared_secret.raw_secret_bytes());
    let mut okm = [0u8; SESSION_KEY_BYTES * 2];
    hkdf.expand(RELAY_HKDF_INFO, &mut okm)
        .map_err(|_| RelayCryptoError::DecryptionFailed)?;

    let mut k1 = [0u8; SESSION_KEY_BYTES];
    let mut k2 = [0u8; SESSION_KEY_BYTES];
    k1.copy_from_slice(&okm[..SESSION_KEY_BYTES]);
    k2.copy_from_slice(&okm[SESSION_KEY_BYTES..]);

    Ok(SessionKeys {
        client_to_host: Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&k1)),
        host_to_client: Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&k2)),
    })
}

/// Frame encryptor (Node `createFrameEncryptor`): per-direction AES-GCM with
/// a random 4-byte IV prefix and a strictly-increasing 8-byte counter.
pub struct FrameEncryptor {
    cipher: Aes256Gcm,
    iv_prefix: [u8; IV_PREFIX_BYTES],
    counter: u64,
}

impl FrameEncryptor {
    pub fn new(cipher: Aes256Gcm) -> Self {
        let mut iv_prefix = [0u8; IV_PREFIX_BYTES];
        OsRng.fill_bytes(&mut iv_prefix);
        Self {
            cipher,
            iv_prefix,
            counter: 0,
        }
    }

    pub fn encrypt(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, RelayCryptoError> {
        if plaintext.len() > MAX_PLAINTEXT_FRAME_BYTES {
            return Err(RelayCryptoError::PlaintextTooLarge);
        }
        self.counter = self
            .counter
            .checked_add(1)
            .ok_or(RelayCryptoError::CounterRegression)?;

        let mut iv = [0u8; ENCRYPTED_FRAME_IV_BYTES];
        iv[..IV_PREFIX_BYTES].copy_from_slice(&self.iv_prefix);
        iv[IV_PREFIX_BYTES..].copy_from_slice(&self.counter.to_be_bytes());

        let nonce = Nonce::from_slice(&iv);
        let ciphertext = self
            .cipher
            .encrypt(nonce, plaintext)
            .map_err(|_| RelayCryptoError::DecryptionFailed)?;

        let mut frame = Vec::with_capacity(ENCRYPTED_FRAME_HEADER_BYTES + ciphertext.len());
        frame.push(ENCRYPTED_FRAME_VERSION);
        frame.extend_from_slice(&iv);
        frame.extend_from_slice(&ciphertext);
        Ok(frame)
    }
}

/// Frame decryptor (Node `createFrameDecryptor`): enforces strictly-increasing
/// per-direction counters and rejects any decryption failure as tampering.
pub struct FrameDecryptor {
    cipher: Aes256Gcm,
    last_counter: u64,
}

impl FrameDecryptor {
    pub fn new(cipher: Aes256Gcm) -> Self {
        Self {
            cipher,
            last_counter: 0,
        }
    }

    pub fn decrypt(&mut self, frame: &[u8]) -> Result<Vec<u8>, RelayCryptoError> {
        if frame.len() < ENCRYPTED_FRAME_HEADER_BYTES + GCM_TAG_BYTES {
            return Err(RelayCryptoError::EncryptedFrameTooShort);
        }
        if frame[0] != ENCRYPTED_FRAME_VERSION {
            return Err(RelayCryptoError::UnsupportedVersion);
        }
        let iv = &frame[1..ENCRYPTED_FRAME_HEADER_BYTES];
        let counter = u64::from_be_bytes(iv[IV_PREFIX_BYTES..].try_into().unwrap());
        if counter <= self.last_counter {
            return Err(RelayCryptoError::CounterRegression);
        }
        let nonce = Nonce::from_slice(iv);
        let payload = Payload {
            msg: &frame[ENCRYPTED_FRAME_HEADER_BYTES..],
            aad: &[],
        };
        let plaintext = self
            .cipher
            .decrypt(nonce, payload)
            .map_err(|_| RelayCryptoError::DecryptionFailed)?;
        self.last_counter = counter;
        Ok(plaintext)
    }
}

// Base64url (no padding) — custom alphabet matching Node `bytesToBase64Url` /
// `base64UrlToBytes`. Use `base64::engine::general_purpose::URL_SAFE_NO_PAD`
// where convenient; the Node implementation is RFC 4648 §5 with the same
// alphabet `A-Z a-z 0-9 - _`, no padding, which is exactly
// `URL_SAFE_NO_PAD`. So Node == base64 crate URL_SAFE_NO_PAD for this
// alphabet (verified by tests below).
pub fn bytes_to_base64_url(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

pub fn base64_url_to_bytes(value: &str) -> Result<Vec<u8>, Base64UrlError> {
    if value.len() % 4 == 1 {
        return Err(Base64UrlError::Invalid);
    }
    URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| Base64UrlError::Invalid)
}

// =========================================================================
// Host handshake state machine (Node `createHostHandshake`)
// =========================================================================

/// What the host does after consuming one inbound text frame.
#[derive(Debug, Clone)]
pub enum HandshakeAction {
    /// Send this plaintext text frame back to the peer.
    SendText(String),
    /// Handshake completed; `reply_text` should be sent before any encrypted
    /// frames. `channel` holds per-direction encryptors/decryptors; `batch`
    /// indicates whether batch envelopes were negotiated.
    Established {
        channel: HostChannel,
        batch: bool,
        reply_text: String,
    },
    /// Drop the frame silently.
    Ignore,
    /// Close the socket with the given code and reason.
    Fail { close_code: u16, reason: String },
}

#[derive(Clone)]
pub struct HostChannel {
    pub encryptor: FrameEncryptorClonable,
    pub decryptor: FrameDecryptorClonable,
}

impl std::fmt::Debug for HostChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostChannel")
            .field("encryptor", &"<aes-gcm>")
            .field("decryptor", &"<aes-gcm>")
            .finish()
    }
}

/// Cheap clone-able wrapper so HostChannel can be returned by value.
#[derive(Clone)]
pub struct FrameEncryptorClonable {
    pub(crate) inner: std::sync::Arc<std::sync::Mutex<FrameEncryptor>>,
}
impl FrameEncryptorClonable {
    pub fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>, RelayCryptoError> {
        self.inner.lock().unwrap().encrypt(plaintext)
    }

    /// Test-only: clone the underlying cipher so callers can pair an encryptor
    /// with a fresh decryptor using the same key. Production code does not need
    /// this — the channel decryptor is set up with the matching key during
    /// handshake.
    #[cfg(test)]
    pub fn cipher_for_test(&self) -> Aes256Gcm {
        self.inner.lock().unwrap().cipher.clone()
    }
}

#[derive(Clone)]
pub struct FrameDecryptorClonable {
    pub(crate) inner: std::sync::Arc<std::sync::Mutex<FrameDecryptor>>,
}
impl FrameDecryptorClonable {
    pub fn decrypt(&self, frame: &[u8]) -> Result<Vec<u8>, RelayCryptoError> {
        self.inner.lock().unwrap().decrypt(frame)
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct HelloMessage {
    #[serde(rename = "t")]
    t: String,
    #[serde(rename = "v")]
    v: u32,
    #[serde(rename = "clientPubJwk")]
    client_pub_jwk: serde_json::Value,
    nonce: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    batch: Option<bool>,
}

#[derive(Debug, Serialize)]
struct ReadyMessage<'a> {
    #[serde(rename = "t")]
    t: &'a str,
    #[serde(rename = "v")]
    v: u32,
    #[serde(rename = "batch", skip_serializing_if = "Option::is_none")]
    batch: Option<bool>,
}

fn parse_handshake(raw: &str) -> Option<ParsedMessage> {
    let parsed: serde_json::Value = serde_json::from_str(raw).ok()?;
    if !parsed.is_object() {
        return None;
    }
    let v = parsed.get("v")?.as_u64()?;
    if v as u32 != RELAY_PROTOCOL_VERSION {
        return None;
    }
    let t = parsed.get("t")?.as_str()?;
    let batch = parsed.get("batch").and_then(|b| b.as_bool()).unwrap_or(false);
    match t {
        "ready" => Some(ParsedMessage::Ready { batch }),
        "hello" => {
            let nonce = parsed.get("nonce")?.as_str()?.to_string();
            let client_pub_jwk = parsed.get("clientPubJwk")?.clone();
            Some(ParsedMessage::Hello {
                nonce,
                client_pub_jwk,
                batch,
            })
        }
        _ => None,
    }
}

#[derive(Debug)]
enum ParsedMessage {
    Hello {
        nonce: String,
        client_pub_jwk: serde_json::Value,
        batch: bool,
    },
    Ready {
        batch: bool,
    },
}

/// Responder handshake state machine (Node `createHostHandshake`).
pub struct HostHandshake {
    host_enc_private_key: SecretKey,
    local_batch: bool,
    established: bool,
    accepted_client_key_fingerprint: Option<String>,
    ready_text: Option<String>,
    negotiated_batch: bool,
}

impl HostHandshake {
    pub fn new(host_enc_private_key: SecretKey) -> Self {
        Self::with_options(host_enc_private_key, true)
    }

    /// `batch = false` forces legacy behavior on the host side.
    pub fn with_options(host_enc_private_key: SecretKey, batch: bool) -> Self {
        Self {
            host_enc_private_key,
            local_batch: batch,
            established: false,
            accepted_client_key_fingerprint: None,
            ready_text: None,
            negotiated_batch: false,
        }
    }

    pub fn is_established(&self) -> bool {
        self.established
    }

    pub fn handle_text(&mut self, raw: &str) -> HandshakeAction {
        let message = match parse_handshake(raw) {
            Some(m) => m,
            None => {
                if self.established {
                    return HandshakeAction::Fail {
                        close_code: close_codes::CHANNEL_FAILURE,
                        reason: "plaintext frame on established channel".to_string(),
                    };
                }
                return HandshakeAction::Ignore;
            }
        };

        let ParsedMessage::Hello {
            nonce,
            client_pub_jwk,
            batch: client_batch,
        } = message
        else {
            // Non-hello text (e.g. `ready`) on established channel = protocol violation.
            if self.established {
                return HandshakeAction::Fail {
                    close_code: close_codes::CHANNEL_FAILURE,
                    reason: "plaintext frame on established channel".to_string(),
                };
            }
            // Non-hello before established — ignore.
            return HandshakeAction::Ignore;
        };

        let fingerprint = public_key_jwk_fingerprint(&client_pub_jwk);
        if let Some(accepted) = &self.accepted_client_key_fingerprint {
            if accepted == &fingerprint {
                if let Some(ready) = &self.ready_text {
                    return HandshakeAction::SendText(ready.clone());
                }
            }
            return HandshakeAction::Fail {
                close_code: close_codes::REKEY_MISMATCH,
                reason: "rekey mismatch".to_string(),
            };
        }

        let client_public_key = match import_ecdh_public_key(&client_pub_jwk) {
            Ok(k) => k,
            Err(_) => {
                return HandshakeAction::Fail {
                    close_code: close_codes::CHANNEL_FAILURE,
                    reason: "malformed hello".to_string(),
                };
            }
        };
        let nonce_bytes = match base64_url_to_bytes(&nonce) {
            Ok(b) => b,
            Err(_) => {
                return HandshakeAction::Fail {
                    close_code: close_codes::CHANNEL_FAILURE,
                    reason: "malformed hello".to_string(),
                };
            }
        };
        let keys = match derive_session_keys(&self.host_enc_private_key, &client_public_key, &nonce_bytes) {
            Ok(k) => k,
            Err(_) => {
                return HandshakeAction::Fail {
                    close_code: close_codes::CHANNEL_FAILURE,
                    reason: "key derivation failed".to_string(),
                };
            }
        };

        self.accepted_client_key_fingerprint = Some(fingerprint);
        self.negotiated_batch = self.local_batch && client_batch;
        let ready_msg = if self.negotiated_batch {
            ReadyMessage {
                t: "ready",
                v: RELAY_PROTOCOL_VERSION,
                batch: Some(true),
            }
        } else {
            ReadyMessage {
                t: "ready",
                v: RELAY_PROTOCOL_VERSION,
                batch: None,
            }
        };
        let ready_text = serde_json::to_string(&ready_msg).unwrap();
        self.ready_text = Some(ready_text.clone());
        self.established = true;

        HandshakeAction::Established {
            channel: HostChannel {
                encryptor: FrameEncryptorClonable {
                    inner: std::sync::Arc::new(std::sync::Mutex::new(FrameEncryptor::new(
                        keys.host_to_client,
                    ))),
                },
                decryptor: FrameDecryptorClonable {
                    inner: std::sync::Arc::new(std::sync::Mutex::new(FrameDecryptor::new(
                        keys.client_to_host,
                    ))),
                },
            },
            batch: self.negotiated_batch,
            reply_text: ready_text,
        }
    }
}

/// Construct a host handshake with default `batch=true`.
pub fn create_host_handshake(host_enc_private_key: SecretKey) -> HostHandshake {
    HostHandshake::new(host_enc_private_key)
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ecdh_keypair_generation_and_jwk_roundtrip() {
        let kp = generate_ecdh_keypair();
        let jwk = export_public_key_jwk(&kp.public_key);
        assert_eq!(jwk["kty"], "EC");
        assert_eq!(jwk["crv"], "P-256");
        let recovered = import_ecdh_public_key(&jwk).unwrap();
        assert_eq!(
            kp.public_key.to_encoded_point(true).as_bytes(),
            recovered.to_encoded_point(true).as_bytes()
        );
    }

    #[test]
    fn ecdh_private_jwk_roundtrip() {
        let kp = generate_ecdh_keypair();
        let private_jwk = export_ecdh_private_jwk(&kp.private_key);
        let recovered = import_ecdh_private_key(&private_jwk).unwrap();
        assert_eq!(kp.private_key.to_bytes(), recovered.to_bytes());
    }

    #[test]
    fn session_keys_derive_symmetrically() {
        let host_kp = generate_ecdh_keypair();
        let client_kp = generate_ecdh_keypair();
        let nonce = generate_handshake_nonce();

        let host_keys = derive_session_keys(&host_kp.private_key, &client_kp.public_key, &nonce).unwrap();
        let client_keys =
            derive_session_keys(&client_kp.private_key, &host_kp.public_key, &nonce).unwrap();

        // Both sides must derive the same key material for both directions.
        let host_c2h = host_keys.client_to_host;
        let client_c2h = client_keys.host_to_client;
        // Encrypt from host with `host_to_client`, decrypt from client with `host_to_client`.
        let plaintext = b"hello from host to client";
        let ct = host_keys
            .host_to_client
            .encrypt(Nonce::from_slice(&[0u8; 12]), plaintext.as_ref())
            .unwrap();
        let pt = client_c2h
            .decrypt(Nonce::from_slice(&[0u8; 12]), ct.as_ref())
            .unwrap();
        assert_eq!(pt, plaintext);

        let plaintext2 = b"hello from client to host";
        let ct2 = client_keys
            .client_to_host
            .encrypt(Nonce::from_slice(&[0u8; 12]), plaintext2.as_ref())
            .unwrap();
        let pt2 = host_c2h
            .decrypt(Nonce::from_slice(&[0u8; 12]), ct2.as_ref())
            .unwrap();
        assert_eq!(pt2, plaintext2);

        // Silence unused warning — both keys must be referenced.
        let _ = (client_keys.client_to_host, host_keys.host_to_client);
    }

    #[test]
    fn nonce_length_validation() {
        let kp = generate_ecdh_keypair();
        let kp2 = generate_ecdh_keypair();
        let err = derive_session_keys(&kp.private_key, &kp2.public_key, &[0u8; 15]).unwrap_err();
        assert_eq!(err, RelayCryptoError::InvalidNonceLength);
    }

    #[test]
    fn frame_encrypt_decrypt_roundtrip() {
        let kp = generate_ecdh_keypair();
        let nonce = generate_handshake_nonce();
        let keys = derive_session_keys(&kp.private_key, &kp.public_key, &nonce).unwrap();
        // Pair of independent keys for client→host and host→client directions.
        // For roundtrip on one direction, encrypt and decrypt must use the same key.
        let mut enc = FrameEncryptor::new(keys.host_to_client.clone());
        let mut dec = FrameDecryptor::new(keys.host_to_client.clone());

        let frame = enc.encrypt(b"plaintext 1").unwrap();
        let recovered = dec.decrypt(&frame).unwrap();
        assert_eq!(recovered, b"plaintext 1");

        let frame2 = enc.encrypt(b"plaintext 2").unwrap();
        let recovered2 = dec.decrypt(&frame2).unwrap();
        assert_eq!(recovered2, b"plaintext 2");
    }

    #[test]
    fn frame_tamper_rejected() {
        let kp = generate_ecdh_keypair();
        let nonce = generate_handshake_nonce();
        let keys = derive_session_keys(&kp.private_key, &kp.public_key, &nonce).unwrap();
        let mut enc = FrameEncryptor::new(keys.host_to_client.clone());
        let mut dec = FrameDecryptor::new(keys.client_to_host.clone());

        let mut frame = enc.encrypt(b"tamper me").unwrap();
        let last = frame.len() - 1;
        frame[last] ^= 0xff;
        let err = dec.decrypt(&frame).unwrap_err();
        assert_eq!(err, RelayCryptoError::DecryptionFailed);
    }

    #[test]
    fn frame_counter_regression_rejected() {
        let kp = generate_ecdh_keypair();
        let nonce = generate_handshake_nonce();
        let keys = derive_session_keys(&kp.private_key, &kp.public_key, &nonce).unwrap();
        let mut enc = FrameEncryptor::new(keys.host_to_client.clone());
        let mut dec = FrameDecryptor::new(keys.host_to_client.clone());

        let frame = enc.encrypt(b"first").unwrap();
        let _ = dec.decrypt(&frame).unwrap();
        let err = dec.decrypt(&frame).unwrap_err();
        assert_eq!(err, RelayCryptoError::CounterRegression);
    }

    #[test]
    fn frame_too_short_rejected() {
        let kp = generate_ecdh_keypair();
        let nonce = generate_handshake_nonce();
        let keys = derive_session_keys(&kp.private_key, &kp.public_key, &nonce).unwrap();
        let mut dec = FrameDecryptor::new(keys.client_to_host.clone());
        let err = dec.decrypt(&[0u8; 10]).unwrap_err();
        assert_eq!(err, RelayCryptoError::EncryptedFrameTooShort);
    }

    #[test]
    fn frame_wrong_version_rejected() {
        let kp = generate_ecdh_keypair();
        let nonce = generate_handshake_nonce();
        let keys = derive_session_keys(&kp.private_key, &kp.public_key, &nonce).unwrap();
        let mut dec = FrameDecryptor::new(keys.client_to_host.clone());
        let mut fake = vec![0x99]; // wrong version
        fake.extend_from_slice(&[0u8; 28]); // 12 IV + 16 tag minimum
        let err = dec.decrypt(&fake).unwrap_err();
        assert_eq!(err, RelayCryptoError::UnsupportedVersion);
    }

    #[test]
    fn plaintext_too_large_rejected() {
        let kp = generate_ecdh_keypair();
        let nonce = generate_handshake_nonce();
        let keys = derive_session_keys(&kp.private_key, &kp.public_key, &nonce).unwrap();
        let mut enc = FrameEncryptor::new(keys.host_to_client.clone());
        let big = vec![0u8; MAX_PLAINTEXT_FRAME_BYTES + 1];
        let err = enc.encrypt(&big).unwrap_err();
        assert_eq!(err, RelayCryptoError::PlaintextTooLarge);
    }

    #[test]
    fn base64url_roundtrip() {
        let bytes = b"hello base64url world";
        let encoded = bytes_to_base64_url(bytes);
        let decoded = base64_url_to_bytes(&encoded).unwrap();
        assert_eq!(decoded, bytes);
    }

    #[test]
    fn base64url_no_padding() {
        let bytes = b"a";
        let encoded = bytes_to_base64_url(bytes);
        assert!(!encoded.contains('='));
    }

    #[test]
    fn base64url_invalid_input() {
        // Length 1 mod 4 is invalid for base64url.
        let err = base64_url_to_bytes("A").unwrap_err();
        assert_eq!(err, Base64UrlError::Invalid);
    }

    #[test]
    fn fingerprint_is_stable() {
        let kp = generate_ecdh_keypair();
        let jwk = export_public_key_jwk(&kp.public_key);
        let f1 = public_key_jwk_fingerprint(&jwk);
        let f2 = public_key_jwk_fingerprint(&jwk);
        assert_eq!(f1, f2);
        assert!(f1.contains("\"crv\":\"P-256\""));
        assert!(f1.contains("\"kty\":\"EC\""));
    }

    #[test]
    fn handshake_happy_path() {
        let host_kp = generate_ecdh_keypair();
        let client_kp = generate_ecdh_keypair();
        let client_pub_jwk = export_public_key_jwk(&client_kp.public_key);
        let nonce = generate_handshake_nonce();
        let nonce_b64 = bytes_to_base64_url(&nonce);

        let mut host = HostHandshake::new(host_kp.private_key.clone());
        let hello = serde_json::json!({
            "t": "hello",
            "v": 1,
            "clientPubJwk": client_pub_jwk,
            "nonce": nonce_b64,
            "batch": true,
        });
        let hello_text = serde_json::to_string(&hello).unwrap();

        let action = host.handle_text(&hello_text);
        match action {
            HandshakeAction::Established { batch, reply_text, channel } => {
                assert!(batch);
                let parsed: serde_json::Value = serde_json::from_str(&reply_text).unwrap();
                assert_eq!(parsed["t"], "ready");
                assert_eq!(parsed["v"], 1);
                assert_eq!(parsed["batch"], true);
                // Channel encryptor uses hostToClient; build a peer decryptor with the same key for the roundtrip.
                let mut enc = channel.encryptor.clone();
                let mut dec = FrameDecryptor::new(channel.encryptor.cipher_for_test());
                let frame = enc.encrypt(b"hello back").unwrap();
                let plain = dec.decrypt(&frame).unwrap();
                assert_eq!(plain, b"hello back");
            }
            other => panic!("expected Established, got {:?}", other),
        }
        assert!(host.is_established());
    }

    #[test]
    fn handshake_rehello_same_key_resends_ready() {
        let host_kp = generate_ecdh_keypair();
        let client_kp = generate_ecdh_keypair();
        let client_pub_jwk = export_public_key_jwk(&client_kp.public_key);
        let nonce = generate_handshake_nonce();
        let nonce_b64 = bytes_to_base64_url(&nonce);

        let mut host = HostHandshake::new(host_kp.private_key.clone());
        let hello = serde_json::json!({
            "t": "hello",
            "v": 1,
            "clientPubJwk": client_pub_jwk,
            "nonce": nonce_b64,
            "batch": true,
        });
        let hello_text = serde_json::to_string(&hello).unwrap();

        let action1 = host.handle_text(&hello_text);
        assert!(matches!(action1, HandshakeAction::Established { .. }));

        let action2 = host.handle_text(&hello_text);
        match action2 {
            HandshakeAction::SendText(text) => {
                let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
                assert_eq!(parsed["t"], "ready");
            }
            other => panic!("expected SendText, got {:?}", other),
        }
    }

    #[test]
    fn handshake_rehello_different_key_fails_1008() {
        let host_kp = generate_ecdh_keypair();
        let client_a = generate_ecdh_keypair();
        let client_b = generate_ecdh_keypair();
        let nonce = generate_handshake_nonce();
        let nonce_b64 = bytes_to_base64_url(&nonce);

        let mut host = HostHandshake::new(host_kp.private_key.clone());

        let hello_a = serde_json::to_string(&serde_json::json!({
            "t": "hello",
            "v": 1,
            "clientPubJwk": export_public_key_jwk(&client_a.public_key),
            "nonce": nonce_b64,
            "batch": false,
        }))
        .unwrap();
        let _ = host.handle_text(&hello_a);

        let hello_b = serde_json::to_string(&serde_json::json!({
            "t": "hello",
            "v": 1,
            "clientPubJwk": export_public_key_jwk(&client_b.public_key),
            "nonce": nonce_b64,
            "batch": false,
        }))
        .unwrap();
        let action = host.handle_text(&hello_b);
        match action {
            HandshakeAction::Fail { close_code, reason } => {
                assert_eq!(close_code, close_codes::REKEY_MISMATCH);
                assert_eq!(reason, "rekey mismatch");
            }
            other => panic!("expected Fail, got {:?}", other),
        }
    }

    #[test]
    fn handshake_plaintext_after_established_fails_1011() {
        let host_kp = generate_ecdh_keypair();
        let client_kp = generate_ecdh_keypair();
        let nonce = generate_handshake_nonce();
        let nonce_b64 = bytes_to_base64_url(&nonce);

        let mut host = HostHandshake::new(host_kp.private_key.clone());
        let hello = serde_json::to_string(&serde_json::json!({
            "t": "hello",
            "v": 1,
            "clientPubJwk": export_public_key_jwk(&client_kp.public_key),
            "nonce": nonce_b64,
            "batch": true,
        }))
        .unwrap();
        let _ = host.handle_text(&hello);

        let action = host.handle_text("{\"t\":\"ready\",\"v\":1,\"batch\":true}");
        match action {
            HandshakeAction::Fail { close_code, .. } => {
                assert_eq!(close_code, close_codes::CHANNEL_FAILURE);
            }
            other => panic!("expected Fail, got {:?}", other),
        }
    }

    #[test]
    fn handshake_malformed_hello_fails_1011() {
        let host_kp = generate_ecdh_keypair();
        let mut host = HostHandshake::new(host_kp.private_key.clone());

        // bad nonce (length 1 mod 4 = invalid base64url)
        let action = host.handle_text(
            r#"{"t":"hello","v":1,"clientPubJwk":{"kty":"EC","crv":"P-256","x":"a","y":"b"},"nonce":"A"}"#,
        );
        match action {
            HandshakeAction::Fail { close_code, .. } => {
                assert_eq!(close_code, close_codes::CHANNEL_FAILURE);
            }
            other => panic!("expected Fail, got {:?}", other),
        }
    }

    #[test]
    fn handshake_legacy_no_batch() {
        let host_kp = generate_ecdh_keypair();
        let client_kp = generate_ecdh_keypair();
        let nonce = generate_handshake_nonce();
        let nonce_b64 = bytes_to_base64_url(&nonce);

        // Host advertises batch, client omits -> negotiated off
        let mut host = HostHandshake::with_options(host_kp.private_key.clone(), true);
        let hello = serde_json::to_string(&serde_json::json!({
            "t": "hello",
            "v": 1,
            "clientPubJwk": export_public_key_jwk(&client_kp.public_key),
            "nonce": nonce_b64,
        }))
        .unwrap();
        let action = host.handle_text(&hello);
        match action {
            HandshakeAction::Established { batch, reply_text, .. } => {
                assert!(!batch);
                let parsed: serde_json::Value = serde_json::from_str(&reply_text).unwrap();
                assert!(parsed.get("batch").is_none());
            }
            other => panic!("expected Established, got {:?}", other),
        }
    }
}