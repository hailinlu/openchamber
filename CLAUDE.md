# OpenChamber

## Source of truth
See AGENTS.md for: architecture, commands, UI patterns, performance rules,
runtime entry points, OpenCode integration, and regression checklist.

## Quick commands
- Validate: `bun run type-check`, `bun run lint`
- Build all: `bun run build`
- Rust: `cargo build` / `cargo test` (in `rust/`)
- Desktop dev (Tauri): `bun run tauri:dev`
- Desktop dev (Electron, legacy): `bun run electron:dev`

## Architecture
- UI: React + TypeScript + Vite + Tailwind v4 (`packages/ui`, `packages/web`)
- Desktop (migration target): Tauri 2.11 (`rust/oc-tauri`)
- Desktop (legacy): Electron 41 (`packages/electron`)
- Server (Node): Express (`packages/web/server`)
- Server (Rust port): axum (`rust/oc-server`)
- VS Code: `packages/vscode`
- Package manager: pnpm; task runner: bun

# currentDate
Today's date is 2026-07-17.
