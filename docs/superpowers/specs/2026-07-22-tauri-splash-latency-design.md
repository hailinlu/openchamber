# Tauri Splash Latency — Re-diagnosis + AGENTS.md Update

**Date:** 2026-07-22
**Status:** Approved
**Scope:** Tauri desktop only (`feature/rust-migration` branch)
**Supersedes:** Initial diagnosis that blamed `OcServer::start` ordering as the #1 bottleneck. That diagnosis was wrong; this spec replaces it.

## Goal

1. Re-state the actual startup-latency picture for the Tauri desktop shell based on real log evidence, not inference.
2. Ship the smallest UX improvement that addresses the user-visible symptom: a 3-second progress hint on the splash while the cube spinner is still showing in Tauri.
3. Capture the latency taxonomy in `AGENTS.md` so any future agent that touches the startup pipeline has the same baseline numbers and stops re-deriving them.

## Current State and Root Causes

### Real timing from `/tmp/tauri-dev-verify.log` (last cold Tauri dev start)

| T+      | Event                                              | Source line |
|---------|----------------------------------------------------|-------------|
| T+0.2s  | Vite ready in 189 ms                               | log line 6  |
| T+10s   | `Running DevCommand` → entire cargo cold compile   | lines 14–280 |
| T+18s   | `spawning opencode port=62067`                     | line 314    |
| T+20s   | `opencode listening, waiting for health`            | line 315    |
| T+20s   | `opencode is healthy and ready` (same second)      | line 316    |
| T+20s   | `oc-server listening addr=127.0.0.1:62074`         | line 323    |
| T+20s   | `oc-server (in-process) ready on port 62074` → Tauri `inject_main_window_runtime` | line 324 |

**Conclusion:** OpenCode subprocess start-to-healthy is ~2 s, not the 3–7 s I assumed earlier. The dominant cost is cargo cold compile.

### Re-ranked cause taxonomy

| #  | Cause                                                            | File:line                                                | Measured window                          |
|----|------------------------------------------------------------------|----------------------------------------------------------|------------------------------------------|
| 1  | Cargo cold compile of Tauri shell + oc-server (5 crates)        | `scripts/tauri-dev.mjs:233`                              | ~10 s cold; ~1–3 s warm                  |
| 2  | `OcServer::start` binds TCP listener AFTER OpenCode is healthy  | `rust/oc-server/src/lib.rs:89-105`                       | ~0–3 s (OpenCode spawn ≈ 2 s)            |
| 3  | `SessionAuthGate` runs two parallel fetches with no `AbortSignal.timeout` | `packages/ui/src/components/auth/SessionAuthGate.tsx:99-108, 420-488` | unbounded on network stall               |
| 4  | Tauri injects runtime AFTER the port is determined (dynamic port vs Vite proxy static target) | `rust/oc-tauri/src-tauri/src/lib.rs:307-321` | ~10 ms once `setup` returns              |
| 5  | `useConfigStore.checkConnection` does 5 attempts × `400 * attempt` ms | `packages/ui/src/stores/useConfigStore.ts:2992-3046`     | worst case ~6 s while behind spinner     |
| 6  | Splash has no progress copy while the cube spinner is spinning  | `packages/web/index.html:519-605`                        | continuous; only symptom, not cause      |

### The Node Express reference pattern (already correct)

`packages/web/server/lib/opencode/startup-pipeline-runtime.js:100-102` binds the listener first, then `fire-and-forget` starts OpenCode. The Rust port at `rust/oc-server/src/lib.rs:89-105` inverted that ordering when it landed in `ca3ea2f7 feat(tauri): Vite-only dev launcher (drop Node Express backend)`. This is a structural regression — but fixing it requires a separate PR that owns Rust listener + spawn lifecycle.

## Chosen Approach

### What ships in this PR

1. **`packages/web/index.html`** — add a 3-second progress hint under the cube spinner.
   - New DOM node: `<div id="initial-loading-status">` directly below the SVG.
   - New inline `<script>`: if `#initial-loading` is still mounted after 3 s AND `window.__GRIDFORGE_LOCAL_ORIGIN__` is set (Tauri detection), populate the status node with `Warming up OpenCode…`.
   - The status node is removed together with the splash by the existing `App.tsx:322-349` dismissal effect — no manual cleanup.
   - Hard-coded English copy matches the existing "AUTOMATED GRID / HUMANLESS OPERATIONS" tone. No new i18n key for this PR; if the copy ever needs translation, lift it into `packages/ui/src/lib/i18n` then.
   - The existing 10 s hard fallback at `index.html:608-625` stays untouched; desktop still bypasses it via the `__GRIDFORGE_LOCAL_ORIGIN__` guard.

2. **`AGENTS.md`** — add `### Startup latency sources (Tauri desktop)` under `## Runtime architecture (IMPORTANT)`.
   - The 4-row impact table above (cargo compile / OcServer / SessionAuthGate / runtime injection).
   - The 5-causes re-ranked taxonomy is collapsed to the 4 rows that AGENTS-level agents should remember.
   - A self-check checklist before any startup-pipeline modification.
   - A Splash UX spec note (cube SVG is brand asset; don't recolor it; status hint at 3 s; remove on splash dismissal).

### What does NOT ship (explicit out-of-scope)

- `OcServer::start` ordering reversal — needs its own PR covering Rust listener + spawn lifecycle.
- Adding `AbortSignal.timeout` to `SessionAuthGate` — touches auth boundary; own PR.
- Changing `useConfigStore.checkConnection` retry curve — touches health-probe semantics; own PR.
- Vite proxy / port negotiation change — needs shared design with the runtime URL injection.
- Cargo compile-time improvements (sccache, RUST_BACKTRACE, profile switch) — outside code edits.
- Push to remote — per `AGENTS.md` "Do not run git/GitHub commands unless explicitly asked."

## Why not the aggressive reordering fix?

Reversing `OcServer::start` (bind listener → spawn OpenCode) would shave ~2 s off the cold path. It is the right fix in the long run, but:
- It changes Rust ownership/lifecycle semantics around `state.set_opencode_ready(true)`.
- It needs parallel changes in `runtime-url.ts` consumers (which currently assume the boot outcome is known at the moment the listener accepts connections).
- It deserves its own PR with separate review.

The 3 s splash hint costs zero runtime correctness and gives the user immediate feedback that the app is working, not stuck. That is the right cost/benefit for this PR.

## Risks

- **3 s threshold is hard-coded.** It matches observed cargo compile floor; if the build host gets faster (sccache) the hint may rarely appear. Acceptable — empty status node is harmless.
- **English-only copy.** Future i18n lift is cheap because the status node has stable `id="initial-loading-status"`.
- **Detecting Tauri via `window.__GRIDFORGE_LOCAL_ORIGIN__`:** matches the existing 10 s fallback's pattern. If that global is renamed, both detection points must move together.

## Verification

1. Cold-run `bun run tauri:dev`, watch for the status line to appear at splash T+3 s and disappear when the React tree mounts.
2. Confirm the existing 10 s fallback still fires in the non-desktop browser path.
3. Visually confirm the cube SVG itself is unchanged.
4. Run `bun run type-check` and `bun run lint` before commit (per `AGENTS.md` baseline green rule).
5. Commit on `feature/rust-migration`. Do not push.