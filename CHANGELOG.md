# Changelog

All notable changes to this project are documented in this file.

## 0.1.0 - 2026-08-28

### Added

- Unified coding-plan quota viewer TUI (`coding-quota`) for Codex, Grok, GLM, Kimi, and Cursor; localized and compact layout.
- Desktop tray app (`coding-quota-desktop`): tray icon with runtime-drawn rounded progress ring, launch with Windows, per-provider visibility menu, quit from tray while the window is hidden.
- Window position memory and auto-fitted window height; providers without credentials are hidden.
- Borderless draggable TUI frame with locked dimensions and aligned reset times.
- Windows Terminal launchers: frameless TUI profile (hidden scrollbar, hidden profile entry) and desktop app shortcut.
- Cross-compilation setup (WSL + `x86_64-pc-windows-gnu`) with MinGW runtime DLLs bundled in `dist/`.
- Codex remaining rate-limit reset count (`rate_limit_reset_credits.available_count`).
- Every quota window shows its period expiry date (relative countdown · MM-DD, local timezone).
- Plan subscription expiry dates: GLM from `/api/biz/subscription/list` (renewal date + product name), Cursor/Grok from their billing period end, and Codex/Kimi via optional `%APPDATA%\coding-quota\plan_expiry.json`.

### Fixed

- Standalone TUI launch without MinGW DLLs.
- Recursive TUI resize events.
- Temporary credential copies are deleted after use.
