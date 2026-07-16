//! Dictation 模块 — 服务端权威流式语音转文字 (STT) + 本地 TTS。
//!
//! 对应 Node `packages/web/server/lib/dictation/` (14 文件, 2904 LOC)。
//!
//! 本轮范围: **仅 openai-compatible 提供方**。本地 sherpa-onnx 推理栈
//! (`local/*` 6 文件, ~1235 LOC) 本轮不移植 — `local` 提供方返回明确
//! "not available" 桩, 等后续阶段决定 native 方案 (Node worker 子进程 vs
//! sherpa-rs)。详见 `service.rs::local_unavailable`。
//!
//! 子模块:
//!   - `audio`           — PCM16 DSP (format 解析 / peak / WAV / 流式重采样)
//!   - `stream_manager`  — 每连接状态机 (seq 重排 + ack + auto-commit + 静音抑制)
//!   - `openai_session`  — 伪流式 Whisper 会话 (复用 `tts::stt::transcribe_audio`)
//!   - `service`         — 提供方解析 + 就绪快照
//!   - `routes`          — WS handler + 4 HTTP handler

pub mod audio;
pub mod openai_session;
pub mod routes;
pub mod service;
pub mod stream_manager;

// =========================================================================
// 常量 (对应 Node runtime.js:28-31 + stream-manager.js:19-25)
// =========================================================================

/// `/api/dictation/ws` 字面路径 (已在 `ui_auth/types.rs:149` WS 白名单)。
/// 契约常量 — 与 `terminal::TERMINAL_WS_PATH` 对齐, 目前路由内联注册;
/// status/文档引用此常量时启用。
#[allow(dead_code)]
pub const DICTATION_WS_PATH: &str = "/api/dictation/ws";

/// WS 最大消息体 512 KB (对应 Node `DICTATION_WS_MAX_PAYLOAD_BYTES`)。
pub const DICTATION_WS_MAX_PAYLOAD_BYTES: usize = 512 * 1024;

/// WS 心跳间隔 30s (对应 Node `DICTATION_WS_HEARTBEAT_INTERVAL_MS`)。
pub const DICTATION_WS_HEARTBEAT_INTERVAL_MS: u64 = 30_000;

/// 静音峰值阈值 (对应 Node stream-manager.js:25 `SILENCE_PEAK_THRESHOLD`)。
/// 峰值低于此值的段被清除而非提交, 避免静音导致 Whisper 幻觉。
pub const SILENCE_PEAK_THRESHOLD: i16 = 300;

/// finalize 基础超时 10s。
pub const DEFAULT_FINAL_TIMEOUT_MS: u64 = 10_000;

/// auto-commit 默认 15s 音频 (f64 以支持测试用 0.05s 细粒度, 对齐 Node)。
pub const DEFAULT_AUTO_COMMIT_SECONDS: f64 = 15.0;

/// finalize 超时上限 5 分钟。
pub const FINAL_TIMEOUT_MAX_MS: u64 = 5 * 60 * 1000;

/// 每个待定段增加 15s finalize 预算。
pub const FINAL_TIMEOUT_PER_PENDING_SEGMENT_MS: u64 = 15 * 1000;

/// 每个待定音频秒增加 1.5s finalize 预算。
pub const FINAL_TIMEOUT_PER_PENDING_AUDIO_SECOND_MS: u64 = 1500;

/// 每个缺失 seq 增加 250ms finalize 预算。
pub const FINAL_TIMEOUT_PER_MISSING_SEQ_MS: u64 = 250;

/// 本地模型不支持时的 reasonCode (明确暴露, 非隐藏降级)。
pub const LOCAL_MODELS_UNSUPPORTED_REASON: &str = "local_models_unsupported";

/// 本地模型不支持时的错误文案。
pub const LOCAL_MODELS_UNSUPPORTED_ERROR: &str =
    "Local speech models are not available in this build. Use the openai-compatible provider.";
