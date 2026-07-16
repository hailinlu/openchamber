//! Cross-compatibility byte-vector tests. Reads the frozen JSON fixture generated
//! by `scripts/generate-relay-fixtures.mjs` and asserts that Rust's relay module
//! output bytes match the normative JS/TS implementation byte-for-byte.
//!
//! Fixture regeneration: run `bun run scripts/generate-relay-fixtures.mjs` from
//! the repo root whenever the JS/TS relay modules change.

use crate::relay::*;

const FIXTURE: &str = include_str!("fixtures/cross_compat_vectors.json");

fn fixture() -> serde_json::Value {
    serde_json::from_str(FIXTURE).expect("fixture is valid JSON")
}

// ---------------------------------------------------------------------------
// Tunnel frame encoding: Rust bytes must match JS bytes exactly.
// ---------------------------------------------------------------------------

#[test]
fn tunnel_frame_encoding_matches_js() {
    let f = fixture();
    let tf = &f["tunnel_frame"];

    let payload = base64_url_to_bytes(tf["payload_b64"].as_str().unwrap()).unwrap();
    let expected =
        base64_url_to_bytes(tf["encoded_b64"].as_str().unwrap()).unwrap();

    let rust_frame = encode_tunnel_frame(
        TunnelFrameType::HttpRequest,
        tf["stream_id"].as_u64().unwrap() as u32,
        &payload,
    );

    assert_eq!(
        rust_frame, expected,
        "tunnel frame encoding differs from JS"
    );
}

// ---------------------------------------------------------------------------
// Batch encoding: Rust encode_frame_batch bytes must match JS.
// ---------------------------------------------------------------------------

#[test]
fn batch_encoding_single_frame_matches_js() {
    let f = fixture();
    let batch = &f["batch"];

    let raw_frame = base64_url_to_bytes(batch["single_raw_frame_b64"].as_str().unwrap()).unwrap();
    let expected =
        base64_url_to_bytes(batch["single_frame_b64"].as_str().unwrap()).unwrap();

    let rust_batch = encode_frame_batch(&[raw_frame]).unwrap();

    assert_eq!(
        rust_batch, expected,
        "single-frame batch encoding differs from JS"
    );
}

#[test]
fn batch_encoding_multi_frame_matches_js() {
    let f = fixture();
    let batch = &f["batch"];

    let raw_frames: Vec<Vec<u8>> = batch["multi_raw_frames"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| base64_url_to_bytes(v.as_str().unwrap()).unwrap())
        .collect();
    let expected =
        base64_url_to_bytes(batch["multi_frame_b64"].as_str().unwrap()).unwrap();

    let rust_batch = encode_frame_batch(&raw_frames).unwrap();

    assert_eq!(
        rust_batch, expected,
        "multi-frame batch encoding differs from JS"
    );
}

// ---------------------------------------------------------------------------
// Batch decoding: Rust decode_frame_batch must produce the same frames as JS.
// ---------------------------------------------------------------------------

#[test]
fn batch_decoding_multi_frame_matches_js() {
    let f = fixture();
    let batch = &f["batch"];

    let encoded =
        base64_url_to_bytes(batch["multi_frame_b64"].as_str().unwrap()).unwrap();
    let expected_frames: Vec<Vec<u8>> = batch["multi_raw_frames"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| base64_url_to_bytes(v.as_str().unwrap()).unwrap())
        .collect();

    let decoded = decode_frame_batch(&encoded).unwrap();

    assert_eq!(
        decoded.len(),
        expected_frames.len(),
        "batch frame count mismatch"
    );
    for (i, (got, expected)) in decoded.iter().zip(expected_frames.iter()).enumerate() {
        assert_eq!(got, expected, "batch decoded frame {i} differs from JS");
    }
}

// ---------------------------------------------------------------------------
// Handshake: Rust HostHandshake must produce the same ready JSON as JS.
// ---------------------------------------------------------------------------

#[test]
fn handshake_with_batch_produces_correct_ready() {
    let f = fixture();
    let keys = &f["keys"];

    let host_priv = import_ecdh_private_key(&keys["host"]["privateJwk"]).unwrap();
    let mut host = HostHandshake::new(host_priv);

    let hello = f["handshake"]["hello_batch"]
        .as_str()
        .unwrap();
    let action = host.handle_text(hello);

    let expected_ready = f["handshake"]["ready_batch"]
        .as_str()
        .unwrap();

    match action {
        HandshakeAction::Established {
            batch,
            reply_text,
            ..
        } => {
            assert!(batch, "batch should be true when both sides advertise it");
            assert_eq!(
                reply_text, expected_ready,
                "ready JSON with batch differs from JS"
            );
        }
        other => panic!("expected Established, got {other:?}"),
    }
}

#[test]
fn handshake_client_omits_batch_produces_legacy_ready() {
    let f = fixture();
    let keys = &f["keys"];

    let host_priv = import_ecdh_private_key(&keys["host"]["privateJwk"]).unwrap();
    let mut host = HostHandshake::new(host_priv);

    let hello = f["handshake"]["hello_no_batch"]
        .as_str()
        .unwrap();
    let action = host.handle_text(hello);

    let expected_ready = f["handshake"]["ready_no_batch"]
        .as_str()
        .unwrap();

    match action {
        HandshakeAction::Established {
            batch,
            reply_text,
            ..
        } => {
            assert!(!batch, "batch should be false when client omits batch");
            assert_eq!(
                reply_text, expected_ready,
                "ready JSON without batch (client omits) differs from JS"
            );
        }
        other => panic!("expected Established, got {other:?}"),
    }
}

#[test]
fn handshake_host_disables_batch_produces_legacy_ready() {
    let f = fixture();
    let keys = &f["keys"];

    let host_priv = import_ecdh_private_key(&keys["host"]["privateJwk"]).unwrap();
    let mut host = HostHandshake::with_options(host_priv, false);

    let hello = f["handshake"]["hello_batch"]
        .as_str()
        .unwrap();
    let action = host.handle_text(hello);

    let expected_ready = f["handshake"]["ready_no_batch"]
        .as_str()
        .unwrap();

    match action {
        HandshakeAction::Established {
            batch,
            reply_text,
            ..
        } => {
            assert!(!batch, "batch should be false when host disables batch");
            assert_eq!(
                reply_text, expected_ready,
                "ready JSON without batch (host disables) differs from JS"
            );
        }
        other => panic!("expected Established, got {other:?}"),
    }
}
