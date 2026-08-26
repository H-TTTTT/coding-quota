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
const MIN_COLUMNS: usize = 44;

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
        fn GetClassNameW(hwnd: *mut c_void, class: *mut u16, max: i32) -> i32;
        fn GetCursorPos(point: *mut Point) -> i32;
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
    }

    impl Watcher {
        pub fn start() -> Self {
            let stop = Arc::new(AtomicBool::new(false));
            let thread_stop = Arc::clone(&stop);
            let thread = thread::spawn(move || watch_drag(thread_stop));
            Self {
                stop,
                thread: Some(thread),
            }
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
        let Some(hwnd) = find_terminal_window(&stop) else {
            return;
        };
        let mut was_down = false;
        let mut offset: Option<(i32, i32)> = None;

        while !stop.load(Ordering::Relaxed) {
            unsafe {
                let down = GetAsyncKeyState(0x01) < 0;
                let mut cursor = Point::default();
                let mut rect = Rect::default();
                if GetCursorPos(&mut cursor) != 0 && GetWindowRect(hwnd, &mut rect) != 0 {
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

pub async fn run(creds: CredentialSet, only: Option<ProviderId>) -> Result<()> {
    let original_size = crossterm::terminal::size().ok();
    enable_raw_mode()?;
    let mut stdout = stdout();
    execute!(stdout, SetTitle("编程额度"), EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    let _drag_watcher = native_drag::Watcher::start();

    let mut snapshot = fetch::fetch_all(&creds, only).await;
    resize_terminal(&mut terminal, &snapshot);
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
                        snapshot = fetch::fetch_all(&creds, only).await;
                        resize_terminal(&mut terminal, &snapshot);
                        last_refresh = Instant::now();
                        loading = false;
                    }
                    _ => {}
                },
                _ => {}
            }
        }
        if last_refresh.elapsed() >= auto {
            loading = true;
            terminal.draw(|frame| draw(frame, &snapshot, loading))?;
            snapshot = fetch::fetch_all(&creds, only).await;
            resize_terminal(&mut terminal, &snapshot);
            last_refresh = Instant::now();
            loading = false;
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
            Constraint::Min(1),
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
            title,
            Style::default().add_modifier(Modifier::BOLD),
        ))),
        chunks[0],
    );

    let width = chunks[1].width as usize;
    let mut lines = Vec::new();
    for (index, report) in snapshot.reports.iter().enumerate() {
        if index > 0 {
            lines.push(Line::default());
        }
        lines.extend(report_lines(report, width));
    }
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }),
        chunks[1],
    );

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "[Q] 关闭  [R] 刷新  每 2 分钟自动刷新",
            Style::default().add_modifier(Modifier::DIM),
        ))),
        chunks[2],
    );
}

fn report_lines(report: &ProviderReport, width: usize) -> Vec<Line<'static>> {
    let title = report_title(report);
    let identity = report.identity.clone().unwrap_or_default();
    let gap = usize::from(!identity.is_empty()) * 2;
    let pad = width.saturating_sub(display_width(&title) + display_width(&identity) + gap);
    let mut lines = vec![Line::from(vec![
        Span::styled(title, Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" ".repeat(pad + gap)),
        Span::styled(identity, Style::default().add_modifier(Modifier::DIM)),
    ])];

    if let Some(error) = &report.error {
        lines.push(Line::from(Span::styled(
            format!("  错误：{}", error_cn(error)),
            Style::default().fg(ratatui::style::Color::Red),
        )));
        return lines;
    }
    if report.windows.is_empty() {
        lines.push(Line::from("  暂无额度数据"));
        return lines;
    }

    for window in &report.windows {
        let label = label_cn(&window.label);
        let reset = window.reset_at.map(compact_until_cn).unwrap_or_default();
        let pad = width.saturating_sub(display_width(&label) + display_width(&reset) + 2);
        lines.push(Line::from(vec![
            Span::raw(format!("  {label}")),
            Span::raw(" ".repeat(pad)),
            Span::styled(reset, Style::default().add_modifier(Modifier::DIM)),
        ]));

        let remaining = (1.0 - window.used_fraction).clamp(0.0, 1.0);
        let extra = match (window.used, window.limit) {
            (Some(used), Some(limit)) => {
                format!("剩余 {:.0}/{limit:.0}", (limit - used).max(0.0))
            }
            _ => format!("剩余 {:.0}%", (remaining * 100.0).round()),
        };
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                bar(remaining, BAR_WIDTH),
                Style::default().fg(status_color(window.used_fraction)),
            ),
            Span::raw("  "),
            Span::styled(extra, Style::default().add_modifier(Modifier::DIM)),
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

fn report_height(report: &ProviderReport) -> usize {
    1 + if report.error.is_some() || report.windows.is_empty() {
        1
    } else {
        report.windows.len() * 2
    }
}

fn required_terminal_size(snapshot: &Snapshot) -> (u16, u16) {
    let mut columns = MIN_COLUMNS;
    for report in &snapshot.reports {
        let title = report_title(report);
        let identity = report.identity.as_deref().unwrap_or_default();
        let gap = usize::from(!identity.is_empty()) * 2;
        columns = columns.max(display_width(&title) + display_width(identity) + gap);
        for window in &report.windows {
            let label = label_cn(&window.label);
            let reset = window.reset_at.map(compact_until_cn).unwrap_or_default();
            columns = columns.max(display_width(&label) + display_width(&reset) + 2);
        }
    }

    let report_rows: usize = snapshot.reports.iter().map(report_height).sum();
    let gaps = snapshot.reports.len().saturating_sub(1);
    let rows = (report_rows + gaps + 2).max(8);
    (columns.min(u16::MAX as usize) as u16, rows.min(u16::MAX as usize) as u16)
}

fn resize_terminal(terminal: &mut AppTerminal, snapshot: &Snapshot) {
    let (columns, rows) = required_terminal_size(snapshot);
    if execute!(terminal.backend_mut(), SetSize(columns, rows)).is_ok() {
        std::thread::sleep(Duration::from_millis(120));
        let _ = terminal.autoresize();
        let _ = terminal.clear();
    }
}
