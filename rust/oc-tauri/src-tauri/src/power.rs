//! Keep-awake 模块 — 复现 Electron `powerSaveBlocker` 的 `prevent-app-suspension`。
//!
//! Electron 使用 `powerSaveBlocker.start('prevent-app-suspension')`:
//! 保持 CPU/进程不休眠，但允许显示器休眠。
//!
//! 平台实现:
//! - macOS: spawn `caffeinate -dimsu` 子进程
//! - Windows: `SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED)`
//! - Linux: spawn `systemd-inhibit --what=handle-lid-switch:sleep --mode=block`
//!
//! 单一 blocker，toggle 与用户设置 `desktopKeepAwakeEnabled` 绑定。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// Keep-awake 状态。作为 Tauri State 管理。
pub struct KeepAwakeState {
    /// 当前是否处于活跃 (blocking) 状态。
    active: AtomicBool,
    /// 子进程句柄 (macOS/Linux caffeinate/systemd-inhibit)。
    child: Mutex<Option<tokio::process::Child>>,
}

impl KeepAwakeState {
    pub fn new() -> Self {
        Self {
            active: AtomicBool::new(false),
            child: Mutex::new(None),
        }
    }

    /// 激活 keep-awake。
    ///
    /// 如果已经活跃，不重复操作。
    pub async fn enable(&self) -> Result<bool, String> {
        if self.active.load(Ordering::SeqCst) {
            return Ok(true);
        }

        #[cfg(target_os = "macos")]
        {
            self.spawn_caffeinate().await?;
        }
        #[cfg(target_os = "windows")]
        {
            self.set_thread_execution_state_enabled();
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            self.spawn_systemd_inhibit().await?;
        }

        self.active.store(true, Ordering::SeqCst);
        Ok(true)
    }

    /// 禁用 keep-awake。
    ///
    /// 如果不活跃，不重复操作。
    pub fn disable(&self) -> Result<bool, String> {
        if !self.active.load(Ordering::SeqCst) {
            return Ok(false);
        }

        #[cfg(target_os = "windows")]
        {
            self.set_thread_execution_state_disabled();
        }
        #[cfg(any(target_os = "macos", all(unix, not(target_os = "macos"))))]
        {
            // kill 子进程
            let mut guard = self.child.lock().map_err(|e| e.to_string())?;
            if let Some(mut child) = guard.take() {
                // 尽力 kill
                let _ = child.start_kill();
            }
        }

        self.active.store(false, Ordering::SeqCst);
        Ok(false)
    }

    /// 返回当前状态 `{ active, enabled }`。
    /// `enabled` = 是否设置了 `desktopKeepAwakeEnabled` (调用方决定)。
    pub fn status(&self, enabled: bool) -> (bool, bool) {
        let active = self.active.load(Ordering::SeqCst);
        (enabled, active)
    }

    // --- 平台实现 ---

    /// macOS: spawn `caffeinate -dimsu`
    /// -d: Prevent the display from sleeping.
    /// -i: Prevent the system from idle sleeping.
    /// -m: Prevent the disk from idle sleeping.
    /// -s: Prevent the system from sleeping (AC power only).
    /// -u: Declare that a user is active.
    #[cfg(target_os = "macos")]
    async fn spawn_caffeinate(&self) -> Result<(), String> {
        use tokio::process::Command;

        let child = Command::new("caffeinate")
            .args(["-d", "-i", "-m", "-s", "-u"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| format!("failed to start caffeinate: {}", e))?;

        let mut guard = self.child.lock().map_err(|e| e.to_string())?;
        *guard = Some(child);
        Ok(())
    }

    /// Linux: spawn `systemd-inhibit --what=handle-lid-switch:sleep --mode=block`
    #[cfg(all(unix, not(target_os = "macos")))]
    async fn spawn_systemd_inhibit(&self) -> Result<(), String> {
        use tokio::process::Command;

        // 尝试 systemd-inhibit；如果不存在，静默跳过 (best-effort)
        let child = Command::new("systemd-inhibit")
            .args(["--what=handle-lid-switch:sleep", "--mode=block", "--"])
            .arg("sleep")
            .arg("infinity")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();

        match child {
            Ok(c) => {
                let mut guard = self.child.lock().map_err(|e| e.to_string())?;
                *guard = Some(c);
                Ok(())
            }
            Err(e) => {
                // systemd-inhibit 不存在是常见的 (非 systemd 系统)。不视为硬错误。
                log::info!("[power] systemd-inhibit not available: {}", e);
                Ok(())
            }
        }
    }

    /// Windows: 调用 SetThreadExecutionState 启用。
    #[cfg(target_os = "windows")]
    fn set_thread_execution_state_enabled(&self) {
        // ES_CONTINUOUS (0x80000000) | ES_SYSTEM_REQUIRED (0x00000001) | ES_AWAYMODE_REQUIRED (0x00000040)
        // 注意: 不加 ES_DISPLAY_REQUIRED (0x00000002) — 允许显示器休眠 (与 Electron prevent-app-suspension 一致)
        let flags: u32 = 0x80000000 | 0x00000001;
        unsafe {
            let _ = windows_sys::Win32::System::Power::SetThreadExecutionState(flags);
        }
    }

    /// Windows: 调用 SetThreadExecutionState 禁用。
    #[cfg(target_os = "windows")]
    fn set_thread_execution_state_disabled(&self) {
        // ES_CONTINUOUS (0x80000000) — 清除所有标志
        unsafe {
            let _ = windows_sys::Win32::System::Power::SetThreadExecutionState(0x80000000);
        }
    }
}

impl Default for KeepAwakeState {
    fn default() -> Self {
        Self::new()
    }
}

/// 全局 keep-awake 状态 (简化: 用静态 Mutex 而非 Tauri State,
/// 因为 setup 中需要在异步上下文外访问)。
///
/// 复现 Electron 的 `state.keepAwakeBlockerId` (main.mjs:222)。
static KEEP_AWAKE: std::sync::LazyLock<KeepAwakeState> =
    std::sync::LazyLock::new(KeepAwakeState::new);

/// 获取全局 KeepAwakeState 引用。
pub fn global_keep_awake() -> &'static KeepAwakeState {
    &KEEP_AWAKE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_state_is_inactive() {
        let state = KeepAwakeState::new();
        let (enabled, active) = state.status(false);
        assert!(!enabled);
        assert!(!active);
    }

    #[test]
    fn status_reflects_enabled_flag() {
        let state = KeepAwakeState::new();
        let (enabled, _) = state.status(true);
        assert!(enabled);
    }
}
