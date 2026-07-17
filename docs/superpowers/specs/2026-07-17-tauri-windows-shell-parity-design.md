# Tauri Windows Shell Parity Design

**Date:** 2026-07-17
**Status:** Approved

## Goal

Bring the Tauri Windows shell to behavioral and visual parity with the packaged Electron shell while continuing to reuse the shared web UI. The Tauri main window will use the existing web-rendered window controls, create its tray eagerly, honor minimize-to-tray settings, and expose the application and tray menu behavior already expected by the UI.

This work is Windows-scoped. macOS keeps its native traffic lights, vibrancy, and menu behavior. Linux keeps its current window decorations and menu behavior.

## Current State and Root Causes

The Electron shell creates a frameless Windows window and lets the shared UI render minimize, maximize/restore, and close controls. Tauri currently creates its main window with default Windows decorations, so Windows renders a native title bar above the same shared UI.

The Tauri tray implementation exists, but it is created lazily only after the UI sends the first `desktop_tray_update`. If UI bootstrap or backend startup is delayed, no tray is visible. Tauri also persists the minimize-to-tray setting without consuming it in `WindowEvent::CloseRequested`.

Additional parity gaps are the missing `desktop_show_app_menu` implementation, incomplete tray menu content, fragile tray menu ID parsing, and missing maximize-state events for the web control icon.

## Chosen Approach

Use platform-aware Rust shell behavior rather than globally changing `tauri.conf.json` or creating a second Tauri-specific title bar.

At startup, the Rust shell will make only the Windows main window frameless. The existing shared `WindowsWindowControls`, desktop bridge commands, and `app-region-drag`/`app-region-no-drag` CSS remain the source of the visible title bar behavior.

This approach keeps one shared UI implementation and confines native lifecycle policy to the shell.

## Window Chrome and Lifecycle

### Windows-only frameless main window

During Tauri main-window initialization, disable native decorations only when targeting Windows. Do not change the macOS or Linux main-window configuration.

The existing UI controls continue invoking:

- `desktop_minimize_current_window`
- `desktop_toggle_current_window_maximized`
- `desktop_close_current_window`
- `desktop_start_window_drag`

No Tauri-specific duplicate controls will be introduced.

### Maximize-state synchronization

The Rust window event handler will detect maximize-state changes caused by any path, including the web button, title-bar double-click, and Windows snap gestures. It will emit `openchamber:window-maximized-changed` only when the state changes.

The shared UI will continue using this event to choose between maximize and restore icons. Resize events that do not change maximize state must not produce duplicate notifications.

### Close-to-tray policy

On Windows, a close request uses the persisted `desktopMinimizeToTrayEnabled` setting:

- When enabled, prevent close and hide the main window.
- When disabled, allow the close and normal application shutdown.
- A deliberate Quit action sets an explicit quit-requested flag and always exits instead of being intercepted.

Hiding the window must not shut down the in-process backend. Backend shutdown remains tied to actual application exit or final window destruction.

If hiding fails, log the failure and allow the close request to proceed so the user is not left with an inaccessible background process.

## Tray Lifecycle

### Eager creation

Create the Tauri tray during application setup, independently of backend readiness and UI bootstrap. The initial tray uses the GridForge icon and a minimal usable menu.

Later `desktop_tray_update` snapshots update the existing tray instance rather than creating a second one. Tray creation failure is logged but does not prevent the window or backend from starting.

### Window restoration

Tray left-click and `Show GridForge` restore the main window in this order:

1. Unminimize.
2. Show.
3. Focus.

Individual failures are logged. A partially failed restoration must not panic the shell.

### Background launch

When launched with the existing background argument, initialize the tray but leave the main window hidden. Normal development launch remains visible.

## Tray Menu Parity

The Tauri tray menu will support the same primary actions and state groups as Electron:

- Pending approvals with Allow once, Always allow, Deny, and focus/open actions.
- Active sessions.
- `More…` submenus instead of silently truncating approvals or sessions.
- Usage information.
- New Session.
- New Mini Chat.
- Show GridForge.
- Quit.

Menu actions continue through the existing event bridge. The native shell presents menus and controls window lifecycle; shared UI/server logic owns session, permission, and usage operations.

Tray menu identifiers must use an unambiguous encoding or an internal lookup map. Parsing identifiers by underscore position is not acceptable because valid session and permission IDs may contain underscores.

Tray menu update failures retain the previous usable tray/menu state where supported. The implementation must not destroy the existing tray before a replacement menu is ready.

## Application Menu

Implement `desktop_show_app_menu` for the Tauri bridge. Clicking the shared UI hamburger button will show the Tauri application menu near the supplied button coordinates.

Windows will not display a permanent native `File / View / Help` menu bar after becoming frameless. The popup menu should provide the Electron-equivalent File, Edit, View, Go, and Help actions that Tauri currently supports.

Unsupported commands must be omitted or visibly disabled. They must not remain clickable and silently fail.

macOS retains its native application menu.

## Error Handling

- Tray creation failure: log an explicit error and continue startup.
- Tray menu update failure: preserve the prior menu when possible and report the error.
- Window hide failure during close-to-tray: log and allow close.
- Show/focus failure from tray: log each failed restoration operation.
- Application menu popup failure: return an explicit IPC error to the UI.
- Maximize-state query failure: do not emit a guessed state.
- Quit: set the explicit quit state before requesting application exit.

## Testing

### Rust tests

Add focused tests around logic extracted from native handles:

- Close policy for enabled/disabled minimize-to-tray settings and ordinary close versus explicit Quit.
- Tray menu model for basic items, Usage, New Mini Chat, and `More…` overflow behavior.
- Tray action identifier routing with IDs containing underscores.
- Maximize-state transition deduplication.
- Background-start argument detection.

Native Tauri calls that require a live window remain integration/manual checks, while policy and menu-model logic should be pure and unit-testable.

### UI tests

Verify:

- `openchamber:window-maximized-changed` switches the maximize/restore icon.
- Minimize, maximize/restore, and close buttons invoke the expected desktop commands.
- Tauri Windows uses the existing shared frameless controls rather than a second component.

### Build and static validation

Run the narrowest relevant checks:

- `cargo test` and `cargo check` for `oc-tauri`, with required local OpenSSL paths on Windows.
- Relevant UI package type-check, lint, and focused tests if UI code changes.
- Conditional-compilation checks sufficient to ensure macOS/Linux paths do not reference Windows-only APIs.

Known unrelated baseline failures must be reported separately rather than represented as regressions from this work.

## Manual Windows Acceptance

1. The system tray appears immediately after launch.
2. The white native title bar and permanent native menu strip are absent.
3. The web title bar can drag the window and double-click to maximize/restore.
4. Web minimize, maximize/restore, and close buttons work.
5. With minimize-to-tray enabled, close hides the window; with it disabled, close exits.
6. Tray left-click and Show GridForge restore and focus the window.
7. New Session, New Mini Chat, Usage, session, and approval tray entries work.
8. Tray Quit always exits the UI and embedded backend.
9. The web hamburger button opens the application menu.
10. macOS and Linux configuration and behavior remain unchanged.

## Out of Scope

- Synchronizing locale localStorage between Electron and Tauri origins.
- Changing OpenCode data directories or session storage.
- Rebuilding the packaged Electron installer.
- Adopting frameless chrome on Linux.
- Replacing native macOS traffic lights with web controls.
