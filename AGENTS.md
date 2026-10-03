# Repository Guidelines

## Project Overview

`coding-quota` is a Rust 2021 quota viewer for Codex, Claude, Grok, GLM, Kimi, Cursor, Devin and Google Antigravity. It provides a Windows-oriented terminal dashboard and an egui desktop widget; both consume the same credential and usage model.

## Architecture & Data Flow

- `src/main.rs` is the `coding-quota-tui` entry point. It dispatches interactive TUI, `--demo`, `--json`, `--snapshot`, `--provider` and `--watch` modes. `src/tui.rs` is a private binary module, not a library export.
- `src/lib.rs` exposes `credentials`, `fetch`, `model`, `cache` and `render`. Flow: `credentials::load()` → `fetch::fetch_all()` → `Snapshot` / `ProviderReport` / `QuotaWindow` → UI rendering.
- `fetch_all_streaming` runs the eight providers in one `JoinSet` and invokes its callback as each report lands (`fetch_all` wraps it with an empty callback); it takes a skip list of tray-hidden providers (treated as unsubscribed: no request, no token refresh) and reuses one process-wide `OnceLock` reqwest/rustls client with a 20-second timeout. The Devin CLI launch is the only blocking fetch: it lives inside its own task via `spawn_blocking`, so it cannot stall the other providers into a timeout. Providers with 2+ consecutive network/parse failures stop being requested for `min(2^n × 5 min, 1 h)` (`src/backoff.rs`); a success or a 401 refresh-retry clears the streak. Claude reads the omp `anthropic` OAuth credential (API-key rows are not subscriptions) and calls `api.anthropic.com/api/oauth/usage` plus `/profile` for the plan tier; prefer the legacy `five_hour`/`seven_day` fields and fall back to the `limits` array. Devin prefers the seat-management API omp itself uses (`server.codeium.com/exa.seat_management_pb.SeatManagementService/GetUserStatus`, Connect RPC over `application/proto`; the omp `devin` token goes into the protobuf metadata `apiKey` field with the `devin-session-token$` prefix for OAuth logins) and returns both daily and weekly windows live; when that credential is missing or fails, fall back to the CLI banner (read via a hidden `conhost.exe --headless` launch of `devin.exe`, showing only whichever of the daily/weekly quotas is tighter, attributed by `merge_devin_banner` by reset time) plus the CLI's `user_status.*.bin` cache for the other window; only self-spawned PID trees may be terminated, never a Devin process by name. Provider parsers normalize usage into `used_fraction`; display remaining quota as `1 - used_fraction`.
- UI refreshes save successful reports through `cache::save`, then use `cache::apply` to recover last-good values for failed providers. Preserve the error and original `fetched_at`; show stale data dimmed with its age. JSON/text snapshot modes expose the current fetch result, not cache-recovered data.
- Missing authorization is distinct from a failed request: `ProviderReport::missing` / `is_missing()` use the exact sentinel `no credential found`. omp logout soft-deletes credentials by setting `auth_credentials.disabled_cause` to `deleted by user`; only rows with `disabled_cause IS NULL` may authorize providers. Missing providers must stay hidden and must not be restored from cache. TUI and GUI reread credentials on refresh.
- TUI uses a Tokio refresh task while polling keyboard events and drawing a spinner. GUI uses a worker thread with its own Tokio runtime and channels to the egui event loop; tray commands arrive separately.
- Antigravity uses the active `google-antigravity` OAuth row and its `projectId` / `project_id`. POST `{project: ...}` to `daily-cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary` with bearer auth and the existing omp-compatible Hub user agent: this endpoint rejects the generic viewer UA. No hardcoded project fallback. Grouped/flat buckets preserve Gemini and Antigravity-internal shared Claude/GPT 5h/weekly windows, including stale-cache backfill; identical bucket ids are deduplicated. These shared quotas are not standalone Claude or Codex subscription quotas. Disabled or unknown-amount buckets are not fabricated as available quota.

## Key Directories

- `src/`: shared library modules plus TUI entry/render/event handling.
- `src/bin/desktop.rs` and `src/bin/desktop/`: GUI entry and native Windows tray module. Keep tray helpers in the subdirectory: a new `src/bin/*.rs` file becomes a Cargo binary.
- `assets/`: committed icon and its Python generator.
- `.cargo/`: Windows GNU linker configuration.
- `dist/`: ignored deployment output, not a source of truth. `target/` is disposable Cargo output and can contain obsolete executables after renames.

## Development Commands

Run commands from the repository root:

- Build TUI on the current host: `cargo build --bin coding-quota-tui`.
- Build both release binaries: `cargo build --release --features gui --bins`.
- Explicit Windows cross-build: `cargo build --release --features gui --target x86_64-pc-windows-gnu --bins`.
- Test: `cargo test --all-targets --features gui`.
- Formatting check: `cargo fmt --all -- --check`. Lint: `cargo clippy --all-targets --features gui`.
- Credential-free preview: `cargo run --bin coding-quota-tui -- --demo` in an isolated terminal.
- One-shot diagnostics: `cargo run --bin coding-quota-tui -- --snapshot --provider codex`; replace `--snapshot` with `--json` for structured output.
- GUI: `cargo run --features gui --bin coding-quota-gui`.
- Regenerate icon, with Pillow installed: `python assets/make_icon.py`; rebuild to embed it.

On the current Windows workstation, Rust is available in WSL Ubuntu-24.04. Use `wsl.exe -d Ubuntu-24.04 --cd "/mnt/h/Develop/20260826 额度显示工具" -- /home/x06579/.cargo/bin/cargo build --release --features gui --target x86_64-pc-windows-gnu --bins`. Do not hide build failure behind a pipeline whose exit status comes from the last filter.

## Code Conventions & Common Patterns

- Follow existing Rust naming (`snake_case` functions/modules, `PascalCase` types) and four-space indentation. Limit formatting changes to the work in scope.
- Use `ProviderReport::ok`, `err` and `missing` constructors. Optional API fields stay `Option`; absent data is not zero. Keep provider alias handling in `ProviderId::parse_filter` and credential precedence in `credentials.rs`.
- Fetch/network failures are represented in reports rather than aborting the whole dashboard. Token-bearing error text must pass existing sanitization; keep concise transport errors.
- Fetchers take a client and credentials explicitly. Reuse that pattern rather than adding global HTTP clients or a dependency-injection framework.
- UI state belongs to the TUI loop / `DesktopApp`; communicate background results through existing task/channel boundaries. Never block the input loop on HTTP.
- Reuse shared labels/time formatters in `render.rs`. Keep measured content, visible-provider filtering and drawn content consistent so dynamic height does not reserve space for hidden cards.
- Overflowing GUI cards use `ScrollArea` and measure its full `content_size`; overflowing TUI cards keep the 48-row cap and scroll with keyboard navigation. TUI height/scroll calculations use the same wrapped `Paragraph::line_count` as drawing, with ratatui 0.29's enabled `unstable-rendered-line-info` feature. Recheck this API when upgrading ratatui; raw `Vec<Line>::len()` loses wrapped errors and can make the last quota unreachable.
- Token usage comes from `src/usage.rs`: incremental scans of `~/.omp/agent/sessions/**/*.jsonl` `message.usage` rows (totalTokens or the sum of input/output/reasoningTokens/cacheRead/cacheWrite), cached per file by (mtime, size) with a 120s store TTL. Rows are provider-level and attach via `usage::attach` at `fetch_all` / the GUI publish closure; attribution follows the currently active credential, not per-account history. Zhipu remote 30-day usage is fetched inside `fetch_glm` from the same-host `model-usage` endpoint. Never convert quota percentages into token counts; "local total" means logs still present on this device.
- Quota reset time, subscription expiry, token expiry and manual reset-credit inventory are different concepts. Never derive one from another or invent a missing count/date.
- GLM manual reset inventory comes from the read-only `/api/biz/customer-package-reset/list?targetType=PERSONAL` endpoint using the same raw API-key auth as quota usage. Count only `available: true` in `fiveHourResets` / `weekResets` and store each count in `QuotaWindow.resets_left`; missing inventory is unknown, not zero. Never call `/use` from this viewer.
- In GLM limit rows, `number` is the total only when it is at least the used value; the MCP row's `number=1` is a plan flag. Real MCP totals come from `currentValue` (used) + `remaining`; never render that row as a percent bar when real counts exist.
- Codex `credits.balance` / `credits.unlimited` map to `ProviderReport.credit_balance`, independently of `rate_limit_reset_credits`. Codex reset cards and credit balance are rendered directly below the primary rate limit window's progress bar as `重置卡：剩余 {resets} 次` and `积分：剩余 {balance}`, matching Zhipu's visual style. GUI/TUI share `render::credit_balance_cn`; snapshot/JSON and stale-cache backfill preserve both balances and window reset cards.
- Windows integration uses handwritten FFI under `cfg(windows)` with non-Windows alternatives. Preserve handle ownership, thread shutdown and platform guards.
- Quota alerts live in `src/alerts.rs`: pure threshold-crossing logic (`transition`) plus a `Tracker` keyed by `(ProviderId::ordinal, QuotaWindow::id)`. Only real crossings alert — low ≤10%, depleted ~0%, recovered <10%→≥50% — seeded from the cache on startup so restarts never replay history. The GUI worker fires one merged balloon per round via `tray::notify` (`Shell_NotifyIconW` NIM_MODIFY + NIF_INFO) when `tray::alerts_enabled()` (file `%APPDATA%\coding-quota\alerts_enabled.txt`, missing = on). Usage tooltips in the widget read `render::UsageItem::models`; keep the compact line and measurement in sync when changing `usage_blocks`.

## Important Files

- `Cargo.toml`, `Cargo.lock`: exact binary names, feature gates and pinned dependencies.
- `build.rs`: embeds `assets/icon.ico` into Windows binaries using `x86_64-w64-mingw32-windres`.
- `.cargo/config.toml`: configures MinGW GCC/ar for `x86_64-pc-windows-gnu`; it does **not** choose a default target.
- `src/credentials.rs`: readonly omp SQLite access, temporary-copy cleanup and `omp token` refresh integration.
- `src/cache.rs`: `%APPDATA%\coding-quota\last_good.json` persistence and stale-data policy.
- `src/main.rs`: Windows Terminal launcher, profile management and content-addressed console-host executable cache.
- `README.md`, `CHANGELOG.md`: user commands and behavior history; record user-visible changes under Unreleased.

## Runtime/Tooling Preferences

- Cargo/Rust are the application toolchain; no Bun/Node package manager is required for this repository. The external `omp` executable is used for token refresh, with an existing WSL fallback.
- Windows GNU builds need MinGW GCC, ar and windres. `rusqlite` bundles SQLite C sources. Runtime DLL needs vary by toolchain: inspect actual imports and preserve required DLLs rather than assuming every build is static.
- Current shipped names are **`coding-quota-tui.exe`** and **`coding-quota-gui.exe`**. Never launch or deploy stale `coding-quota.exe` / `coding-quota-desktop.exe` artifacts from older builds. Verify source/deployment hashes after copying.
- Credentials default to the omp `agent.db`; `CODING_QUOTA_DB` overrides its location. GLM/Kimi also support environment-key overrides. Never print tokens or commit databases, cookies, account dumps or `.env` files.
- Runtime state is outside the repo: `%APPDATA%\coding-quota\{last_good.json,window_pos.txt,hidden_providers.txt}`. Preserve user state during deployment.
- The user's development session runs in PowerShell/Windows Terminal. **Never kill WindowsTerminal, PowerShell, cmd or a process tree selected by a broad title/name match.** Do not send keys to, focus, resize or navigate the user's shared terminal for tests. Use isolated probes, exact child-process ownership and synthetic credential stores.

## Testing & QA

Credential logout regression coverage lives in the `#[cfg(test)]` module in `src/credentials.rs`; run `cargo test --lib soft_logout_removes_provider_and_login_restores_it`. No CI configuration or coverage threshold is configured. Compilation alone is not behavioral proof.
Antigravity parser/auth regressions: `cargo test --lib antigravity`. Wrapped TUI scrolling regression: `cargo test --bin coding-quota-tui scrolling_reaches_last_quota_after_wrapped_refresh_error`.

- For parsing/cache/auth changes, use deterministic synthetic payloads or a temporary SQLite database. Test removed credentials separately from 401/network failures and confirm cache cannot revive logged-out cards.
- `--demo` is the supported no-network TUI preview; it uses demo identities and disables live refresh. It cannot validate credential reload or network behavior.
- Verify TUI transitions with an isolated PTY of nonzero dimensions and the **current** binary name. Use bounded `select`/read deadlines; stale executables in `target/` caused misleading results previously.
- For GUI/TUI layout, verify the actual renderer or an isolated rendered buffer. Windows screenshot coordinates must be DPI-aware; a cropped/occluded image does not prove a layout defect.
- For live SQLite copies, use a consistent backup rather than copying database/WAL/SHM independently during writes. Delete temporary credential copies in cleanup even on failure.
