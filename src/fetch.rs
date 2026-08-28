use crate::credentials::{self, CredentialSet, StoredCred};
use crate::model::{ProviderId, ProviderReport, QuotaWindow, Snapshot};
use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, USER_AGENT};
use serde_json::Value;
use std::collections::HashMap;
use std::future::Future;
use std::time::Duration;

const UA: &str = "coding-quota/0.1";
const TIMEOUT: Duration = Duration::from_secs(20);
/// 周额度周期（额度百分比 + 重置时间）。注意它返回的 billingPeriodEnd 与
/// currentPeriod.end 完全相等，是额度重置时间，**不是**订阅到期日。
const GROK_USAGE_URL: &str = "https://cli-chat-proxy.grok.com/v1/billing?format=credits";
/// 月度账单周期：不带 format 参数时返回真实账单周期（如 08-01 → 09-01），
/// 周期末即订阅续费/到期日。与上面是同一个路径但载荷不同。
const GROK_BILLING_URL: &str = "https://cli-chat-proxy.grok.com/v1/billing";

pub async fn fetch_all(creds: &CredentialSet, only: Option<ProviderId>) -> Snapshot {
    let client = match reqwest::Client::builder().timeout(TIMEOUT).build() {
        Ok(client) => client,
        Err(err) => {
            let reports = [ProviderId::Codex, ProviderId::Grok, ProviderId::Glm, ProviderId::Kimi, ProviderId::Cursor]
                .into_iter()
                .filter(|provider| only.is_none_or(|wanted| wanted == *provider))
                .map(|provider| ProviderReport::err(provider, None, format!("http client: {err}")))
                .collect();
            return Snapshot {
                fetched_at: Utc::now(),
                reports,
            };
        }
    };

    let (codex, grok, glm, kimi, cursor) = tokio::join!(
        maybe_fetch(&client, ProviderId::Codex, creds.codex.clone(), only),
        maybe_fetch(&client, ProviderId::Grok, creds.grok.clone(), only),
        maybe_fetch(&client, ProviderId::Glm, creds.glm.clone(), only),
        maybe_fetch(&client, ProviderId::Kimi, creds.kimi.clone(), only),
        maybe_fetch(&client, ProviderId::Cursor, creds.cursor.clone(), only),
    );

    let manual = manual_expiries();
    Snapshot {
        fetched_at: Utc::now(),
        reports: [codex, grok, glm, kimi, cursor]
            .into_iter()
            .flatten()
            .map(|mut report| {
                if report.expires_at.is_none() {
                    if let Some(expiry) = manual.get(report.provider.key()) {
                        report.expires_at = Some(*expiry);
                    }
                }
                report
            })
            .collect(),
    }
}

async fn maybe_fetch(
    client: &reqwest::Client,
    provider: ProviderId,
    cred: Option<StoredCred>,
    only: Option<ProviderId>,
) -> Option<ProviderReport> {
    if only.is_some_and(|wanted| wanted != provider) {
        return None;
    }
    Some(match cred {
        Some(cred) => match provider {
            ProviderId::Codex => fetch_codex(client, cred).await,
            ProviderId::Grok => fetch_grok(client, cred).await,
            ProviderId::Glm => fetch_glm(client, cred).await,
            ProviderId::Kimi => fetch_kimi(client, cred).await,
            ProviderId::Cursor => fetch_cursor(client, cred).await,
        },
        None => ProviderReport::missing(provider),
    })
}

type FetchFuture<'a> =
    std::pin::Pin<Box<dyn Future<Output = Result<Value, String>> + Send + 'a>>;

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
            Ok(body) => {
                let mut report = parse_glm(cred.identity.clone(), body);
                if let Some((expires_at, product)) = fetch_glm_subscription(client, &key).await {
                    report.expires_at = Some(expires_at);
                    if let Some(product) = product {
                        report.plan = Some(product);
                    }
                }
                return report;
            }
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
            windows.push(QuotaWindow::from_used_percent(id, label, used_percent, reset));
        }
    }
    if windows.is_empty() {
        return ProviderReport::err(ProviderId::Glm, identity, "no quota windows");
    }
    ProviderReport::ok(ProviderId::Glm, "Zhipu Coding Plan", identity, plan, windows)
}

fn glm_count_window(id: &str, label: &str, limit: &Value, reset: Option<DateTime<Utc>>) -> Option<QuotaWindow> {
    let used = number(limit.get("currentValue")).or_else(|| number(limit.get("usage")))?;
    let total = number(limit.get("number")).or_else(|| {
        number(limit.get("remaining")).map(|remain| used + remain)
    })?;
    if total > 1.0 {
        Some(QuotaWindow::from_used_limit(id, label, used, total, "count", reset))
    } else {
        None
    }
}

/// GLM 订阅信息（套餐购买记录）：到期日与产品名。任何失败都静默降级。
async fn fetch_glm_subscription(
    client: &reqwest::Client,
    key: &str,
) -> Option<(DateTime<Utc>, Option<String>)> {
    let body = get_json(client, "https://open.bigmodel.cn/api/biz/subscription/list", raw_auth(key))
        .await
        .ok()?;
    let entries = body.get("data")?.as_array()?;
    let entry = entries
        .iter()
        .find(|entry| entry.get("status").and_then(|v| v.as_str()) == Some("VALID"))?;
    let renew = entry.get("nextRenewTime")?.as_str()?;
    let date = NaiveDate::parse_from_str(renew.trim(), "%Y-%m-%d").ok()?;
    let expires_at = date.and_hms_opt(12, 0, 0)?.and_utc();
    let product = entry
        .get("productName")
        .and_then(|v| v.as_str())
        .map(ToString::to_string);
    Some((expires_at, product))
}

/// 手工维护的套餐到期日：%APPDATA%\coding-quota\plan_expiry.json，
/// 形如 {"codex": "2026-08-30"}。用于没有到期 API 的平台（Codex/Kimi）。
fn manual_expiries() -> HashMap<&'static str, DateTime<Utc>> {
    let mut map = HashMap::new();
    let Some(appdata) = std::env::var_os("APPDATA") else {
        return map;
    };
    let path = std::path::PathBuf::from(appdata)
        .join("coding-quota")
        .join("plan_expiry.json");
    let Ok(raw) = std::fs::read_to_string(path) else {
        return map;
    };
    let Ok(value) = serde_json::from_str::<HashMap<String, String>>(&raw) else {
        return map;
    };
    for (key, date) in value {
        let Some(provider) = ProviderId::parse_filter(&key) else {
            continue;
        };
        if let Ok(date) = NaiveDate::parse_from_str(date.trim(), "%Y-%m-%d") {
            if let Some(midday) = date.and_hms_opt(12, 0, 0) {
                map.insert(provider.key(), midday.and_utc());
            }
        }
    }
    map
}

async fn fetch_kimi(client: &reqwest::Client, cred: StoredCred) -> ProviderReport {
    let identity = cred.identity.clone();
    match fetch_with_refresh(client, &cred, "kimi-code", |client, token| {
        Box::pin(get_json(client, "https://api.kimi.com/coding/v1/usages", bearer(token)))
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
    let mut report = ProviderReport::ok(
        ProviderId::Cursor,
        "Cursor",
        identity.map(|raw| short_cursor_identity(&raw)),
        None,
        windows,
    );
    report.expires_at = reset;
    report
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
    if let Some(usage) = data.get("usage") {
        if let Some(window) = usage_row("kimi-usage", usage, "Total quota") {
            windows.push(window);
        }
    }
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
        number(data.get("remaining")).zip(limit).map(|(remain, limit)| (limit - remain).max(0.0))
    });
    let reset = parse_reset(data);
    let label = data.get("name").and_then(|v| v.as_str()).unwrap_or(default_label);
    match (used, limit) {
        (Some(used), Some(limit)) => Some(QuotaWindow::from_used_limit(id, label, used, limit, "count", reset)),
        (Some(used), None) => Some(QuotaWindow::from_used_percent(id, label, used, reset)),
        _ => None,
    }
}

fn grok_headers(token: &str) -> HeaderMap {
    let mut headers = bearer(token);
    headers.insert("x-grok-client-surface", HeaderValue::from_static("grok-build"));
    headers.insert("x-grok-client-version", HeaderValue::from_static("1.0.0"));
    headers
}

async fn fetch_grok(client: &reqwest::Client, cred: StoredCred) -> ProviderReport {
    let identity = cred.identity.clone();
    match fetch_with_refresh(client, &cred, "xai-oauth", |client, token| {
        Box::pin(get_json(client, GROK_USAGE_URL, grok_headers(token)))
    })
    .await
    {
        Ok((token, body)) => {
            let mut report = parse_grok(identity.clone(), body);
            // 复用已验证可用的 token 再取一次月度账单：周额度端点没有订阅信息，
            // 只有这个端点的 period end 才是订阅到期日。取不到就留空，交给
            // plan_expiry.json 的手工配置兜底。
            if let Ok(billing) = get_json(client, GROK_BILLING_URL, grok_headers(&token)).await {
                if let Some(expiry) = parse_iso(billing.pointer("/config/billingPeriodEnd")) {
                    report.expires_at = Some(expiry);
                }
            }
            report
        }
        Err(err) => ProviderReport::err(ProviderId::Grok, identity, err),
    }
}

fn parse_grok(identity: Option<String>, body: Value) -> ProviderReport {
    let config = body.get("config").cloned().unwrap_or(Value::Null);
    let period = config.get("currentPeriod").cloned().unwrap_or(Value::Null);
    let used_percent = number(config.get("creditUsagePercent")).unwrap_or(0.0);
    // 只用于额度重置时间：currentPeriod.end 与 config.billingPeriodEnd 是同一
    // 时刻，都是周额度周期末，不能拿来当订阅到期日（到期日由月度账单端点提供）。
    let reset = parse_iso(period.get("end")).or_else(|| parse_iso(config.get("billingPeriodEnd")));
    let kind = period.get("type").and_then(|v| v.as_str()).unwrap_or("").to_ascii_uppercase();
    let label = if kind.contains("WEEK") {
        "Weekly credits"
    } else if kind.contains("MONTH") {
        "Monthly credits"
    } else {
        "Period credits"
    };
    // expires_at 不在这里赋值：订阅到期日来自 fetch_grok 的月度账单端点，
    // 取不到时留空，由 fetch_all 用 plan_expiry.json 的手工配置兜底。
    ProviderReport::ok(
        ProviderId::Grok,
        "xAI Grok",
        identity,
        None,
        vec![QuotaWindow::from_used_percent("grok-credits", label, used_percent, reset)],
    )
}

async fn fetch_codex(client: &reqwest::Client, cred: StoredCred) -> ProviderReport {
    let plan = cred.plan.clone();
    let account_fallback = cred.account_id.clone();
    let result = fetch_with_refresh(client, &cred, "openai-codex", move |client, token| {
        let mut headers = bearer(token);
        if let Some(account_id) = credentials::chatgpt_account_id(token, account_fallback.as_deref()) {
            if let Ok(value) = HeaderValue::from_str(&account_id) {
                headers.insert("ChatGPT-Account-Id", value);
            }
        }
        Box::pin(get_json(client, "https://chatgpt.com/backend-api/wham/usage", headers))
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
    let mut report = ProviderReport::ok(ProviderId::Codex, "OpenAI Codex", identity, plan_label, windows);
    report.resets_left =
        number(body.pointer("/rate_limit_reset_credits/available_count")).map(|value| value as i64);
    report
}

fn push_codex_window(windows: &mut Vec<QuotaWindow>, raw: Option<&Value>, review: bool) {
    let Some(window) = raw else { return };
    let Some(used_percent) = number(window.get("used_percent")) else { return };
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
    windows.push(QuotaWindow::from_used_percent(id, label, used_percent, reset));
}

async fn get_json(client: &reqwest::Client, url: &str, headers: HeaderMap) -> Result<Value, String> {
    let response = client
        .get(url)
        .headers(headers)
        .send()
        .await
        .map_err(|err| sanitize(&err.to_string()))?;
    let status = response.status();
    let text = response.text().await.map_err(|err| sanitize(&err.to_string()))?;
    if !status.is_success() {
        return Err(format!("HTTP {status}: {}", snippet(&text)));
    }
    serde_json::from_str(&text).map_err(|_| "invalid JSON".to_string())
}

async fn post_json(client: &reqwest::Client, url: &str, mut headers: HeaderMap) -> Result<Value, String> {
    headers.insert("Content-Type", HeaderValue::from_static("application/json"));
    let response = client
        .post(url)
        .headers(headers)
        .body("{}")
        .send()
        .await
        .map_err(|err| sanitize(&err.to_string()))?;
    let status = response.status();
    let text = response.text().await.map_err(|err| sanitize(&err.to_string()))?;
    if !status.is_success() {
        return Err(format!("HTTP {status}: {}", snippet(&text)));
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
        if let Some(dt) = unix_to_dt(number(data.get(key))).or_else(|| millis_to_dt(number(data.get(key)))) {
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

fn snippet(text: &str) -> String {
    let flat: String = text.chars().filter(|c| !c.is_control()).take(120).collect();
    sanitize(&flat)
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
