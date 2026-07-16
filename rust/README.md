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

**阶段 3b Group 1 — 功能模块: github** (完成):
- [x] GitHub 模块 18 个路由 (`github/`: 直接 `reqwest` 调用 GitHub REST/GraphQL API,
      不引入 `octocrab`; 复用 git 模块的 `get_remotes`/`get_remote_url`/`get_status`)
- [x] `GitHubClient` REST 封装 (`github/client.rs`: `reqwest::Client` 8s 超时 +
      Bearer auth + 23 个 REST 方法 (repos/pulls/issues/checks/actions/search/users) +
      1 个 GraphQL 方法 (POST /graphql) + `GitHubApiError` 携带 status/headers/body)
- [x] OAuth device flow (`github/device_flow.rs`: 2 个 form-encoded POST,
      GitHub 对 pending 状态返回 HTTP 200 + `{error: 'authorization_pending'}`)
- [x] auth 存储 (`github/auth.rs`: `$DATA_DIR/github-auth.json` JSON 数组 mode 0o600 原子写 +
      `resolve_account_id` 4 级回退 (explicit→login→id→token prefix) +
      `normalize_auth_list` 恰好一个 current + gh-CLI token 缓存 30s TTL)
- [x] settings (`github/settings.rs`: client-id/scope 解析 (env→settings→default) +
      gh-CLI enable/disable)
- [x] rate-limit cooldown 门 (`github/rate_limit.rs`: `min(retryAfter ?? 60s, 15min)` +
      429/403+remaining:0/403+retry-after/403+message 检测)
- [x] repo URL 解析 (`github/repo.rs`: SSH/HTTPS/SSH-protocol 三种格式 +
      嵌入式 URL parser (不引入 `url` crate))
- [x] fork 检测 (`github/fork_detection.rs`: `resolve_repo_network` (origin + parent + source) +
      repo 元数据缓存 5min TTL / 200 max / LRU eviction)
- [x] PR status 解析 (`github/pr_status.rs`: 最复杂算法 — remote 排序 [explicit/tracking/origin/upstream/rest] +
      repo network 展开 (parent/source) + `SourceMatcher` repo key + owner key 排序 +
      search API fallback + search-disabled 缓存 5min retry)
- [x] 18 个 axum handler (`github/routes.rs`: auth 6 + user 1 + PR 5 + repo 2 + issue 3 + pull context 2 +
      PR status 缓存 90s TTL / 200 max / 只缓存 connected:true / 12s 超时)
- [x] `cargo test` 199/199 通过 (新增 49 测试), clippy 0 警告

**阶段 3b Group 2 — 功能模块: tunnels** (完成):
- [x] Tunnels 模块 8 个路由 (`tunnels/`: cloudflare + ngrok 隧道子进程编排,
      与 Node `packages/web/server/lib/tunnels/` + `cloudflare-tunnel.js` + `ngrok-tunnel.js` 契约对齐)
- [x] 常量 + normalizers (`tunnels/types.rs`: provider/mode/intent/hostname/TTL/configPath 归一化 +
      `TunnelServiceError` (http_status: missing_dependency→400, validation/provider/mode→422, else→500) +
      `is_path_within_directory` 路径安全 + `resolve_tunnel_config_path` (~展开 + home 边界) +
      `extract_hostname` 嵌入式 URL parser (不引入 `url` crate))
- [x] 跨平台可执行文件搜索 (`tunnels/executable_search.rs`: PATH 搜索 + Windows PATHEXT +
      WindowsApps 目录 + `resolve_executable_launch_target` (绝对路径解析 + Windows Store alias fallback))
- [x] 安装命令元数据 (`tunnels/install_help.rs`: darwin/win32/linux 三平台 + brew/winget/scoop/direct)
- [x] managed remote tunnel token 持久化 (`tunnels/managed_config.rs`:
      `$DATA_DIR/cloudflare-managed-remote-tunnels.json` v1 format + tokio Mutex 串行化 +
      旧文件 `cloudflare-named-tunnels.json` 迁移 + upsert/resolve)
- [x] 隧道认证控制器 (`tunnels/tunnel_auth.rs`: 全内存 Mutex 保护 —
      bootstrap token (32B base64url, SHA-256 hash 存储, 单次使用, TTL) +
      session (cookie → SessionRecord, TTL, revoke) +
      rate-limit (per-IP 5min/20次 + 10min lockout, no-IP fallback 5次) +
      timing-safe hash 比较 (XOR-OR accumulate) + `classify_request_scope` (tunnel/local/unknown-public) +
      手动 Cookie 解析/构建 (不引入 cookie crate) + 私有 hex encode (不引入 hex crate))
- [x] TTL 常量 + normalizers (`tunnels/mod.rs`: bootstrap 30min default / 1min~24h clamp (null透传) +
      session 8h default / 5min~30d clamp (null→default))
- [x] provider 抽象 (`tunnels/providers/mod.rs`: 模块级 async 函数 + match dispatch
      (不引入 `async_trait`); `TunnelController` 持有 `tokio::process::Child` + cleanup)
- [x] cloudflare provider (`tunnels/providers/cloudflare.rs`: 3 模式 (quick/managed-remote/managed-local) +
      `cloudflared` 子进程 spawn + READY_LOG_PATTERNS/FATAL_LOG_PATTERNS 日志就绪检测 +
      6s liveness fallback + 20s hard timeout (tokio::select!) +
      `inspect_managed_local_config` YAML 解析 (serde_yaml) + API reachability check)
- [x] ngrok provider (`tunnels/providers/ngrok.rs`: quick 模式 only +
      `ngrok http --log=stdout --log-format=json` 子进程 spawn +
      双通道 URL 提取 (stdout JSON + 250ms 轮询 localhost:4040 API) +
      `extract_ngrok_public_url_from_text` (JSON + regex) + `summarize_ngrok_output` error level)
- [x] TunnelService 编排 (`tunnels/service.rs`: Mutex-locked start (防并发孤儿进程) +
      stop / check_availability / get_public_url / get_provider_metadata +
      resolve_active_mode/provider + 共享 `TunnelRuntimeState`)
- [x] 8 个 axum handler (`tunnels/routes.rs`: check + doctor (GET+POST) + providers + status +
      managed-remote-token (PUT) + start + stop + `/connect` (bootstrap token→session 交换 + 302 redirect))
- [x] AppState 扩展 (`state.rs`: `tunnel_auth` + `tunnel_runtime` + `managed_config` + `tunnel_service` + `get_active_port`)
- [x] COMPATIBILITY 加 `api.tunnels.v1`
- [x] 新增依赖 `sha2 = "0.10"` (workspace + oc-server)
- [x] `cargo test` 248/248 通过 (新增 49 测试), clippy 0 警告

**阶段 3b Group 3 — 功能模块: ui-auth + client-auth** (完成):
- [x] ui-auth 模块 11 个路由 (`ui_auth/`: 密码会话 / JWT / 限速 / URL-token 范围 /
      WebAuthn passkey, 与 Node `ui-auth.js` + `ui-passkeys.js` 契约对齐)
- [x] 常量 + UiAuth 控制器 (`ui_auth/mod.rs`: session/URL-token/rate-limit/challenge TTL 常量 +
      `UiAuth` 结构持有 password_hasher/session_manager/rate_limiter/url_token_store/passkeys/client_auth +
      `reset_auth()` 轮换 JWT secret + 清空 passkeys + 清空 URL tokens +
      `compute_password_binding()` HMAC-SHA256(jwt_secret, password) → hex +
      私有 `generate_random_hex()` / `hex_encode()` (不引入 hex crate))
- [x] Cookie + URL-token 路径 + normalizers (`ui_auth/types.rs`: `parse_cookies()` (手动 `;` 分割 + percent_decode) +
      `build_cookie()` (SameSite=Strict; HttpOnly; Path=/ 格式精确匹配 Node) +
      `get_bearer_token()` regex + `normalize_password()` (trim only) +
      `is_url_auth_readable_http_path()` / `is_url_auth_websocket_path()` / `can_use_url_auth_token_for_request()` +
      `get_client_ip()` (x-forwarded-for + strip `::ffff:`) + `is_secure_request()` + `get_rate_limit_key()`)
- [x] JWT secret 文件管理 (`ui_auth/jwt_secret.rs`: `$DATA_DIR/jwt-secret` hex string mode 0o600 +
      env `OPENCODE_JWT_SECRET` 覆盖 + `get_or_create_jwt_secret()` + `persist_jwt_secret()`)
- [x] scrypt 密码哈希 (`ui_auth/password.rs`: `PasswordHasher` (SaltString::generate + Scrypt.hash_password) +
      `verify()` (PasswordHash::new + Scrypt.verify_password) — 进程级 salt, 非持久化)
- [x] 登录限速器 (`ui_auth/rate_limit.rs`: per-IP sliding window (5min/10次, 15min 锁定) +
      no-IP fallback (3次) + `check()`/`record_failure()`/`clear()`/`cleanup()`)
- [x] URL auth token store (`ui_auth/url_token.rs`: `oc_url_` 前缀 + 24 字节 base64url, 60s TTL,
      `issue()`/`authenticate()`/`sweep()`/`clear()`)
- [x] Session JWT + cookie (`ui_auth/session.rs`: HS256 `{type:"ui-session",exp,iat}` +
      `issue_session(trust_device)` (12h/7d TTL) + `is_session_valid()` + `build_session_cookie()`/`build_clear_cookie()` +
      `update_secret()` 支持轮换 + 私有 `url_encode()` (encodeURIComponent 等价))
- [x] WebAuthn passkey store (`ui_auth/passkeys.rs`: `$DATA_DIR/ui-passkeys.json` (version/userId/passwordBinding/passkeys) +
      `webauthn-rs` `start_passkey_registration`/`finish_passkey_registration`/`start_passkey_authentication`/`finish_passkey_authentication` +
      per-(rp_id, origin) Webauthn 实例 + passwordBinding 不匹配时清空所有 passkeys + 重生成 user_id +
      `PasskeyError` 枚举 + 注册/认证 challenge in-memory store)
- [x] 11 个 ui-auth axum handler (`ui_auth/routes.rs`: session status/create + url-token + passkey status/auth-options/auth-verify/register-options/register-verify +
      passkey list/revoke + auth reset — tunnel scope 门 + rate-limit + 手动 Cookie 插入)
- [x] client-auth 模块 10 个路由 (`client_auth/`: trusted-device bearer token + Pairing v2 会话,
      与 Node `client-auth/remote-clients.js` + `pairing.js` 契约对齐)
- [x] 常量 (`client_auth/mod.rs`: TOKEN_PREFIX `oc_client_` + PAIRING_ID_PREFIX `pair_` + SECRET_BYTES/TOKEN_BYTES)
- [x] remote client token store (`client_auth/remote_clients.rs`: `$DATA_DIR/remote-clients.json` +
      `oc_client_` 前缀 + 32 字节 base64url + SHA-256 hex hash 存储 + constant-time 比较 (XOR-OR accumulate) +
      `create_client()` (生成 token + hash + dedupe + persist) + `authenticate_bearer_token()` (前缀检查 + hash 比较 + 过期 + 节流 lastUsedAt 60s) +
      `list_clients()`/`revoke_client()`/`revoke_all_clients()`/`has_active_relay_clients()`)
- [x] pairing session store + redeem (`client_auth/pairing.rs`: `$DATA_DIR/client-pairing-sessions.json` +
      `pair_` 前缀 + 32 字节 base64url secret + SHA-256 hex hash + `redeem_session()` 所有失败返回同一 generic error (无 oracle 泄漏) +
      createClient 失败不消费 session + `create_session()`/`list_pending()`/`cancel_session()`/`sweep()`)
- [x] 10 个 client-auth axum handler (`client_auth/routes.rs`: clients list/create/revoke/revoke-all +
      pairing sessions create/list/cancel/redeem + connection/candidates + transports —
      transport candidates (routes 5-7) 返回空/默认值 (relay+LAN 未迁移))
- [x] AppState 扩展 (`state.rs`: `ui_auth` + `remote_client_auth` + `client_pairing`)
- [x] COMPATIBILITY 加 `api.ui-auth.v1` + `api.client-auth.v1`
- [x] 新增依赖 `scrypt`/`hmac`/`jsonwebtoken`/`webauthn-rs`/`webauthn-rs-proto`/`url` (workspace + oc-server)
- [x] `cargo test` 306/306 通过 (新增 57 测试), clippy 0 警告

**阶段 3b Group 4 — 功能模块: notifications** (完成):
- [x] notifications 模块 18 个路由 (`notifications/`: web-push / APNs / SSE 通知流 /
      session activity / attention / view tracking, 与 Node `notifications/routes.js` 契约对齐)
- [x] 常量 (`notifications/mod.rs`: SSE 心跳 20s / 消息截断 250 / 可见性 TTL 30s /
      push/apns 文件 version / APNs JWT TTL 50min / cooldown 5s / debounce 500ms /
      relay URL / APNs production+sandbox host + `now_millis()` helper)
- [x] 文本规范化 (`notifications/message.rs`: markdown→plain text 正则剥离 (fenced/inline code,
      list markers, headings, bold/italic, links) + 空白折叠 + truncate + `...`)
- [x] 解析器 + normalizers (`notifications/types.rs`: `parse_push_subscribe_body()` /
      `parse_push_unsubscribe_body()` / `extract_session_id_from_payload()` /
      `extract_directory_from_payload()` / `format_mode()` / `format_model_id()` /
      `format_project_label()` / `normalize_pem()` + `get_parent_id()`)
- [x] session 状态机 (`notifications/session_state.rs`: 纯内存 Mutex<HashMap> —
      activity phase (idle/busy/cooldown) + session status + attention state +
      viewed-by-clients tracking + needsAttention 推导 (busy/retry→idle + 有用户消息 + 无客户端查看) +
      mark viewed/unviewed/message-sent 广播 `openchamber:session-status` SSE)
- [x] web-push 订阅持久化 (`notifications/push_store.rs`: `$DATA_DIR/push-subscriptions.json` v1 +
      `Mutex<()>` write lock 串行化 read-modify-write + 去重 (endpoint) + MAX_SUBS_PER_SESSION 10 +
      in-memory 可见性 Map (TTL 30s) + `is_ui_visible()`/`is_any_ui_visible()`/`is_any_interactive_client_visible()`)
- [x] APNs token 持久化 (`notifications/apns_store.rs`: `$DATA_DIR/apns-tokens.json` v1 +
      同 write lock 模式 + 去重 (deviceToken) + platform 归一化 (非 android→ios) +
      `remove_token_from_all_sessions()`)
- [x] relay 签名身份 (`notifications/relay_key.rs`: `settings.relaySigningKey = {privateJwk, publicJwk}` +
      ECDSA P-256 `SigningKey` / `VerifyingKey` (p256 crate) +
      `canonical_public_jwk_string()` + `derive_server_id()` (base64url(SHA-256(canonical JWK))) +
      `sign_relay_message()` IEEE-P1363 base64url + `get_or_create_relay_keypair()` + 跨 Node/Rust 兼容)
- [x] APNs 发送 (`notifications/apns_send.rs`: **relay 模式** (默认) POST tokens + generic text 到 relay URL +
      签名 POST body (tokens/title/body/badge/collapseId/env/data/publicKeyJwk/ts/sig) +
      响应 `results[].drop` → 删 token + **direct 模式** (fallback) ES256 JWT 签名 +
      HTTP/2 (h2 crate, tokio-rustls 不在 → DEGRADED warn no-op) +
      410/dead-reason → 删 token + `send_apns_to_all_ui_sessions()` fanout)
- [x] web-push 发送 (`notifications/push_send.rs`: `web-push` crate v0.11 + `p256` (VAPID 密钥生成) +
      `get_or_create_vapid_keys()` (settings 持久化, p256 32-byte scalar → base64url) +
      `ensure_push_initialized()` per-call VAPID + `send_push_to_subscription()` (410/404→删) +
      `send_push_to_all_ui_sessions()` 去重 endpoint + 可见性门控)
- [x] 模板变量解析 (`notifications/template.rs`: `resolve_notification_template()` `{key}` 插值 +
      `build_template_variables()` 解析 project_name/worktree/branch/session_name/agent_name/model_name +
      git branch (`tokio::process::Command` spawn `git`, 3s 超时) +
      `extract_text_from_parts()` / `extract_last_message_text()` / `fetch_last_assistant_message_text()` (reqwest GET OpenCode API) +
      session info 缓存 (TTL 60s))
- [x] SSE emitter (`notifications/emitter.rs`: `broadcast::Sender<SseMessage>` SSE 客户端池 +
      `write_sse_event()` `"data: {json}\n\n"` 到所有客户端 + `emit_desktop_notification()` callback 或 stdout fallback +
      `broadcast_ui_notification()` 包装为 `{type:"openchamber:notification", properties:{...}}` + SSE 广播 +
      `subscribe_sse()` SSE stream handler 用)
- [x] trigger fanout orchestrator (`notifications/trigger.rs`: 最复杂模块, 移植 `runtime.js` 完整逻辑 —
      cooldown 5s (last_ready/last_error) + question debounce 500ms + permission debounce 500ms +
      auto-accept suppression + subtask suppression + goal suppression + window-focus gate +
      `session.idle`/`session.error` → 重写为 `message.updated` (non-recursive `process_payload`) +
      `message.updated` ready/error notification + `question.asked` debounce spawn +
      `permission.asked`/`permission.replied` debounce spawn +
      `Arc<NotificationTrigger>` + `tokio::spawn` debounce timers +
      `fanout_push()` web-push (full templated payload, visibility-gated) + APNs (generic payload, interactive-client-gated) +
      channel 失败不阻塞 (fire-and-forget) + `to_apns_generic_payload()` APNS_TITLE_BY_TYPE)
- [x] 18 个 axum handler (`notifications/routes.rs`: push vapid-key/subscribe (POST+DELETE)/apns-token (POST+DELETE)/visibility (POST+GET) +
      SSE notification stream (20s 心跳, `futures_util::stream::select` 合并 notification broadcast + heartbeat) +
      session-activity/snapshot/status (all+single)/attention (all+single) +
      view/unview/message-sent + auto-accept mirror)
- [x] AppState 扩展 (`state.rs`: `push_store` + `apns_store` + `emitter` + `session_state` + `notification_template` +
      `push_send` + `apns_send` + `notification_trigger` (OnceCell deferred init pattern) +
      `init_notification_trigger()` 订阅 `global_hub.subscribe_event()` 后台消费 task)
- [x] 路由注册 (`main.rs`: 18 路由在 client-auth 之后、SSE proxy 之前 + trigger init after `set_opencode_ready`)
- [x] COMPATIBILITY 加 `api.notifications.v1`
- [x] 新增依赖 `p256` (+pkcs8 feature) / `web-push` / `h2` (workspace + oc-server)
- [x] `cargo test` 389/389 通过 (新增 83 测试), clippy 0 警告

**阶段 3c Group 1 — 功能模块: permission-auto-accept + session-folders + magic-prompts** (完成):
- [x] permission-auto-accept 模块 2 个路由 (`permission_auto_accept.rs`: 移植 `runtime.js` —
      `Policy { sessions: HashMap<String, bool> }` 持久化到 `settings.json` 的 `permissionAutoAccept` key,
      复用 `github::settings::read_settings`/`write_settings`,
      session lineage 向上遍历 parentID 链找最近显式策略 (缺失 parentID 时 GET `/session/{id}` 补全,
      缓存 10000 上限 LRU),
      `process_permission` 去重 (in_flight HashMap) + retry 延迟序列 `[0, 250, 1000ms]` +
      POST `/permission/{id}/reply {reply:"once"}` (404 → 视为已处理) +
      `reconcile_pending` 收集 pending → 对 auto-accept session 自动 reply,
      OpenCode API 调用使用 reqwest `.query(&[("directory", dir)])` (不引入 urlencoding crate))
- [x] GlobalHub 集成 (`permission_auto_accept::start`: 订阅 `global_hub.subscribe_event()` —
      `session.created`/`session.updated` → rememberSession (lineage 缓存) +
      `permission.asked` → processPermission (spawn fire-and-forget task) +
      订阅 `global_hub.subscribe_status()` — `connect` → reconcilePending;
      策略变更广播 `openchamber:permission-auto-accept.updated` SSE 事件
      通过 `emitter.broadcast_ui_notification`)
- [x] session-folders 模块 2 个路由 (`session_folders.rs`: 最简单模块 —
      `$DATA_DIR/sessions-directories.json` JSON 文件原子读写,
      GET 默认空文件返回 `{version:1, foldersMap:{}, collapsedFolderIds:[], updatedAt:0}`,
      POST 4MB 上限 + body 校验 (must be object) + `.tmp → rename` 原子写模式,
      `{pid}-{millis}-{random}` 唯一临时路径, 失败时清理 tmp)
- [x] magic-prompts 模块 4 个路由 (`magic_prompts.rs`: `$DATA_DIR/magic-prompts.json` 文件结构
      `{version:1, overrides:{[id]:text}}` + `FILE_VERSION = 1` + `MAX_PROMPT_TEXT_LENGTH = 200_000` +
      `MagicPromptRuntime` 持有 `Mutex<()>` write_lock 串行化 read-modify-write (同 push_store/apns_store 模式),
      `PROMPT_ID_PATTERN` 正则 `^[a-z0-9._-]{1,160}`,
      `is_visible_prompt_id()` 检测 `.visible` 后缀 (不允许空文本),
      handlers: `get_magic_prompts` (GET) + `put_magic_prompt/{id}` (PUT) +
      `delete_magic_prompt/{id}` (DELETE) + `delete_all_magic_prompts` (DELETE root))
- [x] 复用现有模式 (`github::settings::read_settings/write_settings` policy 持久化 +
      `github::settings::data_dir()` 文件路径解析 +
      原子写 `.tmp → rename` + `Mutex<()>` write_lock +
      `GlobalHub::subscribe_event/subscribe_status` 订阅 +
      `emitter.broadcast_ui_notification` UI 事件广播 +
      `init_permission_auto_accept(self: &Arc<Self>)` 延迟初始化, 同 `init_notification_trigger`)
- [x] AppState 扩展 (`state.rs`: `permission_auto_accept: Arc<PermissionAutoAcceptRuntime>` +
      `init_permission_auto_accept()` 在 `set_opencode_ready` 后调用, 启动后台 fanout task)
- [x] 路由注册 (`main.rs`: 8 路由在 notifications 之后、SSE proxy 之前 + trigger init)
- [x] COMPATIBILITY 加 `api.permission-auto-accept.v1` + `api.session-folders.v1` + `api.magic-prompts.v1`
- [x] `cargo test` 413/413 通过 (新增 24 测试: permission_auto_accept 13 + magic_prompts 8 + session_folders 3), clippy 0 警告

**阶段 3c Group 2 — 功能模块: opencode + small-model** (完成):
- [x] opencode 子模块重组 (`opencode/`: 替换原 `opencode.rs` 单文件为 `opencode/mod.rs` 4 子模块 —
      `paths.rs` + `auth.rs` + `config.rs` + `models_metadata.rs`,
      与 Node `opencode/paths.js` + `auth.js` + `shared.js` + `models-metadata.js` 1:1 对齐)
- [x] paths (`opencode/paths.rs`: HOME_DIR_NAME + OPENCODE_DATA_DIR_NAME + AUTH_FILE_NAME +
      CONFIG_DIR_NAME + CONFIG_FILE_NAME + CUSTOM_CONFIG_FILE_NAME + AGENT/COMMAND/SKILL_DIR_NAME,
      `home_dir()` 复用 `crate::git::paths::home_dir()`,
      `opencode_data_dir()` + `auth_file()` + `config_file()` + `custom_config_file()` + `agent/command/skill_dir()`)
- [x] auth (`opencode/auth.rs`: `AuthError` 枚举 (Io/Parse/Json/Write) +
      `read_auth_file()` / `write_auth_file()` (原子写 `.tmp{pid}-{ts}-{rand} → rename`) +
      `write_auth_file_at()` / `get_provider_auth()` / `set_provider_auth()` / `remove_provider_auth()` /
      `list_provider_auths()`,
      备份模式 `.{name}.openchamber.backup` (与 Node `auth.js:32` 对齐),
      写入后 `chmod 0o600` (Unix only, 与 Node `shared.js#writeConfig` 对齐))
- [x] config (`opencode/config.rs`: 全套 `shared.js` (536 行) 端口 —
      SCOPE 常量 (Agent/Command/Skill) + `ensure_dirs()` 创建 3 种类型所有 scope 目录 +
      `MdFile` (markdown 元数据 + optional frontmatter) + `parse_md_file()` / `write_md_file()`
      (保留空行, 过滤 null frontmatter) +
      `ConfigError` + `read_config_file()` 用 `jsonc-parser v0.33` `parse_to_serde_value` +
      `ParseOptions::default()` (注释 + 尾逗号) +
      `merge_configs()` 递归 deep merge + 数组覆盖不合并 + `null` 覆盖 (`null` 字段胜出) +
      `ConfigLayers` (`{user_config, project_config, custom_config, paths}`) +
      `read_config_layers()` 3-layer merge (project + user + custom) +
      `get_config_for_path()` ancestor merge (向上遍历 worktree 链) +
      `write_config()` 原子写 + 备份 +
      `get_ancestors()` / `find_worktree_root()` 沿 `.git`/`worktree` 向上查找 +
      `is_prompt_file_reference()` 匹配 `(?i)^{file:NAME}` 模式 +
      `resolve_prompt_file_path()` tilde 展开 + 路径在 config 目录内 +
      `write_prompt_file()` 写入到 `<config-dir>/prompts/{name}.md` +
      `Skill` + `list_skills()` + `walk_skill_md_files()` 仅保留 `SKILL.md` (不递归 subdir) +
      `resolve_skill_search_directories()` 3 层目录解析 (project + global + custom) +
      `SkillSupportingFile` + `list_skill_supporting_files()` /
      `read/write/delete_skill_supporting_file()` + `walk_supporting()` +
      `assert_path_within_skill_dir()` 路径遍历防护 (canonicalize + canonical 起点比较))
- [x] models_metadata (`opencode/models_metadata.rs`: `ModelsMetadata` struct +
      `MODELS_DEV_API_URL = "https://models.dev/api.json"` +
      `GlobalState` 用 `tokio::sync::OnceCell<Result<...>>` + 单独 `started_at` TTL/timeout +
      `fetch_catalog()` http GET + JSON parse (4MB 上限) +
      `try_cache()` TTL 命中逻辑 +
      `get_models_metadata()` 入口 (cache hit → return cache, miss → spawn inflight dedup via
      `Arc<tokio::sync::OnceCell<Result<Value, Error>>>` + `Mutex<Option<Arc<InflightHandle>>>`,
      fetch 失败 + 有 cache → stale fallback `{metadata, fromCache:true, stale:true}`,
      fetch 失败 + 无 cache → 错误传播,
      测试用 `tokio::net::TcpListener` mock HTTP server))
- [x] small-model 模块 5 个文件 (`small_model/`: resolve + call + index + routes + mod 骨架,
      与 Node `small-model/` (1307 行) 1:1 对齐)
- [x] resolve (`small_model/resolve.rs`: 常量 `FAMILY_PRIORITY` + `COPILOT_UTILITY_MODELS` +
      `OPENAI_OAUTH_SMALL_MODEL`, `ModelRef` / `ResolvedModel` / `ResolveArgs` 全部 camelCase + skip_if_Option,
      `parse_model_ref()` (provider/model 分割, basename 修剪, validate 非空) +
      `get_auth_entry_for_provider()` (TypeScript 已无 api_key 也算 authenticated) +
      `is_usable_auth_entry()` (空字符串/空白为 false) +
      `pick_by_family()` (按 FAMILY_PRIORITY 顺序扫) +
      `pick_within_provider()` (按 cost/family 启发式) +
      `is_authenticated()` (从 auth.json 找 provider, 列表遍历) +
      `resolve_small_model()` 完整决策链
      1. `preferred_model_id` 显式 → 直接用
      2. `preferred_provider_id` + `restrict_to_preferred_provider=true` → 该 provider 第一个可用
      3. `preferred_provider_id` → 该 provider 内按 family priority + is_authenticated 过滤
      4. fallback → 扫全局 catalog 找最小可用)
- [x] call (`small_model/call.rs`: 常量 `REQUEST_TIMEOUT_MS = 60_000` +
      `DEFAULT_MAX_OUTPUT_TOKENS = 4_000` + `USER_AGENT` + `CODEX_TOKEN_URL` +
      `CODEX_RESPONSES_URL`,
      JWT 解码 `decode_jwt_claims()` + `extract_chatgpt_account_id()`,
      `read_provider_config()` 从 opencode config 读 provider config (apiKey/baseURL) +
      `call_small_model()` dispatcher 按 provider 类型分发 (openai-compatible / anthropic / google /
      openai-codex-SSE), 4 个内部实现不流式, timeout 60s,
      OAuth single-flight refresh 占位 (`Lazy<Option<String>>` + 注释指向 future group))
- [x] index (`small_model/index.rs`: 常量 `DEFAULT_CONTEXT_TOKENS = 64_000` +
      `OUTPUT_RESERVE_TOKENS = 4_000`, `GenerateArgs` + `DescribeArgs` + `GenerateResult`
      (含 `#[serde(rename = "inputTruncated", skip_serializing_if = "Option::is_none")]`),
      `SmallModelError` 含 `status_code` 字段,
      `clamp_prompt_to_model_limit()` 4 chars/token 启发式 + `truncated` 标记,
      `generate_small_model_text()` public API + `describe_small_model()` + `list_authenticated_providers()`)
- [x] routes (`small_model/routes.rs`: `SmallModelQuery` (directory/providerID/modelID) +
      `SmallModelGenerateBody` (prompt/system/maxOutputTokens/model/directory/preferredProviderID/
      preferredModelID/restrictToPreferredProvider),
      2 个 axum handler —
      `GET  /api/small-model` → `{available, model, authenticatedProviders}` +
      `POST /api/small-model/generate` → `{text, providerID, modelID, source, inputTruncated?}`)
- [x] 复用现有模式 (`github::settings::read_settings` 读取 small-model 覆盖 +
      `crate::opencode::auth` 读 provider auth +
      `crate::opencode::config` 读 provider 配置 +
      `crate::git::paths::home_dir` 复用 +
      `crate::opencode::models_metadata` 复用 +
      `serde_json::Value` 作为中间类型与 JS 行为对齐)
- [x] AppState 扩展 (`state.rs`: `small_model_service: Arc<SmallModelService>` unit struct —
      当前 stateless (所有调用走 module-level static + 临时 fetch),
      字段标记 `#[allow(dead_code)]` 后续 group 添加 per-session 缓存或后端路由选择)
- [x] 路由注册 (`main.rs`: 2 路由在 magic-prompts 之后、SSE proxy 之前:
      `GET  /api/small-model` (axum `get`) +
      `POST /api/small-model/generate` (axum `post`))
- [x] COMPATIBILITY 加 `api.small-model.v1`
- [x] 新增依赖 `serde_yaml = "0.9"` (workspace + oc-server, 给 opencode config YAML 文件备用) +
      `jsonc-parser = { version = "0.33", features = ["serde"] }` (workspace + oc-server,
      给 opencode config JSONC 解析)
- [x] 测试串行化 (`auth.rs` 测试 `pub(crate) static TEST_LOCK` + `HomeGuard`/`set_temp_home()` `pub(crate)`,
      `config.rs` 测试通过 `use crate::opencode::auth::tests as auth_tests` 共享同一把锁,
      跨模块 HOME env 串行化避免并行 test 跑时污染;
      `models_metadata.rs` 测试用 `tokio::sync::Mutex` 跨 await 安全序列化全局 STATE,
      所有 `assert_eq!(x, true/false)` 重写为 `assert!(x)` / `assert!(!x)` 避免 `clippy::bool_assert_comparison`)
- [x] `cargo test` 461/461 通过 (新增 53 测试:
      opencode::auth 6 + opencode::config 22 + opencode::models_metadata 3 +
      small_model::resolve 8 + small_model::call 4 + small_model::index 5,
      计划目标 38 → 超出 39% 因为额外加了 jwt/call/edge case 覆盖), clippy 0 警告

**阶段 3c Group 3 — 功能模块: session-assist + session-goal** (完成):
- [x] `session_assist` 子模块 (busy→idle 后 60s 静默期生成 recap + suggestion,
      写入 `metadata.openchamber.assist`)
  - [x] `session_assist/metadata.rs`: `AssistMetadata` (`recap`/`suggestion`/`forMessageID`/`generatedAt`)
        camelCase, `RECAP_CHAR_LIMIT=320` + `SUGGESTION_CHAR_LIMIT=500` clamp,
        `merge_assist_into_openchamber()` 保留其他 openchamber 子字段
  - [x] `session_assist/mod.rs`: `SessionAssistRuntime` (`timers: Mutex<HashMap<JoinHandle>>` +
        `inflight: HashSet` + `stopped: AtomicBool`), 全局 `Weak<AppState>` 注入,
        `start()` 启动 GlobalHub consumer, `process_payload()` 事件分发
        (`session.status:idle` → arm 60s, 其他 → clear, user 消息 createdAt ≥ armedAt → clear),
        `arm_timer` 取消旧 handle + `tokio::spawn(sleep → generate)`,
        `generate_assist` 单飞 (`inflight` HashSet) + sub-agent skip (`parentID` truthy) +
        settings 开关 (`sessionRecapEnabled`/`sessionSuggestionEnabled` 默认 true) +
        tail-moved-on 检查 (re-fetch + 比 `lastAssistantInfo.id`) +
        strict-JSON system prompt (`build_assist_system_prompt`,
        按 `(recap, suggestion)` 4 种组合输出 shape + 示例 1 + 示例 2) +
        `parse_generate_response` trailing JSON 提取 + 字段长度 clamp +
        Cyrillic/CJK/Devanagari/Arabic 脚本语言清洗 (防 small-model 偏离对话语言)
- [x] `session_goal` 子模块 (持久化目标 + audit verdict + auto-continuation,
      终结时广播 `openchamber:session-goal.settled` SSE)
  - [x] `session_goal/objectives.rs`: `GOAL_OBJECTIVE_CHAR_LIMIT=5000`,
        `goals_dir()` + `write_objective` + `read_objective` + `delete_objective` +
        `is_session_goal_enabled()`, path 校验 (URL-safe token 4-128 chars),
        缺失文件 404, `tokio::fs` + 进程级 env (与 Node 一致)
  - [x] `session_goal/audit.rs`: `Verdict` enum (Continue/Complete/Blocked) + `as_str`/`parse`,
        `build_audit_system_prompt()` 严格 JSON `{"verdict", "note"}` 指令 + `note ≤ 200 字符`,
        `extract_json_object()` fence stripping + tail `{...}` 扫描,
        `script_mismatch` Cyrillic/CJK/Devanagari/Arabic 检测 (与 session-assist 共用),
        `parse_audit_outcome()` 解析 + fallback
  - [x] `session_goal/continuation.rs`: `MAX_AUTO_TURNS=20`,
        `escape_xml_text()` (`&` → `&amp;`, `<` → `&lt;`, `>` → `&gt;`),
        `GoalSnapshot { objective, tokens_used, token_budget, turns_used }`,
        `build_continuation_prompt()` 渲染 objective + budget + 状态提示
  - [x] `session_goal/metadata.rs`: `GoalStatus` enum + `GOAL_STATUSES` 常量 slice,
        `GoalMetadata` 17 字段 camelCase (含 `tokenBudget: Option<u64>`),
        `parse_goal_metadata()` `Number.isFinite > 0` + floor 逻辑,
        `merge_goal_into_session_metadata()` + `goal_to_value()` + `is_active()`
  - [x] `session_goal/persistence.rs`: `read_session_openchamber_metadata()` GET →
        提取 `metadata.openchamber` namespace, `patch_session_openchamber_metadata()`
        GET → merge → PATCH (保留 dismissals/review 等其他子字段),
        纯 helper `merge_key_into_openchamber` + `merge_map_into_openchamber` 供
        session-assist 复用
  - [x] `session_goal/mod.rs`: `SessionGoalRuntime` (`timers` + `inflight` + `stopped`),
        常量 `IDLE_QUIET_MS=15_000` + `KICKOFF_QUIET_MS=3_000` +
        `RESUME_KICKOFF_MS=250` + `BLOCKED_STREAK_LIMIT=3` + `AUDIT_FAIL_LIMIT=2`,
        事件分发 (idle → arm, aborted assistant → `pause_after_abort`,
        session.updated → kickoff/resume kickoff),
        `tick` 完整状态机 (单飞 → 终态判定 → 抓 transcript → audit → 决策:
        complete / blocked × N / audit fail × N / 构造 continuation → POST
        `/session/{id}/prompt_async`),
        token accounting (segmented snapshot with `summary:true` 段落分片,
        维护 `tokensBaseline` + `tokensCommitted` + `tokensUsed`,
        `account_tokens` 边界 ≤ goal.created_at 排除 post-goal 旧消息),
        `settle_goal` 终结时持久化 status + 广播 SSE
  - [x] `session_goal/routes.rs`: 3 axum handler (`PUT`/`GET`/`DELETE`
        `/api/goals/objective/{session_id}`, `State<Arc<AppState>>` + `ApiResult<Json<Value>>`,
        GET 缺失返回 404 + `{error}`, DELETE best-effort 容错)
- [x] `opencode/session_client.rs`: `OpenCodeClient` struct (5 方法 —
      `fetch_session` / `fetch_session_messages` / `patch_session_metadata` /
      `prompt_async` / `create_session`, 10s timeout, `Result<Option<Value>, ApiError>`
      GET 形态 + `Result<(), ApiError>` 写形态, 与 `permission_auto_accept::fetch_session_info` 复用模式),
      `build(state: &AppState)` constructor (`reqwest::Client` + `base_url` + `auth_header`
      复用 AppState), mock HTTP 测试用 `tokio::net::TcpListener`,
      供 G4 (scheduled-tasks) + G5 (skills-catalog) 复用
- [x] AppState 扩展 (`state.rs`: `session_assist: Arc<SessionAssistRuntime>` +
      `session_goal: Arc<SessionGoalRuntime>`, `init_session_assist()` +
      `init_session_goal()` 在 `set_opencode_ready` 后调用启动 GlobalHub consumer)
- [x] 路由注册 (`main.rs`: `/api/goals/objective/{session_id}` axum chained
      `put(get/delete)` 在 magic-prompts 之后、SSE proxy 之前,
      `session_assist` + `session_goal` 无新 HTTP 路由 — 状态走 `metadata.openchamber.{assist,goal}`)
- [x] 错误桥修复 (`error.rs`: `ApiError` derive `Debug` + 手写 `Display` 让
      `tracing::warn!`/`format!` 可用 + `From<oc_core::Error>` 保留)
- [x] 复用现有模式 (Arc<Runtime> + start() GlobalHub consumer 复用
      `permission_auto_accept` + `notifications/trigger`,
      单飞 `Mutex<HashSet<String>>` 复用 `permission_auto_accept`,
      防抖 timer 复用 `notifications/trigger`,
      `crate::small_model::index::generate_small_model_text` 给两个 runtime 调 audit/recap,
      `crate::github::settings::read_settings` 读启用开关,
      `crate::github::settings::data_dir()` 给 objectives 文件目录,
      `crate::notifications::emitter::broadcast_ui_notification` 给 goal 终结通知,
      `state.{opencode_base_url, opencode_auth_header, http_client}` 给 session_client,
      `auth::TEST_LOCK` 跨模块串行化 HOME/OPENCHAMBER_DATA_DIR env 污染防护)
- [x] COMPATIBILITY 加 `api.session-assist.v1` + `api.session-goal.v1`
- [x] 测试串行化 (`session_goal/objectives.rs` 测试 `with_temp_data_dir` 复用
      `auth::TEST_LOCK`, 与 `config::with_temp_home` 共享同一把锁,
      跨模块 env 污染防护, `await_holding_lock` 抑制因锁需跨越 async 测试体)
- [x] `cargo test` 547/547 通过 (新增 86 测试:
      opencode::session_client 11 + session_assist::metadata 6 + session_assist::mod 13 +
      session_goal::objectives 10 + session_goal::audit 9 + session_goal::continuation 6 +
      session_goal::metadata 11 + session_goal::persistence 5 + session_goal::mod 15),
      clippy 0 警告 (3 处 `#[allow(clippy::too_many_arguments)]` 因状态机参数聚合是合理的)

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

**阶段 3d Group 1 — 功能模块: TTS** (完成):
- [x] TTS 模块 6 个路由 (`tts/`: voice token / speech synthesis / say status/speak / STT transcribe,
      与 Node `packages/web/server/lib/tts/routes.js` 契约对齐)
- [x] base URL 规范化 (`tts/base_url.rs`: `normalize_custom_openai_base_url()` 校验 scheme/credentials/localhost/remote,
      `is_remote_allowed()` env flag, `LOCAL_BASE_URL_HOSTS` 白名单, URL 重建 (去 fragment/query/trailing slash))
- [x] TTS service (`tts/service.rs`: OpenAI-compatible TTS API caller, `get_openai_api_key()` 多级回退
      (env → auth.json access/token/string), 语音常量列表, `SpeechOptions` + `generate_speech_stream()` 返回 MP3)
- [x] STT service (`tts/stt.rs`: OpenAI-compatible `/v1/audio/transcriptions` 调用,
      手动 multipart/form-data 构造 (不依赖 reqwest multipart), `{text}`/`{transcript}` 双回退)
- [x] macOS `say` 能力探测 (`tts/capability_runtime.rs`: 参数化 `detect_say_tts_capability_impl(platform, run_cmd)`,
      `say -v "?"` 输出解析 regex `^(.+?)\s+([a-zA-Z]{2}_[a-zA-Z]{2,3})\s+#`, lazy 缓存)
- [x] 语音摘要 (`tts/summarize.rs`: `SUMMARIZE_CHAR_LIMIT=2560`, `split_summarize_paragraphs()` 空行/段落分割,
      `NUM_CONSECUTIVE_BLANK_LINES=2`, `MAX_BULLET_POINTS=6`)
- [x] 6 个 axum handler (`tts/routes.rs`: `POST /api/voice/token` (stub: 返回空 token) +
      `POST /api/tts/speak` (MP3 bytes) +
      `GET /api/tts/status` + `GET /api/tts/say/status` + `POST /api/tts/say/speak` (`say` CLI) +
      `POST /api/stt/transcribe`)
- [x] COMPATIBILITY 加 `api.tts.v1`
- [x] `cargo test` 24 TTS 测试 (base_url 12 + service 8 + capability_runtime 4 + routes 8 + stt 3 + summarize 5),
      所有 TTS 测试通过

**阶段 3d Group 2 — 功能模块: Quota** (完成):
- [x] Quota 模块 7 个路由 (`quota/`: provider 列表 / credential 管理 / quota 查询,
      与 Node `packages/web/server/lib/quota/routes.js` 契约对齐)
- [x] Provider 抽象 (`quota/providers/mod.rs`: 17 provider 实现 dispatch,
      统一 `fetch_quota()` 签名 `(credentials, settings, http_client) → Result<QuotaResult, String>`,
      各 provider 适配各自 API 差异 (auth 格式 / 用量字段 / 重置周期))
- [x] Credential 管理 (`quota/credentials/`: managed credentials store + 4 标准 provider 认证,
      `$DATA_DIR/anti-captcha-credentials.json` mode 0o600 原子写, GET 脱敏返回值)
- [x] Utils (`quota/utils/`: auth 读取 + timestamp/transformers + formatters)
- [x] 7 个 axum handler (`quota/routes.rs`: provider 列表 + credential CRUD + validate + import + quota 查询)
- [x] COMPATIBILITY 加 `api.quota.v1`

**阶段 3d Group 3 — 功能模块: Scheduled Tasks** (完成):
- [x] Scheduled Tasks 模块 5+1 路由 (`scheduled_tasks/`: projects CRUD + task run + global status + SSE,
      与 Node `packages/web/server/lib/scheduled-tasks/routes.js` 契约对齐)
- [x] 调度引擎 (`scheduled_tasks/schedule.rs`: `compute_next_run_at` cron/daily/weekly/once 4 模式,
      `parse_scheduled_command_prompt`, `format_scheduled_session_title`, `parse_time_parts`)
- [x] Task 执行 (`scheduled_tasks/execution.rs`: `run_task_with_watchdog` 超时 + OpenCode prompt_async,
      `select_session` 协议, `MAX_TASK_TITLE_LENGTH`, `COMPACT_SUMMARY_SNIPPET_LIMIT`,
      `emit_called_on_status_change` 回调 + broadcast)
- [x] 项目配置 (`scheduled_tasks/project_config.rs`: projects JSON 文件读写, upsert/list/get/delete-by-id)
- [x] 运行时 (`scheduled_tasks/runtime.rs`: `ScheduledTasksRuntime` 全局定时器 + 状态线程安全,
      启动时对所有项目添加定时任务, `run_now`/`get_status`)
- [x] COMPATIBILITY 加 `api.scheduled-tasks.v1`

**阶段 3d Group 4 — 功能模块: Skills Catalog** (完成):
- [x] Skills Catalog 模块 12 个路由 (`skills_catalog/`: 技能发现 / 来源扫描 / 安装 / CRUD / 文件管理,
      与 Node `packages/web/server/lib/opencode/skill-routes.js` 契约对齐)
- [x] Git 源解析 (`skills_catalog/source.rs`: SSH/HTTPS/shorthand 三种格式, `parse_skill_repo_source`,
      `is_clawdhub_source`)
- [x] Git 执行 (`skills_catalog/git.rs`: `GitRunner` trait + `DefaultGitRunner` (system git),
      `looks_like_auth_error`, `assert_git_available`)
- [x] 扫描 (`skills_catalog/scan.rs`: `scan_skills_repository` 浅克隆→稀疏检出→SKILL.md 发现,
      `is_valid_skill_name` 校验, frontmatter 解析, TTL 缓存)
- [x] 安装 (`skills_catalog/install.rs`: `install_skills_from_repository` 克隆→稀疏检出→复制到目标目录,
      冲突检测/解决策略, `get_target_skill_dir` scope/source → 目录映射)
- [x] 本地技能文件 (`skills_catalog/skills.rs`: Skill filesystem CRUD, `discover_skills`, `get_skill_sources`,
      supporting file 操作 (含 path traversal 防护))
- [x] 内存 TTL 缓存 (`skills_catalog/cache.rs`: `HashMap<Mutex>` + 30min TTL, `get_cached_scan`/`set_cached_scan`/`clear_cache`)
- [x] 12 个 axum handler (`skills_catalog/routes.rs`: list/catalog/source/scan/install/
      get/create/update/delete + file read/write/delete)
- [x] COMPATIBILITY 加 `api.skills-catalog.v1`
- [x] 所有测试通过 (skills_catalog 14 + 全量 778 测试)

**阶段 3e Group 1 — projects/ 正确性修复 (scheduled_tasks 对齐)** (完成):

> 针对阶段 3d G3/G4 移植的 scheduled_tasks 模块, 与 Node `projects/project-config.js` +
> `projects/project-id.js` 逐项对比, 修复 **8 个严重 gap**。

- [x] **GAP A — 文件路径对齐** (`project_config.rs`):
      flat 布局 `{projectId}.json` 替代 nested `{projectId}/scheduled-tasks.json`;
      `default_projects_dir` → `user_config_root().join("projects")` (Node 兼容根 `~/.config/openchamber/projects`)
- [x] **GAP B — 验证/clamp 移植** (新建 `normalize.rs`, ~700 行):
      Node `project-config.js` 全套纯函数 — `clamp_length`, `normalize_status`,
      `normalize_time_value` (HH:mm), `normalize_date_value` (YYYY-MM-DD round-trip),
      `normalize_weekdays` (0-6 unique sorted), `resolve_schedule_times`,
      `normalize_timezone` (chrono-tz IANAZone 验证),
      `validate_cron_expression` (5-field 输入校验 → 7-field 转换),
      `normalize_schedule` (daily/weekly/once/cron), `normalize_execution`,
      `normalize_state`, `normalize_task_for_storage`, `normalize_task_for_read`
- [x] **GAP C — 时区数学** (`schedule.rs`):
      `parse_in_tz` / `local_now_with_tz` 真正使用 `tz_name` (chrono-tz `Tz` 类型);
      非 UTC 时区 (America/New_York, Asia/Shanghai, Europe/Paris) 触发时间正确
- [x] **GAP D — cron compute_next_run_at** (`schedule.rs`):
      5-field 表达式 prepend `0 ` + append ` *` → 7-field;
      `cron::Schedule::after(&min_allowed)` 从 `now + TASK_DUE_SLACK_MS` 迭代 (非系统时间)
- [x] **GAP E — updatedAt 位置** (`project_config.rs` + `routes.rs`):
      `update_scheduled_task_state` 把 `updatedAt` 写入 `task.state.updatedAt` (非顶层);
      经 `normalize_state` 归一化 (含 `lastError` clamp + `lastStatus` 校验)
- [x] **GAP F — sibling keys 保留** (`project_config.rs`):
      `write_merged` 读现有 raw config → 仅替换 `version` + `scheduledTasks` → 原子写回;
      `projectNotes` / `projectTodos` / `projectActions` 等兄弟字段不再丢失
- [x] **GAP G — Task ID 生成** (`normalize.rs`):
      `normalize_task_for_storage` 内 `existing_id.or(incoming_id).unwrap_or_else(uuid::Uuid::new_v4)`
- [x] **GAP H — project-id.js 移植** (新建 `project_id.rs`):
      `create_project_id_from_path` → `path_{base64url(normalized_path)}`
- [x] **安全加固** (`project_config.rs`):
      `is_valid_project_id` 显式拒绝 `.` 和 `..` (path traversal); `sanitize_id` 限制文件名字符集
- [x] 新增依赖: `chrono-tz = "0.10"` (内嵌 tzdata, 无系统依赖) + `cron = "0.15"`
- [x] 新增 `user_config_root()` (`github/settings.rs`): `~/.config/openchamber` (硬编码, 不读 `OPENCHAMBER_DATA_DIR`, 与 Node 一致)
- [x] 测试隔离修复 (`routes.rs`): `TempDirWithEnv` Drop 守卫, 保持 `OPENCHAMBER_DATA_DIR` 跨测试不污染
- [x] 所有 scheduled_tasks 测试通过 (89/89); 我改动的文件 cargo clippy 0 warnings

**阶段 3e Group 2 — security 补全: 全局认证门 + 路径边界** (完成):

> 修复 Rust 后端最大安全漏洞: **完全无全局认证中间件** (Node `app.use('/api', requireApiAuth)` 在 Rust 侧缺失)。
> 另修复 fs/list + fs/serve 缺工作区边界校验。

- [x] **Gap 1 (CRITICAL) — 全局 `/api/*` 认证中间件** (新建 `middleware/auth.rs`):
      `require_api_auth` 用 `axum::middleware::from_fn_with_state` 挂为顶层 Router layer;
      对齐 Node `requireApiAuth` (core-routes.js:595-609) 的完整决策链:
      公开路由白名单 → OPTIONS 豁免 → preview-proxy 旁路 → tunnel scope 分类 →
      UI auth (session cookie JWT → url_token → bearer → 无密码放行) → 401
- [x] **Gap 2 (HIGH) — fs/list + fs/serve 工作区边界**:
      `list` 和 `serve` handler 补 `resolve_workspace_path` 校验;
      防止认证用户读取工作区外任意文件 (`~/.ssh/id_rsa`, `/etc/passwd` 等)
- [x] **Gap 3 (HIGH) — WS 端点认证**: 由全局中间件覆盖 upgrade 请求
      (axum 中间件在 `WebSocketUpgrade` extractor 前运行; url_token 支持 WS 路径白名单)
- [x] 提升现有 ui_auth helper 为中间件复用: `has_valid_session`, `authenticate_client`,
      URL-token 白名单函数 (`can_use_url_auth_token_for_request` 等) 去掉 `#[allow(dead_code)]`
- [x] 公开路由白名单: health/version/system-info/connect/auth-session/passkey-auth/
      pairing-redeem/OPTIONS (显式匹配, 比 Node 注册顺序更安全)
- [x] 全量测试通过 (822 passed, 0 failed); 我改动的文件 cargo clippy 0 warnings

**阶段 3e Group 2.5 — 测试隔离修复** (完成):

> 3 个测试模块在并行执行时修改进程级 env var, 导致间歇性失败 (6-11 个测试)。

- [x] `scheduled_tasks/routes.rs`: `TempDirWithEnv` 未持有跨模块共享锁 → 持有
      `auth::tests::TEST_LOCK` + 同步设置 `HOME`
- [x] `quota/credentials/store.rs`: `with_temp_data_dir` 未持有共享锁 → 持有 `TEST_LOCK` 直到函数返回
- [x] `tts/base_url.rs`: `lock_env_block_remote()` 内 `TEST_LOCK` guard 在函数返回时立即释放
      (`let _g = ...`) → `EnvGuard` 持有 `MutexGuard<'static, ()>` 直到 drop
- [x] 全量测试通过 (822 passed, 0 failed, 并行); clippy 0 warnings

**阶段 3e Group 3 — event-stream 合成事件转发** (完成):

> `SessionStateRuntime` 已完整实现 (646 行) 但 `subscribe_events()` 从未被消费,
> 合成的 `openchamber:session-status` / `openchamber:session-activity` 事件全部丢弃。
> Node 侧 (`index.js:447-451`) 把 `broadcastGlobalUiEvent` 注入 sessionRuntime,
> 同时扇出到 SSE 通知客户端和 WS 消息流客户端。

- [x] **C1 (CRITICAL) — SSE 通知流**: `state.rs` 新增 `SessionStateRuntime` 合成事件 consumer task,
      订阅 `subscribe_events()` → `emitter.write_sse_event()` (SSE 通知流)
- [x] **C2 (CRITICAL) — 全局 WS 桥**: `global_hub.rs` 新增 `broadcast_synthetic()` 方法,
      consumer task 同时调 `global_hub.broadcast_synthetic()` → 现有 `run_global_bridge` 自动收到合成事件;
      合成事件无 event_id, 不进 replay ring
- [x] **C3 (MAJOR) — 目录 WS 桥**: `ws_bridge.rs` 新增 `emit_synthetic_session_events()`,
      在 `run_directory_bridge` 的 `UpstreamEvent::Event` 分支对 `session.status` 本地合成两个帧
      (目录桥用 per-connection reader, 不走全局 hub, Part 1 的 broadcast 到不了)
- [x] `emitter.rs`: `write_sse_event` 改 pub (consumer task 需跨模块调用)
- [x] 全量测试通过; 我改动的文件 cargo clippy 0 warnings
