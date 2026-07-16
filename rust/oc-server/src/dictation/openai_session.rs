//! 伪流式 OpenAI-compatible Whisper 转录会话。
//!
//! 对应 Node `dictation/openai-compatible-session.js` (98 LOC)。
//!
//! Whisper HTTP API 无法流式, 所以音频按段 buffer, 在 `commit()` 时上传到
//! `/v1/audio/transcriptions` 转录。实时 partial 只在段边界推进 (manager
//! 每 ~15s 语音 auto-commit 一次)。复用已移植的 `tts::stt::transcribe_audio`。
//!
//! 实现 `stream_manager::SttSession` trait。异步转录在 `commit()` 内 spawn,
//! 完成后把事件 push 到内部 channel, `try_next_event` 取出。

use std::sync::Arc;
use std::sync::Mutex;

use tokio::sync::mpsc;
use uuid::Uuid;

use super::audio::pcm16_to_wav;
use super::stream_manager::{SessionEvent, SttSession};
use crate::tts::stt::{transcribe_audio, TranscribeOptions};

/// OpenAI-compatible 提供方要求的采样率 (16 kHz)。
const OPENAI_COMPATIBLE_SAMPLE_RATE: u32 = 16000;

/// 配置 (对应 Node constructor config)。
#[derive(Debug, Clone, Default)]
pub struct OpenAiCompatibleSessionConfig {
    pub base_url: String,
    pub model: String,
    pub api_key: Option<String>,
    pub language: Option<String>,
}

/// 伪流式 Whisper 转录会话。
pub struct OpenAiCompatibleTranscriptionSession {
    config: OpenAiCompatibleSessionConfig,
    connected: bool,
    segment_id: String,
    #[allow(dead_code)]
    previous_segment_id: Option<String>,
    pcm16: Vec<u8>,
    events_tx: mpsc::UnboundedSender<SessionEvent>,
    events_rx: Mutex<mpsc::UnboundedReceiver<SessionEvent>>,
    // 持有 tokio runtime handle 用于 spawn (manager 在同步上下文调用 commit)
    runtime_handle: tokio::runtime::Handle,
}

impl OpenAiCompatibleTranscriptionSession {
    /// 创建会话 (未连接)。需要 tokio handle 以便 `commit()` 内 spawn 转录。
    pub fn new(config: OpenAiCompatibleSessionConfig) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            config,
            connected: false,
            segment_id: Uuid::new_v4().to_string(),
            previous_segment_id: None,
            pcm16: Vec::new(),
            events_tx: tx,
            events_rx: Mutex::new(rx),
            runtime_handle: tokio::runtime::Handle::current(),
        }
    }

    /// 连接校验 (对应 Node `connect()`)。无 base_url/model → 报错。
    pub fn connect(&mut self) -> Result<(), String> {
        if self.config.base_url.is_empty() {
            return Err("Custom STT server URL is not configured".to_string());
        }
        if self.config.model.is_empty() {
            return Err("STT model is not configured".to_string());
        }
        self.connected = true;
        Ok(())
    }
}

impl SttSession for OpenAiCompatibleTranscriptionSession {
    fn required_sample_rate(&self) -> u32 {
        OPENAI_COMPATIBLE_SAMPLE_RATE
    }

    fn append_pcm16(&mut self, pcm16: &[u8]) {
        if !self.connected {
            let _ = self.events_tx.send(SessionEvent::Error(
                "STT session not connected".to_string(),
            ));
            return;
        }
        self.pcm16.extend_from_slice(pcm16);
    }

    fn commit(&mut self) {
        if !self.connected {
            let _ = self.events_tx.send(SessionEvent::Error(
                "STT session not connected".to_string(),
            ));
            return;
        }

        let committed_id = std::mem::replace(&mut self.segment_id, Uuid::new_v4().to_string());
        self.previous_segment_id = Some(committed_id.clone());
        let committed_pcm = std::mem::take(&mut self.pcm16);

        // emit committed 立即
        let _ = self.events_tx.send(SessionEvent::Committed {
            segment_id: committed_id.clone(),
        });

        // spawn 异步转录
        let tx = self.events_tx.clone();
        let config = self.config.clone();
        self.runtime_handle.spawn(async move {
            match transcribe_segment(&committed_pcm, &committed_id, &config).await {
                Ok(text) => {
                    let _ = tx.send(SessionEvent::Transcript {
                        segment_id: committed_id,
                        transcript: text.trim().to_string(),
                        is_final: true,
                    });
                }
                Err(e) => {
                    let _ = tx.send(SessionEvent::Error(e));
                }
            }
        });
    }

    fn clear(&mut self) {
        self.pcm16.clear();
        self.segment_id = Uuid::new_v4().to_string();
    }

    fn close(&mut self) {
        self.connected = false;
        self.pcm16.clear();
    }

    fn try_next_event(&mut self) -> Option<SessionEvent> {
        self.events_rx.lock().unwrap().try_recv().ok()
    }
}

/// 转录一个段 (对应 Node commit 内的 async IIFE)。
async fn transcribe_segment(
    pcm16: &[u8],
    _segment_id: &str,
    config: &OpenAiCompatibleSessionConfig,
) -> Result<String, String> {
    if pcm16.is_empty() {
        return Ok(String::new());
    }
    let wav = pcm16_to_wav(pcm16, OPENAI_COMPATIBLE_SAMPLE_RATE);
    let opts = TranscribeOptions {
        audio_buffer: wav,
        mime_type: "audio/wav".to_string(),
        model: config.model.clone(),
        base_url: config.base_url.clone(),
        api_key: config.api_key.clone(),
        language: config.language.clone(),
    };
    // transcribe_audio 内部用 reqwest, 返回 Result<String, String>
    let result = transcribe_audio(opts).await;
    // 失败时返回错误字符串 (会被包装成 SessionEvent::Error)
    match result {
        Ok(text) => Ok(text),
        Err(e) => Err(e),
    }
}

// 保留 Arc 用于未来 (若 manager 需共享 session); 当前未用但 trait 对象可能需要。
#[allow(dead_code)]
type _SharedSession = Arc<Mutex<OpenAiCompatibleTranscriptionSession>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn connect_rejects_missing_base_url() {
        let mut session = OpenAiCompatibleTranscriptionSession::new(OpenAiCompatibleSessionConfig {
            base_url: String::new(),
            model: "whisper-1".to_string(),
            ..Default::default()
        });
        assert!(session.connect().is_err());
    }

    #[tokio::test]
    async fn connect_rejects_missing_model() {
        let mut session = OpenAiCompatibleTranscriptionSession::new(OpenAiCompatibleSessionConfig {
            base_url: "http://localhost:8080".to_string(),
            model: String::new(),
            ..Default::default()
        });
        assert!(session.connect().is_err());
    }

    #[tokio::test]
    async fn connect_succeeds_with_config() {
        let mut session = OpenAiCompatibleTranscriptionSession::new(OpenAiCompatibleSessionConfig {
            base_url: "http://localhost:8080".to_string(),
            model: "whisper-1".to_string(),
            ..Default::default()
        });
        assert!(session.connect().is_ok());
        assert!(session.required_sample_rate() == OPENAI_COMPATIBLE_SAMPLE_RATE);
    }
}
