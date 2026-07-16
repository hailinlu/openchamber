//! Tunnel mux frame codec (Layer 3 of the private relay protocol).
//!
//! Pure functions, no I/O. Byte-compatible mirror of
//! `packages/ui/src/lib/relay/tunnel-codec.ts` + Layer 3 constants
//! from `protocol.ts` — and also of `packages/web/server/lib/relay/tunnel-codec.js`.
//!
//! Layout:
//! - **Single tunnel frame**: `[1B frameType | hasMoreFragments][4B BE streamId][payload]`
//! - **Batch envelope** (when both peers negotiated `batch`):
//!   - Single: `[0x00][frame bytes]`
//!   - N frames: `[0x01]([4B BE length][frame bytes])*N`
//!
//! Client-initiated streams use odd streamIds starting at 1; even ids reserved.

use thiserror::Error;

/// Layer 2 plaintext cap (mirror of e2ee.js MAX_PLAINTEXT_FRAME_BYTES).
pub const MAX_PLAINTEXT_FRAME_BYTES: usize = 64 * 1024;

/// Tunnel frame header byte count.
pub const TUNNEL_FRAME_HEADER_BYTES: usize = 5;

/// Top-bit flag on frame type byte marking "more fragments follow".
pub const TUNNEL_FRAGMENT_FLAG: u8 = 0x80;

/// Batch envelope: tag byte indicating a single-frame envelope.
pub const BATCH_CONTAINER_TAG_SINGLE: u8 = 0x00;

/// Batch envelope: tag byte indicating a multi-frame envelope.
pub const BATCH_CONTAINER_TAG_BATCH: u8 = 0x01;

/// 4-byte big-endian length prefix inside multi-frame envelopes.
pub const BATCH_FRAME_LENGTH_BYTES: usize = 4;

/// Per-frame reservation when budgeting against the plaintext cap:
/// 1 byte tag (single) + 4 byte length (multi) is the worst-case overhead.
pub const BATCH_ENVELOPE_RESERVED_BYTES: usize = 1 + BATCH_FRAME_LENGTH_BYTES;

/// Maximum payload bytes per tunnel frame.
pub const MAX_TUNNEL_PAYLOAD_BYTES: usize =
    MAX_PLAINTEXT_FRAME_BYTES - TUNNEL_FRAME_HEADER_BYTES - BATCH_ENVELOPE_RESERVED_BYTES;

/// Batcher defaults (mirror of tunnel-codec.ts).
pub const DEFAULT_BATCH_WINDOW_MS: u64 = 150;
pub const DEFAULT_BATCH_MAX_BYTES: usize = 24 * 1024;
pub const DEFAULT_BATCH_MAX_FRAMES: usize = 32;

/// Tunnel frame types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TunnelFrameType {
    HttpRequest = 1,
    HttpBody = 2,
    HttpResponse = 3,
    StreamEnd = 4,
    StreamAbort = 5,
    WsOpen = 6,
    WsOpened = 7,
    WsText = 8,
    WsBinary = 9,
    WsClose = 10,
    Ping = 11,
    Pong = 12,
}

impl TunnelFrameType {
    pub fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            1 => Some(Self::HttpRequest),
            2 => Some(Self::HttpBody),
            3 => Some(Self::HttpResponse),
            4 => Some(Self::StreamEnd),
            5 => Some(Self::StreamAbort),
            6 => Some(Self::WsOpen),
            7 => Some(Self::WsOpened),
            8 => Some(Self::WsText),
            9 => Some(Self::WsBinary),
            10 => Some(Self::WsClose),
            11 => Some(Self::Ping),
            12 => Some(Self::Pong),
            _ => None,
        }
    }
}

/// Decoded tunnel frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelFrame {
    pub frame_type: TunnelFrameType,
    pub stream_id: u32,
    pub payload: Vec<u8>,
    pub has_more_fragments: bool,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum TunnelCodecError {
    #[error("invalid stream id")]
    InvalidStreamId,
    #[error("tunnel payload exceeds maximum size")]
    PayloadTooLarge,
    #[error("tunnel frame too short")]
    FrameTooShort,
    #[error("unknown tunnel frame type {0}")]
    UnknownFrameType(u8),
    #[error("empty batch plaintext")]
    EmptyBatchPlaintext,
    #[error("unknown batch container tag {0}")]
    UnknownBatchTag(u8),
    #[error("truncated batch frame length")]
    TruncatedBatchLength,
    #[error("truncated batch frame body")]
    TruncatedBatchBody,
    #[error("empty frame batch")]
    EmptyFrameBatch,
    #[error("frame batch exceeds maximum plaintext size")]
    BatchTooLarge,
    #[error("cannot encode an empty frame batch")]
    EmptyBatchEncode,
    #[error("invalid chunk size")]
    InvalidChunkSize,
    #[error("fragmented message exceeds maximum size")]
    FragmentTooLarge,
}

const MAX_STREAM_ID: u32 = 0xffff_ffff;

/// Encode a single tunnel frame.
///
/// Byte layout: `[1B type|fragFlag][4B BE streamId][payload]`.
pub fn encode_tunnel_frame(
    frame_type: TunnelFrameType,
    stream_id: u32,
    payload: &[u8],
) -> Vec<u8> {
    encode_tunnel_frame_with_fragments(frame_type, stream_id, payload, false)
}

/// Encode a single tunnel frame with explicit `hasMoreFragments` flag.
pub fn encode_tunnel_frame_with_fragments(
    frame_type: TunnelFrameType,
    stream_id: u32,
    payload: &[u8],
    has_more_fragments: bool,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(TUNNEL_FRAME_HEADER_BYTES + payload.len());
    let type_byte = if has_more_fragments {
        (frame_type as u8) | TUNNEL_FRAGMENT_FLAG
    } else {
        frame_type as u8
    };
    out.push(type_byte);
    out.push(((stream_id >> 24) & 0xff) as u8);
    out.push(((stream_id >> 16) & 0xff) as u8);
    out.push(((stream_id >> 8) & 0xff) as u8);
    out.push((stream_id & 0xff) as u8);
    out.extend_from_slice(payload);
    out
}

/// Decode a tunnel frame from its raw bytes.
pub fn decode_tunnel_frame(frame: &[u8]) -> Result<TunnelFrame, TunnelCodecError> {
    if frame.len() < TUNNEL_FRAME_HEADER_BYTES {
        return Err(TunnelCodecError::FrameTooShort);
    }
    let raw_type = frame[0];
    let has_more_fragments = (raw_type & TUNNEL_FRAGMENT_FLAG) != 0;
    let frame_type_byte = raw_type & !TUNNEL_FRAGMENT_FLAG;
    let frame_type = TunnelFrameType::from_byte(frame_type_byte)
        .ok_or(TunnelCodecError::UnknownFrameType(frame_type_byte))?;
    let stream_id = u32::from_be_bytes([frame[1], frame[2], frame[3], frame[4]]);
    Ok(TunnelFrame {
        frame_type,
        stream_id,
        payload: frame[TUNNEL_FRAME_HEADER_BYTES..].to_vec(),
        has_more_fragments,
    })
}

// JSON payload helpers (mirror of encodeJsonPayload / decodeJsonPayload).

/// Encode a JSON value into tunnel payload bytes (UTF-8).
pub fn encode_json_payload(value: &serde_json::Value) -> Vec<u8> {
    serde_json::to_vec(value).expect("JSON serialization cannot fail for Value")
}

/// Decode a tunnel payload's bytes into a JSON value. Caller-supplied
/// `validate` decides whether the parsed shape is acceptable.
pub fn decode_json_payload<F: FnOnce(&serde_json::Value) -> bool>(
    payload: &[u8],
    validate: F,
) -> Result<serde_json::Value, TunnelCodecError> {
    let parsed: serde_json::Value = serde_json::from_slice(payload)
        .map_err(|_| TunnelCodecError::EmptyBatchPlaintext)?; // mirror of JS "malformed JSON" — reused error variant
    if !validate(&parsed) {
        return Err(TunnelCodecError::EmptyBatchPlaintext); // mirror of "unexpected shape"
    }
    Ok(parsed)
}

// Chunking + fragmentation.

/// Split a payload into chunks of at most `chunk_size` bytes.
/// Empty input yields one empty chunk (mirror of JS chunkPayload).
pub fn chunk_payload(bytes: &[u8]) -> Vec<Vec<u8>> {
    chunk_payload_with_size(bytes, MAX_TUNNEL_PAYLOAD_BYTES)
}

pub fn chunk_payload_with_size(bytes: &[u8], chunk_size: usize) -> Vec<Vec<u8>> {
    if chunk_size == 0 || chunk_size > MAX_TUNNEL_PAYLOAD_BYTES {
        return Vec::new();
    }
    if bytes.is_empty() {
        return vec![Vec::new()];
    }
    let mut out = Vec::with_capacity(bytes.len().div_ceil(chunk_size));
    let mut offset = 0;
    while offset < bytes.len() {
        let end = (offset + chunk_size).min(bytes.len());
        out.push(bytes[offset..end].to_vec());
        offset = end;
    }
    out
}

/// Encode one logical message as one or more fragmented frames (mirror of
/// `encodeFragmentedMessage`). All but the last frame carry `hasMoreFragments`.
pub fn encode_fragmented_message(
    frame_type: TunnelFrameType,
    stream_id: u32,
    payload: &[u8],
) -> Vec<Vec<u8>> {
    let chunks = chunk_payload(payload);
    let total = chunks.len();
    chunks
        .into_iter()
        .enumerate()
        .map(|(i, c)| {
            let last = i + 1 == total;
            encode_tunnel_frame_with_fragments(frame_type, stream_id, &c, !last)
        })
        .collect()
}

/// Reassembler state keyed by `${streamId}:${frameType}`.
pub struct FragmentAssembler {
    pending: std::collections::HashMap<String, FragmentEntry>,
    max_message_bytes: usize,
}

struct FragmentEntry {
    chunks: Vec<Vec<u8>>,
    total_bytes: usize,
}

impl FragmentAssembler {
    pub fn new() -> Self {
        Self::with_max(16 * 1024 * 1024)
    }

    pub fn with_max(max_message_bytes: usize) -> Self {
        Self {
            pending: std::collections::HashMap::new(),
            max_message_bytes,
        }
    }

    /// Push a frame; returns the complete message payload once all fragments
    /// arrived, or `None` while more fragments are expected.
    pub fn push(&mut self, frame: &TunnelFrame) -> Result<Option<Vec<u8>>, TunnelCodecError> {
        let key = format!("{}:{}", frame.stream_id, frame.frame_type as u8);
        if !frame.has_more_fragments && !self.pending.contains_key(&key) {
            return Ok(Some(frame.payload.clone()));
        }
        let entry = self.pending.entry(key).or_insert_with(|| FragmentEntry {
            chunks: Vec::new(),
            total_bytes: 0,
        });
        entry.total_bytes += frame.payload.len();
        if entry.total_bytes > self.max_message_bytes {
            let key_to_remove = self
                .pending
                .keys()
                .find(|k| k.starts_with(&format!("{}:", frame.stream_id)))
                .cloned();
            if let Some(k) = key_to_remove {
                self.pending.remove(&k);
            }
            return Err(TunnelCodecError::FragmentTooLarge);
        }
        entry.chunks.push(frame.payload.clone());
        if frame.has_more_fragments {
            return Ok(None);
        }
        let entry = self.pending.remove(&format!("{}:{}", frame.stream_id, frame.frame_type as u8)).unwrap();
        let mut out = Vec::with_capacity(entry.total_bytes);
        for chunk in entry.chunks {
            out.extend_from_slice(&chunk);
        }
        Ok(Some(out))
    }

    /// Drop all fragments for a given streamId (called on StreamAbort/StreamEnd).
    pub fn drop_stream(&mut self, stream_id: u32) {
        let prefix = format!("{}:", stream_id);
        self.pending.retain(|k, _| !k.starts_with(&prefix));
    }
}

impl Default for FragmentAssembler {
    fn default() -> Self {
        Self::new()
    }
}

// Batch envelope encoding.

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum DecodeFrameBatchError {
    #[error("empty batch plaintext")]
    Empty,
    #[error("unknown batch container tag {0}")]
    UnknownTag(u8),
    #[error("truncated batch frame length")]
    TruncatedLength,
    #[error("truncated batch frame body")]
    TruncatedBody,
    #[error("empty frame batch")]
    EmptyBatch,
}

/// Encode one or more frames into a batch envelope.
pub fn encode_frame_batch(frames: &[Vec<u8>]) -> Result<Vec<u8>, TunnelCodecError> {
    if frames.is_empty() {
        return Err(TunnelCodecError::EmptyBatchEncode);
    }
    if frames.len() == 1 {
        let frame = &frames[0];
        let mut out = Vec::with_capacity(1 + frame.len());
        out.push(BATCH_CONTAINER_TAG_SINGLE);
        out.extend_from_slice(frame);
        if out.len() > MAX_PLAINTEXT_FRAME_BYTES {
            return Err(TunnelCodecError::BatchTooLarge);
        }
        return Ok(out);
    }
    let mut total: usize = 1;
    for frame in frames {
        total += BATCH_FRAME_LENGTH_BYTES + frame.len();
    }
    if total > MAX_PLAINTEXT_FRAME_BYTES {
        return Err(TunnelCodecError::BatchTooLarge);
    }
    let mut out = Vec::with_capacity(total);
    out.push(BATCH_CONTAINER_TAG_BATCH);
    for frame in frames {
        let len = frame.len() as u32;
        out.push(((len >> 24) & 0xff) as u8);
        out.push(((len >> 16) & 0xff) as u8);
        out.push(((len >> 8) & 0xff) as u8);
        out.push((len & 0xff) as u8);
        out.extend_from_slice(frame);
    }
    Ok(out)
}

/// Decode a batch envelope into its ordered frames.
pub fn decode_frame_batch(plaintext: &[u8]) -> Result<Vec<Vec<u8>>, DecodeFrameBatchError> {
    if plaintext.is_empty() {
        return Err(DecodeFrameBatchError::Empty);
    }
    let tag = plaintext[0];
    if tag == BATCH_CONTAINER_TAG_SINGLE {
        return Ok(vec![plaintext[1..].to_vec()]);
    }
    if tag != BATCH_CONTAINER_TAG_BATCH {
        return Err(DecodeFrameBatchError::UnknownTag(tag));
    }
    let mut frames = Vec::new();
    let mut offset = 1;
    while offset < plaintext.len() {
        if offset + BATCH_FRAME_LENGTH_BYTES > plaintext.len() {
            return Err(DecodeFrameBatchError::TruncatedLength);
        }
        let len = u32::from_be_bytes([
            plaintext[offset],
            plaintext[offset + 1],
            plaintext[offset + 2],
            plaintext[offset + 3],
        ]) as usize;
        offset += BATCH_FRAME_LENGTH_BYTES;
        if offset + len > plaintext.len() {
            return Err(DecodeFrameBatchError::TruncatedBody);
        }
        frames.push(plaintext[offset..offset + len].to_vec());
        offset += len;
    }
    if frames.is_empty() {
        return Err(DecodeFrameBatchError::EmptyBatch);
    }
    Ok(frames)
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_roundtrip_all_types() {
        for &ft in &[
            TunnelFrameType::HttpRequest,
            TunnelFrameType::HttpBody,
            TunnelFrameType::HttpResponse,
            TunnelFrameType::StreamEnd,
            TunnelFrameType::StreamAbort,
            TunnelFrameType::WsOpen,
            TunnelFrameType::WsOpened,
            TunnelFrameType::WsText,
            TunnelFrameType::WsBinary,
            TunnelFrameType::WsClose,
            TunnelFrameType::Ping,
            TunnelFrameType::Pong,
        ] {
            let payload = b"hello world";
            let frame = encode_tunnel_frame(ft, 7, payload);
            let decoded = decode_tunnel_frame(&frame).unwrap();
            assert_eq!(decoded.frame_type, ft);
            assert_eq!(decoded.stream_id, 7);
            assert_eq!(decoded.payload, payload);
            assert!(!decoded.has_more_fragments);
        }
    }

    #[test]
    fn frame_large_stream_id() {
        let stream_id: u32 = 0xfffffffd;
        let payload = b"big-id";
        let frame = encode_tunnel_frame(TunnelFrameType::HttpRequest, stream_id, payload);
        let decoded = decode_tunnel_frame(&frame).unwrap();
        assert_eq!(decoded.stream_id, stream_id);
        assert_eq!(decoded.frame_type, TunnelFrameType::HttpRequest);
        assert_eq!(decoded.payload, payload);
    }

    #[test]
    fn frame_short_payload() {
        let frame = encode_tunnel_frame(TunnelFrameType::Ping, 1, b"");
        let decoded = decode_tunnel_frame(&frame).unwrap();
        assert_eq!(decoded.payload.len(), 0);
    }

    #[test]
    fn frame_truncated_rejected() {
        let err = decode_tunnel_frame(&[0x01, 0x00]).unwrap_err();
        assert_eq!(err, TunnelCodecError::FrameTooShort);
    }

    #[test]
    fn frame_unknown_type_rejected() {
        // 0x63 = 99: not a known TunnelFrameType (1-12).
        let mut frame = vec![0x63, 0, 0, 0, 0];
        frame.extend_from_slice(b"x");
        let err = decode_tunnel_frame(&frame).unwrap_err();
        match err {
            TunnelCodecError::UnknownFrameType(b) => assert_eq!(b, 0x63),
            _ => panic!("expected UnknownFrameType"),
        }
    }

    #[test]
    fn fragment_flag_roundtrip() {
        let frame =
            encode_tunnel_frame_with_fragments(TunnelFrameType::HttpBody, 3, b"chunk", true);
        let decoded = decode_tunnel_frame(&frame).unwrap();
        assert!(decoded.has_more_fragments);
        assert_eq!(decoded.frame_type, TunnelFrameType::HttpBody);
        assert_eq!(decoded.payload, b"chunk");
    }

    #[test]
    fn fragment_assembler_reassembles_multi_chunk() {
        let total_bytes = (MAX_TUNNEL_PAYLOAD_BYTES * 2) + 10;
        let payload: Vec<u8> = (0..total_bytes).map(|i| (i % 256) as u8).collect();

        let frames = encode_fragmented_message(TunnelFrameType::HttpBody, 5, &payload);
        assert!(frames.len() >= 3);

        let mut asm = FragmentAssembler::new();
        let mut assembled: Option<Vec<u8>> = None;
        for (i, f) in frames.iter().enumerate() {
            let decoded = decode_tunnel_frame(f).unwrap();
            let res = asm.push(&decoded).unwrap();
            if i + 1 == frames.len() {
                assembled = res;
            } else {
                assert!(res.is_none(), "intermediate fragments must yield None");
            }
        }
        assert_eq!(assembled.unwrap(), payload);
    }

    #[test]
    fn fragment_assembler_respects_max_bytes() {
        let mut asm = FragmentAssembler::with_max(10);
        // Two-fragment message totaling 20 bytes (exceeds max of 10).
        let first = decode_tunnel_frame(
            &encode_tunnel_frame_with_fragments(
                TunnelFrameType::HttpBody,
                1,
                &vec![0u8; 10],
                true,
            ),
        )
        .unwrap();
        asm.push(&first).unwrap();
        let second = decode_tunnel_frame(
            &encode_tunnel_frame_with_fragments(
                TunnelFrameType::HttpBody,
                1,
                &vec![0u8; 10],
                false,
            ),
        )
        .unwrap();
        let err = asm.push(&second).unwrap_err();
        assert_eq!(err, TunnelCodecError::FragmentTooLarge);
    }

    #[test]
    fn fragment_assembler_drop_stream_clears() {
        let mut asm = FragmentAssembler::new();
        let f1 = decode_tunnel_frame(
            &encode_tunnel_frame_with_fragments(TunnelFrameType::HttpBody, 9, b"chunk", true),
        )
        .unwrap();
        let _ = asm.push(&f1).unwrap();
        assert!(asm.pending.contains_key("9:2"));
        asm.drop_stream(9);
        assert!(asm.pending.is_empty());
    }

    #[test]
    fn batch_single_frame_envelope() {
        let frame = encode_tunnel_frame(TunnelFrameType::HttpBody, 1, b"only");
        let envelope = encode_frame_batch(&[frame.clone()]).unwrap();
        assert_eq!(envelope[0], BATCH_CONTAINER_TAG_SINGLE);
        assert_eq!(&envelope[1..], frame.as_slice());
        let decoded = decode_frame_batch(&envelope).unwrap();
        assert_eq!(decoded, vec![frame]);
    }

    #[test]
    fn batch_multi_frame_envelope_roundtrip() {
        let f1 = encode_tunnel_frame(TunnelFrameType::HttpBody, 1, b"alpha");
        let f2 = encode_tunnel_frame(TunnelFrameType::HttpBody, 1, b"beta");
        let f3 = encode_tunnel_frame(TunnelFrameType::HttpBody, 1, b"gamma");
        let envelope = encode_frame_batch(&[f1.clone(), f2.clone(), f3.clone()]).unwrap();
        assert_eq!(envelope[0], BATCH_CONTAINER_TAG_BATCH);
        let decoded = decode_frame_batch(&envelope).unwrap();
        assert_eq!(decoded, vec![f1, f2, f3]);
    }

    #[test]
    fn batch_empty_rejected() {
        assert_eq!(
            encode_frame_batch(&[]).unwrap_err(),
            TunnelCodecError::EmptyBatchEncode
        );
    }

    #[test]
    fn batch_too_large_rejected() {
        let oversized = vec![0u8; MAX_PLAINTEXT_FRAME_BYTES];
        let err = encode_frame_batch(&[oversized]).unwrap_err();
        assert_eq!(err, TunnelCodecError::BatchTooLarge);
    }

    #[test]
    fn batch_decode_empty_rejected() {
        assert_eq!(
            decode_frame_batch(&[]).unwrap_err(),
            DecodeFrameBatchError::Empty
        );
    }

    #[test]
    fn batch_decode_unknown_tag_rejected() {
        let err = decode_frame_batch(&[0x99, 0, 0, 0, 0]).unwrap_err();
        match err {
            DecodeFrameBatchError::UnknownTag(t) => assert_eq!(t, 0x99),
            _ => panic!("expected UnknownTag"),
        }
    }

    #[test]
    fn batch_decode_truncated_length_rejected() {
        let mut data = vec![BATCH_CONTAINER_TAG_BATCH, 0x00, 0x00];
        let err = decode_frame_batch(&data).unwrap_err();
        assert_eq!(err, DecodeFrameBatchError::TruncatedLength);
    }

    #[test]
    fn batch_decode_truncated_body_rejected() {
        let mut data = vec![BATCH_CONTAINER_TAG_BATCH];
        data.extend_from_slice(&100u32.to_be_bytes());
        // no body bytes after length
        let err = decode_frame_batch(&data).unwrap_err();
        assert_eq!(err, DecodeFrameBatchError::TruncatedBody);
    }

    #[test]
    fn json_payload_roundtrip() {
        let v = serde_json::json!({"method":"GET","path":"/health"});
        let bytes = encode_json_payload(&v);
        let decoded = decode_json_payload(&bytes, |parsed| {
            parsed.get("method").and_then(|m| m.as_str()) == Some("GET")
        })
        .unwrap();
        assert_eq!(decoded, v);
    }

    #[test]
    fn json_payload_malformed_rejected() {
        let err = decode_json_payload(b"not json", |_| true).unwrap_err();
        assert_eq!(err, TunnelCodecError::EmptyBatchPlaintext);
    }

    #[test]
    fn chunk_payload_empty_yields_single_empty_chunk() {
        let chunks = chunk_payload(b"");
        assert_eq!(chunks, vec![Vec::<u8>::new()]);
    }

    #[test]
    fn chunk_payload_smaller_than_max_single_chunk() {
        let chunks = chunk_payload(b"hello");
        assert_eq!(chunks, vec![b"hello".to_vec()]);
    }
}