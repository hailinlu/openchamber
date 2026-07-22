//! STT 服务 — 调用 OpenAI-compatible `POST /v1/audio/transcriptions`。
//!
//! 对应 Node `tts/stt.js`:
//!   - `transcribeAudio({ audioBuffer, mimeType, model, baseURL, apiKey, language })`
//!     上传 multipart, 取回 `{ text }`。
//!   - `mimeTypeToExt(mimeType)` 把 MIME type 映射成文件扩展名。
//!
//! 注意: `reqwest` 的 `multipart` feature 在 workspace 内未启用, 这里手搓
//! multipart/form-data body (RFC 7578)。format 简单: boundary 分割 + headers +
//! parts + 终结 boundary。

use serde::Deserialize;
use serde_json::Value;

/// `transcribe_audio` 入参。
#[derive(Debug, Clone)]
pub struct TranscribeOptions {
    pub audio_buffer: Vec<u8>,
    pub mime_type: String,
    pub model: String,
    pub base_url: String,
    pub api_key: Option<String>,
    pub language: Option<String>,
}

/// OpenAI-compatible transcription endpoint 返回的 `{ text }` (或纯文本)。
#[derive(Debug, Deserialize)]
struct TranscriptionResponse {
    #[serde(default)]
    text: Option<String>,
}

/// 把音频 buffer 上传到 OpenAI-compatible `/v1/audio/transcriptions`。
///
/// 对应 Node `transcribeAudio` (`stt.js` line 23-56)。
pub async fn transcribe_audio(opts: TranscribeOptions) -> Result<String, String> {
    let normalized = super::base_url::normalize_custom_openai_base_url(&opts.base_url)?;
    let normalized = normalized.ok_or_else(|| "Custom server URL is required".to_string())?;

    let ext = mime_type_to_ext(&opts.mime_type);
    let filename = format!("audio.{}", ext);

    // 手搓 multipart/form-data (避免引入 reqwest multipart feature)
    let body = build_multipart_body(
        &opts.audio_buffer,
        &opts.mime_type,
        &filename,
        &[
            ("model", opts.model.as_str()),
            ("response_format", "json"),
        ],
        opts.language.as_deref(),
    );

    let api_key = opts
        .api_key
        .clone()
        .filter(|k| !k.is_empty())
        .or_else(|| {
            std::env::var("OPENAI_API_KEY")
                .ok()
                .filter(|k| !k.is_empty())
        })
        .unwrap_or_else(|| "not-required".to_string());

    let url = format!("{}/audio/transcriptions", normalized.trim_end_matches('/'));

    let client = reqwest::Client::new();
    let response = client
        .post(&url)
        .header(
            "Authorization",
            format!("Bearer {}", api_key),
        )
        .header(
            "Content-Type",
            format!("multipart/form-data; boundary={}", MULTIPART_BOUNDARY),
        )
        .body(body)
        .send()
        .await
        .map_err(|e| format!("Transcription request failed: {}", e))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(format!(
            "Transcription failed: HTTP {} — {}",
            status, body
        ));
    }

    let body_text = response
        .text()
        .await
        .map_err(|e| format!("Transcription read body failed: {}", e))?;

    // 优先尝试 { text } JSON 形态
    if let Ok(parsed) = serde_json::from_str::<TranscriptionResponse>(&body_text) {
        if let Some(t) = parsed.text {
            return Ok(t);
        }
        // text 字段不存在时, 不回退到 "", 而是往下走 Value fallback。
    }
    // 兼容: { transcript } 或任意 string 字段
    if let Ok(value) = serde_json::from_str::<Value>(&body_text) {
        if let Some(s) = value.get("text").and_then(|v| v.as_str()) {
            return Ok(s.to_string());
        }
        if let Some(s) = value.get("transcript").and_then(|v| v.as_str()) {
            return Ok(s.to_string());
        }
    }
    // 纯文本响应
    Ok(body_text)
}

/// Multipart 边界 — 必须全局稳定以保证 body 与 Content-Type 一致。
pub(crate) const MULTIPART_BOUNDARY: &str = "----GridForgeSTTBoundary7MA4YWxkTrZu0gW";

/// 构造 multipart/form-data body。
///
/// 文件 part 字段名 `file`, 文本 parts 按顺序追加 (`model`, `response_format`, 可选 `language`)。
fn build_multipart_body(
    audio_buffer: &[u8],
    mime_type: &str,
    filename: &str,
    text_parts: &[(&str, &str)],
    language: Option<&str>,
) -> Vec<u8> {
    let mut body = Vec::new();
    let b = MULTIPART_BOUNDARY;

    for (name, value) in text_parts {
        body.extend_from_slice(format!("--{}\r\n", b).as_bytes());
        body.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{}\"\r\n\r\n", name).as_bytes(),
        );
        body.extend_from_slice(value.as_bytes());
        body.extend_from_slice(b"\r\n");
    }

    if let Some(lang) = language {
        if !lang.is_empty() {
            body.extend_from_slice(format!("--{}\r\n", b).as_bytes());
            body.extend_from_slice(
                "Content-Disposition: form-data; name=\"language\"\r\n\r\n".as_bytes(),
            );
            body.extend_from_slice(lang.as_bytes());
            body.extend_from_slice(b"\r\n");
        }
    }

    // 文件 part
    body.extend_from_slice(format!("--{}\r\n", b).as_bytes());
    body.extend_from_slice(
        format!(
            "Content-Disposition: form-data; name=\"file\"; filename=\"{}\"\r\n",
            filename
        )
        .as_bytes(),
    );
    body.extend_from_slice(format!("Content-Type: {}\r\n\r\n", mime_type).as_bytes());
    body.extend_from_slice(audio_buffer);
    body.extend_from_slice(b"\r\n");

    // 终结 boundary
    body.extend_from_slice(format!("--{}--\r\n", b).as_bytes());
    body
}

/// MIME type → 文件扩展名 (供 multipart filename)。
///
/// 对应 Node `mimeTypeToExt` (`stt.js` line 63-76`)。
pub fn mime_type_to_ext(mime_type: &str) -> &'static str {
    let mime = mime_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_lowercase();

    match mime.as_str() {
        "audio/webm" => "webm",
        "audio/ogg" => "ogg",
        "audio/wav" => "wav",
        "audio/wave" => "wav",
        "audio/mpeg" => "mp3",
        "audio/mp3" => "mp3",
        "audio/mp4" => "mp4",
        "audio/flac" => "flac",
        _ => "webm",
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mime_type_to_ext_webm() {
        assert_eq!(mime_type_to_ext("audio/webm"), "webm");
        assert_eq!(mime_type_to_ext("audio/webm;codecs=opus"), "webm");
    }

    #[test]
    fn mime_type_to_ext_ogg() {
        assert_eq!(mime_type_to_ext("audio/ogg"), "ogg");
    }

    #[test]
    fn mime_type_to_ext_wav_variants() {
        assert_eq!(mime_type_to_ext("audio/wav"), "wav");
        assert_eq!(mime_type_to_ext("audio/wave"), "wav");
    }

    #[test]
    fn mime_type_to_ext_mpeg_mp3() {
        assert_eq!(mime_type_to_ext("audio/mpeg"), "mp3");
        assert_eq!(mime_type_to_ext("audio/mp3"), "mp3");
    }

    #[test]
    fn mime_type_to_ext_mp4() {
        assert_eq!(mime_type_to_ext("audio/mp4"), "mp4");
    }

    #[test]
    fn mime_type_to_ext_flac() {
        assert_eq!(mime_type_to_ext("audio/flac"), "flac");
    }

    #[test]
    fn mime_type_to_ext_case_insensitive() {
        assert_eq!(mime_type_to_ext("AUDIO/WEBM"), "webm");
        assert_eq!(mime_type_to_ext("Audio/Mp3"), "mp3");
    }

    #[test]
    fn mime_type_to_ext_default_webm() {
        assert_eq!(mime_type_to_ext("audio/unknown"), "webm");
        assert_eq!(mime_type_to_ext(""), "webm");
        assert_eq!(mime_type_to_ext("text/plain"), "webm");
    }

    #[test]
    fn build_multipart_body_contains_expected_parts() {
        let body = build_multipart_body(
            b"hello-audio-bytes",
            "audio/webm",
            "audio.webm",
            &[("model", "whisper"), ("response_format", "json")],
            Some("en"),
        );
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("name=\"model\""));
        assert!(text.contains("whisper"));
        assert!(text.contains("name=\"response_format\""));
        assert!(text.contains("json"));
        assert!(text.contains("name=\"language\""));
        assert!(text.contains("en"));
        assert!(text.contains("filename=\"audio.webm\""));
        assert!(text.contains("Content-Type: audio/webm"));
        assert!(text.contains("hello-audio-bytes"));
        // 终结 boundary
        assert!(text.contains(&format!("--{}--", MULTIPART_BOUNDARY)));
    }

    #[test]
    fn build_multipart_body_omits_language_when_none() {
        let body = build_multipart_body(
            b"x",
            "audio/webm",
            "audio.webm",
            &[("model", "m")],
            None,
        );
        let text = String::from_utf8_lossy(&body);
        assert!(!text.contains("name=\"language\""));
    }

    // --- 集成测试: 用 tokio::net::TcpListener mock transcription endpoint ---

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn start_mock_server(body: &'static str, status: u16) -> (u16, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { break };
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let _ = sock.read(&mut buf).await;
                    let response = format!(
                        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        status,
                        if status == 200 { "OK" } else { "ERROR" },
                        body.len(),
                        body,
                    );
                    let _ = sock.write_all(response.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });

        (port, handle)
    }

    #[tokio::test]
    async fn transcribe_audio_success_via_mock_server() {
        let (port, h) = start_mock_server(r#"{"text":"hello world"}"#, 200).await;
        let base = format!("http://127.0.0.1:{}", port);

        let result = transcribe_audio(TranscribeOptions {
            audio_buffer: vec![0xAA; 32],
            mime_type: "audio/webm".to_string(),
            model: "deepdml/faster-whisper-large-v3-turbo-ct2".to_string(),
            base_url: base,
            api_key: Some("sk-test".to_string()),
            language: Some("en".to_string()),
        })
        .await
        .unwrap();

        assert_eq!(result, "hello world");
        h.abort();
    }

    #[tokio::test]
    async fn transcribe_audio_falls_back_to_text_field() {
        let (port, h) = start_mock_server(r#"{"transcript":"foo"}"#, 200).await;
        let base = format!("http://127.0.0.1:{}", port);

        let result = transcribe_audio(TranscribeOptions {
            audio_buffer: vec![0xBB; 16],
            mime_type: "audio/webm".to_string(),
            model: "m".to_string(),
            base_url: base,
            api_key: Some("sk-test".to_string()),
            language: None,
        })
        .await
        .unwrap();

        assert_eq!(result, "foo");
        h.abort();
    }

    #[tokio::test]
    async fn transcribe_audio_rejects_missing_base_url() {
        let result = transcribe_audio(TranscribeOptions {
            audio_buffer: vec![0x00],
            mime_type: "audio/webm".to_string(),
            model: "m".to_string(),
            base_url: String::new(),
            api_key: Some("sk-test".to_string()),
            language: None,
        })
        .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Custom server URL is required"));
    }
}