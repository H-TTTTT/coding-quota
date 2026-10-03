# Changelog

All notable changes to this project are documented in this file.

## [Unreleased]

### Changed

- 挂件加了单实例锁：重复启动 `coding-quota-gui.exe`（双击、开机自启 + 手动启动）不再出现第二个托盘图标和第二路后台刷新，只会弹一句「已经在运行」的提示后退出。锁按登录会话隔离，远程桌面会话里各开一个仍然允许。
- `--watch` 每轮重新读取凭据库：登录、登出、token 轮换、换 `CODING_QUOTA_DB` 都不必重启进程；watch 期间凭据库临时读不到只跳过本轮并打印一行原因，不再让整个 watch 退出（单发 `--snapshot` 仍按原语义直接失败）。
- 新增 Google Antigravity 额度卡片：读取 omp 的有效 `google-antigravity` OAuth 登录及 projectId，按 Antigravity Hub 请求规范查询实时额度；显示 Gemini 与平台内 Claude/GPT 共享额度的 5 小时、周窗口、剩余百分比及重置时间，共享 bucket 去重。这是 Antigravity 内的模型额度，不是独立 Claude/Codex 订阅额度。GUI、TUI、托盘开关、`--snapshot` / `--json` 和 `--provider antigravity` / `google-antigravity` 均已接入；登出后隐藏，失败沿用已有缓存及退避规则，401 沿用 omp token 刷新。
- GUI 用量行悬浮查看模型明细：鼠标悬停在「1天 / 7天 / 累计」任一档位上，弹出该周期内按用量降序的模型列表（模型名、占比、token 数，最多 8 项），Gemini / Claude&GPT 分组各自的明细独立可用。
- 额度告警与 Windows 原生通知：剩余百分比跌破 10%、归零、或重置回满（<10% 回到 ≥50%）时从托盘弹系统气泡，一轮多条合并为一条；只在真实跨越时触发，重启用缓存播种不回放历史。托盘菜单新增「额度告警」开关（默认开启，状态存 `%APPDATA%\coding-quota\alerts_enabled.txt`）与「测试通知」入口。

### Fixed

- Kimi 的「总额度」实际就是每周额度：卡片改为直接读官方 `usages.limit_5h` / `usages.limit_7d` 字段（used_ratio），标签修正为「5 小时限额」「每周额度」；`usages` 子对象缺失时逐条退回旧的 `limits[]` / `usage` 块。月额度不在 coding API 返回中（只在 kimi.com 会员页，需浏览器登录态），无法经 omp 凭据显示。
- 把 exe 移动或改名之后，开机自启不再静默失效：启动时检查 `HKCU\...\Run` 里的路径（仅在自启已开启时），与实际位置不一致就改写为当前可执行文件，比较忽略大小写。
- 进程被强杀（关掉终端窗口、结束任务）时来不及删除的只读凭据数据库副本（`%TEMP%\coding-quota-*-agent.db` 及其 `-wal` / `-shm`）现在会在启动时清扫；修改时间不足 10 分钟的副本视为并发进程正在使用，不会误删。
- Devin 的日/周额度现在都是实时值：卡片优先走 omp 同款的 seat-management 接口（`server.codeium.com/exa.seat_management_pb.SeatManagementService/GetUserStatus`，用 omp 里的 `devin` 登录授权），不再出现「非更紧窗口停在旧值、与网页对不上」的情况；无该凭据或请求失败时仍回退 CLI 横幅 + `user_status` 缓存。
- 智谱重置卡不再遗漏：用现有 Coding Plan API key 只读查询 `/api/biz/customer-package-reset/list?targetType=PERSONAL`，按官网的 `available: true` 分别统计 5 小时和周重置卡；GUI、TUI、文本快照和 JSON 均显示对应窗口的剩余次数。不可用记录不计数，查询失败保留正常额度，不伪造零次；不调用消耗重置卡的接口。
- Codex（含 Pro 套餐）现在显示 `/wham/usage` 的 `credits.balance` 积分余额，与限流重置次数分开；重置卡与积分余额均改在主限流窗口（7 天窗口 / 5 小时窗口）进度条下方显示「重置卡：剩余 X 次」与「积分：剩余 X」，文案、位置与样式与智谱保持完全一致，不再堆在卡片顶部。GUI/TUI 的尺寸测量包含新增行，`--snapshot` / `--json` 同步输出。
- 新增 token 用量统计（借鉴 dushan-quota 的数据源思路，纯 Rust 实现）：增量扫描本机 OMP 会话日志（按 mtime+size 缓存，未变文件不重扫），按平台聚合近 1 天 / 7 天 / 30 天 / 本机累计与模型明细，`--json` 输出完整模型树；卡片底部以半透明微黑底条精简展示为紧凑单行「用量：1天 X · 7天 Y · 累计 Z」（过滤信息重叠的近 30 天，去掉多余的 `.0` 尾随），高亮白字适配浅色明亮桌面壁纸；智谱卡片独立第二行展示「官方：30天 W」；Google Antigravity 卡片自动按模型前缀将用量精确拆分为「Gemini：...」与「Claude&GPT：...」双行独立显示，分别对应上方的 Gemini 额度与 Claude&GPT 共享额度。
- MCP 每月额度现在显示剩余次数：智谱接口里 MCP 行的 `number=1` 是套餐标志位不是总量，之前 75/4000 被当成 1% 的百分比条。修正为按 `currentValue`（已用）+ `remaining`（剩余）计算总量，GUI、TUI、快照与 JSON 一致显示 `剩余 3925/4000` 这样的次数。
- 卡片增多或屏幕较小时，GUI 正文现在可纵向滚动，TUI 支持方向键、PgUp/PgDn、Home/End，末张 Antigravity 卡片不再因高度上限不可达。TUI 的量高、滚动范围与实际自动换行使用同一口径；长错误提示换行后也能滚到最后一项。

## 0.3.1 - 2026-09-26

### Added

- Claude provider card for Claude Pro / Max subscriptions, read from the omp `anthropic` OAuth login: 5-hour and weekly windows (plus per-model weekly windows when the plan has them) from the same `/api/oauth/usage` endpoint Claude Code's `/usage` uses, with the plan name (Pro / Max 5x / Max 20x) from `/api/oauth/profile`, looked up at most every 6 hours per account because the endpoint's rate limit is shared with omp and Claude Code. Falls back to the newer `limits` array when the legacy `five_hour` / `seven_day` fields are absent. Console API-key logins have no subscription quota and are treated as unsubscribed.

### Fixed

- Refresh rounds no longer fail en masse with `请求超时`: the blocking Devin banner fetch (up to 25s) ran inside `tokio::join!` and stalled every other provider's request until they all hit the 20s timeout. It now runs on the blocking thread pool.
- The Devin plan name no longer picks up TUI noise from the headless console (model selector `SWE-2 Max`, `Press alt+m to switch between available models`, `clipboard` glued to `Pro`); the plan is matched as a known plan-name suffix.
- Devin's weekly line no longer sometimes repeats the daily value: the CLI banner shows only whichever quota is tighter (the daily one once it runs lower than the weekly), but it was always taken as the weekly quota. The live banner value is now attributed to the daily or weekly window by its reset time, and the other window comes from the `user_status` cache.
- HTTP 429 responses are reported as one short line (`HTTP 429 请求过于频繁，稍后自动重试`) instead of the raw JSON body, which stretched the widget to its maximum width.
- A transient credential-store read failure no longer turns Devin's card into an error line: Devin reads the CLI banner rather than the credential database, so its last good quota stays on screen while the other cards report the failure.
- Cached quotas older than 24 hours are no longer shown as a fallback: the card keeps only the error instead of a multi-day-old value (Kimi had been displaying 09-20 numbers for five days).

### Changed

- The widget now draws each provider as soon as its data arrives instead of waiting for the slowest one: Codex / Cursor land in a second or two, while the Devin CLI banner (up to 25s) fills in its card afterwards. The refresh icon keeps spinning until the whole round is done, including rounds where every provider is skipped.
- A provider that keeps failing is retried with exponential backoff (5 minutes, doubling to a 1-hour cap, reset by the first success) instead of being re-requested every round. While it is deferred the card keeps the last known quota and adds a line saying when it will retry, which matters for Claude because its usage endpoint shares a rate limit with Claude Code and omp.
- The HTTP client is created once and reused across rounds, so connections and TLS sessions stay alive instead of being re-established each refresh.

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
