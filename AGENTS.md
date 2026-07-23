# OpenChamber - AI Agent Reference

## Core purpose

OpenChamber provides UI runtimes (web/desktop/VS Code) for interacting with an OpenCode server (local auto-start or remote URL). Official OpenCode traffic goes through `@opencode-ai/sdk`; OpenChamber-owned runtime capabilities go through `RuntimeAPIs`, `runtimeFetch`, and browser/realtime URL helpers.

## Runtime architecture (IMPORTANT)

There are two desktop shells in active development:

- **Tauri (`rust/oc-tauri`)** — the migration target. Default mode embeds the Rust `oc-server` (axum) **in-process** via `oc_server::OcServer::start(config)` (same single-process model as Electron, but Rust+axum instead of Node+Express). `OPENCHAMBER_SIDECAR=1` switches to a sidecar fallback that spawns the `@openchamber/web` CLI as a subprocess (decision: `rust/oc-tauri/src-tauri/src/backend.rs:use_sidecar()`). Migration status is tracked in `rust/README.md` (authoritative, phase-by-phase).
- **Electron (`packages/electron`)** — legacy, still the shipped desktop release. Boots the web server **in the same Node process** as the Electron main, then loads the web UI from `http://127.0.0.1:<port>`. No sidecar subprocess.

Backend/domain logic for the Node path lives in `packages/web/server/*` (and `packages/vscode/*` for VS Code bridge/runtime parity). The Rust port of the same backend lives in `rust/oc-server/src/*`. The desktop shell owns the security boundary: windows, menus, dialogs, notifications, updater, deep-links, runtime host switching, local IPC gates, and SSH/tunnel management.
- Do not add OpenCode feature backends to the native shell. Shared UI features should remain server/runtime APIs unless the capability is inherently native.

### Startup latency sources (Tauri desktop)

Cold-starting the Tauri desktop shell, the entire splash window is dominated by four cost categories stacked in series. Before touching any startup-pipeline code, re-read this table and re-measure on the current branch — the numbers below are anchored to a single `bun run tauri:dev` run on 2026-07-22 (`/tmp/tauri-dev-verify.log`).

| #  | Bottleneck                                                | File:line                                                 | Measured window                            | Notes |
|----|-----------------------------------------------------------|-----------------------------------------------------------|--------------------------------------------|-------|
| 1  | Cargo cold compile of Tauri shell + oc-server (5 crates)  | `scripts/tauri-dev.mjs:282-290` → `cargo tauri dev` (which internally runs `cargo run --no-default-features --features vibrancy`) | ~10 s cold; ~1–3 s warm                    | Top contributor. Dev debug build, not release. |
| 2  | `OcServer::start` binds TCP listener AFTER OpenCode is healthy (serial ordering) | `rust/oc-server/src/lib.rs:89-105`                        | ~0–3 s (OpenCode spawn ≈ 2 s)              | The Node Express path (`packages/web/server/lib/opencode/startup-pipeline-runtime.js:100-102`) binds first then fires OpenCode. The Rust port inverted this when it landed in `ca3ea2f7 feat(tauri): Vite-only dev launcher (drop Node Express backend)`. Structural regression — fix in a separate PR. |
| 3  | `SessionAuthGate` runs parallel fetches with no `AbortSignal.timeout` | `packages/ui/src/components/auth/SessionAuthGate.tsx:99-108, 420-488` | unbounded if backend stalls                 | Adding `AbortSignal.timeout(2000)` is the minimum viable guardrail. Auth-boundary change → own PR. |
| 4  | Fixed-port alignment via `GRIDFORGE_PORT` across launcher → Vite proxy → Rust oc-server → early injection | `scripts/tauri-dev.mjs:35-41,287-294` → `packages/web/vite.config.ts:46-49,104-119` → `rust/oc-server/src/config.rs:28` → `rust/oc-tauri/src-tauri/src/ipc/globals.rs:285` | ~0 ms (config-time)        | Correct as-is. `scripts/tauri-dev.mjs` resolves `GRIDFORGE_PORT ?? OPENCHAMBER_PORT ?? 3001` once and propagates the same value to (a) Vite's proxy targets for `/api`, `/auth`, `/health`, (b) Rust `oc-server` for fixed-port binding (no longer dynamic), and (c) Tauri early-injection (`build_early_globals_script`) so `window.__GRIDFORGE_API_BASE_URL__` is correct from page load. If any of these reads diverge, /api requests ECONNREFUSED (see Vite proxy error in `/tmp/tauri-dev-verify.log`). `OPENCHAMBER_PORT` is preserved as a legacy fallback for old user scripts. |

**Critical cross-check facts (do not re-derive these):**

- The OpenCode subprocess reaches `healthy` in **~2 seconds**, not the 3–7 s previously assumed. Do not make "OpenCode is slow to start" the headline diagnosis.
- The dominant cost is always **cargo cold compile**. Tauri contributors eat this on every cold start.
- Spec re-baselining: `docs/superpowers/specs/2026-07-22-tauri-splash-latency-design.md`.

**Self-check checklist before modifying the startup pipeline:**

1. Run `bun run tauri:dev` cold, capture stdout/stderr to a log. Time `Running DevCommand` → `spawning opencode` — that gap is usually the largest phase.
2. If the OpenCode timing looks suspect, set `OPENCODE_DEBUG=1` and re-check the `opencode listening` line against the `opencode is healthy and ready` line.
3. Confirm whether oc-server is in **in-process** mode (default) or **sidecar** mode (`OPENCHAMBER_SIDECAR=1`, decision site `rust/oc-tauri/src-tauri/src/backend.rs:use_sidecar()`). The two paths have different ordering invariants.
4. Read the spec linked above before writing code. The 2026-07-22 baseline is the only one currently considered accurate.

**Splash UX spec:**

- `packages/web/index.html:519-625` (`#initial-loading`) is the single source of the desktop and web splash.
- In Tauri (detected via `window.__GRIDFORGE_LOCAL_ORIGIN__`), if the splash is still mounted at T+3 s, populate `<div id="initial-loading-status">` (placed directly below the cube SVG) with `Warming up OpenCode…`. Color via the existing splash typography tokens; do not recolor the cube SVG — it is brand-owned.
- The status node is removed together with `#initial-loading` by the dismissal effect in `App.tsx:322-349`. No manual cleanup needed.
- The 10 s hard fallback at `index.html:608-625` is unchanged. It still no-ops in Tauri.
- English-only copy for now. If translation is needed later, lift it into `packages/ui/src/lib/i18n` — the `id="initial-loading-status"` selector is the stable contract.

### Desktop Shell

- **Tauri (migration target):** `rust/oc-tauri/src-tauri/src/`. `lib.rs::run()` builds the app; `setup` spawns the backend (in-process oc-server or sidecar), injects `window.__OPENCHAMBER_DESKTOP__` via `init_main_window`, applies macOS vibrancy, and installs SIGTERM/SIGINT cleanup. Modules: `backend.rs` (handle enum + sidecar decision), `ipc/` (window/system/dialog/shell commands), `tray.rs` (breathing animation), `ssh/` (ControlMaster, 1:1 port of `ssh-manager.mjs`), `settings.rs`, `power.rs`, `updater.rs`, `menu.rs`. Config: `rust/oc-tauri/src-tauri/tauri.conf.json`. Capabilities ACL: `capabilities/default.json`.
- **Electron (legacy):** `packages/electron/`. Desktop-side changes (IPC handlers, native integrations, window/quit/notification behavior) land in `packages/electron/main.mjs` + `packages/electron/preload.mjs`. Electron imports the server via `@openchamber/web/server/index.js` (workspace dep) and calls `startWebUiServer({...})`. The returned handle has `getPort()` / `stop()`. Notifications flow via an `onDesktopNotification` callback injected at startup — no stdout-parsing IPC.
- Windows OS integrations must avoid console-window flashes. Any non-user-visible `child_process` call on Windows (system probes, tool discovery, updater/install helpers, SSH/tunnel helpers, cleanup, etc.) should run the target executable directly with `windowsHide: true`; detached/background helpers usually also need `stdio: 'ignore'`. Avoid `cmd.exe /c` pipelines and wrappers that spawn console grandchildren (`taskkill`, `ping`, nested `powershell`, batch shims), because `windowsHide` only reliably applies to the first child. If a delayed/background operation must outlive the app process, use a single hidden first-level helper (for example `powershell.exe -WindowStyle Hidden -EncodedCommand ...`) or a native Node/Electron API. Only omit this for intentionally user-visible shells/apps.
- Build/release: Electron is the current desktop release target. Tauri is the migration target (Phase 4B complete; see `rust/README.md`).

## Tech stack

- Package manager: pnpm (`package.json` `packageManager: pnpm@11.12.0`, lockfile `pnpm-lock.yaml`, workspaces `pnpm-workspace.yaml`). Bun is used as a task runner (`bun run`, `bun x`) but is NOT the package manager.
- Node >=22 (`package.json` `engines`)
- UI: React, TypeScript, Vite, Tailwind v4
- State: Zustand stores and sync layer (`packages/ui/src/stores/`, `packages/ui/src/sync/`)
- UI primitives: Base UI (`@base-ui/react`, primary source for dropdown/select/dialog/menu/tooltip/etc. — wrappers live in `packages/ui/src/components/ui/`), Radix UI (`package.json` deps, legacy usages being migrated), HeroUI (`package.json` deps), Remixicon as SVG sprite source only (use shared `Icon`, never direct `@remixicon/react` imports)
- Server (Node): Express (`packages/web/server/index.js`)
- Server (Rust): axum (`rust/oc-server`, Rust port of the Express server; edition 2021, MSRV 1.85)
- Desktop (migration target): Tauri 2.11 (`rust/oc-tauri`)
- Desktop (legacy): Electron 41 (`packages/electron/`)
- VS Code: extension + webview (`packages/vscode/`)

## Monorepo layout

Node workspaces are `packages/*` (see `package.json`, `pnpm-workspace.yaml`). The Rust migration lives in a separate Cargo workspace under `rust/` (see `rust/Cargo.toml`).

Node packages:
- Shared UI: `packages/ui`
- Web app + server + CLI: `packages/web`
- Desktop shell (legacy): `packages/electron`
- VS Code extension: `packages/vscode`

Rust workspace (`rust/`, separate from pnpm):
- `rust/oc-core` — shared types/errors (`Error` with `http_status()`/`to_json()`)
- `rust/oc-opencode-sdk` — OpenCode server-side client (replaces `@opencode-ai/sdk` server-side usage)
- `rust/oc-server` — axum backend binary + lib, the Rust port of `packages/web/server` (Express → axum)
- `rust/oc-tauri/src-tauri` — Tauri desktop shell, the Rust replacement for `packages/electron`

## Documentation map

Before changing any mapped module, read its module documentation first.

### web

Web runtime and server implementation for OpenChamber.

#### lib

Server-side integration modules used by API routes and runtime services.

##### event-stream

OpenChamber-owned event stream helpers for server-sent runtime events.

- Module docs: `packages/web/server/lib/event-stream/DOCUMENTATION.md`

##### fs

Filesystem routes, raw file access, search helpers, and workspace-scoped file operations.

- Module docs: `packages/web/server/lib/fs/DOCUMENTATION.md`

##### quota

Quota provider registry, dispatch, and provider integrations for usage endpoints.

- Module docs: `packages/web/server/lib/quota/DOCUMENTATION.md`

##### git

Git repository operations for the web server runtime.

- Module docs: `packages/web/server/lib/git/DOCUMENTATION.md`

##### github

GitHub authentication, OAuth device flow, Octokit client factory, and repository URL parsing.

- Module docs: `packages/web/server/lib/github/DOCUMENTATION.md`

##### opencode

OpenCode server integration utilities including config management, provider authentication, and UI authentication.

- Module docs: `packages/web/server/lib/opencode/DOCUMENTATION.md`

##### notifications

Notification message preparation utilities for system notifications, including text truncation and optional summarization.

- Module docs: `packages/web/server/lib/notifications/DOCUMENTATION.md`

##### permission-auto-accept

Persistent server-owned permission auto-accept policy, subagent inheritance, retries, and reconnect reconciliation.

- Module docs: `packages/web/server/lib/permission-auto-accept/DOCUMENTATION.md`

##### scheduled-tasks

Scheduled task persistence, execution, and event fanout for recurring sessions.

- Module docs: `packages/web/server/lib/scheduled-tasks/DOCUMENTATION.md`

##### text

Text processing helpers shared by server-side routes and summarization flows.

- Module docs: `packages/web/server/lib/text/DOCUMENTATION.md`

##### terminal

WebSocket protocol utilities for terminal input handling including message normalization, control frame parsing, and rate limiting.

- Module docs: `packages/web/server/lib/terminal/DOCUMENTATION.md`

##### tts

Server-side text-to-speech services and summarization helpers for `/api/tts/*` endpoints.

- Module docs: `packages/web/server/lib/tts/DOCUMENTATION.md`

##### relay

Host side of the private relay: outbound E2EE tunnel that lets remote clients reach this instance through OpenChamber-hosted relay infrastructure without inbound exposure. Load the `relay-transport` skill before changing it or any WebSocket/streaming endpoint that rides it.

- Module docs: `packages/web/server/lib/relay/DOCUMENTATION.md`

##### tunnels

Tunnel provider setup and runtime helpers for exposing OpenChamber over remote URLs.

- Module docs: `packages/web/server/lib/tunnels/DOCUMENTATION.md`

##### ui-auth

UI session auth, client tokens, URL-token scoping, passkey/reset flows, and route-level auth gates.

- Module docs: `packages/web/server/lib/ui-auth/DOCUMENTATION.md`

##### skills-catalog

Skills catalog management including discovery, installation, and configuration of agent skill packages.

- Module docs: `packages/web/server/lib/skills-catalog/DOCUMENTATION.md`

### ui

Shared React UI, sync layer, runtime API contracts, and stores.

#### sync

Session synchronization, event pipeline, optimistic updates, caches, and live-state stores.

- Module docs: `packages/ui/src/sync/DOCUMENTATION.md`

#### stores

Zustand store ownership, persistence expectations, and store-splitting guidance.

- Module docs: `packages/ui/src/stores/DOCUMENTATION.md`

#### session sidebar

Session sidebar grouping, ordering, virtualization-adjacent behavior, and project/worktree display.

- Module docs: `packages/ui/src/components/session/sidebar/DOCUMENTATION.md`

#### message parts

Chat message part rendering and message-row performance expectations.

- Module docs: `packages/ui/src/components/chat/message/parts/DOCUMENTATION.md`

## Build / dev commands (verified)

All scripts are in `package.json`.

- Validate: `bun run type-check`, `bun run lint`
- Build all: `bun run build`
- Desktop build (Electron — current release): `bun run electron:build`
- Desktop dev (Electron, legacy): `bun run electron:dev`
- Desktop dev (Tauri, migration target): `bun run tauri:dev` (in-process oc-server; orchestrator: `scripts/tauri-dev.mjs` → `scripts/dev-web-hmr.mjs` + `cargo tauri dev`)
- Desktop dev (Tauri, sidecar fallback): `bun run tauri:dev:sidecar` (`OPENCHAMBER_SIDECAR=1`)
- Desktop build (Tauri): `bun run tauri:build` (orchestrator: `scripts/tauri-build.mjs` → builds web, stages to `rust/oc-tauri/ui-dist`, then `cargo tauri build`). Equivalent manual: `bun run build:web`, copy `packages/web/dist` → `rust/oc-tauri/ui-dist`, then `cargo tauri build` inside `rust/oc-tauri/src-tauri`. (`tauri.conf.json` `frontendDist: "../ui-dist"` resolves relative to `src-tauri`, i.e. `rust/oc-tauri/ui-dist`, not `rust/ui-dist`.)
- Rust build/test: `cargo build` / `cargo test` (run inside `rust/`)
- VS Code build: `bun run vscode:build`
- Release smoke build: `bun run release:test` (shell script: `scripts/test-release-build.sh`)

Note: `tauri:dev` Ctrl+C may print `error: script "dev:server:watch" exited with code 130`. This is benign teardown noise — SIGINT is forwarded through the nodemon child process; bun reports the non-zero exit as an error. Process-tree cleanup (`stopChildTree`) is unaffected.

## Runtime entry points

- Web bootstrap: `packages/web/src/main.tsx`
- Web server (Node): `packages/web/server/index.js`
- Web CLI: `packages/web/bin/cli.js` (package bin: `packages/web/package.json`)
- Desktop (Electron, legacy): `packages/electron/main.mjs` (boots the web server in-process via `startWebUiServer`, loads web UI over loopback; preload at `packages/electron/preload.mjs` exposes the desktop IPC bridge)
- Desktop (Tauri, migration target): `rust/oc-tauri/src-tauri/src/main.rs` → `lib.rs::run()` (embeds oc-server in-process or spawns it as sidecar; injects `window.__OPENCHAMBER_DESKTOP__` init script; config: `rust/oc-tauri/src-tauri/tauri.conf.json`)
- Rust backend: `rust/oc-server/src/main.rs` → `lib.rs::OcServer::start()` (thin shell: `tracing` init + `Config::load()` + `build_router` + ordered `shutdown()`; route registry in `lib.rs::build_router`)
- VS Code extension host: `packages/vscode/src/extension.ts`
- VS Code webview bootstrap: `packages/vscode/webview/main.tsx`

## OpenCode integration

- UI client wrapper: `packages/ui/src/lib/opencode/client.ts` (imports `@opencode-ai/sdk/v2`)
- Sync/event pipeline: app roots mount `SyncProvider` from `packages/ui/src/sync/sync-context.tsx`; OpenCode SSE/WS event handling lives in `packages/ui/src/sync/event-pipeline.ts`
- Web server embeds/starts OpenCode server: `packages/web/server/index.js` (`createOpencodeServer`)
- Web runtime filesystem endpoints: `packages/web/server/lib/fs/routes.js`, registered by `packages/web/server/lib/opencode/feature-routes-runtime.js`
- External server support: Set `OPENCODE_HOST` (full base URL, e.g. `http://hostname:4096`) or `OPENCODE_PORT`, plus `OPENCODE_SKIP_START=true`, to connect to existing OpenCode instance

## Key UI patterns (reference files)

- Settings shell: `packages/ui/src/components/views/SettingsView.tsx`
- Settings shared primitives: `packages/ui/src/components/sections/shared/`
- Settings sections: `packages/ui/src/components/sections/` (incl `skills/`)
- Chat UI: `packages/ui/src/components/chat/` and `packages/ui/src/components/chat/message/`
- Theme + typography: `packages/ui/src/lib/theme/`, `packages/ui/src/lib/typography.ts`
- Terminal UI: `packages/ui/src/components/terminal/` (uses `ghostty-web`)

## External / system integrations (active)

- Runtime API contracts: `packages/ui/src/lib/api/types.ts`; React consumption via `packages/ui/src/hooks/useRuntimeAPIs.ts`
- Runtime transport/auth: `packages/ui/src/lib/runtime-fetch.ts`, `packages/ui/src/lib/runtime-url.ts`, `packages/ui/src/lib/runtime-auth.ts`
- Git: `packages/ui/src/lib/gitApi.ts`, `packages/web/server/lib/git/service.js` (`simple-git`)
- Terminal PTY: `packages/web/server/lib/terminal/runtime.js` (`bun-pty`/`node-pty`)
- Skills catalog: `packages/web/server/lib/skills-catalog/`, UI: `packages/ui/src/components/sections/skills/`

## Agent constraints

- Do not modify `../opencode` (separate repo).
- Do not run git/GitHub commands unless explicitly asked.
- Keep baseline green (run `bun run type-check`, `bun run lint` before finalizing changes).
- **Session scope (this session only) — skip Electron / Mobile / VS Code checks and builds.** Do not run any check or build that scans, builds, or packages `packages/electron`, `packages/mobile`, or `packages/vscode` for the remainder of this session. Concretely, do not run: `bun run type-check`, `bun run lint`, `bun run build`, `bun run dead-code`, `bun run release:prepare`, `bun run release:test`, `bun run electron:*`, `bun run mobile:*`, `bun run vscode:*`, `bun run type-check:electron`, `bun run type-check:mobile`, `bun run lint:electron`, `bun run lint:mobile`, `bun run build:electron`, `bun run build:mobile`, or `bun run --filter '*' …` (which fans out to those packages). Tauri/Rust (`cargo build`, `cargo test`, `bun run tauri:*`) is **not** in this skip scope. When validation is needed, use the narrowly scoped commands instead:
  - UI: `bun run type-check:ui`, `bun run lint:ui`
  - Web: `bun run type-check:web`, `bun run lint:web`
  - Workspace filters: `bun run --filter @openchamber/ui …`, `bun run --filter @openchamber/web …`
  - Rust: `cargo build` / `cargo test` (inside `rust/`), `cargo tauri build` (inside `rust/oc-tauri/src-tauri`)
  - This constraint is **session-only** and does **not** alter the build/dev commands catalogue above, the validation expectations, or any project-skill triggers. Re-evaluate on the next session.

## Agent code of conduct

- Prefer the smallest correct change.
- Preserve working behavior before improving structure.
- Do not add cleverness where a direct implementation is enough.
- Do not infer critical state from weak signals when a stronger source exists.
- Do not encode policy only in UI; enforce it in core logic.
- Do not hide data loss, partial failure, or fallback behavior. Make it explicit in code.
- Finish work end-to-end: implementation, verification, and cleanup.

## Development rules

- Keep diffs tight; avoid drive-by refactors.
- Follow local precedent; inspect nearby code before introducing new patterns.
- Backend changes: keep web, desktop, and VS Code behavior consistent when they share contracts.
- TypeScript: avoid `any`, blind casts, and shape guessing.
- React: prefer function components + hooks; use classes only when required.
- Control flow: prefer early returns and explicit branching over nested ternaries.
- Styling: Tailwind v4, typography via `packages/ui/src/lib/typography.ts`, theme vars via `packages/ui/src/lib/theme/`.
- Shared UI patterns: reuse shared primitives before introducing feature-local markup patterns.
- Toasts: use the wrapper from `@/components/ui`; do not import `sonner` directly in feature code.
- No new deps unless asked.
- Never add secrets or log sensitive data.

## Architecture patterns

### Thin entrypoints, focused modules

- Keep orchestration entrypoints thin: `index.js`, bridge files, bootstrap files, provider roots.
- Move route, domain, and runtime logic into focused modules with clear ownership.
- Prefer dependency injection over hidden module coupling.
- Add or update module documentation when ownership changes.

### Strong source of truth

- Prefer deterministic state over heuristics.
- Use live server/session state for live activity. Do not let historical anomalies masquerade as current execution.
- If a fallback is necessary, scope it narrowly to the active entity and treat it as temporary.
- Restore derived UI state from authoritative records. Example: restore model or agent from the latest user message, not assistant-side guesses.

### Live state vs historical state

- Derive live UI behavior from live state channels, not persisted history.
- Use historical records to restore context, not to infer that work is still in progress.
- If live state is delayed, use the narrowest possible transient fallback and clear it as soon as authoritative state arrives.

### Cross-runtime parity

- If web defines a route or payload contract that shared UI depends on, keep VS Code and desktop parity where applicable.
- Shared behavior differences must be intentional and visible in code.
- Do not ship a web-only assumption into shared UI.

### Partial-failure-safe flows

- Cross-directory and multi-entity operations must tolerate partial failure.
- Prefer per-item results, rollback paths, or resumable cleanup over all-or-nothing assumptions.
- Never leave optimistic state or local caches stranded after failure.

### Distinguish fetch failure from empty success

Client API methods that feed authoritative state (bootstrap, reconnect resync, retry loops) **must signal fetch failure distinctly from a successful-but-empty server response.** A method that swallows errors and returns `[]`/`{}`/`null` lets the caller delete or overwrite legitimate state on a transient network blip, indistinguishable from "the server says nothing here."

- **Decide which methods are authoritative.** A method is authoritative if any caller uses its result to delete, clear, or replace persisted/sync state. UI-display-only methods (autocomplete, dropdowns, settings pages) can keep silent-empty fallback because the user's next action refreshes them.
- **For authoritative methods, pick one of two patterns** — both already exist in the codebase, do not invent a third:
  - **Throw on failure** (e.g. `listPendingPermissions`, `listPendingQuestions`, `listAgents`, the `unwrap()` helper in `packages/ui/src/sync/bootstrap.ts`). Use this when the caller has an outer `try/catch` per logical block — the throw skips the block and preserves prior state.
  - **Return `T | null` on failure, where `null` strictly means "fetch failed"** (e.g. `getSessionStatusForDirectory`, the `.catch(() => null)` + early-return-on-null pattern at the per-session reconnect loop in `sync-context.tsx`). Use this when the caller has follow-up work that should still run when one fetch fails.
- **Never swallow inside the method while returning the same type as success.** The SDK's `{data, error}` shape already does this silently — wrap with `if (result.error) throw …` so the failure can't be lost.
- **Verify the caller actually preserves state on failure.** Adding the throw is only half the fix; the consumer must not run the "delete missing" / "overwrite" branch unless it knows the fetch succeeded. The relevant outer `try/catch` is often already there but dormant.
- **Retry loops require a failure signal.** A `for (let attempt = 0; attempt < 3; …)` retry around a method that swallows to `[]` will run exactly once — the loop never sees an error.

This rule is the API-layer counterpart of "Use live server/session state for live activity. Do not let historical anomalies masquerade as current execution." A fetch failure is the same kind of anomaly — don't let it masquerade as authoritative server state.

### Reconnect-loop pacing

The SSE/WebSocket reconnect loop in `packages/ui/src/sync/event-pipeline.ts` retries indefinitely. To avoid burning battery and server load on dead/idle connections, the loop's pacing must respect three signals:

- **`navigator.onLine`**: when the browser reports offline, use the long backoff cap (~60s) instead of the short one (~5s). The expected recovery path is the `online` event, not the next probe.
- **`document.visibilityState`**: when hidden, use the long cap too. A backgrounded PWA shouldn't hammer the network at 1/5s; the browser may also throttle our timers, but state the intent in code rather than relying on it.
- **HTTP status of the last failure**: permanent 4xx errors (401, 403, 404, …) don't recover from blind retry. Jump straight to the long cap instead of running the normal exponential path; otherwise a stale-path or expired-token client would put ~12 reqs/min on the server log forever. 408 (Request Timeout) and 429 (Too Many Requests) are retryable in spirit — let them go through normal backoff.
- **Consecutive failures**: real exponential growth (`base * 2^failures`, clamped), not constant 500ms. A hard-down server should see geometrically fewer probes per minute over time.

The inter-attempt wait must be interruptible by `online`, visibility-becomes-visible, and the pipeline's abort signal — otherwise recovery is delayed by however long the current sleep had left to run.

## CLI Parity and Safety Policy (MANDATORY)

### Principle: policy-first, UX-second

All safety and correctness rules MUST be enforced in core command logic, independent of output mode.

Interactive/pretty UX (`@clack/prompts`) is a presentation layer only.
It must never be the only place where validation or restriction is enforced.

### Required parity across modes

The same functional outcome and safety gates MUST hold for all execution modes:

- Interactive TTY (full Clack UX)
- Non-interactive shells (piped/stdin-less automation)
- `--quiet`
- `--json`
- Fully pre-specified flags (no prompts)

In all modes, invalid operations MUST fail with non-zero exit code and deterministic error semantics.

### Non-negotiable rule

Do not rely on prompts to enforce policy.

- Prompts MAY help users choose valid inputs.
- Core validators MUST run even when prompts are unavailable or skipped.
- `--quiet` suppresses non-essential output only; it does not weaken validation.
- `--json` changes output shape only; it does not weaken validation.

Detailed Clack UX patterns (primitives, prompt gating, and implementation checklist)
are defined in the `clack-cli-patterns` skill and should not be duplicated here.

## Project Skills (MANDATORY)

Project skills live under `.agents/skills/*/SKILL.md`. Before editing, agents **MUST** load every skill whose trigger matches the work; if multiple rows apply, load all of them.

| Work being done | Required skill call |
|---|---|
| Terminal CLI commands, prompts, or output formatting, especially `packages/web/bin/*` | `skill({ name: "clack-cli-patterns" })` |
| Shared UI data access, `RuntimeAPIs`, `runtimeFetch`, `runtime-url`, OpenCode SDK calls, VS Code bridges/proxies, authenticated browser assets, Electron runtime switching, or web server API endpoints | `skill({ name: "ui-api-decoupling" })` |
| UI components, styling, visual elements, colors, buttons, or icons | `skill({ name: "theme-system" })` |
| User-facing UI text: labels, buttons, placeholders, aria labels, empty/error/loading states, toasts, dialogs, settings copy, or navigation labels | `skill({ name: "locale-ui-patterns" })` |
| Settings pages, settings dialogs, configuration UI, or visual/layout changes inside Settings | `skill({ name: "settings-ui-patterns" })` |
| Drag-to-reorder, sortable lists/chips/grids, or `@dnd-kit` behavior including touch/mobile and wrapping variable-width items | `skill({ name: "drag-to-reorder" })` |
| iOS Simulator preview/control for the mobile app, `serve-sim`, simulator taps/typing/gestures/rotation, or headless install/launch workflows outside Xcode | `skill({ name: "serve-sim" })` |
| WebSocket/SSE/streaming endpoints (terminal, dictation/voice, event stream, notifications), opening a WebSocket in shared UI, runtime transport refactors (`runtime-fetch`/`runtime-url`/`runtime-switch`/`runtime-auth`), the private relay tunnel, or anything under `packages/ui/src/lib/relay` or `packages/web/server/lib/relay` | `skill({ name: "relay-transport" })` |

Skill docs are the source of truth for detailed patterns. Do not duplicate their full guidance here; load the skill and follow it before making matching changes.

## Performance rules (MANDATORY)

These rules exist because violating them has caused measurable regressions (render cascades, memory bloat, UI jank). They apply to all UI and sync layer work.

### Shared-store render discipline

- **Treat common stores as render fanout boundaries.** An unnecessary reference change in shared state can re-render large parts of the app.
- **Do not put high-frequency state in broadly consumed stores.** Fast-changing state should live in narrow stores with narrow subscribers.
- **Update only the fields that changed.** Preserve references for untouched state branches.
- **Prefer leaf selectors over container selectors.** Subscribe to the smallest stable value that satisfies the component.
- **Isolate hot consumers.** If a value changes often and only a few components need it, move it to a narrower store or consume it in a memoized child.
- **Do not subscribe shell/layout components to broad live collections.** If a shell only needs one field, entity, or derived flag, subscribe to that instead of the whole collection.
- **Treat provider roots as global hot paths.** A top-level provider must not subscribe to high-frequency data unless the feature is actually enabled and the subscription is essential.

### Zustand referential equality

Zustand skips re-renders when a selector returns the same reference (`Object.is`). Every new object/array reference triggers a re-render in every subscriber.

- **Never spread all state fields in an update.** Only create new references for fields that actually changed. A `message.part.delta` event should not clone `session`, `permission`, etc.
- **Select leaf values, not containers.** `useStore((s) => s.permission[sessionID])` is correct. `useStore((s) => s.permission)` subscribes to every permission change across all sessions.
- **Preserve references when merging.** If prepending older messages, keep existing message object references. Only add truly new items. Return the original array if nothing was added.
- **For derived collections, preserve item identity when presentation-relevant fields are unchanged.** Reuse previous item references for unchanged rows/items and move high-frequency live fields to narrow per-item selectors.

### Store splitting

A single store with N properties means every subscriber re-evaluates on every state change. Split stores by change frequency and subscriber set.

- **Group state by how often it changes.** Streaming state (updated 60/sec) must not live with user preferences (updated on click).
- **Group state by who reads it.** If only 2 components need a value, it belongs in a store that only those 2 subscribe to.
- **Cross-store reads use `.getState()`.** Actions in one store that need another store call `useOtherStore.getState()` — imperative, no subscription.
- **Never add unrelated state to an existing store** just because it's convenient. Create a new store.

### Event pipeline and SSE

- **Gate expensive operations on the hot path.** During streaming, `message.part.delta` and `message.part.updated` fire ~60/sec. Any `findIndex`, `filter`, or iteration added to these handlers multiplies across every event. Gate behind a cheap boolean check first (e.g., check `next[0]` before scanning the array).
- **Skip no-op updates.** If an incoming event doesn't change the state (same role, same finish, same timestamps), return `false` from the reducer to avoid creating new references.
- **Coalesce by key.** Same-entity events (e.g., repeated `session.status` for the same session) should replace earlier ones in the queue, not accumulate.
- **Preserve event ordering semantics.** Reducers and queues must not let stale deltas or out-of-order events corrupt the latest state.
- **Do not widen live-activity fallbacks.** A fallback for delayed status should inspect only the current trailing entity, not arbitrary historical records.

### Polling payload fidelity

- **Do not let lightweight polling erase rich fields.** If light mode omits fields (e.g., `diffStats`), preserve previous rich data until a heavy follow-up fetch lands.
- **Use two-phase polling.** Run cheap change detection first; only run heavy status fetches for directories that actually changed.

### Optimistic updates

- **Use the shadow Map pattern.** Insert optimistic data into the store for instant UI, AND register it in a separate tracking Map. Cleanup happens deterministically via `mergeOptimisticPage` on the next data fetch — not via heuristics in the event reducer.
- **Pass client-generated IDs to the server.** Use the same ID format as the server (hex-encoded timestamps). Pass `messageID` to `promptAsync` so the server echoes back the same ID. This prevents duplicates and enables in-place replacement.
- **Rollback on error.** Remove the optimistic entry from both the store and the shadow Map.
- **Stabilize bridge callbacks.** When wiring hook callbacks into module-level refs, use stable ref wrappers so effects do not loop on changing function identities.

### Session/input consistency

- **Capture send config at queue time.** Queue items must include provider/model/agent/variant snapshot; do not re-resolve from mutable live state at send time.
- **Keep server-selected attachments sendable.** Preserve server-backed file selections in queue/submit flows and convert them to proper `file://` URLs before sending.
- **Do not let text input state repaint unrelated chrome.** Typing should not force unrelated controls, menus, indicators, or toolbars to re-render on every keystroke.
- **Extract slow-changing chrome from hot input paths.** If controls do not depend on the current text value, move them behind memoized boundaries with stable callbacks.

### Bootstrap resilience

- **Treat startup 502/503 as transient.** Retry bootstrap/session-list flows with bounded retries/intervals, especially in VS Code where API readiness can lag bridge startup.
- **Use polling recovery when failures are swallowed.** If an async loader resolves without throwing on failure, recover with interval retries gated by loaded-state checks.

### Scroll and DOM

- **Never use `await waitForFrames()` for scroll preservation.** Frames of visible scroll jump are unacceptable. Use `useLayoutEffect` to adjust scroll synchronously after React commits DOM — before the browser paints.
- **Capture scroll state before the state change, restore in layout effect.** The pattern: save `scrollHeight`/`scrollTop` into a ref before triggering the update, consume it in `useLayoutEffect` on the rendered output.
- **Do not let viewport resizes masquerade as content growth.** Viewport-height changes must not trigger the same scroll compensation logic used for actual content growth.
- **Disable or narrow native/browser scroll anchoring when custom scroll logic exists.** Browser anchoring and app-managed pinning/follow logic will fight and produce jiggle.
- **Autosize textareas without transient collapse on growth.** Avoid `height='auto'` shrink/expand cycles on every character when the content only grew; this creates visible layout bounce.

### List ordering and view consistency

- **Do not sort structural lists directly from high-churn live fields.** If live updates are frequent, sorting directly from them causes reorder thrash and wide rerender cascades.
- **If live recency is required, freeze order during high-frequency updates and apply a one-shot reorder only at an intentional lifecycle edge.** Choose the lifecycle edge explicitly instead of letting every intermediate update reshuffle the UI.
- **Use one ordering source for all views of the same data.** Different views of the same entities must derive from the same ranked list or rank map; do not let each surface re-derive ordering independently.
- **Do not mix global snapshots and local live snapshots without an explicit reconciliation policy.** If multiple data sources feed one view, define which fields win and how they merge.

### Component isolation

- **Extract high-frequency hook consumers into separate components.** If a hook re-evaluates 60/sec (e.g., streaming status), wrap its consumer in a `React.memo` child component so the parent doesn't re-render.
- **Use custom `React.memo` comparators for message rows.** Compare render-relevant fields (role, finish, parts count, part IDs) — not object references.

### Caching and memory

- **Cap in-memory caches with both count and byte limits.** Entry count alone doesn't prevent memory bloat from large files. Use dual-constraint LRU (e.g., 40 entries OR 20MB).
- **Set store session limits to match loaded data.** If bootstrap loads N sessions, set `limit >= N`. Otherwise the next SSE event triggers trimming that silently removes sessions.
- **Invalidate caches on mutations.** File content cache must clear entries on write, delete, rename. Prefetch cache must clear on session eviction.
- **Use TTLs to prevent redundant fetches.** If a session was fetched <15s ago, skip re-fetching — SSE events keep it current.

### Directory context

- **Never cache directory strings in closures.** Directory can change at any time (worktree switch). Read it dynamically from `opencodeClient.getDirectory()` at call time.
- **Pass directory hints when the source of truth isn't available yet.** Newly created sessions aren't in the sync store until SSE delivers them. Pass the known directory as a parameter instead of relying on lookup.

## Regression-prevention checklist

- When adding fallback logic, ask: can stale persisted data keep this path active forever?
- When deriving UI state, ask: is this live state, historical state, or inferred state?
- When adding store fields, ask: who reads this, how often does it change, and should it live elsewhere?
- When touching polling or bootstrap, ask: can a lighter payload erase richer existing data?
- When handling optimistic updates, ask: where is rollback, reconciliation, and duplicate prevention?
- When changing shared routes or state contracts, ask: what breaks in web, desktop, and VS Code?
- When fixing a bug with a heuristic, prefer narrowing the heuristic over widening it.

## Validation expectations

- Run type-check/lint validation before finalizing source-code changes that can affect TypeScript, runtime behavior, builds, lint rules, package resolution, or generated assets, and run `bun run dead-code` when the change can add, remove, rename, or reshape files, exports, types, workspace entrypoints, or module imports. Keep validation scoped to the edited workspace by default. Prefer the package-level command for the package you changed (for example the relevant workspace's `type-check`/`lint`) instead of workspace-wide `bun run type-check` / `bun run lint`. Use workspace-wide checks only when the change spans multiple workspaces, shared package contracts, root tooling/config, dependency resolution, generated assets used across packages, or when a narrower command cannot cover the risk. Use a sufficiently long tool timeout for any broad checks (for example 240000ms) so successful package-level results are not lost to a tool timeout. For docs-only or isolated config-only changes, run the narrowest relevant validation instead (for example JSON/schema validation) and do not run full checks unless the change can affect code execution.
- For hot-path changes, verify behavior under streaming or repeated events, not just static render.
- For sync or startup changes, verify fresh load, retry/failure, and restart behavior.
- For session changes, verify create, stream, abort, permission, archive/delete, and revisit flows when relevant.

## Frequently-misdiagnosed runtime issues

These are recurring user-facing failures whose on-screen copy is misleading enough that an agent reading the report will guess the wrong root cause. The entries below capture the real cause and the right place to look. Do not patch the symptom (the visible UI / error screen) without first reading the entry — the symptom is usually a downstream effect of a problem elsewhere.

### "We could not verify the UI session" / "Unable to reach server"

- **Symptom**: Opening OpenChamber (typically from a phone / tablet / second laptop on the LAN, but can also happen on the desktop itself) shows:

  > **Unable to reach server**
  > We could not verify the UI session. If you're opening OpenChamber from another device on your local network, make sure Desktop Network Access is enabled on the desktop app and use the LAN address shown in Settings.

  Note the title — "Unable to reach server" — is the real signal. The body text is a generic message; it does **not** mean LAN access is the problem.

- **Actual root cause**: the browser is fetching the session status from an API base URL whose **port does not match the backend that is actually serving the page**. The page rendered on one port, but `window.__OPENCHAMBER_API_BASE_URL__` (read by `packages/ui/src/lib/runtime-url.ts`) points to a different port, so `runtimeFetch('/api/client-auth/status', ...)` (`SessionAuthGate.tsx:99 fetchSessionStatus`) throws a network-level error → `setState('error')` → `ErrorScreen errorType='network'` (`SessionAuthGate.tsx:273`) → the misleading copy above.

  History: commit `fd1dfd66 fix: resolve runtime URLs from injected desktop API base` introduced call-time resolution of the injected base URL precisely to stop the resolver from holding a stale port; regressions of this issue usually mean the injected `__OPENCHAMBER_API_BASE_URL__` is missing or wrong for the surface the user opened.

- **How to confirm before doing anything else**:
  1. In the failing browser, open DevTools → Console and run:
     ```js
     window.__OPENCHAMBER_API_BASE_URL__
     window.__OPENCHAMBER_LOCAL_ORIGIN__
     location.origin
     ```
  2. `location.origin` is the port the page is on. `__OPENCHAMBER_API_BASE_URL__` must point to the **same** scheme + host + port (or a routed equivalent). If they differ → bug surface, do not chase LAN settings.
  3. From the same browser, try fetching the API base directly:
     ```js
     fetch(window.__OPENCHAMBER_API_BASE_URL__ + '/api/client-auth/status', { credentials: 'include' })
     ```
     A network error / CORS error / non-200 means the backend is not reachable on that port. If `location.origin` is fine but the injected base URL points elsewhere, that is the bug.

- **Where this surfaces in code**:
  - Error UI: `packages/ui/src/components/auth/SessionAuthGate.tsx:273` (`ErrorScreen`, `errorType='network'`).
  - Status probe: `packages/ui/src/components/auth/SessionAuthGate.tsx:99` (`fetchSessionStatus` → `runtimeFetch(STATUS_CHECK_ENDPOINT, …)`).
  - URL plumbing: `packages/ui/src/lib/runtime-url.ts` (resolver), `packages/ui/src/lib/runtime-fetch.ts:245` (`runtimeFetch`), `packages/web/src/runtimeConfig.ts:32` (reads `window.__OPENCHAMBER_API_BASE_URL__`).
  - i18n keys: `sessionAuth.error.networkTitle` / `networkDescription` in every locale (`packages/ui/src/lib/i18n/messages/*.ts`).

- **Triage rules for an agent seeing this report**:
  1. First check port alignment via the DevTools snippet above. If `__OPENCHAMBER_API_BASE_URL__` ≠ `location.origin`, that is the cause — fix the injection site (`packages/web/server/`, `packages/electron/`, or the runtime surface that built the injected value), do **not** tweak `SessionAuthGate`.
  2. The "enable Desktop Network Access" instruction in the body copy is unrelated to this failure mode. Do not send users down that path when the title says "Unable to reach server" — it is a port-mismatch, not a LAN-binding issue.
  3. Do **not** loosen the auth gate (e.g. skip `fetchSessionStatus` on error) — that would hide real backend outages. The correct response is to repair the URL plumbing.

## Recent changes

- Releases + high-level changes: `CHANGELOG.md`
- Recent commits: `git log --oneline` (latest tags: `v1.11.7`, `v1.11.6`)
