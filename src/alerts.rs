//! 额度告警：跟踪每个平台每个窗口的「剩余百分比」，只在跨越阈值时触发。
//!
//! 规则（避免每轮刷新都刷通知，只报状态变化）：
//! - 剩余从 >10% 跌到 ≤10%：低额度预警；
//! - 剩余从 >0% 跌到 0%：已耗尽；
//! - 剩余从 <10% 回到 ≥50%：额度已重置（重置窗口滚动了）。
//!
//! 进程重启时用缓存里的上一轮报表初始化状态，不回放历史告警。

use crate::model::{ProviderReport, QuotaWindow};
use std::collections::HashMap;

const LOW_THRESHOLD: f64 = 0.10;
const DEPLETED_THRESHOLD: f64 = 0.005;
const RECOVERED_THRESHOLD: f64 = 0.50;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertKind {
    /// 剩余跌破 10%。
    Low,
    /// 剩余归零。
    Depleted,
    /// 重置后回满。
    Recovered,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Alert {
    pub kind: AlertKind,
    /// 平台中文名 + 窗口中文标签，如「智谱 Coding Plan · 5 小时窗口」。
    pub subject: String,
    /// 剩余百分比（Recovered 时是重置后的新值）。
    pub remaining: f64,
}

impl Alert {
    /// 气泡正文：一行事实描述，不带感叹号（挂件风格是陈述事实）。
    pub fn body_cn(&self) -> String {
        let pct = (self.remaining * 100.0).round().clamp(0.0, 100.0) as i64;
        match self.kind {
            AlertKind::Low => format!("{} 剩余 {pct}%", self.subject),
            AlertKind::Depleted => format!("{} 已耗尽", self.subject),
            AlertKind::Recovered => format!("{} 已重置，剩余 {pct}%", self.subject),
        }
    }
}

/// 单窗口的阈值跨越判定；prev 为 None 表示首次见到（播种，不告警）。
pub fn transition(prev: Option<f64>, now: f64) -> Option<AlertKind> {
    let now = now.clamp(0.0, 1.0);
    let prev = prev.map(|value| value.clamp(0.0, 1.0));
    match prev {
        None => None,
        Some(prev) => {
            if now <= DEPLETED_THRESHOLD && prev > DEPLETED_THRESHOLD {
                Some(AlertKind::Depleted)
            } else if now <= LOW_THRESHOLD && prev > LOW_THRESHOLD {
                Some(AlertKind::Low)
            } else if prev < LOW_THRESHOLD && now >= RECOVERED_THRESHOLD {
                Some(AlertKind::Recovered)
            } else {
                None
            }
        }
    }
}

/// 每个平台每个窗口的剩余百分比状态表。
#[derive(Default)]
pub struct Tracker {
    last: HashMap<(usize, String), f64>,
}

impl Tracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// 用缓存里的上一轮报表播种状态：只记录，不产生告警。
    pub fn seed(&mut self, reports: &[ProviderReport]) {
        for report in reports {
            self.update(report);
        }
    }

    /// 用新报表更新状态，返回本轮跨越阈值的所有告警。
    pub fn update(&mut self, report: &ProviderReport) -> Vec<Alert> {
        let provider_title = crate::render::title_cn(&report.title);
        let mut alerts = Vec::new();
        for window in &report.windows {
            let remaining = (1.0 - window.used_fraction).clamp(0.0, 1.0);
            let key = (report.provider.ordinal(), window.id.clone());
            let prev = self.last.insert(key, remaining);
            let Some(kind) = transition(prev, remaining) else {
                continue;
            };
            alerts.push(Alert {
                kind,
                subject: format!("{} · {}", provider_title, window_label_cn(window)),
                remaining,
            });
        }
        alerts
    }
}

fn window_label_cn(window: &QuotaWindow) -> String {
    crate::render::label_cn(&window.label)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transition_reports_threshold_crossings_only() {
        // 首次见到：播种，不告警。
        assert_eq!(transition(None, 0.05), None);
        // 高位晃动：不告警。
        assert_eq!(transition(Some(0.95), 0.90), None);
        // 跌破 10%：低额度。
        assert_eq!(transition(Some(0.12), 0.09), Some(AlertKind::Low));
        // 已经在低位继续下探：不重复告警。
        assert_eq!(transition(Some(0.09), 0.05), None);
        // 归零：耗尽。
        assert_eq!(transition(Some(0.05), 0.0), Some(AlertKind::Depleted));
        // 重置回满（≥50%）：恢复。
        assert_eq!(transition(Some(0.02), 0.98), Some(AlertKind::Recovered));
        // 小幅回升但没过 50%：不算重置。
        assert_eq!(transition(Some(0.02), 0.30), None);
        // 恢复后再度跌破：再次预警。
        assert_eq!(transition(Some(0.98), 0.08), Some(AlertKind::Low));
    }

    #[test]
    fn tracker_seeds_without_alerts_then_alerts_on_crossing() {
        use crate::model::ProviderId;
        let mut report = ProviderReport::ok(
            ProviderId::Glm,
            "Zhipu Coding Plan",
            None,
            None,
            vec![QuotaWindow::from_used_percent(
                "5h",
                "5h window",
                20.0,
                None,
            )],
        );
        let mut tracker = Tracker::new();
        // 播种：80% 剩余，无告警。
        assert!(tracker.update(&report).is_empty());
        // 仍在高位：无告警。
        report.windows[0].used_fraction = 0.85;
        assert!(tracker.update(&report).is_empty());
        // 跌破 10%：告警一次。
        report.windows[0].used_fraction = 0.93;
        let alerts = tracker.update(&report);
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].kind, AlertKind::Low);
        assert_eq!(alerts[0].subject, "智谱 Coding Plan · 5 小时窗口");
        assert_eq!(alerts[0].body_cn(), "智谱 Coding Plan · 5 小时窗口 剩余 7%");
        // 重复同值：不再告警。
        assert!(tracker.update(&report).is_empty());
    }

    #[test]
    fn body_cn_covers_all_kinds() {
        let alert = |kind, remaining| Alert {
            kind,
            subject: "Kimi Code · 每周额度".into(),
            remaining,
        };
        assert_eq!(
            alert(AlertKind::Depleted, 0.0).body_cn(),
            "Kimi Code · 每周额度 已耗尽"
        );
        assert_eq!(
            alert(AlertKind::Recovered, 1.0).body_cn(),
            "Kimi Code · 每周额度 已重置，剩余 100%"
        );
    }
}
