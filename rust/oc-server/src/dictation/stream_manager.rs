//! `DictationStreamManager` — 每连接的流式听写状态机。
//!
//! 对应 Node `dictation/stream-manager.js` (461 LOC)。
//!
//! 职责:
//! - 按 `seq` 重排入站 chunk, 并 ack 最高连续 seq
//! - 把客户端 PCM (默认 16 kHz) 重采样到提供方要求的采样率
//! - 每 `auto_commit_seconds` 音频 auto-commit 一个段, 但静音段 (峰值 <
//!   `SILENCE_PEAK_THRESHOLD`) 被清除而非提交
//! - 把每段 transcript 拼成实时 partial, 并在所有已提交段都有 final
//!   transcript 后发出最终文本
//! - 根据待定工作应用自适应 finalize 超时预算
//!
//! 异步模型 (关键差异):
//! - Node: STT 会话是 `EventEmitter`, `commit()` 等方法同步返回, 异步回调
//!   (如 transcribe 完成) 通过事件循环稍后 emit `committed`/`transcript`。
//! - Rust: STT 会话实现 `SttSession` trait, 内部 spawn 异步工作后把事件
//!   push 到 channel; `try_next_event()` 非阻塞取出。管理器方法在每次
//!   触发后调用 `pump_events()` 同步排空待处理事件 — 对齐 Node 的事件
//!   循环延迟语义。
//!
//! 测试 oracle: 移植自 `stream-manager.test.js` 6 个 `it`。

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use tokio::sync::mpsc;

use super::audio::{pcm16le_peak_abs, parse_pcm_rate_from_format, Pcm16MonoResampler};

// =========================================================================
// 公共类型
// =========================================================================

/// 管理器 → bridge/test 的输出帧 (对应 WS server→client 消息)。
#[derive(Debug, Clone)]
pub enum ManagerOutput {
    Ack {
        dictation_id: String,
        ack_seq: i64,
    },
    Partial {
        dictation_id: String,
        text: String,
    },
    FinishAccepted {
        dictation_id: String,
        timeout_ms: u64,
    },
    Final {
        dictation_id: String,
        text: String,
    },
    Error {
        dictation_id: String,
        error: String,
        retryable: bool,
        reason_code: Option<String>,
    },
    /// 响应客户端 `ping` (应用层 pong)。
    Pong,
}

/// STT 会话推给管理器的异步事件 (对应 Node EventEmitter emit)。
#[derive(Debug, Clone)]
pub enum SessionEvent {
    Committed { segment_id: String },
    Transcript {
        segment_id: String,
        transcript: String,
        is_final: bool,
    },
    Error(String),
}

/// `create_stt_session` 的返回: 成功给会话, 失败给就绪错误。
pub enum CreateSttOutcome {
    Session(Box<dyn SttSession>),
    Error {
        error: String,
        retryable: bool,
        reason_code: Option<String>,
    },
}

/// STT 流式转录会话契约 — 对应 Node `StreamingTranscriptionSession`。
///
/// 实现者负责: `append_pcm16`/`commit`/`clear`/`close` 是同步方法 (可能
/// 内部 spawn 异步工作); 异步结果通过 channel 在 `try_next_event` 暴露。
pub trait SttSession: Send {
    /// 提供方要求的采样率 (客户端 PCM 会被重采样到此)。
    fn required_sample_rate(&self) -> u32;
    /// 追加 PCM16LE 字节到当前段。
    fn append_pcm16(&mut self, pcm16: &[u8]);
    /// 提交当前段 (异步产出 `Committed` + 最终 `Transcript`)。
    fn commit(&mut self);
    /// 清除当前段的累积音频 (静音抑制)。
    fn clear(&mut self);
    /// 关闭会话, 释放资源。
    fn close(&mut self);
    /// 非阻塞取一个待处理事件; 无事件返回 `None`。
    fn try_next_event(&mut self) -> Option<SessionEvent>;
}

/// `start` 消息的提供方/配置选项 (对应 Node `options`)。
#[derive(Debug, Clone, Default)]
pub struct StartOptions {
    pub provider: Option<String>,
    pub language: Option<String>,
    /// 本地模型 ID。本轮 local 提供方不支持, 此字段仅解析保留。
    #[allow(dead_code)]
    pub local_model: Option<String>,
    pub openai_compatible: Option<OpenAiCompatibleConfig>,
}

/// openai-compatible 提供方配置。
#[derive(Debug, Clone, Default)]
pub struct OpenAiCompatibleConfig {
    pub base_url: String,
    pub model: String,
    pub api_key: Option<String>,
}

// =========================================================================
// 单流状态
// =========================================================================

struct DictationStream {
    stt: Box<dyn SttSession>,
    #[allow(dead_code)]
    input_rate: u32,
    #[allow(dead_code)]
    output_rate: u32,
    resampler: Option<Pcm16MonoResampler>,
    received_chunks: HashMap<u32, Vec<u8>>,
    next_seq_to_forward: u32,
    ack_seq: i64,
    auto_commit_bytes: usize,
    bytes_since_commit: usize,
    peak_since_commit: i16,
    committed_segment_ids: Vec<String>,
    transcripts_by_segment_id: HashMap<String, String>,
    final_transcript_segment_ids: HashSet<String>,
    awaiting_final_commit: bool,
    finish_requested: bool,
    finish_sealed: bool,
    final_seq: Option<i64>,
    /// finalize 超时截止 (由 `handle_finish` 设置)。
    finalize_deadline: Option<Instant>,
    /// finalize 超时毫秒 (用于 `finish_accepted` 回传 + 重设)。
    finalize_timeout_ms: u64,
}

/// auto-commit 决策 (内联于 handle_chunk 转发循环, 避免 &mut self 二次借用)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommitAction {
    /// 不动作 (未达阈值 / 已 finish)。
    None,
    /// 静音段 → clear (不提交)。
    Clear,
    /// 非静音段 → commit。
    Commit,
}

/// 判断当前是否应 auto-commit (对应 Node `maybeAutoCommitSegment` 的条件)。
fn should_auto_commit(stream: &DictationStream) -> CommitAction {
    if stream.finish_requested {
        return CommitAction::None;
    }
    if stream.auto_commit_bytes == 0 || stream.bytes_since_commit < stream.auto_commit_bytes {
        return CommitAction::None;
    }
    if stream.peak_since_commit < super::SILENCE_PEAK_THRESHOLD {
        CommitAction::Clear
    } else {
        CommitAction::Commit
    }
}

// =========================================================================
// 管理器
// =========================================================================

/// 每连接的听写状态机。
///
/// `emit_tx` 接收所有输出帧 (ack/partial/final/error/...); bridge 或测试
/// 从对应的 `mpsc::UnboundedReceiver` 消费并转成 WS 文本帧。
pub struct DictationStreamManager<F>
where
    F: Fn(StartOptions) -> futures_util::future::BoxFuture<'static, CreateSttOutcome> + Send + Sync,
{
    emit_tx: mpsc::UnboundedSender<ManagerOutput>,
    create_stt_session: F,
    final_timeout_ms: u64,
    auto_commit_seconds: f64,
    streams: HashMap<String, DictationStream>,
}

impl<F> DictationStreamManager<F>
where
    F: Fn(StartOptions) -> futures_util::future::BoxFuture<'static, CreateSttOutcome> + Send + Sync,
{
    /// 创建管理器。
    ///
    /// - `emit_tx`: 输出帧通道
    /// - `create_stt_session`: 异步工厂, 入参 `StartOptions`, 返回会话或就绪错误
    /// - `final_timeout_ms`: finalize 基础超时 (默认 `DEFAULT_FINAL_TIMEOUT_MS`)
    /// - `auto_commit_seconds`: auto-commit 秒数 (默认 `DEFAULT_AUTO_COMMIT_SECONDS`)
    pub fn new(
        emit_tx: mpsc::UnboundedSender<ManagerOutput>,
        create_stt_session: F,
        final_timeout_ms: u64,
        auto_commit_seconds: f64,
    ) -> Self {
        Self {
            emit_tx,
            create_stt_session,
            final_timeout_ms,
            auto_commit_seconds,
            streams: HashMap::new(),
        }
    }

    /// 排空所有活跃流的待处理 STT 事件, 返回产生的输出 (按序)。
    /// WS bridge 依赖各 `handle_*` 内部的排空, 此方法主要用于测试与诊断。
    #[allow(dead_code)]
    pub fn pump_events(&mut self) -> Vec<ManagerOutput> {
        let mut outputs = Vec::new();
        // 收集所有活跃 dictation_id (避免 borrow 冲突)
        let ids: Vec<String> = self.streams.keys().cloned().collect();
        for dictation_id in ids {
            self.drain_stream_events(&dictation_id, &mut outputs);
        }
        outputs
    }

    /// 排空单个流的待处理事件 (对应 Node `stt.on('committed'/'transcript'/'error')` 回调)。
    fn drain_stream_events(&mut self, dictation_id: &str, outputs: &mut Vec<ManagerOutput>) {
        loop {
            // 先取事件 (需要可变借用 stt), 再处理 (可能移除 stream)
            let event = match self.streams.get_mut(dictation_id) {
                Some(stream) => stream.stt.try_next_event(),
                None => return,
            };
            let Some(event) = event else {
                return;
            };
            match event {
                SessionEvent::Committed { segment_id } => {
                    self.on_committed(dictation_id, segment_id, outputs);
                }
                SessionEvent::Transcript {
                    segment_id,
                    transcript,
                    is_final,
                } => {
                    self.on_transcript(dictation_id, segment_id, transcript, is_final, outputs);
                }
                SessionEvent::Error(msg) => {
                    self.fail_and_cleanup_stream(dictation_id, msg, true, None, outputs);
                    return; // 流已移除
                }
            }
        }
    }

    /// 处理 `start` (对应 Node `handleStart`)。异步因 `createSttSession`。
    pub async fn handle_start(
        &mut self,
        dictation_id: String,
        format: &str,
        start_options: StartOptions,
    ) {
        let mut outputs = Vec::new();
        // 清理已有同名流
        self.cleanup_stream(&dictation_id, &mut outputs);

        let input_rate = parse_pcm_rate_from_format(format, Some(16000)).unwrap_or(16000);
        if input_rate == 0 {
            self.fail_stream(
                &dictation_id,
                &format!("Invalid dictation input rate in format: {format}"),
                false,
                None,
                &mut outputs,
            );
            self.forward_outputs(outputs);
            return;
        }

        let resolved = (self.create_stt_session)(start_options).await;
        let stt = match resolved {
            CreateSttOutcome::Session(s) => s,
            CreateSttOutcome::Error {
                error,
                retryable,
                reason_code,
            } => {
                self.fail_stream(&dictation_id, &error, retryable, reason_code, &mut outputs);
                self.forward_outputs(outputs);
                return;
            }
        };

        let output_rate = stt.required_sample_rate();
        let resampler = if input_rate == output_rate {
            None
        } else {
            Some(Pcm16MonoResampler::new(input_rate, output_rate))
        };
        let auto_commit_bytes = if self.auto_commit_seconds > 0.0 {
            // 对应 Node: Math.round(autoCommitSeconds * requiredSampleRate * 2)
            std::cmp::max(
                1usize,
                (self.auto_commit_seconds * output_rate as f64 * 2.0).round() as usize,
            )
        } else {
            0
        };

        let stream = DictationStream {
            stt,
            input_rate,
            output_rate,
            resampler,
            received_chunks: HashMap::new(),
            next_seq_to_forward: 0,
            ack_seq: -1,
            auto_commit_bytes,
            bytes_since_commit: 0,
            peak_since_commit: 0,
            committed_segment_ids: Vec::new(),
            transcripts_by_segment_id: HashMap::new(),
            final_transcript_segment_ids: HashSet::new(),
            awaiting_final_commit: false,
            finish_requested: false,
            finish_sealed: false,
            final_seq: None,
            finalize_deadline: None,
            finalize_timeout_ms: self.final_timeout_ms,
        };
        self.streams.insert(dictation_id.clone(), stream);

        let _ = self.emit_tx.send(ManagerOutput::Ack {
            dictation_id,
            ack_seq: -1,
        });
    }

    /// 处理 `chunk` (对应 Node `handleChunk`)。同步。触发后排空事件。
    pub fn handle_chunk(&mut self, dictation_id: &str, seq: u32, audio_base64: &str) {
        // 先排空已有事件 (之前的 commit/transcribe 可能已完成)
        let mut outputs = Vec::new();
        self.drain_stream_events(dictation_id, &mut outputs);
        self.forward_outputs(outputs);

        let Some(stream) = self.streams.get_mut(dictation_id) else {
            self.fail_stream(
                dictation_id,
                "Dictation stream not started",
                true,
                None,
                &mut Vec::new(),
            );
            return;
        };

        // seq < nextSeqToForward → 重发 ack (已处理过的旧 seq)
        if seq < stream.next_seq_to_forward {
            let ack_seq = stream.ack_seq;
            let _ = self.emit_tx.send(ManagerOutput::Ack {
                dictation_id: dictation_id.to_string(),
                ack_seq,
            });
            return;
        }

        // 去重: 仅在该 seq 未见过时解码并缓存 (对齐 Node 的 chunk 去重)。
        use std::collections::hash_map::Entry;
        match stream.received_chunks.entry(seq) {
            Entry::Occupied(_) => { /* 已缓存, 跳过解码 (去重) */ }
            Entry::Vacant(e) => {
                let chunk = match base64_decode(audio_base64) {
                    Some(c) => c,
                    None => return,
                };
                // 奇数字节 → 截断到偶数 (对齐 Node chunk.subarray)
                let chunk = if chunk.len() % 2 != 0 {
                    chunk[..chunk.len() - 1].to_vec()
                } else {
                    chunk
                };
                e.insert(chunk);
            }
        }

        // 按序转发连续 chunk
        let mut outputs = Vec::new();
        loop {
            let dictation_id_owned = dictation_id.to_string();
            let stream = match self.streams.get_mut(&dictation_id_owned) {
                Some(s) => s,
                None => break,
            };
            if !stream.received_chunks.contains_key(&stream.next_seq_to_forward) {
                break;
            }
            let next_seq = stream.next_seq_to_forward;
            let pcm16 = stream.received_chunks.remove(&next_seq).unwrap_or_default();

            let resampled = if let Some(r) = stream.resampler.as_mut() {
                match r.process_chunk(&pcm16) {
                    Ok(b) => b,
                    Err(e) => {
                        self.fail_and_cleanup_stream(
                            &dictation_id_owned,
                            e,
                            true,
                            None,
                            &mut outputs,
                        );
                        self.forward_outputs(outputs);
                        return;
                    }
                }
            } else {
                pcm16
            };

            if !resampled.is_empty() {
                stream.stt.append_pcm16(&resampled);
                stream.bytes_since_commit += resampled.len();
                if let Ok(peak) = pcm16le_peak_abs(&resampled) {
                    if peak > stream.peak_since_commit {
                        stream.peak_since_commit = peak;
                    }
                }
                // maybeAutoCommitSegment (可能 commit, 触发异步 transcribe)
                // 内联决策以避免 &mut self 二次借用 (stream 仍被借用)
                let commit_action = should_auto_commit(stream);
                if commit_action == CommitAction::Clear {
                    stream.stt.clear();
                    stream.bytes_since_commit = 0;
                    stream.peak_since_commit = 0;
                } else if commit_action == CommitAction::Commit {
                    stream.bytes_since_commit = 0;
                    stream.peak_since_commit = 0;
                    stream.stt.commit();
                }
            }

            stream.next_seq_to_forward += 1;
            stream.ack_seq = stream.next_seq_to_forward as i64 - 1;
        }

        // 排空 commit 触发的事件
        self.drain_stream_events(dictation_id, &mut outputs);
        // maybeSeal + maybeFinalize
        self.maybe_seal_stream_finish(dictation_id, &mut outputs);
        self.maybe_finalize_stream(dictation_id, &mut outputs);

        // 发 ack
        if let Some(stream) = self.streams.get(dictation_id) {
            let ack_seq = stream.ack_seq;
            outputs.push(ManagerOutput::Ack {
                dictation_id: dictation_id.to_string(),
                ack_seq,
            });
        }
        self.forward_outputs(outputs);
    }

    /// 处理 `finish` (对应 Node `handleFinish`)。
    pub fn handle_finish(&mut self, dictation_id: &str, final_seq: i64) {
        let mut outputs = Vec::new();
        self.drain_stream_events(dictation_id, &mut outputs);

        let Some(stream) = self.streams.get_mut(dictation_id) else {
            self.fail_stream(
                dictation_id,
                "Dictation stream not started",
                true,
                None,
                &mut outputs,
            );
            self.forward_outputs(outputs);
            return;
        };

        stream.finish_requested = true;
        stream.final_seq = Some(final_seq);

        // finish 到达但从未收到 chunk → fail fast (对齐 Node:238-251)
        if final_seq >= 0
            && stream.ack_seq < 0
            && stream.next_seq_to_forward == 0
            && stream.received_chunks.is_empty()
        {
            self.fail_stream(
                dictation_id,
                "Dictation finished but no audio chunks were received",
                true,
                None,
                &mut outputs,
            );
            self.cleanup_stream(dictation_id, &mut outputs);
            self.forward_outputs(outputs);
            return;
        }

        self.maybe_seal_stream_finish(dictation_id, &mut outputs);
        // seal 可能 commit() 入队新事件 (Committed/Transcript); 排空它们以驱动 finalize。
        // 对齐 Node: seal 的 commit 触发 EventEmitter 回调, 回调里再次 finalize。
        self.drain_stream_events(dictation_id, &mut outputs);
        self.maybe_finalize_stream(dictation_id, &mut outputs);

        // 设置 finalize 超时
        let timeout_ms = self
            .streams
            .get(dictation_id)
            .map(|s| self.estimate_finalization_timeout(s))
            .unwrap_or(self.final_timeout_ms);

        if let Some(stream) = self.streams.get_mut(dictation_id) {
            stream.finalize_deadline = Some(Instant::now() + std::time::Duration::from_millis(timeout_ms));
            stream.finalize_timeout_ms = timeout_ms;
        }

        outputs.push(ManagerOutput::FinishAccepted {
            dictation_id: dictation_id.to_string(),
            timeout_ms,
        });
        self.forward_outputs(outputs);
    }

    /// 处理 `cancel` (对应 Node `handleCancel`)。
    pub fn handle_cancel(&mut self, dictation_id: &str) {
        let mut outputs = Vec::new();
        self.cleanup_stream(dictation_id, &mut outputs);
        self.forward_outputs(outputs);
    }

    /// 处理应用层 `ping`。
    pub fn handle_ping(&mut self) {
        let _ = self.emit_tx.send(ManagerOutput::Pong);
    }

    /// finalize 超时触发 (由 bridge 计时器调用)。
    pub fn on_finalize_timeout(&mut self, dictation_id: &str) {
        let mut outputs = Vec::new();
        self.fail_and_cleanup_stream(
            dictation_id,
            "Timed out waiting for final transcription".to_string(),
            true,
            None,
            &mut outputs,
        );
        self.forward_outputs(outputs);
    }

    /// 返回最早到期的 finalize 截止 (dictation_id, deadline)。bridge 用此设 sleep。
    pub fn earliest_finalize_deadline(&self) -> Option<(&str, Instant)> {
        self.streams
            .iter()
            .filter_map(|(id, s)| s.finalize_deadline.map(|d| (id.as_str(), d)))
            .min_by_key(|(_, d)| *d)
    }

    /// 清理所有流 (WS 关闭时调用)。
    pub fn cleanup_all(&mut self) {
        let ids: Vec<String> = self.streams.keys().cloned().collect();
        let mut outputs = Vec::new();
        for id in ids {
            self.cleanup_stream(&id, &mut outputs);
        }
        self.forward_outputs(outputs);
    }

    // ------------------------------------------------------------------
    // 内部: 事件处理 (对应 Node stt.on 回调)
    // ------------------------------------------------------------------

    fn on_committed(&mut self, dictation_id: &str, segment_id: String, outputs: &mut Vec<ManagerOutput>) {
        {
            let Some(stream) = self.streams.get_mut(dictation_id) else {
                return;
            };
            stream.committed_segment_ids.push(segment_id);
            stream.bytes_since_commit = 0;
            stream.peak_since_commit = 0;
            if stream.finish_requested && stream.awaiting_final_commit {
                stream.awaiting_final_commit = false;
            }
        }
        self.maybe_finalize_stream(dictation_id, outputs);
    }

    fn on_transcript(
        &mut self,
        dictation_id: &str,
        segment_id: String,
        transcript: String,
        is_final: bool,
        outputs: &mut Vec<ManagerOutput>,
    ) {
        let need_partial_text;
        {
            let Some(stream) = self.streams.get_mut(dictation_id) else {
                return;
            };
            stream
                .transcripts_by_segment_id
                .insert(segment_id.clone(), transcript);
            if is_final {
                stream.final_transcript_segment_ids.insert(segment_id.clone());
            }
            if stream.finish_requested && stream.awaiting_final_commit && is_final {
                stream.awaiting_final_commit = false;
            }

            // 拼 partial 文本
            let in_committed = stream.committed_segment_ids.contains(&segment_id);
            let mut ordered_ids = stream.committed_segment_ids.clone();
            if !in_committed {
                ordered_ids.push(segment_id.clone());
            }
            let partial_text = ordered_ids
                .iter()
                .map(|id| stream.transcripts_by_segment_id.get(id).cloned().unwrap_or_default())
                .collect::<Vec<_>>()
                .join(" ")
                .trim()
                .to_string();
            need_partial_text = partial_text;
        }

        outputs.push(ManagerOutput::Partial {
            dictation_id: dictation_id.to_string(),
            text: need_partial_text,
        });

        self.maybe_seal_stream_finish(dictation_id, outputs);
        self.maybe_finalize_stream(dictation_id, outputs);
    }

    // ------------------------------------------------------------------
    // 内部: seal / finalize (对应 Node 同名方法)
    // ------------------------------------------------------------------
    // 注: maybeAutoCommitSegment 的决策已内联到 handle_chunk 的转发循环
    // (should_auto_commit + CommitAction), 避免 &mut self 二次借用。

    fn maybe_seal_stream_finish(&mut self, dictation_id: &str, outputs: &mut Vec<ManagerOutput>) {
        let action: Option<bool>; // Some(true)=commit, Some(false)=clear, None=noop
        {
            let Some(stream) = self.streams.get_mut(dictation_id) else {
                return;
            };
            if !stream.finish_requested || stream.final_seq.is_none() {
                return;
            }
            let final_seq = stream.final_seq.unwrap();
            if stream.ack_seq < final_seq {
                return;
            }
            if stream.finish_sealed {
                return;
            }

            if stream.bytes_since_commit > 0 {
                if stream.peak_since_commit < super::SILENCE_PEAK_THRESHOLD {
                    stream.stt.clear();
                    stream.bytes_since_commit = 0;
                    stream.peak_since_commit = 0;
                    stream.awaiting_final_commit = false;
                    Self::drop_uncommitted_nonfinal_transcripts(stream);
                    action = Some(false);
                } else {
                    stream.awaiting_final_commit = true;
                    action = Some(true);
                }
            } else {
                stream.awaiting_final_commit = false;
                action = None;
            }
            stream.finish_sealed = true;
        }

        if action == Some(true) {
            if let Some(stream) = self.streams.get_mut(dictation_id) {
                stream.stt.commit();
            }
        }
        let _ = outputs;
    }

    fn drop_uncommitted_nonfinal_transcripts(stream: &mut DictationStream) {
        let committed_set: HashSet<String> = stream.committed_segment_ids.iter().cloned().collect();
        let to_remove: Vec<String> = stream
            .transcripts_by_segment_id
            .keys()
            .filter(|id| {
                !committed_set.contains(*id) && !stream.final_transcript_segment_ids.contains(*id)
            })
            .cloned()
            .collect();
        for id in to_remove {
            stream.transcripts_by_segment_id.remove(&id);
        }
    }

    fn maybe_finalize_stream(&mut self, dictation_id: &str, outputs: &mut Vec<ManagerOutput>) {
        let finalize_text: Option<String>; // Some(text) → emit final + cleanup
        {
            let Some(stream) = self.streams.get(dictation_id) else {
                return;
            };
            if !stream.finish_requested || stream.final_seq.is_none() {
                return;
            }
            let final_seq = stream.final_seq.unwrap();
            if stream.ack_seq < final_seq {
                return;
            }
            if stream.awaiting_final_commit {
                return;
            }

            let committed_set: HashSet<String> =
                stream.committed_segment_ids.iter().cloned().collect();
            let mut ordered_segment_ids = stream.committed_segment_ids.clone();
            for segment_id in stream.transcripts_by_segment_id.keys() {
                if !committed_set.contains(segment_id) {
                    ordered_segment_ids.push(segment_id.clone());
                }
            }

            if ordered_segment_ids.is_empty() {
                finalize_text = Some(String::new());
            } else {
                let all_ready = ordered_segment_ids.iter().all(|id| {
                    stream.final_transcript_segment_ids.contains(id)
                });
                if !all_ready {
                    return;
                }
                let text = ordered_segment_ids
                    .iter()
                    .map(|id| stream.transcripts_by_segment_id.get(id).cloned().unwrap_or_default())
                    .collect::<Vec<_>>()
                    .join(" ")
                    .trim()
                    .to_string();
                finalize_text = Some(text);
            }
        }

        if let Some(text) = finalize_text {
            outputs.push(ManagerOutput::Final {
                dictation_id: dictation_id.to_string(),
                text,
            });
            self.cleanup_stream(dictation_id, outputs);
        }
    }

    // ------------------------------------------------------------------
    // 内部: fail / cleanup / emit
    // ------------------------------------------------------------------

    fn estimate_finalization_timeout(&self, stream: &DictationStream) -> u64 {
        let bytes_per_second = std::cmp::max(1, stream.output_rate as u64 * 2);

        let pending_committed_segments = stream
            .committed_segment_ids
            .iter()
            .filter(|id| !stream.final_transcript_segment_ids.contains(*id))
            .count() as u64;

        let committed_set: HashSet<&String> = stream.committed_segment_ids.iter().collect();
        let pending_uncommitted = stream
            .transcripts_by_segment_id
            .keys()
            .filter(|id| {
                !committed_set.contains(*id)
                    && !stream.final_transcript_segment_ids.contains(*id)
            })
            .count() as u64;

        let pending_segments =
            pending_committed_segments + pending_uncommitted + if stream.awaiting_final_commit { 1 } else { 0 };

        let pending_audio_seconds = (stream.bytes_since_commit as u64).div_ceil(bytes_per_second);
        let missing_seq_count = stream
            .final_seq
            .map(|fs| std::cmp::max(0, fs - stream.ack_seq) as u64)
            .unwrap_or(0);

        let extra_ms = pending_segments * super::FINAL_TIMEOUT_PER_PENDING_SEGMENT_MS
            + pending_audio_seconds * super::FINAL_TIMEOUT_PER_PENDING_AUDIO_SECOND_MS
            + missing_seq_count * super::FINAL_TIMEOUT_PER_MISSING_SEQ_MS;

        std::cmp::max(
            self.final_timeout_ms,
            std::cmp::min(
                super::FINAL_TIMEOUT_MAX_MS,
                self.final_timeout_ms + extra_ms,
            ),
        )
    }

    fn fail_stream(
        &self,
        dictation_id: &str,
        error: &str,
        retryable: bool,
        reason_code: Option<String>,
        outputs: &mut Vec<ManagerOutput>,
    ) {
        outputs.push(ManagerOutput::Error {
            dictation_id: dictation_id.to_string(),
            error: error.to_string(),
            retryable,
            reason_code,
        });
    }

    fn fail_and_cleanup_stream(
        &mut self,
        dictation_id: &str,
        error: String,
        retryable: bool,
        reason_code: Option<String>,
        outputs: &mut Vec<ManagerOutput>,
    ) {
        self.fail_stream(dictation_id, &error, retryable, reason_code, outputs);
        self.cleanup_stream(dictation_id, outputs);
    }

    fn cleanup_stream(&mut self, dictation_id: &str, _outputs: &mut Vec<ManagerOutput>) {
        if let Some(mut stream) = self.streams.remove(dictation_id) {
            stream.stt.close();
        }
    }

    fn forward_outputs(&self, outputs: Vec<ManagerOutput>) {
        for o in outputs {
            let _ = self.emit_tx.send(o);
        }
    }
}

// =========================================================================
// 辅助
// =========================================================================

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.decode(s).ok()
}

// =========================================================================
// 测试 — 移植自 stream-manager.test.js (6 个 it)
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    const FORMAT: &str = "audio/pcm;rate=16000;bits=16";

    /// FakeSttSession — 模拟 Node 测试里的 EventEmitter-based fake。
    /// `commit()` 立即 push `Committed`, 然后 spawn 一个延迟 0 的 `Transcript`。
    /// `try_next_event` 从内部队列取。
    struct FakeSttSession {
        required_rate: u32,
        appended: Arc<Mutex<Vec<Vec<u8>>>>,
        commits: Arc<Mutex<u32>>,
        clears: Arc<Mutex<u32>>,
        closed: Arc<Mutex<bool>>,
        events_tx: mpsc::UnboundedSender<SessionEvent>,
        events_rx: Mutex<mpsc::UnboundedReceiver<SessionEvent>>,
        segment_counter: Mutex<u32>,
        transcript_by_segment: Box<dyn Fn() -> String + Send + Sync>,
    }

    impl FakeSttSession {
        #[allow(clippy::type_complexity)]
        fn new() -> (Self, Arc<Mutex<Vec<Vec<u8>>>>, Arc<Mutex<u32>>, Arc<Mutex<u32>>, Arc<Mutex<bool>>) {
            Self::with_transcript(|| "hello world".to_string())
        }

        #[allow(clippy::type_complexity)]
        fn with_transcript<F>(f: F) -> (Self, Arc<Mutex<Vec<Vec<u8>>>>, Arc<Mutex<u32>>, Arc<Mutex<u32>>, Arc<Mutex<bool>>)
        where
            F: Fn() -> String + Send + Sync + 'static,
        {
            let (tx, rx) = mpsc::unbounded_channel();
            let appended = Arc::new(Mutex::new(Vec::new()));
            let commits = Arc::new(Mutex::new(0));
            let clears = Arc::new(Mutex::new(0));
            let closed = Arc::new(Mutex::new(false));
            let session = Self {
                required_rate: 16000,
                appended: appended.clone(),
                commits: commits.clone(),
                clears: clears.clone(),
                closed: closed.clone(),
                events_tx: tx,
                events_rx: Mutex::new(rx),
                segment_counter: Mutex::new(0),
                transcript_by_segment: Box::new(f),
            };
            (session, appended, commits, clears, closed)
        }
    }

    impl SttSession for FakeSttSession {
        fn required_sample_rate(&self) -> u32 {
            self.required_rate
        }
        fn append_pcm16(&mut self, pcm16: &[u8]) {
            self.appended.lock().unwrap().push(pcm16.to_vec());
        }
        fn commit(&mut self) {
            *self.commits.lock().unwrap() += 1;
            let mut sc = self.segment_counter.lock().unwrap();
            let segment_id = format!("seg-{}", *sc);
            *sc += 1;
            drop(sc);
            let _ = self.events_tx.send(SessionEvent::Committed {
                segment_id: segment_id.clone(),
            });
            // 模拟异步 transcript (Node 用 setTimeout 0; 这里立即入队, 由 pump 排空)
            let text = (self.transcript_by_segment)();
            let _ = self.events_tx.send(SessionEvent::Transcript {
                segment_id,
                transcript: text,
                is_final: true,
            });
        }
        fn clear(&mut self) {
            *self.clears.lock().unwrap() += 1;
        }
        fn close(&mut self) {
            *self.closed.lock().unwrap() = true;
        }
        fn try_next_event(&mut self) -> Option<SessionEvent> {
            self.events_rx.lock().unwrap().try_recv().ok()
        }
    }

    fn loud_chunk_b64(samples: usize, amplitude: i16) -> String {
        use base64::Engine;
        let mut buf = Vec::with_capacity(samples * 2);
        for i in 0..samples {
            let v = if i % 2 == 0 { amplitude } else { -amplitude };
            buf.extend_from_slice(&v.to_le_bytes());
        }
        base64::engine::general_purpose::STANDARD.encode(&buf)
    }

    fn silent_chunk_b64(samples: usize) -> String {
        use base64::Engine;
        let buf = vec![0u8; samples * 2];
        base64::engine::general_purpose::STANDARD.encode(&buf)
    }

    /// 构造 manager + 收集输出。工厂返回 FakeSttSession。
    #[allow(clippy::type_complexity)]
    fn create_manager(
    ) -> (
        DictationStreamManager<
            impl Fn(StartOptions) -> futures_util::future::BoxFuture<'static, CreateSttOutcome> + Send + Sync,
        >,
        Arc<Mutex<Vec<Vec<u8>>>>,
        Arc<Mutex<u32>>,
        Arc<Mutex<u32>>,
        Arc<Mutex<bool>>,
        mpsc::UnboundedReceiver<ManagerOutput>,
    ) {
        let (tx, rx) = mpsc::unbounded_channel();
        let (session, appended, commits, clears, closed) = FakeSttSession::new();
        let session_box: Box<dyn SttSession> = Box::new(session);
        // 工厂每次返回同一个 session (测试用); 用 Arc + Mutex 持有
        // 但 SttSession 不是 Clone; 测试只 start 一次, 所以用 Option + take
        let session_holder = Arc::new(Mutex::new(Some(session_box)));
        let factory = move |_opts: StartOptions| {
            let holder = session_holder.clone();
            Box::pin(async move {
                let s = holder.lock().unwrap().take();
                match s {
                    Some(s) => CreateSttOutcome::Session(s),
                    None => CreateSttOutcome::Error {
                        error: "no session".to_string(),
                        retryable: false,
                        reason_code: None,
                    },
                }
            })
                as futures_util::future::BoxFuture<'static, CreateSttOutcome>
        };
        let manager = DictationStreamManager::new(
            tx,
            factory,
            super::super::DEFAULT_FINAL_TIMEOUT_MS,
            super::super::DEFAULT_AUTO_COMMIT_SECONDS,
        );
        (manager, appended, commits, clears, closed, rx)
    }

    /// 排空 manager 输出直到谓词满足或超时。返回累积的所有输出 (含满足谓词的那一批)。
    async fn wait_for_outputs<F>(
        rx: &mut mpsc::UnboundedReceiver<ManagerOutput>,
        pred: F,
    ) -> Vec<ManagerOutput>
    where
        F: Fn(&[ManagerOutput]) -> bool,
    {
        let mut collected = Vec::new();
        let deadline = Instant::now() + std::time::Duration::from_secs(2);
        loop {
            // 先检查已有收集
            if pred(&collected) {
                return collected;
            }
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .unwrap_or(std::time::Duration::ZERO);
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Some(o)) => collected.push(o),
                _ => {
                    // 超时; 最后检查一次
                    if pred(&collected) {
                        return collected;
                    }
                    panic!("wait_for_outputs timed out; collected={collected:?}");
                }
            }
        }
    }

    fn collect_available(rx: &mut mpsc::UnboundedReceiver<ManagerOutput>) -> Vec<ManagerOutput> {
        let mut out = Vec::new();
        while let Ok(o) = rx.try_recv() {
            out.push(o);
        }
        out
    }

    // ===== 移植测试 1: transcribes ordered chunks and emits final text =====
    #[tokio::test]
    async fn transcribes_ordered_chunks_and_emits_final_text() {
        let (mut manager, appended, commits, _clears, closed, mut rx) = create_manager();

        manager
            .handle_start("d1".to_string(), FORMAT, StartOptions::default())
            .await;

        // chunk 0
        manager.handle_chunk("d1", 0, &loud_chunk_b64(1600, 8000));
        // chunk 1
        manager.handle_chunk("d1", 1, &loud_chunk_b64(1600, 8000));
        manager.pump_events();
        // finish
        manager.handle_finish("d1", 1);

        // 排空直到 final (commit 的 transcript 事件需 pump)
        let outs = wait_for_outputs(&mut rx, |outs| {
            outs.iter().any(|o| matches!(o, ManagerOutput::Final { .. }))
        })
        .await;

        let final_text = outs
            .iter()
            .find_map(|o| match o {
                ManagerOutput::Final { text, .. } => Some(text.clone()),
                _ => None,
            })
            .expect("final message missing");
        assert_eq!(final_text, "hello world");

        assert!(!appended.lock().unwrap().is_empty(), "expected at least 1 appended chunk");
        assert!(*commits.lock().unwrap() >= 1, "expected at least 1 commit");
        assert!(*closed.lock().unwrap(), "session should be closed after final");
    }

    // ===== 移植测试 2: reorders out-of-order chunks =====
    #[tokio::test]
    async fn reorders_out_of_order_chunks() {
        let (mut manager, appended, _commits, _clears, _closed, mut rx) = create_manager();

        manager
            .handle_start("d1".to_string(), FORMAT, StartOptions::default())
            .await;

        // 先发 seq 1 → 不应追加 (等 seq 0)
        manager.handle_chunk("d1", 1, &loud_chunk_b64(1600, 8000));
        assert_eq!(appended.lock().unwrap().len(), 0, "seq 1 before seq 0 should not append");

        // 发 seq 0 → 两个都追加 (按序)
        manager.handle_chunk("d1", 0, &loud_chunk_b64(1600, 8000));
        assert!(
            appended.lock().unwrap().len() >= 2,
            "after seq 0, both chunks should append"
        );

        manager.handle_finish("d1", 1);
        wait_for_outputs(&mut rx, |outs| {
            outs.iter().any(|o| matches!(o, ManagerOutput::Final { .. }))
        })
        .await;
    }

    // ===== 移植测试 3: clears silence-only tails =====
    #[tokio::test]
    async fn clears_silence_only_tails_instead_of_committing() {
        let (mut manager, _appended, commits, clears, _closed, mut rx) = create_manager();

        manager
            .handle_start("d1".to_string(), FORMAT, StartOptions::default())
            .await;

        // 静音 chunk
        manager.handle_chunk("d1", 0, &silent_chunk_b64(1600));
        manager.handle_finish("d1", 0);

        wait_for_outputs(&mut rx, |outs| {
            outs.iter().any(|o| matches!(o, ManagerOutput::Final { .. }))
        })
        .await;

        let outs = collect_available(&mut rx);
        let final_text = outs
            .iter()
            .find_map(|o| match o {
                ManagerOutput::Final { text, .. } => Some(text.clone()),
                _ => None,
            })
            .unwrap_or_default();
        assert_eq!(final_text, "");
        assert_eq!(*commits.lock().unwrap(), 0, "silence should not commit");
        assert!(*clears.lock().unwrap() >= 1, "silence should clear");
    }

    // ===== 移植测试 4: fails fast when finish with no chunks =====
    #[tokio::test]
    async fn fails_fast_when_finish_arrives_with_no_chunks() {
        let (mut manager, _appended, _commits, _clears, closed, mut rx) = create_manager();

        manager
            .handle_start("d1".to_string(), FORMAT, StartOptions::default())
            .await;
        manager.handle_finish("d1", 3);

        let outs = collect_available(&mut rx);
        let has_error = outs.iter().any(|o| {
            matches!(
                o,
                ManagerOutput::Error {
                    retryable: true,
                    ..
                }
            )
        });
        assert!(has_error, "expected a retryable error, got {:?}", outs);
        assert!(*closed.lock().unwrap(), "session should be closed after fail");
    }

    // ===== 移植测试 5: reports provider readiness errors =====
    #[tokio::test]
    async fn reports_provider_readiness_errors() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let factory = |_opts: StartOptions| {
            Box::pin(async {
                CreateSttOutcome::Error {
                    error: "Dictation model is downloading".to_string(),
                    retryable: true,
                    reason_code: Some("model_download_in_progress".to_string()),
                }
            }) as futures_util::future::BoxFuture<'static, CreateSttOutcome>
        };
        let mut manager = DictationStreamManager::new(
            tx,
            factory,
            super::super::DEFAULT_FINAL_TIMEOUT_MS,
            super::super::DEFAULT_AUTO_COMMIT_SECONDS,
        );

        manager
            .handle_start("d1".to_string(), FORMAT, StartOptions::default())
            .await;

        let outs = collect_available(&mut rx);
        let error = outs
            .iter()
            .find_map(|o| match o {
                ManagerOutput::Error {
                    reason_code,
                    retryable,
                    ..
                } => Some((reason_code.clone(), *retryable)),
                _ => None,
            })
            .expect("expected error message");
        assert_eq!(error.0.as_deref(), Some("model_download_in_progress"));
        assert!(error.1);
    }

    // ===== 移植测试 6: emits partials across segments =====
    #[tokio::test]
    async fn emits_partials_as_segment_transcripts_arrive() {
        // 两个段: "first part" / "second part"
        let counter = Arc::new(Mutex::new(0u32));
        let counter_clone = counter.clone();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (session, _appended, _commits, _clears, _closed) =
            FakeSttSession::with_transcript(move || {
                let mut c = counter_clone.lock().unwrap();
                *c += 1;
                if *c == 1 {
                    "first part".to_string()
                } else {
                    "second part".to_string()
                }
            });
        let session_box: Box<dyn SttSession> = Box::new(session);
        let holder = Arc::new(Mutex::new(Some(session_box)));
        let holder_clone = holder.clone();
        let factory = move |_opts: StartOptions| {
            let h = holder_clone.clone();
            Box::pin(async move {
                h.lock()
                    .unwrap()
                    .take()
                    .map(CreateSttOutcome::Session)
                    .unwrap_or(CreateSttOutcome::Error {
                        error: "no session".to_string(),
                        retryable: false,
                        reason_code: None,
                    })
            })
                as futures_util::future::BoxFuture<'static, CreateSttOutcome>
        };
        // auto-commit 0.05s ≈ 1600 samples (force 短段)
        // 0.05 * 16000 * 2 = 1600 bytes
        let mut manager = DictationStreamManager::new(tx, factory, super::super::DEFAULT_FINAL_TIMEOUT_MS, 0.05);

        manager
            .handle_start("d1".to_string(), FORMAT, StartOptions::default())
            .await;

        // chunk 0 → auto-commit 触发 seg-0 (first part)
        manager.handle_chunk("d1", 0, &loud_chunk_b64(1600, 8000));
        // 等 seg-0 transcript
        wait_for_outputs(&mut rx, |outs| {
            outs.iter().any(|o| {
                matches!(o, ManagerOutput::Partial { text, .. } if text.contains("first part"))
            })
        })
        .await;

        // chunk 1 → seg-1 (second part)
        manager.handle_chunk("d1", 1, &loud_chunk_b64(1600, 8000));
        manager.handle_finish("d1", 1);

        let outs = wait_for_outputs(&mut rx, |outs| {
            outs.iter().any(|o| matches!(o, ManagerOutput::Final { .. }))
        })
        .await;

        let final_text = outs
            .iter()
            .find_map(|o| match o {
                ManagerOutput::Final { text, .. } => Some(text.clone()),
                _ => None,
            })
            .expect("final message missing");
        assert_eq!(final_text, "first part second part");
        let partials = outs
            .iter()
            .filter(|o| matches!(o, ManagerOutput::Partial { .. }))
            .count();
        assert!(partials > 0, "expected at least one partial");
        let _ = counter;
    }
}
