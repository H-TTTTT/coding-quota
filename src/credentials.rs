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
    /// omp 凭据类型（"oauth" / "api_key"）：Devin 的 token 进 protobuf metadata
    /// 时 OAuth 需要加 devin-session-token$ 前缀，API key 不加。
    pub kind: Option<String>,
    /// 同 provider 下第几个 active OAuth 账号（1-based，按 rowid 升序），
    /// 供 `omp token <provider> --account <N>` 绑定当前卡片对应的凭据行，
    /// 避免多账号时 401 刷新切到另一个账号的 token。API key 凭据为 None。
    pub oauth_account: Option<usize>,
}
#[derive(Debug, Clone)]
pub struct ClaudeProfile {
    pub slot: String,
    pub active: bool,
    pub cred: StoredCred,
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
    pub deepseek: Option<StoredCred>,
    pub minimax: Option<StoredCred>,
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
    if set.claude.is_none() {
        set.claude = load_claude_code_cred(set.db_path.as_deref());
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

    static CACHED_DB: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    if let Some(cached) = CACHED_DB.get() {
        if cached.is_file() {
            return Some(cached.clone());
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
    for path in candidates {
        if path.is_file() {
            let _ = CACHED_DB.set(path.clone());
            return Some(path);
        }
    }

    // 仅当纯文件探测全部未命中时才兜底调用一次 wsl.exe，避免每轮刷新拉起子进程触发安全软件拦截。
    let path = wsl_db_via_wsl_exe()?;
    let _ = CACHED_DB.set(path.clone());
    Some(path)
}

/// OMP 会话日志目录：与凭据库同目录（`~/.omp/agent/sessions`）。
pub fn omp_sessions_root() -> Option<PathBuf> {
    find_omp_db().and_then(|db| db.parent().map(|dir| dir.join("sessions")))
}

/// Claude Code 会话日志目录列表（`~/.claude/projects`），供本地 Token 统计只读扫描。
pub fn claude_projects_roots() -> Vec<PathBuf> {
    let mut out = Vec::new();
    for home in candidate_homes(find_omp_db().as_deref()) {
        let dir = home.join(".claude").join("projects");
        if dir.is_dir() && !out.contains(&dir) {
            out.push(dir);
        }
    }
    out
}

fn is_wsl_unc_path(path: &Path) -> bool {
    let raw = path.to_string_lossy();
    raw.get(..r"\\wsl.localhost".len())
        .is_some_and(|head| head.eq_ignore_ascii_case(r"\\wsl.localhost"))
        || raw
            .get(..r"\\wsl$".len())
            .is_some_and(|head| head.eq_ignore_ascii_case(r"\\wsl$"))
}

/// 本机（非 WSL UNC）用户主目录列表：供 Windows 桌面端专属目录（如 Antigravity 桌面 App）
/// 只读发现文件，避免探测 `\\wsl.localhost` 唤醒休眠中的 WSL，同时在 Linux 单测下保留 `HOME`。
fn local_homes() -> Vec<PathBuf> {
    let mut homes = Vec::new();
    let mut push_unique = |path: PathBuf| {
        if !path.as_os_str().is_empty() && !is_wsl_unc_path(&path) && !homes.contains(&path) {
            homes.push(path);
        }
    };
    for key in ["USERPROFILE", "HOME"] {
        if let Some(root) = std::env::var_os(key) {
            push_unique(PathBuf::from(root));
        }
    }
    if let (Some(drive), Some(home_path)) =
        (std::env::var_os("HOMEDRIVE"), std::env::var_os("HOMEPATH"))
    {
        let mut combined = drive;
        combined.push(home_path);
        push_unique(PathBuf::from(combined));
    }
    homes
}

/// Antigravity Hub 桌面客户端的本地 RPC 发现文件；只读，不接触系统凭据库或 refresh token。
pub fn antigravity_daemon_files() -> Vec<PathBuf> {
    antigravity_daemon_files_in(&local_homes())
}

fn antigravity_daemon_files_in(homes: &[PathBuf]) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for home in homes {
        let dir = home.join(".gemini/antigravity/daemon");
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.starts_with("hub_") && name.ends_with(".json") && path.is_file() {
                    files.push(path);
                }
            }
        }
    }
    files.sort_by_cached_key(|path| {
        std::cmp::Reverse(path.metadata().and_then(|meta| meta.modified()).ok())
    });
    files
}

/// Antigravity 桌面客户端的本机会话数据库目录（`~/.gemini/antigravity/conversations`），只读扫描。
pub fn antigravity_conversations_root() -> Option<PathBuf> {
    antigravity_conversations_root_in(&local_homes())
}

fn antigravity_conversations_root_in(homes: &[PathBuf]) -> Option<PathBuf> {
    homes
        .iter()
        .map(|home| home.join(".gemini/antigravity/conversations"))
        .find(|root| root.is_dir())
}

#[cfg(windows)]
fn wsl_lxss_distros() -> Option<Vec<String>> {
    use core::ffi::c_void;

    const HKEY_CURRENT_USER: *mut c_void = 0x8000_0001_usize as *mut c_void;
    const KEY_QUERY_VALUE: u32 = 0x0001;
    const KEY_ENUMERATE_SUB_KEYS: u32 = 0x0008;
    const ERROR_FILE_NOT_FOUND: i32 = 2;
    const ERROR_PATH_NOT_FOUND: i32 = 3;

    #[link(name = "advapi32")]
    extern "system" {
        fn RegOpenKeyExW(
            key: *mut c_void,
            subkey: *const u16,
            options: u32,
            desired: u32,
            result: *mut *mut c_void,
        ) -> i32;
        fn RegCloseKey(key: *mut c_void) -> i32;
        fn RegEnumKeyExW(
            key: *mut c_void,
            index: u32,
            name: *mut u16,
            name_len: *mut u32,
            reserved: *mut u32,
            class: *mut u16,
            class_len: *mut u32,
            last_write: *mut c_void,
        ) -> i32;
        fn RegQueryValueExW(
            key: *mut c_void,
            name: *const u16,
            reserved: *mut u32,
            kind: *mut u32,
            data: *mut u8,
            size: *mut u32,
        ) -> i32;
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    unsafe fn read_reg_string(key: *mut c_void, name: &str) -> Option<String> {
        let name = wide(name);
        let mut buffer = [0_u16; 256];
        let mut size = (buffer.len() * 2) as u32;
        if RegQueryValueExW(
            key,
            name.as_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            buffer.as_mut_ptr() as *mut u8,
            &mut size,
        ) != 0
        {
            return None;
        }
        let chars = (size as usize / 2).min(buffer.len());
        let units: Vec<u16> = buffer[..chars]
            .iter()
            .copied()
            .take_while(|unit| *unit != 0)
            .collect();
        let value = String::from_utf16_lossy(&units).trim().to_string();
        (!value.is_empty()).then_some(value)
    }

    unsafe {
        let subkey = wide(r"Software\Microsoft\Windows\CurrentVersion\Lxss");
        let mut parent: *mut c_void = std::ptr::null_mut();
        let status = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            0,
            KEY_ENUMERATE_SUB_KEYS | KEY_QUERY_VALUE,
            &mut parent,
        );
        if status == ERROR_FILE_NOT_FOUND || status == ERROR_PATH_NOT_FOUND {
            return Some(Vec::new());
        }
        if status != 0 {
            return None;
        }
        let default_guid = read_reg_string(parent, "DefaultDistribution");
        let mut default_distro = None;
        let mut others = Vec::new();
        let mut index = 0_u32;
        loop {
            let mut name = [0_u16; 256];
            let mut len = name.len() as u32;
            if RegEnumKeyExW(
                parent,
                index,
                name.as_mut_ptr(),
                &mut len,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            ) != 0
            {
                break;
            }
            index += 1;
            let guid = String::from_utf16_lossy(&name[..len as usize]);
            let mut sub_name = name[..len as usize].to_vec();
            sub_name.push(0);
            let mut sub: *mut c_void = std::ptr::null_mut();
            if RegOpenKeyExW(parent, sub_name.as_ptr(), 0, KEY_QUERY_VALUE, &mut sub) != 0 {
                continue;
            }
            if let Some(distro) = read_reg_string(sub, "DistributionName") {
                if default_guid
                    .as_deref()
                    .is_some_and(|want| want.eq_ignore_ascii_case(&guid))
                {
                    default_distro = Some(distro);
                } else {
                    others.push(distro);
                }
            }
            RegCloseKey(sub);
        }
        RegCloseKey(parent);
        let mut out = Vec::new();
        if let Some(distro) = default_distro {
            out.push(distro);
        }
        out.extend(others);
        Some(out)
    }
}

/// 返回本机实际已安装的 WSL 发行版名称，避免探测不存在的发行版触发 Windows WebDAV 回退。
fn installed_wsl_distros() -> Vec<String> {
    let mut distros = Vec::new();
    let mut push = |raw: String| {
        let name = raw.trim().to_string();
        if name.is_empty()
            || name.eq_ignore_ascii_case("docker-desktop")
            || name.eq_ignore_ascii_case("docker-desktop-data")
            || distros.contains(&name)
        {
            return;
        }
        distros.push(name);
    };
    if let Ok(env_distro) = std::env::var("WSL_DISTRO_NAME") {
        push(env_distro);
    }
    #[cfg(windows)]
    match wsl_lxss_distros() {
        Some(installed) => {
            for name in installed {
                push(name);
            }
        }
        None => {
            for name in ["Ubuntu-24.04", "Ubuntu", "Ubuntu-22.04", "Debian"] {
                push(name.into());
            }
        }
    }
    distros
}

fn wsl_home_dirs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    for distro in installed_wsl_distros() {
        // 两种前缀指向同一个发行版，\\wsl.localhost 命中后立即跳出，不再重复探测 \\wsl$。
        for prefix in [r"\\wsl.localhost", r"\\wsl$"] {
            let Ok(entries) = std::fs::read_dir(format!(r"{prefix}\{distro}\home")) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if !out.contains(&path) {
                    out.push(path);
                }
            }
            break;
        }
    }
    out
}

fn candidate_homes(omp_db: Option<&Path>) -> Vec<PathBuf> {
    let mut homes = Vec::new();
    let mut push_unique = |path: PathBuf| {
        if !homes.contains(&path) {
            homes.push(path);
        }
    };
    if let Some(home) = omp_db
        .and_then(Path::parent)
        .and_then(Path::parent)
        .and_then(Path::parent)
    {
        push_unique(home.to_path_buf());
    }
    for key in ["HOME", "USERPROFILE"] {
        if let Ok(root) = std::env::var(key) {
            if !root.is_empty() {
                push_unique(PathBuf::from(root));
            }
        }
    }
    if let (Ok(drive), Ok(home_path)) = (std::env::var("HOMEDRIVE"), std::env::var("HOMEPATH")) {
        push_unique(PathBuf::from(format!("{drive}{home_path}")));
    }
    // omp 的 agent.db 可能在 Windows 用户目录下（如 C:\Users\<user>\.omp\agent\agent.db），
    // 而 Claude Code 登录在 WSL（\\wsl.localhost\<distro>\home\<user>\.claude），
    // 因此除非显式指定了 CODING_QUOTA_DB，否则枚举已安装 WSL 发行版的 home 目录。
    if std::env::var_os("CODING_QUOTA_DB").is_none() {
        for home in wsl_home_dirs() {
            push_unique(home);
        }
    }
    homes
}

pub fn load_claude_code_profiles(omp_db: Option<&Path>) -> Vec<ClaudeProfile> {
    for home in candidate_homes(omp_db) {
        let profiles = load_claude_code_profiles_from_home(&home);
        if !profiles.is_empty() {
            return profiles;
        }
    }
    Vec::new()
}

fn load_claude_code_cred(omp_db: Option<&Path>) -> Option<StoredCred> {
    for home in candidate_homes(omp_db) {
        let profiles = load_claude_code_profiles_from_home(&home);
        if profiles.len() >= 2 {
            let now_ms = chrono::Utc::now().timestamp_millis();
            if let Some(p) = profiles
                .iter()
                .find(|p| p.active && p.cred.expires_ms.map_or(true, |exp| exp > now_ms))
                .or_else(|| {
                    profiles
                        .iter()
                        .find(|p| p.cred.expires_ms.map_or(true, |exp| exp > now_ms))
                })
                .or_else(|| profiles.iter().find(|p| p.active))
                .or_else(|| profiles.first())
            {
                return Some(p.cred.clone());
            }
        }
        if let Some(cred) = load_claude_code_cred_from_home(&home) {
            return Some(cred);
        }
    }
    None
}

/// 只读加载正版 Claude Code CLI 的 `~/.claude/.credentials.json` 与 `~/.claude.json`。
/// 标记 `kind = "claude_code_oauth"` 且 `oauth_account = None`，后续遇到 401 绝不调用
/// omp 抢刷 refresh_token，续期完全由用户在 WSL 启动正版 `claude` 时自行完成。
fn load_claude_code_cred_from_home(home: &Path) -> Option<StoredCred> {
    load_claude_code_cred_from_files(
        &home.join(".claude").join(".credentials.json"),
        &home.join(".claude.json"),
    )
}

fn load_claude_code_cred_from_files(cred_path: &Path, state_path: &Path) -> Option<StoredCred> {
    let raw = std::fs::read_to_string(cred_path).ok()?;
    let root: Value = serde_json::from_str(&raw).ok()?;
    let oauth = root.get("claudeAiOauth")?;
    let access = first_string(oauth, &["accessToken", "access_token", "access"])?;
    let expires_ms = first_i64(oauth, &["expiresAt", "expires_at", "expires"]);
    let plan = claude_code_plan(oauth);
    let identity = std::fs::read_to_string(state_path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|val| {
            val.get("oauthAccount")
                .and_then(|acct| first_string(acct, &["emailAddress", "email"]))
        });
    Some(StoredCred {
        identity,
        access: Some(access),
        expires_ms,
        account_id: None,
        plan,
        kind: Some("claude_code_oauth".into()),
        oauth_account: None,
    })
}

fn load_claude_code_profiles_from_home(home: &Path) -> Vec<ClaudeProfile> {
    let root = home.join(".claude-profiles");
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let current = std::fs::read_to_string(root.join(".current"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let active_cred = load_claude_code_cred_from_home(home);
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(slot) = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::to_string)
        else {
            continue;
        };
        if slot.starts_with('.') {
            continue;
        }
        let is_active = current.as_deref() == Some(slot.as_str());
        let slot_cred = load_claude_code_cred_from_files(
            &path.join(".credentials.json"),
            &path.join(".claude.json"),
        );
        let cred = match (is_active, active_cred.clone(), slot_cred) {
            (true, Some(act), Some(saved)) => {
                if act.expires_ms.unwrap_or(0) >= saved.expires_ms.unwrap_or(0) {
                    act
                } else {
                    saved
                }
            }
            (true, Some(act), None) => act,
            (_, _, Some(saved)) => saved,
            _ => continue,
        };
        out.push(ClaudeProfile {
            slot,
            active: is_active,
            cred,
        });
    }
    out.sort_by(
        |a, b| match (a.slot.parse::<u32>(), b.slot.parse::<u32>()) {
            (Ok(na), Ok(nb)) => na.cmp(&nb),
            _ => a.slot.cmp(&b.slot),
        },
    );
    out
}

fn claude_code_plan(oauth: &Value) -> Option<String> {
    let tier = first_string(oauth, &["rateLimitTier", "rate_limit_tier"]).unwrap_or_default();
    for (marker, name) in [("max_20x", "Max 20x"), ("max_5x", "Max 5x")] {
        if tier.contains(marker) {
            return Some(name.to_string());
        }
    }
    let sub = first_string(oauth, &["subscriptionType", "subscription_type"])?;
    let sub = sub.strip_prefix("claude_").unwrap_or(&sub);
    let mut chars = sub.chars();
    let first = chars.next()?;
    Some(
        first
            .to_uppercase()
            .chain(chars)
            .collect::<String>()
            .replace('_', " "),
    )
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

    let distros = installed_wsl_distros();
    let mut out = Vec::new();
    for distro in &distros {
        let mut found_share = false;
        // Windows 用户名与 WSL 用户名不同（如 Windows 为 xuhaitao、WSL 为 x06579）时，
        // 直接枚举已安装发行版 \\wsl.localhost\<distro>\home 下的目录即可定位凭据库，
        // 命中后立即跳出，不再重复探测 \\wsl$，也不触碰未安装的发行版名称。
        for prefix in [r"\\wsl.localhost", r"\\wsl$"] {
            let home_root = PathBuf::from(format!(r"{prefix}\{distro}\home"));
            if let Ok(entries) = std::fs::read_dir(home_root) {
                found_share = true;
                for home in &homes {
                    let linux_home = if let Some(idx) = home.find(r"\home\") {
                        format!("/home/{}", home[idx + 6..].replace('\\', "/"))
                    } else if home.starts_with('/') {
                        home.replace('\\', "/")
                    } else {
                        continue;
                    };
                    let suffix =
                        format!("{}/.omp/agent/agent.db", linux_home.trim_end_matches('/'));
                    out.push(PathBuf::from(format!(
                        r"{prefix}\{distro}{}",
                        suffix.replace('/', "\\")
                    )));
                }
                for entry in entries.flatten() {
                    out.push(entry.path().join(r".omp\agent\agent.db"));
                }
                break;
            }
        }
        if !found_share {
            for home in &homes {
                let linux_home = if let Some(idx) = home.find(r"\home\") {
                    format!("/home/{}", home[idx + 6..].replace('\\', "/"))
                } else if home.starts_with('/') {
                    home.replace('\\', "/")
                } else {
                    continue;
                };
                let suffix = format!("{}/.omp/agent/agent.db", linux_home.trim_end_matches('/'));
                out.push(PathBuf::from(format!(
                    r"\\wsl.localhost\{distro}{}",
                    suffix.replace('/', "\\")
                )));
            }
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
        .stdin(std::process::Stdio::null())
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
         FROM auth_credentials WHERE disabled_cause IS NULL
         ORDER BY rowid ASC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;

    let mut oauth_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for row in rows {
        let (provider, credential_type, identity_key, data) = row?;
        let mut cred = parse_cred(&provider, &credential_type, &identity_key, &data);
        if credential_type == "oauth" {
            let index = oauth_counts.entry(provider.clone()).or_insert(0);
            *index += 1;
            cred.oauth_account = Some(*index);
        }
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
            "cursor" => set.cursor = Some(cred),
            "devin" => set.devin = Some(cred),
            // 余额查询走 api.deepseek.com 的 API key，与订阅登录无关。
            "deepseek" if credential_type == "api_key" => set.deepseek = Some(cred),
            "minimax-code-cn" if credential_type == "api_key" => set.minimax = Some(cred),
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
        kind: Some(credential_type.trim().to_string()),
        oauth_account: None,
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
        kind: Some("api_key".into()),
        oauth_account: None,
    }
}

pub fn secret_from_omp(
    provider: &str,
    oauth_account: Option<usize>,
    force_refresh: bool,
) -> Option<String> {
    let mut cmd = hidden_command("omp");
    cmd.stdin(std::process::Stdio::null());
    cmd.arg("token").arg(provider);
    if let Some(account) = oauth_account {
        cmd.arg("--account").arg(account.to_string());
    }
    if force_refresh {
        cmd.arg("--force-refresh");
    }
    if let Some(token) = token_from_output(cmd.output()) {
        return Some(token);
    }
    // Windows build: omp lives inside WSL and is not on the Windows PATH.
    let mut shell = format!("\"$HOME/.bun/bin/omp\" token {provider}");
    if let Some(account) = oauth_account {
        shell.push_str(&format!(" --account {account}"));
    }
    if force_refresh {
        shell.push_str(" --force-refresh");
    }
    token_from_output(
        hidden_command("wsl.exe")
            .args(["-e", "sh", "-c", &shell])
            .stdin(std::process::Stdio::null())
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
    fn every_credentialed_provider_is_mapped_from_its_omp_row() -> Result<()> {
        use crate::model::ProviderId;
        // 守卫 load_from_sqlite 的分支表：新增平台时若漏写或误删映射，
        // 这里会直接失败，而不是表现为「明明登录了却提示无凭据」。
        let path = std::env::temp_dir().join(format!(
            "cq-provider-mapping-{}-{}.db",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        let result = (|| -> Result<()> {
            let db = rusqlite::Connection::open(&path)?;
            db.execute_batch(
                "CREATE TABLE auth_credentials (
                    provider TEXT, credential_type TEXT, identity_key TEXT,
                    data TEXT, disabled_cause TEXT
                 );",
            )?;
            for (provider, credential_type) in [
                ("openai-codex", "oauth"),
                ("anthropic", "oauth"),
                ("xai-oauth", "oauth"),
                ("zhipu-coding-plan", "api_key"),
                ("kimi-code", "oauth"),
                ("cursor", "oauth"),
                ("devin", "oauth"),
                ("deepseek", "api_key"),
                ("minimax-code-cn", "api_key"),
            ] {
                db.execute(
                    "INSERT INTO auth_credentials VALUES (?1, ?2, 'identity:probe', '{}', NULL)",
                    [provider, credential_type],
                )?;
            }
            let mut set = CredentialSet::default();
            load_from_sqlite(&path, &mut set)?;
            let missing: Vec<&str> = [
                (ProviderId::Codex, set.codex.is_none()),
                (ProviderId::Claude, set.claude.is_none()),
                (ProviderId::Grok, set.grok.is_none()),
                (ProviderId::Glm, set.glm.is_none()),
                (ProviderId::Kimi, set.kimi.is_none()),
                (ProviderId::Cursor, set.cursor.is_none()),
                (ProviderId::Devin, set.devin.is_none()),
                (ProviderId::Deepseek, set.deepseek.is_none()),
                (ProviderId::Minimax, set.minimax.is_none()),
            ]
            .into_iter()
            .filter(|(_, absent)| *absent)
            .map(|(provider, _)| provider.title())
            .collect();
            assert!(
                missing.is_empty(),
                "these providers are authorized in omp but never reach the set: {missing:?}"
            );
            Ok(())
        })();
        let _ = std::fs::remove_file(&path);
        result
    }

    #[test]
    fn minimax_reads_active_wal_credential_and_honors_logout() -> Result<()> {
        let path = std::env::temp_dir().join(format!(
            "cq-minimax-auth-{}-{}.db",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        let result = (|| -> Result<()> {
            let db = rusqlite::Connection::open(&path)?;
            db.execute_batch(
                "PRAGMA journal_mode=WAL;
                 PRAGMA wal_autocheckpoint=0;
                 CREATE TABLE auth_credentials (
                    provider TEXT, credential_type TEXT, identity_key TEXT,
                    data TEXT, disabled_cause TEXT
                 );
                 PRAGMA wal_checkpoint(TRUNCATE);",
            )?;
            // 这次写入仍在 WAL，不能只复制 agent.db 主文件后判断账号是否存在。
            db.execute(
                "INSERT INTO auth_credentials VALUES ('minimax-code-cn', 'api_key', '', ?1, NULL)",
                [r#"{"key":"test-only"}"#],
            )?;
            db.execute(
                "INSERT INTO auth_credentials VALUES ('minimax-code-cn', 'oauth', '', ?1, NULL)",
                [r#"{"access":"unsupported"}"#],
            )?;
            let mut active = CredentialSet::default();
            load_from_sqlite(&path, &mut active)?;
            assert!(active.minimax.is_some());
            db.execute_batch(
                "UPDATE auth_credentials SET disabled_cause='deleted by user'
                 WHERE credential_type='api_key';",
            )?;
            let mut logged_out = CredentialSet::default();
            load_from_sqlite(&path, &mut logged_out)?;
            assert!(logged_out.minimax.is_none());
            db.execute_batch(
                "UPDATE auth_credentials SET disabled_cause=NULL WHERE credential_type='api_key'",
            )?;
            let mut logged_in = CredentialSet::default();
            load_from_sqlite(&path, &mut logged_in)?;
            assert!(logged_in.minimax.is_some());
            Ok(())
        })();
        for file in [
            &path,
            &path.with_extension("db-wal"),
            &path.with_extension("db-shm"),
        ] {
            let _ = std::fs::remove_file(file);
        }
        result
    }

    #[test]
    fn deepseek_only_active_api_key_authorizes_balance() -> Result<()> {
        let path = std::env::temp_dir().join(format!(
            "coding-quota-deepseek-{}-{}.db",
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
                    ('deepseek', 'oauth', '', '{\"access\":\"test-only\"}', NULL);",
            )?;
            let mut oauth = CredentialSet::default();
            load_from_sqlite(&path, &mut oauth)?;
            assert!(oauth.deepseek.is_none());

            db.execute(
                "UPDATE auth_credentials SET credential_type = 'api_key', data = ?1",
                [r#"{"key":"test-only"}"#],
            )?;
            let mut active = CredentialSet::default();
            load_from_sqlite(&path, &mut active)?;
            assert!(active.deepseek.is_some());

            db.execute_batch("UPDATE auth_credentials SET disabled_cause = 'deleted by user'")?;
            let mut logged_out = CredentialSet::default();
            load_from_sqlite(&path, &mut logged_out)?;
            assert!(logged_out.deepseek.is_none());

            db.execute_batch("UPDATE auth_credentials SET disabled_cause = NULL")?;
            let mut logged_in = CredentialSet::default();
            load_from_sqlite(&path, &mut logged_in)?;
            assert!(logged_in.deepseek.is_some());
            Ok(())
        })();
        let _ = std::fs::remove_file(&path);
        result
    }

    #[test]
    fn multiple_oauth_accounts_preserve_1_based_account_index_for_refresh() -> Result<()> {
        let path = std::env::temp_dir().join(format!(
            "coding-quota-multi-oauth-{}-{}.db",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        let result = (|| -> Result<()> {
            let db = rusqlite::Connection::open(&path)?;
            db.execute_batch(
                r#"CREATE TABLE auth_credentials (
                    provider TEXT, credential_type TEXT, identity_key TEXT,
                    data TEXT, disabled_cause TEXT
                 );
                 INSERT INTO auth_credentials VALUES
                    ('cursor', 'oauth', 'email:first@example.com',
                     '{"access":"tok-1","email":"first@example.com"}', NULL),
                    ('cursor', 'api_key', '', '{"key":"ignored-key"}', NULL),
                    ('cursor', 'oauth', 'email:disabled@example.com',
                     '{"access":"tok-disabled","email":"disabled@example.com"}', 'deleted'),
                    ('cursor', 'oauth', 'email:second@example.com',
                     '{"access":"tok-2","email":"second@example.com"}', NULL);"#,
            )?;
            let mut set = CredentialSet::default();
            load_from_sqlite(&path, &mut set)?;
            let cred = set.cursor.expect("second active OAuth row selected");
            assert_eq!(cred.identity.as_deref(), Some("second@example.com"));
            assert_eq!(cred.access.as_deref(), Some("tok-2"));
            assert_eq!(
                cred.oauth_account,
                Some(2),
                "active OAuth rows must carry their 1-based omp --account index so 401 refresh never switches to account 1"
            );
            Ok(())
        })();
        let _ = std::fs::remove_file(&path);
        result
    }

    #[test]
    fn claude_code_credentials_file_loads_token_plan_and_email_without_omp_row() -> Result<()> {
        let home = std::env::temp_dir().join(format!(
            "cq-claude-code-home-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        let claude_dir = home.join(".claude");
        std::fs::create_dir_all(&claude_dir)?;
        std::fs::write(
            claude_dir.join(".credentials.json"),
            r#"{
                "claudeAiOauth": {
                    "accessToken": "cc-access-token",
                    "refreshToken": "cc-refresh-token",
                    "expiresAt": 1792746737902,
                    "subscriptionType": "pro",
                    "rateLimitTier": "default_claude_ai"
                }
            }"#,
        )?;
        std::fs::write(
            home.join(".claude.json"),
            r#"{"oauthAccount": {"emailAddress": "ht9989@gmail.com"}}"#,
        )?;
        let cred = load_claude_code_cred_from_home(&home).expect("claude code cred loaded");
        let _ = std::fs::remove_dir_all(&home);
        assert_eq!(cred.access.as_deref(), Some("cc-access-token"));
        assert_eq!(cred.plan.as_deref(), Some("Pro"));
        assert_eq!(cred.identity.as_deref(), Some("ht9989@gmail.com"));
        assert_eq!(cred.kind.as_deref(), Some("claude_code_oauth"));
        assert_eq!(cred.oauth_account, None);
        Ok(())
    }

    #[test]
    fn antigravity_desktop_paths_exclude_wsl_shares_and_sort_hub_files_newest_first() -> Result<()>
    {
        assert!(is_wsl_unc_path(Path::new(
            r"\\wsl.localhost\Ubuntu-24.04\home\x06579"
        )));
        assert!(is_wsl_unc_path(Path::new(r"\\WSL$\Ubuntu\home\x06579")));
        assert!(!is_wsl_unc_path(Path::new(r"C:\Users\haita")));
        assert!(!is_wsl_unc_path(Path::new("/home/x06579")));

        let home = std::env::temp_dir().join(format!(
            "cq-ag-desktop-home-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        let daemon_dir = home.join(".gemini/antigravity/daemon");
        let conv_dir = home.join(".gemini/antigravity/conversations");
        std::fs::create_dir_all(&daemon_dir)?;
        std::fs::create_dir_all(&conv_dir)?;

        let older = daemon_dir.join("hub_6800.json");
        let newer = daemon_dir.join("hub_6811.json");
        let ignored = daemon_dir.join("other.json");
        std::fs::write(&older, b"{}")?;
        std::fs::write(&newer, b"{}")?;
        std::fs::write(&ignored, b"{}")?;

        let now = SystemTime::now();
        std::fs::File::options()
            .write(true)
            .open(&older)?
            .set_modified(now - std::time::Duration::from_secs(60))?;
        std::fs::File::options()
            .write(true)
            .open(&newer)?
            .set_modified(now)?;

        let homes = vec![home.clone()];
        let files = antigravity_daemon_files_in(&homes);
        let found_conv = antigravity_conversations_root_in(&homes);
        let _ = std::fs::remove_dir_all(&home);

        assert_eq!(files, vec![newer, older]);
        assert_eq!(found_conv, Some(conv_dir));
        Ok(())
    }
}
