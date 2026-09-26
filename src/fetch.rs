use crate::backoff::Backoff;
use crate::credentials::{self, CredentialSet, StoredCred};
use crate::model::{ProviderId, ProviderReport, QuotaWindow, Snapshot};
use chrono::{DateTime, TimeZone, Utc};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, USER_AGENT};
use serde_json::Value;
use std::future::Future;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

const UA: &str = "coding-quota/0.1";
const TIMEOUT: Duration = Duration::from_secs(20);
/// 失败的平台先按 5 分钟退避（与挂件的刷新节奏一致），连续失败翻倍，1 小时封顶。
const BACKOFF_BASE: Duration = Duration::from_secs(300);
const BACKOFF_MAX: Duration = Duration::from_secs(3600);

/// 复用同一个 HTTP client：连接与 TLS 会话能跨轮复用，不必每轮重建连接池、重做握手。
static CLIENT: OnceLock<Result<reqwest::Client, String>> = OnceLock::new();

fn http_client() -> Result<&'static reqwest::Client, String> {
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .timeout(TIMEOUT)
                .build()
                .map_err(|err| format!("http client: {err}"))
        })
        .as_ref()
        .map_err(Clone::clone)
}

/// 每个平台各自的失败退避状态：成功即清零，进程重启即重置。
static BACKOFF: OnceLock<Mutex<Backoff>> = OnceLock::new();

fn backoff() -> &'static Mutex<Backoff> {
    BACKOFF.get_or_init(|| Mutex::new(Backoff::new(BACKOFF_BASE, BACKOFF_MAX)))
}

/// 还在退避中的平台：本轮不发请求，只回一条说明（旧额度由缓存回填，界面照常显示）。
fn deferred_report(provider: ProviderId, identity: Option<String>) -> Option<ProviderReport> {
    let (failures, wait) = backoff().lock().ok()?.deferred(provider)?;
    Some(ProviderReport::err(
        provider,
        identity,
        format!("连续 {failures} 次失败（{}后自动重试）", minutes_cn(wait)),
    ))
}

fn minutes_cn(wait: Duration) -> String {
    let minutes = (wait.as_secs_f64() / 60.0).ceil().max(1.0) as u64;
    format!("{minutes} 分钟")
}

/// 取数一轮，每路完成立刻回调：界面不必等最慢的一路（Devin 横幅最长 25s）才有数据。
/// 返回的 Snapshot 仍是本轮全部报表，顺序固定（见 `ProviderId::ALL`），供 CLI / TUI 使用。
pub async fn fetch_all_streaming(
    creds: &CredentialSet,
    only: Option<ProviderId>,
    skip: &[ProviderId],
    mut on_report: impl FnMut(ProviderReport),
) -> Snapshot {
    let client = match http_client() {
        Ok(client) => client,
        Err(err) => {
            let reports: Vec<ProviderReport> = ProviderId::ALL
                .into_iter()
                .filter(|provider| only.is_none_or(|wanted| wanted == *provider))
                .filter(|provider| !skip.contains(provider))
                .map(|provider| ProviderReport::err(provider, None, err.clone()))
                .collect();
            return Snapshot {
                fetched_at: Utc::now(),
                reports,
            };
        }
    };

    let skip: Arc<[ProviderId]> = Arc::from(skip);
    let mut tasks = tokio::task::JoinSet::new();
    for (provider, cred) in [
        (ProviderId::Codex, creds.codex.clone()),
        (ProviderId::Claude, creds.claude.clone()),
        (ProviderId::Grok, creds.grok.clone()),
        (ProviderId::Glm, creds.glm.clone()),
        (ProviderId::Kimi, creds.kimi.clone()),
        (ProviderId::Cursor, creds.cursor.clone()),
    ] {
        let client = client.clone();
        let skip = skip.clone();
        tasks.spawn(async move { maybe_fetch(&client, provider, cred, only, &skip).await });
    }
    {
        // Devin 走 CLI 横幅，不使用 CredentialSet 里的凭据；它本身是阻塞调用，
        // 由 maybe_devin 挪到 blocking 线程池，不拖住其他平台。
        let skip = skip.clone();
        tasks.spawn(async move { maybe_devin(only, &skip).await });
    }

    let mut reports = Vec::new();
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok(Some(report)) => {
                on_report(report.clone());
                reports.push(report);
            }
            Ok(None) => {}
            // 任务 panic：只有这一路本轮没有数据，其他平台照常显示
            Err(_) => {}
        }
    }
    // 并行回来的是完成顺序，卡片位置要按平台固定顺序排，否则每轮跳位置
    reports.sort_by_key(|report| report.provider.ordinal());
    Snapshot {
        fetched_at: Utc::now(),
        reports,
    }
}

pub async fn fetch_all(
    creds: &CredentialSet,
    only: Option<ProviderId>,
    skip: &[ProviderId],
) -> Snapshot {
    fetch_all_streaming(creds, only, skip, |_| {}).await
}

async fn maybe_fetch(
    client: &reqwest::Client,
    provider: ProviderId,
    cred: Option<StoredCred>,
    only: Option<ProviderId>,
    skip: &[ProviderId],
) -> Option<ProviderReport> {
    if only.is_some_and(|wanted| wanted != provider) {
        return None;
    }
    // 用户在挂件里退订（隐藏）的平台：完全不取数
    if skip.contains(&provider) {
        return None;
    }
    let Some(cred) = cred else {
        // 没有凭据：不发请求，也不占着退避状态（重新登录后要能立刻取数）
        if let Ok(mut state) = backoff().lock() {
            state.succeed(provider);
        }
        return Some(ProviderReport::missing(provider));
    };
    // 连续失败、还在退避中的平台：本轮只回一条说明，旧额度由缓存回填
    if let Some(report) = deferred_report(provider, cred.identity.clone()) {
        return Some(report);
    }
    let report = match provider {
        ProviderId::Codex => fetch_codex(client, cred).await,
        ProviderId::Claude => fetch_claude(client, cred).await,
        ProviderId::Grok => fetch_grok(client, cred).await,
        ProviderId::Glm => fetch_glm(client, cred).await,
        ProviderId::Kimi => fetch_kimi(client, cred).await,
        ProviderId::Cursor => fetch_cursor(client, cred).await,
        // Devin 走 CLI 横幅，不使用 CredentialSet 里的凭据
        ProviderId::Devin => fetch_devin(),
    };
    // 成功清零、失败退避：下一轮不再按原节奏硬撞（Claude 的端点与 Claude Code 共享限流）
    if let Ok(mut state) = backoff().lock() {
        if report.error.is_none() {
            state.succeed(provider);
        } else {
            state.fail(provider);
        }
    }
    Some(report)
}

type FetchFuture<'a> = std::pin::Pin<Box<dyn Future<Output = Result<Value, String>> + Send + 'a>>;

/// Resolves a token, runs the request, and on HTTP 401 force-refreshes the
/// token through omp and retries once.
async fn fetch_with_refresh<F>(
    client: &reqwest::Client,
    cred: &StoredCred,
    omp_provider: &str,
    request: F,
) -> Result<(String, Value), String>
where
    F: for<'a> Fn(&'a reqwest::Client, &'a str) -> FetchFuture<'a>,
{
    let Some(token) = resolve_secret(cred, omp_provider) else {
        return Err(format!("missing token ({omp_provider})"));
    };
    match request(client, &token).await {
        Ok(body) => Ok((token, body)),
        Err(err) if err.starts_with("HTTP 401") => {
            let Some(fresh) = credentials::secret_from_omp(omp_provider, true) else {
                return Err(err);
            };
            if fresh == token {
                return Err(err);
            }
            let body = request(client, &fresh).await?;
            Ok((fresh, body))
        }
        Err(err) => Err(err),
    }
}

fn resolve_secret(cred: &StoredCred, omp_provider: &str) -> Option<String> {
    // Try the stored token first. Refreshing preemptively from a stale
    // expires_ms value launches omp on every polling cycle; a real 401 is
    // the authoritative signal and fetch_with_refresh retries it once.
    cred.access
        .clone()
        .or_else(|| credentials::secret_from_omp(omp_provider, false))
}

async fn fetch_glm(client: &reqwest::Client, cred: StoredCred) -> ProviderReport {
    let Some(key) = resolve_secret(&cred, "zhipu-coding-plan") else {
        return ProviderReport::err(ProviderId::Glm, cred.identity, "missing API key");
    };
    let urls = [
        "https://open.bigmodel.cn/api/monitor/usage/quota/limit",
        "https://bigmodel.cn/api/monitor/usage/quota/limit",
    ];
    let mut last_err = "no endpoint responded".to_string();
    for url in urls {
        match get_json(client, url, raw_auth(&key)).await {
            Ok(body) => return parse_glm(cred.identity.clone(), body),
            Err(err) => last_err = err,
        }
    }
    ProviderReport::err(ProviderId::Glm, cred.identity, last_err)
}

fn parse_glm(identity: Option<String>, body: Value) -> ProviderReport {
    let data = body.get("data").unwrap_or(&body);
    let plan = data
        .get("level")
        .and_then(|v| v.as_str())
        .map(|s| format!("Coding Plan {}", s.to_ascii_uppercase()));
    let Some(limits) = data.get("limits").and_then(|v| v.as_array()) else {
        return ProviderReport::err(ProviderId::Glm, identity, "invalid quota payload");
    };

    let mut windows = Vec::new();
    for limit in limits {
        let kind = limit.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let unit = limit.get("unit").and_then(|v| v.as_i64()).unwrap_or(0);
        let used_percent = number(limit.get("percentage")).unwrap_or(0.0);
        let reset = millis_to_dt(number(limit.get("nextResetTime")));
        let (id, label) = match (kind, unit) {
            ("TOKENS_LIMIT" | "CREDIT_LIMIT", 3) => ("glm-5h", "5h window"),
            ("TOKENS_LIMIT" | "CREDIT_LIMIT", 6) => ("glm-week", "Weekly"),
            ("TIME_LIMIT", _) => ("glm-mcp", "MCP / tools"),
            ("TOKENS_LIMIT" | "CREDIT_LIMIT", _) => ("glm-credit", "Credits"),
            _ => continue,
        };
        if let Some(window) = glm_count_window(id, label, limit, reset) {
            windows.push(window);
        } else {
            windows.push(QuotaWindow::from_used_percent(
                id,
                label,
                used_percent,
                reset,
            ));
        }
    }
    if windows.is_empty() {
        return ProviderReport::err(ProviderId::Glm, identity, "no quota windows");
    }
    ProviderReport::ok(
        ProviderId::Glm,
        "Zhipu Coding Plan",
        identity,
        plan,
        windows,
    )
}

fn glm_count_window(
    id: &str,
    label: &str,
    limit: &Value,
    reset: Option<DateTime<Utc>>,
) -> Option<QuotaWindow> {
    let used = number(limit.get("currentValue")).or_else(|| number(limit.get("usage")))?;
    let total = number(limit.get("number"))
        .or_else(|| number(limit.get("remaining")).map(|remain| used + remain))?;
    if total > 1.0 {
        Some(QuotaWindow::from_used_limit(
            id, label, used, total, "count", reset,
        ))
    } else {
        None
    }
}

const CLAUDE_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const CLAUDE_PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";

/// 套餐名很少变：进程内按账号记住 6 小时，避免每轮刷新都多查一次 profile。
/// usage 接口限流很紧（与 omp / Claude Code 共用同一账号额度，omp 遇到 429 也不重试）。
const CLAUDE_PLAN_TTL: Duration = Duration::from_secs(6 * 3600);
type ClaudePlanMemo = Option<(Option<String>, String, std::time::Instant)>;
static CLAUDE_PLAN: std::sync::Mutex<ClaudePlanMemo> = std::sync::Mutex::new(None);

fn remembered_claude_plan(identity: Option<&str>) -> Option<String> {
    let memo = CLAUDE_PLAN.lock().ok()?;
    let (who, plan, at) = memo.as_ref()?;
    (who.as_deref() == identity && at.elapsed() < CLAUDE_PLAN_TTL).then(|| plan.clone())
}

fn remember_claude_plan(identity: Option<&str>, plan: &str) {
    if let Ok(mut memo) = CLAUDE_PLAN.lock() {
        *memo = Some((
            identity.map(str::to_string),
            plan.to_string(),
            std::time::Instant::now(),
        ));
    }
}

/// Claude 订阅（Pro / Max）额度：Claude Code `/usage` 用的同一个 OAuth 接口。
/// profile 只用来识别套餐，失败不影响额度显示；两者用同一 token，401 时一起刷新重试。
async fn fetch_claude(client: &reqwest::Client, cred: StoredCred) -> ProviderReport {
    let identity = cred.identity.clone();
    let remembered = remembered_claude_plan(identity.as_deref());
    let need_profile = remembered.is_none();
    match fetch_with_refresh(client, &cred, "anthropic", move |client, token| {
        Box::pin(async move {
            let usage = get_json(client, CLAUDE_USAGE_URL, claude_headers(token));
            let (usage, profile) = if need_profile {
                let profile = get_json(client, CLAUDE_PROFILE_URL, claude_headers(token));
                let (usage, profile) = tokio::join!(usage, profile);
                (usage, profile.unwrap_or(Value::Null))
            } else {
                (usage.await, Value::Null)
            };
            Ok(serde_json::json!({ "usage": usage?, "profile": profile }))
        })
    })
    .await
    {
        Ok((_, body)) => {
            let plan = remembered.or_else(|| {
                let plan = claude_plan(&body["profile"])?;
                remember_claude_plan(identity.as_deref(), &plan);
                Some(plan)
            });
            parse_claude(identity, &body["usage"], plan)
        }
        Err(err) => ProviderReport::err(ProviderId::Claude, identity, err),
    }
}

/// 旧字段 five_hour / seven_day / seven_day_{opus,sonnet} 与新版 limits 数组
/// （kind = session / weekly_all / weekly_scoped）并存、正在迁移：旧字段优先，
/// 缺失时回落到 limits（与 omp 取法一致）。utilization / percent 都是 0–100 的已用百分比。
fn parse_claude(identity: Option<String>, usage: &Value, plan: Option<String>) -> ProviderReport {
    let limits = usage
        .get("limits")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let limit_of = |kind: &str| {
        limits
            .iter()
            .find(|item| item.get("kind").and_then(Value::as_str) == Some(kind))
    };
    let mut windows = Vec::new();
    for (id, label, legacy, kind) in [
        ("claude-5h", "5h window", "five_hour", "session"),
        ("claude-week", "Weekly", "seven_day", "weekly_all"),
    ] {
        if let Some(window) = claude_window(id, label, usage.get(legacy))
            .or_else(|| claude_window(id, label, limit_of(kind)))
        {
            windows.push(window);
        }
    }
    // 按模型单列的周额度（Max 套餐常见）：旧字段在前，limits 里同名模型不重复列出。
    let scoped = [
        ("Opus", usage.get("seven_day_opus")),
        ("Sonnet", usage.get("seven_day_sonnet")),
    ]
    .into_iter()
    .map(|(name, bucket)| (name.to_string(), bucket))
    .chain(
        limits
            .iter()
            .filter(|item| item.get("kind").and_then(Value::as_str) == Some("weekly_scoped"))
            .filter_map(|item| {
                let name = item.pointer("/scope/model/display_name")?.as_str()?.trim();
                (!name.is_empty()).then(|| (name.to_string(), Some(item)))
            }),
    );
    let mut seen: Vec<String> = Vec::new();
    for (name, bucket) in scoped {
        let key = name.to_ascii_lowercase();
        if seen.contains(&key) {
            continue;
        }
        let id = format!("claude-week-{key}");
        if let Some(window) = claude_window(&id, &format!("Weekly · {name}"), bucket) {
            seen.push(key);
            windows.push(window);
        }
    }
    if windows.is_empty() {
        return ProviderReport::err(ProviderId::Claude, identity, "no quota windows");
    }
    ProviderReport::ok(ProviderId::Claude, "Claude", identity, plan, windows)
}

fn claude_window(id: &str, label: &str, bucket: Option<&Value>) -> Option<QuotaWindow> {
    let bucket = bucket?;
    let used = number(bucket.get("utilization")).or_else(|| number(bucket.get("percent")))?;
    Some(QuotaWindow::from_used_percent(
        id,
        label,
        used,
        parse_iso(bucket.get("resets_at")),
    ))
}

/// 套餐取自 profile：rate_limit_tier 区分 Max 5x / 20x，organization_type 形如
/// claude_pro / claude_max。omp 凭据里的 orgName 是组织名（「xxx's Organization」），不是套餐。
fn claude_plan(profile: &Value) -> Option<String> {
    let org = profile.get("organization");
    let field = |key: &str| org.and_then(|org| org.get(key)).and_then(Value::as_str);
    let tier = field("rate_limit_tier").unwrap_or_default();
    if let Some((_, name)) = [("max_20x", "Max 20x"), ("max_5x", "Max 5x")]
        .into_iter()
        .find(|(marker, _)| tier.contains(marker))
    {
        return Some(name.to_string());
    }
    if let Some(kind) = field("organization_type").and_then(|kind| kind.strip_prefix("claude_")) {
        let mut chars = kind.chars();
        if let Some(first) = chars.next() {
            return Some(
                first
                    .to_uppercase()
                    .chain(chars)
                    .collect::<String>()
                    .replace('_', " "),
            );
        }
    }
    let flag = |key: &str| {
        profile
            .pointer(&format!("/account/{key}"))
            .and_then(Value::as_bool)
    };
    if flag("has_claude_max") == Some(true) {
        Some("Max".into())
    } else if flag("has_claude_pro") == Some(true) {
        Some("Pro".into())
    } else {
        None
    }
}

async fn fetch_kimi(client: &reqwest::Client, cred: StoredCred) -> ProviderReport {
    let identity = cred.identity.clone();
    match fetch_with_refresh(client, &cred, "kimi-code", |client, token| {
        Box::pin(get_json(
            client,
            "https://api.kimi.com/coding/v1/usages",
            bearer(token),
        ))
    })
    .await
    {
        Ok((_, body)) => parse_kimi(identity, body),
        Err(err) => ProviderReport::err(ProviderId::Kimi, identity, err),
    }
}

async fn fetch_cursor(client: &reqwest::Client, cred: StoredCred) -> ProviderReport {
    let identity = cred.identity.clone();
    match fetch_with_refresh(client, &cred, "cursor", |client, token| {
        Box::pin(post_json(
            client,
            "https://api2.cursor.sh/aiserver.v1.DashboardService/GetCurrentPeriodUsage",
            bearer(token),
        ))
    })
    .await
    {
        Ok((_, body)) => parse_cursor(identity, body),
        Err(err) => ProviderReport::err(ProviderId::Cursor, identity, err),
    }
}

fn parse_cursor(identity: Option<String>, body: Value) -> ProviderReport {
    let usage = body.get("planUsage").cloned().unwrap_or(Value::Null);
    let reset = millis_to_dt(number(body.get("billingCycleEnd")));
    let mut windows = Vec::new();
    for (id, label, key) in [
        ("cursor-api", "API / named models", "apiPercentUsed"),
        ("cursor-auto", "Auto models", "autoPercentUsed"),
        ("cursor-total", "Included total", "totalPercentUsed"),
    ] {
        if let Some(percent) = number(usage.get(key)) {
            windows.push(QuotaWindow::from_used_percent(id, label, percent, reset));
        }
    }
    if windows.is_empty() {
        return ProviderReport::err(ProviderId::Cursor, identity, "no plan usage data");
    }
    ProviderReport::ok(
        ProviderId::Cursor,
        "Cursor",
        identity.map(|raw| short_cursor_identity(&raw)),
        None,
        windows,
    )
}

fn short_cursor_identity(raw: &str) -> String {
    match raw.split_once('|') {
        Some((name, user)) if !name.is_empty() => {
            let short: String = user.chars().take(14).collect();
            format!("{name} · {short}…")
        }
        _ => raw.to_string(),
    }
}

fn parse_kimi(identity: Option<String>, body: Value) -> ProviderReport {
    let data = body.get("data").unwrap_or(&body);
    let mut windows = Vec::new();
    if let Some(limits) = data.get("limits").and_then(|v| v.as_array()) {
        for (idx, item) in limits.iter().enumerate() {
            let detail = item.get("detail").unwrap_or(item);
            let label = item
                .get("name")
                .or_else(|| detail.get("name"))
                .and_then(|v| v.as_str())
                .map(ToString::to_string)
                .unwrap_or_else(|| kimi_window_label(item, idx));
            if let Some(window) = usage_row(&format!("kimi-{idx}"), detail, &label) {
                windows.push(window);
            }
        }
    }
    if let Some(usage) = data.get("usage") {
        if let Some(window) = usage_row("kimi-usage", usage, "Total quota") {
            windows.push(window);
        }
    }
    if windows.is_empty() {
        return ProviderReport::err(ProviderId::Kimi, identity, "no quota windows");
    }
    ProviderReport::ok(ProviderId::Kimi, "Kimi Code", identity, None, windows)
}

fn kimi_window_label(item: &Value, idx: usize) -> String {
    let window = item.get("window").unwrap_or(item);
    let duration = number(window.get("duration")).unwrap_or(0.0);
    let unit = window
        .get("timeUnit")
        .or_else(|| item.get("timeUnit"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if unit.contains("MINUTE") && duration >= 60.0 {
        format!("{}h limit", (duration / 60.0) as i64)
    } else if unit.contains("HOUR") {
        format!("{}h limit", duration as i64)
    } else if unit.contains("DAY") {
        format!("{}d limit", duration as i64)
    } else {
        format!("Limit #{}", idx + 1)
    }
}

fn usage_row(id: &str, data: &Value, default_label: &str) -> Option<QuotaWindow> {
    let limit = number(data.get("limit"));
    let used = number(data.get("used")).or_else(|| {
        number(data.get("remaining"))
            .zip(limit)
            .map(|(remain, limit)| (limit - remain).max(0.0))
    });
    let reset = parse_reset(data);
    let label = data
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or(default_label);
    match (used, limit) {
        (Some(used), Some(limit)) => Some(QuotaWindow::from_used_limit(
            id, label, used, limit, "count", reset,
        )),
        (Some(used), None) => Some(QuotaWindow::from_used_percent(id, label, used, reset)),
        _ => None,
    }
}

async fn fetch_grok(client: &reqwest::Client, cred: StoredCred) -> ProviderReport {
    let identity = cred.identity.clone();
    match fetch_with_refresh(client, &cred, "xai-oauth", |client, token| {
        let mut headers = bearer(token);
        headers.insert(
            "x-grok-client-surface",
            HeaderValue::from_static("grok-build"),
        );
        headers.insert("x-grok-client-version", HeaderValue::from_static("1.0.0"));
        Box::pin(get_json(
            client,
            "https://cli-chat-proxy.grok.com/v1/billing?format=credits",
            headers,
        ))
    })
    .await
    {
        Ok((_, body)) => parse_grok(identity, body),
        Err(err) => ProviderReport::err(ProviderId::Grok, identity, err),
    }
}

fn parse_grok(identity: Option<String>, body: Value) -> ProviderReport {
    let config = body.get("config").cloned().unwrap_or(Value::Null);
    let period = config.get("currentPeriod").cloned().unwrap_or(Value::Null);
    let used_percent = number(config.get("creditUsagePercent")).unwrap_or(0.0);
    let reset = parse_iso(period.get("end")).or_else(|| parse_iso(config.get("billingPeriodEnd")));
    let kind = period
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_ascii_uppercase();
    let label = if kind.contains("WEEK") {
        "Weekly credits"
    } else if kind.contains("MONTH") {
        "Monthly credits"
    } else {
        "Period credits"
    };
    ProviderReport::ok(
        ProviderId::Grok,
        "xAI Grok",
        identity,
        None,
        vec![QuotaWindow::from_used_percent(
            "grok-credits",
            label,
            used_percent,
            reset,
        )],
    )
}

async fn fetch_codex(client: &reqwest::Client, cred: StoredCred) -> ProviderReport {
    let plan = cred.plan.clone();
    let account_fallback = cred.account_id.clone();
    let result = fetch_with_refresh(client, &cred, "openai-codex", move |client, token| {
        let mut headers = bearer(token);
        if let Some(account_id) =
            credentials::chatgpt_account_id(token, account_fallback.as_deref())
        {
            if let Ok(value) = HeaderValue::from_str(&account_id) {
                headers.insert("ChatGPT-Account-Id", value);
            }
        }
        Box::pin(get_json(
            client,
            "https://chatgpt.com/backend-api/wham/usage",
            headers,
        ))
    })
    .await;
    match result {
        Ok((token, body)) => {
            let identity = credentials::jwt_email(&token).or(cred.identity);
            parse_codex(identity, plan, body)
        }
        Err(err) => ProviderReport::err(ProviderId::Codex, cred.identity, err),
    }
}

fn parse_codex(identity: Option<String>, plan: Option<String>, body: Value) -> ProviderReport {
    let plan_label = body
        .get("plan_type")
        .and_then(|v| v.as_str())
        .map(ToString::to_string)
        .or(plan);
    let mut windows = Vec::new();
    if let Some(rate) = body.get("rate_limit") {
        push_codex_window(&mut windows, rate.get("primary_window"), false);
        push_codex_window(&mut windows, rate.get("secondary_window"), false);
    }
    if let Some(code_review) = body.pointer("/code_review_rate_limit/primary_window") {
        push_codex_window(&mut windows, Some(code_review), true);
    }
    if windows.is_empty() {
        return ProviderReport::err(ProviderId::Codex, identity, "no quota windows");
    }
    let mut report = ProviderReport::ok(
        ProviderId::Codex,
        "OpenAI Codex",
        identity,
        plan_label,
        windows,
    );
    report.resets_left =
        number(body.pointer("/rate_limit_reset_credits/available_count")).map(|value| value as i64);
    report
}

fn push_codex_window(windows: &mut Vec<QuotaWindow>, raw: Option<&Value>, review: bool) {
    let Some(window) = raw else { return };
    let Some(used_percent) = number(window.get("used_percent")) else {
        return;
    };
    let seconds = number(window.get("limit_window_seconds")).unwrap_or(0.0) as i64;
    let (mut id, mut label) = match seconds {
        18000 => ("codex-5h", "5h"),
        604800 => ("codex-7d", "7 days"),
        2628000 => ("codex-month", "Monthly"),
        _ => ("codex-window", "Window"),
    };
    if review {
        id = "codex-review";
        label = "Code review";
    } else if seconds == 604800 && windows.iter().any(|item| item.id == "codex-7d") {
        id = "codex-spark";
        label = "7 days (Spark)";
    }
    let reset = unix_to_dt(number(window.get("reset_at")))
        .or_else(|| seconds_from_now(number(window.get("reset_after_seconds"))));
    windows.push(QuotaWindow::from_used_percent(
        id,
        label,
        used_percent,
        reset,
    ));
}

async fn get_json(
    client: &reqwest::Client,
    url: &str,
    headers: HeaderMap,
) -> Result<Value, String> {
    let response = client
        .get(url)
        .headers(headers)
        .send()
        .await
        .map_err(|err| brief(&err))?;
    let status = response.status();
    let text = response.text().await.map_err(|err| brief(&err))?;
    if !status.is_success() {
        return Err(http_error(status, &text));
    }
    serde_json::from_str(&text).map_err(|_| "invalid JSON".to_string())
}

async fn post_json(
    client: &reqwest::Client,
    url: &str,
    mut headers: HeaderMap,
) -> Result<Value, String> {
    headers.insert("Content-Type", HeaderValue::from_static("application/json"));
    let response = client
        .post(url)
        .headers(headers)
        .body("{}")
        .send()
        .await
        .map_err(|err| brief(&err))?;
    let status = response.status();
    let text = response.text().await.map_err(|err| brief(&err))?;
    if !status.is_success() {
        return Err(http_error(status, &text));
    }
    serde_json::from_str(&text).map_err(|_| "invalid JSON".to_string())
}

fn raw_auth(key: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_static(UA));
    if let Ok(value) = HeaderValue::from_str(key) {
        headers.insert(AUTHORIZATION, value);
    }
    headers.insert("Content-Type", HeaderValue::from_static("application/json"));
    headers
}

fn bearer(token: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_static(UA));
    if let Ok(value) = HeaderValue::from_str(&format!("Bearer {token}")) {
        headers.insert(AUTHORIZATION, value);
    }
    headers.insert("Accept", HeaderValue::from_static("application/json"));
    headers
}

/// 与 Claude Code / omp 一致带上 OAuth beta 头：目前服务端不强制，防日后收紧。
fn claude_headers(token: &str) -> HeaderMap {
    let mut headers = bearer(token);
    headers.insert(
        "anthropic-beta",
        HeaderValue::from_static("oauth-2025-04-20"),
    );
    headers
}

fn number(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn millis_to_dt(ms: Option<f64>) -> Option<DateTime<Utc>> {
    let ms = ms?;
    if !ms.is_finite() || ms <= 0.0 {
        return None;
    }
    Utc.timestamp_millis_opt(ms as i64).single()
}

fn unix_to_dt(seconds: Option<f64>) -> Option<DateTime<Utc>> {
    let seconds = seconds?;
    if !seconds.is_finite() || seconds <= 0.0 {
        return None;
    }
    Utc.timestamp_opt(seconds as i64, 0).single()
}

fn seconds_from_now(seconds: Option<f64>) -> Option<DateTime<Utc>> {
    let seconds = seconds?;
    if !seconds.is_finite() || seconds <= 0.0 {
        return None;
    }
    Some(Utc::now() + chrono::Duration::seconds(seconds as i64))
}

fn parse_iso(value: Option<&Value>) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value?.as_str()?)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

fn parse_reset(data: &Value) -> Option<DateTime<Utc>> {
    for key in ["reset_at", "resetAt", "reset_time", "resetTime"] {
        if let Some(Value::String(raw)) = data.get(key) {
            if let Ok(dt) = DateTime::parse_from_rfc3339(raw) {
                return Some(dt.with_timezone(&Utc));
            }
        }
        if let Some(dt) =
            unix_to_dt(number(data.get(key))).or_else(|| millis_to_dt(number(data.get(key))))
        {
            return Some(dt);
        }
    }
    for key in ["reset_in", "resetIn", "ttl"] {
        if let Some(dt) = seconds_from_now(number(data.get(key))) {
            return Some(dt);
        }
    }
    None
}

/// 网络层错误压成一句短话：挂件一行放不下带完整 URL 的 reqwest 报错。
fn brief(err: &reqwest::Error) -> String {
    if err.is_timeout() {
        "请求超时".to_string()
    } else if err.is_connect() {
        "连接失败".to_string()
    } else {
        // 其余错误去掉 Display 末尾附带的 " for url (...)"，正文照旧（已脱敏）
        let text = err.to_string();
        match text.find(" for url ") {
            Some(idx) => sanitize(&text[..idx]),
            None => sanitize(&text),
        }
    }
}

fn snippet(text: &str) -> String {
    let flat: String = text.chars().filter(|c| !c.is_control()).take(120).collect();
    sanitize(&flat)
}

/// HTTP 错误压成一行。429 的响应体（各家都是一大段 JSON）对用户没有信息量，
/// 原样塞进挂件会把窗口撑到最大宽度，只说限流即可。
fn http_error(status: reqwest::StatusCode, text: &str) -> String {
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        "HTTP 429 请求过于频繁，稍后自动重试".to_string()
    } else {
        format!("HTTP {status}: {}", snippet(text))
    }
}

fn sanitize(text: &str) -> String {
    let mut out = text.to_string();
    for key in ["Bearer ", "eyJ"] {
        if let Some(idx) = out.find(key) {
            out.replace_range(idx.., "[redacted]");
        }
    }
    out
}

/// Devin CLI 没有公开额度 API：横幅里那行「Pro · 9% remaining (resets in 2d 4h)」
/// 是它启动时自己调 GetUserStatus 拿到的实时数据。这里无窗口拉起 devin.exe、
/// 从 stdout 抓这一行，抓到即杀进程。
async fn maybe_devin(only: Option<ProviderId>, skip: &[ProviderId]) -> Option<ProviderReport> {
    if only.is_some_and(|wanted| wanted != ProviderId::Devin) || skip.contains(&ProviderId::Devin) {
        return None;
    }
    // 横幅连续抓不到时退避：省掉每轮最长 25s 的 devin.exe 拉起
    if let Some(report) = deferred_report(ProviderId::Devin, None) {
        return Some(report);
    }
    // fetch_devin 是同步阻塞（等横幅最长 25s）：在 tokio::join! 里直接调用会
    // 冻结同任务的其他 provider，全部跟着超时。挪到 blocking 线程池。
    let report = tokio::task::spawn_blocking(fetch_devin)
        .await
        .unwrap_or_else(|_| ProviderReport::err(ProviderId::Devin, None, "devin 刷新线程异常"));
    if let Ok(mut state) = backoff().lock() {
        if report.error.is_none() {
            state.succeed(ProviderId::Devin);
        } else {
            state.fail(ProviderId::Devin);
        }
    }
    Some(report)
}

fn devin_executable() -> Option<std::path::PathBuf> {
    let local = std::env::var_os("LOCALAPPDATA")?;
    let path = std::path::PathBuf::from(local)
        .join("devin")
        .join("cli")
        .join("bin")
        .join("devin.exe");
    path.is_file().then_some(path)
}

fn fetch_devin() -> ProviderReport {
    // 横幅是实时数据（日/周中更紧的那个，见 merge_devin_banner）。实测 devin
    // 运行中并行拉起互不影响（用户日常就高频多开；清理用 PID 树精确终止，不留孤儿）。
    // 横幅失败再退回 user_status 缓存兜底。
    let report = fetch_devin_banner();
    if report.error.is_some() {
        if let Some(cached) = parse_devin_cache() {
            return cached;
        }
    }
    report
}

fn fetch_devin_banner() -> ProviderReport {
    let Some(exe) = devin_executable() else {
        return ProviderReport::missing(ProviderId::Devin);
    };
    // 横幅只在拥有真实控制台时渲染：CREATE_NO_WINDOW 下 devin 直接不画 TUI。
    // conhost --headless 给它一个隐形控制台；cwd 用已信任目录，避免目录信任弹窗。
    let mut command = credentials::hidden_command("conhost.exe");
    command
        .arg("--headless")
        .arg("--")
        .arg(&exe)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    if let Some(trusted) = devin_trusted_cwd() {
        command.current_dir(trusted);
    }
    let Ok(mut child) = command.spawn() else {
        return ProviderReport::err(ProviderId::Devin, None, "无法启动 devin.exe");
    };
    let mut stdout = child.stdout.take();
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        use std::io::Read;
        let Some(stream) = stdout.as_mut() else {
            return;
        };
        let mut chunk = [0u8; 8192];
        loop {
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx
                        .send(String::from_utf8_lossy(&chunk[..n]).into_owned())
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
    });

    let deadline = std::time::Instant::now() + Duration::from_secs(25);
    let mut text = String::new();
    let mut banner = None;
    while std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(500));
        while let Ok(part) = rx.try_recv() {
            text.push_str(&part);
        }
        let clean = strip_ansi(&text);
        if let Some(found) = parse_devin_banner(&clean) {
            banner = Some(found);
            break;
        }
    }
    // conhost 只是壳，child.kill() 杀不掉里面的 devin.exe；孤儿 devin 会持有
    // session 锁。只终止自己拉起的这棵进程树，绝不按进程名杀别人的 devin。
    let _ = credentials::hidden_command("taskkill")
        .args(["/PID", &child.id().to_string(), "/T", "/F"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    let _ = child.wait();

    match banner {
        Some((plan, remaining, reset_in)) => {
            let now = Utc::now();
            let reset_at = reset_in
                .as_deref()
                .and_then(parse_devin_reset)
                .map(|secs| now + chrono::Duration::seconds(secs));
            // 缓存只在同账号（同套餐名）时可用
            let cached = parse_devin_cache()
                .filter(|cached| cached.plan.as_deref() == Some(plan.as_str()))
                .map(|cached| cached.windows)
                .unwrap_or_default();
            let windows = merge_devin_banner(100.0 - remaining, reset_at, &cached, now);
            ProviderReport::ok(ProviderId::Devin, "Devin", None, Some(plan), windows)
        }
        None => ProviderReport::err(ProviderId::Devin, None, "未从 devin 横幅捕获到额度"),
    }
}

/// devin 横幅只显示日/周额度里「更紧」的那一个：09-18 实测「9% · resets in 2d 4h」是
/// 周额度（当时日额度 100%），09-23「15% · resets in 5h 17m」是日额度（周额度 58%）。
/// 按重置时间认领：超过 1 天才重置的只能是周额度；与缓存里周额度的重置时刻（整周不变）
/// 对得上的是周额度，否则是日额度。每周最后一天日/周同一时刻重置、两者都对得上时，
/// 取缓存里用得更多的那个。横幅值是实时的，另一个额度只能取缓存（真实 devin 会话
/// 启动时才刷新）。
fn merge_devin_banner(
    used_percent: f64,
    reset_at: Option<DateTime<Utc>>,
    cached: &[QuotaWindow],
    now: DateTime<Utc>,
) -> Vec<QuotaWindow> {
    // 横幅超过 1 天时只精确到小时（「2d 4h」）；日/周重置时刻要么相同要么相差整天，
    // 2 小时余量足够区分。
    const TOLERANCE_SECS: i64 = 2 * 3600;
    let daily = cached.iter().find(|window| window.id == "daily");
    let weekly = cached.iter().find(|window| window.id == "weekly");
    let matches = |window: Option<&QuotaWindow>| match (reset_at, window.and_then(|w| w.reset_at)) {
        (Some(banner), Some(cached)) => (banner - cached).num_seconds().abs() <= TOLERANCE_SECS,
        _ => false,
    };
    let banner_is_weekly = match reset_at {
        None => None,
        Some(at) if (at - now).num_seconds() > 24 * 3600 + TOLERANCE_SECS => Some(true),
        Some(_) => match (matches(weekly), matches(daily)) {
            (true, true) => weekly
                .zip(daily)
                .map(|(weekly, daily)| weekly.used_fraction >= daily.used_fraction),
            (true, false) => Some(true),
            (false, _) if !cached.is_empty() => Some(false),
            (false, _) => None,
        },
    };
    let banner =
        |id: &str, label: &str| QuotaWindow::from_used_percent(id, label, used_percent, reset_at);
    match banner_is_weekly {
        Some(true) => daily
            .cloned()
            .into_iter()
            .chain([banner("weekly", "Weekly")])
            .collect(),
        Some(false) => [banner("daily", "Daily")]
            .into_iter()
            .chain(weekly.cloned())
            .collect(),
        // 认不出来（无缓存且 1 天内重置，或横幅没给重置时间）：有缓存用缓存，
        // 否则如实标成「当前额度」，不硬套日/周。
        None if !cached.is_empty() => cached.to_vec(),
        None => vec![banner("current", "Current limit")],
    }
}

/// 从 user_status 缓存读额度。缓存是 JSON 包 base64 的 protobuf，f13 内关键字段：
/// f1.2=套餐名，f14=日额度剩余%，f15=周额度剩余%，
/// f17=日额度重置锚点（未消费时停留在过去，不展示），f18=周额度重置时间戳。
/// 注意：缓存只在真实 devin 进程启动时刷新，长跑任务期间会滞后。
fn parse_devin_cache() -> Option<ProviderReport> {
    parse_devin_cache_inner()
}

fn parse_devin_cache_inner() -> Option<ProviderReport> {
    let local = std::env::var_os("LOCALAPPDATA")?;
    let dir = std::path::PathBuf::from(local).join("devin").join("cli");
    let newest = std::fs::read_dir(&dir)
        .ok()?
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("user_status.")
        })
        .max_by_key(|entry| entry.metadata().ok().and_then(|m| m.modified().ok()))?;
    let raw = std::fs::read_to_string(newest.path()).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let payload = base64_decode(value.get("payload")?.as_str()?)?;
    let quota = proto_field(&payload, 13)?;
    let plan = proto_field(&quota, 1)
        .and_then(|plan_msg| proto_field(&plan_msg, 2))
        .and_then(|bytes| proto_string(&bytes))
        .unwrap_or_else(|| "Unknown".into());
    let weekly = proto_varint_field(&quota, 15).unwrap_or(0);
    let weekly_reset = proto_varint_field(&quota, 18)
        .filter(|secs| *secs > 1_600_000_000)
        .and_then(|secs| chrono::DateTime::from_timestamp(secs, 0));
    if !(0..=100).contains(&weekly) {
        return None;
    }
    let mut windows = Vec::new();
    // proto3 省略零值字段：额度耗尽时 f14/f15 干脆不出现，按 0% 剩余处理，
    // 否则用尽期间日/周额度行会整行消失。
    let daily = proto_varint_field(&quota, 14).unwrap_or(0);
    if (0..=100).contains(&daily) {
        let daily_reset = proto_varint_field(&quota, 17)
            .filter(|secs| *secs > Utc::now().timestamp())
            .and_then(|secs| chrono::DateTime::from_timestamp(secs, 0));
        windows.push(QuotaWindow::from_used_percent(
            "daily",
            "Daily",
            100.0 - daily as f64,
            daily_reset,
        ));
    }
    windows.push(QuotaWindow::from_used_percent(
        "weekly",
        "Weekly",
        100.0 - weekly as f64,
        weekly_reset,
    ));
    Some(ProviderReport::ok(
        ProviderId::Devin,
        "Devin",
        None,
        Some(plan),
        windows,
    ))
}

/// 取 protobuf 消息里指定字段号的原始字节（wire type 2）。
fn proto_field(data: &[u8], want: u64) -> Option<Vec<u8>> {
    let mut i = 0;
    while i < data.len() {
        let (tag, next) = proto_varint_at(data, i)?;
        i = next;
        let field = tag >> 3;
        match tag & 7 {
            0 => {
                let (_, next) = proto_varint_at(data, i)?;
                i = next;
            }
            1 => i += 8,
            5 => i += 4,
            2 => {
                let (len, next) = proto_varint_at(data, i)?;
                i = next;
                let end = i.checked_add(len as usize)?;
                if field == want {
                    return data.get(i..end).map(|slice| slice.to_vec());
                }
                i = end;
            }
            _ => return None,
        }
    }
    None
}

/// 取 varint 字段（wire 0）的数值。proto_field 只返回字节串字段。
fn proto_varint_field(data: &[u8], want: u64) -> Option<i64> {
    let mut i = 0;
    while i < data.len() {
        let (tag, next) = proto_varint_at(data, i)?;
        i = next;
        let field = tag >> 3;
        match tag & 7 {
            0 => {
                let (value, next) = proto_varint_at(data, i)?;
                i = next;
                if field == want {
                    return Some(value as i64);
                }
            }
            1 => i += 8,
            5 => i += 4,
            2 => {
                let (len, next) = proto_varint_at(data, i)?;
                i = next;
                let end = i.checked_add(len as usize)?;
                if end > data.len() {
                    return None;
                }
                i = end;
            }
            _ => return None,
        }
    }
    None
}

fn proto_varint_at(data: &[u8], start: usize) -> Option<(u64, usize)> {
    let mut value = 0_u64;
    let mut shift = 0;
    let mut i = start;
    while i < data.len() {
        let byte = data[i];
        i += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some((value, i));
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
    None
}

fn proto_string(data: &[u8]) -> Option<String> {
    Some(String::from_utf8_lossy(data).into_owned())
}

/// base64 标准字母表解码（payload 是无 padding 的概率极低，容错处理）。
fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let mut buffer = 0_u32;
    let mut bits = 0;
    for ch in text.chars() {
        let digit = match ch {
            'A'..='Z' => ch as u32 - 'A' as u32,
            'a'..='z' => ch as u32 - 'a' as u32 + 26,
            '0'..='9' => ch as u32 - '0' as u32 + 52,
            '+' => 62,
            '/' => 63,
            _ => continue,
        };
        buffer = (buffer << 6) | digit;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    Some(out)
}

/// 从 trusted_workspaces.json 取第一个存在的已信任目录，作为 devin 的工作目录。
fn devin_trusted_cwd() -> Option<std::path::PathBuf> {
    let appdata = std::env::var_os("APPDATA")?;
    let path = std::path::PathBuf::from(appdata)
        .join("devin")
        .join("cli")
        .join("trusted_workspaces.json");
    let raw = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    value
        .get("trusted_paths")?
        .as_array()?
        .iter()
        .filter_map(|item| item.as_str())
        .map(std::path::PathBuf::from)
        .find(|dir| dir.is_dir())
}

/// 去掉 CSI/OSC 转义序列，TUI 横幅行里混着颜色码。
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\x1b' {
            out.push(ch);
            continue;
        }
        match chars.peek() {
            Some('[') => {
                chars.next();
                for inner in chars.by_ref() {
                    if inner.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            Some(']') => {
                chars.next();
                for inner in chars.by_ref() {
                    if inner == '\x07' {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// 匹配 `Pro · 9% remaining (resets in 2d 4h)`，返回 (套餐, 剩余百分比, 剩余时间原文)。
fn parse_devin_banner(text: &str) -> Option<(String, f64, Option<String>)> {
    for line in text.lines() {
        let line = line.trim();
        let Some((head, tail)) = line.split_once('·') else {
            continue;
        };
        let Some(rest) = tail.trim().strip_suffix(')') else {
            continue;
        };
        let Some((pct_part, reset_part)) = rest.split_once(" (resets in ") else {
            continue;
        };
        let Some(pct) = pct_part
            .trim()
            .strip_suffix("% remaining")
            .and_then(|raw| raw.trim().parse::<f64>().ok())
        else {
            continue;
        };
        let plan = head.trim();
        if plan.is_empty() {
            continue;
        }
        // 无头 conhost 会把 TUI 提示挤进横幅同一行：模型选择器
        // 「SWE-2 Max … Press alt+m to switch between available models」、
        // 「clipboard」图标文字、「Unsupported terminal」警告等，且常与套餐名
        // 无空格粘连（clipboardPro）。套餐名取行尾已知套餐词的后缀匹配。
        let plan = plan
            .rsplit_once("available models")
            .map(|(_, tail)| tail.trim())
            .unwrap_or(plan);
        let plan = plan
            .rsplit_once("best experience")
            .map(|(_, tail)| tail.trim())
            .unwrap_or(plan);
        let plan = ["Enterprise", "Team", "Free", "Pro", "Max", "Core"]
            .into_iter()
            .find(|name| plan.ends_with(name))
            .unwrap_or_else(|| plan.split_whitespace().last().unwrap_or(plan));
        let reset = reset_part.trim();
        return Some((
            plan.to_string(),
            pct,
            (!reset.is_empty()).then(|| reset.to_string()),
        ));
    }
    None
}

/// 「2d 4h」「3h」「45m」→ 秒。
fn parse_devin_reset(text: &str) -> Option<i64> {
    let mut seconds = 0_i64;
    let mut matched = false;
    for token in text.split_whitespace() {
        let (value, unit) = token.split_at(token.len().saturating_sub(1));
        let value: i64 = value.parse().ok()?;
        let unit = unit.to_ascii_lowercase();
        seconds += match unit.as_str() {
            "d" => value * 86_400,
            "h" => value * 3_600,
            "m" => value * 60,
            "s" => value,
            _ => return None,
        };
        matched = true;
    }
    matched.then_some(seconds)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Anthropic 正把额度从 five_hour / seven_day 迁到 limits 数组：旧字段为 null 时
    /// 必须回落到 limits，否则整张卡片退化成「no quota windows」。
    #[test]
    fn claude_usage_falls_back_to_limits_array() {
        let usage = serde_json::json!({
            "five_hour": null,
            "seven_day": null,
            "seven_day_opus": null,
            "limits": [
                {"kind": "session", "percent": 40, "resets_at": "2026-09-23T06:59:59.752087+00:00"},
                {"kind": "weekly_all", "percent": 10, "resets_at": "2026-09-28T03:59:59+00:00"},
                {"kind": "weekly_scoped", "percent": 70, "resets_at": null,
                 "scope": {"model": {"display_name": "Opus"}}}
            ]
        });
        let profile = serde_json::json!({
            "organization": {
                "organization_type": "claude_max",
                "rate_limit_tier": "default_claude_max_20x"
            }
        });
        let report = parse_claude(None, &usage, claude_plan(&profile));
        assert!(report.error.is_none(), "{:?}", report.error);
        assert_eq!(report.plan.as_deref(), Some("Max 20x"));
        let rows: Vec<(&str, f64)> = report
            .windows
            .iter()
            .map(|window| (window.label.as_str(), window.used_fraction))
            .collect();
        assert_eq!(
            rows,
            [("5h window", 0.4), ("Weekly", 0.1), ("Weekly · Opus", 0.7)]
        );
        assert!(report.windows[0].reset_at.is_some());
    }

    /// 无头 conhost 把模型选择器、按键提示、clipboard 图标文字挤进横幅同一行，
    /// 且与套餐名无空格粘连（两行均取自实际抓到的横幅）；套餐名只能剩套餐词。
    #[test]
    fn devin_banner_plan_ignores_tui_noise() {
        for line in [
            "SWE-2 Max        Press alt+m to switch between available modelsPro · 0% remaining (resets in 5h)",
            "clipboardPro · 0% remaining (resets in 4h 30m)",
        ] {
            let (plan, pct, reset) = parse_devin_banner(line).expect("banner line must parse");
            assert_eq!(plan, "Pro", "{line}");
            assert_eq!(pct, 0.0);
            assert!(reset.is_some());
        }
    }

    /// 横幅只显示日/周里更紧的那个额度，必须按重置时间认领，不能一律当周额度
    /// （09-23 实测：横幅「15% · resets in 5h 17m」是日额度，周额度实为 58%）。
    #[test]
    fn devin_banner_is_attributed_by_reset_time() {
        let now = Utc.with_ymd_and_hms(2026, 9, 23, 2, 42, 0).unwrap();
        let at =
            |hours: i64, minutes: i64| Some(now + chrono::Duration::minutes(hours * 60 + minutes));
        let cached = |id: &str, remaining: f64, reset: Option<DateTime<Utc>>| {
            QuotaWindow::from_used_percent(id, id, 100.0 - remaining, reset)
        };
        let summary = |windows: Vec<QuotaWindow>| -> Vec<String> {
            windows
                .iter()
                .map(|w| format!("{}={:.0}", w.id, (1.0 - w.used_fraction) * 100.0))
                .collect()
        };

        // 日额度更紧：横幅归日额度，周额度取缓存
        let cache = [
            cached("daily", 20.0, at(5, 18)),
            cached("weekly", 58.0, at(101, 18)),
        ];
        let merged = merge_devin_banner(85.0, at(5, 17), &cache, now);
        assert_eq!(summary(merged), ["daily=15", "weekly=58"]);

        // 周最后一天且周额度更紧（日额度未动用，缓存里没有日重置时间）：横幅归周额度
        let cache = [
            cached("daily", 100.0, None),
            cached("weekly", 12.0, at(5, 10)),
        ];
        let merged = merge_devin_banner(91.0, at(5, 0), &cache, now);
        assert_eq!(summary(merged), ["daily=100", "weekly=9"]);

        // 日/周同一时刻重置：横幅是缓存里用得更多的那个
        let cache = [
            cached("daily", 60.0, at(3, 0)),
            cached("weekly", 25.0, at(3, 0)),
        ];
        let merged = merge_devin_banner(80.0, at(3, 0), &cache, now);
        assert_eq!(summary(merged), ["daily=60", "weekly=20"]);

        // 无缓存且 1 天内重置：认不出日/周，如实标成当前额度
        let merged = merge_devin_banner(50.0, at(3, 0), &[], now);
        assert_eq!(summary(merged), ["current=50"]);
    }
}
