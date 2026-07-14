//! `oc-tauri` — OpenChamber 桌面壳骨架 (阶段 0 占位)。
//!
//! 替换目标: `packages/electron/` (main.mjs ~4900 行 + preload.mjs + 托盘/SSH/更新器)。
//!
//! 阶段 0: 仅占位, 验证 workspace 共存。当前只打印一段说明后退出。
//! 阶段 4 会:
//!   - 通过 tauri-cli 初始化完整 Tauri 应用 (tauri.conf.json + webview)
//!   - 同进程内嵌 oc-server (axum), WebView 加载 loopback
//!   - 迁移 Electron 能力: 窗口/托盘/菜单/深链/自动更新/SSH
//!   - preload 桥对等 (保持 window.__OPENCHAMBER_DESKTOP__ IPC 契约)

fn main() -> anyhow::Result<()> {
    println!("oc-tauri 桌面壳 (阶段 0 占位) — 见 docs/plan/rust-migration-plan.md 阶段 4");
    println!("当前尚未初始化 Tauri 运行时。阶段 4 会通过 tauri-cli 接入。");
    Ok(())
}
