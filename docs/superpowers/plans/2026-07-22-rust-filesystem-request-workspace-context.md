# Rust Filesystem Request Workspace Context Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make Rust `/api/fs/*` routes validate target paths against the project/session/worktree directory supplied by the real request instead of an unrelated settings fallback.

**Architecture:** The existing web FilesAPI already reads `getDirectory()` at request time and sends it as `x-opencode-directory`; the defect is entirely in the Rust filesystem handlers, which currently replace real headers with `HeaderMap::new()`. Extract `HeaderMap` in every workspace-bound handler, pass it to a request-aware base-directory helper, and preserve all existing target-path, grant, home-listing, and settings-fallback rules.

**Tech Stack:** Rust 2021, axum 0.8 extractors, Tokio filesystem APIs, existing `project_dir` and `fs::workspace` modules, Cargo tests.

---

## File Structure

- Modify `rust/oc-server/src/fs/routes.rs`: extract real headers in filesystem handlers, resolve the request-scoped workspace root, and add focused helper tests.
- Modify `rust/oc-server/src/project_dir.rs`: add resolver tests proving raw and URI-encoded request directory headers win over settings fallback.
- Modify `rust/oc-server/src/fs/workspace.rs`: add portable boundary tests using existing directories to cover request-resolved roots and sibling rejection; do not change boundary policy unless a test exposes a Windows representation bug.
- Modify `rust/oc-server/src/fs/DOCUMENTATION.md` if present; otherwise update `rust/README.md` only if its migration parity table explicitly tracks filesystem request-context parity. Do not modify the Node module documentation because its behavior is already correct.

The shared web API requires no production change: `packages/web/src/api/files.ts:47-65` already computes `x-opencode-directory` at call time for `listDirectory`, and the same helper is used by mutations and reads.

### Task 1: Prove Request Directory Resolution

**Files:**
- Modify: `rust/oc-server/src/project_dir.rs:211-260`

- [ ] **Step 1: Add a test helper and failing raw-header precedence test**

Add these imports and helper inside `project_dir.rs`'s existing `tests` module:

```rust
use axum::http::{HeaderMap, HeaderValue};
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_test_dir(label: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "openchamber-project-dir-{label}-{}-{unique}",
        std::process::id()
    ));
    std::fs::create_dir_all(&path).expect("create temp project directory");
    path
}
```

Add the test:

```rust
#[tokio::test]
async fn request_header_directory_wins_over_settings() {
    let request_dir = temp_test_dir("request");
    let settings_dir = temp_test_dir("settings");
    let settings_path = settings_dir.join("settings.json");
    std::fs::write(
        &settings_path,
        serde_json::json!({ "lastDirectory": settings_dir }).to_string(),
    )
    .expect("write settings");

    let mut headers = HeaderMap::new();
    headers.insert(
        "x-opencode-directory",
        HeaderValue::from_str(request_dir.to_str().expect("utf-8 path"))
            .expect("valid header path"),
    );

    let resolved = resolve_project_directory(&headers, None, &settings_path).await;
    assert_eq!(resolved, request_dir.canonicalize().ok());

    std::fs::remove_dir_all(request_dir).ok();
    std::fs::remove_dir_all(settings_dir).ok();
}
```

- [ ] **Step 2: Run the focused test and record the baseline**

Run from `rust/`:

```bash
cargo test -p oc-server project_dir::tests::request_header_directory_wins_over_settings -- --exact
```

Expected: PASS. This is a characterization test proving the existing project-directory resolver is not the defective layer.

- [ ] **Step 3: Add URI-encoded header coverage**

Add a small portable encoder in the test module:

```rust
fn percent_encode_path(path: &str) -> String {
    path.bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}
```

Add the test:

```rust
#[tokio::test]
async fn uri_encoded_request_header_directory_is_decoded() {
    let request_dir = temp_test_dir("encoded request");
    let settings_path = request_dir.join("missing-settings.json");
    let encoded = percent_encode_path(request_dir.to_str().expect("utf-8 path"));

    let mut headers = HeaderMap::new();
    headers.insert(
        "x-opencode-directory",
        HeaderValue::from_str(&encoded).expect("valid encoded header"),
    );
    headers.insert(
        "x-opencode-directory-encoding",
        HeaderValue::from_static("uri"),
    );

    let resolved = resolve_project_directory(&headers, None, &settings_path).await;
    assert_eq!(resolved, request_dir.canonicalize().ok());

    std::fs::remove_dir_all(request_dir).ok();
}
```

- [ ] **Step 4: Run all project directory tests**

Run from `rust/`:

```bash
cargo test -p oc-server project_dir::tests
```

Expected: all `project_dir::tests` pass.

- [ ] **Step 5: Commit only if explicitly authorized**

The workspace instructions prohibit Git commands unless explicitly requested. If authorized, run:

```bash
git add rust/oc-server/src/project_dir.rs
git commit -m "test(oc-server): cover request directory resolution"
```

Otherwise leave the verified changes uncommitted and report that explicitly.

### Task 2: Make Filesystem Root Resolution Request-Aware

**Files:**
- Modify: `rust/oc-server/src/fs/routes.rs:129-198`
- Modify: `rust/oc-server/src/fs/routes.rs:204-324`
- Modify: `rust/oc-server/src/fs/routes.rs:330-400`
- Modify: `rust/oc-server/src/fs/routes.rs:482-525`

- [ ] **Step 1: Add a failing helper-level regression test**

In `routes.rs`'s existing test module, add imports and a helper:

```rust
use axum::http::{HeaderMap, HeaderValue};
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_test_dir(label: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "openchamber-fs-routes-{label}-{}-{unique}",
        std::process::id()
    ));
    std::fs::create_dir_all(&path).expect("create temp directory");
    path
}
```

Add a test against the new intended helper signature before implementing it:

```rust
#[tokio::test]
async fn resolve_base_dir_prefers_request_header() {
    let request_dir = temp_test_dir("request");
    let settings_dir = temp_test_dir("settings");
    let settings_path = settings_dir.join("settings.json");
    std::fs::write(
        &settings_path,
        serde_json::json!({ "lastDirectory": settings_dir }).to_string(),
    )
    .expect("write settings");

    let mut headers = HeaderMap::new();
    headers.insert(
        "x-opencode-directory",
        HeaderValue::from_str(request_dir.to_str().expect("utf-8 path"))
            .expect("valid header path"),
    );

    let resolved = resolve_base_dir_from_request(&headers, None, &settings_path).await;
    assert_eq!(resolved, request_dir.canonicalize().expect("canonical request dir"));

    std::fs::remove_dir_all(request_dir).ok();
    std::fs::remove_dir_all(settings_dir).ok();
}
```

- [ ] **Step 2: Run the test and verify it fails for the missing helper**

Run from `rust/`:

```bash
cargo test -p oc-server fs::routes::tests::resolve_base_dir_prefers_request_header -- --exact
```

Expected: FAIL to compile because `resolve_base_dir_from_request` does not exist.

- [ ] **Step 3: Implement the request-aware resolver helper**

Replace the context-free helper in `routes.rs` with:

```rust
async fn resolve_base_dir_from_request(
    headers: &HeaderMap,
    query_directory: Option<&str>,
    settings_path: &std::path::Path,
) -> PathBuf {
    crate::project_dir::resolve_project_directory(headers, query_directory, settings_path)
        .await
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")))
}

async fn resolve_base_dir(state: &AppState, headers: &HeaderMap) -> PathBuf {
    resolve_base_dir_from_request(headers, None, &state.settings_path).await
}
```

The split keeps the resolver independently testable without constructing the large `AppState`.

- [ ] **Step 4: Extract `HeaderMap` in all workspace-bound handlers**

Add `headers: HeaderMap` after `State(...)` and before body/query/path extractors in these handlers:

```rust
pub async fn mkdir(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<MkdirBody>,
) -> ApiResult<Json<Value>>
```

Apply the same extractor order to:

- `write`
- `delete`
- `rename`
- `reveal`
- `stat`
- `read`
- `raw`
- `serve`
- `list`
- `exec`

Do not add headers to `home`, `grant`, `exec_status`, or `clone`, because they do not resolve an operation target against an active workspace.

- [ ] **Step 5: Pass real headers to every base-directory call**

Change simple handler calls from:

```rust
let base_dir = resolve_base_dir(&state).await;
```

to:

```rust
let base_dir = resolve_base_dir(&state, &headers).await;
```

Apply this to `mkdir`, `write`, `delete`, `rename`, `reveal`, `serve`, `list`, and `exec`.

- [ ] **Step 6: Thread headers through normal read-path resolution**

Change `resolve_read_path` to accept headers:

```rust
async fn resolve_read_path(
    state: &AppState,
    headers: &HeaderMap,
    path: &str,
    scope: &str,
    query: &PathQuery,
) -> ApiResult<PathBuf> {
```

Keep the grant branch unchanged. In the normal branch use:

```rust
let base_dir = resolve_base_dir(state, headers).await;
```

Update callers:

```rust
let resolved = resolve_read_path(&state, &headers, path, "stat", &query).await?;
let resolved = resolve_read_path(&state, &headers, path, "read", &query).await?;
let resolved = resolve_read_path(&state, &headers, path, "raw", &query).await?;
```

- [ ] **Step 7: Run the helper regression test**

Run from `rust/`:

```bash
cargo test -p oc-server fs::routes::tests::resolve_base_dir_prefers_request_header -- --exact
```

Expected: PASS.

- [ ] **Step 8: Run all filesystem route tests**

Run from `rust/`:

```bash
cargo test -p oc-server fs::routes::tests
```

Expected: all route tests pass.

- [ ] **Step 9: Commit only if explicitly authorized**

If authorized:

```bash
git add rust/oc-server/src/fs/routes.rs
git commit -m "fix(oc-server): honor filesystem request workspace"
```

Otherwise leave changes uncommitted.

### Task 3: Guard Workspace Boundaries With Portable Regression Tests

**Files:**
- Modify: `rust/oc-server/src/fs/workspace.rs:135-214`

- [ ] **Step 1: Add an existing-directory workspace test**

In the existing `workspace.rs` test module add:

```rust
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_workspace(label: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "openchamber-workspace-{label}-{}-{unique}",
        std::process::id()
    ));
    std::fs::create_dir_all(&path).expect("create temp workspace");
    path
}

#[test]
fn existing_workspace_root_and_child_are_allowed() {
    let root = temp_workspace("root");
    let child = root.join("src");
    std::fs::create_dir_all(&child).expect("create child");

    assert!(resolve_workspace_path(root.to_str().expect("utf-8 root"), &root, None).is_ok());
    assert!(resolve_workspace_path(child.to_str().expect("utf-8 child"), &root, None).is_ok());

    std::fs::remove_dir_all(root).ok();
}
```

- [ ] **Step 2: Add sibling rejection coverage**

Add:

```rust
#[test]
fn existing_sibling_workspace_is_rejected() {
    let parent = temp_workspace("parent");
    let root = parent.join("active");
    let sibling = parent.join("sibling");
    std::fs::create_dir_all(&root).expect("create active root");
    std::fs::create_dir_all(&sibling).expect("create sibling");

    let result = resolve_workspace_path(
        sibling.to_str().expect("utf-8 sibling"),
        &root.canonicalize().expect("canonical root"),
        None,
    );
    assert!(matches!(result, Err(oc_core::Error::BadRequest(message)) if message == "Path is outside of active workspace"));

    std::fs::remove_dir_all(parent).ok();
}
```

- [ ] **Step 3: Run workspace boundary tests**

Run from `rust/`:

```bash
cargo test -p oc-server fs::workspace::tests
```

Expected: all workspace tests pass on Windows and the current host. If the root/child test fails only because one side uses a Windows verbatim prefix or case difference, stop and add one minimal normalization test before changing `is_path_within_root`; do not weaken the sibling rejection invariant.

- [ ] **Step 4: Commit only if explicitly authorized**

If authorized:

```bash
git add rust/oc-server/src/fs/workspace.rs
git commit -m "test(oc-server): guard filesystem workspace boundaries"
```

Otherwise leave changes uncommitted.

### Task 4: Document Rust Filesystem Context Ownership

**Files:**
- Create if absent: `rust/oc-server/src/fs/DOCUMENTATION.md`
- Otherwise modify: `rust/oc-server/src/fs/DOCUMENTATION.md`

- [ ] **Step 1: Check whether Rust filesystem module documentation exists**

Use the dedicated file tree/read tools, not shell scanning. If `rust/oc-server/src/fs/DOCUMENTATION.md` exists, preserve its structure. If absent, create it with:

```markdown
# Rust Filesystem Module

## Purpose

Own `/api/fs/*` behavior for oc-server, including workspace-bound path validation, directory listing, file reads and mutations, outside-workspace grants, reveal, and command execution.

## Request workspace context

Workspace-bound handlers resolve the active root from the real request context through `project_dir::resolve_project_directory`:

1. `x-opencode-directory` header
2. explicit directory query when a route defines one
3. settings fallback
4. process-current-directory fallback when no project can be resolved

The request workspace root and operation target are separate inputs. `x-opencode-directory` identifies the allowed root; `path` or a request-body path identifies the target that must be validated under that root.

## Security invariants

- A target path never authorizes itself as a workspace root.
- Read and mutation routes reject targets outside the request workspace unless a valid, scope-specific outside-file grant is supplied.
- Directory listing retains its intentional home-directory browsing allowance.
- Missing request context does not disable boundary validation.
```

- [ ] **Step 2: Cross-check the documentation against implementation**

Verify that every route named in the implementation uses request headers exactly as documented and that grant/home-list exceptions remain explicit.

- [ ] **Step 3: Commit only if explicitly authorized**

If authorized:

```bash
git add rust/oc-server/src/fs/DOCUMENTATION.md
git commit -m "docs(oc-server): describe filesystem workspace context"
```

Otherwise leave changes uncommitted.

### Task 5: Full Verification and Windows Tauri Regression

**Files:**
- Verify only; no planned source modifications.

- [ ] **Step 1: Run the complete oc-server test suite**

Run from `rust/`:

```bash
cargo test -p oc-server
```

Expected: all tests pass.

- [ ] **Step 2: Run Rust formatting verification**

Run from `rust/`:

```bash
cargo fmt --all -- --check
```

Expected: exit code 0. If formatting fails, run `cargo fmt --all`, then repeat the check.

- [ ] **Step 3: Run Rust compile verification**

Run from `rust/`:

```bash
cargo check -p oc-server
```

Expected: exit code 0 with no compile errors.

- [ ] **Step 4: Run the required shared-code checks**

No TypeScript production code is expected to change. Still verify that the existing request-header producer remains green from the repository root:

```bash
bun run type-check
bun run lint
```

Expected: exit code 0 for both, or report exact pre-existing failures without claiming success.

- [ ] **Step 5: Start Tauri development mode**

From the repository root run:

```bash
bun run tauri:dev
```

Expected startup evidence:

```text
[tauri:dev] opencode binary: ...
oc-server (in-process) ready on port ...
```

- [ ] **Step 6: Verify the original request**

In Tauri DevTools, with `E:/Company/workspace/demo/dat_raw` selected, run:

```js
fetch('/api/fs/list?' + new URLSearchParams({
  path: 'E:/Company/workspace/demo/dat_raw',
}), {
  headers: {
    'x-opencode-directory': 'E:/Company/workspace/demo/dat_raw',
  },
}).then(async (response) => ({
  status: response.status,
  body: await response.text(),
})).then(console.log)
```

Expected: status 200 and a JSON body containing an `entries` array.

- [ ] **Step 7: Verify the sidebar flow**

Open the Files tab, expand a child directory, and open a file. Expected: no `Path is outside of active workspace` response and no stale tree after switching projects.

- [ ] **Step 8: Verify the security boundary**

Run a request whose `path` is a sibling of the header workspace root:

```js
fetch('/api/fs/list?' + new URLSearchParams({
  path: 'E:/Company/workspace/demo/another-project',
}), {
  headers: {
    'x-opencode-directory': 'E:/Company/workspace/demo/dat_raw',
  },
}).then(async (response) => ({
  status: response.status,
  body: await response.text(),
})).then(console.log)
```

Expected: HTTP 400 with `Path is outside of active workspace` unless that sibling falls under the route's pre-existing home-listing allowance. For a strict assertion, repeat with `/api/fs/stat` against a file in the sibling and expect HTTP 400.

- [ ] **Step 9: Report completion without unrequested Git operations**

Summarize modified files, commands and results, original reproduction outcome, and security-boundary outcome. Do not commit, push, merge, or create a PR unless explicitly requested.
