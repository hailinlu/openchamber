//! PCM16 音频辅助函数 — 对应 Node `dictation/audio.js` (195 LOC)。
//!
//! 所有 dictation 音频以 16-bit little-endian 单声道 PCM 传输。客户端按
//! 16 kHz 采集; 提供方可能要求不同采样率, 所以 chunk 在追加到 STT 会话前
//! 用 `Pcm16MonoResampler` 重采样。
//!
//! 移植说明: Node 用 `Buffer` 视图 (Int16Array), Rust 用 `&[u8]` / `Vec<i16>`。
//! 字节序固定 LE (little-endian), 用 `i16::from_le_bytes` 解码。
//!
//! `pcm16le_to_float32` / `float32_to_pcm16le` / `Pcm16MonoResampler` 的
//! 访问器本轮未被调用 — 它们为后续阶段的本地 sherpa STT/TTS 路径预留
//! (对齐 Node `local/sherpa-recognizer.js` / `local/sherpa-tts.js`)。

use once_cell::sync::Lazy;
use regex::Regex;

/// 匹配 `rate=16000` (对应 Node audio.js:16)。允许前导/尾随分隔符 `;,空格` 或串首/串尾。
static RE_PCM_RATE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:^|[;,\s])rate\s*=\s*(\d+)(?:$|[;,\s])").expect("invalid RE_PCM_RATE")
});

/// 从格式串解析采样率, 如 `"audio/pcm;rate=16000;bits=16"`。
/// 找不到时返回 `fallback` (对应 Node `parsePcmRateFromFormat`)。
pub fn parse_pcm_rate_from_format(format: &str, fallback: Option<u32>) -> Option<u32> {
    let s = format;
    if let Some(caps) = RE_PCM_RATE.captures(s) {
        if let Some(m) = caps.get(1) {
            if let Ok(rate) = m.as_str().parse::<u32>() {
                if rate > 0 {
                    return Some(rate);
                }
            }
        }
    }
    fallback
}

/// PCM16LE 缓冲的峰值绝对样本值, 用于静音检测 (对应 Node `pcm16lePeakAbs`)。
/// 奇数字节长度报错。空缓冲返回 0。达到 32767 时提前返回 (早退优化)。
pub fn pcm16le_peak_abs(pcm16le: &[u8]) -> Result<i16, String> {
    if pcm16le.is_empty() {
        return Ok(0);
    }
    if pcm16le.len() % 2 != 0 {
        return Err(format!(
            "PCM16 chunk byteLength must be even, got {}",
            pcm16le.len()
        ));
    }
    let mut peak: i32 = 0;
    for chunk in pcm16le.chunks_exact(2) {
        let v = i16::from_le_bytes([chunk[0], chunk[1]]);
        let abs = if v < 0 { -(v as i32) } else { v as i32 };
        if abs > peak {
            peak = abs;
            if peak >= 32767 {
                break;
            }
        }
    }
    Ok(peak.min(i16::MAX as i32) as i16)
}

/// PCM16LE → Float32 样本 `[-1, 1]`, 可选增益 (对应 Node `pcm16leToFloat32`)。
/// 奇数字节长度报错。输出已 clamp 到 `[-1, 1]`。
#[allow(dead_code)] // 为本地 sherpa STT 路径预留 (后续阶段)
pub fn pcm16le_to_float32(pcm16le: &[u8], gain: f32) -> Result<Vec<f32>, String> {
    if pcm16le.len() % 2 != 0 {
        return Err(format!(
            "PCM16 chunk byteLength must be even, got {}",
            pcm16le.len()
        ));
    }
    let mut out = Vec::with_capacity(pcm16le.len() / 2);
    for chunk in pcm16le.chunks_exact(2) {
        let v = i16::from_le_bytes([chunk[0], chunk[1]]);
        let scaled = (v as f32 / 32768.0) * gain;
        out.push(scaled.clamp(-1.0, 1.0));
    }
    Ok(out)
}

/// 把原始 PCM16LE 单声道音频包进 WAV 容器 (对应 Node `pcm16ToWav`)。
/// 固定单声道 / 16-bit。返回 44 字节头 + PCM 数据。
pub fn pcm16_to_wav(pcm: &[u8], sample_rate: u32) -> Vec<u8> {
    let channels: u32 = 1;
    let bits_per_sample: u32 = 16;
    let header_size = 44usize;
    let byte_rate = (sample_rate * channels * bits_per_sample) / 8;
    let block_align = (channels * bits_per_sample) / 8;

    let mut wav = Vec::with_capacity(header_size + pcm.len());

    // RIFF header
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + pcm.len() as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVE");

    // fmt chunk
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes()); // PCM fmt chunk size
    wav.extend_from_slice(&1u16.to_le_bytes()); // audio_format = 1 (PCM)
    wav.extend_from_slice(&(channels as u16).to_le_bytes());
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&byte_rate.to_le_bytes());
    wav.extend_from_slice(&(block_align as u16).to_le_bytes());
    wav.extend_from_slice(&(bits_per_sample as u16).to_le_bytes());

    // data chunk
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
    wav.extend_from_slice(pcm);

    wav
}

/// Float32 样本 → PCM16LE 字节 (对应 Node `sherpa-tts.js::float32ToPcm16le`)。
/// clamp 到 `[-1, 1]` 后 `round(x * 32767)`。
#[allow(dead_code)] // 为本地 sherpa TTS 路径预留 (后续阶段)
pub fn float32_to_pcm16le(samples: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 2);
    for &s in samples {
        let clamped = s.clamp(-1.0, 1.0);
        let v = (clamped * 32767.0).round() as i16;
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

/// 流式线性插值重采样器 (对应 Node `Pcm16MonoResampler`)。
///
/// PCM16LE 单声道。跨 chunk 边界携带一个样本 (carry sample), 使连续 chunk
/// 重采样无缝。`input_rate → output_rate` 固定。
#[allow(dead_code)] // input_rate/output_rate 字段仅用于诊断; 本轮 openai-compatible 16k→16k 无重采样
pub struct Pcm16MonoResampler {
    input_rate: u32,
    output_rate: u32,
    step: f64,
    pos: f64,
    carry_sample: Option<i16>,
}

impl Pcm16MonoResampler {
    pub fn new(input_rate: u32, output_rate: u32) -> Self {
        Self {
            input_rate,
            output_rate,
            step: input_rate as f64 / output_rate as f64,
            pos: 0.0,
            carry_sample: None,
        }
    }

    #[allow(dead_code)] // 为本地模型路径预留 (后续阶段)
    pub fn reset(&mut self) {
        self.pos = 0.0;
        self.carry_sample = None;
    }

    #[allow(dead_code)] // 为本地模型路径预留 (后续阶段)
    pub fn input_rate(&self) -> u32 {
        self.input_rate
    }
    #[allow(dead_code)] // 为本地模型路径预留 (后续阶段)
    pub fn output_rate(&self) -> u32 {
        self.output_rate
    }

    /// 处理一个 chunk, 返回重采样后的 PCM16LE 字节。
    /// 奇数字节长度报错 (对齐 Node)。
    pub fn process_chunk(&mut self, pcm16le: &[u8]) -> Result<Vec<u8>, String> {
        if pcm16le.is_empty() {
            return Ok(Vec::new());
        }
        if pcm16le.len() % 2 != 0 {
            return Err(format!(
                "PCM16 chunk byteLength must be even, got {}",
                pcm16le.len()
            ));
        }

        // 解码源样本为 i16
        let src_chunk: Vec<i16> = pcm16le
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect();

        let has_carry = self.carry_sample.is_some();
        let src_len = src_chunk.len() + if has_carry { 1 } else { 0 };
        if src_len < 2 {
            // 不足 2 样本无法插值; 保留最后一个样本作为 carry
            self.carry_sample = src_chunk.last().copied().or(self.carry_sample);
            return Ok(Vec::new());
        }

        // 构建 src float 数组 (含 carry)
        let mut src = Vec::with_capacity(src_len);
        let mut offset = 0;
        if has_carry {
            src.push(self.carry_sample.unwrap() as f64 / 32768.0);
            offset = 1;
        }
        for s in &src_chunk {
            src.push(*s as f64 / 32768.0);
        }
        let _ = offset;

        let mut out: Vec<i16> = Vec::new();
        let max_pos = (src.len() - 1) as f64;

        while self.pos < max_pos {
            let i = self.pos.floor() as usize;
            let frac = self.pos - i as f64;
            let s0 = src[i];
            let s1 = src[i + 1];
            let sample = s0 + (s1 - s0) * frac;
            let clamped = sample.clamp(-1.0, 1.0);
            out.push((clamped * 32767.0).round() as i16);
            self.pos += self.step;
        }

        // 更新 carry: 源 chunk 最后一个样本
        self.carry_sample = src_chunk.last().copied();

        // 归一化 pos: 减去已消费的源长度 - 1
        let shift = (src.len() - 1) as f64;
        self.pos -= shift;
        if self.pos < 0.0 {
            self.pos = 0.0;
        }

        // i16 → LE bytes
        let mut bytes = Vec::with_capacity(out.len() * 2);
        for v in out {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        Ok(bytes)
    }
}

// ============================================================================
// 测试
// ============================================================================

#[cfg(test)]
#[allow(clippy::unreadable_literal, clippy::float_cmp)]
mod tests {
    use super::*;

    // ---------------- parse_pcm_rate_from_format ----------------

    #[test]
    fn parse_rate_standard() {
        assert_eq!(
            parse_pcm_rate_from_format("audio/pcm;rate=16000;bits=16", None),
            Some(16000)
        );
    }

    #[test]
    fn parse_rate_missing_returns_fallback() {
        assert_eq!(parse_pcm_rate_from_format("audio/pcm;bits=16", Some(8000)), Some(8000));
        assert_eq!(parse_pcm_rate_from_format("audio/pcm;bits=16", None), None);
    }

    #[test]
    fn parse_rate_case_insensitive() {
        assert_eq!(
            parse_pcm_rate_from_format("audio/pcm;RATE=48000", None),
            Some(48000)
        );
    }

    #[test]
    fn parse_rate_zero_invalid() {
        // rate=0 被视为无效 → fallback
        assert_eq!(parse_pcm_rate_from_format("audio/pcm;rate=0", Some(16000)), Some(16000));
    }

    // ---------------- pcm16le_peak_abs ----------------

    #[test]
    fn peak_abs_empty_is_zero() {
        assert_eq!(pcm16le_peak_abs(&[]).unwrap(), 0);
    }

    #[test]
    fn peak_abs_odd_length_errors() {
        assert!(pcm16le_peak_abs(&[0x01]).is_err());
    }

    #[test]
    fn peak_abs_finds_max() {
        // 样本 [100, -2000, 50] → 峰值 2000
        let mut buf = Vec::new();
        buf.extend_from_slice(&100i16.to_le_bytes());
        buf.extend_from_slice(&(-2000i16).to_le_bytes());
        buf.extend_from_slice(&50i16.to_le_bytes());
        assert_eq!(pcm16le_peak_abs(&buf).unwrap(), 2000);
    }

    #[test]
    fn peak_abs_silent_chunk_is_low() {
        // 全零缓冲 → 峰值 0
        let buf = vec![0u8; 3200]; // 1600 samples × 2 bytes
        assert_eq!(pcm16le_peak_abs(&buf).unwrap(), 0);
    }

    #[test]
    fn peak_abs_early_exit_at_32767() {
        // 样本含 32767 → 立即返回
        let mut buf = Vec::new();
        buf.extend_from_slice(&32767i16.to_le_bytes());
        buf.extend_from_slice(&100i16.to_le_bytes());
        assert_eq!(pcm16le_peak_abs(&buf).unwrap(), 32767);
    }

    // ---------------- pcm16_to_wav ----------------

    #[test]
    fn wav_header_is_44_bytes_and_correct() {
        let pcm = vec![0u8; 3200]; // 1600 samples
        let wav = pcm16_to_wav(&pcm, 16000);
        assert_eq!(wav.len(), 44 + 3200);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[12..16], b"fmt ");
        assert_eq!(&wav[36..40], b"data");
        // data size = 3200
        assert_eq!(u32::from_le_bytes([wav[40], wav[41], wav[42], wav[43]]), 3200);
        // sample rate = 16000
        assert_eq!(u32::from_le_bytes([wav[24], wav[25], wav[26], wav[27]]), 16000);
        // channels = 1
        assert_eq!(u16::from_le_bytes([wav[22], wav[23]]), 1);
        // bits per sample = 16
        assert_eq!(u16::from_le_bytes([wav[34], wav[35]]), 16);
    }

    // ---------------- Pcm16MonoResampler ----------------

    fn loud_chunk(samples: usize, amplitude: i16) -> Vec<u8> {
        let mut buf = Vec::with_capacity(samples * 2);
        for i in 0..samples {
            let v = if i % 2 == 0 { amplitude } else { -amplitude };
            buf.extend_from_slice(&v.to_le_bytes());
        }
        buf
    }

    #[test]
    fn resampler_empty_chunk_returns_empty() {
        let mut r = Pcm16MonoResampler::new(16000, 16000);
        assert_eq!(r.process_chunk(&[]).unwrap().len(), 0);
    }

    #[test]
    fn resampler_odd_length_errors() {
        let mut r = Pcm16MonoResampler::new(16000, 8000);
        assert!(r.process_chunk(&[0x01, 0x02, 0x03]).is_err());
    }

    #[test]
    fn resampler_passthrough_same_rate() {
        // 相同采样率 step=1.0, 每个输入样本产出一个输出样本
        let mut r = Pcm16MonoResampler::new(16000, 16000);
        let chunk = loud_chunk(1600, 8000);
        let out = r.process_chunk(&chunk).unwrap();
        // 输出样本数 = 输入样本数 (carry 模式下首样本可能被跳过, 但同率 step=1 应保留)
        let out_samples = out.len() / 2;
        assert!(
            (1500..=1600).contains(&out_samples),
            "passthrough out_samples={out_samples}"
        );
    }

    #[test]
    fn resampler_downsample_halves_samples() {
        // 16000 → 8000, 输出样本数约为输入的一半
        let mut r = Pcm16MonoResampler::new(16000, 8000);
        let chunk = loud_chunk(1600, 8000);
        let out = r.process_chunk(&chunk).unwrap();
        let out_samples = out.len() / 2;
        assert!(
            (700..=900).contains(&out_samples),
            "downsample out_samples={out_samples} (expected ~800)"
        );
    }

    #[test]
    fn resampler_carries_sample_across_chunks() {
        // 两个连续 chunk 应无缝: 第二个 chunk 的输出不应因 carry 丢失
        let mut r = Pcm16MonoResampler::new(16000, 8000);
        let chunk1 = loud_chunk(1600, 8000);
        let chunk2 = loud_chunk(1600, 8000);
        let out1 = r.process_chunk(&chunk1).unwrap();
        let out2 = r.process_chunk(&chunk2).unwrap();
        // 两个 chunk 都应产出有效输出
        assert!(!out1.is_empty());
        assert!(!out2.is_empty());
    }
}
