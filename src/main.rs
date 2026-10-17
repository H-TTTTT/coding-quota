#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod tui;

use anyhow::Result;
use clap::Parser;
use coding_quota::model::ProviderId;
use coding_quota::{credentials, fetch, render};
use std::io::IsTerminal;

#[derive(Parser, Debug)]
#[command(
    name = "coding-quota-tui",
    version,
    about = "Show coding-plan quotas and DeepSeek account balance in one place"
)]
struct Cli {
    /// Machine-readable JSON
    #[arg(long)]
    json: bool,

    /// Print a one-shot text snapshot instead of the TUI
    #[arg(long)]
    snapshot: bool,

    /// Only query one provider: codex, claude, grok, glm, kimi, cursor, devin, antigravity, deepseek, minimax
    #[arg(long, short = 'p')]
    provider: Option<String>,

    /// Refresh every N seconds in snapshot mode
    #[arg(long)]
    watch: Option<u64>,

    /// Show the TUI with mock data (no credentials needed)
    #[arg(long)]
    demo: bool,
}

fn enable_windows_console() {
    #[cfg(windows)]
    {
        #[link(name = "kernel32")]
        extern "system" {
            fn AttachConsole(process_id: u32) -> i32;
            fn SetConsoleOutputCP(code_page: u32) -> i32;
            fn SetConsoleCP(code_page: u32) -> i32;
        }
        unsafe {
            const ATTACH_PARENT_PROCESS: u32 = u32::MAX;
            AttachConsole(ATTACH_PARENT_PROCESS);
            SetConsoleOutputCP(65001);
            SetConsoleCP(65001);
        }
    }
}

#[cfg_attr(not(windows), allow(dead_code))]
mod terminal_profile {
    use serde_json::Value;
    use std::path::{Path, PathBuf};

    const LEGACY_GUID: &str = "{d0ac4c18-765a-43fd-b12d-4005d099cd6f}";
    const LEGACY_NAME: &str = "Coding Quota TUI";

    /// 清理早期版本写进 Windows Terminal 的 `Coding Quota TUI` 配置文件，
    /// 避免在终端设置和下拉菜单里留下多余条目。
    pub fn cleanup_legacy() {
        let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") else {
            return;
        };
        let local_app_data = PathBuf::from(local_app_data);
        let fragment_dir = local_app_data
            .join("Microsoft")
            .join("Windows Terminal")
            .join("Fragments")
            .join("CodingQuota");
        let _ = std::fs::remove_dir_all(fragment_dir);

        for settings_path in [
            local_app_data
                .join("Packages")
                .join("Microsoft.WindowsTerminal_8wekyb3d8bbwe")
                .join("LocalState")
                .join("settings.json"),
            local_app_data
                .join("Packages")
                .join("Microsoft.WindowsTerminalPreview_8wekyb3d8bbwe")
                .join("LocalState")
                .join("settings.json"),
            local_app_data
                .join("Microsoft")
                .join("Windows Terminal")
                .join("settings.json"),
        ] {
            if settings_path.is_file() {
                let _ = remove_from_settings_file(&settings_path);
            }
            let backup = settings_path.with_file_name("settings.json.coding-quota-backup");
            let _ = std::fs::remove_file(backup);
        }
    }

    fn remove_from_settings_file(path: &Path) -> Option<bool> {
        let raw = std::fs::read_to_string(path).ok()?;
        let updated = strip_legacy_profile(&raw)?;
        std::fs::write(path, updated).ok()?;
        Some(true)
    }

    pub(crate) fn strip_legacy_profile(raw: &str) -> Option<String> {
        if !raw.contains(LEGACY_GUID) && !raw.contains(LEGACY_NAME) {
            return None;
        }
        let mut root: Value = serde_json::from_str(raw).ok()?;
        let list = root.pointer_mut("/profiles/list")?.as_array_mut()?;
        let before = list.len();
        list.retain(|entry| {
            let guid = entry.get("guid").and_then(Value::as_str);
            let name = entry.get("name").and_then(Value::as_str);
            !guid.is_some_and(|g| g.eq_ignore_ascii_case(LEGACY_GUID)) && name != Some(LEGACY_NAME)
        });
        if list.len() == before {
            return None;
        }
        let mut rendered = serde_json::to_string_pretty(&root).ok()?;
        if raw.contains("\r\n") {
            rendered = rendered.replace('\n', "\r\n");
        }
        if raw.ends_with("\r\n") && !rendered.ends_with("\r\n") {
            rendered.push_str("\r\n");
        } else if raw.ends_with('\n') && !rendered.ends_with('\n') {
            rendered.push('\n');
        }
        Some(rendered)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn strips_legacy_coding_quota_profile_and_preserves_other_profiles() {
            let sample = r#"{
    "profiles": {
        "list": [
            {
                "guid": "{963ff2f7-6aed-5ce3-9d91-90d99571f53a}",
                "name": "Ubuntu-24.04"
            },
            {
                "closeOnExit": "graceful",
                "commandline": "cmd.exe",
                "guid": "{d0ac4c18-765a-43fd-b12d-4005d099cd6f}",
                "hidden": true,
                "historySize": 0,
                "name": "Coding Quota TUI",
                "padding": "0, 2",
                "scrollbarState": "hidden"
            },
            {
                "guid": "{2b761077-1626-5d25-ba1b-1aa81e2f101e}",
                "name": "Developer PowerShell for VS 2022"
            }
        ]
    }
}
"#;
            let cleaned = strip_legacy_profile(sample).expect("legacy profile must be removed");
            assert!(!cleaned.contains(LEGACY_GUID));
            assert!(!cleaned.contains(LEGACY_NAME));
            assert!(cleaned.contains("Ubuntu-24.04"));
            assert!(cleaned.contains("Developer PowerShell for VS 2022"));
            assert!(strip_legacy_profile(&cleaned).is_none());
        }
    }
}

#[cfg(windows)]
fn console_host_executable(executable: &std::path::Path) -> Option<std::path::PathBuf> {
    use std::io::Write;

    const PE_POINTER_OFFSET: usize = 0x3c;
    const OPTIONAL_HEADER_OFFSET: usize = 24;
    const CHECKSUM_OFFSET: usize = 64;
    const SUBSYSTEM_OFFSET: usize = 68;
    const WINDOWS_GUI: u16 = 2;
    const WINDOWS_CUI: u16 = 3;

    let mut image = std::fs::read(executable).ok()?;
    let pe_pointer = image.get(PE_POINTER_OFFSET..PE_POINTER_OFFSET + 4)?;
    let pe_offset = u32::from_le_bytes(pe_pointer.try_into().ok()?) as usize;
    if image.get(pe_offset..pe_offset + 4)? != b"PE\0\0" {
        return None;
    }
    let optional_header = pe_offset.checked_add(OPTIONAL_HEADER_OFFSET)?;
    let checksum = optional_header.checked_add(CHECKSUM_OFFSET)?;
    let subsystem = optional_header.checked_add(SUBSYSTEM_OFFSET)?;
    let current = u16::from_le_bytes(image.get(subsystem..subsystem + 2)?.try_into().ok()?);
    if current == WINDOWS_CUI {
        return Some(executable.to_path_buf());
    }
    if current != WINDOWS_GUI {
        return None;
    }
    image.get_mut(checksum..checksum + 4)?.fill(0);
    image
        .get_mut(subsystem..subsystem + 2)?
        .copy_from_slice(&WINDOWS_CUI.to_le_bytes());

    fn cache_file(path: &std::path::Path, content: &[u8]) -> Option<()> {
        if std::fs::read(path).ok().as_deref() == Some(content) {
            return Some(());
        }
        let pending = path.with_extension(format!("{}.tmp", std::process::id()));
        let _ = std::fs::remove_file(&pending);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&pending)
            .ok()?;
        if file.write_all(content).is_err() || file.sync_all().is_err() {
            let _ = std::fs::remove_file(&pending);
            return None;
        }
        drop(file);
        if std::fs::rename(&pending, path).is_err() {
            let _ = std::fs::remove_file(&pending);
            if std::fs::read(path).ok().as_deref() != Some(content) {
                return None;
            }
        }
        Some(())
    }

    let executable_dir = executable.parent()?;
    let dependencies = ["libgcc_s_seh-1.dll", "libwinpthread-1.dll"]
        .into_iter()
        .filter_map(|name| {
            std::fs::read(executable_dir.join(name))
                .ok()
                .map(|content| (name, content))
        })
        .collect::<Vec<_>>();
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for content in std::iter::once(image.as_slice())
        .chain(dependencies.iter().map(|(_, content)| content.as_slice()))
    {
        for byte in content {
            hash = (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    let directory = std::env::temp_dir()
        .join("CodingQuota")
        .join(format!("{hash:016x}"));
    std::fs::create_dir_all(&directory).ok()?;
    let runtime = directory.join("coding-quota-tui.exe");
    cache_file(&runtime, &image)?;
    for (name, content) in dependencies {
        cache_file(&directory.join(name), &content)?;
    }
    Some(runtime)
}

#[cfg(windows)]
fn launch_focused_tui() -> bool {
    const HOSTED: &str = "CODING_QUOTA_TUI_HOSTED";
    if std::env::var_os(HOSTED).is_some() {
        return false;
    }
    let Ok(executable) = std::env::current_exe() else {
        return false;
    };
    let Some(executable) = console_host_executable(&executable) else {
        return false;
    };

    std::env::set_var(HOSTED, "1");
    let size = format!("{},{}", tui::TUI_COLUMNS, tui::TUI_ROWS);
    let mut command = std::process::Command::new("wt.exe");
    command
        .args(["--focus", "--size"])
        .arg(size)
        .args(["--window", "new", "new-tab"]);
    terminal_profile::cleanup_legacy();
    let launched = command
        .args(["--suppressApplicationTitle", "--title", "编程额度"])
        .arg(executable)
        .args(std::env::args_os().skip(1))
        .spawn()
        .is_ok();
    if !launched {
        std::env::remove_var(HOSTED);
    }
    launched
}

#[cfg(not(windows))]
fn launch_focused_tui() -> bool {
    false
}

#[tokio::main]
async fn main() -> Result<()> {
    enable_windows_console();
    let cli = Cli::parse();
    let only = match cli.provider.as_deref() {
        Some(raw) => Some(ProviderId::parse_filter(raw).ok_or_else(|| {
            anyhow::anyhow!(
                "unknown provider `{raw}` (codex|claude|grok|glm|kimi|cursor|devin|antigravity|deepseek|minimax)"
            )
        })?),
        None => None,
    };
    let interactive = !cli.json && !cli.snapshot && cli.watch.is_none();
    let hosted = std::env::var_os("CODING_QUOTA_TUI_HOSTED").is_some();
    if interactive && launch_focused_tui() {
        return Ok(());
    }
    if cli.demo {
        return tui::run_demo().await;
    }
    // 终端窗口被直接关掉时进程是被强杀的，来不及删掉只读凭据副本；启动时扫一次。
    let _ = credentials::sweep_stale_db_copies();
    #[cfg(windows)]
    terminal_profile::cleanup_legacy();
    let want_tui = interactive && (hosted || std::io::stdout().is_terminal());

    if cli.json {
        let creds = credentials::load()?;
        let snapshot = fetch::fetch_all(&creds, only, &[]).await;
        println!("{}", serde_json::to_string_pretty(&snapshot)?);
        return Ok(());
    }

    if want_tui {
        return tui::run(credentials::load()?, only).await;
    }

    loop {
        // --watch 每轮重新读取凭据：登录、登出、token 轮换都不必重启进程。
        // watch 模式下读不到凭据只跳过本轮（网络盘抖动、omp 正在写库都可能瞬时失败）；
        // 单发 --snapshot 仍按原语义直接失败退出。
        let snapshot = match credentials::load() {
            Ok(creds) => Some(fetch::fetch_all(&creds, only, &[]).await),
            Err(err) if cli.watch.is_some() => {
                eprintln!("凭据读取失败，本轮跳过：{err:#}");
                None
            }
            Err(err) => return Err(err),
        };
        if let Some(snapshot) = snapshot {
            if cli.watch.is_some() {
                print!("\x1B[2J\x1B[H");
            }
            print!("{}", render::snapshot_text(&snapshot));
        }
        match cli.watch {
            Some(secs) if secs > 0 => {
                tokio::time::sleep(std::time::Duration::from_secs(secs)).await
            }
            _ => break,
        }
    }
    Ok(())
}
