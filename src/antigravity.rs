//! Query the official CLI's read-only /usage command; never launch the desktop app.
//! agy lives in WSL (`~/.local/bin/agy`) and runs through `wsl.exe`; a Windows install
//! under `%LOCALAPPDATA%\agy\bin` is only a fallback.

use crate::credentials;
use serde_json::Value;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

/// Where the Antigravity CLI is installed.
pub(crate) enum Cli {
    /// agy inside a WSL distro, started as `wsl.exe -d <distro> -e <path>`.
    Wsl { distro: String, path: String },
    /// Legacy Windows install.
    Windows(PathBuf),
}

pub(crate) fn executable() -> Option<Cli> {
    wsl_executable().or_else(|| {
        let path = PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("agy/bin/agy.exe");
        path.is_file().then_some(Cli::Windows(path))
    })
}

/// Finds `~/.local/bin/agy` through the `\\wsl.localhost` share without starting wsl.exe.
fn wsl_executable() -> Option<Cli> {
    credentials::wsl_homes().into_iter().find_map(|home| {
        home.unc
            .join(r".local\bin\agy")
            .is_file()
            .then(|| Cli::Wsl {
                path: format!("{}/.local/bin/agy", home.linux),
                distro: home.distro,
            })
    })
}

pub(crate) async fn query(cli: &Cli) -> Result<Value, String> {
    let (mut command, place) = match cli {
        Cli::Wsl { distro, path } => {
            let mut command = credentials::hidden_command("wsl.exe");
            command.args(["-d", distro, "-e", path]);
            (tokio::process::Command::from(command), "WSL")
        }
        Cli::Windows(binary) => (
            tokio::process::Command::from(credentials::hidden_command(
                binary.to_str().ok_or("Antigravity CLI 安装路径无效")?,
            )),
            "PowerShell",
        ),
    };
    command
        .args([
            "--print",
            "/usage",
            "--output-format",
            "json",
            "--disable-slash-commands=false",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(60), command.output())
        .await
        .map_err(|_| "Antigravity CLI 额度查询超时")?
        .map_err(|_| "无法启动 Antigravity CLI")?;
    if !output.status.success() {
        // Diagnostics may contain authentication details; don't echo stderr.
        return Err(format!(
            "Antigravity CLI 查询失败（退出码 {}），请在 {place} 运行 agy 检查登录",
            output
                .status
                .code()
                .map_or_else(|| "未知".into(), |code| code.to_string())
        ));
    }
    let mut body: Value =
        serde_json::from_slice(&output.stdout).map_err(|_| "Antigravity CLI 返回无效 JSON")?;
    if body.get("status").and_then(Value::as_str) != Some("SUCCESS") {
        return Err(format!(
            "Antigravity CLI 额度查询失败，请在 {place} 运行 agy 检查登录"
        ));
    }
    if body.pointer("/command/name").and_then(Value::as_str) != Some("usage") {
        return Err("Antigravity CLI 未返回 /usage 额度结果".into());
    }
    body.pointer_mut("/command/data")
        .map(Value::take)
        .ok_or_else(|| "Antigravity CLI 缺少额度数据".into())
}
