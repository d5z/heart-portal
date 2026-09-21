#![windows_subsystem = "windows"]

use std::{
    os::windows::process::CommandExt,
    path::PathBuf,
    process::{Command, Stdio},
};

fn main() {
    std::process::exit(run().unwrap_or(1));
}

fn run() -> std::io::Result<i32> {
    let system = std::env::var_os("SystemRoot")
        .ok_or_else(|| std::io::Error::other("SystemRoot is missing"))?;
    // Neither the scheduled task nor its child creates a console. Waiting
    // preserves task ownership and crash recovery without locking Portal.exe.
    let status =
        Command::new(PathBuf::from(system).join("System32/WindowsPowerShell/v1.0/powershell.exe"))
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
            ])
            .args(std::env::args_os().skip(1))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(0x0800_0000)
            .status()?;
    Ok(status.code().unwrap_or(1))
}
