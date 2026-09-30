# coding-quota

See the remaining quota of seven AI coding plans in one place: **OpenAI Codex, Claude (Pro / Max), xAI Grok, Zhipu GLM Coding Plan, Kimi Code, Cursor, and Devin**.

![TUI screenshot](assets/tui.jpg)

## Two flavors

| | `coding-quota-gui.exe` (desktop widget) | `coding-quota-tui.exe` (terminal TUI / CLI) |
| --- | --- | --- |
| UI | Borderless acrylic widget pinned to the desktop | Compact 48-column TUI in a frameless Windows Terminal window |
| Interaction | Drag by the title bar, tray icon context menu | `R` refresh, `Q` quit, auto-refresh every 5 minutes |
| Extras | Launch with Windows, per-provider visibility, window position memory | `--snapshot` plain text, `--json` for scripts, `-p <provider>` to filter, `--demo` for a mock-data preview |

## Download

Grab the executables from [Releases](../../releases):

- `coding-quota-tui.exe` — terminal TUI / CLI
- `coding-quota-gui.exe` — desktop widget

Both are standalone (only Windows system DLLs are linked), no installation required.

## Credentials

Zero configuration: the tool reads the existing omp credential store (`~/.omp/agent/agent.db`) read-only and refreshes expiring tokens through omp. Providers without credentials are hidden automatically — nothing ever asks you to log in.

## Features

- Each quota window shows the remaining percentage, a usage-colored bar (green <70%, yellow ≥70%, red ≥90%), and its reset time
- Codex additionally shows remaining rate-limit reset credits and its credit balance (including Pro plans); finite balances retain decimals, unlimited credits are labelled explicitly, and unavailable balances are not shown as zero
- Zhipu shows available reset cards separately for the 5-hour and weekly windows; only records marked available are counted, and a reset-inventory query failure does not hide regular usage. Inventory queries are read-only: this tool never spends reset cards
- `--json` exposes Codex `credit_balance` (`kind: limited` with `balance`, or `kind: unlimited`) and each Zhipu window's optional `resets_left`; missing fields mean unknown, while a known empty reset-card inventory is zero
- Failed refreshes keep the last good values (cached in `%APPDATA%\coding-quota\last_good.json`), drawn dimmed with their age; values older than 24 hours are dropped rather than shown, and a provider that keeps failing is retried with backoff instead of every round. `--json` / `--snapshot` always report the live result
- Both the widget and the TUI fit their size to the content

## Build from source

```
cargo build --release --features gui
```

The Windows target is `x86_64-pc-windows-gnu` (see `.cargo/config.toml` for the linker setup).

## License

MIT
