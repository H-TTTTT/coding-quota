use coding_quota::credentials::CredentialSet;
use coding_quota::fetch;
use coding_quota::model::{ProviderId, Snapshot};
use coding_quota::render::{ago, bar, compact_until, status_color};
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use std::io::stdout;
use std::time::{Duration, Instant};

pub async fn run(creds: CredentialSet, only: Option<ProviderId>) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(ratatui::backend::CrosstermBackend::new(stdout))?;

    let mut snapshot = fetch::fetch_all(&creds, only).await;
    let mut last_refresh = Instant::now();
    let auto = Duration::from_secs(120);
    let mut loading = false;

    let result = loop {
        terminal.draw(|frame| draw(frame, &snapshot, loading))?;
        let timeout = Duration::from_millis(200);
        if event::poll(timeout)? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break Ok(()),
                    KeyCode::Char('r') => {
                        loading = true;
                        terminal.draw(|frame| draw(frame, &snapshot, loading))?;
                        snapshot = fetch::fetch_all(&creds, only).await;
                        last_refresh = Instant::now();
                        loading = false;
                    }
                    _ => {}
                }
            }
        }
        if last_refresh.elapsed() >= auto {
            loading = true;
            terminal.draw(|frame| draw(frame, &snapshot, loading))?;
            snapshot = fetch::fetch_all(&creds, only).await;
            last_refresh = Instant::now();
            loading = false;
        }
    };

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    result
}

fn draw(frame: &mut Frame, snapshot: &Snapshot, loading: bool) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(1)])
        .split(area);

    let title = if loading {
        format!(" Coding Quota · refreshing… ")
    } else {
        format!(" Coding Quota · fetched {} ", ago(snapshot.fetched_at))
    };
    let block = Block::default().title(title).borders(Borders::ALL);
    let inner = block.inner(chunks[0]);
    frame.render_widget(block, chunks[0]);

    let count = snapshot.reports.len().max(1) as u16;
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(vec![Constraint::Percentage(100 / count); snapshot.reports.len().max(1)])
        .split(inner);

    for (idx, report) in snapshot.reports.iter().enumerate() {
        draw_report(frame, rows[idx], report);
    }

    let help = Paragraph::new(" q quit   r refresh   auto-refresh 2m ");
    frame.render_widget(help, chunks[1]);
}

fn draw_report(frame: &mut Frame, area: Rect, report: &coding_quota::model::ProviderReport) {
    let mut title = report.title.clone();
    if let Some(plan) = &report.plan {
        title.push_str(" · ");
        title.push_str(plan);
    }
    if let Some(identity) = &report.identity {
        title.push_str("  ");
        title.push_str(identity);
    }

    let mut lines = Vec::new();
    if let Some(error) = &report.error {
        lines.push(Line::from(Span::styled(
            format!("  {error}"),
            Style::default().fg(ratatui::style::Color::Red),
        )));
    } else {
        let width = area.width as usize;
        for window in &report.windows {
            let remaining = (1.0 - window.used_fraction).clamp(0.0, 1.0);
            let pct = (remaining * 100.0).round();
            let extra = match (window.used, window.limit) {
                (Some(used), Some(limit)) => format!("{:.0}/{limit:.0} left", (limit - used).max(0.0)),
                _ => format!("{pct:.0}% left"),
            };
            let reset = window.reset_at.map(compact_until).unwrap_or_default();
            let pad = width.saturating_sub(window.label.chars().count() + reset.chars().count() + 4);
            lines.push(Line::from(vec![
                Span::raw(format!("  {}", window.label)),
                Span::raw(" ".repeat(pad)),
                Span::styled(reset, Style::default().add_modifier(Modifier::DIM)),
            ]));
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(bar(remaining, 22), Style::default().fg(status_color(window.used_fraction))),
                Span::raw("  "),
                Span::styled(extra, Style::default().add_modifier(Modifier::DIM)),
            ]));
        }
    }

    let widget = Paragraph::new(lines)
        .block(Block::default().title(format!(" {title} ")).borders(Borders::ALL))
        .wrap(Wrap { trim: true });
    frame.render_widget(widget, area);
}
