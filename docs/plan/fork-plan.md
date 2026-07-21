# Fork Plan: OpenChamber → <YourApp>

将当前 Rust + 前端代码库（OpenChamber）改造为独立应用，逐步移除 OpenChamber 官方服务依赖，
并防止用户同时安装两者时产生冲突。

> **基线**: Rust 迁移已完成（所有阶段标记完成，`cargo test` 1120+ 通过）。
> 本计划覆盖从 OpenChamber 分叉所需的所有变更。
>
> **⚠️ 范围**: 本计划只涉及 **Rust 后端 (`rust/`)** 和 **前端 (`packages/ui/`, `packages/web/`, `packages/mobile/`)**。
> Electron (`packages/electron/`) 保持不动，后续会移除。
>
> **⚠️ 说明**: `GridForge` 是此前已完成的品牌重命名结果。
> Tauri 中出现的 `GridForge`（产品名等）是你自己的品牌，
> **不是需要再改的原始标识**。本计划聚焦于：
> 1. 移除残留在代码库中的 OpenChamber 原始标识符
> 2. 移除对 OpenChamber 官方服务的依赖
> 3. 防止未来的分叉应用与原始 OpenChamber 冲突
> 4. 前端必须与 Rust 后端同步修改（注入变量、路由前缀、deep-link scheme、settings schema、i18n）

---

## 目录

1. [冲突清单](#1-冲突清单)
2. [品牌重命名范围 — Rust 后端](#2-品牌重命名范围--rust-后端)
3. [品牌重命名范围 — 前端](#3-品牌重命名范围--前端)
4. [服务依赖分析](#4-服务依赖分析)
5. [功能裁剪决策](#5-功能裁剪决策)
6. [分阶段迁移计划](#6-分阶段迁移计划)
7. [附录](#附录)

---

## 1. 冲突清单

用户同时安装 OpenChamber 和分叉应用时，以下冲突会逐一发生。
按严重程度排列。

### 1.1 🔴 致命级 — 一定会导致不可用

| # | 冲突 | 原因 | 影响 |
|---|------|------|------|
| C1 | **相同的 Bundle ID** | 两者都用 `dev.openchamber.desktop` | macOS Launch Services 无法区分；安装时会互相覆盖；权限/偏好设置冲突 |
| C2 | **相同的 `openchamber://` 协议处理器** | 两者都注册为 `openchamber://` 的默认处理程序 | 点击链接时随机打开其中一个；系统只能注册一个 |
| C3 | **相同的应用名称"GridForge"** | Tauri 和 Electron 都叫 GridForge（你已完成的改名） | 这是当前迁移过渡期的状态（同一应用的两个壳），不是分叉冲突。分叉后改产品名即可解决 |
| C4 | **数据目录完全重叠** | 默认都写入 `~/.config/openchamber/` | settings.json、jwt-secret、推送订阅等 12+ 文件被互相覆盖，状态完全损坏 |

### 1.2 🟠 高级 — 数据损坏或功能异常

| # | 冲突 | 原因 | 影响 |
|---|------|------|------|
| C5 | **共享 `settings.json`** | 两者都读写同一文件，使用共享 schema | 分叉后 schema 会分岔，一方写回会无声删除另一方的配置项 |
| C6 | **共享 `jwt-secret`** | 认证密钥相同 | 如果 JWT 格式变化，会话会互相拒绝 |
| C7 | **共享 `relay-host.lock`** | 中继锁文件冲突 | 两个进程抢夺中继连接，网络状态混乱 |
| C8 | **共享 `remote-clients.json` / `client-pairing-sessions.json`** | 远程客户端认证数据共享 | 配对 token 泄漏、客户端列表互相污染 |
| C9 | **共享 `push-subscriptions.json` / `apns-tokens.json`** | 推送订阅数据共享 | 推送会发到错误的应用；Token 互相覆盖 |
| C10 | **共享 GitHub OAuth 配置** | 使用相同的 GitHub Client ID (`Ov23lizomPOC3eFYo56r`) | 授权流向错误的应用 |
| C11 | **SSH ControlMaster socket 冲突** | `/tmp/ocssh-<hash>.sock` | SSH 连接互相干扰 |
| C12 | **开机自启互覆盖** | 两者用相同标识注册 auto-start | 最后一次安装的生效，另一个可能无法启动 |

### 1.3 🟡 中级 — 取决于用户配置

| # | 冲突 | 原因 | 影响 |
|---|------|------|------|
| C13 | **相同环境变量** | 都读 `OPENCHAMBER_PORT`、`OPENCHAMBER_DATA_DIR` 等 | 同时运行时配置互相覆盖 |
| C14 | **端口冲突** | 都监听 `OPENCHAMBER_PORT` 指定的端口 | 启动失败（端口被占用） |
| C15 | **共享 tunnel 配置文件** | 两者都写 `cloudflare-managed-remote-tunnels.json` | Tunnel token 互相覆盖 |
| C16 | **共享 `ui-passkeys.json`** | WebAuthn passkey 存储 | Passkey 可能被另一方删除或覆盖 |
| C17 | **共享 `git-identities.json`** | Git 身份配置共享 | 提交身份混乱 |

### 1.4 🔵 低级别 — 体验问题

| # | 冲突 | 原因 | 影响 |
|---|------|------|------|
| C18 | **日志目录命名冲突** | 都写 `~/Library/Logs/` 但文件名不同 | 目录混乱但功能正常 |
| C19 | **macOS 菜单栏图标同名** | 托盘图标可能都用"OpenChamber" | 用户无法区分 |
| C20 | **快捷键冲突** | 如果注册了全局快捷键 | 热键冲突 |

---

## 2. 品牌重命名范围 — Rust 后端

### 2.1 应用标识（高优先级）

| 标识 | 当前位置 | 替换建议 |
|------|---------|---------|
| Bundle ID `dev.openchamber.desktop` | `rust/oc-tauri/src-tauri/tauri.conf.json:5` | `com.yourapp.desktop` |
| APNs Bundle ID `com.openchamber.app` | `rust/oc-server/src/notifications/mod.rs:80` | `com.yourapp.app` |
| 实例名 `Local OpenChamber` | `rust/oc-tauri/src-tauri/src/tray.rs:1401,1428,1489,1491,1523` | `Local GridForge` 或新名（注意：当前仍是 `"Local OpenChamber"`，**尚未**从之前重命名中更新） |

### 2.2 数据目录（高优先级）

| 路径 | 引用文件数 | 替换建议 |
|------|-----------|---------|
| `~/.config/openchamber/` 硬编码 | ~15 个源文件 | `~/.config/yourapp/` |
| `user_config_root()` 不读 env | `rust/oc-server/src/github/settings.rs:27` | 改为读 `YOURAPP_CONFIG_DIR` env |

需要修改的关键文件：
- `rust/oc-server/src/github/settings.rs` — `user_config_root()`, `data_dir()`
- `rust/oc-server/src/state.rs` — AppState 中硬编码路径
- `rust/oc-server/src/behavior.rs` — settings 路径
- `rust/oc-server/src/fs/workspace.rs` — 测试中的硬编码路径
- `rust/oc-server/src/scheduled_tasks/project_config.rs`
- `rust/oc-server/src/session_goal/objectives.rs`
- `rust/oc-server/src/quota/credentials/store.rs`
- `rust/oc-server/src/resolution_routes.rs`
- `rust/oc-server/src/git/identity.rs`
- `rust/oc-tauri/src-tauri/src/settings.rs`
- `rust/oc-tauri/src-tauri/src/ssh/mod.rs:1306`
- `rust/oc-tauri/src-tauri/src/lib.rs`

### 2.3 环境变量（高优先级）

| 当前名 | 替换名 |
|--------|--------|
| `OPENCHAMBER_DATA_DIR` | `YOURAPP_DATA_DIR` |
| `OPENCHAMBER_PORT` | `YOURAPP_PORT` |
| `OPENCHAMBER_HOST` | `YOURAPP_HOST` |
| `OPENCHAMBER_SIDECAR` | `YOURAPP_SIDECAR` |
| `OPENCHAMBER_API_ONLY` | `YOURAPP_API_ONLY` |
| `OPENCHAMBER_UI_PASSWORD` | `YOURAPP_UI_PASSWORD` |
| `OPENCHAMBER_DIST_DIR` | `YOURAPP_DIST_DIR` |
| `OPENCHAMBER_HMR_UI_URL` | `YOURAPP_HMR_UI_URL` |
| `OPENCHAMBER_OPENCODE_HOSTNAME` | 保留或重命名 |
| `OPENCHAMBER_ALLOW_UNAUTHENTICATED_LAN` | `YOURAPP_ALLOW_UNAUTH_LAN` |
| `OPENCHAMBER_REQUIRE_CLIENT_AUTH` | `YOURAPP_REQUIRE_CLIENT_AUTH` |
| `OPENCHAMBER_TERMINAL_SHELL` | `YOURAPP_TERMINAL_SHELL` |
| `OPENCHAMBER_FS_EXEC_TIMEOUT_MS` | `YOURAPP_FS_EXEC_TIMEOUT` |
| `OPENCHAMBER_GIT_BINARY` | `YOURAPP_GIT_BINARY` |
| `OPENCHAMBER_RELAY_URL` | （删除，见 §4.1） |
| `OPENCHAMBER_PUSH_RELAY_URL` | （删除，见 §4.2） |
| `OPENCHAMBER_PUSH_RELAY_DISABLED` | （删除） |
| `OPENCHAMBER_VAPID_SUBJECT` | `YOURAPP_VAPID_SUBJECT` |
| `OPENCHAMBER_PUBLIC_ORIGIN` | `YOURAPP_PUBLIC_ORIGIN` |
| `OPENCHAMBER_APNS_*` | `YOURAPP_APNS_*` |
| `OPENCHAMBER_GITHUB_*` | `YOURAPP_GITHUB_*` |
| `OPENCHAMBER_RUNTIME` | 保留或重命名 |
| `OPENCHAMBER_ALLOW_REMOTE_OPENAI_COMPAT_URLS` | `YOURAPP_ALLOW_REMOTE_OPENAI` |
| `OPENCHAMBER_SSH_ASKPASS_VALUE` | `YOURAPP_SSH_ASKPASS` |
| `OPENCHAMBER_OPENCODE_HOSTNAME` | 保留或重命名 |

### 2.4 IPC / SSE 事件名称（中优先级）

事件名对 Web UI 无影响（UI 也会被替换），但如果保留 Web UI 兼容则需更改：

| 事件 | 位置 |
|------|------|
| `openchamber:emit` | `ipc/globals.rs` |
| `openchamber:update-progress` | `updater.rs` |
| `openchamber:menu-action` | `tray.rs`, `menu.rs` |
| `openchamber:tray-action` | `tray.rs` |
| `openchamber:open-session` | `tray.rs` |
| `openchamber:open-mini-chat` | `tray.rs` |
| `openchamber:vibrancy-ready` | `lib.rs` |
| `openchamber:check-for-updates` | `menu.rs` |
| `openchamber:ssh-instance-status` | `ssh/mod.rs` |
| `openchamber:event-stream-ready` | `scheduled_tasks/routes.rs` |
| `openchamber:heartbeat` | `scheduled_tasks/routes.rs`, `realtime/ws_bridge.rs` |
| `openchamber:session-status` | `realtime/global_hub.rs`, `realtime/ws_bridge.rs`, `notifications/session_state.rs` |
| `openchamber:session-activity` | `realtime/global_hub.rs`, `realtime/ws_bridge.rs`, `notifications/session_state.rs` |
| `openchamber:notification` | `notifications/emitter.rs` |
| `openchamber:notification-stream-ready` | `notifications/routes.rs` |
| `openchamber:permission-auto-accept.updated` | `permission_auto_accept.rs` |

### 2.5 API 路由（中优先级）

| 路由 | 位置 |
|------|------|
| `/api/openchamber/relay/*` | `relay/routes.rs` + `lib.rs` |
| `/api/openchamber/tunnel/*` | `tunnels/routes.rs` + `lib.rs` |
| `/api/openchamber/events` | `lib.rs` |
| `/api/openchamber/scheduled-tasks/*` | `scheduled_tasks/routes.rs` |
| `/api/openchamber/realtime-proxy/*` | `ui_auth/types.rs` 白名单 |

### 2.6 JS 注入变量（中优先级）

| 变量 | 位置 |
|------|------|
| `__OPENCHAMBER_LOCAL_ORIGIN__` | `ipc/globals.rs` |
| `__OPENCHAMBER_API_BASE_URL__` | `ipc/globals.rs`, `lib.rs` |
| `__OPENCHAMBER_CLIENT_TOKEN__` | `ipc/globals.rs` |
| `__OPENCHAMBER_HOME__` | `ipc/globals.rs` |
| `__OPENCHAMBER_RELAY_HOST_ID__` | `ipc/globals.rs` |
| `__OPENCHAMBER_RUNTIME_HEADERS__` | `ipc/globals.rs` |
| `__OPENCHAMBER_MACOS_MAJOR__` | `ipc/globals.rs` |
| `__OPENCHAMBER_PLATFORM__` | `ipc/globals.rs` |
| `__OPENCHAMBER_ELECTRON__` | `ipc/globals.rs` (Tauri 兼容桩) |
| `__OPENCHAMBER_DESKTOP__` | `ipc/globals.rs` |
| `__OPENCHAMBER_DESKTOP_BOOT_OUTCOME__` | `ipc/globals.rs` |

### 2.7 Tauri 命令 / Sidecar / 其他（中优先级）

| 项 | 位置 | 说明 |
|----|------|------|
| `openchamber_invoke` | `lib.rs`, `ipc/mod.rs`, `ipc/globals.rs` | IPC 分派函数 |
| `openchamber_dialog_open` | `lib.rs`, `ipc/dialog_cmd.rs`, `ipc/globals.rs` | 对话框 |
| `openchamber_file_grant` | `lib.rs`, `ipc/dialog_cmd.rs`, `ipc/globals.rs` | 文件授权 |
| Sidecar 二进制名 "openchamber" | `sidecar.rs` | Sidecar 进程 |
| NPM 包 `@openchamber/web` | `ssh/mod.rs` | 远程安装引用 |
| `openchamber-ui://` 协议 | `ipc/dialog_cmd.rs`, `ipc/mod.rs`, `middleware/cors.rs` | 自定义协议 URI |
| `x-openchamber-*` HTTP 头 | `relay/tunnel_host.rs`, `preview/routes.rs`, `git/routes.rs` | 自定义头 |
| `".openchamber.backup"` 后缀 | `opencode/auth.rs`, `opencode/config.rs` | 备份文件 |
| `metadata.openchamber.*` 命名空间 | `session_assist/`, `session_goal/`, `scheduled_tasks/` | 会话元数据 |
| Preview Bridge ID `openchamber-preview-bridge` | `preview/mod.rs` | Script ID |
| 多部分 boundary `OpenChamberSTTBoundary7MA4YWxkTrZu0gW` | `tts/stt.rs` | STT multipart |
| User-Agent "opencode/1.0 openchamber" | `small_model/call.rs` | HTTP User-Agent |
| GitHub Client UA "openchamber" | `github/client.rs` | GitHub API User-Agent |

### 2.8 WebAuthn RP ID（注意保留策略）

```rust
// 文件: ui_auth/passkeys.rs:285
.start_passkey_registration(user_uuid, "openchamber-ui", "GridForge UI", exclude)
```

**⚠️ 重要**: 修改 RP ID 会**使所有现有 passkey 失效**。如果这是重命名而非完全新应用，应考虑保留或渐进过渡策略。

### 2.9 响应体字段

| 字段 | 位置 |
|------|------|
| `"openchamberVersion"` | `routes.rs:64,91,103` |
| `"openchamberLatestVersion"` | (如果保留更新检查) |

---

## 3. 品牌重命名范围 — 前端

> **强制项**: 前端的所有 `__OPENCHAMBER_*__` 注入变量、`/api/openchamber/*` 路由前缀、
> `openchamber://` deep-link scheme、settings schema key、`openchamber:…` 事件名、
> localStorage/zustand-persist 命名空间、`metadata.openchamber.*` 命名空间都必须与
> Rust 后端同步修改。前端是后端契约的直接消费者，改后端而不改前端会导致 UI 直接挂掉。

### 3.1 运行时注入变量读取点（高优先级）

每个变量必须与 `rust/oc-tauri/src-tauri/src/ipc/globals.rs` 中的写入保持一一对应。

| 变量 | 主要读取位置 |
|------|------------|
| `__OPENCHAMBER_HOME__` | `packages/ui/src/stores/useDirectoryStore.ts:123`, `packages/ui/src/lib/persistence.ts:18,23`, `packages/ui/src/lib/openchamberConfig.ts:210`, `packages/ui/src/lib/desktop.ts:593`, `packages/ui/src/components/mini-chat/MiniChatLayout.tsx:34` |
| `__OPENCHAMBER_API_BASE_URL__` | `packages/ui/src/lib/runtime-url.ts:38,44`, `packages/ui/src/lib/runtime-fetch.ts:245`, `packages/web/src/runtimeConfig.ts:19,20,32,33,38,39,51`, `packages/web/vite.config.ts:43,55,56`（注入源） |
| `__OPENCHAMBER_CLIENT_TOKEN__` | `packages/ui/src/lib/runtime-auth.ts:47,53,115`, `packages/ui/src/lib/runtime-switch.ts:24,26,28,55,61`, `packages/web/src/runtimeConfig.ts:21,35` |
| `__OPENCHAMBER_RUNTIME_HEADERS__` | `packages/ui/src/lib/runtime-switch.ts:105,106,107,109,110,111`, `packages/web/src/runtimeConfig.ts:22,36` |
| `__OPENCHAMBER_LOCAL_ORIGIN__` | `packages/ui/src/lib/runtime-url.ts:126`, `packages/ui/src/stores/useProjectsStore.ts:80`, `packages/ui/src/components/auth/SessionAuthGate.tsx:46`, `packages/ui/src/components/layout/Header.tsx:899`, `packages/ui/src/components/desktop/DesktopHostSwitcher.tsx:88`, `packages/ui/src/hooks/useTraySync.ts:201`, `packages/ui/src/hooks/useWindowTitle.ts:70`, `packages/ui/src/lib/desktop.ts:504` |
| `__OPENCHAMBER_RUNTIME_APIS__` | `packages/ui/src/main.tsx:20,24`, `packages/web/src/main.tsx:12,17,110`, `packages/web/src/mini-chat-main.tsx:8,12,16`, `packages/web/src/mobile-main.tsx:8,12,16`, `packages/ui/src/contexts/runtimeAPIRegistry.ts:18,19` |
| `__OPENCHAMBER_DESKTOP__` | `packages/ui/src/lib/desktop.ts:234`, `packages/ui/src/lib/desktopSsh.ts:433`, `packages/ui/src/lib/url.ts:122`, `packages/ui/src/hooks/useMenuActions.ts:343`, `packages/ui/src/hooks/useTraySync.ts:582`, `packages/web/src/api/notifications.ts:187,217` |
| `__OPENCHAMBER_ELECTRON__` | `packages/ui/src/lib/desktop.ts:229,241,259,261`, `packages/ui/src/lib/debug.ts:224`, `packages/ui/src/lib/theme/cssGenerator.ts:200,201`, `packages/ui/src/components/layout/ContextPanel.tsx:1298`, `packages/ui/src/components/sections/openchamber/OpenChamberVisualSettings.tsx:365,368`（Tauri 兼容桩，移除 Electron 后可删除） |
| `__OPENCHAMBER_PLATFORM__` | `packages/ui/src/lib/desktop.ts:249`, `packages/ui/src/lib/openInApps.ts:38`, `packages/ui/src/hooks/useTraySync.ts:104`, `packages/ui/src/components/desktop/OpenInAppButton.tsx:27`, `packages/ui/src/components/sections/openchamber/OpenChamberVisualSettings.tsx:170,171,377`, `packages/ui/src/components/views/SettingsView.tsx:335,339` |
| `__OPENCHAMBER_MACOS_MAJOR__` | `packages/ui/src/components/layout/Header.tsx:800`, `packages/ui/src/components/mini-chat/MiniChatLayout.tsx:71`, `packages/ui/src/components/multirun/MultiRunLauncher.tsx:198`, `packages/ui/src/lib/openCodeStatus.ts:282` |
| `__OPENCHAMBER_DESKTOP_BOOT_OUTCOME__` | `packages/ui/src/lib/desktopBoot.ts:5,276,277,296,297`, `packages/ui/src/App.tsx:755` |
| `__OPENCHAMBER_RELAY_HOST_ID__` | `packages/web/src/runtimeConfig.ts:63`（随 relay 模块一起删除） |
| `__OPENCHAMBER_WIDGET_SNAPSHOT__` | `packages/ui/src/apps/mobileWidgetSnapshot.ts:12,103` |
| `__OPENCHAMBER_CSP_NONCE__` | `packages/ui/src/components/ui/CodeMirrorEditor.tsx:363` |
| `__OPENCHAMBER_SURFACE__` | `packages/ui/src/lib/runtimeSurface.ts:7,26`, `packages/web/src/main.tsx:13,40` |
| `__OPENCHAMBER_STARTUP_TRACE__` / `__STARTUP_TRACE_START__` / `__STARTUP_TRACE_SUMMARY__` | `packages/ui/src/lib/startupTrace.ts:9-47`, `packages/ui/src/App.tsx:212` |
| `__OPENCHAMBER_VSCODE_SHIKI_THEMES__` | `packages/ui/src/types/vscode.d.ts:3` |
| `__OPENCHAMBER_VSCODE_THEME__` | `packages/ui/src/contexts/ThemeSystemContext.tsx:171,302` |
| `__OPENCHAMBER_PANEL_TYPE__` | `packages/ui/src/apps/VSCodeApp.tsx:30,46` |
| `__OPENCHAMBER_CONNECTION__` | `packages/ui/src/components/layout/VSCodeLayout.tsx:175,336`, `packages/ui/src/components/views/agent-manager/AgentManagerView.tsx:24,53` |
| `__OPENCHAMBER_SET_PWA_INSTALL_NAME__` / `__OPENCHAMBER_SET_PWA_ORIENTATION__` / `__OPENCHAMBER_UPDATE_PWA_MANIFEST__` | `packages/ui/src/components/sections/openchamber/OpenChamberVisualSettings.tsx:133,134,135,643,644,650,663,664,670`, `packages/ui/src/hooks/usePwaManifestSync.ts:13,89` |
| 类型声明 | `packages/ui/src/types/desktop.d.ts:5-10`（所有全局变量类型签名集中处） |

**其他 camelCase 全局函数**（后端 init script 注入）：
- `__openchamberSetEmbeddedVisibility` — `packages/ui/src/App.tsx:548,551,556,557`
- `__openchamberDesktopBrowserCancelInspect` — `packages/ui/src/components/layout/ContextPanel.tsx:317,318,372,373,394,401,402`
- `__openchamber_sync_context__` — `packages/ui/src/sync/sync-context.tsx:65`

### 3.2 Deep-link scheme（高优先级）

| 位置 | 说明 |
|------|------|
| `packages/ui/src/apps/deepLinks.ts:13` | `DEEP_LINK_SCHEME = 'openchamber'` — 单一来源，需改为新品牌 |
| `packages/ui/src/apps/deepLinkNavigation.ts:7,101` | 解析 + 应用 |
| `packages/ui/src/apps/MobileApp.tsx:70,701,2177,2180,2203,2836,2997` | handler 注册 |
| `packages/ui/src/apps/mobileQrScan.ts:3,118` | QR 扫描解析 |
| `packages/ui/src/lib/connectionPayload.ts:196` | `openchamber://connect?...` pairing payload 构造 |
| `packages/web/bin/lib/commands-connect-url.js:125` | CLI 生成 pairing URL |
| `packages/web/bin/lib/cli-args.js:458` | CLI 解析 |

### 3.3 API 路由前缀（高优先级）

| 路由 | 位置 |
|------|------|
| `/api/openchamber/models-metadata` | `packages/ui/src/stores/useConfigStore.ts:26` |
| `/api/openchamber/update-check` | `packages/ui/src/stores/useUpdateStore.ts:120`, `packages/ui/src/components/ui/UpdateDialog.tsx:164`, `packages/ui/src/components/layout/Header.tsx:939` |
| `/api/openchamber/update-install` | `packages/ui/src/components/ui/UpdateDialog.tsx:125` |
| `/api/openchamber/tunnel/*` | `packages/ui/src/components/sections/openchamber/TunnelSettings.tsx:499,526,527,529,743,948,1043,1044` |
| `/api/openchamber/realtime-proxy/{sse,ws}` | `packages/ui/src/lib/runtime-url.ts:126` |
| `/api/openchamber/events` | `packages/ui/src/lib/openchamberEvents.ts:136`（EventSource） |

### 3.4 localStorage / zustand-persist / IndexedDB 命名空间（高优先级）

⚠️ **保留 vs 重命名的判断**：
- 如果**没有用户数据迁移路径** → 直接重命名（旧数据会被自然淘汰）
- 如果**有用户数据迁移路径** → 保留旧 key 在读取端做回退，新写入使用新 key

| Key | 位置 |
|-----|------|
| `openchamber.pwaName` | `packages/ui/src/lib/persistence.ts:100,102`, `packages/web/index.html:29` |
| `openchamber.pwaOrientation` | `packages/web/index.html:30` |
| `openchamber.mobileKeyboardMode` | `packages/web/index.html:31` |
| `openchamber.pwaRecentSessions` | `packages/web/index.html:32` |
| `openchamber.i18n.v1` | `packages/ui/src/lib/i18n/runtime.ts:20` |
| `openchamber-mobile-layout` | `packages/ui/src/lib/mobileLayoutPreference.ts:3` |
| `openchamber.mobile.connections.v1` | `packages/ui/src/apps/mobileConnections.test.ts:34`（生产代码 `mobileConnections.ts`） |
| `openchamber-notification-claim:` | `packages/web/src/api/notifications.ts:5` |
| `openchamber_stream_debug` | `packages/ui/src/stores/utils/streamDebug.ts:4`, `packages/ui/src/components/layout/VSCodeLayout.tsx:389` |
| `openchamber:sync:debug` | `packages/ui/src/sync/debug.ts:5,8,13` |
| `openchamber-session-todos` (zustand) | `packages/ui/src/stores/useTodosPersistStore.ts:56` |
| `persist:openchamber-browser` (partition) | `packages/ui/src/components/layout/ContextPanel.tsx:2038` |

### 3.5 自定义 DOM/事件名 `openchamber:…`（中优先级）

完整事件名清单（约 55 个，按 `openchamber:` 前缀的 union 类型）：

集中定义/分发的位置：
- `packages/ui/src/App.tsx`（注册 + dispatch：`openchamber:embedded-visibility`、`openchamber:open-session`、`openchamber:open-mini-chat`、`openchamber:open-draft-session`、`openchamber:open-project`、`openchamber:app-ready`）
- `packages/ui/src/sync/sync-context.tsx`、`sync/event-pipeline.ts`（`openchamber:session-status`、`openchamber:session-activity`、`openchamber:heartbeat`、`openchamber:system-resume`、`openchamber:catch-up` 等）
- `packages/ui/src/lib/openchamberEvents.ts`（全局事件 helper）
- `packages/ui/src/contexts/ThemeSystemContext.tsx`（`openchamber:theme-sync`、`openchamber:vscode-theme`）
- `packages/ui/src/hooks/useTraySync.ts`、`useMenuActions.ts`、`useKeyboardShortcuts.ts`
- `packages/ui/src/components/chat/ChatContainer.tsx`、`CommandAutocomplete.tsx`、`StatusRow.tsx`、`ChatInput.tsx`、`MessageBody.tsx`、`TextSelectionMenu.tsx`
- `packages/ui/src/components/layout/ContextPanel.tsx`、`VSCodeLayout.tsx`、`Header.tsx`、`terminal/TerminalViewport.tsx`、`desktop/WindowsWindowControls.tsx`
- `packages/ui/src/components/sections/openchamber/NotificationSettings.tsx`（`tag: 'openchamber-test'`）
- `packages/ui/src/components/update/OpenCodeUpdateToast.tsx`
- `packages/ui/src/stores/useOpenInAppsStore.ts`、`useProjectsStore.ts`
- `packages/ui/src/components/sections/plugins/PluginsSidebar.tsx`

完整事件名：app-ready, catch-up, chat-force-scroll-bottom, chat-scroll-to-message, chat-settings-request, chat-settings-sync, check-for-updates, chunk-import-reload, compact, connection-status, copy, craft-goal, cycle-theme-request, debug, dictation-toggle, embedded-visibility, event-stream-ready, explore, file-viewer-preview-mode-changed, handoff-review, heartbeat, init, installed-apps-updated, menu-action, mini-chat-presence, navigate, notification, notification-stream-ready, open-draft-session, open-mini-chat, open-project, open-session, opencode-update-available, permission-auto-accept.updated, plan-feature, project-actions-updated, project-notes-updated, project-plan-saved, redo, runtime-endpoint-changed, scheduled-task-ran, session-activity, session-status, settings-open-plugin-add, settings-synced, ssh-instance-status, summary, system-resume, theme-sync, timeline, tray-action, undo, update-progress, vscode-notification-event, vscode-theme, weigh, window-maximized-changed, window-resized, workspace-review。

> **后端 Rust 同步点**：`openchamber:emit`、`openchamber:menu-action`、`openchamber:tray-action`、`openchamber:open-session`、`openchamber:open-mini-chat`、`openchamber:vibrancy-ready`、`openchamber:check-for-updates`、`openchamber:ssh-instance-status`、`openchamber:event-stream-ready`、`openchamber:heartbeat`、`openchamber:session-status`、`openchamber:session-activity`、`openchamber:notification`、`openchamber:notification-stream-ready`、`openchamber:permission-auto-accept.updated`、`openchamber:update-progress`、`openchamber:event-stream-ready`（见 §2.4）。前后端必须同步。

### 3.6 自定义 DOM 属性 / DnD mime / 内嵌链接前缀（中优先级）

| 标识 | 位置 |
|------|------|
| `application/x-openchamber-file-path` (DnD mime) | `packages/ui/src/components/chat/ChatInput.tsx:3471,3598,3652`, `packages/ui/src/components/layout/SidebarFilesTree.tsx:351,1267` |
| `data-openchamber-file-link` / `-file-ref` / `-file-path` / `-block-path-token` / `-block-paths-scanned` | `packages/ui/src/components/chat/MarkdownRendererImpl.tsx:87,143,144,146,271,276,457,458,459,542,543,544,555,617`, `packages/ui/src/components/chat/markdown/decorate.ts:495` |
| `data-openchamber-agent-mention` | `packages/ui/src/components/chat/markdown/markdownCore.ts:173` |
| `#openchamber-skill:` / `#openchamber-agent:` | `packages/ui/src/lib/messages/inlineMessageLinks.ts:1,2`, 测试 `UserTextPart.test.ts:48,49` |

### 3.7 Settings i18n 命名空间 `settings.openchamber.*`（中优先级）

⚠️ **保留策略讨论**：
- 这是 i18n key 的**逻辑命名空间**，不是存储 JSON schema 的 key。
- 重命名会**丢失所有现有用户翻译**（已翻译成 9 种语言）。
- 建议：**保留 i18n key 名不变**（仅改 settings UI 中显示给用户的标签），让内部 key 与产品品牌脱钩。

涉及位置：
- `packages/ui/src/lib/settings/search.ts` — 100+ 个 `titleKey/descriptionKey`（行 40-840 跨度）
- `packages/ui/src/lib/i18n/messages/{en,zh-CN,zh-TW,ja,ko,fr,es,pt-BR,pl,uk}.settings.ts` — 共 ~3369 次出现
- `packages/ui/src/components/sections/openchamber/AboutSettings.tsx:113` — `settings.openchamber.about.toast.latestVersion`

涉及的命名空间分组：
- `settings.openchamber.visual.*` — `search.ts` 多行
- `settings.openchamber.defaults.*`
- `settings.openchamber.sessionRetention.*`
- `settings.openchamber.desktopNetwork.*`
- `settings.openchamber.desktopPassword.*`
- `settings.openchamber.opencodeCli.*`
- `settings.openchamber.git.*`
- `settings.openchamber.worktrees.*`
- `settings.openchamber.keyboardShortcuts.*`
- `settings.openchamber.tunnel.*`
- `settings.openchamber.about.*`

### 3.8 OpenCode session metadata 命名空间 `metadata.openchamber.*`（中优先级）

⚠️ **重要**: 这部分数据**已经持久化在服务端 session 历史**中（OpenCode server 存），改 key 名后旧 session 的关联数据全部失效。

| 位置 | 字段 |
|------|------|
| `packages/ui/src/lib/sessionReviewMetadata.ts:20,44,58,77,79` | `metadata.openchamber.reviewSessionID` / `originalSessionID` / `kind` |
| `packages/ui/src/components/chat/SessionSuggestionChip.tsx:40` | `metadata.openchamber.assist` |
| `packages/ui/src/hooks/useSessionGoal.ts` | `metadata.openchamber.goal`（隐式） |
| 测试 fixture | `packages/ui/src/stores/globalSessions.test.ts:19,49`, `packages/ui/src/sync/sanitize.test.ts:67,96` |

> **决策点**：是迁移数据（写时迁移 + 读时回退），还是直接断舍离（旧 session 失去 review 关联）？

### 3.9 主题 / PWA manifest / Logo（低优先级）

| 标识 | 位置 |
|------|------|
| `openchamberLightTheme` / `openchamberDarkTheme` | `packages/ui/src/lib/theme/themes/index.ts:6,7,11,12,18,19,23` |
| 主题 ID `'openchamber-light'` / `'openchamber-dark'` | `packages/ui/src/lib/theme/themes/index.ts`、`fields-of-the-shire-{light,dark}.json:3` |
| PWA manifest `name` / `short_name` / `description` | `packages/web/public/site.webmanifest:2,3,4` |
| `<svg id="openchamber-icon-sprite">` | `packages/ui/src/components/icon/README.md:58` |
| `OpenChamberLogo` 组件 | `packages/ui/src/components/ui/OpenChamberLogo.tsx:4,12,15`，被 `AboutDialog`、`ConfigUpdateOverlay`、`ChatEmptyState`、`ContextPanel`、`SessionAuthGate`、`AboutSettings` 引用 |

### 3.10 Capacitor / 移动端（高优先级）

#### Capacitor 配置
- `packages/mobile/capacitor.config.ts:4` — `appId: 'com.openchamber.app'`（appName 已是 `GridForge`）

#### iOS
- Bundle ID：`com.openchamber.app` 出现于 `packages/mobile/ios/App/App.xcodeproj/project.pbxproj:565,590`（主 app）、`:614,639`（widget）、`:663,688`（notification service）
- App Group：`group.com.openchamber.app` 出现于 `App.entitlements:15`、`OpenChamberNotificationService.entitlements:8`、`OpenChamberWidget.entitlements:8`、`AppDelegate.swift:113`、`NotificationService.swift:11`、`WidgetShared.swift:27,46-55`
- CFBundleDisplayName：`<string>OpenChamber</string>` — `packages/mobile/ios/App/App/Info.plist:8`
- URL scheme：`openchamber` + `com.openchamber.app.deeplink` — `Info.plist:46,49`
- Widget deep-link：`openchamber://new|session/...|status|...` — `WidgetShared.swift:27,46-55`、`OpenChamberControl.swift:36`
- 三个 NSUsageDescription 提及 "OpenChamber" — `Info.plist:35,37,39`

#### Android
- `namespace "com.openchamber.app"`、`applicationId "com.openchamber.app"` — `packages/mobile/android/app/build.gradle:12,15`
- `package com.openchamber.app;` — `packages/mobile/android/app/src/main/java/com/openchamber/app/MainActivity.java:1`
- `google-services.json:4,5,12` — `project_id: "openchamber-8bf7e"`、`storage_bucket`、`package_name`（重新生成 google-services.json 即可）
- `strings.xml:3,4,5,6` — `app_name="OpenChamber"`、`title_activity_main="OpenChamber"`、`package_name`、`custom_url_scheme="com.openchamber.app"`

#### 脚本
- `packages/mobile/scripts/ios-sim.mjs:5` — `BUNDLE_ID = 'com.openchamber.app'`
- `packages/mobile/scripts/android-device.mjs:14` — `APP_ID = 'com.openchamber.app'`

> **重要**: 发布后 Bundle ID 一旦更改，App Store / Play Store / 推送 Token / Keychain Group 全部失效。需要在第一次发版前完成。

### 3.11 配置持久化文件路径（中优先级）

| 路径 | 位置 |
|------|------|
| `~/.config/openchamber/<projectId>.json` | `packages/ui/src/lib/openchamberConfig.ts:3,20,555-721` |
| `<project>/.openchamber/openchamber.json` (legacy) | `packages/ui/src/lib/openchamberConfig.ts:4,17,19,599,600` |
| `USER_PROJECTS_DIR_SEGMENTS = ['.config','openchamber','projects']` | `packages/ui/src/lib/openchamberConfig.ts:20` |
| `CONFIG_FILENAME = 'openchamber.json'` | `packages/ui/src/lib/openchamberConfig.ts:17` |
| `LEGACY_CONFIG_DIR = '.openchamber'` | `packages/ui/src/lib/openchamberConfig.ts:19` |

### 3.12 TS 类型 / 函数名（中优先级）

所有 `OpenChamber*` / `openchamber*` 开头的 TS 类型与函数名（包括 `readOpenChamberConfig`、`fetchOpenChamberDefaults`、`loadOpenChamberVersion`、`markSessionAsOpenChamberCreated` 等）：

集中位置：
- `packages/ui/src/lib/openchamberConfig.ts:1-734`（核心模块）
- `packages/ui/src/lib/sessionReviewMetadata.ts:5-79`
- `packages/ui/src/lib/openchamberEvents.ts:13`
- `packages/ui/src/lib/openCodeStatus.ts:17,34,162,175,184,216,262`
- `packages/ui/src/stores/useConfigStore.ts:53,69,72`
- `packages/ui/src/stores/useMultiRunStore.ts:54`
- `packages/ui/src/stores/useProjectsStore.ts:493`
- `packages/ui/src/stores/useAgentsStore.ts:121`
- `packages/ui/src/components/sections/openchamber/{types.ts, AboutSettings.tsx, OpenChamberPage.tsx, OpenChamberVisualSettings.tsx}`
- `packages/ui/src/components/sections/remote-instances/RemoteInstancesPage.tsx:109,111`
- `packages/ui/src/components/sections/projects/ProjectActionsSection.tsx:31,47`
- `packages/ui/src/components/layout/ProjectActionsButton.tsx:23,183,261,388,435,453,607,673`
- `packages/ui/src/components/session/ProjectNotesTodoPanel.tsx` 多行
- `packages/ui/src/components/session/GitHubIssuePickerDialog.tsx:424`
- `packages/ui/src/components/session/NewWorktreeDialog.tsx:900`
- `packages/ui/src/components/views/PlanView.tsx:154,576,636`
- `packages/ui/src/components/views/SettingsView.tsx:37,38,478,823`
- `packages/ui/src/components/chat/CommandAutocomplete.tsx:24,154-259,370,448`
- `packages/ui/src/components/sections/providers/{ProvidersSidebar.tsx:65, ProvidersPage.tsx:296}`
- `packages/ui/src/lib/detectDevServer.ts:1,34,99`
- `packages/ui/src/lib/worktreeSessionCreator.ts:145,329`

### 3.13 散落硬编码 "OpenChamber" 字符串（低优先级）

| 位置 | 内容 |
|------|------|
| `packages/ui/src/hooks/useTraySync.ts:77,196,200,204` | `"Local OpenChamber"`（tray 菜单文字，与 `rust/oc-tauri/src-tauri/src/tray.rs:1401` 对应） |
| `packages/ui/src/lib/openCodeStatus.ts:262` | `console.log("OpenChamber version: ${appVersion}")` |
| `packages/ui/src/lib/worktreeSessionCreator.ts:329` | 错误消息 `'Project is not registered in OpenChamber'` |
| `packages/ui/src/lib/magicPrompts.ts:627,640` | 系统提示文案 |
| `packages/ui/src/stores/useProjectsStore.ts:493` | `console.log` 前缀 `[OpenChamber][VSCode][projects]` |
| `packages/ui/src/components/layout/VSCodeLayout.tsx:395,417` | `console.log` 前缀 |
| `packages/ui/src/components/sections/openchamber/` | 整目录名 + 子文件都带 openchamber 前缀 |
| `packages/ui/src/components/session/sidebar/SidebarFooter.tsx:57,61` | i18n key `sessions.sidebar.footer.actions.aboutOpenChamber` |
| `packages/ui/src/components/sections/remote-instances/RemoteInstancesPage.tsx:109,111` | i18n key `settings.remoteInstances.page.phase.installingOpenChamber/updatingOpenChamber` |
| i18n 文件 | `updateDialog.error.takingLonger` 在 10 语言中含 `openchamber update` CLI 命令文案 |
| i18n 文件 | `settings.remoteInstances.direct.import.placeholder` = `'openchamber://connect?...'` 在 8+ 语言 |
| i18n 文件 | `settings.openchamber.opencodeCli.tipMiddle` 在多语言含 "OpenChamber" 字样 |

### 3.14 前端测试 fixture（中优先级）

| 文件 | 内容 |
|------|------|
| `packages/ui/src/lib/runtime-url.test.ts` 多行 | `__OPENCHAMBER_API_BASE_URL__`、`__OPENCHAMBER_LOCAL_ORIGIN__`、`openchamber-ui://app` fake origin、`/api/openchamber/...` |
| `packages/ui/src/lib/runtime-fetch.test.ts:37,40` | `openchamber-ui://app` |
| `packages/ui/src/lib/runtime-switch.test.ts:22,27,32` | 运行时注入变量 |
| `packages/ui/src/lib/runtime-auth.test.ts:50` | `__OPENCHAMBER_CLIENT_TOKEN__` |
| `packages/ui/src/lib/desktopBoot.test.ts` 多行 | `__OPENCHAMBER_DESKTOP_BOOT_OUTCOME__` |
| `packages/ui/src/lib/desktopHosts.test.ts:9` | `__OPENCHAMBER_DESKTOP__` |
| `packages/ui/src/lib/persistence.test.ts` 多行 | `__OPENCHAMBER_HOME__` |
| `packages/ui/src/lib/connectionPayload.test.ts` 多行 | `openchamber://connect?v=2&p=…` |
| `packages/ui/src/lib/gitApi.test.ts:19` | `__OPENCHAMBER_RUNTIME_APIS__` |
| `packages/ui/src/components/auth/SessionAuthGate.behavior.test.tsx:186,187` | mock `@/components/ui/OpenChamberLogo` |
| `packages/ui/src/components/chat/message/parts/JsonSummaryView.test.tsx:14,21` | `linear.app/openchamber/issue/…` |
| `packages/ui/src/components/chat/message/parts/UserTextPart.test.ts:48,49` | `#openchamber-agent:…`、`#openchamber-skill:…` |
| `packages/ui/src/stores/globalSessions.test.ts:19,49` | `openchamber: {...}` metadata fixture |
| `packages/ui/src/stores/useMultiRunStore.test.ts:39,91` | `markSessionAsOpenChamberCreated` mock |
| `packages/ui/src/sync/sanitize.test.ts:67,96` | `metadata.openchamber` fixture |
| `packages/ui/src/sync/event-pipeline.test.ts:145,157` | `openchamber:session-status` fixture |
| `packages/ui/src/sync/__tests__/event-pipeline-resume.test.js:13,99` | `openchamber:system-resume` |
| `packages/ui/src/apps/mobileQrScan.test.ts:35-40` | `openchamber://connect` parse |
| `packages/ui/src/apps/mobileConnections.test.ts:34` | `STORAGE_KEY = 'openchamber.mobile.connections.v1'` |

### 3.15 postMessage type 字符串（中优先级）

| Type | 位置 |
|------|------|
| `openchamber:chat-settings-request` | `packages/ui/src/components/chat/ChatContainer.tsx:685`, `ContextPanel.tsx:2445`（监听） |
| `openchamber:chat-settings-sync` | `packages/ui/src/components/chat/ChatContainer.tsx:678`（监听），`ContextPanel.tsx:2389`（dispatch） |
| `openchamber:cycle-theme-request` | `packages/ui/src/hooks/useKeyboardShortcuts.ts:227`, `ContextPanel.tsx:2449`（监听） |

---

## 4. 服务依赖分析

### 4.1 🔴 中继服务 (Relay)

**OpenChamber 端点**:
- WebSocket: `wss://relay.openchamber.app/v1`
- HKDF 信息: `b"openchamber-relay-v1"`

**作用**: E2EE 加密隧道，允许远程客户端连接到本地 OpenChamber 实例。

**影响范围**:
- `rust/oc-server/src/relay/identity.rs:34` — `DEFAULT_RELAY_URL`
- `rust/oc-server/src/relay/crypto.rs:35` — `RELAY_HKDF_INFO`
- `rust/oc-server/src/relay/tunnel_host.rs` — 自定义头 `x-openchamber-relay-connection`, `x-openchamber-real`
- `rust/oc-server/src/relay/service.rs` — `OPENCHAMBER_RELAY_URL` env

| 选项 | 说明 | 复杂度 |
|------|------|--------|
| **A. 移除整个 relay 模块** | 删除 `relay/` 8 个文件 + 路由注册 + AppState | 低 |
| **B. 替换为中继端点** | 保留 relay 协议但指向自己的中继服务器 | 高（需搭建服务） |
| **C. 保留但可配置** | 保留代码但默认置空，用户自行配置 | 中 |

> **建议**: 选项 A（移除）。Relay 是 OpenChamber 服务中最中心的在线依赖。
> 除非你有自己的中继服务器，否则删除最干净。

### 4.2 🟠 推送通知中继

**OpenChamber 端点**:
- POST: `https://api.openchamber.dev/v1/push/send`
- Token 注册: `https://api.openchamber.dev/v1/push/register-token`

**作用**: 将 APNs / Web Push 的通知请求通过 OpenChamber 的中继服务器转发到 Apple/浏览器推送网络。

**影响范围**:
- `rust/oc-server/src/notifications/mod.rs:71` — `DEFAULT_RELAY_URL`
- `rust/oc-server/src/notifications/apns_send.rs` — relay 模式（默认）+ 直接模式（fallback）
- `rust/oc-server/src/notifications/push_send.rs` — Web Push (VAPID)
- `rust/oc-server/src/notifications/relay_key.rs` — ECDSA 签名密钥（与 relay 共享）

| 选项 | 说明 | 复杂度 |
|------|------|--------|
| **A. 移除推送中继** | 仅保留直接 APNs 模式、删除 relay URL 常量 | 中 |
| **B. 保留直接 APNs + Web Push** | 保留本地 push 功能，移除 relay | 低 |
| **C. 移除整个通知模块** | 如果不需要桌面通知 | 高（影响大） |

> **建议**: 方案 B。保留直接 APNs + Web Push (VAPID)，移除 relay 模式。
> 推送通知是桌面应用核心体验，但不需要经过 OpenChamber 中继。

### 4.3 🟠 模型元数据目录

**OpenChamber 端点**:
- `https://models.dev/api.json`

**作用**: 获取 AI 模型目录（可用模型、定价、能力元数据）。

**影响范围**:
- `rust/oc-server/src/opencode/models_metadata.rs:33`

| 选项 | 说明 | 复杂度 |
|------|------|--------|
| **A. 替换为自己的端点** | 搭建自己的模型目录服务 | 高 |
| **B. 移除** | 删除模型目录依赖，使用本地硬编码列表或配置 | 中 |
| **C. 保留但可配置** | 默认指向自己的 URL | 低 |

> **建议**: 方案 C（可配置）→ 后续过渡到 B 或 A。
> 先改为可配置的 `MODELS_API_URL`，后续再决定替换还是移除。

### 4.4 🟡 桌面更新

**OpenChamber 端点**:
- `https://github.com/openchamber/openchamber/releases/latest/download/latest.json`

**影响范围**:
- `rust/oc-tauri/src-tauri/tauri.conf.json:43`

| 选项 | 说明 | 复杂度 |
|------|------|--------|
| **A. 改为自己的更新源** | 搭建或使用 GitHub Releases | 低 |
| **B. 移除** | 删除更新插件，用户手动升级 | 低 |

> **建议**: 方案 A。改为指向自己的 GitHub 仓库 Releases。

### 4.5 🟡 GitHub OAuth 应用

**需要注册自己的 GitHub OAuth 应用**:
- `rust/oc-server/src/github/mod.rs:25` — `DEFAULT_GITHUB_CLIENT_ID`

**影响**: GitHub 集成功能（PR 状态、代码审查等）需要自己的 OAuth 应用注册。

> **建议**: 注册新的 GitHub OAuth App，替换默认 Client ID。

### 4.6 🟢 OpenCode Go 配额检查

**端点**: `https://opencode.ai/workspace/{workspaceId}/go`

**影响范围**:
- `rust/oc-server/src/quota/providers/opencode_go.rs`

> **建议**: 如果不用 OpenCode Go，直接移除此 provider。

### 4.7 🟡 预览/分类中的 OpenChamber URL（测试数据）

**测试引用**:
- `http://openchamber-preview.local` — `preview/rewrite.rs:51`
- `https://openchamber.dev` — `preview/classify.rs:396,399`
- `https://docs.openchamber.dev` — `preview/normalize.rs:176-177,256-257`

> **建议**: 替换为你的应用域名或示例 URL。

---

## 5. 功能裁剪决策

### 5.1 核心后端模块（建议保留）

这些模块是应用核心功能，与 OpenChamber 服务无关，**建议全保留**：

| 模块 | 说明 | 服务依赖 |
|------|------|---------|
| `text/` | 文本摘要 | 无 |
| `fs/` | 文件系统操作 | 无 |
| `git/` | Git 操作 | 无 |
| `github/` | GitHub 集成 | 需替换 Client ID |
| `terminal/` | PTY 终端 | 无 |
| `preview/` | 预览代理 | 无 |
| `tts/` | 文字转语音 | 无（STT 需 API key） |
| `small_model/` | 小模型调用 | 无 |
| `session_assist/` | 会话摘要 | 无 |
| `session_goal/` | 会话目标 | 无 |
| `permission_auto_accept/` | 权限自动接受 | 无 |
| `magic_prompts/` | 魔法提示词 | 无 |
| `session_folders/` | 会话文件夹 | 无 |
| `ui_auth/` | UI 认证 | 无（自包含） |
| `client_auth/` | 客户端认证 | 无（自包含） |
| `middleware/` | 中间件 | 无 |

### 5.2 有条件保留的模块

| 模块 | 保留条件 | 需要改动 |
|------|---------|---------|
| `tunnels/` | 如果需要 cloudflare/ngrok 隧道 | 无（不依赖 OpenChamber） |
| `notifications/` | 如果需要推送通知 | 移除 relay 模式，保留直接模式 |
| `quota/` | 如果保留配额管理 | 移除 `opencode_go` provider |
| `scheduled_tasks/` | 如果需要定时任务 | 无 |
| `skills_catalog/` | 如果需要技能市场 | 无 |

### 5.3 建议移除的模块

| 模块 | 原因 | 替代方案 |
|------|------|---------|
| `relay/` | 完全依赖 OpenChamber 中继服务 | 如有需要自行搭建中继 |
| `dictation/` local stub | 已经只是 stub（`local_models_unsupported`） | 保留 stub 或将来实现 native |
| `opencode_go` quota provider | 依赖 opencode.ai | 直接删除此 provider |

### 5.4 深度链接（Deep Link）

**当前状态**: Tauri 已初始化 `tauri-plugin-deep-link` 但事件处理为空 (TODO)。

**决策**:
- 更新协议名：`openchamber://` → `yourapp://`
- 实现事件处理（从 `<name>://session/xxx` 等解析并导航）
- 注册为系统的默认协议处理程序
- 前端 `DEEP_LINK_SCHEME` 同步更新（见 §3.2）
- Capacitor `Info.plist` + `strings.xml` 同步更新（见 §3.10）

> **建议**: 在品牌重命名 TODO 中一同修复，而非单独阶段。

---

## 6. 分阶段迁移计划

### 阶段 0 — 基础隔离（阻止冲突）

**目标**: 确保分叉应用可以与 OpenChamber **同时安装、互不干扰**。

| 步骤 | 内容 | 涉及文件 |
|------|------|---------|
| 0.1 | 修改 Bundle ID `dev.openchamber.desktop` → `com.yourapp.desktop` | `tauri.conf.json:5` |
| 0.2 | 确认产品名（当前为 `GridForge`），如不改则跳过 | `tauri.conf.json:3` |
| 0.3 | 修改 Rust 后端数据目录 `~/.config/openchamber/` → `~/.config/yourapp/` | 约 15 个源文件（见 §2.2） |
| 0.4 | 修改 `user_config_root()` 从硬编码改为读环境变量 | `github/settings.rs` |
| 0.5 | 更新 Tauri deep-link 协议（自动从 bundle ID 派生）+ 实现 TODO handler | `lib.rs:192,411` |
| 0.6 | 更新 APNs bundle ID `com.openchamber.app` → `com.yourapp.app` | `notifications/mod.rs:80` |
| 0.7 | 更新 updater 端点到自己的发布仓库 | `tauri.conf.json:43` |
| 0.8 | 更新 Rust 端 tray icon 实例名 `"Local OpenChamber"` → 新品牌 | `tray.rs:1401,1428,1489,1491,1523` |
| 0.9 | 更新 Capacitor `appId` | `packages/mobile/capacitor.config.ts:4` |
| 0.10 | 更新 iOS Bundle ID + App Group | `packages/mobile/ios/.../project.pbxproj` (4 个 target), `App.entitlements:15`, `OpenChamberNotificationService.entitlements:8`, `OpenChamberWidget.entitlements:8` |
| 0.11 | 更新 iOS CFBundleDisplayName + URL scheme | `packages/mobile/ios/App/App/Info.plist:8,46,49` |
| 0.12 | 更新 Android `namespace` + `applicationId` + `strings.xml` | `packages/mobile/android/app/build.gradle:12,15`, `strings.xml:3,4,5,6`, `MainActivity.java:1` |
| 0.13 | 重新生成 `google-services.json` | `packages/mobile/android/app/google-services.json:4,5,12` |
| 0.14 | 更新移动端脚本中的 BUNDLE_ID/APP_ID | `ios-sim.mjs:5`, `android-device.mjs:14` |

**验证**: 两个应用各启动一次，确认不共享数据目录。
`ls ~/.config/yourapp/` 不应包含 OpenChamber 的旧数据。

### 阶段 1 — 品牌重命名（系统完成）

**目标**: 代码库中所有显式的 `openchamber` 品牌标识符替换为你的品牌。
⚠️ **Rust 后端与前端必须同步修改**，任何一面漏改都会直接导致 UI 故障。

| 步骤 | 内容 | 涉及范围 | 前端对应位置 |
|------|------|---------|------------|
| 1.1 | 环境变量 `OPENCHAMBER_*` → `YOURAPP_*` | `config.rs`, 所有模块 | — |
| 1.2 | Rust IPC 事件名 `openchamber:*` → `yourapp:*` | `ipc/`, `tray.rs`, `menu.rs`, `updater.rs` | 见 §3.5（前端 `openchamber:*` 同步） |
| 1.3 | JS 注入变量 `__OPENCHAMBER_*__` → `__YOURAPP_*__`（写入端） | `ipc/globals.rs`（Tauri 写入） | 见 §3.1（前端所有读取点） |
| 1.4 | API 路由 `/api/openchamber/*` → `/api/yourapp/*` | `relay/`, `tunnels/`, `lib.rs`, `ui_auth/types.rs` | 见 §3.3（前端所有调用点） |
| 1.5 | Tauri 命令 `openchamber_*` → `yourapp_*` | `lib.rs`, `ipc/mod.rs`, `ipc/dialog_cmd.rs`, `ipc/globals.rs` | — |
| 1.6 | Sidecar 二进制名 `"openchamber"` → `"yourapp"` | `sidecar.rs` | — |
| 1.7 | `openchamber-ui://` 协议 → `yourapp-ui://` | `ipc/dialog_cmd.rs`, `ipc/mod.rs`, `middleware/cors.rs` | CORS allowlist + 测试 fake origin |
| 1.8 | `x-openchamber-*` 自定义头 | `relay/tunnel_host.rs`, `preview/routes.rs`, `git/routes.rs` | （前端透传于 `__OPENCHAMBER_RUNTIME_HEADERS__`） |
| 1.9 | `".openchamber.backup"` 后缀 | `opencode/auth.rs`, `opencode/config.rs` | — |
| 1.10| `metadata.openchamber.*` 命名空间（后端） | `session_assist/`, `session_goal/` | 见 §3.8（前端读写处，**需要数据迁移策略**） |
| 1.11| WebAuthn RP ID `"openchamber-ui"` | `ui_auth/passkeys.rs:285` | —（前端通过 passkey 自动处理） |
| 1.12| API 响应字段 `"openchamberVersion"` | `routes.rs` | 前端 `openCodeStatus.ts` 引用 |
| 1.13| User-Agent 字符串 | `small_model/call.rs`, `github/client.rs` | — |
| 1.14| 多部分 boundary / Preview Bridge ID | `tts/stt.rs`, `preview/mod.rs` | — |
| 1.15| NPM 包 `/ssh/` 中的远程安装引用 | `ssh/mod.rs` | — |
| 1.16| **前端**：deep-link scheme | — | §3.2（`deepLinks.ts` 等） |
| 1.17| **前端**：localStorage / zustand-persist 命名空间 | — | §3.4（11 个 key） |
| 1.18| **前端**：settings i18n 命名空间 `settings.openchamber.*` | — | §3.7（**建议保留**，避免翻译失效） |
| 1.19| **前端**：自定义 DOM 属性 / DnD mime / 内嵌链接前缀 | — | §3.6（7 个标识） |
| 1.20| **前端**：postMessage type 字符串 | — | §3.15（3 个 type） |
| 1.21| **前端**：配置持久化文件路径 | — | §3.11（`~/.config/openchamber/...`、`.openchamber/...`） |
| 1.22| **前端**：TS 类型/函数名 `OpenChamber*`/`openchamber*` | — | §3.12（约 50+ 个） |
| 1.23| **前端**：移动端 Capacitor / iOS / Android | — | §3.10 |
| 1.24| **前端**：PWA manifest / Logo / Theme id | — | §3.9 |
| 1.25| 测试中的硬编码字符串 | 所有测试文件（含前端） | §3.14 |
| 1.26| 注释中的 `openchamber` 引用 | 所有文件注释（可选） | — |

### 阶段 2 — 服务依赖裁剪

**目标**: 移除或替换所有 OpenChamber 托管服务。

| 步骤 | 内容 | 复杂度 |
|------|------|--------|
| 2.1 | **移除 relay 模块**：删除 `relay/` 目录、路由注册、AppState 引用 | 低 |
| 2.2 | **清理 relay 残留**：删除 `RELAY_HKDF_INFO`、自定义头 | 低 |
| 2.3 | **推送通知解除 relay**：`apns_send.rs` 移除 relay 模式（默认值+env+代码） | 中 |
| 2.4 | **保留直接 APNs + Web Push**：确认在没有 relay 时正常工作 | 中 |
| 2.5 | **移除 `opencode_go` quota provider** | 低 |
| 2.6 | **模型目录**：替换 `MODELS_DEV_API_URL` 或改为空/可配置 | 低 |
| 2.7 | **桌面更新**：已替换（阶段 0），确认更新插件可工作 | 低 |
| 2.8 | **GitHub Client ID**：注册自己的 OAuth App 并替换 | 低 |

### 阶段 3 — 配置隔离 + 深度清理

**目标**: 确保残留的旧文件、旧配置不会影响新应用。

| 步骤 | 内容 |
|------|------|
| 3.1 | **自动迁移脚本**：提供 `~/.config/openchamber/` → `~/.config/yourapp/` 迁移工具（可选） |
| 3.2 | **处理 SSH socket 路径**：`/tmp/ocssh-*` → `/tmp/yourapp-ssh-*` |
| 3.3 | **处理 log 目录**：更新所有日志路径 |
| 3.4 | **处理 crash dump / diagnostic 信息** |

### 阶段 4 — 前端品牌与持久化重命名（同步阶段 1 的前端部分）

**目标**: 把 §3 列出的所有前端 `openchamber` 标识替换为新品牌，**所有变更必须与阶段 1 的 Rust 后端同步**，否则 UI 直接挂掉。

| 步骤 | 内容 | 对应章节 |
|------|------|---------|
| 4.1 | 更新 `__OPENCHAMBER_*__` 全局变量 → `__YOURAPP_*__`（写入端 + 读取端） | §3.1 |
| 4.2 | 更新 `DEEP_LINK_SCHEME = 'openchamber'` → 新品牌；同步所有 `openchamber://` URL 构造/解析 | §3.2 |
| 4.3 | 更新所有 `/api/openchamber/*` 前端调用点 | §3.3 |
| 4.4 | 更新 localStorage / zustand-persist key（按 §3.4 策略：直接改 or 读时回退） | §3.4 |
| 4.5 | 更新 `openchamber:…` 自定义事件名（前端 ~55 个 + 后端对应 ~15 个） | §3.5 |
| 4.6 | 更新自定义 DOM 属性 / DnD mime / 内嵌链接前缀 | §3.6 |
| 4.7 | 决策并执行 settings i18n 命名空间 `settings.openchamber.*` 改造（建议保留） | §3.7 |
| 4.8 | **决策点**：`metadata.openchamber.*` 数据迁移策略（迁移 or 断舍离） | §3.8 |
| 4.9 | 更新 PWA manifest / Logo 组件 / Theme id | §3.9 |
| 4.10 | 更新 TS 类型/函数名 `OpenChamber*`（约 50+ 个） | §3.12 |
| 4.11 | 更新前端配置持久化文件路径（`~/.config/openchamber/...`、`.openchamber/`） | §3.11 |
| 4.12 | 更新 postMessage type 字符串 | §3.15 |
| 4.13 | 更新前端测试 fixture | §3.14 |
| 4.14 | 清理散落硬编码 "OpenChamber" 字符串（tray 菜单文字、错误消息、console.log 前缀） | §3.13 |

### 阶段 5 — 测试验证

| 步骤 | 内容 |
|------|------|
| 5.1 | `cargo test --workspace` 全量测试通过 |
| 5.2 | 前端 `bun run type-check`、`bun run lint`、`bun run dead-code` 全量通过 |
| 5.3 | 两个应用同时运行，确认不冲突（Bundle ID、协议、目录均隔离） |
| 5.4 | `bun run tauri:dev` 启动验证（Tauri 进程能加载 UI、能调用所有注入变量） |
| 5.5 | 验证新数据目录 `~/.config/yourapp/` 创建正确，settings 持久化/读取正常 |
| 5.6 | 验证 deep-link 在 Tauri / iOS / Android 端均能正确触发（修改后 scheme） |
| 5.7 | 验证 mobile pairing QR 扫描（修改后的 `yourapp://connect?...`） |
| 5.8 | 验证更新机制（如果配置了，端点指向自己的仓库） |
| 5.9 | 验证 `metadata.openchamber.*` 迁移（如做了数据迁移） |

---

## 附录

### A. 关键文件索引

#### A.1 Rust 后端

| 领域 | 核心文件 |
|------|---------|
| 应用配置 | `rust/oc-tauri/src-tauri/tauri.conf.json` |
| 服务路由 | `rust/oc-server/src/lib.rs` (build_router) |
| 全局状态 | `rust/oc-server/src/state.rs` |
| 环境/CLI 配置 | `rust/oc-server/src/config.rs` |
| GitHub OAuth/设置 | `rust/oc-server/src/github/mod.rs`, `settings.rs` |
| 数据目录 | `rust/oc-server/src/github/settings.rs` (user_config_root) |
| Tauri 初始化 | `rust/oc-tauri/src-tauri/src/lib.rs` |
| Tauri IPC 桥 | `rust/oc-tauri/src-tauri/src/ipc/globals.rs` |
| Sidecar 管理 | `rust/oc-tauri/src-tauri/src/sidecar.rs` |
| 推送通知 | `rust/oc-server/src/notifications/mod.rs`, `apns_send.rs`, `push_send.rs` |
| Relay 中继 | `rust/oc-server/src/relay/identity.rs`, `crypto.rs`, `service.rs` |
| 模型目录 | `rust/oc-server/src/opencode/models_metadata.rs` |

#### A.2 前端

| 领域 | 核心文件 |
|------|---------|
| 运行时 URL/auth | `packages/ui/src/lib/runtime-url.ts`, `runtime-fetch.ts`, `runtime-auth.ts`, `runtime-switch.ts` |
| 运行时配置入口 | `packages/web/src/runtimeConfig.ts`, `packages/web/vite.config.ts` |
| 类型声明（全局变量） | `packages/ui/src/types/desktop.d.ts` |
| 入口引导 | `packages/web/src/main.tsx`, `mini-chat-main.tsx`, `mobile-main.tsx` |
| Deep-link | `packages/ui/src/apps/deepLinks.ts`, `deepLinkNavigation.ts`, `mobileQrScan.ts`, `connectionPayload.ts` |
| 设置持久化 | `packages/ui/src/lib/openchamberConfig.ts`, `persistence.ts` |
| OpenCode metadata | `packages/ui/src/lib/sessionReviewMetadata.ts` |
| 事件 helper | `packages/ui/src/lib/openchamberEvents.ts` |
| 移动端 | `packages/mobile/capacitor.config.ts`, `packages/mobile/ios/...`, `packages/mobile/android/...` |
| PWA manifest | `packages/web/public/site.webmanifest` |
| 主题 | `packages/ui/src/lib/theme/themes/index.ts`, `presets.ts`, `fields-of-the-shire-{light,dark}.json` |
| Logo | `packages/ui/src/components/ui/OpenChamberLogo.tsx` |
| i18n 设置搜索 | `packages/ui/src/lib/settings/search.ts` |
| i18n 资源 | `packages/ui/src/lib/i18n/messages/{en,zh-CN,zh-TW,ja,ko,fr,es,pt-BR,pl,uk}.settings.ts` |

### B. 冲突解决检查清单

用于快速验证分叉应用是否与 OpenChamber 完全隔离：

#### B.1 Rust 后端

- [ ] Bundle ID 不同 (`com.yourapp.desktop` ≠ `dev.openchamber.desktop`)
- [ ] 数据目录不同 (`~/.config/yourapp/` ≠ `~/.config/openchamber/`)
- [ ] 协议 handler 不同 (`yourapp://` ≠ `openchamber://`)
- [ ] 产品名不同（如不改则跳过，当前名为 `GridForge`）
- [ ] 所有 `OPENCHAMBER_*` env var 已更名（开发脚本、`justfile`、`bun run` 包装脚本同步）
- [ ] 更新端点指向自己的仓库
- [ ] GitHub OAuth Client ID 已替换
- [ ] APNs Bundle ID 已更新
- [ ] SSH socket 路径不同（`/tmp/ocssh-*.sock` → `/tmp/yourapp-ssh-*.sock`）
- [ ] 日志目录名不同
- [ ] `cargo test --workspace` 通过 + clippy 0 warnings

#### B.2 前端

- [ ] 所有 `__OPENCHAMBER_*__` 读取点已改为 `__YOURAPP_*__`，**写入端与读取端一一对应**
- [ ] `DEEP_LINK_SCHEME` 改完且所有 URL 构造/解析同步
- [ ] 所有 `/api/openchamber/*` 前端调用改完且与后端路由一致
- [ ] localStorage / zustand-persist key 决策已执行（迁移 or 重命名）
- [ ] `openchamber:…` 自定义事件名改完且前后端一一对应
- [ ] 自定义 DOM 属性 / DnD mime / 内嵌链接前缀改完
- [ ] `metadata.openchamber.*` 迁移策略已决策并执行
- [ ] PWA manifest / Logo / Theme id 改完
- [ ] `settings.openchamber.*` i18n 决策已执行
- [ ] TS 类型/函数名改完
- [ ] 配置持久化路径改完（`~/.config/openchamber/...`、`.openchamber/`）
- [ ] postMessage type 字符串改完
- [ ] 测试 fixture 改完且全量通过
- [ ] Capacitor `appId` 已改
- [ ] iOS Bundle ID / App Group / CFBundleDisplayName / URL scheme 改完
- [ ] Android `namespace` / `applicationId` / `strings.xml` / `google-services.json` 改完
- [ ] `bun run type-check` / `bun run lint` / `bun run dead-code` 通过

### C. 注意：Electron 与 Node web 服务器

本计划**不涉及** Electron (`packages/electron/`) 与 Node web 服务器 (`packages/web/server/`)，
因为它们将被移除。但需要注意：

- Rust `sidecar.rs` 仍保留 `OPENCHAMBER_SIDECAR=1` 的 sidecar 路径引用（指向 `@openchamber/web`
  CLI）。当 Electron 移除后，这段路径会自然失效，应在 sidecar 决策点时同步删除。
- `OPENCHAMBER_SIDECAR` env 改名（如阶段 1.1）会同时影响 sidecar 配置入口。
