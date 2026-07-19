//! PTY 会话封装 — 基于 `portable-pty` 的 spawn / write / resize / kill / 输出读取 / 退出监听。
//!
//! 对应 Node `runtime.js` 的 PTY 部分 (`spawnTerminalPtyWithFallback`,
//! `killTerminalProcess`, `wireTerminalSession`, shell candidate 解析, env sanitize)。
//!
//! `portable-pty` 在 Unix 已通过 `setsid()` 把子进程设为 session leader,
//! 故 `process_id()` 即为 pgid, 可用 `libc::kill(-pid, sig)` 杀整组。

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use portable_pty::{native_pty_system, Child, ChildKiller, CommandBuilder, MasterPty, PtySize};
use tokio::sync::{broadcast, Mutex};
use tracing::warn;

use super::replay_buffer::ReplayBuffer;
use super::TERMINAL_OUTPUT_REPLAY_MAX_BYTES;

/// PTY 输出事件 (broadcast 载荷)。
#[derive(Clone, Debug)]
pub struct PtyOutput {
    /// PTY 输出内容。
    pub data: String,
    /// Producer replay buffer 中对应的 chunk id。
    pub replay_id: Option<u64>,
}

/// PTY 退出事件。
#[derive(Clone, Debug)]
pub struct PtyExit {
    pub exit_code: u32,
    pub signal: Option<String>,
}

/// 杀进程模式。
#[derive(Clone, Copy, Debug)]
pub enum KillMode {
    /// SIGTERM (优雅, 对齐 Node `'term'`)。
    Term,
    /// SIGKILL (强制, 对齐 Node `'kill'`)。
    Kill,
}

/// PTY 会话句柄。
///
/// 持有 master writer (写入输入)、master handle (resize)、killer (杀进程组)
/// 和 child (wait 退出)。drop 时尽力清理。
pub struct TerminalPty {
    /// master writer — 持有到会话结束 (drop 会发 EOF)。
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    /// master handle (仅用于 resize; `take_writer` 已在 spawn 时调用)。
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    /// killer (用于 kill)。
    killer: Arc<dyn ChildKiller + Send + Sync>,
    /// 子进程 PID (spawn 时从 child.process_id() 获取; 用于 Unix kill(-pid))。
    pid: Option<u32>,
    /// 输出 broadcast (reader task 发送, WS/SSE handler 接收)。
    output_tx: broadcast::Sender<PtyOutput>,
    /// 退出 broadcast (exit watcher task 发送)。
    exit_tx: broadcast::Sender<PtyExit>,
    /// 输出 replay (在任何客户端订阅前也持续记录)。
    replay_buffer: Arc<ReplayBuffer>,
    /// 最近一次退出状态 (供晚到的客户端读取)。
    exit_snapshot: Arc<StdMutex<Option<PtyExit>>>,
    /// 后端名 (对齐 Node `ptyBackend`, 当前固定 `"portable-pty"`)。
    pub backend: &'static str,
    /// 实际使用的 shell 路径。
    #[allow(dead_code)]
    pub shell: PathBuf,
    /// child (用于 wait; reader/exit task 持有 Arc clone 引用避免提前 drop)。
    /// 用 Mutex<Option> 因为 wait 需要可变借用。
    #[allow(dead_code)]
    child: Arc<Mutex<Option<Box<dyn Child + Send + Sync>>>>,
}

impl TerminalPty {
    /// spawn 一个 PTY 会话。
    ///
    /// `cwd` 必须存在且为目录。`cols`/`rows` 为 0 时回退 80×24。
    /// env 为已 sanitize 的环境 (调用方负责清理 `BASH_XTRACEFD` 等)。
    pub fn spawn(
        cwd: &Path,
        cols: u16,
        rows: u16,
        env: &HashMap<String, String>,
    ) -> anyhow::Result<Self> {
        let shell = resolve_shell()
            .ok_or_else(|| anyhow::anyhow!("No executable shell found for terminal session"))?;

        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows: rows.max(1),
            cols: cols.max(1),
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let mut cmd = CommandBuilder::new(&shell);
        cmd.cwd(cwd);
        for (k, v) in env {
            cmd.env(k, v);
        }

        let child = pair.slave.spawn_command(cmd)?;
        // slave drop 关闭从端 fd (master 仍持有引用)。
        drop(pair.slave);

        let reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        let killer = child.clone_killer();
        let pid = child.process_id();

        let (output_tx, _) = broadcast::channel(256);
        let (exit_tx, _) = broadcast::channel(8);

        let master = Arc::new(Mutex::new(pair.master));
        let writer = Arc::new(Mutex::new(writer));
        let killer: Arc<dyn ChildKiller + Send + Sync> = Arc::from(killer);
        let child = Arc::new(Mutex::new(Some(child)));

        let replay_buffer = Arc::new(ReplayBuffer::new());
        let exit_snapshot = Arc::new(StdMutex::new(None));

        // reader task: 阻塞读取 → replay + broadcast。
        let output_tx_clone = output_tx.clone();
        let child_for_reader = child.clone();
        let replay_buffer_for_reader = replay_buffer.clone();
        tokio::task::spawn_blocking(move || {
            run_reader_loop(
                reader,
                output_tx_clone,
                replay_buffer_for_reader,
                child_for_reader,
            );
        });

        // exit watcher task: 阻塞 wait → snapshot + broadcast。
        let exit_tx_clone = exit_tx.clone();
        let child_for_exit = child.clone();
        let exit_snapshot_for_watcher = exit_snapshot.clone();
        tokio::task::spawn_blocking(move || {
            run_exit_watcher(child_for_exit, exit_tx_clone, exit_snapshot_for_watcher);
        });

        Ok(Self {
            writer,
            master,
            killer,
            pid,
            output_tx,
            exit_tx,
            replay_buffer,
            exit_snapshot,
            backend: "portable-pty",
            shell,
            child,
        })
    }

    /// 写入输入到 PTY。
    pub async fn write(&self, data: &str) -> std::io::Result<()> {
        let mut w = self.writer.lock().await;
        w.write_all(data.as_bytes())?;
        w.flush()?;
        Ok(())
    }

    /// 调整窗口大小。
    pub async fn resize(&self, cols: u16, rows: u16) -> anyhow::Result<()> {
        let master = self.master.lock().await;
        master.resize(PtySize {
            rows: rows.max(1),
            cols: cols.max(1),
            pixel_width: 0,
            pixel_height: 0,
        })?;
        Ok(())
    }

    /// 杀进程组 (Unix) 或进程 (Windows)。
    ///
    /// 对齐 Node `killTerminalProcess`: Unix 先 `kill(-pgid, SIGTERM/SIGKILL)` 杀整组,
    /// 再 `ptyProcess.kill()`。Windows 直接 kill (Job Object 语义由 portable-pty 处理)。
    pub fn kill_process_group(&self, mode: KillMode) {
        #[cfg(unix)]
        {
            if let Some(pid) = self.pid {
                let sig = match mode {
                    KillMode::Term => libc::SIGTERM,
                    KillMode::Kill => libc::SIGKILL,
                };
                // SAFETY: killpg 是 POSIX 标准调用。pid 来自 portable-pty 的 child.process_id(),
                // portable-pty 在 spawn 时已 setsid() 把子进程设为 session leader, 故 pid == pgid。
                // 负 pid = 杀整个进程组。
                unsafe {
                    let _ = libc::kill(-(pid as i32), sig);
                }
            }
        }

        // 附加 kill (portable-pty 的 kill 发 SIGHUP on Unix / TerminateProcess on Windows)。
        let mut killer_clone = self.killer.clone_killer();
        let _ = killer_clone.kill();
    }

    /// 订阅 PTY 输出流。
    pub fn subscribe_output(&self) -> broadcast::Receiver<PtyOutput> {
        self.output_tx.subscribe()
    }

    /// 订阅退出事件。
    pub fn subscribe_exit(&self) -> broadcast::Receiver<PtyExit> {
        self.exit_tx.subscribe()
    }

    /// 获取 PTY 输出 replay 缓冲。
    pub fn replay_buffer(&self) -> Arc<ReplayBuffer> {
        self.replay_buffer.clone()
    }

    /// 获取最近一次退出状态，供晚到的客户端补发。
    pub fn latest_exit(&self) -> Option<PtyExit> {
        self.exit_snapshot.lock().ok().and_then(|snapshot| snapshot.clone())
    }
}

impl Drop for TerminalPty {
    fn drop(&mut self) {
        // 尽力杀进程组 (SIGKILL), 避免 shell 孤儿。
        self.kill_process_group(KillMode::Kill);
    }
}

/// reader 阻塞循环: 读 PTY → replay + broadcast 输出。
///
/// replay 由 producer 侧写入，因此客户端晚订阅时仍能拿到启动 prompt。
/// 没有 broadcast receiver 只是暂时没有实时消费者，不应停止 reader。
/// `child` 引用仅用于延长 child 生命周期 (避免 reader 仍在读时 child 被 drop)。
fn run_reader_loop(
    mut reader: Box<dyn Read + Send>,
    output_tx: broadcast::Sender<PtyOutput>,
    replay_buffer: Arc<ReplayBuffer>,
    _child: Arc<Mutex<Option<Box<dyn Child + Send + Sync>>>>,
) {
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break, // EOF
            Ok(n) => {
                let data = String::from_utf8_lossy(&buf[..n]).into_owned();
                publish_output(&data, &output_tx, &replay_buffer);
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
}

fn publish_output(
    data: &str,
    output_tx: &broadcast::Sender<PtyOutput>,
    replay_buffer: &ReplayBuffer,
) {
    let replay_id = replay_buffer
        .append(data, TERMINAL_OUTPUT_REPLAY_MAX_BYTES)
        .map(|chunk| chunk.id);
    let _ = output_tx.send(PtyOutput {
        data: data.to_string(),
        replay_id,
    });
}

/// exit watcher 阻塞循环: wait child → snapshot + broadcast 退出事件。
fn run_exit_watcher(
    child: Arc<Mutex<Option<Box<dyn Child + Send + Sync>>>>,
    exit_tx: broadcast::Sender<PtyExit>,
    exit_snapshot: Arc<StdMutex<Option<PtyExit>>>,
) {
    // 拿走 child 所有权 (wait 需要可变借用)。
    let mut child_opt = child.blocking_lock().take();
    let Some(mut child) = child_opt.take() else {
        return;
    };

    let exit = match child.wait() {
        Ok(status) => PtyExit {
            exit_code: status.exit_code(),
            signal: status.signal().map(|s| s.to_string()),
        },
        Err(e) => {
            warn!("terminal pty wait failed: {e}");
            PtyExit {
                exit_code: 1,
                signal: Some(format!("wait error: {e}")),
            }
        }
    };

    publish_exit(&exit, &exit_tx, &exit_snapshot);
    // child 已 wait 完成 (退出), drop 是 no-op。
}

fn publish_exit(
    exit: &PtyExit,
    exit_tx: &broadcast::Sender<PtyExit>,
    exit_snapshot: &StdMutex<Option<PtyExit>>,
) {
    if let Ok(mut snapshot) = exit_snapshot.lock() {
        *snapshot = Some(exit.clone());
    }
    let _ = exit_tx.send(exit.clone());
}

// =========================================================================
// Shell candidate 解析 (对齐 Node `getTerminalShellCandidates`)
// =========================================================================

/// 解析可用 shell 路径, 按优先级返回第一个可执行的。
fn resolve_shell() -> Option<PathBuf> {
    shell_candidates().into_iter().find(|c| is_executable(c))
}

/// 返回 shell 候选列表 (按优先级)。
fn shell_candidates() -> Vec<PathBuf> {
    #[cfg(unix)]
    {
        let mut candidates: Vec<PathBuf> = Vec::new();
        let mut push = |c: PathBuf| {
            if !candidates.contains(&c) {
                candidates.push(c);
            }
        };

        if let Ok(s) = std::env::var("OPENCHAMBER_TERMINAL_SHELL") {
            let s = s.trim();
            if !s.is_empty() {
                push(PathBuf::from(s));
            }
        }
        if let Ok(s) = std::env::var("SHELL") {
            let s = s.trim();
            if !s.is_empty() {
                push(PathBuf::from(s));
            }
        }
        push(PathBuf::from("/bin/zsh"));
        push(PathBuf::from("/bin/bash"));
        push(PathBuf::from("/bin/sh"));
        // PATH 查找的裸名
        for name in &["zsh", "bash", "sh"] {
            if let Some(p) = find_on_path(name) {
                push(p);
            }
        }
        candidates
    }
    #[cfg(windows)]
    {
        let mut candidates: Vec<PathBuf> = Vec::new();
        let mut push = |c: PathBuf| {
            if !candidates.contains(&c) {
                candidates.push(c);
            }
        };
        if let Ok(s) = std::env::var("OPENCHAMBER_TERMINAL_SHELL") {
            let s = s.trim();
            if !s.is_empty() {
                push(PathBuf::from(s));
            }
        }
        if let Ok(s) = std::env::var("SHELL") {
            let s = s.trim();
            if !s.is_empty() {
                push(PathBuf::from(s));
            }
        }
        if let Ok(s) = std::env::var("ComSpec") {
            push(PathBuf::from(s));
        }
        let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());
        push(PathBuf::from(system_root)
            .join("System32")
            .join("WindowsPowerShell")
            .join("v1.0")
            .join("powershell.exe"));
        push(PathBuf::from("pwsh.exe"));
        push(PathBuf::from("powershell.exe"));
        push(PathBuf::from("cmd.exe"));
        candidates
    }
}

/// 在 `$PATH` 中查找可执行文件 (对齐 Node `searchPathFor`)。
fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var("PATH").ok()?;
    let separator = if cfg!(windows) { ';' } else { ':' };
    for dir in path.split(separator) {
        if dir.is_empty() {
            continue;
        }
        let candidate = Path::new(dir).join(name);
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// 判断路径是否为可执行文件 (对齐 Node `isExecutable`)。
fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match std::fs::metadata(path) {
            Ok(meta) => {
                if !meta.is_file() {
                    return false;
                }
                meta.permissions().mode() & 0o111 != 0
            }
            Err(_) => false,
        }
    }
    #[cfg(windows)]
    {
        match std::fs::metadata(path) {
            Ok(meta) => {
                if !meta.is_file() {
                    return false;
                }
                let ext = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.to_lowercase())
                    .unwrap_or_default();
                ext.is_empty() || matches!(ext.as_str(), "exe" | "cmd" | "bat" | "com")
            }
            Err(_) => false,
        }
    }
}

// =========================================================================
// 环境 sanitize + locale 回退 (对齐 Node `runtime.js`)
// =========================================================================

/// 从进程环境构建 sanitize 后的 PTY 环境。
///
/// 删除 `BASH_XTRACEFD`/`BASH_ENV`/`ENV` (对齐 Node `sanitizeTerminalEnv`)。
/// 注入 `TERM`/`COLORTERM`/`LANG`/`LC_CTYPE` 回退。
pub fn build_pty_env(cols: u16, rows: u16) -> HashMap<String, String> {
    let mut env: HashMap<String, String> = std::env::vars().collect();
    env.remove("BASH_XTRACEFD");
    env.remove("BASH_ENV");
    env.remove("ENV");

    env.insert("TERM".into(), "xterm-256color".into());
    env.insert("COLORTERM".into(), "truecolor".into());

    let (lang_fallback, lc_ctype_fallback) = if cfg!(target_os = "macos") {
        ("en_US.UTF-8", "UTF-8")
    } else {
        ("C.UTF-8", "C.UTF-8")
    };

    if !env.contains_key("LANG") {
        env.insert("LANG".into(), lang_fallback.into());
    }
    if !env.contains_key("LC_CTYPE") {
        env.insert("LC_CTYPE".into(), lc_ctype_fallback.into());
    }

    // cols/rows 仅用于日志; PTY 尺寸由 openpty() 设置, 不放入 env。
    let _ = (cols, rows);

    env
}

/// 默认空闲超时 (对齐 Node `TERMINAL_IDLE_TIMEOUT = 30min`)。
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// idle sweep 间隔 (对齐 Node `5min`)。
pub const IDLE_SWEEP_INTERVAL: Duration = Duration::from_secs(5 * 60);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::replay_buffer::ReplayBuffer;

    #[test]
    fn publishes_output_to_replay_without_subscribers() {
        let (output_tx, _) = broadcast::channel(4);
        let replay_buffer = ReplayBuffer::new();

        publish_output("prompt$ ", &output_tx, &replay_buffer);

        let chunks = replay_buffer.list_since(0);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].data, "prompt$ ");
    }

    #[test]
    fn continues_publishing_after_subscriber_attaches_late() {
        let (output_tx, _) = broadcast::channel(4);
        let replay_buffer = ReplayBuffer::new();

        publish_output("prompt$ ", &output_tx, &replay_buffer);
        let mut receiver = output_tx.subscribe();
        publish_output("echo ok\\r\\n", &output_tx, &replay_buffer);

        let output = receiver.try_recv().expect("late subscriber receives live output");
        assert_eq!(output.data, "echo ok\\r\\n");
        assert_eq!(replay_buffer.list_since(0).len(), 2);
    }

    #[test]
    fn caches_exit_without_subscribers() {
        let (exit_tx, _) = broadcast::channel(1);
        let exit_snapshot = StdMutex::new(None);
        let exit = PtyExit {
            exit_code: 130,
            signal: Some("SIGINT".to_string()),
        };

        publish_exit(&exit, &exit_tx, &exit_snapshot);

        assert_eq!(exit_snapshot.lock().unwrap().as_ref().map(|value| value.exit_code), Some(130));
        assert_eq!(exit_snapshot.lock().unwrap().as_ref().and_then(|value| value.signal.clone()).as_deref(), Some("SIGINT"));
    }
}
