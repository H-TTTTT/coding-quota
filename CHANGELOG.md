# Changelog

All notable changes to this project are documented in this file.

## Unreleased

### Fixed

- Grok plan expiry showed the quota reset time instead of the subscription date. `/v1/billing?format=credits` returns the **weekly usage** period (`currentPeriod.end` and `config.billingPeriodEnd` are the same instant), so it was mislabelled as the expiry. The real monthly billing cycle now comes from `/v1/billing` without the query string, falling back to `plan_expiry.json` when unavailable.
- The desktop widget clipped content on the right: the window width was fixed at 340px while provider titles grew (plan name + expiry date) and the reset column switched to `6天·09-04`. The width is now fitted to the widest report card, clamped to 300..560 logical px.

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
- Plan subscription expiry dates: GLM from `/api/biz/subscription/list` (renewal date + product name), Cursor from `billingCycleEnd`, Grok from the monthly `/v1/billing` period end, and Codex/Kimi via optional `%APPDATA%\coding-quota\plan_expiry.json`.

### Fixed

- Standalone TUI launch without MinGW DLLs.
- Recursive TUI resize events.
- Temporary credential copies are deleted after use.
