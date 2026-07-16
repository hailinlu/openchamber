# Phase 4B：oc-server 进程内嵌入设计

- **日期**: 2026-07-16
- **阶段**: Rust 迁移 Phase 4B（最终态：单二进制内嵌 Rust 后端）
- **前置**: Phase 0–3f（后端模块全部 Rust 化）+ Phase 4A（Tauri 桌面壳 sidecar 过渡态）
- **关联文档**: `docs/plan/rust-migration-plan.md`（阶段 4B，L201-208）、`rust/README.md`

---

## 1. 目标与背景

### 目标
把 `oc-server` 从 Tauri 的 **sidecar 子进程** 改为 **进程内嵌入**：Tauri 主进程直接在自身进程内启动 axum 后端，不再 spawn `openchamber serve --foreground` 子进程。

同时保留 sidecar 作为环境变量门控的回退路径，使迁移期若嵌入路径出问题可一键切回。

### 背景
- Phase 4A 完成了 Tauri 桌面壳，通过 `SidecarBuilder` spawn Node CLI 子进程（进程组/Job Object 整树清理 + `/health` 就绪门）
- `oc-server` 当前只有 `[[bin]]`（`main.rs` ~160 行启动编排），无 `lib.rs`，无法被同进程调用
- 迁移计划给 Phase 4B 的定位：「仅改启动逻辑，原生集成层与嵌入方式解耦」，低风险

### 非目标（YAGNI）
- 不动 relay 桩（`has_relay_demand`、`TungsteniteHostTransport` 激活、`syncSandboxesToOpenCodeDb`）—— 留后续阶段
- 不删 Node 后端（`packages/web/server`、`packages/electron`）—— Phase 5 收尾
- 不改 IPC 契约 / preload / UI —— WebView 仍是 loopback HTTP，后端嵌入与否对 UI 透明
- 不碰原生集成层（托盘、菜单、深链、vibrancy、power、updater、ssh、mini_chat）
- 不引入新依赖（`oneshot` 来自 tokio，`BackendHandle` 用 std enum）

---

## 2. 架构总览

### 结构变化

```
oc-server/
  src/
    lib.rs      ← 新增：OcServer 句柄 + start/shutdown 编排（从 main.rs 提取）
    main.rs     ← 瘦壳：load config → OcServer::start → 等 ctrl_c → shutdown
    ...（所有模块不变）

oc-tauri/src-tauri/src/
    lib.rs      ← 启动逻辑分叉：默认进程内嵌入，OPENCHAMBER_SIDECAR=1 走旧路径
    sidecar.rs  ← 保留不动（回退路径）
    backend.rs  ← 新增：BackendHandle 枚举 + use_sidecar() 决策
```

### 数据流

```
Tauri setup()
  ├─ 读 OPENCHAMBER_SIDECAR env
  ├─ 若 truthy → SidecarBuilder::start()（现状不变）
  └─ 否则（默认）→ OcServer::start(config)
        ├─ Config::load() / bind_host::enforce()
        ├─ opencode::start() → spawn opencode 子进程
        ├─ AppState::new() + 7 步模块 init（与现 main.rs 一致）
        ├─ build_router()
        ├─ tokio::net::TcpListener::bind(127.0.0.1:0) → OS 分配端口
        ├─ tokio::spawn(axum::serve(listener, app).with_graceful_shutdown(rx))
        └─ 返回 OcServer { base_url, shutdown_tx, join_handle, state, oc_handle }

WebView → http://127.0.0.1:<port>（UI 零改动，仍是 loopback HTTP）

Tauri Exit → BackendHandle::shutdown()
  ├─ InProcess: shutdown_tx 触发 → hub.stop → terminal.kill_all → opencode.shutdown
  └─ Sidecar: handle.kill()（现状）
```

### Runtime 共享策略
进程内嵌入路径用**专属 multi-thread tokio Runtime**（存进 `BackendState.rt`），与 sidecar 路径的 runtime 管理方式一致。不混用 Tauri 的 `async_runtime`——避免 shutdown 时跨 runtime 死锁。`OcServer::start()` 内部的 `tokio::spawn(axum::serve(...))` 跑在这个专属 runtime 上；关闭时 `rt.block_on(server.shutdown())`。

---

## 3. `OcServer` 句柄 API（方案 A：句柄式）

新增 `rust/oc-server/src/lib.rs`，把 `main.rs` 的启动编排提取为封装良好的句柄。

### API

```rust
/// 进程内嵌入的 oc-server 句柄。
/// 封装完整的启动编排 + 优雅关闭顺序。
pub struct OcServer {
    base_url: String,
    shutdown_tx: tokio::sync::oneshot::Sender<()>,
    join_handle: tokio::task::JoinHandle<()>,
    state: Arc<AppState>,
    oc_handle: opencode::Handle,   // OpenCode 子进程生命周期
}

impl OcServer {
    /// 启动后端：配置 → OpenCode spawn → AppState + 模块 init → 路由 → axum::serve。
    /// 端口绑定 127.0.0.1:0（OS 分配），返回句柄含实际 base_url。
    pub async fn start(config: Config) -> anyhow::Result<Self>;

    /// `http://127.0.0.1:<port>`，供 WebView 和 IPC 命令使用。
    pub fn base_url(&self) -> &str;

    /// 优雅关闭（与现 main.rs 顺序一致）：
    /// 触发 axum graceful shutdown → 等任务退出 → hub.stop →
    /// terminal.kill_all → opencode.shutdown。
    pub async fn shutdown(self);
}
```

### 关键设计决策

1. **端口分配**：`TcpListener::bind((127.0.0.1, 0))` 让 OS 分配——与现有 `sidecar::allocate_port` 策略一致。`start()` 内部 bind 后读取 `local_addr().port()` 填入 `base_url`。

2. **axum::serve 作为 spawned task**：不在 `start()` 里 await serve（那会阻塞调用方），而是：
   ```rust
   let (shutdown_tx, shutdown_rx) = oneshot::channel();
   let join = tokio::spawn(async move {
       axum::serve(listener, app)
           .with_graceful_shutdown(async { let _ = shutdown_rx.await; })
           .await
           .expect("oc-server serve failed");
   });
   ```
   `start()` 在 listener 已 bind 后立即返回，serve 在后台跑。

3. **关闭顺序封装在 `shutdown()`**，严格复刻 `main.rs:142-158` 的顺序：
   - 先停 axum（`shutdown_tx` 触发，停止接收新请求）
   - 等 `join_handle` 退出（serve loop 结束）
   - `state.global_hub.stop().await`（停上游 SSE reader）
   - `state.terminal_sessions.kill_all().await`（杀终端会话）
   - `oc_handle.shutdown().await`（杀 OpenCode 子进程）
   - 顺序不漂移到调用方。

4. **`opencode_skip_start` 分支**：`start()` 内部按 `config.opencode_skip_start` 分支。当为 true（如 `Config::for_tests()`）时跳过 `opencode::start()`，`oc_handle` 用 no-op 占位——使单元测试无需真实 opencode 二进制。

5. **模块可见性**：lib 化后，各模块声明从 `main.rs` 移入 `lib.rs` 为 `pub mod`。`main.rs` 只 `use oc_server::{Config, OcServer}`。bin 和 lib 不重复声明模块。

### `main.rs` 瘦壳

从 ~160 行降到 ~10 行：

```rust
use oc_server::{Config, OcServer};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config = Config::load()?;
    let server = OcServer::start(config).await?;
    println!("openchamber server listening on {}", server.base_url());

    tokio::signal::ctrl_c().await?;
    server.shutdown().await;
    tracing::info!("oc-server stopped");
    Ok(())
}
```

---

## 4. Tauri 集成与 sidecar 回退

### `backend.rs`：统一后端句柄

新增 `rust/oc-tauri/src-tauri/src/backend.rs`：

```rust
/// 统一后端句柄：进程内嵌入 或 sidecar 回退。
pub enum BackendHandle {
    InProcess(oc_server::OcServer),
    Sidecar(sidecar::SidecarHandle),
}

impl BackendHandle {
    pub fn base_url(&self) -> String {
        match self {
            Self::InProcess(s) => s.base_url().to_string(),
            Self::Sidecar(h) => h.base_url(),
        }
    }

    pub async fn shutdown(self) {
        match self {
            Self::InProcess(s) => s.shutdown().await,
            Self::Sidecar(mut h) => { let _ = h.kill().await; }
        }
    }
}

/// 决策：环境变量 OPENCHAMBER_SIDECAR=truthy → sidecar，否则进程内嵌入。
pub fn use_sidecar() -> bool {
    std::env::var("OPENCHAMBER_SIDECAR")
        .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE"))
        .unwrap_or(false)
}
```

### `lib.rs` 改动

**`SidecarState` → `BackendState`**：
```rust
struct BackendState {
    handle: Option<BackendHandle>,
    rt: Option<tokio::runtime::Runtime>,   // 两条路径都用（sidecar 驱动 spawn，
                                            // 进程内嵌入驱动 OcServer::start + serve）
}
static BACKEND: Mutex<Option<BackendState>> = Mutex::new(None);

pub fn backend_base_url() -> Option<String> { ... }  // 原 sidecar_base_url 改名
```

**setup 分叉**（替换现有 `SidecarBuilder` 块，`#[cfg(desktop)]` 内）：
```rust
if backend::use_sidecar() {
    // —— 回退路径：现状不变 ——
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all().build()?;
    let handle = rt.block_on(async {
        SidecarBuilder::new()
            .ready_timeout(std::time::Duration::from_secs(45))
            .arg("--api-only")
            .start().await
    });
    match handle {
        Ok(h) => {
            let port = h.port();
            mini_chat::set_sidecar_port(port);
            // 注入 init_script（与现有代码相同）
            ...
            *BACKEND.lock().unwrap() = Some(BackendState {
                handle: Some(BackendHandle::Sidecar(h)),
                rt: Some(rt),
            });
        }
        Err(e) => { log::error!("sidecar startup failed: {:#}", e); drop(rt); }
    }
} else {
    // —— 新路径：进程内嵌入 ——
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all().build()?;
    let server_result = rt.block_on(async {
        let config = oc_server::Config::load()?;
        oc_server::OcServer::start(config).await
    });
    match server_result {
        Ok(server) => {
            let base_url = server.base_url().to_string();
            let port = parse_port(&base_url);  // 从 "http://127.0.0.1:<port>" 提取
            mini_chat::set_sidecar_port(port);
            // 注入 init_script（与现有代码相同）
            ...
            *BACKEND.lock().unwrap() = Some(BackendState {
                handle: Some(BackendHandle::InProcess(server)),
                rt: Some(rt),
            });
        }
        Err(e) => { log::error!("oc-server embed startup failed: {:#}", e); drop(rt); }
    }
}
```

### 关闭路径

`shutdown_sidecar` → `shutdown_backend`：
```rust
fn shutdown_backend() {
    let mut guard = BACKEND.lock().unwrap();
    if let Some(mut state) = guard.take() {
        if let (Some(rt), Some(handle)) = (state.rt.take(), state.handle.take()) {
            rt.block_on(async { handle.shutdown().await; });
        }
        drop(state);
    }
}

pub fn shutdown_backend_public() { shutdown_backend(); }
```

### 依赖与可见性

`rust/oc-tauri/src-tauri/Cargo.toml` 新增：
```toml
oc-server = { workspace = true }
```

**循环依赖确认**：workspace 依赖图中 `oc-tauri → oc-server → oc-core / oc-opencode-sdk`，`oc-server` 不依赖 `oc-tauri`，无环。

`oc-server` 加 `[lib]` 段：
```toml
[lib]
name = "oc_server"
path = "src/lib.rs"

[[bin]]
name = "oc-server"
path = "src/main.rs"
```

---

## 5. 测试策略

### 5.1 lib 单元测试（`oc-server/src/lib.rs`）

验证 `OcServer` 基本契约：

```rust
#[tokio::test]
async fn oc_server_starts_and_serves_health() {
    let config = Config::for_tests();   // port=0, opencode_skip_start=true
    let server = OcServer::start(config).await.expect("start");
    let resp = reqwest::get(format!("{}/health", server.base_url()))
        .await.unwrap();
    assert!(resp.status().is_success());
    server.shutdown().await;
}

#[tokio::test]
async fn oc_server_shutdown_releases_port() {
    let server = OcServer::start(Config::for_tests()).await.unwrap();
    let port = parse_port(server.base_url());
    server.shutdown().await;
    let l = tokio::net::TcpListener::bind(("127.0.0.1", port)).await;
    assert!(l.is_ok(), "port should be free after shutdown");
}
```

`Config::for_tests()` 的 `opencode_skip_start: true` 要求 `OcServer::start()` 显式按此分支跳过 opencode spawn（见 3.4）。

### 5.2 启动编排回归

`OcServer::start()` 包含 7 步模块 init。关键不变量「start 成功 = 所有 init 成功 + listener 已 bind」由 5.1 两个测试覆盖。模块内部正确性已由现有 1120 个测试保证，Phase 4B 不重复测。

### 5.3 `main.rs` 薄壳

手动 smoke：`cargo run -p oc-server` 正常启动、响应 /health、ctrl-c 优雅退出。不写集成测试。

### 5.4 `backend.rs` 决策函数

```rust
#[test]
fn use_sidecar_defaults_false() {
    std::env::remove_var("OPENCHAMBER_SIDECAR");
    assert!(!backend::use_sidecar());
}

#[test]
fn use_sidecar_truthy_values() {
    for v in ["1", "true", "TRUE"] {
        std::env::set_var("OPENCHAMBER_SIDECAR", v);
        assert!(backend::use_sidecar());
    }
    std::env::remove_var("OPENCHAMBER_SIDECAR");
}
```

测试隔离（AGENTS.md）：env 测试串行，`--test-threads=1` 已是该 crate 惯例。

### 5.5 手动 smoke（两条路径）
- 默认进程内：`cargo tauri dev` → UI 加载、/health 可达、退出无孤儿进程
- sidecar 回退：`OPENCHAMBER_SIDECAR=1 cargo tauri dev` → 仍走旧路径

---

## 6. 实施范围与边界

### 改动文件清单

| 文件 | 改动 | 性质 |
|---|---|---|
| `rust/oc-server/src/lib.rs` | **新增**：`OcServer` 句柄 + `start()`/`shutdown()` 编排（从 main.rs 提取 7 步 init + serve + 关闭） | 核心 |
| `rust/oc-server/src/main.rs` | **瘦身**：~160 行 → ~10 行薄壳 | 核心 |
| `rust/oc-server/Cargo.toml` | 加 `[lib]` 段（`name = "oc_server"`），保留 `[[bin]]` | 配置 |
| `rust/oc-tauri/src-tauri/src/backend.rs` | **新增**：`BackendHandle` 枚举 + `use_sidecar()` 决策 + 单元测试 | 核心 |
| `rust/oc-tauri/src-tauri/src/lib.rs` | setup 分叉、`SidecarState`→`BackendState`、改名 `shutdown_sidecar`→`shutdown_backend`、`sidecar_base_url`→`backend_base_url` | 核心 |
| `rust/oc-tauri/src-tauri/Cargo.toml` | 加 `oc-server = { workspace = true }` 依赖 | 配置 |
| `rust/oc-tauri/src-tauri/src/sidecar.rs` | **不动**（保留为回退路径） | — |

### 明确不做

1. 不动 relay 桩（`has_relay_demand`、`TungsteniteHostTransport` 激活、`syncSandboxesToOpenCodeDb`）
2. 不删 Node 后端（`packages/web/server`、`packages/electron`）—— Phase 5
3. 不改 IPC 契约 / preload / UI
4. 不碰原生集成层（托盘、菜单、深链、vibrancy、power、updater、ssh、mini_chat）
5. 不引入新依赖

### 风险与缓解

| 风险 | 缓解 |
|---|---|
| `oc-server` 加 `[lib]` 后模块可见性问题（`mod` 在 bin 私有） | 模块声明全部移入 `lib.rs` 为 `pub mod`，`main.rs` 只 `use oc_server::{Config, OcServer}` |
| 进程内嵌入 Runtime 与 Tauri async_runtime 混用致 shutdown 死锁 | 进程内路径用专属 multi-thread runtime（存 `BackendState.rt`），`rt.block_on(shutdown)`，不混用 Tauri runtime |
| `Config::load()` 在 Tauri 上下文读 argv/env 与 bin 不同 | Tauri 调用时 argv 为空，走 env 默认值——这正是 `--api-only` + loopback 的预期路径 |
| `opencode::start()` 在 Tauri 上下文 spawn opencode 子进程失败 | 与 sidecar 路径同失败模式；`start` 返回 `Result`，setup 的 `Err` 分支已覆盖 |

---

## 7. 成功标准（验收）

1. `cargo run -p oc-server` — 独立 bin 正常工作（回归零）
2. `cargo tauri dev`（默认进程内嵌入）— UI 加载、/health 可达、退出无孤儿进程
3. `OPENCHAMBER_SIDECAR=1 cargo tauri dev` — sidecar 回退仍工作
4. `cargo test -p oc-server -- --test-threads=1` — 全绿（1120 + 新增 lib 测试）
5. `cargo check --workspace` — 全 crate 编译通过
