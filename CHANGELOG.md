# Changelog

All notable changes to this project are documented in this file.

## Unreleased

### Added

- Claude provider card for Claude Pro / Max subscriptions, read from the omp `anthropic` OAuth login: 5-hour and weekly windows (plus per-model weekly windows when the plan has them) from the same `/api/oauth/usage` endpoint Claude Code's `/usage` uses, with the plan name (Pro / Max 5x / Max 20x) from `/api/oauth/profile`. Falls back to the newer `limits` array when the legacy `five_hour` / `seven_day` fields are absent. Console API-key logins have no subscription quota and are treated as unsubscribed.

### Fixed

- Refresh rounds no longer fail en masse with `请求超时`: the blocking Devin banner fetch (up to 25s) ran inside `tokio::join!` and stalled every other provider's request until they all hit the 20s timeout. It now runs on the blocking thread pool.
- The Devin plan name no longer picks up TUI noise from the headless console (model selector `SWE-2 Max`, `Press alt+m to switch between available models`, `clipboard` glued to `Pro`); the plan is matched as a known plan-name suffix.

## 0.3.0 - 2026-09-19

### Added

- Devin provider card: the weekly quota is fetched live from the CLI banner via a hidden `conhost.exe` launch of `devin.exe` (safe alongside running Devin tasks), and the daily quota is merged in from the CLI's `user_status` cache. Helper processes are spawned without console windows, so the widget no longer flashes a black console every refresh round.
- Providers hidden in the tray menu are now skipped entirely — no requests and no token refresh, treating hidden as unsubscribed. The CLI and TUI still fetch all providers.

### Fixed

- The last-good cache now merges successful reports per provider instead of rewriting the whole file per round, so a partially failed round no longer discards good data for the other providers.
- The tray icon is now removed when the widget exits: the add/delete calls used different icon ids, and the normal exit path never reached the tray thread's cleanup. The icon is now deleted synchronously from `on_exit`.
- Devin's daily quota line no longer disappears while a quota is exhausted: the `user_status` protobuf omits zero-valued fields (proto3), which used to make the whole cache read fail. Missing fields are now treated as 0% remaining.

### Changed

- Renamed the executables: `coding-quota-tui.exe` (terminal TUI/CLI, was `coding-quota.exe`) and `coding-quota-gui.exe` (desktop widget, was `coding-quota-desktop.exe`).
- TUI reloads omp credentials on every refresh and hides providers whose credentials were removed, including after logout. Hidden providers no longer reserve vertical space; transient credential-read failures retain the startup credentials rather than falsely treating all accounts as logged out.
- Credential loading now excludes rows with a non-null `disabled_cause`. omp logout marks credentials `deleted by user` instead of deleting their rows; both UIs now treat these accounts as unauthorized rather than querying stale tokens.
- The CLI now reports its version with `--version` and lists all six providers (including Devin) in `--help`.

## 0.2.0 - 2026-09-10

### Added

- Codex remaining rate-limit reset count (`rate_limit_reset_credits.available_count`), shown in the desktop widget, TUI and `--snapshot` output.
- Desktop widget width is fitted to the widest report card instead of being fixed at 340px, so longer titles are no longer clipped on the right. 340 stays as the lower bound, so the widget keeps its usual width and only grows when the content needs more.
- Failed refreshes now keep showing the last good quota values with the error message alongside, instead of replacing the card with an error-only line. Last good reports are cached in `%APPDATA%\coding-quota\last_good.json` (so data survives a restart, e.g. network not ready at boot); restored values are drawn dimmed and labelled with their age. Applies to the desktop widget and the TUI; `--json`/`--snapshot` still report the raw result of the current round.
- Transport errors are condensed to a short phrase (`连接失败` / `请求超时`) instead of the full reqwest message with its URL, so error lines fit the widget card in one line.
- Clicking refresh (title-bar button or tray menu) spins the refresh icon itself until the new snapshot arrives; existing cards stay visible. The same spin covers the first load.
- TUI title-bar refresh spinner (braille) while a round is in flight; existing cards stay visible. First load included. Refresh no longer blocks the event loop.
- TUI layout: status dot per provider, reset time (`N天后重置`) at the right edge of the label row, remaining percent in a uniform-width bold column, and highlighted [Q]/[R] keys. Bars stretch to fill the card width with a dimmed track so only the filled part carries the status color, and the window height fits the content instead of a fixed 34 rows.
- Kimi's 5h limit window is now listed above the total quota (applies to the desktop widget, TUI and snapshot output).
- `--demo` renders the TUI with mock data (identities are `demo@example.com` etc.) for previews and screenshots, without touching credentials or the network.

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
