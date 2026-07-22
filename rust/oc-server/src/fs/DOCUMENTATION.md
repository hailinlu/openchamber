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
