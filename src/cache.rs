//! 上一轮成功额度数据的磁盘缓存：刷新失败时保留显示旧值，而不是只剩一行报错。
//! 每轮刷新先把成功的报表写入 `%APPDATA%\coding-quota\last_good.json`；
//! 有失败的报表时再用缓存回填（错误信息保留），界面照常画额度并附上报错。

use crate::model::{ProviderId, ProviderReport, Snapshot};
use std::collections::HashMap;
use std::path::PathBuf;

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

fn load() -> HashMap<ProviderId, ProviderReport> {
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

/// 把本轮成功的报表落盘。全部失败时不写，避免把仅存的好数据抹掉。
pub fn save(snapshot: &Snapshot) {
    let reports: Vec<ProviderReport> = snapshot
        .reports
        .iter()
        .filter(|report| report.error.is_none())
        .cloned()
        .collect();
    if reports.is_empty() {
        return;
    }
    let Some(path) = cache_path() else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let Ok(text) = serde_json::to_string(&CacheFile { reports }) else {
        return;
    };
    let _ = std::fs::write(path, text);
}

/// 失败的报表用缓存数据回填（错误信息保留，fetched_at 用旧数据的时间，
/// 界面上的「x分钟前」才如实反映数据年龄）。
/// 没有凭据的平台不回填：授权已被移除，旧数据不该继续挂着。
pub fn apply(snapshot: &mut Snapshot) {
    let cache = load();
    if cache.is_empty() {
        return;
    }
    for report in &mut snapshot.reports {
        if report.error.is_none() || report.is_missing() {
            continue;
        }
        if let Some(stale) = cache.get(&report.provider) {
            report.identity.clone_from(&stale.identity);
            report.plan.clone_from(&stale.plan);
            report.resets_left = stale.resets_left;
            report.windows.clone_from(&stale.windows);
            report.fetched_at = stale.fetched_at;
        }
    }
}
