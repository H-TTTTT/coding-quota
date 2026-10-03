use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// 拉起子进程但不弹控制台窗口。GUI 是 windows 子系统程序，spawn 控制台程序
/// （tasklist/taskkill 等）不加 CREATE_NO_WINDOW 就会闪一个黑色窗口。
pub fn hidden_command(program: &str) -> Command {
    let command = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let mut command = command;
        command.creation_flags(CREATE_NO_WINDOW);
        command
    }
    #[cfg(not(windows))]
    {
        command
    }
}

#[derive(Debug, Clone)]
pub struct StoredCred {
    pub identity: Option<String>,
    pub access: Option<String>,
    pub expires_ms: Option<i64>,
    pub account_id: Option<String>,
    pub plan: Option<String>,
    /// Antigravity 的 Google Cloud 项目 ID（data 里的 `projectId` / `project_id`），
    /// 查询共享 Claude/GPT 桶时要用；其他平台没有这个概念。
    pub project_id: Option<String>,
    /// omp 凭据类型（"oauth" / "api_key"）：Devin 的 token 进 protobuf metadata
    /// 时 OAuth 需要加 devin-session-token$ 前缀，API key 不加。
    pub kind: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct CredentialSet {
    pub db_path: Option<PathBuf>,
    pub codex: Option<StoredCred>,
    pub claude: Option<StoredCred>,
    pub grok: Option<StoredCred>,
    pub glm: Option<StoredCred>,
    pub kimi: Option<StoredCred>,
    pub cursor: Option<StoredCred>,
    pub devin: Option<StoredCred>,
    pub antigravity: Option<StoredCred>,
}

/// 临时凭据副本的文件名前缀，见 `copy_db` / `sweep_stale_db_copies`。
const DB_COPY_PREFIX: &str = "coding-quota-";

/// 进程被强杀时来不及执行 `Drop`，`%TEMP%` 里会留下只读凭据数据库的副本
/// （连同 `-wal` / `-shm`）。启动时清扫一次，跳过其他进程刚创建的副本。
pub fn sweep_stale_db_copies() -> Result<usize> {
    let mut removed = 0;
    for entry in std::fs::read_dir(std::env::temp_dir())? {
        // 临时目录里可能有并发创建/删除的条目，单个读不到就跳过，不放弃整轮清扫。
        let Ok(entry) = entry else {
            continue;
        };
        let path = entry.path();
        if is_db_copy(path.as_path()) && is_stale(path.as_path()) {
            // 并发进程可能刚好删掉同一个文件，删不掉不是错误。
            let _ = std::fs::remove_file(&path);
            removed += 1;
        }
    }
    Ok(removed)
}

/// 判断是否为临时凭据副本：`coding-quota-<pid>-agent.db`，含 `-wal` / `-shm` 伴生文件。
fn is_db_copy(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let Some(rest) = name.strip_prefix(DB_COPY_PREFIX) else {
        return false;
    };
    rest.ends_with("-agent.db")
        || rest.ends_with("-agent.db-wal")
        || rest.ends_with("-agent.db-shm")
}

/// 修改时间超过 10 分钟才算「无人使用」：正常情况下副本只存在几毫秒，
/// 10 分钟足够覆盖慢盘上的一次完整读取，又不会误删并发进程的副本。
fn is_stale(path: &Path) -> bool {
    let Ok(meta) = path.metadata() else {
        return false;
    };
    let Ok(mtime) = meta.modified() else {
        return false;
    };
    match mtime.duration_since(UNIX_EPOCH) {
        Ok(age) => SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|now| now.as_secs().saturating_sub(age.as_secs()) > 600)
            .unwrap_or(false),
        // 时钟偏差导致 mtime 落在未来：宁可留着也不删。
        Err(_) => false,
    }
}

pub fn load() -> Result<CredentialSet> {
    let mut set = CredentialSet::default();
    if let Some(path) = find_omp_db() {
        set.db_path = Some(path.clone());
        load_from_sqlite(&path, &mut set)?;
    }
    apply_env_overrides(&mut set);
    Ok(set)
}

fn find_omp_db() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("CODING_QUOTA_DB") {
        let path = PathBuf::from(explicit);
        if path.is_file() {
            return Some(path);
        }
    }

    let mut candidates = Vec::new();
    for key in ["HOME", "USERPROFILE"] {
        if let Ok(root) = std::env::var(key) {
            if !root.is_empty() {
                candidates.push(PathBuf::from(root).join(".omp/agent/agent.db"));
            }
        }
    }
    if let (Ok(drive), Ok(home_path)) = (std::env::var("HOMEDRIVE"), std::env::var("HOMEPATH")) {
        candidates.push(PathBuf::from(format!("{drive}{home_path}")).join(".omp/agent/agent.db"));
    }
    candidates.extend(wsl_unc_candidates());
    if let Some(path) = wsl_db_via_wsl_exe() {
        candidates.push(path);
    }

    for path in candidates {
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

/// OMP 会话日志目录：与凭据库同目录（`~/.omp/agent/sessions`）。
pub fn omp_sessions_root() -> Option<PathBuf> {
    find_omp_db().and_then(|db| db.parent().map(|dir| dir.join("sessions")))
}

fn wsl_unc_candidates() -> Vec<PathBuf> {
    let mut homes = Vec::new();
    if let Ok(home) = std::env::var("HOME") {
        if home.starts_with('/') || home.contains("wsl.localhost") || home.contains(r"wsl$") {
            homes.push(home);
        }
    }
    if let Ok(user) = std::env::var("USERNAME").or_else(|_| std::env::var("USER")) {
        if !user.is_empty() {
            homes.push(format!("/home/{user}"));
        }
    }

    let distros = [
        std::env::var("WSL_DISTRO_NAME").ok(),
        Some("Ubuntu".into()),
        Some("Debian".into()),
        Some("Ubuntu-22.04".into()),
        Some("Ubuntu-24.04".into()),
    ];
    let mut out = Vec::new();
    for home in homes {
        let linux_home = if let Some(idx) = home.find(r"\home\") {
            format!("/home/{}", home[idx + 6..].replace('\\', "/"))
        } else if home.starts_with('/') {
            home.replace('\\', "/")
        } else {
            continue;
        };
        let suffix = format!("{}/.omp/agent/agent.db", linux_home.trim_end_matches('/'));
        for distro in distros.iter().flatten() {
            out.push(PathBuf::from(format!(
                r"\\wsl.localhost\{distro}{}",
                suffix.replace('/', "\\")
            )));
            out.push(PathBuf::from(format!(
                r"\\wsl$\{distro}{}",
                suffix.replace('/', "\\")
            )));
        }
    }
    out
}

fn wsl_db_via_wsl_exe() -> Option<PathBuf> {
    let output = hidden_command("wsl.exe")
        .args([
            "-e",
            "sh",
            "-c",
            "wslpath -w \"$HOME/.omp/agent/agent.db\" 2>/dev/null || true",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.contains("wslpath"))
        .map(PathBuf::from)?;
    path.is_file().then_some(path)
}

fn path_looks_remote(path: &Path) -> bool {
    let raw = path.to_string_lossy();
    raw.starts_with(r"\\")
        || raw.starts_with("//")
        || raw.contains("wsl.localhost")
        || raw.contains(r"wsl$")
}

/// 只读打开的凭据库。远程/锁定场景会落到临时副本，ReadonlyDb 在 Drop 时
/// 先关连接（Windows 上打开的文件删不掉）再删除副本，避免 token 在 %TEMP% 残留。
struct ReadonlyDb {
    conn: Option<Connection>,
    tmp: Option<PathBuf>,
}

impl ReadonlyDb {
    fn conn(&self) -> &Connection {
        self.conn.as_ref().expect("db connection")
    }
}

impl Drop for ReadonlyDb {
    fn drop(&mut self) {
        if self.tmp.is_none() {
            return;
        }
        if let Some(conn) = self.conn.take() {
            let _ = conn.close();
        }
        if let Some(tmp) = self.tmp.take() {
            remove_tmp_db(&tmp);
        }
    }
}

/// 删除临时副本及其 wal/shm 伴随文件。
fn remove_tmp_db(tmp: &Path) {
    let _ = std::fs::remove_file(tmp);
    if let Some(name) = tmp.file_name().and_then(|n| n.to_str()) {
        for suffix in ["-wal", "-shm"] {
            let _ = std::fs::remove_file(tmp.with_file_name(format!("{name}{suffix}")));
        }
    }
}

fn open_sqlite_readonly(path: &Path) -> Result<ReadonlyDb> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    if !path_looks_remote(path) {
        match Connection::open_with_flags(path, flags) {
            Ok(conn) => {
                return Ok(ReadonlyDb {
                    conn: Some(conn),
                    tmp: None,
                })
            }
            Err(err) if !is_lock_error(&err) => {
                return Err(err).with_context(|| format!("open {}", path.display()));
            }
            Err(_) => {}
        }
    }

    let tmp = std::env::temp_dir().join(format!("coding-quota-{}-agent.db", std::process::id()));
    std::fs::copy(path, &tmp).with_context(|| format!("copy {}", path.display()))?;
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        for suffix in ["-wal", "-shm"] {
            let src = path.with_file_name(format!("{name}{suffix}"));
            if src.is_file() {
                let dest = tmp.with_file_name(format!(
                    "{}{suffix}",
                    tmp.file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("agent.db")
                ));
                let _ = std::fs::copy(&src, dest);
            }
        }
    }
    match Connection::open_with_flags(&tmp, flags) {
        Ok(conn) => Ok(ReadonlyDb {
            conn: Some(conn),
            tmp: Some(tmp),
        }),
        Err(err) => {
            // 打开失败也不能留下含 token 的临时副本
            remove_tmp_db(&tmp);
            Err(err).with_context(|| format!("open {}", tmp.display()))
        }
    }
}

fn is_lock_error(err: &rusqlite::Error) -> bool {
    err.to_string().to_ascii_lowercase().contains("locked")
}

fn load_from_sqlite(path: &Path, set: &mut CredentialSet) -> Result<()> {
    let db = open_sqlite_readonly(path)?;
    let mut stmt = db.conn().prepare(
        "SELECT provider, credential_type, COALESCE(identity_key, ''), data
         FROM auth_credentials WHERE disabled_cause IS NULL",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;

    for row in rows {
        let (provider, credential_type, identity_key, data) = row?;
        let cred = parse_cred(&provider, &credential_type, &identity_key, &data);
        match provider.as_str() {
            "openai-codex" | "openai" | "codex" => {
                if set.codex.is_none() || provider == "openai-codex" {
                    set.codex = Some(cred);
                }
            }
            // 订阅额度只对 OAuth 登录有意义；Console API key 没有 5h/周额度，
            // 当作未订阅，避免挂一张永远 401 的卡片。
            "anthropic" if credential_type == "oauth" => set.claude = Some(cred),
            "xai-oauth" | "xai" => {
                if set.grok.is_none() || provider == "xai-oauth" {
                    set.grok = Some(cred);
                }
            }
            "zhipu-coding-plan" | "zhipuai-coding-plan" | "zai" => {
                if provider == "zhipu-coding-plan" || set.glm.is_none() {
                    set.glm = Some(cred);
                }
            }
            "kimi-code" | "kimi-for-coding" | "kimi" => {
                if set.kimi.is_none() || provider == "kimi-code" {
                    set.kimi = Some(cred);
                }
            }
            "devin" => set.devin = Some(cred),
            // 订阅额度只对 OAuth 登录有意义；API key 行不授权（同 Claude 的理由）。
            "google-antigravity" if credential_type == "oauth" => set.antigravity = Some(cred),
            _ => {}
        }
    }
    Ok(())
}

fn parse_cred(provider: &str, credential_type: &str, identity_key: &str, data: &str) -> StoredCred {
    let value: Value = serde_json::from_str(data).unwrap_or(Value::Null);
    let access = first_string(
        &value,
        &["access", "access_token", "key", "apiKey", "api_key"],
    );
    let refresh = first_string(&value, &["refresh", "refresh_token"]);
    let expires_ms = first_i64(&value, &["expires", "expiresAt", "expires_at"]);
    let account_id = first_string(&value, &["accountId", "account_id"]);
    let email = first_string(&value, &["email"]);
    let plan = first_string(&value, &["orgName", "plan"]);
    let project_id = first_string(&value, &["projectId", "project_id"]);
    let identity = if !identity_key.is_empty() {
        Some(pretty_identity(identity_key, email.as_deref()))
    } else {
        email.clone()
    };
    let _ = (provider, refresh, email);
    StoredCred {
        identity,
        access,
        expires_ms,
        account_id,
        plan,
        project_id,
        kind: Some(credential_type.trim().to_string()),
    }
}

fn pretty_identity(identity_key: &str, email: Option<&str>) -> String {
    if let Some(email) = email {
        return email.to_string();
    }
    identity_key
        .split('|')
        .find_map(|part| {
            part.strip_prefix("email:")
                .or_else(|| part.strip_prefix("account:"))
        })
        .unwrap_or(identity_key)
        .to_string()
}

fn first_string(value: &Value, keys: &[&str]) -> Option<String> {
    let obj = value.as_object()?;
    for key in keys {
        if let Some(Value::String(s)) = obj.get(*key) {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

fn first_i64(value: &Value, keys: &[&str]) -> Option<i64> {
    let obj = value.as_object()?;
    for key in keys {
        match obj.get(*key) {
            Some(Value::Number(n)) => return n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
            Some(Value::String(s)) => return s.parse().ok(),
            _ => {}
        }
    }
    None
}

fn apply_env_overrides(set: &mut CredentialSet) {
    if let Ok(key) =
        std::env::var("ZHIPU_API_KEY").or_else(|_| std::env::var("ZHIPU_CODING_PLAN_API_KEY"))
    {
        if !key.trim().is_empty() {
            set.glm = Some(api_key_cred(&key));
        }
    }
    if let Ok(key) = std::env::var("KIMI_API_KEY").or_else(|_| std::env::var("KIMI_CODE_API_KEY")) {
        if !key.trim().is_empty() {
            set.kimi = Some(api_key_cred(&key));
        }
    }
}

fn api_key_cred(key: &str) -> StoredCred {
    StoredCred {
        identity: None,
        access: Some(key.trim().to_string()),
        expires_ms: None,
        account_id: None,
        plan: None,
        project_id: None,
        kind: Some("api_key".into()),
    }
}

pub fn secret_from_omp(provider: &str, force_refresh: bool) -> Option<String> {
    let mut cmd = hidden_command("omp");
    cmd.arg("token").arg(provider);
    if force_refresh {
        cmd.arg("--force-refresh");
    }
    if let Some(token) = token_from_output(cmd.output()) {
        return Some(token);
    }
    // Windows build: omp lives inside WSL and is not on the Windows PATH.
    let mut shell = format!("\"$HOME/.bun/bin/omp\" token {provider}");
    if force_refresh {
        shell.push_str(" --force-refresh");
    }
    token_from_output(
        hidden_command("wsl.exe")
            .args(["-e", "sh", "-c", &shell])
            .output(),
    )
}

fn token_from_output(output: std::io::Result<std::process::Output>) -> Option<String> {
    let output = output.ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() || text.to_ascii_lowercase().contains("no active credential") {
        None
    } else {
        Some(text)
    }
}

pub fn chatgpt_account_id(token: &str, fallback: Option<&str>) -> Option<String> {
    jwt_claim(token, "https://api.openai.com/auth")
        .and_then(|v| {
            v.get("chatgpt_account_id")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string())
        })
        .or_else(|| fallback.map(|s| s.to_string()))
}

pub fn jwt_email(token: &str) -> Option<String> {
    jwt_claim(token, "https://api.openai.com/profile").and_then(|v| {
        v.get("email")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string())
    })
}

fn jwt_claim(token: &str, key: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let decoded =
        base64::Engine::decode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, payload)
            .or_else(|_| {
                base64::Engine::decode(&base64::engine::general_purpose::URL_SAFE, payload)
            })
            .ok()?;
    let json: Value = serde_json::from_slice(&decoded).ok()?;
    json.get(key).cloned()
}

pub fn token_expiring(expires_ms: Option<i64>, token: Option<&str>) -> bool {
    let skew_ms = 120_000_i64;
    let now = chrono::Utc::now().timestamp_millis();
    if let Some(expires) = expires_ms {
        if expires - now <= skew_ms {
            return true;
        }
    }
    if let Some(token) = token {
        if let Some(payload) = token.split('.').nth(1) {
            if let Ok(decoded) =
                base64::Engine::decode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, payload)
            {
                if let Ok(json) = serde_json::from_slice::<Value>(&decoded) {
                    if let Some(exp) = json.get("exp").and_then(|v| v.as_i64()) {
                        return exp * 1000 - now <= skew_ms;
                    }
                }
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sweep_removes_only_stale_database_copies() {
        // 造两个副本：一个「刚创建」模拟并发进程，一个把 mtime 推到 11 分钟前。
        let dir = std::env::temp_dir();
        let tag = std::process::id();
        let fresh = dir.join(format!("coding-quota-{tag}-1-agent.db"));
        let stale = dir.join(format!("coding-quota-{tag}-2-agent.db-wal"));
        let other = dir.join("coding-quota-notes.txt");
        for path in [&fresh, &stale, &other] {
            std::fs::write(path, b"probe").unwrap();
        }
        let file = std::fs::File::options().write(true).open(&stale).unwrap();
        file.set_modified(SystemTime::now() - std::time::Duration::from_secs(11 * 60))
            .unwrap();
        drop(file);

        let removed = sweep_stale_db_copies().unwrap();

        assert!(!stale.exists(), "过期副本必须被清掉");
        assert!(fresh.exists(), "并发进程刚建的副本不能删");
        assert!(other.exists(), "同名前缀但非副本的文件不能碰");
        assert!(removed >= 1);

        for path in [fresh, other, stale] {
            let _ = std::fs::remove_file(path);
        }
    }

    #[test]
    fn soft_logout_removes_provider_and_login_restores_it() -> Result<()> {
        let path = std::env::temp_dir().join(format!(
            "coding-quota-logout-{}-{}.db",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        let result = (|| -> Result<()> {
            let db = rusqlite::Connection::open(&path)?;
            db.execute_batch(
                "CREATE TABLE auth_credentials (
                    provider TEXT, credential_type TEXT, identity_key TEXT,
                    data TEXT, disabled_cause TEXT
                 );
                 INSERT INTO auth_credentials VALUES
                    ('xai-oauth', 'oauth', '', '{}', NULL),
                    ('kimi-code', 'oauth', '', '{}', NULL);",
            )?;
            let mut before = CredentialSet::default();
            load_from_sqlite(&path, &mut before)?;
            assert!(before.grok.is_some());

            db.execute(
                "UPDATE auth_credentials SET disabled_cause = 'deleted by user'
                 WHERE provider = 'xai-oauth'",
                [],
            )?;
            let mut logged_out = CredentialSet::default();
            load_from_sqlite(&path, &mut logged_out)?;
            assert!(
                logged_out.grok.is_none(),
                "soft-deleted credentials must not authorize Grok"
            );
            assert!(
                logged_out.kimi.is_some(),
                "other active providers must remain available"
            );

            db.execute(
                "UPDATE auth_credentials SET disabled_cause = NULL WHERE provider = 'xai-oauth'",
                [],
            )?;
            let mut logged_in = CredentialSet::default();
            load_from_sqlite(&path, &mut logged_in)?;
            assert!(logged_in.grok.is_some(), "login must restore the provider");
            Ok(())
        })();
        let _ = std::fs::remove_file(&path);
        result
    }

    #[test]
    fn antigravity_extracts_project_and_only_oauth_authorizes() -> Result<()> {
        let path = std::env::temp_dir().join(format!(
            "coding-quota-antigravity-{}-{}.db",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        let result = (|| -> Result<()> {
            let db = rusqlite::Connection::open(&path)?;
            db.execute_batch(
                "CREATE TABLE auth_credentials (
                    provider TEXT, credential_type TEXT, identity_key TEXT,
                    data TEXT, disabled_cause TEXT
                 );
                 INSERT INTO auth_credentials VALUES
                    ('google-antigravity', 'oauth', 'email:ag-user@example.com',
                     '{\"access\":\"ag-oauth-token\",\"projectId\":\"ag-proj-42\",\"expires\":1893456000000}', NULL),
                    ('google-antigravity', 'api_key', '', '{\"key\":\"ag-api-key\"}', NULL),
                    ('kimi-code', 'oauth', '', '{}', NULL);",
            )?;
            let mut before = CredentialSet::default();
            load_from_sqlite(&path, &mut before)?;
            let cred = before
                .antigravity
                .as_ref()
                .expect("active OAuth row must authorize Antigravity");
            assert_eq!(cred.access.as_deref(), Some("ag-oauth-token"));
            assert_eq!(
                cred.project_id.as_deref(),
                Some("ag-proj-42"),
                "projectId 必须提取到 StoredCred.project_id"
            );
            assert_eq!(cred.kind.as_deref(), Some("oauth"));
            assert!(before.kimi.is_some());

            // 软登出 OAuth 行：剩下的 api_key 行仍处于启用状态，但不得授权。
            db.execute(
                "UPDATE auth_credentials SET disabled_cause = 'deleted by user'
                 WHERE provider = 'google-antigravity' AND credential_type = 'oauth'",
                [],
            )?;
            let mut logged_out = CredentialSet::default();
            load_from_sqlite(&path, &mut logged_out)?;
            assert!(
                logged_out.antigravity.is_none(),
                "软删除 OAuth 行后，仍在启用的 api_key 行也不得授权 Antigravity"
            );
            assert!(
                logged_out.kimi.is_some(),
                "other active providers must remain available"
            );

            db.execute(
                "UPDATE auth_credentials SET disabled_cause = NULL
                 WHERE provider = 'google-antigravity' AND credential_type = 'oauth'",
                [],
            )?;
            let mut logged_in = CredentialSet::default();
            load_from_sqlite(&path, &mut logged_in)?;
            let cred = logged_in
                .antigravity
                .as_ref()
                .expect("login must restore the provider");
            assert_eq!(cred.project_id.as_deref(), Some("ag-proj-42"));
            Ok(())
        })();
        let _ = std::fs::remove_file(&path);
        result
    }
}
