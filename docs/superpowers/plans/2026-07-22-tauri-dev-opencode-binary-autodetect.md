# tauri-dev OpenCode binary auto-detect Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `bun run tauri:dev` work on Windows out of the box by auto-detecting the npm-global `opencode` binary when `OPENCODE_BINARY` is unset.

**Architecture:** Add a small `resolveOpencodeBinary()` helper to `scripts/tauri-dev.mjs`. It runs **only when** `OPENCODE_BINARY` is not set, picks the npm-global `opencode-ai/bin/opencode.exe` on Windows (or a Unix-path-based candidate matching `scripts/oc-dev.mjs` precedence), and forwards the resolved path into the cargo subprocess via the existing `env` mechanism in `spawnProcess`. On miss it returns `null` and the script falls back to literal `"opencode"` — preserving Rust's existing diagnostic error.

**Tech Stack:** Node 22+ ESM (`.mjs`), `node:child_process` `spawnSync`, `node:fs` `existsSync`, `node:os`, `node:path`. No new deps.

---

## File map

| File | Action | Purpose |
|---|---|---|
| `scripts/tauri-dev.mjs` | Modify (single file, ~35 lines added) | Add helper + import + call site + env injection |

No new files. No test files (intentional — see spec § "Non-goals").

---

## Task 1: Reproduce the pre-fix failure

**Files:** none (read-only inspection + repro).

This is the "failing test" for a script that has no test infrastructure. Confirm the bug exists on the current branch **before** changing anything, so the post-fix verification in Task 4 has something to compare against.

- [ ] **Step 1: Verify `OPENCODE_BINARY` is unset in the current shell**

Run: `echo "OPENCODE_BINARY=[$OPENCODE_BINARY]"`
Expected: `OPENCODE_BINARY=[]` (empty). If your shell already has it set, run `unset OPENCODE_BINARY` (or `Remove-Item Env:OPENCODE_BINARY` in PowerShell) before proceeding.

- [ ] **Step 2: Verify npm-global `opencode-ai/bin/opencode.exe` exists**

Run (Git Bash on Windows): `ls "C:/Program Files/nodejs/node_modules/opencode-ai/bin/opencode.exe"`
Expected: file path printed. If missing on this machine, the fix has nothing to detect against and Tasks 2-4 should still be implemented (they handle "miss" silently) but Task 4 smoke test #1 cannot be run locally — note this and rely on smoke tests #2 and #3.

- [ ] **Step 3: Run `bun run tauri:dev` and confirm the pre-fix failure**

Run from repo root: `bun run tauri:dev`
Expected (within ~30 s of Vite + cargo cold-compile finishing):
```
[oc_server::opencode][INFO] spawning opencode port=…
[oc_tauri_lib][ERROR] oc-server embed startup failed:
    failed to start opencode: failed to spawn opencode binary `opencode`: program not found
```
Then either the Tauri window exits, or it stays open showing the "We could not verify the UI session" error screen with Vite `ECONNREFUSED 127.0.0.1:3001` flooding stderr. **Stop the dev process** (Ctrl+C) before moving on.

- [ ] **Step 4: Take a snapshot of the failure log**

Capture the last ~30 lines of the Tauri dev stdout + stderr to a scratch file under `tmp/` (or similar out-of-tree location — **do not commit**). This becomes the baseline for Task 4's comparison. Do not commit this snapshot.

- [ ] **Step 5: No commit yet**

We're just confirming the repro. Do not commit anything in this task.

---

## Task 2: Add `resolveOpencodeBinary()` helper + imports

**Files:**
- Modify: `scripts/tauri-dev.mjs` (imports block at line 20-24, new helper near line 51)

- [ ] **Step 1: Extend the `node:fs` import to include `existsSync`**

Current (line 21):
```js
import { rmSync } from 'node:fs';
```
Replace with:
```js
import { existsSync, rmSync } from 'node:fs';
```

- [ ] **Step 2: Add `node:os` import**

Current import block (lines 20-24):
```js
import { spawn, spawnSync } from 'node:child_process';
import { rmSync } from 'node:fs';
import net from 'node:net';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
```
Replace with:
```js
import { spawn, spawnSync } from 'node:child_process';
import { existsSync, rmSync } from 'node:fs';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
```
Note: imports are alphabetized by `from` module name. `os` slots between `net` and `path`.

- [ ] **Step 3: Add the `resolveOpencodeBinary()` helper**

Insert immediately after the existing `resolveWindowsCommand` helper (currently ending at line 63), before `function spawnProcess`:

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

- [ ] **Step 4: Sanity-check the file still parses**

Run: `node --check scripts/tauri-dev.mjs`
Expected: silent (exit 0). If a syntax error is reported, fix it before proceeding.

- [ ] **Step 5: Commit**

```bash
cd "E:\Company\workspace\OpencodeIDE\openchamber"
git add scripts/tauri-dev.mjs
git -c user.name=hailinlu -c user.email=hailinlu@local commit -m "feat(scripts/tauri-dev): add resolveOpencodeBinary helper for Windows auto-detect"
```

---

## Task 3: Wire the helper into the cargo spawn

**Files:**
- Modify: `scripts/tauri-dev.mjs` (call site at lines 230-239, inside `main()`)

- [ ] **Step 1: Add the resolve + log line before the cargo spawn**

Current code (lines 228-239):
```js
  const backendMode = process.env.OPENCHAMBER_SIDECAR === '1' ? 'sidecar' : 'in-process';
  console.log(`[tauri:dev] backend mode: ${backendMode}`);

  // cargo tauri dev 会自己 cargo run, 不需要我们 build。
  // cwd 指向 src-tauri 让 tauri-cli 找到 tauri.conf.json。
  const tauri = spawnProcess('cargo', ['tauri', 'dev'], {
    cwd: tauriSrcDir,
    env: {
      GRIDFORGE_HMR_UI_URL: `http://127.0.0.1:${uiPort}`,
      OPENCHAMBER_PORT: apiPort,
    },
  });
```

Replace with:
```js
  const backendMode = process.env.OPENCHAMBER_SIDECAR === '1' ? 'sidecar' : 'in-process';
  console.log(`[tauri:dev] backend mode: ${backendMode}`);

  // 自动探测 OpenCode binary 路径 (Windows npm 全局 / Unix 常见位置)。
  // 仅在用户没显式设 OPENCODE_BINARY 时探测,探测失败回退到 'opencode' 让 Rust 报原本的错误。
  const opencodeBinary = resolveOpencodeBinary();
  const opencodeSource = (process.env.OPENCODE_BINARY || '').trim() ? '(from env)' : '(auto-detected)';
  console.log(`[tauri:dev] opencode binary: ${opencodeBinary || 'opencode'}${opencodeBinary ? ' ' + opencodeSource : ''}`);

  // cargo tauri dev 会自己 cargo run, 不需要我们 build。
  // cwd 指向 src-tauri 让 tauri-cli 找到 tauri.conf.json。
  const tauri = spawnProcess('cargo', ['tauri', 'dev'], {
    cwd: tauriSrcDir,
    env: {
      GRIDFORGE_HMR_UI_URL: `http://127.0.0.1:${uiPort}`,
      OPENCHAMBER_PORT: apiPort,
      OPENCODE_BINARY: opencodeBinary || 'opencode',
    },
  });
```

- [ ] **Step 2: Re-check the file parses**

Run: `node --check scripts/tauri-dev.mjs`
Expected: silent (exit 0).

- [ ] **Step 3: Re-read the modified region to spot-check**

Re-read `scripts/tauri-dev.mjs` lines 220-260 (or whatever the new line range is). Verify:
- `resolveOpencodeBinary()` is called exactly once.
- `OPENCODE_BINARY: opencodeBinary || 'opencode'` is in the `env` block.
- No other call sites reference `opencodeBinary` (the helper is internal-only).

- [ ] **Step 4: Commit**

```bash
cd "E:\Company\workspace\OpencodeIDE\openchamber"
git add scripts/tauri-dev.mjs
git -c user.name=hailinlu -c user.email=hailinlu@local commit -m "feat(scripts/tauri-dev): wire opencode binary detection into cargo spawn"
```

---

## Task 4: Smoke test the three scenarios

**Files:** none (manual verification only).

These are the manual smoke tests from the spec § "Verification". No automated tests exist; the script has no test infrastructure (intentional, see spec).

- [ ] **Step 1: Smoke test #1 — auto-detect success (Windows + npm global)**

Pre-conditions:
- `OPENCODE_BINARY` is unset (verify with `echo "$OPENCODE_BINARY"`).
- `C:\Program Files\nodejs\node_modules\opencode-ai\bin\opencode.exe` exists.
- Working directory is repo root.

Run: `bun run tauri:dev`

Expected within ~30 s of Tauri boot:
```
[tauri:dev] opencode binary: C:\Program Files\nodejs\node_modules\opencode-ai\bin\opencode.exe (auto-detected)
[oc_server::opencode][INFO] spawning opencode port=…
[oc_server::opencode][INFO] opencode is healthy and ready base_url=http://127.0.0.1:…
[oc_tauri_lib][INFO] oc-server (in-process) ready on port …
```

The Tauri window opens with the normal UI (no "We could not verify the UI session" error).

Stop the dev process (Ctrl+C). Verify clean shutdown — no new errors during teardown.

- [ ] **Step 2: Smoke test #2 — explicit override**

Pre-conditions: same as Step 1.

Run: `OPENCODE_BINARY="/c/Program Files/nodejs/opencode.cmd" bun run tauri:dev`

Expected log line:
```
[tauri:dev] opencode binary: /c/Program Files/nodejs/opencode.cmd (from env)
```
Note `(from env)`, not `(auto-detected)`. Tauri should boot the same as Step 1. (Using the `.cmd` shim path explicitly here exercises that the user's existing workaround from the diagnosis still works — we're not regressing it.)

Stop the dev process.

- [ ] **Step 3: Smoke test #3 — auto-detect miss (silent fallback)**

Pre-conditions: `OPENCODE_BINARY` unset, but pretend npm-global has no `opencode-ai` package.

Easiest portable way: temporarily rename or move `C:\Program Files\nodejs\node_modules\opencode-ai` aside. If you can't touch system files, run:

```bash
PATH="/usr/bin:/bin" bun run tauri:dev
```

This strips the npm bin from `cargo run`'s PATH. Even though `npm root -g` still resolves the right directory, the subsequent `existsSync` on the .exe will fail because the .exe won't be there… actually, that won't work — the .exe is a real file regardless of PATH. To force the miss path reliably, instead use:

```bash
# Force npm root -g to return a directory we control
NPM_CONFIG_PREFIX="$TMPDIR/fake-npm-prefix" mkdir -p "$TMPDIR/fake-npm-prefix"
# This fake prefix has no node_modules/opencode-ai/bin/opencode.exe
PATH="/usr/bin:/bin" OPENCODE_BINARY= bun run tauri:dev
```

Or, simplest: temporarily move the opencode-ai package, run the test, move it back.

Expected log line:
```
[tauri:dev] opencode binary: opencode
```
No `(auto-detected)` or `(from env)` suffix because the resolved value is falsy (we explicitly avoid printing the suffix when the value is the literal fallback).

Then, expected oc-server error (within ~30 s):
```
[oc_server::opencode][INFO] spawning opencode port=…
[oc_tauri_lib][ERROR] oc-server embed startup failed:
    failed to start opencode: failed to spawn opencode binary `opencode`: program not found
```

This **must be identical** to the pre-fix error from Task 1 Step 3 — same wording, same exit path. If the error wording has changed, something is wrong with the `|| 'opencode'` fallback.

Restore whatever you moved. Stop the dev process.

- [ ] **Step 4: Compare against the pre-fix baseline**

Open the snapshot you saved in Task 1 Step 4. Confirm:
- Pre-fix error wording matches smoke test #3 error wording byte-for-byte.
- Post-fix Tauri window (smoke test #1) does **not** show the "We could not verify the UI session" screen.
- Vite `ECONNREFUSED 127.0.0.1:3001` noise (AGENTS.md startup-latency row 4) is unchanged — that's a known separate issue, not regressed by this fix.

- [ ] **Step 5: No commit (verification only)**

Smoke tests don't add code. If everything passes, you're done.

---

## Task 5: Final repo-wide validation

**Files:** none.

- [ ] **Step 1: Type-check**

Run from repo root: `bun run type-check`
Expected: same exit code as before the change (the modified file is `.mjs`, not TS, but the script participates in lint scope).

- [ ] **Step 2: Lint**

Run from repo root: `bun run lint`
Expected: same exit code as before. No new ESLint warnings on `scripts/tauri-dev.mjs`. If a warning appears (e.g. about `spawnSync` arg shape), fix the code, not the lint rule.

- [ ] **Step 3: Confirm only two commits land on this branch**

Run: `git log --oneline -5`

Expected to see, on top of the pre-existing baseline (`13094bae docs(spec): …`):
1. `feat(scripts/tauri-dev): add resolveOpencodeBinary helper for Windows auto-detect`
2. `feat(scripts/tauri-dev): wire opencode binary detection into cargo spawn`

And nothing else (no smoke-test scratch files, no stray edits).

- [ ] **Step 4: Confirm no stray files**

Run: `git status`

Expected: clean working tree. The `tmp/` scratch snapshot from Task 1 Step 4 must not be tracked.

- [ ] **Step 5: Done**

Stop here. Report success against the four acceptance criteria from spec § "Behavior matrix":
1. Unset env + npm-global present → auto-detect works.
2. Unset env + nothing installed → silent fallback, original error preserved.
3. Explicit env → respected, no probing.
4. Explicit env to non-existent path → forwarded verbatim, Rust reports spawn failure.

---

## Acceptance criteria recap (from spec)

| Criterion | Where verified |
|---|---|
| Auto-detect picks `node_modules\opencode-ai\bin\opencode.exe` on Windows | Task 4 Step 1 |
| Explicit `OPENCODE_BINARY` overrides detection with `(from env)` label | Task 4 Step 2 |
| Detection miss falls back to literal `"opencode"` and preserves Rust's error | Task 4 Step 3 |
| Type-check + lint pass | Task 5 Steps 1-2 |
| Only `scripts/tauri-dev.mjs` modified | Task 5 Step 3-4 |