use crate::credentials::{self, CredentialSet, StoredCred};
use crate::model::{ProviderId, ProviderReport, QuotaWindow, Snapshot};
use chrono::{DateTime, TimeZone, Utc};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, USER_AGENT};
use serde_json::Value;
use std::future::Future;
use std::time::Duration;

const UA: &str = "coding-quota/0.1";
const TIMEOUT: Duration = Duration::from_secs(20);

pub async fn fetch_all(creds: &CredentialSet, only: Option<ProviderId>, skip: &[ProviderId]) -> Snapshot {
    let client = match reqwest::Client::builder().timeout(TIMEOUT).build() {
        Ok(client) => client,
        Err(err) => {
            let reports = [
                ProviderId::Codex,
                ProviderId::Grok,
                ProviderId::Glm,
                ProviderId::Kimi,
                ProviderId::Cursor,
                ProviderId::Devin,
            ]
            .into_iter()
            .filter(|provider| only.is_none_or(|wanted| wanted == *provider))
            .filter(|provider| !skip.contains(provider))
            .map(|provider| ProviderReport::err(provider, None, format!("http client: {err}")))
            .collect();
            return Snapshot {
                fetched_at: Utc::now(),
                reports,
            };
        }
    };

    let (codex, grok, glm, kimi, cursor, devin) = tokio::join!(
        maybe_fetch(&client, ProviderId::Codex, creds.codex.clone(), only, skip),
        maybe_fetch(&client, ProviderId::Grok, creds.grok.clone(), only, skip),
        maybe_fetch(&client, ProviderId::Glm, creds.glm.clone(), only, skip),
        maybe_fetch(&client, ProviderId::Kimi, creds.kimi.clone(), only, skip),
        maybe_fetch(&client, ProviderId::Cursor, creds.cursor.clone(), only, skip),
        maybe_devin(only, skip),
    );

    Snapshot {
        fetched_at: Utc::now(),
        reports: [codex, grok, glm, kimi, cursor, devin]
            .into_iter()
            .flatten()
            .collect(),
    }
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
    Some(match cred {
        Some(cred) => match provider {
            ProviderId::Codex => fetch_codex(client, cred).await,
            ProviderId::Grok => fetch_grok(client, cred).await,
            ProviderId::Glm => fetch_glm(client, cred).await,
            ProviderId::Kimi => fetch_kimi(client, cred).await,
            ProviderId::Cursor => fetch_cursor(client, cred).await,
            // Devin 走 CLI 横幅，不使用 CredentialSet 里的凭据
            ProviderId::Devin => fetch_devin(),
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

async fn fetch_grok(client: &reqwest::Client, cred: StoredCred) -> ProviderReport {
    let identity = cred.identity.clone();
    match fetch_with_refresh(client, &cred, "xai-oauth", |client, token| {
        let mut headers = bearer(token);
        headers.insert("x-grok-client-surface", HeaderValue::from_static("grok-build"));
        headers.insert("x-grok-client-version", HeaderValue::from_static("1.0.0"));
        Box::pin(get_json(client, "https://cli-chat-proxy.grok.com/v1/billing?format=credits", headers))
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
    let kind = period.get("type").and_then(|v| v.as_str()).unwrap_or("").to_ascii_uppercase();
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
    let mut report =
        ProviderReport::ok(ProviderId::Codex, "OpenAI Codex", identity, plan_label, windows);
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
        .map_err(|err| brief(&err))?;
    let status = response.status();
    let text = response.text().await.map_err(|err| brief(&err))?;
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
        .map_err(|err| brief(&err))?;
    let status = response.status();
    let text = response.text().await.map_err(|err| brief(&err))?;
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
    Some(fetch_devin())
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
    // 用户可能有 devin 任务在跑：spawn 新实例会抢 session 锁，必须让路。
    if devin_running() {
        return parse_devin_cache()
            .unwrap_or_else(|| ProviderReport::err(ProviderId::Devin, None, "devin 任务运行中，暂停刷新"));
    }
    fetch_devin_banner()
}

fn fetch_devin_banner() -> ProviderReport {
    let Some(exe) = devin_executable() else {
        return ProviderReport::missing(ProviderId::Devin);
    };
    // 横幅只在拥有真实控制台时渲染：CREATE_NO_WINDOW 下 devin 直接不画 TUI。
    // conhost --headless 给它一个隐形控制台；cwd 用已信任目录，避免目录信任弹窗。
    let mut command = std::process::Command::new("conhost.exe");
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
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let Ok(mut child) = command
        .spawn()
    else {
        return ProviderReport::err(ProviderId::Devin, None, "无法启动 devin.exe");
    };
    let mut stdout = child.stdout.take();
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        use std::io::Read;
        let Some(stream) = stdout.as_mut() else { return };
        let mut chunk = [0u8; 8192];
        loop {
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(String::from_utf8_lossy(&chunk[..n]).into_owned()).is_err() {
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
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &child.id().to_string(), "/T", "/F"])
        .status();
    let _ = child.wait();

    match banner {
        Some((plan, remaining, reset_in)) => {
            // CLI 启动时刚刷新过 user_status 缓存，里面有日额度；
            // 缓存新鲜且套餐名一致（同一账号）就直接用缓存的完整两行。
            if let Some(cached) = parse_devin_cache_fresh(180) {
                if cached.plan.as_deref() == Some(plan.as_str()) {
                    return cached;
                }
            }
            let reset_at = reset_in
                .as_deref()
                .and_then(parse_devin_reset)
                .map(|secs| Utc::now() + chrono::Duration::seconds(secs));
            let used = 100.0 - remaining;
            ProviderReport::ok(
                ProviderId::Devin,
                "Devin",
                None,
                Some(plan),
                vec![QuotaWindow::from_used_percent(
                    "weekly",
                    "Weekly",
                    used,
                    reset_at,
                )],
            )
        }
        None => ProviderReport::err(ProviderId::Devin, None, "未从 devin 横幅捕获到额度"),
    }
}


/// 只读检测 devin.exe 是否在运行（用户可能有任务在跑）。绝不杀进程。
fn devin_running() -> bool {
    let output = std::process::Command::new("tasklist")
        .args(["/FI", "IMAGENAME eq devin.exe", "/NH"])
        .output();
    match output {
        Ok(out) => String::from_utf8_lossy(&out.stdout).to_lowercase().contains("devin.exe"),
        Err(_) => false,
    }
}

/// 用户任务运行期间从 user_status 缓存读额度，不 spawn 新实例。
/// 缓存是 JSON 包 base64 的 protobuf。f13 内关键字段：
/// f1.2=套餐名，f14=日额度剩余%，f15=周额度剩余%，
/// f17=日额度重置锚点（未消费时停留在过去，不展示），f18=周额度重置时间戳。
fn parse_devin_cache() -> Option<ProviderReport> {
    parse_devin_cache_inner(0)
}

/// 只接受 max_age_secs 内的新鲜缓存（banner 路径刚让 CLI 刷新过缓存时用）。
fn parse_devin_cache_fresh(max_age_secs: i64) -> Option<ProviderReport> {
    parse_devin_cache_inner(max_age_secs)
}

fn parse_devin_cache_inner(max_age_secs: i64) -> Option<ProviderReport> {
    let local = std::env::var_os("LOCALAPPDATA")?;
    let dir = std::path::PathBuf::from(local)
        .join("devin")
        .join("cli");
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
    if max_age_secs > 0 {
        let fetched = value.get("fetched_at_secs")?.as_i64()?;
        if Utc::now().timestamp() - fetched > max_age_secs {
            return None;
        }
    }
    let payload = base64_decode(value.get("payload")?.as_str()?)?;
    let quota = proto_field(&payload, 13)?;
    let plan = proto_field(&quota, 1)
        .and_then(|plan_msg| proto_field(&plan_msg, 2))
        .and_then(|bytes| proto_string(&bytes))
        .unwrap_or_else(|| "Unknown".into());
    let weekly = proto_varint_field(&quota, 15)?;
    let weekly_reset = proto_varint_field(&quota, 18)
        .filter(|secs| *secs > 1_600_000_000)
        .and_then(|secs| chrono::DateTime::from_timestamp(secs, 0));
    if !(0..=100).contains(&weekly) {
        return None;
    }
    let mut windows = Vec::new();
    if let Some(daily) = proto_varint_field(&quota, 14).filter(|d| (0..=100).contains(d)) {
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
