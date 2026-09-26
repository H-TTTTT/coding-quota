//! 上一轮成功额度数据的磁盘缓存：刷新失败时保留显示旧值，而不是只剩一行报错。
//! 每轮刷新先把成功的报表写入 `%APPDATA%\coding-quota\last_good.json`；
//! 有失败的报表时再用缓存回填（错误信息保留），界面照常画额度并附上报错。
//! 挂件按平台增量取数，所以除了整轮的 `save` / `apply`，还有按条目的 `Cache`。

use crate::model::{ProviderId, ProviderReport, Snapshot};
use std::collections::HashMap;
use std::path::PathBuf;

/// 超过这个时长的旧值不再回填：数据太旧时，显示报错比展示过期额度更诚实
/// （Kimi 曾连续 5 天挂着 09-20 的额度）。
const MAX_STALE_AGE: chrono::Duration = chrono::Duration::hours(24);

fn cache_path() -> Option<PathBuf> {
    let appdata = std::env::var_os("APPDATA")?;
    Some(
        PathBuf::from(appdata)
            .join("coding-quota")
            .join("last_good.json"),
    )
}

#[derive(serde::Serialize, serde::Deserialize)]
struct CacheFile {
    reports: Vec<ProviderReport>,
}

/// 旧值是否还值得回填（`MAX_STALE_AGE` 之内）。
fn usable(report: &ProviderReport) -> bool {
    chrono::Utc::now() - report.fetched_at <= MAX_STALE_AGE
}

fn read() -> HashMap<ProviderId, ProviderReport> {
    let mut map = HashMap::new();
    let Some(path) = cache_path() else {
        return map;
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return map;
    };
    // 缓存损坏（手改、写到一半断电）当作没有，下一轮成功后自然修复。
    let Ok(file) = serde_json::from_str::<CacheFile>(&text) else {
        return map;
    };
    for report in file.reports {
        map.insert(report.provider, report);
    }
    map
}

/// 内存里的缓存副本：挂件每条报表都要回填/落盘，不必每条都重读一次文件。
#[derive(Debug, Default)]
pub struct Cache {
    reports: HashMap<ProviderId, ProviderReport>,
}

impl Cache {
    pub fn load() -> Self {
        Self { reports: read() }
    }

    /// 失败或退避中的报表用旧值回填（错误信息与旧数据的 `fetched_at` 都保留，
    /// 界面上的「x 分钟前」才如实反映数据年龄）。没有凭据、或旧值已过期则不回填。
    pub fn backfill(&self, report: &mut ProviderReport) {
        if report.error.is_none() || report.is_missing() {
            return;
        }
        let Some(stale) = self.reports.get(&report.provider) else {
            return;
        };
        if !usable(stale) {
            return;
        }
        report.identity.clone_from(&stale.identity);
        report.plan.clone_from(&stale.plan);
        report.resets_left = stale.resets_left;
        report.windows.clone_from(&stale.windows);
        report.fetched_at = stale.fetched_at;
    }

    /// 单条结果合并落盘：成功覆盖、凭据被移除的清掉、失败不动
    /// （部分失败的轮次不该把失败平台的好数据抹掉）。
    pub fn save_report(&mut self, report: &ProviderReport) {
        if !self.merge(report) {
            return;
        }
        self.prune();
        self.write();
    }

    /// 合并单条结果（不落盘）：返回缓存是否真的变了。
    fn merge(&mut self, report: &ProviderReport) -> bool {
        if report.is_missing() {
            return self.reports.remove(&report.provider).is_some();
        }
        if report.error.is_some() {
            return false;
        }
        self.reports.insert(report.provider, report.clone());
        true
    }

    fn prune(&mut self) {
        self.reports.retain(|_, report| usable(report));
    }

    fn write(&self) {
        let Some(path) = cache_path() else {
            return;
        };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let mut reports: Vec<&ProviderReport> = self.reports.values().collect();
        reports.sort_by_key(|report| report.provider.ordinal());
        let file = CacheFile {
            reports: reports.into_iter().cloned().collect(),
        };
        let Ok(text) = serde_json::to_string(&file) else {
            return;
        };
        // 临时文件 + rename：挂件与 TUI 可能同时落盘，别让对方读到写了一半的文件；
        // 临时名带上进程号，两个进程也不会撞进同一个临时文件。
        let pending = path.with_extension(format!("json.tmp.{}", std::process::id()));
        if std::fs::write(&pending, text).is_err() {
            let _ = std::fs::remove_file(&pending);
            return;
        }
        if std::fs::rename(&pending, &path).is_err() {
            let _ = std::fs::remove_file(&pending);
        }
    }
}

/// 整轮落盘：把本轮成功的报表与已有缓存合并写回（TUI / 文本快照沿用）。
/// 凭据被移除（missing）的平台顺手清掉缓存条目，旧数据不该继续挂着。
pub fn save(snapshot: &Snapshot) {
    let mut cache = Cache::load();
    let mut changed = false;
    for report in &snapshot.reports {
        changed |= cache.merge(report);
    }
    if !changed {
        return;
    }
    cache.prune();
    cache.write();
}

/// 整轮回填：失败或退避中的报表用缓存里的旧值替换（错误信息与旧数据时间都保留）。
pub fn apply(snapshot: &mut Snapshot) {
    let cache = Cache::load();
    for report in &mut snapshot.reports {
        cache.backfill(report);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::QuotaWindow;

    fn report(
        provider: ProviderId,
        remaining_percent: f64,
        age: chrono::Duration,
    ) -> ProviderReport {
        let mut report = ProviderReport::ok(
            provider,
            provider.title(),
            Some("user@example.com".into()),
            Some("Pro".into()),
            vec![QuotaWindow::from_used_percent(
                "window",
                "Window",
                100.0 - remaining_percent,
                None,
            )],
        );
        report.fetched_at = chrono::Utc::now() - age;
        report
    }

    fn cache_with(entries: Vec<ProviderReport>) -> Cache {
        Cache {
            reports: entries
                .into_iter()
                .map(|report| (report.provider, report))
                .collect(),
        }
    }

    /// 失败平台的旧额度照常显示，且数据年龄必须如实（界面靠 fetched_at 标「x 分钟前」）。
    #[test]
    fn backfill_keeps_error_and_data_age() {
        let stale = report(ProviderId::Kimi, 42.0, chrono::Duration::minutes(90));
        let cache = cache_with(vec![stale.clone()]);
        let mut failed = ProviderReport::err(ProviderId::Kimi, None, "HTTP 500: boom");
        cache.backfill(&mut failed);
        assert_eq!(failed.error.as_deref(), Some("HTTP 500: boom"));
        assert_eq!(failed.fetched_at, stale.fetched_at);
        assert_eq!(failed.plan.as_deref(), Some("Pro"));
        assert_eq!(failed.windows.len(), 1);
        assert!((failed.windows[0].used_fraction - 0.58).abs() < 1e-9);
    }

    /// 超过 24 小时的旧值不再回填（Kimi 曾连续 5 天挂着 09-20 的额度）。
    #[test]
    fn backfill_refuses_expired_data() {
        let cache = cache_with(vec![report(
            ProviderId::Kimi,
            42.0,
            chrono::Duration::hours(30),
        )]);
        let mut failed = ProviderReport::err(ProviderId::Kimi, None, "HTTP 500: boom");
        cache.backfill(&mut failed);
        assert!(failed.windows.is_empty(), "过期旧值不该回填");
    }

    /// 授权被移除（missing）的平台不回填：旧额度不该继续挂着。
    #[test]
    fn backfill_never_revives_removed_credentials() {
        let cache = cache_with(vec![report(
            ProviderId::Kimi,
            42.0,
            chrono::Duration::minutes(5),
        )]);
        let mut logged_out = ProviderReport::missing(ProviderId::Kimi);
        cache.backfill(&mut logged_out);
        assert!(logged_out.is_missing());
        assert!(
            logged_out.windows.is_empty(),
            "登录态已移除的平台不该复活旧额度"
        );
    }

    /// 合并语义：成功覆盖、失败不动（好数据不被抹掉）、登出清条目。
    #[test]
    fn merge_keeps_success_and_drops_logged_out() {
        let mut cache = Cache::default();
        assert!(cache.merge(&report(ProviderId::Grok, 80.0, chrono::Duration::zero())));
        assert_eq!(cache.reports.len(), 1);
        assert!(!cache.merge(&ProviderReport::err(
            ProviderId::Grok,
            None,
            "HTTP 500: boom"
        )));
        assert_eq!(cache.reports.len(), 1, "失败不覆盖好数据");
        assert!(cache.merge(&ProviderReport::missing(ProviderId::Grok)));
        assert!(cache.reports.is_empty());
        assert!(
            !cache.merge(&ProviderReport::missing(ProviderId::Grok)),
            "本来就没有条目，不算变化"
        );
    }

    #[test]
    fn prune_drops_expired_entries() {
        let mut cache = cache_with(vec![
            report(ProviderId::Kimi, 42.0, chrono::Duration::hours(30)),
            report(ProviderId::Grok, 80.0, chrono::Duration::minutes(5)),
        ]);
        cache.prune();
        assert!(cache.reports.contains_key(&ProviderId::Grok));
        assert!(!cache.reports.contains_key(&ProviderId::Kimi));
    }
}
