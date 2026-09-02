use coding_quota::cache;
use coding_quota::credentials::CredentialSet;
use coding_quota::fetch;
use coding_quota::model::{ProviderId, ProviderReport, Snapshot};
use coding_quota::render::{
    ago_cn, bar, compact_until_cn, label_cn, status_color, title_cn,
};
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen, SetSize,
    SetTitle,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use std::io::{stdout, Stdout};
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthStr;

type AppTerminal = Terminal<CrosstermBackend<Stdout>>;
const BAR_WIDTH: usize = 22;
pub const TUI_COLUMNS: u16 = 48;
pub const TUI_LEFT_GUTTER: usize = 2;
pub const TUI_ROWS: u16 = 34;

#[cfg(windows)]
mod native_drag {
    use core::ffi::c_void;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    use std::thread::{self, JoinHandle};
    use std::time::Duration;

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
            SetThreadDpiAwarenessContext(
                DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2 as *mut c_void,
            );
        }
        let Some(hwnd) = find_terminal_window(&stop) else {
            return;
        };
        thread::sleep(Duration::from_millis(300));
        lock_window_size(hwnd);
        let (fixed_width, fixed_height) = unsafe {
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
                    if width != fixed_width || height != fixed_height {
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
            DwmSetWindowAttribute(
                hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                &DWMWCP_DONOTROUND,
                4,
            );
            let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
            SetWindowLongPtrW(hwnd, GWL_STYLE, style & !WS_OVERLAPPEDWINDOW);
            const DWMWA_NCRENDERING_POLICY: u32 = 2;
            const DWMNCRP_DISABLED: u32 = 1;
            DwmSetWindowAttribute(
                hwnd,
                DWMWA_NCRENDERING_POLICY,
                &DWMNCRP_DISABLED,
                4,
            );
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
                SWP_NOSIZE
                    | SWP_NOMOVE
                    | SWP_NOZORDER
                    | SWP_NOACTIVATE
                    | SWP_FRAMECHANGED,
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
}

/// 刷新一轮：成功的落盘，失败的用上一轮数据回填（错误信息保留）。
async fn refresh_snapshot(creds: &CredentialSet, only: Option<ProviderId>) -> Snapshot {
    let mut snapshot = fetch::fetch_all(creds, only).await;
    cache::save(&snapshot);
    cache::apply(&mut snapshot);
    snapshot
}

pub async fn run(creds: CredentialSet, only: Option<ProviderId>) -> Result<()> {
    let original_size = crossterm::terminal::size().ok();
    enable_raw_mode()?;
    let mut stdout = stdout();
    execute!(stdout, SetTitle("编程额度"), EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;

    let mut snapshot = refresh_snapshot(&creds, only).await;
    resize_terminal(&mut terminal);
    let _drag_watcher = native_drag::Watcher::start();
    let mut last_refresh = Instant::now();
    let auto = Duration::from_secs(120);
    let mut loading = false;

    let result = loop {
        terminal.draw(|frame| draw(frame, &snapshot, loading))?;
        if event::poll(Duration::from_millis(200))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break Ok(()),
                    KeyCode::Char('r') => {
                        loading = true;
                        terminal.draw(|frame| draw(frame, &snapshot, loading))?;
                        snapshot = refresh_snapshot(&creds, only).await;
                        last_refresh = Instant::now();
                    }
                    _ => {}
                },
                _ => {}
            }
        }
        if last_refresh.elapsed() >= auto {
            loading = true;
            terminal.draw(|frame| draw(frame, &snapshot, loading))?;
            snapshot = refresh_snapshot(&creds, only).await;
            last_refresh = Instant::now();
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

fn draw(frame: &mut Frame, snapshot: &Snapshot, loading: bool) {
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

    let title = if loading {
        "编程额度 · 正在刷新…".to_string()
    } else {
        format!("编程额度 · 更新于 {}", ago_cn(snapshot.fetched_at))
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!("{}{}", " ".repeat(TUI_LEFT_GUTTER), title),
            Style::default().add_modifier(Modifier::BOLD),
        ))),
        chunks[1],
    );

    let width = (chunks[3].width as usize).saturating_sub(TUI_LEFT_GUTTER);
    let mut lines = Vec::new();
    for (index, report) in snapshot.reports.iter().enumerate() {
        if index > 0 {
            lines.push(Line::default());
        }
        lines.extend(report_lines(report, width));
    }
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }),
        chunks[3],
    );

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!("{}[Q] 关闭  [R] 刷新  每 2 分钟自动刷新", " ".repeat(TUI_LEFT_GUTTER)),
            Style::default().add_modifier(Modifier::DIM),
        ))),
        chunks[5],
    );
}

fn report_lines(report: &ProviderReport, width: usize) -> Vec<Line<'static>> {
    let title = report_title(report);
    let identity = report.identity.clone().unwrap_or_default();
    let gap = usize::from(!identity.is_empty()) * 2;
    let pad = width.saturating_sub(
        TUI_LEFT_GUTTER + display_width(&title) + display_width(&identity) + gap,
    );
    let mut lines = vec![Line::from(vec![
        Span::raw(" ".repeat(TUI_LEFT_GUTTER)),
        Span::styled(title, Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" ".repeat(pad + gap)),
        Span::styled(identity, Style::default().add_modifier(Modifier::DIM)),
    ])];
    if let Some(resets) = report.resets_left {
        lines.push(Line::from(Span::styled(
            format!("{}限流重置：剩余 {resets} 次", " ".repeat(TUI_LEFT_GUTTER)),
            Style::default().add_modifier(Modifier::DIM),
        )));
    }

    // 有回填数据（windows 非空）时：报错行 + 变灰的旧额度，不再直接返回
    let stale = report.error.is_some() && !report.windows.is_empty();
    if let Some(error) = &report.error {
        let text = if stale {
            format!(
                "更新失败，显示{}数据：{}",
                ago_cn(report.fetched_at),
                error_cn(error)
            )
        } else {
            format!("错误：{}", error_cn(error))
        };
        lines.push(Line::from(Span::styled(
            format!("{}{text}", " ".repeat(TUI_LEFT_GUTTER)),
            Style::default().fg(ratatui::style::Color::Red),
        )));
        if !stale {
            return lines;
        }
    }
    if report.windows.is_empty() {
        lines.push(Line::from(format!("{}暂无额度数据", " ".repeat(TUI_LEFT_GUTTER))));
        return lines;
    }

    for window in &report.windows {
        let label = label_cn(&window.label);
        lines.push(Line::from(Span::raw(format!("{}{label}", " ".repeat(TUI_LEFT_GUTTER)))));

        let remaining = (1.0 - window.used_fraction).clamp(0.0, 1.0);
        let extra = if report.provider == ProviderId::Kimi {
            format!("剩余 {:.0}%", (remaining * 100.0).round())
        } else {
            match (window.used, window.limit) {
                (Some(used), Some(limit)) => {
                    format!("剩余 {:.0}/{limit:.0}", (limit - used).max(0.0))
                }
                _ => format!("剩余 {:.0}%", (remaining * 100.0).round()),
            }
        };
        let reset = window.reset_at.map(compact_until_cn).unwrap_or_default();
        let used_width = TUI_LEFT_GUTTER
            + BAR_WIDTH
            + 2
            + display_width(&extra)
            + display_width(&reset);
        let pad = width.saturating_sub(used_width);
        lines.push(Line::from(vec![
            Span::raw(" ".repeat(TUI_LEFT_GUTTER)),
            Span::styled(
                bar(remaining, BAR_WIDTH),
                Style::default().fg(if stale {
                    ratatui::style::Color::DarkGray
                } else {
                    status_color(window.used_fraction)
                }),
            ),
            Span::raw("  "),
            Span::styled(extra, Style::default().add_modifier(Modifier::DIM)),
            Span::raw(" ".repeat(pad)),
            Span::styled(reset, Style::default().add_modifier(Modifier::DIM)),
        ]));
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

fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}


fn resize_terminal(terminal: &mut AppTerminal) {
    if execute!(
        terminal.backend_mut(),
        SetSize(TUI_COLUMNS, TUI_ROWS)
    )
    .is_ok()
    {
        std::thread::sleep(Duration::from_millis(120));
        let _ = terminal.autoresize();
        let _ = terminal.clear();
    }
}
