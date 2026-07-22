# Rust Filesystem Request Workspace Context Design

**Date:** 2026-07-22
**Status:** Approved design

## Problem

In the Tauri in-process Rust backend, the sidebar file tree requests a directory such as:

```text
E:/Company/workspace/demo/dat_raw
```

The OpenCode session status request succeeds:

```http
GET /api/session/status?directory=E%3A%2FCompany%2Fworkspace%2Fdemo%2Fdat_raw
200 {}
```

But the OpenChamber filesystem request fails:

```http
GET /api/fs/list?path=E%3A%2FCompany%2Fworkspace%2Fdemo%2Fdat_raw
400 {"error":"bad request: Path is outside of active workspace"}
```

An empty session status object is a valid authoritative response meaning no sessions in that directory are currently busy or retrying. It does not validate the filesystem workspace boundary.

The Rust filesystem handlers currently resolve their base directory with an empty request context:

```rust
resolve_project_directory(&HeaderMap::new(), None, &state.settings_path)
```

This discards the request's current project/session/worktree directory and falls back to the project stored in settings. When that stored project differs from the directory selected in the UI, filesystem boundary validation rejects the selected directory.

The Node implementation does not discard this context. Its filesystem route passes the real request to `resolveProjectDirectory(req)` before validating the target path.

## Goals

- Resolve the filesystem workspace root from the real request context.
- Keep the workspace root distinct from the target file or directory path.
- Preserve filesystem boundary enforcement for list, read, write, delete, rename, mkdir, reveal, serve, and command execution where applicable.
- Preserve settings-based fallback when no request directory is supplied.
- Support Windows drive paths and URI-encoded directory headers.
- Maintain Node/Rust runtime parity.

## Non-goals

- Do not loosen filesystem boundary checks.
- Do not treat the requested target path as an implicitly trusted workspace root.
- Do not change session status semantics.
- Do not fix unrelated Base UI `nativeButton` warnings in this change.
- Do not redesign project selection or session state.

## Selected Approach

Use request-scoped workspace context for all Rust filesystem operations.

The inputs have separate responsibilities:

```text
x-opencode-directory = current project/session/worktree workspace root
path                 = target file or directory for this operation
```

The shared web FilesAPI will send the current effective directory through the existing `x-opencode-directory` request convention. The Rust handlers will extract the real request headers and pass them to `project_dir::resolve_project_directory`. If a filesystem route has an explicit `directory` query field, it may supply that as the secondary directory candidate. The existing resolver already supports both sources and checks that candidates exist.

Request flow:

```text
Shared UI effective directory
  -> Web FilesAPI request header
  -> Rust filesystem handler HeaderMap
  -> resolve_project_directory(headers, query_directory, settings_path)
  -> request-scoped workspace root
  -> resolve_workspace_path(target_path, workspace_root)
  -> filesystem operation
```

## Backend Design

### Request-context resolver

Replace the context-free filesystem `resolve_base_dir(state)` helper with a helper that accepts:

- `&AppState`
- `&HeaderMap`
- optional query directory

It will call the existing `project_dir::resolve_project_directory` implementation. Resolution priority remains:

1. `x-opencode-directory` header
2. explicit `directory` query value, where supported
3. configured active project/settings fallback
4. existing process-current-directory fallback only when no project directory can be resolved

The helper must not derive the workspace root from `path`. The target path is untrusted operation input and remains subject to boundary validation.

### Route coverage

Every filesystem route that validates a path must use the same request-scoped root. This includes the applicable handlers for:

- list
- stat/read/raw
- write/delete/rename/mkdir
- reveal
- serve
- command execution

This prevents list from succeeding while a subsequent read or mutation of the same path fails because it used a different workspace source.

Routes with outside-workspace grants keep their existing grant behavior. Request-scoped workspace resolution applies only to the normal workspace path.

### Worktrees

The initial fix preserves existing Rust boundary behavior and request-root selection. If Rust lacks Node's secondary worktree-root fallback, coverage should determine whether the supplied effective worktree directory already serves as the request root. Any broader worktree discovery port is outside this bug fix unless a failing regression test proves it is required for parity.

## UI and Transport Design

The shared UI already computes an effective directory using this priority:

1. attached worktree path
2. session worktree metadata
3. active session directory
4. open draft directory override
5. global directory fallback

The FilesAPI implementation should obtain the authoritative current directory at request time rather than capture it in a stale closure. It should attach that directory using the established `x-opencode-directory` header convention and preserve runtimeFetch ownership of base URL and authentication.

No hardcoded host or port is introduced. The request continues through RuntimeAPIs/runtimeFetch so web, Tauri, Electron, remote runtime, and VS Code boundaries remain explicit.

The FilesAPI method signatures do not need to overload `path` with workspace context. If the runtime implementation cannot obtain the effective directory safely at call time, extend the existing options with a distinct optional `directory` field rather than infer it from the target path.

## Security Invariants

- A request directory is a candidate workspace root, not permission to access arbitrary sibling paths.
- The resolver accepts only existing directory candidates.
- Target paths must equal or be descendants of the resolved workspace root, except for existing narrowly scoped user-config, home-listing, or grant behavior.
- A target path must never authorize itself.
- Missing request context falls back to current behavior; it does not disable validation.
- Read and mutation routes retain stricter boundaries than the intentional home-directory listing allowance.

## Error Handling

- An invalid or nonexistent request directory falls through the existing project-directory resolution chain.
- A target outside the resolved workspace continues to return HTTP 400 with `Path is outside of active workspace`.
- Missing required workspace context continues to use the existing deterministic fallback/error behavior.
- UI file-tree failures remain visible; the client must not turn a failed authoritative directory listing into a successful empty result.

## Testing

### Rust resolver and route tests

Add focused regression coverage for:

1. A Windows workspace root such as `E:/Company/workspace/demo/dat_raw` supplied through `x-opencode-directory`.
2. URI-encoded header values with `x-opencode-directory-encoding: uri`.
3. Listing the workspace root succeeds.
4. Listing or reading descendants succeeds.
5. A sibling project directory is rejected.
6. Missing request context retains settings fallback.
7. Read and mutation routes use the same request root as list.
8. Existing outside-workspace grant and home-list behavior remain unchanged.

Tests must be portable: use temporary directories for actual filesystem assertions and isolate Windows-specific normalization assertions where required.

### Web FilesAPI tests

Verify that filesystem requests:

- use RuntimeAPIs/runtimeFetch;
- attach the current directory context at call time;
- preserve existing request method, query, body, authorization, and abort behavior;
- do not derive or cache a runtime base URL locally.

### Manual regression

In Tauri on Windows:

1. Open project `E:/Company/workspace/demo/dat_raw`.
2. Open the sidebar Files tab.
3. Confirm `/api/fs/list` returns 200 and renders entries.
4. Expand a child directory and open a file.
5. Switch to another project and confirm requests use the new directory context.
6. Switch back and confirm no stale response populates the wrong tree.
7. Attempt a sibling path outside the selected workspace and confirm it remains rejected.

## Validation

Run the narrowest affected checks first:

- Rust filesystem/unit tests in the `rust` workspace.
- Relevant web/UI RuntimeAPI tests.
- Package-level UI/web type-check and lint if available.

Because the change crosses the shared UI-to-server contract and Rust backend, finish with the repository-required type-check and lint coverage appropriate to the affected workspaces.
