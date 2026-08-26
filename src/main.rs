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
    let creds = credentials::load()?;

    if cli.json {
        let snapshot = fetch::fetch_all(&creds, only).await;
        println!("{}", serde_json::to_string_pretty(&snapshot)?);
        return Ok(());
    }

    let want_tui = !cli.snapshot && cli.watch.is_none() && std::io::stdout().is_terminal();
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
