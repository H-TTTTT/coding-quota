use crate::model::{ProviderReport, QuotaWindow, Snapshot};
use chrono::{DateTime, Utc};

pub fn snapshot_text(snapshot: &Snapshot) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "Coding Quota · fetched {}\n",
        ago(snapshot.fetched_at)
    ));
    for report in &snapshot.reports {
        out.push('\n');
        out.push_str(&provider_block(report));
    }
    out
}

fn provider_block(report: &ProviderReport) -> String {
    let mut title = report.title.clone();
    if let Some(plan) = &report.plan {
        title.push_str(" · ");
        title.push_str(plan);
    }
    let mut lines = vec![format!("{} — {}", title, report.identity.as_deref().unwrap_or("1 account"))];
    if let Some(error) = &report.error {
        lines.push(format!("  ○ {error}"));
        return lines.join("\n");
    }
    if report.windows.is_empty() {
        lines.push("  ○ no usage data".into());
        return lines.join("\n");
    }
    for window in &report.windows {
        lines.push(format_window(window));
    }
    lines.join("\n")
}

const BAR_WIDTH: usize = 24;
const ROW_WIDTH: usize = 38;

fn format_window(window: &QuotaWindow) -> String {
    let remaining = (1.0 - window.used_fraction).clamp(0.0, 1.0);
    let pct = (remaining * 100.0).round();
    let extra = match (window.used, window.limit) {
        (Some(used), Some(limit)) => format!("{:.0}/{limit:.0} left", (limit - used).max(0.0)),
        _ => format!("{pct:.0}% left"),
    };
    let reset = window.reset_at.map(compact_until).unwrap_or_default();
    let pad = (ROW_WIDTH - 2).saturating_sub(window.label.chars().count() + reset.chars().count());
    format!(
        "  {}{}{}\n  {}  {}",
        window.label,
        " ".repeat(pad),
        reset,
        bar(remaining, BAR_WIDTH),
        extra,
    )
}

pub fn bar(fraction: f64, width: usize) -> String {
    let filled = ((fraction.clamp(0.0, 1.0) * width as f64).round() as usize).min(width);
    format!("{}{}", "█".repeat(filled), "░".repeat(width - filled))
}

pub fn compact_until(when: DateTime<Utc>) -> String {
    let delta = when.signed_duration_since(Utc::now());
    let mins = delta.num_minutes();
    if mins <= 0 {
        return "now".into();
    }
    if mins >= 60 * 24 {
        format!("{}d", mins / (60 * 24))
    } else if mins >= 60 {
        let hours = (mins as f64 / 30.0).round() / 2.0;
        if hours.fract() == 0.0 {
            format!("{}h", hours as i64)
        } else {
            format!("{hours:.1}h")
        }
    } else {
        format!("{mins}m")
    }
}

pub fn ago(when: DateTime<Utc>) -> String {
    let secs = Utc::now().signed_duration_since(when).num_seconds().max(0);
    if secs < 5 {
        "just now".into()
    } else if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else {
        format!("{}h ago", secs / 3600)
    }
}

pub fn status_color(fraction: f64) -> ratatui::style::Color {
    use ratatui::style::Color;
    if fraction >= 0.90 {
        Color::Red
    } else if fraction >= 0.70 {
        Color::Yellow
    } else {
        Color::Green
    }
}
