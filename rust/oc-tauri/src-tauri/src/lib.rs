//! OpenChamber 桌面壳 (Tauri)。
//!
//! 阶段 4A (sidecar 过渡态): Tauri 进程以子进程方式拉起 `@openchamber/web`
//! CLI (`openchamber serve --foreground`), WebView 通过 loopback 加载 UI。
//! OpenCode 二进制由 web CLI 再 spawn 为孙进程, 整树清理由 sidecar 模块保证。
//!
//! 见 docs/plan/rust-migration-plan.md。

mod sidecar;

use std::sync::Mutex;

use sidecar::{SidecarBuilder, SidecarHandle};
use tauri::Manager;

/// 全局 sidecar 句柄 + 它专属的 tokio 运行时。
/// Tauri 主线程是同步的, 我们用一个独立运行时驱动 sidecar 的异步任务。
struct SidecarState {
    handle: Option<SidecarHandle>,
    rt: Option<tokio::runtime::Runtime>,
}

static SIDECAR: Mutex<Option<SidecarState>> = Mutex::new(None);

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
  tauri::Builder::default()
    .setup(|app| {
      if cfg!(debug_assertions) {
        app.handle().plugin(
          tauri_plugin_log::Builder::default()
            .level(log::LevelFilter::Info)
            .build(),
        )?;
      }

      // 启动 sidecar (仅桌面端; mobile 不 spawn 子进程)。
      #[cfg(desktop)]
      {
        let rt = tokio::runtime::Builder::new_multi_thread()
          .enable_all()
          .build()?;
        let handle = rt.block_on(async {
          SidecarBuilder::new()
            .ready_timeout(std::time::Duration::from_secs(45))
            .start()
            .await
        });
        match handle {
          Ok(h) => {
            let port = h.port();
            log::info!("sidecar ready on port {}", port);
            // 把 base_url 暴露给前端 (可选, devUrl 模式下前端已硬编码端口)。
            *SIDECAR.lock().unwrap() = Some(SidecarState {
              handle: Some(h),
              rt: Some(rt),
            });
          }
          Err(e) => {
            log::error!("sidecar startup failed: {:#}", e);
            // 不在这里 panic: 让 Tauri 继续启动, 前端会显示连接错误。
            // 用户可看到错误并重启。
            drop(rt);
          }
        }
      }

      Ok(())
    })
    .on_window_event(|window, event| {
      // 主窗口关闭时触发 sidecar 清理 (Windows/Linux 上这通常是退出时机)。
      if let tauri::WindowEvent::Destroyed = event {
        let app = window.app_handle();
        // 仅当没有其他窗口时才真正退出 (macOS 行为不同)。
        if app.webview_windows().is_empty() {
          shutdown_sidecar();
        }
      }
    })
    .build(tauri::generate_context!())
    .expect("error while building tauri application")
    .run(|_app_handle, event| {
      // RunEvent::ExitRequested / Exit: 应用退出时确保整树清理。
      if matches!(event, tauri::RunEvent::Exit) {
        shutdown_sidecar();
      }
    });
}

/// 同步清理 sidecar: 从全局状态取出句柄, 在它的运行时上 block_on kill。
fn shutdown_sidecar() {
  let mut guard = SIDECAR.lock().unwrap();
  if let Some(mut state) = guard.take() {
    if let (Some(rt), Some(mut handle)) = (state.rt.take(), state.handle.take()) {
      // 在 sidecar 自己的运行时上同步执行 kill (整树清理)。
      let _ = rt.block_on(async { handle.kill().await });
    }
    // rt drop 会回收工作线程。handle drop 会兜底 kill (Drop impl)。
    drop(state);
  }
}
