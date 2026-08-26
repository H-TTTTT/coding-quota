#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod tui;

use anyhow::Result;
use clap::Parser;
use coding_quota::model::ProviderId;
use coding_quota::{credentials, fetch, render};
use std::io::IsTerminal;

#[derive(Parser, Debug)]
#[command(
    name = "coding-quota",
    about = "Show Codex, Grok, GLM, Kimi, and Cursor coding-plan quotas in one place"
)]
struct Cli {
    /// Machine-readable JSON
    #[arg(long)]
    json: bool,

    /// Print a one-shot text snapshot instead of the TUI
    #[arg(long)]
    snapshot: bool,

    /// Only query one provider: codex, grok, glm, kimi, cursor
    #[arg(long, short = 'p')]
    provider: Option<String>,

    /// Refresh every N seconds in snapshot mode
    #[arg(long)]
    watch: Option<u64>,
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

#[cfg(windows)]
mod terminal_profile {
    use std::path::{Path, PathBuf};

    const GUID: &str = "{d0ac4c18-765a-43fd-b12d-4005d099cd6f}";
    pub const NAME: &str = "Coding Quota TUI";
    const ENTRY: &str = r#"            {
                "closeOnExit": "graceful",
                "commandline": "cmd.exe",
                "guid": "{d0ac4c18-765a-43fd-b12d-4005d099cd6f}",
                "historySize": 0,
                "name": "Coding Quota TUI",
                "padding": "0, 2",
                "scrollbarState": "hidden"
            }"#;
    const FRAGMENT: &str = r#"{
  "profiles": [
    {
      "closeOnExit": "graceful",
      "commandline": "cmd.exe",
      "guid": "{d0ac4c18-765a-43fd-b12d-4005d099cd6f}",
      "historySize": 0,
      "name": "Coding Quota TUI",
      "padding": "0, 2",
      "scrollbarState": "hidden"
    }
  ]
}
"#;

    pub fn ensure() -> bool {
        let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") else {
            return false;
        };
        let local_app_data = PathBuf::from(local_app_data);
        let fragment = local_app_data
            .join("Microsoft")
            .join("Windows Terminal")
            .join("Fragments")
            .join("CodingQuota")
            .join("profile.json");

        if let Some((installed, changed)) = install_active_settings(&local_app_data) {
            if installed {
                let _ = std::fs::remove_file(fragment);
                if changed {
                    std::thread::sleep(std::time::Duration::from_millis(400));
                }
            }
            return installed;
        }

        let Some(directory) = fragment.parent() else {
            return false;
        };
        std::fs::create_dir_all(directory).is_ok()
            && std::fs::write(fragment, FRAGMENT).is_ok()
    }

    fn install_active_settings(local_app_data: &Path) -> Option<(bool, bool)> {
        let candidates = [
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
        ];
        candidates
            .into_iter()
            .find(|path| path.is_file())
            .map(|path| patch_settings(&path))
    }

    fn patch_settings(path: &Path) -> (bool, bool) {
        let Ok(settings) = std::fs::read_to_string(path) else {
            return (false, false);
        };
        if settings.contains(GUID) {
            let needs_padding = !settings.contains(r#""padding": "0, 2""#);
            if needs_padding {
                let start = settings.find(GUID).unwrap_or(0);
                let legacy = settings[start..].find(r#""padding": "0""#).map(|i| start + i);
                if let Some(index) = legacy {
                    let backup = path.with_file_name("settings.json.coding-quota-backup");
                    if !backup.exists() && std::fs::copy(path, &backup).is_err() {
                        return (false, false);
                    }
                    let mut updated = settings;
                    updated.replace_range(index..index + r#""padding": "0""#.len(), r#""padding": "0, 2""#);
                    return (std::fs::write(path, updated).is_ok(), true);
                }
            }
            return (true, false);
        }
        let Some(profiles) = settings.find("\"profiles\"") else {
            return (false, false);
        };
        let Some(list) = settings[profiles..].find("\"list\"").map(|index| profiles + index)
        else {
            return (false, false);
        };
        let Some(open) = settings[list..].find('[').map(|index| list + index) else {
            return (false, false);
        };
        let Some(close) = array_end(&settings, open) else {
            return (false, false);
        };

        let comma = if settings[open + 1..close].trim().is_empty() {
            ""
        } else {
            ","
        };
        let mut updated = String::with_capacity(settings.len() + ENTRY.len() + 16);
        updated.push_str(&settings[..close]);
        updated.push_str(comma);
        updated.push('\n');
        updated.push_str(ENTRY);
        updated.push('\n');
        updated.push_str("        ");
        updated.push_str(&settings[close..]);

        let backup = path.with_file_name("settings.json.coding-quota-backup");
        if !backup.exists() && std::fs::copy(path, &backup).is_err() {
            return (false, false);
        }
        (std::fs::write(path, updated).is_ok(), true)
    }

    fn array_end(text: &str, open: usize) -> Option<usize> {
        let mut depth = 0usize;
        let mut quoted = false;
        let mut escaped = false;
        for (offset, character) in text[open..].char_indices() {
            if quoted {
                if escaped {
                    escaped = false;
                } else if character == '\\' {
                    escaped = true;
                } else if character == '"' {
                    quoted = false;
                }
                continue;
            }
            match character {
                '"' => quoted = true,
                '[' => depth += 1,
                ']' => {
                    depth = depth.checked_sub(1)?;
                    if depth == 0 {
                        return Some(open + offset);
                    }
                }
                _ => {}
            }
        }
        None
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
        .map(|name| Some((name, std::fs::read(executable_dir.join(name)).ok()?)))
        .collect::<Option<Vec<_>>>()?;
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
    let runtime = directory.join("coding-quota.exe");
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
    if terminal_profile::ensure() {
        command.args(["--profile", terminal_profile::NAME]);
    }
    let launched = command
        .args([
            "--suppressApplicationTitle",
            "--title",
            "编程额度",
        ])
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
        Some(raw) => Some(
            ProviderId::parse_filter(raw)
                .ok_or_else(|| anyhow::anyhow!("unknown provider `{raw}` (codex|grok|glm|kimi|cursor)"))?,
        ),
        None => None,
    };
    let interactive = !cli.json && !cli.snapshot && cli.watch.is_none();
    let hosted = std::env::var_os("CODING_QUOTA_TUI_HOSTED").is_some();
    if interactive && launch_focused_tui() {
        return Ok(());
    }
    let want_tui = interactive && (hosted || std::io::stdout().is_terminal());

    let creds = credentials::load()?;
    if cli.json {
        let snapshot = fetch::fetch_all(&creds, only).await;
        println!("{}", serde_json::to_string_pretty(&snapshot)?);
        return Ok(());
    }

    if want_tui {
        return tui::run(creds, only).await;
    }

    loop {
        let snapshot = fetch::fetch_all(&creds, only).await;
        if cli.watch.is_some() {
            print!("\x1B[2J\x1B[H");
        }
        print!("{}", render::snapshot_text(&snapshot));
        match cli.watch {
            Some(secs) if secs > 0 => tokio::time::sleep(std::time::Duration::from_secs(secs)).await,
            _ => break,
        }
    }
    Ok(())
}
