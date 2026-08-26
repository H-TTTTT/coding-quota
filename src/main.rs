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
            fn SetConsoleOutputCP(code_page: u32) -> i32;
            fn SetConsoleCP(code_page: u32) -> i32;
        }
        unsafe {
            SetConsoleOutputCP(65001);
            SetConsoleCP(65001);
        }
    }
}

#[cfg(windows)]
fn ensure_terminal_profile() -> bool {
    const PROFILE: &str = r#"{
  "profiles": [
    {
      "guid": "{d0ac4c18-765a-43fd-b12d-4005d099cd6f}",
      "name": "Coding Quota TUI",
      "commandline": "cmd.exe",
      "closeOnExit": "graceful",
      "historySize": 0,
      "padding": "0",
      "scrollbarState": "hidden"
    }
  ]
}
"#;

    let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") else {
        return false;
    };
    let directory = std::path::PathBuf::from(local_app_data)
        .join("Microsoft")
        .join("Windows Terminal")
        .join("Fragments")
        .join("CodingQuota");
    let path = directory.join("profile.json");
    let changed = std::fs::read_to_string(&path).ok().as_deref() != Some(PROFILE);
    if changed {
        if std::fs::create_dir_all(&directory).is_err() || std::fs::write(&path, PROFILE).is_err() {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(400));
    }
    true
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

    std::env::set_var(HOSTED, "1");
    let size = format!("{},{}", tui::TUI_COLUMNS, tui::TUI_ROWS);
    let mut command = std::process::Command::new("wt.exe");
    command
        .args(["--focus", "--size"])
        .arg(size)
        .args(["--window", "new", "new-tab"]);
    if ensure_terminal_profile() {
        command.args(["--profile", "Coding Quota TUI"]);
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
    let want_tui =
        !cli.json && !cli.snapshot && cli.watch.is_none() && std::io::stdout().is_terminal();
    if want_tui && launch_focused_tui() {
        return Ok(());
    }

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
