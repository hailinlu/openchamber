//! macOS `say` 命令能力探测。
//!
//! 对应 Node `tts/capability-runtime.js`:
//!   - 在 darwin 上运行 `say -v "?"`, 解析每行 `Name Locale # ...`
//!   - 缓存进 `state.say_tts_capability` (startup 时探测一次)
//!
//! Rust 实现里把 platform check 和 command runner 解耦为
//! `detect_say_tts_capability_impl(platform, run_cmd)` 便于测试。

use std::future::Future;
use std::pin::Pin;

use once_cell::sync::Lazy;
use regex::Regex;
use serde::Serialize;

/// `say` TTS 单条 voice 信息。
#[derive(Debug, Clone, Serialize)]
pub struct SayTtsVoice {
    pub name: String,
    pub locale: String,
}

/// 探测结果。
#[derive(Debug, Clone, Serialize)]
pub struct SayTtsCapability {
    pub available: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub voices: Vec<SayTtsVoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl SayTtsCapability {
    /// 未初始化时返回的默认值 (对应 Node `available: false, voices: [], reason: 'Not initialized'`)。
    pub fn not_initialized() -> Self {
        Self {
            available: false,
            voices: vec![],
            reason: Some("Not initialized".to_string()),
        }
    }
}

/// Lazy 解析 `Name Locale # ...` 的行。`^(.+?)\s+([a-zA-Z]{2}_[a-zA-Z]{2,3})\s+#/`。
static SAY_VOICE_LINE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^(.+?)\s+([a-zA-Z]{2}_[a-zA-Z]{2,3})\s+#").unwrap());

/// 默认 `say` 命令运行器 — 异步跑 `say -v "?"`, 失败时返回 Err。
async fn run_say_command_default() -> Result<String, String> {
    let output = tokio::process::Command::new("say")
        .arg("-v")
        .arg("?")
        .output()
        .await
        .map_err(|e| format!("say command not available: {}", e))?;

    if !output.status.success() {
        return Err(format!("say command exited with {}", output.status));
    }

    String::from_utf8(output.stdout).map_err(|e| format!("say stdout not utf-8: {}", e))
}

/// 把任意 async 函数包装成 `Pin<Box<dyn Future + Send>>`, 满足 trait bound。
fn box_future<F>(f: F) -> Pin<Box<dyn Future<Output = F::Output> + Send>>
where
    F: Future + Send + 'static,
{
    Box::pin(f)
}

/// macOS `say` 能力探测入口。
///
/// 对应 Node `detectSayTtsCapability(processLike)` (`capability-runtime.js` line 1-31)。
pub async fn detect_say_tts_capability() -> SayTtsCapability {
    let platform = std::env::consts::OS;
    detect_say_tts_capability_impl(platform, || box_future(run_say_command_default())).await
}

/// 可测试版本: 显式传入 platform 和 command runner (返回 boxed future)。
pub async fn detect_say_tts_capability_impl<F>(platform: &str, run_cmd: F) -> SayTtsCapability
where
    F: FnOnce() -> Pin<Box<dyn Future<Output = Result<String, String>> + Send>>,
{
    if platform != "darwin" {
        return SayTtsCapability {
            available: false,
            voices: vec![],
            reason: Some("Not macOS".to_string()),
        };
    }

    match run_cmd().await {
        Ok(stdout) => {
            let voices = parse_say_voices(&stdout);
            SayTtsCapability {
                available: true,
                voices,
                reason: None,
            }
        }
        Err(_) => SayTtsCapability {
            available: false,
            voices: vec![],
            reason: Some("say command not available".to_string()),
        },
    }
}

/// 解析 `say -v "?"` 输出, 提取 `{ name, locale }`。
///
/// 对应 JS: `line.match(/^(.+?)\s+([a-zA-Z]{2}_[a-zA-Z]{2,3})\s+#/)`。
pub fn parse_say_voices(stdout: &str) -> Vec<SayTtsVoice> {
    stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| {
            SAY_VOICE_LINE.captures(line).map(|cap| SayTtsVoice {
                name: cap.get(1).unwrap().as_str().trim().to_string(),
                locale: cap.get(2).unwrap().as_str().to_string(),
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn run_with_output(stdout: Result<String, String>) -> impl FnOnce() -> Pin<Box<dyn Future<Output = Result<String, String>> + Send>> {
        move || {
            Box::pin(async move { stdout })
        }
    }

    #[tokio::test]
    async fn non_darwin_returns_not_macos() {
        let cap = detect_say_tts_capability_impl("linux", run_with_output(Ok(String::new()))).await;
        assert!(!cap.available);
        assert_eq!(cap.reason.as_deref(), Some("Not macOS"));
        assert!(cap.voices.is_empty());
    }

    #[tokio::test]
    async fn darwin_with_say_output_parses_voices() {
        let sample = "\
Alex                en_US    # Most people are used to this voice
Samantha            en_US    # Female
Daniel              en_GB    # Male
Karen               en_AU    # Female
Mei-Jia             zh_TW    # Mandarin
Yuki                ja_JP    # Japanese
Bad Line            # not a match";
        let cap = detect_say_tts_capability_impl("darwin", run_with_output(Ok(sample.to_string()))).await;
        assert!(cap.available);
        assert_eq!(cap.voices.len(), 6);
        assert_eq!(cap.voices[0].name, "Alex");
        assert_eq!(cap.voices[0].locale, "en_US");
        assert_eq!(cap.voices[2].name, "Daniel");
        assert_eq!(cap.voices[2].locale, "en_GB");
        assert_eq!(cap.voices[4].name, "Mei-Jia");
        assert_eq!(cap.voices[4].locale, "zh_TW");
    }

    #[tokio::test]
    async fn darwin_with_failed_command() {
        let cap = detect_say_tts_capability_impl("darwin", run_with_output(Err("not found".to_string()))).await;
        assert!(!cap.available);
        assert_eq!(cap.reason.as_deref(), Some("say command not available"));
        assert!(cap.voices.is_empty());
    }

    #[tokio::test]
    async fn darwin_with_empty_output_returns_zero_voices() {
        let cap = detect_say_tts_capability_impl("darwin", run_with_output(Ok(String::new()))).await;
        assert!(cap.available, "darwin + successful run → available:true even with no voices");
        assert!(cap.voices.is_empty());
    }

    #[test]
    fn parse_say_voices_skips_blank_lines() {
        let input = "\n\nAlex en_US # hi\n\nSamantha en_US # bye\n";
        let voices = parse_say_voices(input);
        assert_eq!(voices.len(), 2);
        assert_eq!(voices[0].name, "Alex");
        assert_eq!(voices[1].name, "Samantha");
    }

    #[test]
    fn not_initialized_has_default_reason() {
        let cap = SayTtsCapability::not_initialized();
        assert!(!cap.available);
        assert!(cap.voices.is_empty());
        assert_eq!(cap.reason.as_deref(), Some("Not initialized"));
    }
}