use crate::model::{ProviderReport, QuotaWindow, Snapshot};
use chrono::{DateTime, Datelike, Local, Utc};

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
    if let Some(expiry) = report.expires_at {
        title.push_str(&format!(" · due {}", expiry_date(expiry)));
    }
    let mut lines = vec![format!("{} — {}", title, report.identity.as_deref().unwrap_or("1 account"))];
    if let Some(resets) = report.resets_left {
        lines.push(format!("  rate-limit resets left: {resets}"));
    }
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
    let reset = window.reset_at.map(reset_with_due).unwrap_or_default();
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

pub fn title_cn(title: &str) -> &str {
    match title {
        "Zhipu Coding Plan" => "智谱 Coding Plan",
        other => other,
    }
}

pub fn label_cn(label: &str) -> String {
    match label {
        "Weekly credits" | "Weekly" => "每周额度".into(),
        "Monthly credits" => "每月额度".into(),
        "Period credits" => "周期额度".into(),
        "MCP / tools" => "MCP / 工具".into(),
        "Total quota" => "总额度".into(),
        "API / named models" => "API / 指定模型".into(),
        "Auto models" => "Auto 模型".into(),
        "Included total" => "套餐内总量".into(),
        "5h window" => "5 小时窗口".into(),
        "5h limit" => "5 小时限额".into(),
        other => {
            if let Some(days) = other.strip_suffix(" days") {
                format!("{days} 天窗口")
            } else if let Some(hours) = other.strip_suffix(" hours") {
                format!("{hours} 小时窗口")
            } else if let Some(hours) = other.strip_suffix("h limit") {
                format!("{hours} 小时限额")
            } else if let Some(days) = other.strip_suffix("d limit") {
                format!("{days} 天限额")
            } else {
                other.to_string()
            }
        }
    }
}

pub fn compact_until_cn(when: DateTime<Utc>) -> String {
    let mins = when.signed_duration_since(Utc::now()).num_minutes();
    if mins <= 0 {
        "现在".into()
    } else if mins >= 60 * 24 {
        format!("{}天", mins / (60 * 24))
    } else if mins >= 60 {
        let hours = (mins as f64 / 30.0).round() / 2.0;
        if hours.fract() == 0.0 {
            format!("{}小时", hours as i64)
        } else {
            format!("{hours:.1}小时")
        }
    } else {
        format!("{mins}分钟")
    }
}
/// 周期到期日期（本地时区；跨年带年份）。
pub fn expiry_date(when: DateTime<Utc>) -> String {
    let local = when.with_timezone(&Local);
    if local.year() == Local::now().year() {
        local.format("%m-%d").to_string()
    } else {
        local.format("%Y-%m-%d").to_string()
    }
}

pub fn reset_with_due(when: DateTime<Utc>) -> String {
    format!("{}·{}", compact_until(when), expiry_date(when))
}

pub fn reset_with_due_cn(when: DateTime<Utc>) -> String {
    format!("{}·{}", compact_until_cn(when), expiry_date(when))
}

pub fn ago_cn(when: DateTime<Utc>) -> String {
    let secs = Utc::now().signed_duration_since(when).num_seconds().max(0);
    if secs < 5 {
        "刚刚".into()
    } else if secs < 60 {
        format!("{secs} 秒前")
    } else if secs < 3600 {
        format!("{} 分钟前", secs / 60)
    } else {
        format!("{} 小时前", secs / 3600)
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
