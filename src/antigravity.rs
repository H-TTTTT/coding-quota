//! Only the official Windows language server manages Google authentication.
//! Its stdin pipe ties its lifetime to this process, including forced GUI exit.

use crate::credentials;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Mutex;
use std::time::Duration;
use tokio::process::Child;

static BACKGROUND: Mutex<Option<Child>> = Mutex::new(None);

pub(crate) fn executable() -> Option<PathBuf> {
    let path = PathBuf::from(std::env::var_os("LOCALAPPDATA")?)
        .join("Programs/antigravity/resources/bin/language_server.exe");
    path.is_file().then_some(path)
}

pub(crate) fn owned_pid() -> Option<u32> {
    let mut state = BACKGROUND.lock().ok()?;
    let child = state.as_mut()?;
    if matches!(child.try_wait(), Ok(None)) {
        child.id()
    } else {
        *state = None;
        None
    }
}

pub(crate) fn start() -> Result<u32, String> {
    let mut state = BACKGROUND
        .lock()
        .map_err(|_| "Antigravity 后台状态不可用")?;
    if let Some(child) = state.as_mut() {
        if matches!(child.try_wait(), Ok(None)) {
            return child.id().ok_or_else(|| "Antigravity 后台已退出".into());
        }
    }
    *state = None;
    let binary = executable().ok_or("未找到 Antigravity，请先安装并登录官方客户端")?;
    let mut random = [0_u8; 32];
    getrandom::fill(&mut random).map_err(|_| "无法生成 Antigravity 本地认证密钥")?;
    let csrf = URL_SAFE_NO_PAD.encode(random);
    let mut command = tokio::process::Command::from(credentials::hidden_command(
        binary.to_str().ok_or("Antigravity 安装路径无效")?,
    ));
    // No Electron/UI is launched. The server's --headless flag is intentionally
    // absent: it selects interactive terminal login instead of native keyring auth.
    let child = command
        .args([
            "--standalone",
            "--exit_on_stdin_close",
            "--override_ide_name",
            "antigravity",
            "--subclient_type",
            "hub",
            "--override_user_agent_name",
            "antigravity",
            "--app_data_dir",
            "antigravity",
            "--https_server_port",
            "0",
            "--csrf_token",
            &csrf,
            "--api_server_url",
            "https://generativelanguage.googleapis.com",
            "--cloud_code_endpoint",
            "https://daily-cloudcode-pa.googleapis.com",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| "Antigravity 官方后台启动失败")?;
    let pid = child.id().ok_or("Antigravity 官方后台已退出")?;
    *state = Some(child);
    Ok(pid)
}

pub(crate) async fn stop() {
    let child = BACKGROUND.lock().ok().and_then(|mut state| state.take());
    if let Some(mut child) = child {
        // Only stop our own child; never signal the user's normal client.
        drop(child.stdin.take());
        if !matches!(
            tokio::time::timeout(Duration::from_secs(3), child.wait()).await,
            Ok(Ok(_))
        ) {
            let _ = child.kill().await;
        }
    }
}
