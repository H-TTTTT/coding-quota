#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::process::{Command, ExitCode};

#[cfg(windows)]
const RUNTIME_NAME: &str = "coding-quota-console.exe";
#[cfg(not(windows))]
const RUNTIME_NAME: &str = "coding-quota-console";

#[cfg(windows)]
fn hide_console(command: &mut Command) {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn hide_console(_command: &mut Command) {}

fn main() -> ExitCode {
    let Ok(executable) = std::env::current_exe() else {
        return ExitCode::FAILURE;
    };
    let runtime = executable.with_file_name(RUNTIME_NAME);
    let mut args = std::env::args_os().skip(1).peekable();
    let interactive = args.peek().is_none();

    let mut command = Command::new(runtime);
    command.args(args);
    hide_console(&mut command);

    if interactive {
        command.env("CODING_QUOTA_LAUNCH_REQUEST", "1");
        return match command.spawn() {
            Ok(_) => ExitCode::SUCCESS,
            Err(_) => ExitCode::FAILURE,
        };
    }

    match command.status() {
        Ok(status) if status.success() => ExitCode::SUCCESS,
        _ => ExitCode::FAILURE,
    }
}
