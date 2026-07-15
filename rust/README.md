# OpenChamber Rust workspace (迁移中)

渐进式迁移 `packages/web/server` (Express → axum) 与 `packages/electron`
(Electron → Tauri) 到 Rust。完整计划见
[`docs/plan/rust-migration-plan.md`](../docs/plan/rust-migration-plan.md)。

## 结构

| Crate | 角色 | 替换目标 |
|---|---|---|
| `oc-core` | 共享类型与错误 | (新建) |
| `oc-opencode-sdk` | OpenCode 服务端客户端 | `@opencode-ai/sdk` (服务端用法) |
| `oc-server` | axum 后端二进制 | `packages/web/server` |
| `oc-tauri` | Tauri 桌面壳 | `packages/electron` |

> `oc-tauri` 的实际 Rust 代码在 `oc-tauri/src-tauri/`(Tauri 标准布局,
> 由 `tauri-cli init` 生成)。workspace member 指向 `oc-tauri/src-tauri`。
> **过渡态**(阶段 4A):sidecar spawn 现有 `@openchamber/web` CLI;
> **最终态**(阶段 4B):进程内嵌 `oc-server`。

## 构建

```bash
cd rust
cargo check --workspace      # 类型检查 (全部 crate)
cargo run -p oc-server       # 启动后端 (阶段 1: OpenCode 代理 + dist 托管 + /health)
cargo tauri dev              # 启动桌面壳 (dev URL 模式, 需先起 web dev server)
```

## 当前进度

**阶段 0 — 脚手架** (完成):
- [x] cargo workspace + 4 个 crate 骨架
- [x] Win10 编译验证 (`cargo check`)
- [x] Tauri 应用初始化 (`oc-tauri/src-tauri`, `cargo check` 通过)
- [x] `/api` 契约快照 (273 路由 + 5 WS + 5 SSE + catch-all proxy,
      `bun run snapshot:routes` → `rust/oc-server/api-routes-snapshot.json`)
- [x] 前置解耦: `mintOutsideFileGrant` 走 HTTP `POST /api/fs/grant`
      (Tauri 跨进程调用 sidecar, Electron 保持原 import)

**阶段 1 — 第一个垂直切片 (axum 后端)** (完成):
- [x] CLI/env 解析 (clap): host/port/api-only/ui-password/dist-dir/opencode 全套 env parity
- [x] 绑定地址安全检查 (`bind_host.rs`: loopback 检测, 拒绝未认证 LAN)
- [x] OpenCode 进程管理 (`opencode.rs`: managed spawn + managed password + stdout 就绪行解析 + /global/health 轮询门 + 进程组整杀)
- [x] OpenCode HTTP 反向代理 (`proxy.rs`: /api/* catch-all, 头过滤, Basic auth 注入, 4min 超时, 流式响应)
- [x] 状态端点 (`routes.rs`: /health, /api/version, /api/system/info — JSON 与 Node 对齐)
- [x] 静态 dist 托管 + SPA fallback (`static_files.rs`: ServeDir + index.html fallback)
- [x] 优雅关闭 (SIGINT/SIGTERM → kill OpenCode 子进程)
- [x] oc-core: Error http_status()/to_json() helpers
- [x] oc-opencode-sdk: health() 实现 (GET /global/health + Basic auth)
- [x] `cargo test` 25/25 通过 (oc-server 22 + oc-opencode-sdk 3), clippy 0 警告

**阶段 2 — 实时传输层 (SSE + WebSocket)** (完成):
- [x] SSE 透传代理 (`realtime/sse_proxy.rs`: `/api/event`, `/api/global/event`
      — 纯 chunk 透传 + 20s 边界感知心跳 `:heartbeat\n\n` + Last-Event-ID 透传)
- [x] WS 全局事件桥 (`realtime/ws_bridge.rs`: `/api/global/event/ws`
      — 共享上游 reader + 2048 事件 replay ring + ready 握手 + reconnect-after-ready)
- [x] WS 目录事件桥 (`realtime/ws_bridge.rs`: `/api/event/ws`
      — 每连接独享上游 reader + Last-Event-ID 续传, 无 replay)
- [x] 上游 SSE reader (`realtime/upstream_reader.rs`: stall 检测 + 无声重连
      + Last-Event-ID 跨重连持久 + SSE envelope 解析)
- [x] 全局 hub (`realtime/global_hub.rs`: 单共享 reader → broadcast fan-out
      + bounded replay ring + 状态通知)
- [x] WS 帧协议 (`realtime/protocol.rs`: ready/event/error/backpressure
      4 种 JSON-over-text-frames, 与 `event-pipeline.ts` 对齐)
- [x] 背压三层 (max_write_buffer 16MB 硬断 + 12MB 警告帧 + send().await 天然背压)
- [x] `cargo test` 52/52 通过 (新增 25 测试), clippy 0 警告

**阶段 3a (前半) — 功能模块: text + fs** (完成):
- [x] axum 错误桥 (`error.rs`: `ApiError` newtype 包装 `oc_core::Error`,
      `impl IntoResponse` 绕过 orphan rule, wire 格式 `{ "error": "..." }`)
- [x] 文本摘要模块 (`text/`: `POST /api/text/summarize`
      — 移植 `summarization.js` 正则管道 (TTS/notification/note 三模式)
      + 手动实现句分割 (JS lookbehind `(?<=[.!?])\s+` → Rust 手动扫描)
      + U+2026 省略号蒸馏 + 条件 omit originalLength/summaryLength)
- [x] 工作区目录解析 (`project_dir.rs`: header/query hint → settings.json
      lastDirectory → activeProjectId → projects[0], `~` 展开, URI 解码)
- [x] 文件系统模块 15 个路由 (`fs/`: grant/home/mkdir/clone/stat/read/raw/serve/
      write/delete/rename/reveal/exec/exec-status/list)
- [x] outside-workspace grant 系统 (`fs/grants.rs`: Map + 10min TTL + scope 检查
      + canonical path 精确相等, 对齐 `mintOutsideFileGrant`)
- [x] 工作区边界检查 (`fs/workspace.rs`: `is_path_within_root` + lexical normalize
      + project dir / user config root 双根检查)
- [x] 文件操作 (`fs/operations.rs`: 原子写 .tmp→rename, optional stat, 平台 reveal)
- [x] 命令执行系统 (`fs/exec.rs`: `/bin/sh -c` + 超时 + TTL 30min job 存储
      + background=true 始终拒绝, windowsHide)
- [x] 文件服务 (`fs/serve.rs`: 24 扩展名 MIME 表 + RFC 5987 Content-Disposition
      + Cache-Control: no-store + X-Content-Type-Options: nosniff + 100MiB 上限)
- [x] `cargo test` 117/117 通过 (新增 65 测试), clippy 0 警告

**阶段 3a (后半) — 功能模块: git** (完成):
- [x] Git 模块 68 个路由 (`git/`: 通过 `tokio::process::Command` spawn `git` CLI 二进制,
      不使用 `simple-git` 或 `git2`)
- [x] `GitRunner` 核心抽象 (`git/runner.rs`: 二进制解析 (Windows 探测) +
      `SSH_AUTH_SOCK` 探测 (`~/.gnupg/S.gpg-agent.ssh` → `gpgconf`) +
      `run()`/`run_or_throw()` + `windowsHide` + 20MB stdout 缓冲)
- [x] 纯解析函数 (`git/parsing.rs`: porcelain v1 status + numstat + log `\x1e`/`\x1f` 分隔 +
      worktree porcelain + shortstat regex + remotes verbose + name-status + stash list)
- [x] 仓库上下文 (`git/context.rs`: `RepoContext` (repo root 解析) +
      `GitFileContext` (路径遍历防护 + repo-relative 路径计算))
- [x] 身份管理 (`git/identity.rs`: profiles CRUD (`~/.config/openchamber/git-identities.json`) +
      global identity + current/has-local/set-identity + `~/.git-credentials` 解析)
- [x] `get_status` 最复杂函数 (`git/status.rs`: numstat 合并 + 新文件行数统计 (200文件/1MB/binary NUL检测) +
      ahead/behind fallback (origin/HEAD → main → master) + upstream remote 比较 + merge/rebase 检测)
- [x] Diff 操作 (`git/diff.rs`: get_diff + get_file_diff + get_commit_file_diff + range diff)
- [x] Log 操作 (`git/log.rs`: `\x1e`/`\x1f` 分隔格式 + shortstat + all mode)
- [x] Branch 操作 (`git/branch.rs`: list/create/delete/rename/checkout + remote branch filtering)
- [x] Commit 操作 (`git/commit.rs`: stage/unstage/commit/revert/hunk apply +
      cherry-pick/revert-commit/reset-to-commit)
- [x] Remote 操作 (`git/remote.rs`: pull/push/fetch/remotes + push 的 3 层 upstream fallback)
- [x] Merge/Rebase (`git/merge_rebase.rs`: merge/rebase + abort/continue + conflict 检测)
- [x] Stash (`git/stash.rs`: list/apply/pop/drop/push + batch file-counts)
- [x] Worktree (`git/worktree.rs`: list/create/remove/validate/preview +
      bootstrap status (stub) + canonicalize + primary-root/toplevel 解析)
- [x] Integrate (`git/integrate.rs`: plan/run/abort/continue + conflict-details + cherry-pick-status)
- [x] OpenCode DB sync stub (`syncSandboxesToOpenCodeDb` — 阶段 4B 进程内嵌时实现)
- [x] `cargo test` 150/150 通过 (新增 33 测试), clippy 0 警告

**阶段 4A — Tauri 桌面壳 (优先, sidecar 过渡)** (进行中):
- [x] `tauri-cli` 初始化, workspace 集成
- [x] Tauri 启动加载 UI (dev URL 模式, `cargo tauri dev` 验证 WebView 渲染)
- [x] sidecar 管理 (`sidecar.rs`: `SidecarBuilder`/`SidecarHandle`, 平台整树杀,
      `/health` 就绪门, `cargo test` 6/6 通过)
- [x] IPC 契约对等 (`window.__OPENCHAMBER_DESKTOP__`)
      — `init_script` 注入标量全局变量 + 5 方法桥 (invoke/openDialog/grantFileAccess/openExternal/listen),
      `openchamber_invoke` 分发 ~50 命令 (含 17 个 `COMMANDS_SAFE_FOR_REMOTE` origin 门),
      事件双路径 (handler + DOM CustomEvent), `cargo test` 15/15 通过
- [x] 原生集成 (窗口 chrome / shell / 通知 / 对话框 / 应用菜单 / 深链 / 开机自启)
- [x] settings.json 原子持久化 (`settings.rs`, 与 Electron 共享同一文件)
- [x] keep-awake (`power.rs`: macOS caffeinate / Windows SetThreadExecutionState / Linux systemd-inhibit)
- [x] 动画托盘 (`tray.rs`: 16 帧 ping-pong breathing 动画, title/tooltip, 状态行图标, macOS template)
- [x] macOS Vibrancy (`window-vibrancy`: Sidebar 材质, settings 驱动, flash 防护)
- [x] Mini-chat 多窗口 (`mini_chat.rs`: session/draft 模式, 去重, pinning)
- [x] Auto-update (`updater.rs`: tauri-plugin-updater, 404 容错, 进度事件)
- [x] 应用发现 (`discovery.rs`: host probe /health + /version, pairing candidate)
- [x] SSH 管理 (`ssh/`: ControlMaster 编排, ~1300 行, 1:1 移植 ssh-manager.mjs)
- [x] `cargo test` 69/69 通过
