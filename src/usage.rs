//! 本机 OMP 会话的 token 用量：增量扫描 `~/.omp/agent/sessions` 下的 JSONL。
//!
//! 每轮 `message` 行的 `message.usage` 记录该轮 input / output / reasoning /
//! cacheRead / cacheWrite / totalTokens；按平台聚合成近 1 天 / 7 天 / 30 天与
//! 本机累计。「本机累计」只是当前设备仍保留的日志合计，不等于服务端账号累计。
//! 文件按 (mtime, size) 缓存解析结果，未变化的文件不重复解析；扫描结果在进程内
//! 保留 120 秒，刷新循环里只有首次扫描是全量的。

use crate::credentials;
use crate::model::{ProviderId, UsageModel, UsageRow};
use serde_json::Value;
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

/// 聚合结果的保鲜间隔：与挂件刷新节奏一致，避免每轮都重扫目录。
const REFRESH_INTERVAL: Duration = Duration::from_secs(120);

#[derive(Debug, Clone)]
struct UsageEvent {
    provider: ProviderId,
    timestamp: i64,
    model: String,
    total_tokens: u64,
}

/// 文件级缓存键：任一维度变化才重新解析。
#[derive(Clone, PartialEq)]
struct FileKey {
    mtime: SystemTime,
    size: u64,
}

#[derive(Default)]
struct Store {
    files: HashMap<PathBuf, (FileKey, Vec<UsageEvent>)>,
    scanned_at: Option<Instant>,
}

static STORE: Mutex<Option<Store>> = Mutex::new(None);

/// 把本机聚合挂到报表上：缺失授权的卡片不挂；已有的远端行保留在后面。
pub fn attach(report: &mut crate::model::ProviderReport) {
    if report.is_missing() {
        return;
    }
    let mut rows = rows_for(report.provider);
    if rows.is_empty() {
        return;
    }
    let existing = report.usage.take().unwrap_or_default();
    rows.extend(existing);
    report.usage = Some(rows);
}

fn rows_for(provider: ProviderId) -> Vec<UsageRow> {
    refresh_if_stale();
    let guard = STORE.lock().ok();
    let events: Vec<UsageEvent> = guard
        .as_ref()
        .and_then(|guard| guard.as_ref())
        .map(|store| {
            store
                .files
                .values()
                .flat_map(|(_, events)| events.iter())
                .filter(|event| event.provider == provider)
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    if provider == ProviderId::Antigravity {
        let (gemini, claude_gpt): (Vec<UsageEvent>, Vec<UsageEvent>) =
            events.into_iter().partition(|e| is_gemini_model(&e.model));
        let mut rows = Vec::new();
        let mut gemini_rows = aggregate(gemini);
        for row in &mut gemini_rows {
            row.group = Some("gemini".into());
        }
        rows.extend(gemini_rows);
        let mut claude_gpt_rows = aggregate(claude_gpt);
        for row in &mut claude_gpt_rows {
            row.group = Some("claude_gpt".into());
        }
        rows.extend(claude_gpt_rows);
        rows
    } else {
        aggregate(events)
    }
}

fn is_gemini_model(model: &str) -> bool {
    model.to_ascii_lowercase().starts_with("gemini")
}
/// 按事件时间聚合为 1d / 7d / 30d / 本机累计四档；零值档不输出。
fn aggregate(events: Vec<UsageEvent>) -> Vec<UsageRow> {
    /// (id, 英文说明, 偏移秒)；「本机累计」无时间下限。
    const BUCKETS: [(&str, &str, i64); 4] = [
        ("1d", "last 1 day", 86_400),
        ("7d", "last 7 days", 7 * 86_400),
        ("30d", "last 30 days", 30 * 86_400),
        ("all", "local total", i64::MAX),
    ];
    let now = chrono::Utc::now().timestamp();
    let mut models: [HashMap<String, u64>; 4] = Default::default();
    let mut totals = [0_u64; 4];
    for event in events {
        for (index, (_, _, offset)) in BUCKETS.iter().enumerate() {
            let cutoff = now.saturating_sub(*offset);
            if event.timestamp >= cutoff {
                totals[index] += event.total_tokens;
                *models[index].entry(event.model.clone()).or_default() += event.total_tokens;
            }
        }
    }
    let mut rows = Vec::new();
    for ((id, label, _), (total, models)) in BUCKETS.iter().zip(totals.into_iter().zip(models)) {
        if total == 0 {
            continue;
        }
        rows.push(UsageRow {
            id: id.to_string(),
            label: label.to_string(),
            source: "local".into(),
            total_tokens: total,
            group: None,
            models: sorted_models(models),
        });
    }
    rows
}

fn sorted_models(models: HashMap<String, u64>) -> Vec<UsageModel> {
    let mut models: Vec<UsageModel> = models
        .into_iter()
        .map(|(name, total_tokens)| UsageModel { name, total_tokens })
        .collect();
    models.sort_by(|a, b| {
        b.total_tokens
            .cmp(&a.total_tokens)
            .then(a.name.cmp(&b.name))
    });
    models
}

fn refresh_if_stale() {
    let mut guard = match STORE.lock() {
        Ok(guard) => guard,
        Err(_) => return,
    };
    if let Some(store) = guard.as_ref() {
        if store
            .scanned_at
            .is_some_and(|at| at.elapsed() < REFRESH_INTERVAL)
        {
            return;
        }
    }
    let mut store = guard.take().unwrap_or_default();
    let root = credentials::omp_sessions_root();
    if let Some(root) = root.as_deref() {
        scan_root(&mut store, root);
    }
    store.scanned_at = Some(Instant::now());
    *guard = Some(store);
}

/// 增量扫描：新文件与 (mtime, size) 变化的文件重新解析，其余复用缓存。
fn scan_root(store: &mut Store, root: &Path) {
    let mut current: Vec<PathBuf> = Vec::new();
    collect_jsonl(root, &mut current);
    let mut next: HashMap<PathBuf, (FileKey, Vec<UsageEvent>)> = HashMap::new();
    for path in current {
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        let key = FileKey {
            mtime: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            size: meta.len(),
        };
        let cached = store.files.get(&path).filter(|(old, _)| *old == key);
        let events = match cached {
            Some((_, events)) => events.clone(),
            None => parse_file(&path),
        };
        next.insert(path, (key, events));
    }
    store.files = next;
}

fn collect_jsonl(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_jsonl(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "jsonl") {
            out.push(path);
        }
    }
}

fn parse_file(path: &Path) -> Vec<UsageEvent> {
    let Ok(file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let mut events = Vec::new();
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else {
            continue;
        };
        // 逐行先做包含判断，绝大多数行（用户输入、工具输出）不含 usage。
        if !line.contains("\"usage\"") {
            continue;
        }
        let Ok(row) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let message = row
            .get("message")
            .filter(|message| message.is_object())
            .unwrap_or(&row);
        let Some(provider) = message
            .get("provider")
            .and_then(Value::as_str)
            .and_then(provider_id)
        else {
            continue;
        };
        let Some(usage) = message.get("usage").and_then(Value::as_object) else {
            continue;
        };
        let total = usage
            .get("totalTokens")
            .and_then(as_u64)
            .unwrap_or_else(|| {
                [
                    "input",
                    "output",
                    "reasoningTokens",
                    "cacheRead",
                    "cacheWrite",
                ]
                .iter()
                .map(|key| usage.get(*key).and_then(as_u64).unwrap_or(0))
                .sum()
            });
        if total == 0 {
            continue;
        }
        let model = message
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("未标记模型")
            .to_string();
        let timestamp = row
            .get("timestamp")
            .or_else(|| message.get("timestamp"))
            .and_then(Value::as_str)
            .and_then(parse_timestamp)
            .unwrap_or(0);
        events.push(UsageEvent {
            provider,
            timestamp,
            model,
            total_tokens: total,
        });
    }
    events
}

fn as_u64(value: &Value) -> Option<u64> {
    match value {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn parse_timestamp(raw: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|dt| dt.with_timezone(&chrono::Utc).timestamp())
}

/// OMP 凭据库里的 provider 名映射到卡片平台；未识别的不计。
fn provider_id(raw: &str) -> Option<ProviderId> {
    match raw {
        "openai-codex" | "openai" | "codex" => Some(ProviderId::Codex),
        "anthropic" => Some(ProviderId::Claude),
        "xai-oauth" | "xai" => Some(ProviderId::Grok),
        "zhipu-coding-plan" | "zhipuai-coding-plan" | "zai" => Some(ProviderId::Glm),
        "kimi-code" | "kimi-for-coding" | "kimi" => Some(ProviderId::Kimi),
        "cursor" => Some(ProviderId::Cursor),
        "devin" => Some(ProviderId::Devin),
        "google-antigravity" => Some(ProviderId::Antigravity),
        "deepseek" => Some(ProviderId::Deepseek),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_session(dir: &Path, name: &str, lines: &[String]) -> PathBuf {
        let path = dir.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, lines.join("\n")).unwrap();
        path
    }

    fn usage_line(provider: &str, model: &str, timestamp: &str, total: u64) -> String {
        serde_json::json!({
            "type": "message",
            "timestamp": timestamp,
            "message": {
                "provider": provider,
                "model": model,
                "usage": {"totalTokens": total}
            }
        })
        .to_string()
    }

    /// 解析必须只认 message 行的 usage；未知平台、零值与非 message 行都不计。
    #[test]
    fn parse_file_filters_provider_and_zero_totals() {
        let dir = std::env::temp_dir().join(format!("cq-usage-parse-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = write_session(
            &dir,
            "a.jsonl",
            &[
                usage_line("zhipu-coding-plan", "glm-5.3", "2026-10-01T00:00:00Z", 100),
                usage_line("no-such-provider", "x", "2026-10-01T00:00:00Z", 500),
                usage_line("zhipu-coding-plan", "glm-5.3", "2026-10-01T01:00:00Z", 0),
                serde_json::json!({
                    "type": "message",
                    "timestamp": "2026-10-01T02:00:00Z",
                    "message": {
                        "provider": "zhipu-coding-plan",
                        "model": "glm-5.3",
                        "usage": {"input": 30, "output": 10, "reasoningTokens": 5,
                                  "cacheRead": 4, "cacheWrite": 1}
                    }
                })
                .to_string(),
                serde_json::json!({"type": "user", "message": {"provider": "zhipu-coding-plan"}})
                    .to_string(),
                "not json at all".to_string(),
            ],
        );
        let events = parse_file(&path);
        assert_eq!(events.len(), 2, "未知平台、零值与非 message 行都不计");
        assert_eq!(events[0].total_tokens, 100);
        // totalTokens 缺失时按五个分项求和（30+10+5+4+1）。
        assert_eq!(events[1].total_tokens, 50);
        assert_eq!(
            events[0].timestamp,
            chrono::DateTime::parse_from_rfc3339("2026-10-01T00:00:00Z")
                .unwrap()
                .timestamp()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 时间桶按事件时间归入 1d/7d/30d/累计；模型明细按用量降序。
    #[test]
    fn aggregate_buckets_by_event_time_with_model_details() {
        let now = chrono::Utc::now().timestamp();
        let event = |hours_ago: i64, model: &str, total: u64| UsageEvent {
            provider: ProviderId::Codex,
            timestamp: now - hours_ago * 3600,
            model: model.to_string(),
            total_tokens: total,
        };
        let rows = aggregate(vec![
            event(1, "gpt-6-astra", 1_000),
            event(72, "gpt-6-astra", 10_000),
            event(24 * 10, "gpt-5.6-luna", 100_000),
            event(24 * 40, "gpt-5.6-luna", 1_000_000),
            event(2, "gpt-6-astra", 50),
        ]);
        let totals: Vec<(String, u64)> = rows
            .iter()
            .map(|row| (row.id.clone(), row.total_tokens))
            .collect();
        assert_eq!(
            totals,
            vec![
                ("1d".into(), 1_050),
                ("7d".into(), 11_050),
                ("30d".into(), 111_050),
                ("all".into(), 1_111_050),
            ]
        );
        // 7 天桶只含 72 小时内的事件，luna 要到 30 天桶才出现。
        let weekly = &rows[1];
        assert_eq!(weekly.models.len(), 1);
        assert_eq!(weekly.models[0].name, "gpt-6-astra");
        assert_eq!(weekly.models[0].total_tokens, 11_050);
        let monthly = &rows[2];
        assert_eq!(monthly.models[0].name, "gpt-5.6-luna");
        assert_eq!(monthly.models[0].total_tokens, 100_000);
        assert!(rows.iter().all(|row| row.source == "local"));
    }

    /// (mtime, size) 未变化的文件不重新解析；变化的文件重新解析。
    #[test]
    fn scan_root_reuses_cached_files_until_they_change() {
        let dir = std::env::temp_dir().join(format!("cq-usage-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = write_session(
            &dir,
            "c.jsonl",
            &[usage_line("kimi-code", "k3", "2026-10-01T00:00:00Z", 7)],
        );
        let mut store = Store::default();
        scan_root(&mut store, &dir);
        assert_eq!(store.files[&path].1.len(), 1);
        // 未变化（mtime/size 相同）：复用缓存条目。
        scan_root(&mut store, &dir);
        assert_eq!(store.files[&path].1.len(), 1);
        // 追加内容后 size 变化，重新解析出两行。
        write_session(
            &dir,
            "c.jsonl",
            &[
                usage_line("kimi-code", "k3", "2026-10-01T00:00:00Z", 7),
                usage_line("kimi-code", "k3", "2026-10-02T00:00:00Z", 9),
            ],
        );
        scan_root(&mut store, &dir);
        assert_eq!(store.files[&path].1.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn test_is_gemini_model() {
        assert!(is_gemini_model("gemini-3.8-flash"));
        assert!(is_gemini_model("Gemini-2.5-Pro"));
        assert!(is_gemini_model("gemini-1.5-flash"));
        assert!(!is_gemini_model("claude-opus-4-6"));
        assert!(!is_gemini_model("claude-3-7-sonnet"));
        assert!(!is_gemini_model("gpt-4o"));
    }
}
