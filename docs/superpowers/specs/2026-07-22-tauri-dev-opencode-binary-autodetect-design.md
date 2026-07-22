# tauri-dev OpenCode binary auto-detect (Windows)

- **Date**: 2026-07-22
- **Owner**: desktop runtime
- **Status**: design — pending user review
- **Scope**: `scripts/tauri-dev.mjs` only

## Problem

On Windows, `bun run tauri:dev` fails out of the box because Rust cannot locate the
`opencode` CLI. The Rust server tries `Command::new("opencode")` from inside the
`cargo run` subprocess (`rust/oc-server/src/opencode/mod.rs:169`), but Windows
process-level path resolution does not match what `cmd.exe` does with
`PATHEXT`.

Concretely, on the typical Windows dev machine the opencode CLI is installed
via npm globally. That places three shims in `C:\Program Files\nodejs\`:

```
opencode        (POSIX shell shim, Git Bash only — not a Windows .exe)
opencode.cmd    (cmd.exe shim — works in interactive shells, fails in Rust spawn)
opencode.ps1    (PowerShell shim — fails in Rust spawn)
```

The real binary is `C:\Program Files\nodejs\node_modules\opencode-ai\bin\opencode.exe`.
That is the path Rust must be given.

When `OPENCODE_BINARY` is unset:

```
[oc_tauri_lib][ERROR] oc-server embed startup failed:
    failed to start opencode: failed to spawn opencode binary `opencode`: program not found
```

The Tauri shell exits because `OcServer::start` failed to embed. Vite is still
running on `:5180`, so the Tauri webview loads it. The webview issues
`/auth/session`, `/api/config/settings`, etc., which Vite proxies to
`127.0.0.1:3001` (the static target in `vite.config.ts:103-119`) — no backend is
listening there, so every request returns `ECONNREFUSED`. `SessionAuthGate`
treats that as a network failure and shows the misleading
"We could not verify the UI session / enable Desktop Network Access" error
(AGENTS.md "Frequently-misdiagnosed runtime issues" entry).

Workaround used during diagnosis: prefix the command with
`OPENCODE_BINARY="C:\\Program Files\\nodejs\\opencode.cmd"`. This is friction
that every Windows contributor hits, and the body copy of the error screen is
misleading enough that contributors (and AI agents) go down the wrong triage
path before finding this.

## Non-goals

- **Not** changing Rust spawn semantics in `rust/oc-server/src/opencode/mod.rs`.
  The module already accepts `OPENCODE_BINARY`; the bug is that nobody upstream
  sets it on Windows.
- **Not** adding a new dependency. The fix uses only `node:child_process`,
  `node:fs`, `node:os`, `node:path` — all already available to a `node:`-prefixed
  ESM script.
- **Not** changing `scripts/oc-dev.mjs`. It already has its own equivalent
  helper for SSH remote runtime (line 39). That path is unrelated to the local
  Tauri dev workflow.
- **Not** touching `vite.config.ts` proxy targets. The Vite proxy `3001`
  fallback noise is a separate, known issue (AGENTS.md startup-latency table
  row 4) tracked in a different PR.
- **Not** writing unit tests for this fix. The script has no test infrastructure
  today; adding vitest just for a ~30-line helper violates the
  "No new deps unless asked" rule. Manual smoke tests are sufficient.

## Goal

`bun run tauri:dev` (and `bun run tauri:dev:sidecar`, which uses the same
`main()`) finds a working `opencode` binary on Windows without the user having
to set `OPENCODE_BINARY` manually, **when one is available at the standard
npm-global location**. Existing explicit `OPENCODE_BINARY` settings keep
working unchanged. When no binary can be detected, behavior is identical to
today (Rust reports its own `program not found`).

## Design

### 1. New helper: `resolveOpencodeBinary()`

Add to `scripts/tauri-dev.mjs`, near the existing `resolveWindowsCommand`
helper (lines 51-63):

```js
// Resolve the OpenCode binary path. Only auto-detects when OPENCODE_BINARY
// is unset. Returns null when no candidate is found — caller falls back to
// the Rust default "opencode" and lets oc-server report its own spawn error.
function resolveOpencodeBinary() {
  const explicit = (process.env.OPENCODE_BINARY || '').trim();
  if (explicit) return explicit;

  if (process.platform === 'win32') {
    // npm global root → node_modules\opencode-ai\bin\opencode.exe
    const npmRoot = spawnSync('npm', ['root', '-g'], {
      encoding: 'utf8',
      windowsHide: true,
      shell: true,
    });
    if (npmRoot.status === 0) {
      const candidate = path.join(
        (npmRoot.stdout || '').trim(),
        'opencode-ai',
        'bin',
        'opencode.exe'
      );
      if (existsSync(candidate)) return candidate;
    }
    return null;
  }

  // Unix: match the precedence used by scripts/oc-dev.mjs REMOTE_RUNTIME_ENV
  const unixCandidates = [
    path.join(os.homedir(), '.opencode', 'bin', 'opencode'),
    path.join(os.homedir(), '.local', 'bin', 'opencode'),
    path.join(os.homedir(), '.bun', 'bin', 'opencode'),
  ];
  for (const candidate of unixCandidates) {
    if (existsSync(candidate)) return candidate;
  }
  // Final PATH fallback (matches today's behavior — Rust still does its own lookup)
  const which = spawnSync('which', ['opencode'], { encoding: 'utf8' });
  if (which.status === 0 && which.stdout) {
    return which.stdout.trim().split(/\r?\n/)[0] || null;
  }
  return null;
}
```

Notes:

- **`shell: true` for `npm root -g`**: npm's `.cmd` shim needs to go through
  cmd.exe on Windows, same as the existing `spawnProcess` helper does
  (lines 65-85). Without `shell: true`, `Command::new('npm')` would fail with
  the same PATHEXT issue we're working around.
- **`existsSync` check** on the final `.exe` path: avoids handing Rust a path
  that doesn't actually exist (e.g. npm global root resolved but the
  `opencode-ai` package wasn't installed there).
- **Unix path mirrors `scripts/oc-dev.mjs:39`** so contributors get consistent
  behavior across `tauri:dev` and the SSH remote helper. The `.local/bin` and
  `.bun/bin` candidates come straight from that line.

### 2. New imports

Extend the existing import block:

```js
import { existsSync, rmSync } from 'node:fs';   // add existsSync
import os from 'node:os';                       // new import
```

### 3. Call site

In `scripts/tauri-dev.mjs::main()`, immediately before the existing
`spawnProcess('cargo', ['tauri', 'dev'], …)` call (line 233), insert:

```js
const opencodeBinary = resolveOpencodeBinary();
const opencodeSource = (process.env.OPENCODE_BINARY || '').trim() ? '(from env)' : '(auto-detected)';
console.log(`[tauri:dev] opencode binary: ${opencodeBinary || 'opencode'} ${opencodeBinary ? opencodeSource : ''}`);
```

Then update the cargo spawn's `env` block to forward the resolved value:

```js
const tauri = spawnProcess('cargo', ['tauri', 'dev'], {
  cwd: tauriSrcDir,
  env: {
    GRIDFORGE_HMR_UI_URL: `http://127.0.0.1:${uiPort}`,
    OPENCHAMBER_PORT: apiPort,
    OPENCODE_BINARY: opencodeBinary || 'opencode',
  },
});
```

The `opencodeBinary || 'opencode'` is intentional: it preserves Rust's default
behavior when nothing could be detected, so the existing error message
(`failed to spawn opencode binary \`opencode\``: program not found) stays
diagnostic.

### 4. Behavior matrix

| `OPENCODE_BINARY` env | npm global has `opencode-ai/bin/opencode.exe` | Behavior |
|---|---|---|
| unset | yes | auto-detect `.exe`; log `(auto-detected)`; spawn works |
| unset | no | resolve returns `null`; env falls back to literal `"opencode"`; Rust reports `program not found` (today's behavior, unchanged) |
| set to anything | any | use the env value verbatim; log `(from env)`; no probing |
| set to non-existent path | any | use the env value verbatim (we do not validate); Rust reports spawn failure on that specific path |

### 5. Sidecar path

`bun run tauri:dev:sidecar` is just `OPENCHAMBER_SIDECAR=1 node ./scripts/tauri-dev.mjs`
(`package.json:52`). It runs the same `main()`. The auto-detect applies
identically. No additional change needed.

## Verification

Manual smoke tests on Windows (run from repo root after the change):

1. **Auto-detect success** — `bun run tauri:dev` with `OPENCODE_BINARY` unset and
   `C:\Program Files\nodejs\node_modules\opencode-ai\bin\opencode.exe` present.
   Expected log line: `[tauri:dev] opencode binary: <…>\opencode.exe (auto-detected)`.
   `oc-server::opencode::start_managed` log shows `spawning opencode port=…`,
   followed by `opencode is healthy and ready`. Tauri window opens.

2. **Explicit override** — `OPENCODE_BINARY=/some/other/path bun run tauri:dev`.
   Expected log line: `[tauri:dev] opencode binary: /some/other/path (from env)`.
   Auto-detect is bypassed.

3. **Auto-detect miss (silent fallback)** — `OPENCODE_BINARY=` (empty) with no
   opencode install at all. Expected: log shows
   `[tauri:dev] opencode binary: opencode` with no `(auto-detected)` suffix,
   followed by Rust's existing `failed to spawn opencode binary \`opencode\`` error.
   No new error message introduced.

4. **Type-check + lint** — `bun run type-check` and `bun run lint` from repo
   root. No new TypeScript (the file is `.mjs`), but ESLint scope includes it.

## Risks

- **`npm root -g` adds ~100–300 ms to startup on Windows** because npm is
  itself a Node process. Acceptable: this runs once before Vite is spawned, so
  it doesn't sit on the critical startup path. `bun run tauri:dev` already
  pays a multi-second cargo cold-compile cost on every cold start; a 200 ms
  npm probe is negligible by comparison (AGENTS.md startup-latency table row 1).
- **`shell: true` + `npm`** carries the usual shell-injection caveats, but the
  command is hard-coded — no user input enters it. Safe.
- **`(opencodeBinary || 'opencode')` fallback** preserves the existing
  diagnostic message exactly when detection fails, so any user already
  searching for `program not found` keeps finding it.

## Files changed

| File | Change |
|---|---|
| `scripts/tauri-dev.mjs` | + `existsSync` import, + `os` import, + `resolveOpencodeBinary` helper, + ~3 lines in `main()`, + `OPENCODE_BINARY` in cargo spawn env. Total: ~35 lines added. |