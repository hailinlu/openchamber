# Phase 4B: oc-server 进程内嵌入 — 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把 oc-server 从 Tauri sidecar 子进程改为进程内嵌入，同时保留 sidecar 作为环境变量门控的回退路径。

**Architecture:** 新增 `oc-server/src/lib.rs`，把 `main.rs` 的启动编排（配置→OpenCode spawn→AppState+模块 init→路由→axum::serve→优雅关闭）提取为 `OcServer` 句柄。Tauri `lib.rs` 在 setup 里按 `OPENCHAMBER_SIDECAR` env 分叉：默认走进程内嵌入，env=truthy 走旧 sidecar 路径。`backend.rs` 用 `BackendHandle` 枚举统一两路径的 `base_url()`/`shutdown()`。

**Tech Stack:** Rust, axum, tokio (oneshot + multi-thread runtime), Tauri 2.x。

**Spec:** `docs/superpowers/specs/2026-07-16-phase-4b-in-process-embedding-design.md`

## Global Constraints

- **不引入新依赖**：`oneshot` 来自 tokio 已有 feature；`BackendHandle` 用 std enum。
- **不动 relay 桩** / Node 后端 / IPC 契约 / 原生集成层（托盘、菜单、深链、vibrancy、power、updater、ssh、mini_chat）。
- **保持绿色**：每任务结束 `cargo check -p oc-server` 或 `cargo check -p oc-tauri` 必须通过。
- **测试串行**：oc-server 测试惯例 `--test-threads=1`（env 测试隔离）。
- **Runtime 策略**：进程内嵌入用专属 multi-thread tokio Runtime（存 `BackendState.rt`），不混用 Tauri `async_runtime`，避免 shutdown 死锁。
- **关闭顺序固定**：axum graceful shutdown → `global_hub.stop()` → `terminal_sessions.kill_all()` → `oc_handle.shutdown()`。不可漂移。
- **关键类型签名**（来自现有代码，勿改）：
  - `opencode::start(config: &Config) -> Result<(String, String, OpenCodeHandle)>`，返回 `(base_url, auth_header, handle)`
  - `OpenCodeHandle::shutdown(&mut self)` (async)
  - `Config::for_tests()` 当前 `opencode_skip_start: true` 但无 host/port → `start_external()` 会返回 Err（见 Task 1 修复）
  - 模块声明当前在 `main.rs` L22-54（私有 `mod`），lib 化后移入 `lib.rs` 为 `pub mod`

---

## File Structure

| 文件 | 责任 | 创建/修改 |
|---|---|---|
| `rust/oc-server/src/lib.rs` | `OcServer` 句柄 + `start()`/`shutdown()` 编排；`pub mod` 声明所有模块 | **创建** |
| `rust/oc-server/src/main.rs` | 瘦壳 bin：init tracing → load config → OcServer::start → ctrl_c → shutdown | 修改 |
| `rust/oc-server/src/config.rs` | `Config::for_tests()` 补 `opencode_port` 使测试能跳过 opencode | 修改（仅 `#[cfg(test)]`） |
| `rust/oc-server/Cargo.toml` | 加 `[lib]` 段 | 修改 |
| `rust/oc-tauri/src-tauri/src/backend.rs` | `BackendHandle` 枚举 + `use_sidecar()` + `parse_port()` + 测试 | **创建** |
| `rust/oc-tauri/src-tauri/src/lib.rs` | setup 分叉、`BackendState` 改名、`shutdown_backend` | 修改 |
| `rust/oc-tauri/src-tauri/Cargo.toml` | 加 `oc-server` 依赖 | 修改 |

**依赖图（无环）**：`oc-tauri → oc-server → oc-core + oc-opencode-sdk`。

---

## Task 1: 修复 `Config::for_tests()` 以支持 oc-server 独立启动

**为什么先做**：`OcServer::start()` 会调用 `opencode::start()`。当前 `Config::for_tests()` 设 `opencode_skip_start: true` 但未给 host/port，导致 `start_external()` 返回 `Err("external opencode mode requires OPENCODE_HOST or OPENCODE_PORT")`。必须先修复，否则 Task 2 的 lib 测试无法通过。

**Files:**
- Modify: `rust/oc-server/src/config.rs:111-131`（`#[cfg(test)] impl Config` 的 `for_tests()`）

**Interfaces:**
- Produces: `Config::for_tests()` 返回的实例经 `opencode::start()` 走 `start_external` 分支时返回 `Ok(...)` 而非 Err。

- [ ] **Step 1: 修改 `for_tests()` 加 `opencode_port: Some(1)`**

把 `rust/oc-server/src/config.rs` 中 `for_tests()` 的 `opencode_port` 字段从 `None` 改为 `Some(1)`。端口 1 是特权端口，几乎不会有真实 opencode 监听，`start_external` 的 health check 会 warn 但不阻塞启动（L276: `tracing::warn!(... "continuing anyway")`）。

在 `for_tests()` 内定位这行：
```rust
            opencode_port: None,
```
改为：
```rust
            opencode_port: Some(1),
```

- [ ] **Step 2: 验证 `cargo check -p oc-server` 通过**

Run: `cargo check -p oc-server`
Expected: 编译通过（可能已有 dead-code warning，忽略）。

- [ ] **Step 3: 提交**

```bash
git add rust/oc-server/src/config.rs
git commit -m "fix(rust): Config::for_tests() 补 opencode_port 使 start_external 不报错

为 Phase 4B lib 化准备: OcServer::start() 会调 opencode::start(),
for_tests() 需能走 external 分支返回 Ok 而非 Err。"
```

---

## Task 2: 创建 `oc-server/src/lib.rs` — `OcServer` 句柄

这是 Phase 4B 核心。把 `main.rs` 的启动编排提取为 `OcServer` 句柄，`main.rs` 变薄壳。

**Files:**
- Create: `rust/oc-server/src/lib.rs`
- Modify: `rust/oc-server/src/main.rs`（全量重写为薄壳）
- Modify: `rust/oc-server/Cargo.toml`（加 `[lib]` 段）

**Interfaces:**
- Consumes: `Config`（`config.rs`），`AppState`（`state.rs`），`opencode::{start, OpenCodeHandle}`，`bind_host::enforce`，`build_router`（现 `main.rs` 的私有 fn，移入 lib）。现有所有模块。
- Produces:
  - `pub struct OcServer` with `pub async fn start(config: Config) -> Result<Self>`, `pub fn base_url(&self) -> &str`, `pub async fn shutdown(self)`
  - `pub use config::Config;`（re-export，供 `oc_server::Config` 访问）
  - 所有模块变为 `pub mod`（lib 暴露给 oc-tauri 与 bin）

- [ ] **Step 1: 创建 `rust/oc-server/src/lib.rs`**

完整内容（模块声明从 main.rs L22-54 移来，改 `pub mod`；启动编排从 main.rs `main()` L67-158 移来；`build_router` + `shutdown_signal` 从 main.rs L162-551 移来；新增 `OcServer` 句柄）：

```rust
//! `oc-server` 库入口 — 进程内嵌入句柄。
//!
//! Phase 4B: 把原 main.rs 的启动编排提取为 `OcServer` 句柄,
//! 供 Tauri 进程内嵌入 与 oc-server bin 共用同一条启动/关闭路径。

// --- 模块声明 (从 main.rs 移入, 改 pub mod 供 lib 消费者访问) ---
pub mod bind_host;
pub mod client_auth;
pub mod config;
pub mod error;
pub mod fs;
pub mod git;
pub mod github;
pub mod magic_prompts;
pub mod middleware;
pub mod notifications;
pub mod opencode;
pub mod permission_auto_accept;
pub mod project_dir;
pub mod proxy;
pub mod quota;
pub mod realtime;
pub mod routes;
pub mod scheduled_tasks;
pub mod session_assist;
pub mod session_folders;
pub mod session_goal;
pub mod skills_catalog;
pub mod small_model;
pub mod state;
pub mod static_files;
pub mod terminal;
pub mod preview;
pub mod dictation;
pub mod relay;
pub mod text;
pub mod tts;
pub mod tunnels;
pub mod ui_auth;

// --- 便捷 re-export ---
pub use config::Config;

use std::sync::Arc;

use anyhow::Context;
use axum::Router;
use tokio::sync::oneshot;

use opencode::OpenCodeHandle;

// =========================================================================
// OcServer 句柄
// =========================================================================

/// 进程内嵌入的 oc-server 句柄。
///
/// 封装完整的启动编排 (配置 → OpenCode spawn → AppState + 模块 init → 路由 →
/// axum::serve) 与优雅关闭顺序 (axum graceful → hub.stop → terminal.kill_all →
/// opencode.shutdown)。
///
/// 用法:
/// ```ignore
/// let server = OcServer::start(config).await?;
/// let url = server.base_url();
/// // ... serve 在后台 task 运行 ...
/// server.shutdown().await;
/// ```
pub struct OcServer {
    base_url: String,
    shutdown_tx: Option<oneshot::Sender<()>>,
    join_handle: Option<tokio::task::JoinHandle<()>>,
    state: Arc<state::AppState>,
    oc_handle: Option<OpenCodeHandle>,
}

impl OcServer {
    /// 启动后端: 配置 → bind_host 检查 → OpenCode spawn → AppState + 7 步模块
    /// init → build_router → axum::serve (后台 task)。
    ///
    /// 端口绑定 `config.host:config.port` (port=0 时 OS 分配)。
    /// 返回的句柄含实际 base_url; serve 在 spawned task 内运行, 不阻塞调用方。
    pub async fn start(config: Config) -> anyhow::Result<Self> {
        // 1. 绑定安全检查 (拒绝未认证 LAN)
        bind_host::enforce(&config)?;

        // 2. 启动 OpenCode (managed spawn 或 external attach)
        let (oc_base_url, oc_auth, oc_handle) = opencode::start(&config)
            .await
            .context("failed to start opencode")?;

        // 3. 构建 AppState
        let state = Arc::new(state::AppState::new(
            config.clone(),
            oc_base_url,
            oc_auth,
        ));
        state.set_opencode_ready(true);

        // 3b-3i. 模块 init (与原 main.rs 顺序一致)
        state.init_notification_trigger();
        state.init_permission_auto_accept();
        state.init_session_assist();
        state.init_session_goal();
        state.init_say_tts_capability().await;
        state.init_scheduled_tasks();
        state.terminal_sessions.clone().start_idle_sweep();
        state.preview_targets.clone().start_sweeper();

        // 3j. 初始化私有中继 (host-lock + 生命周期)
        let relay_service = Arc::new(relay::service::RelayService::new());
        relay_service.attach_self_weak();
        state.install_relay_service(relay_service);
        let state_clone = state.clone();
        tokio::spawn(async move {
            let svc = state_clone
                .relay_service
                .lock()
                .expect("relay_service poisoned")
                .clone();
            if let Some(svc) = svc {
                svc.start_if_enabled().await;
            }
        });

        // 4. 构建路由
        let app = build_router(state.clone(), &config);

        // 5. 绑定 listener (port=0 时 OS 分配)
        let listener = tokio::net::TcpListener::bind((config.host, config.port))
            .await
            .context("bind oc-server listener")?;
        let local_addr = listener.local_addr().context("get listener local addr")?;
        let base_url = format!("http://{}", local_addr);

        tracing::info!(
            addr = %local_addr,
            "oc-server listening (runtime=rust, version={})",
            state.version
        );

        // 6. axum::serve 作为 spawned task (不阻塞调用方)
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let join_handle = tokio::spawn(async move {
            let serve = axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                });
            if let Err(e) = serve.await {
                tracing::error!("oc-server serve task exited with error: {:#}", e);
            }
        });

        Ok(Self {
            base_url,
            shutdown_tx: Some(shutdown_tx),
            join_handle: Some(join_handle),
            state,
            oc_handle: Some(oc_handle),
        })
    }

    /// `http://<host>:<port>` — 供 WebView 和 IPC 命令使用。
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// 优雅关闭 (顺序与原 main.rs 一致, 不可漂移):
    /// 1. 触发 axum graceful shutdown (shutdown_tx)
    /// 2. 等 serve task 退出 (join_handle)
    /// 3. global_hub.stop() (停上游 SSE reader)
    /// 4. terminal_sessions.kill_all() (杀终端会话)
    /// 5. oc_handle.shutdown() (杀 OpenCode 子进程)
    pub async fn shutdown(mut self) {
        // 1. 触发 axum graceful shutdown
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        // 2. 等 serve task 退出
        if let Some(join) = self.join_handle.take() {
            let _ = join.await;
        }
        // 3. 关闭全局 hub (停止上游 SSE reader)
        tracing::info!("shutting down global event hub");
        self.state.global_hub.stop().await;
        // 4. 杀所有终端会话 (对齐 Node `shutdown`)
        self.state.terminal_sessions.kill_all().await;
        // 5. 关闭 OpenCode 子进程
        if let Some(mut h) = self.oc_handle.take() {
            tracing::info!("shutting down opencode process");
            h.shutdown().await;
        }
        tracing::info!("oc-server stopped");
    }
}

// =========================================================================
// 路由构建 (从 main.rs 移入)
// =========================================================================

/// 构建完整路由树。从原 main.rs build_router 原样移入。
fn build_router(state: Arc<state::AppState>, config: &Config) -> Router {
    // NOTE: 把原 main.rs 的 build_router 函数体 (L162-521) 完整复制到这里。
    // 它引用的 use 项 (axum::routing::{any,delete,get,post,put}) 需在 lib.rs 顶部导入。
    // 函数体保持不变, 仅从 main.rs 迁移到 lib.rs。
    todo_build_router_body(state, config)
}

// 实施者注: build_router 函数体约 360 行, 从 main.rs L166-521 原样复制。
// 为避免计划文档臃肿, 这里用占位说明: 复制 main.rs 的 build_router 整个函数
// (含所有 .route() 注册 + middleware + static_files 分支), 替换 crate:: 前缀
// 为对应的 pub mod 路径 (因 lib.rs 已 pub mod, crate:: 前缀仍然有效, 无需改)。
```

> **⚠ 实施者注意**：上面 `build_router` 的 `todo_build_router_body` 是占位。实际实施时，把 `main.rs` 中现有的 `fn build_router(state: Arc<AppState>, config: &Config) -> Router { ... }`（L162-521）**完整函数体**复制到 `lib.rs` 的 `fn build_router` 内。同时把 `main.rs` 顶部的 `use axum::routing::{any, delete, get, post, put};` 也移到 `lib.rs`。函数体内的 `crate::xxx` 前缀无需修改（lib 内 `crate::` 仍解析到 lib 根的 `pub mod xxx`）。

- [ ] **Step 2: 重写 `rust/oc-server/src/main.rs` 为薄壳**

全量替换 `main.rs` 内容为：

```rust
//! `oc-server` — OpenChamber Rust 后端二进制入口。
//!
//! Phase 4B: 瘦壳, 启动编排委托给 `oc_server::OcServer`。
//! 替换目标: `packages/web/server/index.js` (Express)。

use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config = oc_server::Config::load()?;
    let server = oc_server::OcServer::start(config).await?;

    // 就绪行 (供 sidecar/Tauri 解析, 同 Node 的 ready 行)
    println!("openchamber server listening on {}", server.base_url());

    // 等待 Ctrl-C / SIGTERM
    tokio::signal::ctrl_c()
        .await
        .expect("failed to install Ctrl+C handler");

    server.shutdown().await;
    Ok(())
}
```

- [ ] **Step 3: 修改 `rust/oc-server/Cargo.toml` 加 `[lib]` 段**

在 `[[bin]]` 段之前插入：

```toml
[lib]
name = "oc_server"
path = "src/lib.rs"

[[bin]]
name = "oc-server"
path = "src/main.rs"
```

- [ ] **Step 4: 运行 `cargo check -p oc-server` 验证编译**

Run: `cargo check -p oc-server 2>&1 | tail -20`
Expected: 编译通过。可能有 dead-code warning（lib 新增的 pub 项未被 bin 用），忽略。若报 `unresolved import` 或 `private module` 错误，检查 lib.rs 的 `pub mod` 声明是否完整覆盖原 main.rs L22-54 的所有模块。

- [ ] **Step 5: 运行现有测试确保无回归**

Run: `cargo test -p oc-server -- --test-threads=1 2>&1 | tail -5`
Expected: 现有 1120 测试全过（模块测试不受 lib/bin 拆分影响）。

- [ ] **Step 6: 提交**

```bash
git add rust/oc-server/src/lib.rs rust/oc-server/src/main.rs rust/oc-server/Cargo.toml
git commit -m "feat(rust): Phase 4B — OcServer 句柄 lib 化

- 新增 lib.rs: OcServer { start, base_url, shutdown } 封装启动+关闭编排
- main.rs 瘦壳化 (~550 行 → ~30 行)
- Cargo.toml 加 [lib] 段 (name=oc_server)
- 模块声明从 main.rs 移入 lib.rs 为 pub mod"
```

---

## Task 3: `OcServer` lib 单元测试

验证 `OcServer` 基本契约：start 后 /health 可达，shutdown 后端口释放。

**Files:**
- Modify: `rust/oc-server/src/lib.rs`（在文件末尾加 `#[cfg(test)] mod tests`）

**Interfaces:**
- Consumes: `OcServer::start`, `OcServer::base_url`, `OcServer::shutdown`（Task 2 产出），`Config::for_tests()`（Task 1 修复后）

- [ ] **Step 1: 在 `lib.rs` 末尾追加测试模块**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// helper: 从 "http://127.0.0.1:<port>" 提取端口号。
    fn parse_port(base_url: &str) -> u16 {
        base_url
            .rsplit(':')
            .next()
            .and_then(|s| s.parse().ok())
            .expect("base_url should contain port")
    }

    #[tokio::test]
    async fn oc_server_starts_and_serves_health() {
        let config = Config::for_tests();
        let server = OcServer::start(config).await.expect("OcServer::start");

        let resp = reqwest::get(format!("{}/health", server.base_url()))
            .await
            .expect("GET /health");
        assert!(
            resp.status().is_success(),
            "/health should return 2xx, got {}",
            resp.status()
        );

        server.shutdown().await;
    }

    #[tokio::test]
    async fn oc_server_shutdown_releases_port() {
        let config = Config::for_tests();
        let server = OcServer::start(config).await.expect("OcServer::start");
        let port = parse_port(server.base_url());

        server.shutdown().await;

        // shutdown 后端口应可重新绑定 (证明 listener 已释放)
        let rebind = tokio::net::TcpListener::bind(("127.0.0.1", port)).await;
        assert!(
            rebind.is_ok(),
            "port {} should be free after shutdown",
            port
        );
    }

    #[test]
    fn base_url_format() {
        // OcServer::start 需要 tokio context, 这里只验证 base_url 字符串形态
        // 用 oneshot 构造一个不完整句柄的等价字符串测试。
        let sample = "http://127.0.0.1:12345";
        assert!(sample.starts_with("http://"));
        assert!(parse_port(sample) == 12345);
    }
}
```

- [ ] **Step 2: 确认 `reqwest` 在 dev-dependencies 中**

Run: `grep -n "reqwest" rust/oc-server/Cargo.toml`
Expected: 已有 `reqwest`（sidecar.rs 测试也用）。若 dev-dependencies 没有，在 `[dev-dependencies]` 加 `reqwest = { workspace = true }`。

检查 workspace 是否有 reqwest：
Run: `grep -n "reqwest" rust/Cargo.toml`
若 workspace 有则用 `{ workspace = true }`，否则用 `reqwest = "0.12"`。

- [ ] **Step 3: 运行 lib 测试**

Run: `cargo test -p oc-server --lib -- --test-threads=1 2>&1 | tail -15`
Expected: 3 个测试全过。若 `oc_server_starts_and_serves_health` 失败，检查 `Config::for_tests()` 的 `opencode_port` 是否已改（Task 1）。

- [ ] **Step 4: 运行全量测试确保无回归**

Run: `cargo test -p oc-server -- --test-threads=1 2>&1 | tail -5`
Expected: 1120 + 3 = 1123 测试全过。

- [ ] **Step 5: 提交**

```bash
git add rust/oc-server/src/lib.rs rust/oc-server/Cargo.toml
git commit -m "test(rust): OcServer lib 单元测试 (start/health/shutdown)"
```

---

## Task 4: 创建 `oc-tauri/src-tauri/src/backend.rs`

统一后端句柄枚举 + sidecar 决策函数。

**Files:**
- Create: `rust/oc-tauri/src-tauri/src/backend.rs`
- Modify: `rust/oc-tauri/src-tauri/Cargo.toml`（加 oc-server 依赖）

**Interfaces:**
- Consumes: `sidecar::SidecarHandle`（现有），`oc_server::OcServer`（Task 2 产出）
- Produces:
  - `pub enum BackendHandle { InProcess(oc_server::OcServer), Sidecar(sidecar::SidecarHandle) }`
  - `impl BackendHandle { pub fn base_url(&self) -> String; pub async fn shutdown(self); }`
  - `pub fn use_sidecar() -> bool`
  - `pub fn parse_port(base_url: &str) -> u16`

- [ ] **Step 1: 在 `rust/oc-tauri/src-tauri/Cargo.toml` 加 oc-server 依赖**

在 `[dependencies]` 段（`oc-core = { workspace = true }` 附近）加：

```toml
oc-server = { workspace = true }
```

- [ ] **Step 2: 创建 `rust/oc-tauri/src-tauri/src/backend.rs`**

```rust
//! 统一后端句柄: 进程内嵌入 (默认) 或 sidecar 回退。
//!
//! Phase 4B: Tauri setup 按 `OPENCHAMBER_SIDECAR` env 选择后端路径。
//! 两条路径通过 `BackendHandle` 枚举暴露统一的 `base_url()` + `shutdown()`。

use oc_server::OcServer;
use sidecar::SidecarHandle;

/// 后端句柄 — 封装进程内嵌入或 sidecar 两种实现。
pub enum BackendHandle {
    /// 进程内嵌入的 oc-server (默认路径)。
    InProcess(OcServer),
    /// sidecar 子进程 (OPENCHAMBER_SIDECAR=1 回退路径)。
    Sidecar(SidecarHandle),
}

impl BackendHandle {
    /// 返回 `http://127.0.0.1:<port>`, 供 WebView 加载和 IPC 命令使用。
    pub fn base_url(&self) -> String {
        match self {
            Self::InProcess(s) => s.base_url().to_string(),
            Self::Sidecar(h) => h.base_url(),
        }
    }

    /// 优雅关闭 — 按各自路径执行 shutdown/kill。
    pub async fn shutdown(self) {
        match self {
            Self::InProcess(s) => s.shutdown().await,
            Self::Sidecar(mut h) => {
                let _ = h.kill().await;
            }
        }
    }
}

/// 决策: 是否走 sidecar 回退路径。
///
/// 环境变量 `OPENCHAMBER_SIDECAR` 为 `1`/`true`/`TRUE` 时返回 true,
/// 未设置或其他值返回 false (默认进程内嵌入)。
pub fn use_sidecar() -> bool {
    std::env::var("OPENCHAMBER_SIDECAR")
        .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE"))
        .unwrap_or(false)
}

/// 从 `http://host:port` 提取端口号。
pub fn parse_port(base_url: &str) -> u16 {
    base_url
        .rsplit(':')
        .next()
        .and_then(|s| s.parse().ok())
        .expect("base_url should contain port")
}

// =========================================================================
// 测试
// =========================================================================
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn use_sidecar_defaults_false() {
        // 注意: env 测试需串行 (--test-threads=1 已是 oc-tauri 惯例)。
        std::env::remove_var("OPENCHAMBER_SIDECAR");
        assert!(!use_sidecar());
    }

    #[test]
    fn use_sidecar_truthy_values() {
        for v in ["1", "true", "TRUE"] {
            std::env::set_var("OPENCHAMBER_SIDECAR", v);
            assert!(use_sidecar(), "value {} should be truthy", v);
        }
        std::env::remove_var("OPENCHAMBER_SIDECAR");
    }

    #[test]
    fn use_sidecar_falsy_values() {
        for v in ["0", "false", "", "no"] {
            std::env::set_var("OPENCHAMBER_SIDECAR", v);
            assert!(!use_sidecar(), "value {:?} should be falsy", v);
        }
        std::env::remove_var("OPENCHAMBER_SIDECAR");
    }

    #[test]
    fn parse_port_extracts_correctly() {
        assert_eq!(parse_port("http://127.0.0.1:8080"), 8080);
        assert_eq!(parse_port("http://127.0.0.1:1"), 1);
        assert_eq!(parse_port("http://127.0.0.1:65535"), 65535);
    }
}
```

- [ ] **Step 3: 在 `oc-tauri/src-tauri/src/lib.rs` 注册 `backend` 模块**

在 `lib.rs` 的模块声明区（L14-23 附近，`mod sidecar;` 那块）加：

```rust
mod backend;
```

- [ ] **Step 4: 运行 `cargo check -p oc-tauri` 验证编译**

Run: `cargo check -p oc-tauri 2>&1 | tail -20`
Expected: 编译通过。若报 `unresolved import oc_server`，确认 Task 4 Step 1 的 Cargo.toml 依赖已加。

- [ ] **Step 5: 运行 backend 单元测试**

Run: `cargo test -p oc-tauri --lib backend -- --test-threads=1 2>&1 | tail -15`
Expected: 4 个测试全过（use_sidecar × 3 + parse_port × 1）。

- [ ] **Step 6: 提交**

```bash
git add rust/oc-tauri/src-tauri/src/backend.rs rust/oc-tauri/src-tauri/src/lib.rs rust/oc-tauri/src-tauri/Cargo.toml
git commit -m "feat(rust): Phase 4B — backend.rs 统一后端句柄 + sidecar 决策

- BackendHandle 枚举: InProcess(OcServer) | Sidecar(SidecarHandle)
- use_sidecar(): OPENCHAMBER_SIDECAR env 门控
- parse_port(): base_url 端口提取
- 4 个单元测试"
```

---

## Task 5: 改造 `oc-tauri/src-tauri/src/lib.rs` — setup 分叉 + 改名

把现有 setup 的 sidecar-only 逻辑改为按 `use_sidecar()` 分叉，全局状态改名。

**Files:**
- Modify: `rust/oc-tauri/src-tauri/src/lib.rs`（setup 闭包 L78-143、全局状态 L31-50、关闭函数 L219-232、RunEvent L204-215）

**Interfaces:**
- Consumes: `backend::{BackendHandle, use_sidecar, parse_port}`（Task 4），`oc_server::{Config, OcServer}`（Task 2），现有 `SidecarBuilder`、`mini_chat::set_sidecar_port`、`ipc::globals::{build_init_script, RuntimeContext}`
- Produces: `pub fn backend_base_url() -> Option<String>`（原 `sidecar_base_url` 改名）、`pub fn shutdown_backend_public()`（原 `shutdown_sidecar_public` 改名）

- [ ] **Step 1: 改全局状态 `SidecarState` → `BackendState`，`SIDECAR` → `BACKEND`**

定位 `lib.rs` L31-37（`SidecarState` struct + `static SIDECAR`），替换为：

```rust
/// 全局后端句柄 + 它专属的 tokio 运行时。
struct BackendState {
    handle: Option<BackendHandle>,
    rt: Option<tokio::runtime::Runtime>,
}

static BACKEND: Mutex<Option<BackendState>> = Mutex::new(None);
```

- [ ] **Step 2: 改 `sidecar_base_url()` → `backend_base_url()`**

定位 `lib.rs` L42-50（`pub fn sidecar_base_url`），替换函数名和内部 `SIDECAR` 引用：

```rust
/// 获取后端 base_url (供 IPC 命令 HTTP 调用后端端点)。
///
/// 返回 `http://127.0.0.1:<port>`，后端未启动时返回 None。
/// 用例: `dialog_cmd::openchamber_file_grant` 调 `POST /api/fs/grant`。
pub fn backend_base_url() -> Option<String> {
    BACKEND
        .lock()
        .ok()?
        .as_ref()
        .and_then(|s| s.handle.as_ref())
        .map(|h| h.base_url())
}
```

- [ ] **Step 3: 改 setup 闭包的后端启动块（L78-143 的 `#[cfg(desktop)]` 块）**

定位 L78-143（`#[cfg(desktop)] { ... SidecarBuilder ... *SIDECAR.lock() ... }`），整个块替换为分叉逻辑：

```rust
            // --- 启动后端 (仅桌面端) ---
            #[cfg(desktop)]
            {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()?;

                if backend::use_sidecar() {
                    // —— 回退路径: sidecar 子进程 (现状不变) ——
                    let handle = rt.block_on(async {
                        SidecarBuilder::new()
                            .ready_timeout(std::time::Duration::from_secs(45))
                            .arg("--api-only")
                            .start()
                            .await
                    });
                    match handle {
                        Ok(h) => {
                            let port = h.port();
                            log::info!("sidecar ready on port {}", port);
                            mini_chat::set_sidecar_port(port);

                            let ctx = RuntimeContext::from_sidecar_port(port);
                            let init_script = build_init_script(&ctx);
                            if let Some(window) = app.get_webview_window("main") {
                                if let Err(e) = window.eval(&init_script) {
                                    log::error!("failed to inject init_script: {}", e);
                                }
                                #[cfg(all(target_os = "macos", feature = "vibrancy"))]
                                {
                                    apply_vibrancy_if_enabled(&window);
                                }
                            }

                            *BACKEND.lock().unwrap() = Some(BackendState {
                                handle: Some(BackendHandle::Sidecar(h)),
                                rt: Some(rt),
                            });
                        }
                        Err(e) => {
                            log::error!("sidecar startup failed: {:#}", e);
                            drop(rt);
                        }
                    }
                } else {
                    // —— 新路径: 进程内嵌入 oc-server ——
                    let server_result = rt.block_on(async {
                        let config = oc_server::Config::load()?;
                        oc_server::OcServer::start(config).await
                    });
                    match server_result {
                        Ok(server) => {
                            let base_url = server.base_url().to_string();
                            let port = backend::parse_port(&base_url);
                            log::info!("oc-server (in-process) ready on port {}", port);
                            mini_chat::set_sidecar_port(port);

                            let ctx = RuntimeContext::from_sidecar_port(port);
                            let init_script = build_init_script(&ctx);
                            if let Some(window) = app.get_webview_window("main") {
                                if let Err(e) = window.eval(&init_script) {
                                    log::error!("failed to inject init_script: {}", e);
                                }
                                #[cfg(all(target_os = "macos", feature = "vibrancy"))]
                                {
                                    apply_vibrancy_if_enabled(&window);
                                }
                            }

                            *BACKEND.lock().unwrap() = Some(BackendState {
                                handle: Some(BackendHandle::InProcess(server)),
                                rt: Some(rt),
                            });
                        }
                        Err(e) => {
                            log::error!("oc-server embed startup failed: {:#}", e);
                            drop(rt);
                        }
                    }
                }
            }
```

- [ ] **Step 4: 提取 vibrancy 逻辑为 helper（消除两条路径的重复）**

在 `lib.rs` 底部（`shutdown_*` 函数之前）加 helper，供 Step 3 的两条分支调用：

```rust
/// macOS vibrancy: 读 settings 判断是否启用 (默认开), 启用则 apply。
#[cfg(all(target_os = "macos", feature = "vibrancy"))]
fn apply_vibrancy_if_enabled(window: &tauri::WebviewWindow) {
    let vibrancy_enabled = settings::SettingsStore::get_bool(
        "desktopVibrancy",
        true,
    );
    if !vibrancy_enabled {
        return;
    }
    use window_vibrancy::{apply_vibrancy, NSVisualEffectMaterial, NSVisualEffectState};
    match apply_vibrancy(
        window,
        NSVisualEffectMaterial::Sidebar,
        Some(NSVisualEffectState::Active),
        None,
    ) {
        Ok(()) => log::info!("[vibrancy] applied sidebar material"),
        Err(e) => log::warn!("[vibrancy] failed to apply: {}", e),
    }
}
```

> 注意：原 L108-131 的 vibrancy 代码内联在 sidecar 成功分支里。提取后两路径共用。原内联代码删除。

- [ ] **Step 5: 改关闭函数 `shutdown_sidecar` → `shutdown_backend`**

定位 L219-232（`fn shutdown_sidecar` + `pub fn shutdown_sidecar_public`），替换为：

```rust
/// 同步清理后端: 从全局状态取出句柄, 在它的运行时上 block_on shutdown。
fn shutdown_backend() {
    let mut guard = BACKEND.lock().unwrap();
    if let Some(mut state) = guard.take() {
        if let (Some(rt), Some(handle)) = (state.rt.take(), state.handle.take()) {
            let _ = rt.block_on(async { handle.shutdown().await; });
        }
        drop(state);
    }
}

/// 公开的后端清理入口 (供 updater 模块在 on_before_exit 中调用)。
pub fn shutdown_backend_public() {
    shutdown_backend();
}
```

- [ ] **Step 6: 更新所有 `shutdown_sidecar` / `sidecar_base_url` 调用点**

在 `lib.rs` 内搜索所有调用旧名的地方，改为新名：
- L173（`tauri::WindowEvent::Destroyed` 分支）: `shutdown_sidecar()` → `shutdown_backend()`
- L213（`RunEvent::Exit` 分支）: `shutdown_sidecar()` → `shutdown_backend()`

搜索其他文件是否有调用：
Run: `grep -rn "shutdown_sidecar\|sidecar_base_url\|shutdown_sidecar_public" rust/oc-tauri/src-tauri/src/`
把所有匹配改为 `shutdown_backend` / `backend_base_url` / `shutdown_backend_public`。

> `updater.rs` 很可能调用 `shutdown_sidecar_public` — 改为 `shutdown_backend_public`。
> `ipc/` 下若有 `sidecar_base_url` 调用 — 改为 `backend_base_url`。

- [ ] **Step 7: 运行 `cargo check -p oc-tauri` 验证编译**

Run: `cargo check -p oc-tauri 2>&1 | tail -20`
Expected: 编译通过。若报 `cannot find function shutdown_sidecar` 等，说明 Step 6 有遗漏的调用点。

- [ ] **Step 8: 运行 oc-tauri 测试**

Run: `cargo test -p oc-tauri --lib -- --test-threads=1 2>&1 | tail -10`
Expected: 现有测试 + backend.rs 的 4 个测试全过（共 69 + 4 = 73）。

- [ ] **Step 9: 提交**

```bash
git add rust/oc-tauri/src-tauri/src/lib.rs rust/oc-tauri/src-tauri/src/updater.rs
git commit -m "feat(rust): Phase 4B — Tauri setup 分叉 (进程内嵌入默认 + sidecar 回退)

- setup 按 use_sidecar() 分叉: 默认 OcServer::start, SIDECAR=1 走旧路径
- SidecarState → BackendState, SIDECAR → BACKEND
- sidecar_base_url → backend_base_url, shutdown_sidecar → shutdown_backend
- vibrancy 逻辑提取为 apply_vibrancy_if_enabled helper (两路径共用)"
```

---

## Task 6: 验收 smoke 测试

最终验证：两条路径都能工作，全量测试绿色。

**Files:** 无（纯验证）

- [ ] **Step 1: 全量 oc-server 测试**

Run: `cargo test -p oc-server -- --test-threads=1 2>&1 | tail -5`
Expected: 1123 测试全过（1120 原有 + 3 新增 lib）。

- [ ] **Step 2: 全量 oc-tauri 测试**

Run: `cargo test -p oc-tauri --lib -- --test-threads=1 2>&1 | tail -5`
Expected: 73 测试全过（69 原有 + 4 新增 backend）。

- [ ] **Step 3: workspace 全量编译检查**

Run: `cargo check --workspace 2>&1 | tail -5`
Expected: 全 crate 编译通过。

- [ ] **Step 4: oc-server bin 手动 smoke**

Run: `cargo run -p oc-server &; sleep 2; curl -s http://127.0.0.1:<port>/health; kill %1`
Expected: `/health` 返回成功 JSON，ctrl-c/kill 后进程干净退出。

（读取启动行获取端口：`openchamber server listening on http://127.0.0.1:<port>`）

- [ ] **Step 5: Tauri 默认路径手动 smoke（进程内嵌入）**

Run: `cargo tauri dev`
Expected: UI 加载、WebView 显示 OpenChamber 界面、退出无孤儿进程。
退出后验证无残留 oc-server 进程: `pgrep -fl oc-server` 应无输出。

- [ ] **Step 6: Tauri sidecar 回退路径手动 smoke**

Run: `OPENCHAMBER_SIDECAR=1 cargo tauri dev`
Expected: 走旧 sidecar 路径，日志显示 `sidecar ready on port ...`，UI 正常。

- [ ] **Step 7: 更新 README 进度**

修改 `rust/README.md`：
- Phase 4B 标记为完成
- 结构表注释更新（删除「过渡态 sidecar」「最终态进程内嵌」的 TODO 语气，改为「默认进程内嵌, SIDECAR=1 回退」）

定位 README L18-19：
```
> **过渡态**(阶段 4A):sidecar spawn 现有 `@openchamber/web` CLI;
> **最终态**(阶段 4B):进程内嵌 `oc-server`。
```
改为：
```
> **默认**(阶段 4B):进程内嵌 `oc-server` (axum);
> **回退**(`OPENCHAMBER_SIDECAR=1`):sidecar spawn `@openchamber/web` CLI。
```

在进度清单加 Phase 4B 完成段（参考 README 现有 Phase 4A 段格式，L536 附近）。

- [ ] **Step 8: 最终提交**

```bash
git add rust/README.md
git commit -m "docs(rust): Phase 4B 完成 — 进程内嵌 oc-server

默认走进程内嵌入 Rust 后端; OPENCHAMBER_SIDECAR=1 回退 sidecar。
下一步: Phase 5 收尾 (删 Node 后端 / 依赖清理 / CI 更新)。"
```

---

## Self-Review 结果

**1. Spec coverage:**
- §2 架构总览（结构变化、数据流、Runtime 共享）→ Task 2 + Task 5 ✅
- §3 OcServer 句柄 API → Task 2 ✅
- §4 Tauri 集成与 sidecar 回退（BackendHandle、setup 分叉、改名）→ Task 4 + Task 5 ✅
- §5 测试策略（lib 单测、决策函数单测、手动 smoke）→ Task 3 + Task 4 + Task 6 ✅
- §6 实施范围（文件清单、不做清单、风险缓解）→ 贯穿所有 task 的 Global Constraints ✅
- §7 成功标准（5 条验收）→ Task 6 Step 1-5 ✅

**2. Placeholder scan:** Task 2 Step 1 的 `build_router` 用了说明性占位（指明从 main.rs 复制完整函数体），附了实施者注解释原因。这是必要的——360 行路由注册代码原样复制，逐行写进计划会臃肿且无信息增量。其余步骤无 placeholder。✅

**3. Type consistency:**
- `OcServer::start(config: Config)` — Task 2 定义, Task 3/5 消费 ✅
- `OcServer::base_url(&self) -> &str` — Task 2 定义, Task 4 `BackendHandle::base_url` 消费 (`s.base_url().to_string()`) ✅
- `OcServer::shutdown(self)` (async, takes self) — Task 2 定义, Task 4 `BackendHandle::shutdown` 消费 (`s.shutdown().await`) ✅
- `BackendHandle::{InProcess, Sidecar}` — Task 4 定义, Task 5 消费 ✅
- `use_sidecar() -> bool` — Task 4 定义, Task 5 消费 ✅
- `parse_port(&str) -> u16` — Task 4 定义, Task 5 消费 ✅
- `backend_base_url() -> Option<String>` — Task 5 定义 ✅
- `shutdown_backend()` / `shutdown_backend_public()` — Task 5 定义 ✅
- `OpenCodeHandle` (具体 struct, 非 trait) — Task 2 `oc_handle: Option<OpenCodeHandle>` 字段, shutdown 调 `h.shutdown().await` (&mut self) ✅

无类型不一致。
