# OpenChamber 后端 + 桌面壳 Rust 化改造计划

> 本文档基于对现有代码库的实际调研(41.4 万行 TS/TSX/JS,6 个工作区)制定。
> 计划为**渐进式并存迁移**:新旧后端并行,按垂直切片逐步切换,每阶段独立可交付。

---

## 一、现状摸底(基于实际代码)

| 部分 | 体量 | 角色 | 本次改造 |
|---|---|---|---|
| `packages/ui` | ~30 万行 / 886 文件 | React + ~90 Zustand store + 自研同步引擎 | **不改** |
| `packages/web/server` | ~8 万行 / 25+ 功能模块 | Express 后端 + OpenCode 代理 | **Rust 重写** |
| `packages/vscode` | ~2.4 万行 | VS Code 扩展 | **不改**(物理不可迁移) |
| `packages/electron` | ~0.9 万行(`main.mjs` 4900 行 + `ssh-manager.mjs` 49KB) | 桌面壳 | **换 Tauri** |
| `packages/mobile` + Swift | ~736 行 Swift | Capacitor iOS | **不改** |
| OpenCode 二进制 | 独立仓库(`../opencode`) | AI 服务端 | **保持代理关系** |

**合计约 41.4 万行 TS/TSX/JS**,本次改造目标约 9 万行(后端 + 桌面壳)。

## 二、不可迁移的硬约束

1. **VS Code 扩展**:`vscode` 引擎只能跑 JS/TS,无法 Rust 化。调研确认它**只耦合 OpenCode 二进制、不依赖 `packages/web/server` 任何路由**,所以后端替换对它**零影响**。
2. **iOS Swift**(WidgetKit/NSE/Control Center):iOS 专属,留 Swift。
3. **浏览器侧 `ghostty-web` 终端渲染器**:UI 侧 WASM,与后端无关。

## 三、关键决策(已确认)

- **范围**:后端 + 桌面壳
- **OpenCode**:保持 spawn 二进制 + HTTP/SSE 代理
- **策略**:渐进式并存迁移(新旧并行,按垂直切片逐步切换,每阶段可交付)
- **桌面集成**:Rust 后端嵌入 Tauri 进程(同进程内嵌 web 服务器,UI 走 loopback,单二进制)

## 四、架构目标

```
┌─────────────────────────────────────────────────────────┐
│  Tauri 进程(单二进制)                                    │
│  ┌───────────────────────────────────────────────────┐  │
│  │  Rust 后端(axum)  ←  替换 packages/web/server      │  │
│  │  • HTTP/SSE/WS 路由(与现有 /api/* 契约字节对齐)      │  │
│  │  • OpenCode 代理(spawn + http-proxy 等价)          │  │
│  │  • PTY / relay / tunnels / git / quota / 推送      │  │
│  └───────────────┬───────────────────────────────────┘  │
│                  │ loopback 127.0.0.1:<port>              │
│  ┌───────────────▼───────────────────────────────────┐  │
│  │  WebView(现有 React UI,零改动)                     │  │
│  └───────────────────────────────────────────────────┘  │
│  + 原生集成(托盘/菜单/自动更新/SSH)替换 Electron 层      │
└─────────────────────────────────────────────────────────┘
        │ 代理 HTTP/SSE
        ▼
   opencode 二进制(独立,保持 spawn)
```

**核心原则**:Rust 后端逐字节复刻现有 `/api/*` 契约(路径、schema、SSE 事件格式、WS 帧协议、认证头语义)。`packages/ui` 的 `runtime-fetch`/`runtime-url`/`runtime-auth` 层不感知后端语言切换 → **UI 零改动**。

## 五、依赖映射表(已核实现有用法)

| 现有(Node) | 用途 | Rust 替代 | 风险 |
|---|---|---|---|
| `express` 5 + `http-proxy-middleware` | HTTP 服务器 + OpenCode 反代 | `axum` + `reqwest` | 低 |
| `ws` 8 | WS 服务器(终端/事件/dictation/relay) | `tokio-tungstenite` | 低 |
| `simple-git` | git 操作 | `git2` + `tokio::process` | 中 |
| `better-sqlite3` | SQLite | `rusqlite` | 低 |
| `node-pty`/`bun-pty` | PTY | `portable-pty` | 低(ConPTY 对齐) |
| `jose` | JWT/JWK | `jsonwebtoken` | 低 |
| `@simplewebauthn/server` | WebAuthn/passkey | `webauthn-rs` | 中 |
| `web-push` | VAPID 推送 | `web-push` crate | 低 |
| `@octokit/rest` | GitHub | `octocrab` | 低 |
| **`@opencode-ai/sdk`(162 文件)** | OpenCode typed 客户端 | **自研 crate(OpenAPI codegen)** | **高** |
| `sherpa-onnx-node` | 本地语音 | 保留 C++ 库走 FFI | 中 |
| `electron` 41 + `electron-updater` | 桌面壳 | `tauri` + `tauri-plugin-updater` | 中 |
| `yaml`/`zod` | 配置解析/校验 | `serde_yaml` + `validator` | 低 |

## 六、渐进式迁移路线(5 个阶段)

### 阶段 0:脚手架与契约固化(1-2 周)

**不动现有代码,搭好 Rust 工作区并锁定契约。**

- 新建 `rust/` 工作区,crate 结构:
  - `oc-server`(axum 二进制,替换 `packages/web/server`)
  - `oc-opencode-sdk`(OpenCode 客户端,替换 SDK 服务端用法)
  - `oc-core`(共享类型/错误)
  - `oc-tauri`(桌面壳,替换 `packages/electron`)
- **写 `/api` 契约快照测试**:扫描现有路由注册 + 导出 TS schema,作为 Rust 侧基准线
- 本机 Win10 验证 `rusqlite`/`portable-pty`/`axum`/`tokio-tungstenite`/`git2` 全链路编译
- **前置解耦**:`packages/electron/main.mjs` 直接 import `@openchamber/web/server/lib/fs/routes.js` 的 `mintOutsideFileGrant` → 抽成独立模块或 HTTP 端点,消除编译期耦合

**可交付**:空 Rust 工作区 + 契约快照文档。现有产品完全不变。

---

### 阶段 1:第一个垂直切片——静态资源 + 健康检查 + OpenCode 代理(2-3 周)

**Rust 后端能启动、托管 React dist、代理 OpenCode,UI 基本工作。**

- CLI/env 解析(`clap`)
- 绑定地址安全检查(拒绝未认证 LAN 绑定)
- OpenCode 生命周期:`spawn('opencode serve')`、解析就绪行、15s 健康探测、重启/优雅关闭
- OpenCode HTTP/SSE 代理:动态目标、注入 managed-password、hop-by-hop 头过滤、6s 就绪门
- 静态 dist 托管 + SPA fallback + PWA manifest
- `/health`、`/api/system/info`、`/api/version`

**验证**:Rust 后端起在另一端口,环境变量把 UI 指过去,跑真实会话验证基本通。旧 Node 后端继续作为默认。

---

### 阶段 2:实时传输层(3-4 周,最高风险)

**SSE 事件流 + WebSocket 端点逐字节对齐。**

- SSE 转发:`/api/global/event`、`/api/event`(背压 forwarder、20s 心跳、TCP_NODELAY、`Last-Event-ID` 续传)
- WS 桥:`/api/event/ws`、`/api/global/event/ws`(per-directory + 全局 hub + bounded replay)
- 上游 SSE reader(单共享 reader、stall 重连)
- **验证**:高并发流式会话下,事件序列与 Node 后端 diff 对比一致

> 这是整个迁移的技术深水区(仅 `event-pipeline.ts` 就 800+ 行 backoff/coalescing)。

---

### 阶段 3:功能模块逐个迁移(8-12 周,可并行)

按"依赖少、价值高、风险低"排序:

#### 批次 3a(低风险,机械映射)

- `fs/`(读写/exec/reveal,含 `mintOutsideFileGrant` 的 Rust 实现)
- `git/`(status/diff/log/branch/worktree/commit/pull/push/merge/rebase/stash/identity)→ `git2`
- `text/`

#### 批次 3b(中风险,外部集成)

- `github/`(OAuth device flow、PR status)→ `octocrab`
- `tunnels/` + `cloudflare-tunnel.js`/`ngrok-tunnel.js`
- `ui-auth/` + `client-auth/`(scrypt、WebAuthn、pairing v2)
- `notifications/`(web-push、APNs relay/direct、SSE fan-out)

#### 批次 3c(高风险,专项攻坚)

- **`relay/`(E2EE 隧道:ECDH+AEAD、tunnel codec)**——必须通过现有 `cross-compat.test.js` 字节兼容测试。**先移植测试向量,TDD 实现到全绿**
- `terminal/`(PTY WS、二进制控制帧、64KB replay)→ `portable-pty`
- `dictation/` + `tts/`(sherpa-onnx FFI、OpenAI-compatible)
- `quota/`(16 个 provider,纯体力活)
- `scheduled-tasks/`、`permission-auto-accept/`、`session-{assist,goal,folders}/`、`skills-catalog/`、`small-model/`、`magic-prompts/`

#### 每模块统一切换协议

1. Rust 实现 + 移植现有 `*.test.js` 为 `#[test]`
2. 同一 UI 流程打新旧后端对比响应
3. 环境变量灰度切换该模块路由;旧路由暂留回退
4. 观察 1-2 周无回归,删旧路由

---

### 阶段 4:Tauri 桌面壳替换 Electron(4-6 周)

**前置:阶段 1-3 完成。**

- `oc-tauri` 启动时同进程拉起 `oc-server`,WebView 加载 loopback
- 迁移 `main.mjs` + `preload.mjs` 能力:
  - 窗口管理(多窗口、mini-chat、vibrancy、macOS traffic light)
  - 托盘(`tray.mjs`:活动指示、待审批 Allow/Deny、会话列表)
  - 菜单、深链 `openchamber-ui://`、launch-at-login、powerSaveBlocker
  - **自动更新**(`electron-updater` → `tauri-plugin-updater`)
  - **SSH 管理**(49KB `ssh-manager.mjs`:ControlMaster、端口转发)→ `russh` 重写或继续 spawn `ssh`
  - 原生通知、shell open、文件对话框、屏幕捕获
- preload 桥对等:Tauri `invoke` + 事件系统替换 `window.__OPENCHAMBER_DESKTOP__`,**保持 IPC 契约不变** → `packages/ui/src/lib/desktop.ts` 几乎零改动
- 打包:macOS(dmg,notarized)、Windows(NSIS)、Linux(AppImage x64+arm64)

---

### 阶段 5:收尾与清理(1-2 周)

- 删除 `packages/web/server` 和 `packages/electron`(或保留 Node 回退)
- 更新 `AGENTS.md`、`CHANGELOG.md`、CI/CD、`scripts/test-release-build.sh`
- `bun.lock`/依赖清理;`packages/web` 收缩为纯 UI bundle + CLI 壳
- 模块文档迁移(`server/lib/*/DOCUMENTATION.md` → Rust 模块文档)

## 七、风险登记册

| 风险 | 影响 | 缓解 |
|---|---|---|
| OpenCode SDK 契约面巨大(162 文件) | 高 | 阶段 0 类型快照;优先 OpenAPI codegen |
| relay E2EE 跨语言字节兼容 | 高 | 先移植 `cross-compat.test.js` 测试向量,TDD |
| 实时层 60Hz 事件不丢不乱序 | 高 | 阶段 2 事件序列 diff 测试(Rust vs Node 并行) |
| Windows ConPTY / 原生 crate 编译 | 中 | 阶段 0 本机 Win10 验证全链路 |
| Electron→Tauri 能力缺口(SSH、托盘细节) | 中 | 阶段 4 前做能力对照表;`russh` 兜底 |
| 迁移期双后端运营成本 | 中 | 环境变量灰度;每阶段有单一默认后端 |
| `sherpa-onnx` FFI 维护负担 | 中 | 保留 C++ 库,只绑最小接口 |

## 八、工期粗估(供决策,非承诺)

| 阶段 | 周期 |
|---|---|
| 阶段 0 脚手架 | 1-2 周 |
| 阶段 1 第一切片 | 2-3 周 |
| 阶段 2 实时层(深水区) | 3-4 周 |
| 阶段 3 功能模块(可并行) | 8-12 周 |
| 阶段 4 Tauri 壳 | 4-6 周 |
| 阶段 5 收尾 | 1-2 周 |
| **合计** | **约 5-8 个月**(1-2 人全职;阶段 3 并行可压缩) |

## 九、显式排除范围

- **UI(`packages/ui`)**:维持 React,任何 Rust 化不在本计划内
- **VS Code 扩展**:物理不可迁移,且后端替换对其零影响
- **OpenCode 二进制本身**(`../opencode`):AGENTS.md 禁止修改,保持代理关系
- **iOS Swift Widget/NSE/Control Center**:iOS 专属

## 十、后续推进建议

若要推进,建议从**阶段 0**起步(风险最低、产出最明确,能先验证 Rust 依赖链在 Windows 上跑得通)。阶段 0 的前置解耦(抽出 `mintOutsideFileGrant`)是解锁桌面壳独立迁移的关键。
