use crate::model::{CreditBalance, ProviderReport, QuotaWindow, Snapshot};
use chrono::{DateTime, Utc};
use std::fmt::Write;

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
    let mut lines = vec![format!(
        "{} — {}",
        title,
        report.identity.as_deref().unwrap_or("1 account")
    )];
    if report.windows.iter().all(|w| w.resets_left.is_none()) {
        if let Some(resets) = report.resets_left {
            lines.push(format!("  rate-limit resets left: {resets}"));
        }
    }
    if report.windows.is_empty() {
        if let Some(credits) = report.credit_balance {
            lines.push(match credits {
                CreditBalance::Limited(balance) => format!("  credits left: {balance}"),
                CreditBalance::Unlimited => "  credits: unlimited".into(),
            });
        }
    }
    if let Some(error) = &report.error {
        lines.push(format!("  ○ {error}"));
        return lines.join("\n");
    }
    if report.windows.is_empty() {
        lines.push("  ○ no usage data".into());
        return lines.join("\n");
    }
    for (idx, window) in report.windows.iter().enumerate() {
        lines.push(format_window(window));
        if idx == 0 {
            if let Some(credits) = report.credit_balance {
                lines.push(match credits {
                    CreditBalance::Limited(balance) => format!("  credits left: {balance}"),
                    CreditBalance::Unlimited => "  credits: unlimited".into(),
                });
            }
        }
    }
    if let Some(usage) = &report.usage {
        let has_groups = usage.iter().any(|r| r.group.is_some());
        if has_groups {
            for (group_id, name) in [("gemini", "Gemini"), ("claude_gpt", "Claude&GPT")] {
                let local: Vec<String> = usage
                    .iter()
                    .filter(|r| r.source == "local" && r.group.as_deref() == Some(group_id))
                    .map(|r| {
                        let tag = match r.id.as_str() {
                            "1d" | "7d" | "30d" => &r.id,
                            _ => "total",
                        };
                        format!("{tag} {}", compact_tokens(r.total_tokens))
                    })
                    .collect();
                if !local.is_empty() {
                    lines.push(format!("  usage ({name}): {}", local.join("  ")));
                }
            }
        } else {
            let local: Vec<String> = usage
                .iter()
                .filter(|r| r.source == "local")
                .map(|r| {
                    let tag = match r.id.as_str() {
                        "1d" | "7d" | "30d" => &r.id,
                        _ => "total",
                    };
                    format!("{tag} {}", compact_tokens(r.total_tokens))
                })
                .collect();
            if !local.is_empty() {
                lines.push(format!("  usage: {}", local.join("  ")));
            }
        }
        for r in usage.iter().filter(|r| r.source == "remote") {
            lines.push(format!(
                "  usage: {} {} (server)",
                r.id,
                compact_tokens(r.total_tokens)
            ));
        }
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
    let mut out = format!(
        "  {}{}{}\n  {}  {}",
        window.label,
        " ".repeat(pad),
        reset,
        bar(remaining, BAR_WIDTH),
        extra,
    );
    if let Some(resets) = window.resets_left {
        write!(out, "\n  reset cards left: {resets}").expect("writing to a String cannot fail");
    }
    out
}

pub fn bar(fraction: f64, width: usize) -> String {
    let (filled, track) = bar_parts(fraction, width);
    format!("{filled}{track}")
}

pub fn bar_parts(fraction: f64, width: usize) -> (String, String) {
    let filled = ((fraction.clamp(0.0, 1.0) * width as f64).round() as usize).min(width);
    ("█".repeat(filled), "░".repeat(width - filled))
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

pub fn credit_balance_cn(credits: CreditBalance) -> String {
    match credits {
        CreditBalance::Limited(balance) => format!("积分：剩余 {balance}"),
        CreditBalance::Unlimited => "积分：无限".into(),
    }
}

/// token 数的紧凑英文（快照 / JSON 侧文案）：1.55B / 456.1M / 60.7M / 892K / 892。
pub fn compact_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000_000 {
        format!("{:.2}B", tokens as f64 / 1e9)
    } else if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1e6)
    } else if tokens >= 1_000 {
        format!("{:.1}K", tokens as f64 / 1e3)
    } else {
        tokens.to_string()
    }
}

/// token 数的紧凑中文（界面文案）：15.53亿 / 6069万 / 125万 / 892。
/// 去除冗余的 .0 尾随和小数，去除数字与单位间的空格。
pub fn compact_tokens_cn(tokens: u64) -> String {
    if tokens >= 100_000_000 {
        let val = tokens as f64 / 1e8;
        let mut s = format!("{val:.2}");
        if s.ends_with('0') {
            s.pop();
            if s.ends_with('0') {
                s.pop();
                if s.ends_with('.') {
                    s.pop();
                }
            }
        }
        format!("{s}亿")
    } else if tokens >= 10_000 {
        let val = tokens as f64 / 1e4;
        if val >= 1000.0 {
            format!("{:.0}万", val.round())
        } else {
            let mut s = format!("{val:.1}");
            if s.ends_with(".0") {
                s.truncate(s.len() - 2);
            }
            format!("{s}万")
        }
    } else {
        tokens.to_string()
    }
}

/// 结构化的用量显示块：每个块包含分类标签（"用量" / "官方"）
/// 和按周期划分的 (周期标签, 格式化数值) 列表。
#[derive(Debug, Clone, PartialEq)]
pub struct UsageBlock {
    pub tag: &'static str,
    pub items: Vec<(&'static str, String)>,
}

/// 解析报表中的用量数据，生成结构化块。
/// 本机日志只保留 1天 / 7天 / 累计 三档（跳过信息量重叠的近 30 天）。
pub fn usage_blocks(report: &ProviderReport) -> Vec<UsageBlock> {
    let Some(rows) = &report.usage else {
        return Vec::new();
    };
    let mut blocks = Vec::new();
    let has_groups = rows.iter().any(|r| r.group.is_some());
    if has_groups {
        for (group_id, tag) in [("gemini", "Gemini"), ("claude_gpt", "Claude&GPT")] {
            let mut items = Vec::new();
            for row in rows.iter().filter(|r| {
                r.source == "local" && r.group.as_deref() == Some(group_id) && r.id != "30d"
            }) {
                let label = match row.id.as_str() {
                    "1d" => "1天",
                    "7d" => "7天",
                    _ => "累计",
                };
                items.push((label, compact_tokens_cn(row.total_tokens)));
            }
            if !items.is_empty() {
                blocks.push(UsageBlock { tag, items });
            }
        }
    } else {
        let mut local_items = Vec::new();
        for row in rows
            .iter()
            .filter(|row| row.source == "local" && row.id != "30d")
        {
            let label = match row.id.as_str() {
                "1d" => "1天",
                "7d" => "7天",
                _ => "累计",
            };
            local_items.push((label, compact_tokens_cn(row.total_tokens)));
        }
        if !local_items.is_empty() {
            blocks.push(UsageBlock {
                tag: "用量",
                items: local_items,
            });
        }
    }
    for row in rows.iter().filter(|row| row.source == "remote") {
        let label = match row.id.as_str() {
            "30d" => "30天",
            _ => "30天",
        };
        blocks.push(UsageBlock {
            tag: "官方",
            items: vec![(label, compact_tokens_cn(row.total_tokens))],
        });
    }
    blocks
}

/// 卡片底部的 token 用量完整单行文本（用于尺寸测算与平铺展示）。
pub fn usage_lines_cn(report: &ProviderReport) -> Vec<String> {
    usage_blocks(report)
        .into_iter()
        .map(|block| {
            let items: Vec<String> = block
                .items
                .into_iter()
                .map(|(p, v)| format!("{p} {v}"))
                .collect();
            format!("{}：{}", block.tag, items.join("  ·  "))
        })
        .collect()
}

pub fn label_cn(label: &str) -> String {
    match label {
        "Weekly credits" | "Weekly" => "每周额度".into(),
        "Daily" => "1 天额度".into(),
        "Current limit" => "当前额度".into(),
        "Monthly credits" => "每月额度".into(),
        "Period credits" => "周期额度".into(),
        "MCP / tools" => "MCP / 工具".into(),
        "Total quota" => "总额度".into(),
        "API / named models" => "API / 指定模型".into(),
        "Auto models" => "Auto 模型".into(),
        "Included total" => "套餐内总量".into(),
        "5h window" => "5 小时窗口".into(),
        "5h limit" => "5 小时限额".into(),
        "Gemini · 5h window" => "Gemini · 5 小时窗口".into(),
        "Gemini · Weekly" => "Gemini · 每周额度".into(),
        "Claude & GPT (shared) · 5h window" => "Claude&GPT 共享 · 5 小时窗口".into(),
        "Claude & GPT (shared) · Weekly" => "Claude&GPT 共享 · 每周额度".into(),
        other => {
            if let Some(days) = other.strip_suffix(" days") {
                format!("{days} 天窗口")
            } else if let Some(hours) = other.strip_suffix(" hours") {
                format!("{hours} 小时窗口")
            } else if let Some(hours) = other.strip_suffix("h limit") {
                format!("{hours} 小时限额")
            } else if let Some(days) = other.strip_suffix("d limit") {
                format!("{days} 天限额")
            } else if let Some(model) = other.strip_prefix("Weekly · ") {
                format!("每周额度 · {model}")
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
#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ProviderId, UsageRow};

    #[test]
    fn compact_tokens_english_and_chinese() {
        assert_eq!(compact_tokens(892), "892");
        assert_eq!(compact_tokens(1_250), "1.2K");
        assert_eq!(compact_tokens(1_250_000), "1.2M");
        assert_eq!(compact_tokens(60_688_000), "60.7M");
        assert_eq!(compact_tokens(456_146_001), "456.1M");
        assert_eq!(compact_tokens(1_552_626_856), "1.55B");

        assert_eq!(compact_tokens_cn(892), "892");
        assert_eq!(compact_tokens_cn(10_000), "1万");
        assert_eq!(compact_tokens_cn(1_250_000), "125万");
        assert_eq!(compact_tokens_cn(12_340_000), "1234万");
        assert_eq!(compact_tokens_cn(60_688_000), "6069万");
        assert_eq!(compact_tokens_cn(100_000_000), "1亿");
        assert_eq!(compact_tokens_cn(456_146_001), "4.56亿");
        assert_eq!(compact_tokens_cn(1_552_626_856), "15.53亿");
        assert_eq!(compact_tokens_cn(3_166_652_059), "31.67亿");
    }

    #[test]
    fn usage_blocks_structure() {
        let mut report = ProviderReport::ok(ProviderId::Codex, "Codex", None, None, vec![]);
        report.usage = Some(vec![
            UsageRow {
                id: "1d".into(),
                label: "last 1 day".into(),
                source: "local".into(),
                total_tokens: 1_250_000,
                group: None,
                models: vec![],
            },
            UsageRow {
                id: "7d".into(),
                label: "last 7 days".into(),
                source: "local".into(),
                total_tokens: 12_340_000,
                group: None,
                models: vec![],
            },
            UsageRow {
                id: "30d".into(),
                label: "last 30 days".into(),
                source: "local".into(),
                total_tokens: 50_000_000,
                group: None,
                models: vec![],
            },
            UsageRow {
                id: "all".into(),
                label: "local total".into(),
                source: "local".into(),
                total_tokens: 89_230_000,
                group: None,
                models: vec![],
            },
            UsageRow {
                id: "30d".into(),
                label: "last 30 days (server)".into(),
                source: "remote".into(),
                total_tokens: 3_100_000_000,
                group: None,
                models: vec![],
            },
        ]);

        let blocks = usage_blocks(&report);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].tag, "用量");
        assert_eq!(
            blocks[0].items,
            vec![
                ("1天", "125万".into()),
                ("7天", "1234万".into()),
                ("累计", "8923万".into()),
            ]
        );
        assert_eq!(blocks[1].tag, "官方");
        assert_eq!(blocks[1].items, vec![("30天", "31亿".into())]);

        let lines = usage_lines_cn(&report);
        assert_eq!(lines[0], "用量：1天 125万  ·  7天 1234万  ·  累计 8923万");
        assert_eq!(lines[1], "官方：30天 31亿");
    }

    #[test]
    fn usage_blocks_antigravity_split() {
        let mut report =
            ProviderReport::ok(ProviderId::Antigravity, "Antigravity", None, None, vec![]);
        report.usage = Some(vec![
            UsageRow {
                id: "1d".into(),
                label: "last 1 day".into(),
                source: "local".into(),
                total_tokens: 17_210_000,
                group: Some("gemini".into()),
                models: vec![],
            },
            UsageRow {
                id: "all".into(),
                label: "local total".into(),
                source: "local".into(),
                total_tokens: 66_750_000,
                group: Some("gemini".into()),
                models: vec![],
            },
            UsageRow {
                id: "1d".into(),
                label: "last 1 day".into(),
                source: "local".into(),
                total_tokens: 4_980_000,
                group: Some("claude_gpt".into()),
                models: vec![],
            },
            UsageRow {
                id: "all".into(),
                label: "local total".into(),
                source: "local".into(),
                total_tokens: 4_980_000,
                group: Some("claude_gpt".into()),
                models: vec![],
            },
        ]);

        let blocks = usage_blocks(&report);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].tag, "Gemini");
        assert_eq!(
            blocks[0].items,
            vec![("1天", "1721万".into()), ("累计", "6675万".into())]
        );
        assert_eq!(blocks[1].tag, "Claude&GPT");
        assert_eq!(
            blocks[1].items,
            vec![("1天", "498万".into()), ("累计", "498万".into())]
        );

        let lines = usage_lines_cn(&report);
        assert_eq!(lines[0], "Gemini：1天 1721万  ·  累计 6675万");
        assert_eq!(lines[1], "Claude&GPT：1天 498万  ·  累计 498万");
    }
}
