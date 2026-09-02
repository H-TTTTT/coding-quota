# Changelog

All notable changes to this project are documented in this file.

## Unreleased

### Added

- Codex remaining rate-limit reset count (`rate_limit_reset_credits.available_count`), shown in the desktop widget, TUI and `--snapshot` output.
- Desktop widget width is fitted to the widest report card instead of being fixed at 340px, so longer titles are no longer clipped on the right. 340 stays as the lower bound, so the widget keeps its usual width and only grows when the content needs more.
- Failed refreshes now keep showing the last good quota values with the error message alongside, instead of replacing the card with an error-only line. Last good reports are cached in `%APPDATA%\coding-quota\last_good.json` (so data survives a restart, e.g. network not ready at boot); restored values are drawn dimmed and labelled with their age. Applies to the desktop widget and the TUI; `--json`/`--snapshot` still report the raw result of the current round.
- Transport errors are condensed to a short phrase (`连接失败` / `请求超时`) instead of the full reqwest message with its URL, so error lines fit the widget card in one line.
- Clicking refresh (title-bar button or tray menu) spins the refresh icon itself until the new snapshot arrives; existing cards stay visible. The same spin covers the first load.

## 0.1.0 - 2026-08-28

### Added

- Unified coding-plan quota viewer TUI (`coding-quota`) for Codex, Grok, GLM, Kimi, and Cursor; localized and compact layout.
- Desktop tray app (`coding-quota-desktop`): tray icon with runtime-drawn rounded progress ring, launch with Windows, per-provider visibility menu, quit from tray while the window is hidden.
- Window position memory and auto-fitted window height; providers without credentials are hidden.
- Borderless draggable TUI frame with locked dimensions and aligned reset times.
- Windows Terminal launchers: frameless TUI profile (hidden scrollbar, hidden profile entry) and desktop app shortcut.
- Cross-compilation setup (WSL + `x86_64-pc-windows-gnu`) with MinGW runtime DLLs bundled in `dist/`.

### Fixed

- Standalone TUI launch without MinGW DLLs.
- Recursive TUI resize events.
- Temporary credential copies are deleted after use.
