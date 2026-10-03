use anyhow::Result;
use chrono::Utc;
use coding_quota::cache;
use coding_quota::credentials::{self, CredentialSet};
use coding_quota::fetch;
use coding_quota::model::{ProviderId, ProviderReport, QuotaWindow, Snapshot, UsageRow};
use coding_quota::render::{
    ago_cn, bar_parts, compact_until_cn, credit_balance_cn, label_cn, status_color, title_cn,
    usage_blocks,
};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen, SetSize,
    SetTitle,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use std::io::{stdout, Stdout};
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;
use unicode_width::UnicodeWidthStr;

type AppTerminal = Terminal<CrosstermBackend<Stdout>>;
const BAR_MIN_WIDTH: usize = 22;
pub const TUI_COLUMNS: u16 = 48;
pub const TUI_LEFT_GUTTER: usize = 2;
pub const TUI_ROWS: u16 = 34;
const TUI_MIN_ROWS: u16 = 8;
/// 物理窗口高度上限保持原值：内容装不下时靠 ↑↓ 滚动到达末张卡片，不撑高终端。
const TUI_MAX_ROWS: u16 = 48;
const CHROME_ROWS: u16 = 5;
const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

const FG_MUTED: Color = Color::DarkGray;
const FG_ACCENT: Color = Color::Cyan;
const FG_ERR: Color = Color::Red;

#[cfg(windows)]
mod native_drag {
    use core::ffi::c_void;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    use std::thread::{self, JoinHandle};
    use std::time::Duration;

    static PROGRAMMATIC_RESIZE: AtomicBool = AtomicBool::new(false);

    pub fn begin_resize() {
        PROGRAMMATIC_RESIZE.store(true, Ordering::Relaxed);
    }

    pub fn end_resize() {
        PROGRAMMATIC_RESIZE.store(false, Ordering::Relaxed);
    }
    pub struct Watcher {
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    }

    #[repr(C)]
    #[derive(Default)]
    struct Point {
        x: i32,
        y: i32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct Rect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }

    #[link(name = "user32")]
    extern "system" {
        fn GetAsyncKeyState(key: i32) -> i16;
        fn ClientToScreen(hwnd: *mut c_void, point: *mut Point) -> i32;
        fn GetClientRect(hwnd: *mut c_void, rect: *mut Rect) -> i32;
        fn GetClassNameW(hwnd: *mut c_void, class: *mut u16, max: i32) -> i32;
        fn GetCursorPos(point: *mut Point) -> i32;
        fn GetWindowLongPtrW(hwnd: *mut c_void, index: i32) -> isize;
        fn GetForegroundWindow() -> *mut c_void;
        fn GetWindowRect(hwnd: *mut c_void, rect: *mut Rect) -> i32;
        fn SetWindowPos(
            hwnd: *mut c_void,
            after: isize,
            x: i32,
            y: i32,
            cx: i32,
            cy: i32,
            flags: u32,
        ) -> i32;
        fn SetWindowLongPtrW(hwnd: *mut c_void, index: i32, value: isize) -> isize;
        fn SetThreadDpiAwarenessContext(context: *mut c_void) -> *mut c_void;
        fn SetWindowRgn(hwnd: *mut c_void, region: *mut c_void, redraw: i32) -> i32;
    }

    #[link(name = "dwmapi")]
    extern "system" {
        fn DwmSetWindowAttribute(
            hwnd: *mut c_void,
            attribute: u32,
            value: *const u32,
            size: u32,
        ) -> i32;
    }

    #[link(name = "gdi32")]
    extern "system" {
        fn CreateRectRgn(left: i32, top: i32, right: i32, bottom: i32) -> *mut c_void;
        fn DeleteObject(object: *mut c_void) -> i32;
    }

    impl Watcher {
        pub fn start() -> Self {
            let stop = Arc::new(AtomicBool::new(false));
            let thread = if std::env::var_os("CODING_QUOTA_TUI_HOSTED").is_some() {
                let thread_stop = Arc::clone(&stop);
                Some(thread::spawn(move || watch_drag(thread_stop)))
            } else {
                None
            };
            Self { stop, thread }
        }
    }

    impl Drop for Watcher {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    fn watch_drag(stop: Arc<AtomicBool>) {
        unsafe {
            const DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2: isize = -4;
            SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2 as *mut c_void);
        }
        let Some(hwnd) = find_terminal_window(&stop) else {
            return;
        };
        thread::sleep(Duration::from_millis(300));
        lock_window_size(hwnd);
        let (mut fixed_width, mut fixed_height) = unsafe {
            let mut rect = Rect::default();
            if GetWindowRect(hwnd, &mut rect) == 0 {
                return;
            }
            (rect.right - rect.left, rect.bottom - rect.top)
        };
        let mut was_down = false;
        let mut offset: Option<(i32, i32)> = None;

        while !stop.load(Ordering::Relaxed) {
            unsafe {
                let down = GetAsyncKeyState(0x01) < 0;
                let mut cursor = Point::default();
                let mut rect = Rect::default();
                if GetCursorPos(&mut cursor) != 0 && GetWindowRect(hwnd, &mut rect) != 0 {
                    let width = rect.right - rect.left;
                    let height = rect.bottom - rect.top;
                    if PROGRAMMATIC_RESIZE.load(Ordering::Relaxed) {
                        fixed_width = width;
                        fixed_height = height;
                        clip_to_client(hwnd);
                    } else if width != fixed_width || height != fixed_height {
                        const SWP_NOZORDER: u32 = 0x0004;
                        const SWP_NOACTIVATE: u32 = 0x0010;
                        SetWindowPos(
                            hwnd,
                            0,
                            rect.left,
                            rect.top,
                            fixed_width,
                            fixed_height,
                            SWP_NOZORDER | SWP_NOACTIVATE,
                        );
                        rect.right = rect.left + fixed_width;
                        rect.bottom = rect.top + fixed_height;
                    }
                    if down && !was_down {
                        let foreground = GetForegroundWindow();
                        let in_header = cursor.x >= rect.left
                            && cursor.x < rect.right
                            && cursor.y >= rect.top
                            && cursor.y < rect.top + 32;
                        if foreground == hwnd && in_header {
                            offset = Some((cursor.x - rect.left, cursor.y - rect.top));
                        }
                    }
                    if down {
                        if let Some((offset_x, offset_y)) = offset {
                            const SWP_NOSIZE: u32 = 0x0001;
                            const SWP_NOZORDER: u32 = 0x0004;
                            const SWP_NOACTIVATE: u32 = 0x0010;
                            SetWindowPos(
                                hwnd,
                                0,
                                cursor.x - offset_x,
                                cursor.y - offset_y,
                                0,
                                0,
                                SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                            );
                        }
                    } else {
                        offset = None;
                    }
                }
                was_down = down;
            }
            thread::sleep(Duration::from_millis(8));
        }
    }

    fn lock_window_size(hwnd: *mut c_void) {
        unsafe {
            const GWL_STYLE: i32 = -16;
            const WS_OVERLAPPEDWINDOW: isize = 0x00CF_0000;
            const DWMWA_WINDOW_CORNER_PREFERENCE: u32 = 33;
            const DWMWCP_DONOTROUND: u32 = 1;
            DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &DWMWCP_DONOTROUND, 4);
            let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
            SetWindowLongPtrW(hwnd, GWL_STYLE, style & !WS_OVERLAPPEDWINDOW);
            const DWMWA_NCRENDERING_POLICY: u32 = 2;
            const DWMNCRP_DISABLED: u32 = 1;
            DwmSetWindowAttribute(hwnd, DWMWA_NCRENDERING_POLICY, &DWMNCRP_DISABLED, 4);
            const DWMWA_BORDER_COLOR: u32 = 34;
            const NO_BORDER: u32 = 0xFFFF_FFFE;
            DwmSetWindowAttribute(hwnd, DWMWA_BORDER_COLOR, &NO_BORDER, 4);
            const SWP_NOSIZE: u32 = 0x0001;
            const SWP_NOMOVE: u32 = 0x0002;
            const SWP_NOZORDER: u32 = 0x0004;
            const SWP_NOACTIVATE: u32 = 0x0010;
            const SWP_FRAMECHANGED: u32 = 0x0020;
            SetWindowPos(
                hwnd,
                0,
                0,
                0,
                0,
                0,
                SWP_NOSIZE | SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            );
            thread::sleep(Duration::from_millis(50));
            clip_to_client(hwnd);
        }
    }

    unsafe fn clip_to_client(hwnd: *mut c_void) {
        let mut window = Rect::default();
        let mut client = Rect::default();
        let mut origin = Point::default();
        if GetWindowRect(hwnd, &mut window) == 0
            || GetClientRect(hwnd, &mut client) == 0
            || ClientToScreen(hwnd, &mut origin) == 0
        {
            return;
        }
        let left = origin.x - window.left;
        let top = origin.y - window.top;
        let region = CreateRectRgn(
            left,
            top,
            left + client.right - client.left,
            top + client.bottom - client.top,
        );
        if region.is_null() {
            return;
        }
        if SetWindowRgn(hwnd, region, 1) == 0 {
            DeleteObject(region);
        }
    }

    fn find_terminal_window(stop: &AtomicBool) -> Option<*mut c_void> {
        for _ in 0..100 {
            if stop.load(Ordering::Relaxed) {
                return None;
            }
            unsafe {
                let hwnd = GetForegroundWindow();
                let mut class = [0u16; 128];
                let len = GetClassNameW(hwnd, class.as_mut_ptr(), class.len() as i32);
                if len > 0 {
                    let class = String::from_utf16_lossy(&class[..len as usize]);
                    if class.contains("CASCADIA") {
                        return Some(hwnd);
                    }
                }
            }
            thread::sleep(Duration::from_millis(50));
        }
        None
    }
}

#[cfg(not(windows))]
mod native_drag {
    pub struct Watcher;

    impl Watcher {
        pub fn start() -> Self {
            Self
        }
    }

    pub fn begin_resize() {}
    pub fn end_resize() {}
}

/// 刷新一轮：成功的落盘，失败的用上一轮数据回填（错误信息保留）。
async fn refresh_snapshot(creds: &CredentialSet, only: Option<ProviderId>) -> Snapshot {
    let mut snapshot = fetch::fetch_all(creds, only, &[]).await;
    cache::save(&snapshot);
    cache::apply(&mut snapshot);
    snapshot
}

pub async fn run(creds: CredentialSet, only: Option<ProviderId>) -> Result<()> {
    run_with(creds, only, false).await
}

/// 演示模式：固定模拟数据，不读凭据、不联网、不自动刷新（供截图与预览）。
pub async fn run_demo() -> Result<()> {
    run_with(CredentialSet::default(), None, true).await
}

async fn run_with(creds: CredentialSet, only: Option<ProviderId>, demo: bool) -> Result<()> {
    let original_size = crossterm::terminal::size().ok();
    enable_raw_mode()?;
    let mut stdout = stdout();
    execute!(stdout, SetTitle("编程额度"), EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;

    resize_terminal(&mut terminal, TUI_COLUMNS, TUI_ROWS);
    let _drag_watcher = native_drag::Watcher::start();
    let mut snapshot: Option<Snapshot> = demo.then(demo_snapshot);
    // 内容行数（未截断）与滚动偏移：视口装不下全部卡片时用 ↑↓ 翻到最后一张。
    let mut content_rows: u16 = snapshot
        .as_ref()
        .map(|snap| body_paragraph(snap, TUI_COLUMNS).line_count(TUI_COLUMNS) as u16)
        .unwrap_or(0);
    let mut scroll: u16 = 0;
    let mut last_rows = TUI_ROWS;
    if demo {
        // 演示模式没有刷新回调，启动时直接按内容收一次高度。
        let rows = needed_rows(snapshot.as_ref());
        if rows != last_rows {
            last_rows = rows;
            resize_terminal(&mut terminal, TUI_COLUMNS, rows);
        }
    }
    let mut inflight: Option<JoinHandle<Snapshot>> =
        (!demo).then(|| spawn_refresh(creds.clone(), only));
    let mut last_refresh = Instant::now();
    let auto = Duration::from_secs(120);
    let mut spin_frame: usize = 0;

    let result = loop {
        if inflight.as_ref().is_some_and(|handle| handle.is_finished()) {
            if let Some(handle) = inflight.take() {
                if let Ok(snap) = handle.await {
                    content_rows =
                        body_paragraph(&snap, TUI_COLUMNS).line_count(TUI_COLUMNS) as u16;
                    // 内容变短时收回超出的滚动，末张卡片始终可达。
                    scroll = scroll.min(max_scroll(content_rows));
                    snapshot = Some(snap);
                    last_refresh = Instant::now();
                    let rows = needed_rows(snapshot.as_ref());
                    if rows != last_rows {
                        last_rows = rows;
                        resize_terminal(&mut terminal, TUI_COLUMNS, rows);
                    }
                }
            }
        }

        let loading = inflight.is_some();
        terminal.draw(|frame| {
            draw(
                frame,
                snapshot.as_ref(),
                loading.then_some(spin_frame),
                scroll,
            )
        })?;
        if loading {
            spin_frame = spin_frame.wrapping_add(1);
        }

        let poll = if loading {
            Duration::from_millis(80)
        } else {
            Duration::from_millis(200)
        };
        if event::poll(poll)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break Ok(()),
                    KeyCode::Char('r') if inflight.is_none() && !demo => {
                        inflight = Some(spawn_refresh(creds.clone(), only));
                    }
                    KeyCode::Up | KeyCode::Char('k') => scroll = scroll.saturating_sub(1),
                    KeyCode::Down | KeyCode::Char('j') => {
                        scroll = scroll.saturating_add(1).min(max_scroll(content_rows));
                    }
                    KeyCode::PageUp => {
                        scroll = scroll.saturating_sub(body_viewport_rows().max(1));
                    }
                    KeyCode::PageDown => {
                        let page = body_viewport_rows().max(1);
                        scroll = scroll.saturating_add(page).min(max_scroll(content_rows));
                    }
                    KeyCode::Home => scroll = 0,
                    KeyCode::End => scroll = max_scroll(content_rows),
                    _ => {}
                },
                _ => {}
            }
        }
        if !demo && inflight.is_none() && last_refresh.elapsed() >= auto {
            inflight = Some(spawn_refresh(creds.clone(), only));
        }
    };
    drop(_drag_watcher);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    if let Some((columns, rows)) = original_size {
        let _ = execute!(terminal.backend_mut(), SetSize(columns, rows));
    }
    result
}

fn spawn_refresh(fallback: CredentialSet, only: Option<ProviderId>) -> JoinHandle<Snapshot> {
    tokio::spawn(async move {
        // 每轮重读 omp 凭据库：omp 里 logout 的平台下一轮刷新即变为
        // 「未找到凭据」并从界面隐藏；库读取失败时退回启动时的凭据。
        let creds = credentials::load().unwrap_or(fallback);
        refresh_snapshot(&creds, only).await
    })
}

fn days(offset: i64) -> Option<chrono::DateTime<chrono::Utc>> {
    Some(chrono::Utc::now() + chrono::Duration::days(offset))
}

fn hours(offset: i64) -> Option<chrono::DateTime<chrono::Utc>> {
    Some(chrono::Utc::now() + chrono::Duration::hours(offset))
}

fn now() -> chrono::DateTime<chrono::Utc> {
    chrono::Utc::now()
}

/// 固定的演示数据：身份一律为 example.com / demo 占位，用量覆盖绿黄红三档。
/// Antigravity 演示 Gemini 和平台内 Claude/GPT 共享额度的四个窗口。
fn demo_snapshot() -> Snapshot {
    let mut codex = ProviderReport::ok(
        ProviderId::Codex,
        "OpenAI Codex",
        Some("demo@example.com".into()),
        Some("pro".into()),
        vec![QuotaWindow::from_used_percent(
            "7d",
            "7 days",
            76.0,
            days(4),
        )],
    );
    codex.windows[0].resets_left = Some(2);
    codex.resets_left = Some(2);
    codex.usage = Some(vec![
        UsageRow {
            id: "1d".into(),
            label: "last 1 day".into(),
            source: "local".into(),
            total_tokens: 1_250_000,
            group: None,
            models: Vec::new(),
        },
        UsageRow {
            id: "7d".into(),
            label: "last 7 days".into(),
            source: "local".into(),
            total_tokens: 12_340_000,
            group: None,
            models: Vec::new(),
        },
        UsageRow {
            id: "all".into(),
            label: "local total".into(),
            source: "local".into(),
            total_tokens: 89_230_000,
            group: None,
            models: Vec::new(),
        },
    ]);
    let reports = vec![
        codex,
        ProviderReport::ok(
            ProviderId::Grok,
            "xAI Grok",
            Some("demo@example.com".into()),
            None,
            vec![QuotaWindow::from_used_percent(
                "weekly",
                "Weekly credits",
                8.0,
                days(3),
            )],
        ),
        ProviderReport::ok(
            ProviderId::Glm,
            "Zhipu Coding Plan",
            None,
            Some("Coding Plan MAX".into()),
            vec![
                QuotaWindow::from_used_percent("5h", "5h window", 24.0, hours(2)),
                QuotaWindow::from_used_percent("week", "Weekly", 55.0, days(1)),
                QuotaWindow::from_used_percent("mcp", "MCP / tools", 12.0, days(4)),
            ],
        ),
        ProviderReport::ok(
            ProviderId::Kimi,
            "Kimi Code",
            Some("demo-user-id".into()),
            None,
            vec![
                QuotaWindow::from_used_percent("5h", "5h limit", 15.0, hours(3)),
                QuotaWindow::from_used_percent("total", "Total quota", 48.0, days(1)),
            ],
        ),
        ProviderReport::ok(
            ProviderId::Cursor,
            "Cursor",
            Some("demo".into()),
            None,
            vec![
                QuotaWindow::from_used_percent("api", "API / named models", 100.0, days(2)),
                QuotaWindow::from_used_percent("auto", "Auto models", 30.0, days(2)),
                QuotaWindow::from_used_percent("total", "Included total", 44.0, days(2)),
            ],
        ),
        ProviderReport::ok(
            ProviderId::Antigravity,
            "Google Antigravity",
            Some("demo@example.com".into()),
            None,
            vec![
                QuotaWindow::from_used_percent(
                    "antigravity-gemini-5h",
                    "Gemini · 5h window",
                    18.0,
                    hours(3),
                ),
                QuotaWindow::from_used_percent(
                    "antigravity-gemini-weekly",
                    "Gemini · Weekly",
                    74.0,
                    days(2),
                ),
                QuotaWindow::from_used_percent(
                    "antigravity-3p-5h",
                    "Claude & GPT (shared) · 5h window",
                    93.0,
                    hours(1),
                ),
                QuotaWindow::from_used_percent(
                    "antigravity-3p-weekly",
                    "Claude & GPT (shared) · Weekly",
                    45.0,
                    days(5),
                ),
            ],
        ),
    ];
    Snapshot {
        fetched_at: now() - chrono::Duration::minutes(1),
        reports,
    }
}

fn draw(frame: &mut Frame, snapshot: Option<&Snapshot>, spin: Option<usize>, scroll: u16) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(frame.area());
    let mut right_spans: Vec<Span> = Vec::new();
    if let Some(snapshot) = snapshot {
        right_spans.push(Span::styled(
            ago_cn(snapshot.fetched_at),
            Style::default().fg(FG_MUTED),
        ));
    }
    if let Some(frame_i) = spin {
        if !right_spans.is_empty() {
            right_spans.push(Span::raw(" "));
        }
        right_spans.push(Span::styled(
            SPINNER[frame_i % SPINNER.len()].to_string(),
            Style::default().fg(FG_ACCENT),
        ));
    }
    let title_width = chunks[1].width as usize;
    let right_width: usize = right_spans
        .iter()
        .map(|s| display_width(s.content.as_ref()))
        .sum();
    let used = TUI_LEFT_GUTTER + display_width("编程额度") + right_width;
    let pad = title_width.saturating_sub(used + 2);
    let mut title = vec![
        Span::raw(" ".repeat(TUI_LEFT_GUTTER)),
        Span::styled("编程额度", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" ".repeat(pad)),
    ];
    title.extend(right_spans);
    frame.render_widget(Paragraph::new(Line::from(title)), chunks[1]);

    let mut overflow = false;
    if let Some(snapshot) = snapshot {
        let paragraph = body_paragraph(snapshot, chunks[3].width);
        let max = (paragraph.line_count(chunks[3].width) as u16).saturating_sub(chunks[3].height);
        overflow = max > 0;
        frame.render_widget(paragraph.scroll((scroll.min(max), 0)), chunks[3]);
    }

    let mut footer = vec![
        Span::raw(" ".repeat(TUI_LEFT_GUTTER)),
        Span::styled("[Q]", Style::default().fg(FG_ACCENT)),
        Span::styled(" 关闭  ", Style::default().add_modifier(Modifier::DIM)),
        Span::styled("[R]", Style::default().fg(FG_ACCENT)),
        Span::styled(
            " 刷新  每 2 分钟自动刷新",
            Style::default().add_modifier(Modifier::DIM),
        ),
    ];
    if overflow {
        footer.push(Span::styled(
            " ",
            Style::default().add_modifier(Modifier::DIM),
        ));
        footer.push(Span::styled("[↑↓]", Style::default().fg(FG_ACCENT)));
        footer.push(Span::styled(
            " 滚动",
            Style::default().add_modifier(Modifier::DIM),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(footer)), chunks[5]);
}

fn body_lines(snapshot: &Snapshot, width: usize) -> Vec<Line<'static>> {
    let extra_width = snapshot
        .reports
        .iter()
        .flat_map(|report| {
            report
                .windows
                .iter()
                .map(move |window| display_width(&remaining_extra(report, window)))
        })
        .max()
        .unwrap_or(0);
    let bar_width = width
        .saturating_sub(TUI_LEFT_GUTTER + 2 + extra_width)
        .max(BAR_MIN_WIDTH);
    let mut lines = Vec::new();
    for (index, report) in snapshot
        .reports
        .iter()
        .filter(|report| !report.is_missing())
        .enumerate()
    {
        if index > 0 {
            lines.push(Line::default());
        }
        lines.extend(report_lines(report, width, bar_width));
    }
    lines
}

fn needed_rows(snapshot: Option<&Snapshot>) -> u16 {
    let content = snapshot.map_or(1, |snap| {
        body_paragraph(snap, TUI_COLUMNS).line_count(TUI_COLUMNS)
    });
    (content as u16 + CHROME_ROWS).clamp(TUI_MIN_ROWS, TUI_MAX_ROWS)
}

/// 绘制、量高和滚动使用同一个自动换行口径，长错误行也不会截断末张卡片。
fn body_paragraph(snapshot: &Snapshot, columns: u16) -> Paragraph<'static> {
    let width = usize::from(columns).saturating_sub(TUI_LEFT_GUTTER);
    Paragraph::new(body_lines(snapshot, width)).wrap(Wrap { trim: false })
}

/// 当前终端里正文区看得见的高度：布局里除正文外还有 CHROME_ROWS 行固定装饰。
fn body_viewport_rows() -> u16 {
    crossterm::terminal::size()
        .map(|(_, rows)| rows.saturating_sub(CHROME_ROWS))
        .unwrap_or(TUI_ROWS.saturating_sub(CHROME_ROWS))
}

/// 内容超出视口时允许滚动的最大行数（滚到末张卡片底部）。
fn max_scroll(content_rows: u16) -> u16 {
    content_rows.saturating_sub(body_viewport_rows())
}

fn report_lines(report: &ProviderReport, width: usize, bar_width: usize) -> Vec<Line<'static>> {
    let stale = report.error.is_some() && !report.windows.is_empty();
    let worst = report
        .windows
        .iter()
        .map(|window| window.used_fraction)
        .fold(0.0_f64, f64::max);
    let dot_color = if stale { FG_MUTED } else { status_color(worst) };

    let title = report_title(report);
    let identity = report.identity.clone().unwrap_or_default();
    let gap = usize::from(!identity.is_empty()) * 2;
    let pad = width.saturating_sub(
        TUI_LEFT_GUTTER + 2 + display_width(&title) + display_width(&identity) + gap,
    );
    let mut lines = vec![Line::from(vec![
        Span::raw(" ".repeat(TUI_LEFT_GUTTER)),
        Span::styled("●", Style::default().fg(dot_color)),
        Span::raw(" "),
        Span::styled(title, Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" ".repeat(pad + gap)),
        Span::styled(identity, Style::default().add_modifier(Modifier::DIM)),
    ])];
    if report.windows.iter().all(|w| w.resets_left.is_none()) {
        if let Some(resets) = report.resets_left {
            lines.push(Line::from(vec![
                Span::raw(" ".repeat(TUI_LEFT_GUTTER)),
                Span::styled(
                    format!("重置卡：剩余 {resets} 次"),
                    Style::default().fg(if stale { FG_MUTED } else { FG_ACCENT }),
                ),
            ]));
        }
    }
    if report.windows.is_empty() {
        if let Some(credits) = report.credit_balance {
            lines.push(Line::from(vec![
                Span::raw(" ".repeat(TUI_LEFT_GUTTER)),
                Span::styled(
                    credit_balance_cn(credits),
                    Style::default().fg(if stale { FG_MUTED } else { FG_ACCENT }),
                ),
            ]));
        }
    }

    if let Some(text) = error_line(report) {
        lines.push(Line::from(Span::styled(
            format!("{}{text}", " ".repeat(TUI_LEFT_GUTTER)),
            Style::default().fg(FG_ERR),
        )));
        if !stale {
            return lines;
        }
    }
    if report.windows.is_empty() {
        lines.push(Line::from(format!(
            "{}暂无额度数据",
            " ".repeat(TUI_LEFT_GUTTER)
        )));
        return lines;
    }

    for (idx, window) in report.windows.iter().enumerate() {
        let label = label_cn(&window.label);
        let reset = reset_text(window);
        let label_pad =
            width.saturating_sub(TUI_LEFT_GUTTER + display_width(&label) + display_width(&reset));
        lines.push(Line::from(vec![
            Span::raw(format!("{}{label}", " ".repeat(TUI_LEFT_GUTTER))),
            Span::raw(" ".repeat(label_pad)),
            Span::styled(reset, Style::default().fg(FG_MUTED)),
        ]));

        let remaining = (1.0 - window.used_fraction).clamp(0.0, 1.0);
        let extra = remaining_extra(report, window);
        let color = if stale {
            FG_MUTED
        } else {
            status_color(window.used_fraction)
        };
        let used_width = TUI_LEFT_GUTTER + bar_width + 2 + display_width(&extra);
        let pad = width.saturating_sub(used_width);
        let (filled, track) = bar_parts(remaining, bar_width);
        lines.push(Line::from(vec![
            Span::raw(" ".repeat(TUI_LEFT_GUTTER)),
            Span::styled(filled, Style::default().fg(color)),
            Span::styled(track, Style::default().fg(FG_MUTED)),
            Span::raw("  "),
            Span::styled(
                extra,
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" ".repeat(pad)),
        ]));
        if let Some(resets) = window.resets_left {
            lines.push(Line::from(vec![
                Span::raw(" ".repeat(TUI_LEFT_GUTTER)),
                Span::styled(
                    format!("重置卡：剩余 {resets} 次"),
                    Style::default().fg(if stale { FG_MUTED } else { FG_ACCENT }),
                ),
            ]));
        }
        if idx == 0 {
            if let Some(credits) = report.credit_balance {
                lines.push(Line::from(vec![
                    Span::raw(" ".repeat(TUI_LEFT_GUTTER)),
                    Span::styled(
                        credit_balance_cn(credits),
                        Style::default().fg(if stale { FG_MUTED } else { FG_ACCENT }),
                    ),
                ]));
            }
        }
    }
    // 用量行：标签与周期 DIM，数值用清爽的冰蓝灰区分于额度条与重置卡
    let blocks = usage_blocks(report);
    let tag_style = Style::default().fg(FG_MUTED);
    let period_style = Style::default().fg(FG_MUTED);
    let dot_style = Style::default().fg(FG_MUTED);
    let val_style = Style::default().fg(if stale {
        FG_MUTED
    } else {
        Color::Rgb(160, 205, 235)
    });
    for block in blocks {
        let mut spans = vec![
            Span::raw(" ".repeat(TUI_LEFT_GUTTER)),
            Span::styled(format!("{}：", block.tag), tag_style),
        ];
        for (i, item) in block.items.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled("  ·  ", dot_style));
            }
            spans.push(Span::styled(format!("{} ", item.period), period_style));
            spans.push(Span::styled(item.value.clone(), val_style));
        }
        lines.push(Line::from(spans));
    }
    lines
}

fn report_title(report: &ProviderReport) -> String {
    let mut title = title_cn(&report.title).to_string();
    if let Some(plan) = &report.plan {
        title.push_str(" · ");
        title.push_str(plan);
    }
    title
}

fn error_cn(error: &str) -> String {
    match error {
        "no credential found" => "未找到凭据".into(),
        "invalid quota payload" => "额度响应格式无效".into(),
        "no endpoint responded" => "额度接口无响应".into(),
        other if other.starts_with("missing token") => {
            other.replacen("missing token", "缺少访问令牌", 1)
        }
        other => other.to_string(),
    }
}

fn reset_text(window: &QuotaWindow) -> String {
    match window.reset_at {
        Some(when) if when <= Utc::now() => "即将重置".into(),
        Some(when) => format!("{}后重置", compact_until_cn(when)),
        None => String::new(),
    }
}

fn remaining_extra(report: &ProviderReport, window: &QuotaWindow) -> String {
    let remaining = (1.0 - window.used_fraction).clamp(0.0, 1.0);
    if report.provider == ProviderId::Kimi {
        format!("剩余 {:>3.0}%", remaining * 100.0)
    } else {
        match (window.used, window.limit) {
            (Some(used), Some(limit)) => {
                format!("剩余 {:.0}/{limit:.0}", (limit - used).max(0.0))
            }
            _ => format!("剩余 {:>3.0}%", remaining * 100.0),
        }
    }
}

fn error_line(report: &ProviderReport) -> Option<String> {
    let error = report.error.as_deref()?;
    let stale = !report.windows.is_empty();
    Some(if stale {
        format!(
            "更新失败，显示{}数据：{}",
            ago_cn(report.fetched_at),
            error_cn(error)
        )
    } else {
        format!("错误：{}", error_cn(error))
    })
}

fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

fn resize_terminal(terminal: &mut AppTerminal, columns: u16, rows: u16) {
    native_drag::begin_resize();
    if execute!(terminal.backend_mut(), SetSize(columns, rows)).is_ok() {
        std::thread::sleep(Duration::from_millis(120));
        let _ = terminal.autoresize();
        let _ = terminal.clear();
    }
    native_drag::end_resize();
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    #[test]
    fn scrolling_reaches_last_quota_after_wrapped_refresh_error() {
        let mut snapshot = demo_snapshot();
        snapshot.reports[0].error =
            Some("quota request temporarily failed while showing cached data ".repeat(8));
        let marker = "tail-quota-marker";
        snapshot
            .reports
            .last_mut()
            .unwrap()
            .windows
            .last_mut()
            .unwrap()
            .label = marker.into();
        let mut terminal = Terminal::new(TestBackend::new(TUI_COLUMNS, 20)).unwrap();
        let visible_text = |terminal: &Terminal<TestBackend>| {
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>()
        };
        terminal
            .draw(|frame| draw(frame, Some(&snapshot), None, 0))
            .unwrap();
        assert!(!visible_text(&terminal).contains(marker));
        terminal
            .draw(|frame| draw(frame, Some(&snapshot), None, u16::MAX))
            .unwrap();
        assert!(
            visible_text(&terminal).contains(marker),
            "the last quota must remain reachable even when a cached-error row wraps"
        );
    }
}
