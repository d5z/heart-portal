//! Shared shell exec policy: allowlist validation and environment for shell execution.

use crate::config::PortalConfig;
use anyhow::Result;
use tokio::process::Command;

#[cfg(test)]
#[path = "exec_policy_windows_tests.rs"]
mod windows_tests;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExecShell {
    Default,
    #[cfg(windows)]
    PowerShell,
}

impl ExecShell {
    pub(crate) fn parse(value: Option<&serde_json::Value>) -> Result<Self> {
        match value {
            None => Ok(Self::Default),
            Some(serde_json::Value::String(value)) if value == "default" => Ok(Self::Default),
            #[cfg(windows)]
            Some(serde_json::Value::String(value)) if value == "powershell" => Ok(Self::PowerShell),
            _ => anyhow::bail!(
                "shell must be 'default' or 'powershell' (PowerShell requires Windows)"
            ),
        }
    }

    pub(crate) fn program(self) -> &'static str {
        match self {
            Self::Default => shell_program(),
            #[cfg(windows)]
            Self::PowerShell => "powershell.exe",
        }
    }
}

#[cfg(any(windows, test))]
fn powershell_encoded_command(command: &str) -> String {
    use base64::Engine;
    // -EncodedCommand transports source as UTF-16LE; the launched shell's
    // redirected output and common text cmdlets use UTF-8 explicitly.
    let script = format!(
        "$OutputEncoding = [Console]::OutputEncoding = [Console]::InputEncoding = [System.Text.UTF8Encoding]::new($false);\n\
         $PSDefaultParameterValues['Get-Content:Encoding'] = 'UTF8';\n\
         $PSDefaultParameterValues['Set-Content:Encoding'] = 'UTF8';\n\
         $PSDefaultParameterValues['Add-Content:Encoding'] = 'UTF8';\n\
         $PSDefaultParameterValues['Out-File:Encoding'] = 'UTF8';\n\
         {command}"
    );
    let bytes: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[cfg(unix)]
pub(crate) fn shell_program() -> &'static str {
    "sh"
}

#[cfg(windows)]
pub(crate) fn shell_program() -> &'static str {
    "cmd"
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn shell_program() -> &'static str {
    "sh"
}

#[cfg(unix)]
fn shell_arg_flag() -> &'static str {
    "-c"
}

#[cfg(not(any(unix, windows)))]
fn shell_arg_flag() -> &'static str {
    "-c"
}

/// Shell metacharacters that can chain or subshell commands (beyond a single argv[0]).
fn has_subshell_or_backtick(command: &str) -> bool {
    command.contains('`') || command.contains("$(")
}

/// `;` `|` `&` split shell pipelines / command lists (allowlist must cover each segment's command).
fn has_chain_metachars(command: &str) -> bool {
    command.contains(';') || command.contains('|') || command.contains('&')
}

fn split_chain_segments(command: &str) -> impl Iterator<Item = &str> {
    command
        .split(';')
        .flat_map(|c| c.split('|'))
        .flat_map(|p| p.split('&'))
}

/// When an exec allowlist is configured, reject command injection via metacharacters.
pub(crate) fn validate_exec_allowlist(command: &str, allowlist: &[String]) -> Result<()> {
    if allowlist.is_empty() {
        return Ok(());
    }

    let cmd_first = command.split_whitespace().next().unwrap_or("");
    if !allowlist.iter().any(|a| a == cmd_first) {
        anyhow::bail!("Command '{}' not in exec allowlist", cmd_first);
    }

    if has_subshell_or_backtick(command) {
        anyhow::bail!("Command contains shell metacharacters with non-allowlisted commands");
    }

    if has_chain_metachars(command) {
        for segment in split_chain_segments(command) {
            let seg = segment.trim();
            if seg.is_empty() {
                continue;
            }
            let word = seg.split_whitespace().next().unwrap_or("");
            if word.is_empty() {
                continue;
            }
            if !allowlist.iter().any(|a| a == word) {
                anyhow::bail!(
                    "Command contains shell metacharacters with non-allowlisted commands"
                );
            }
        }
    }

    Ok(())
}

/// Configure shell execution the same way for sync exec and background spawn (HOME, PATH, etc.).
pub(crate) fn configure_shell_command(
    cmd: &mut Command,
    command: &str,
    config: &PortalConfig,
    workdir: &str,
    shell: ExecShell,
) {
    // cmd /S strips exactly one outer pair of quotes. Supply that pair ourselves
    // so a leading quoted executable and its quoted arguments survive intact.
    // Keep raw_arg: C-runtime escaping would break cmd's quotes and operators.
    #[cfg(windows)]
    {
        match shell {
            ExecShell::Default => {
                cmd.raw_arg("/D /S /C").raw_arg(format!("\"{}\"", command));
            }
            ExecShell::PowerShell => {
                cmd.args([
                    "-NoLogo",
                    "-NoProfile",
                    "-NonInteractive",
                    "-EncodedCommand",
                ])
                .arg(powershell_encoded_command(command));
            }
        }
        cmd.current_dir(workdir);
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW for foreground/background exec
    }
    #[cfg(not(windows))]
    {
        let _ = shell;
        cmd.arg(shell_arg_flag()).arg(command).current_dir(workdir);
    }

    // These describe the supervised Portal, not arbitrary commands it launches.
    cmd.env_remove("HEART_PORTAL_SUPERVISED")
        .env_remove("PORTAL_CONNECT_LINK");

    if std::env::var_os("HOME").is_none() {
        let home = std::env::var("HOME").ok().unwrap_or_else(|| {
            #[cfg(unix)]
            {
                let uid = unsafe { libc::getuid() };
                let pw = unsafe { libc::getpwuid(uid) };
                if !pw.is_null() {
                    let dir = unsafe { std::ffi::CStr::from_ptr((*pw).pw_dir) };
                    if let Ok(s) = dir.to_str() {
                        return s.to_string();
                    }
                }
            }
            #[cfg(windows)]
            {
                if let Ok(profile) = std::env::var("USERPROFILE") {
                    return profile;
                }
            }
            config
                .security
                .workspace_root
                .to_string_lossy()
                .into_owned()
        });
        cmd.env("HOME", home);
    }
    if std::env::var_os("USER").is_none() {
        let user = if config.name.is_empty() {
            "being"
        } else {
            config.name.as_str()
        };
        cmd.env("USER", user);
    }
    #[cfg(windows)]
    cmd.env("PATH", windows_exec_path());
    #[cfg(not(windows))]
    {
        let path = std::env::var("PATH").unwrap_or_default();
        cmd.env(
            "PATH",
            format!("/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin:{}", path),
        );
    }
    if let Ok(tz) = std::env::var("TZ") {
        cmd.env("TZ", tz);
    }
    // Windows: ensure UTF-8 output from Python and other tools
    #[cfg(windows)]
    {
        cmd.env("PYTHONIOENCODING", "utf-8");
        cmd.env("PYTHONUTF8", "1");
        // Force .NET/PowerShell to use UTF-8
        cmd.env("DOTNET_CLI_UI_LANGUAGE", "en");
    }
}

/// Only accept actual Git layouts. Never search the workdir for Unix tools.
#[cfg(any(windows, test))]
fn git_usr_bin(roots: impl IntoIterator<Item = std::path::PathBuf>) -> Option<std::path::PathBuf> {
    roots.into_iter().find_map(|root| {
        if !root.is_absolute() {
            return None;
        }
        let git_exists = [
            "cmd/git.exe",
            "bin/git.exe",
            "mingw64/bin/git.exe",
            "mingw32/bin/git.exe",
        ]
        .iter()
        .any(|relative| root.join(relative).is_file());
        let tools = root.join("usr/bin");
        (git_exists && tools.join("ls.exe").is_file() && tools.join("bash.exe").is_file())
            .then_some(tools)
    })
}

#[cfg(any(windows, test))]
fn windows_exec_path() -> std::ffi::OsString {
    use std::path::PathBuf;
    let inherited = std::env::var_os("PATH").unwrap_or_default();
    let entries: Vec<PathBuf> = std::env::split_paths(&inherited).collect();
    let mut roots = Vec::new();
    // Git/cmd, Git/bin and Git/mingw64/bin can all appear on PATH.
    for entry in &entries {
        if entry.is_absolute() && entry.join("git.exe").is_file() {
            roots.extend(entry.ancestors().take(3).map(PathBuf::from));
        }
    }
    for variable in ["ProgramW6432", "ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(value) = std::env::var_os(variable) {
            roots.push(PathBuf::from(value).join("Git"));
        }
    }
    if let Some(value) = std::env::var_os("LOCALAPPDATA") {
        roots.push(PathBuf::from(value).join("Programs/Git"));
    }
    let system =
        PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into()));
    let mut paths = vec![
        system.join("System32"),
        system.clone(),
        system.join("System32/WindowsPowerShell/v1.0"),
    ];
    paths.extend(entries);
    if let Some(tools) = git_usr_bin(roots) {
        // Append as a fallback: do not shadow Windows tools or user-selected apps.
        if !paths.iter().any(|path| {
            path.as_os_str()
                .as_encoded_bytes()
                .eq_ignore_ascii_case(tools.as_os_str().as_encoded_bytes())
        }) {
            paths.push(tools);
        }
    }
    std::env::join_paths(paths).unwrap_or(inherited)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn powershell_script_transports_unicode_losslessly() {
        use base64::Engine;
        let command = "Write-Output '中文🙂'; Get-Content -LiteralPath '中文文件.txt'";
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(powershell_encoded_command(command))
            .unwrap();
        let wide: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        let script = String::from_utf16(&wide).unwrap();
        assert!(script.ends_with(command));
        assert!(script.contains("[Console]::OutputEncoding"));
        assert!(script.contains("['Get-Content:Encoding'] = 'UTF8'"));
    }

    #[test]
    fn invalid_shell_never_falls_back_to_another_interpreter() {
        assert_eq!(ExecShell::parse(None).unwrap(), ExecShell::Default);
        assert!(ExecShell::parse(Some(&serde_json::json!("typo"))).is_err());
        assert!(ExecShell::parse(Some(&serde_json::json!(42))).is_err());
        #[cfg(not(windows))]
        assert!(ExecShell::parse(Some(&serde_json::json!("powershell"))).is_err());
    }

    #[test]
    fn allowlist_empty_skips_metachar_check() {
        let allow: Vec<String> = vec![];
        assert!(validate_exec_allowlist("ls; rm -rf /", &allow).is_ok());
    }

    #[test]
    fn allowlist_blocks_semicolon_injection() {
        let allow = vec!["ls".to_string()];
        let err = validate_exec_allowlist("ls; rm -rf /", &allow).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("not in exec allowlist") || msg.contains("metacharacters"),
            "unexpected error: {}",
            msg
        );
    }

    #[test]
    fn allowlist_allows_simple_ls() {
        let allow = vec!["ls".to_string()];
        assert!(validate_exec_allowlist("ls -la", &allow).is_ok());
    }

    #[test]
    fn allowlist_blocks_subshell() {
        let allow = vec!["echo".to_string()];
        let err = validate_exec_allowlist("echo $(rm -rf /)", &allow).unwrap_err();
        assert!(err
            .to_string()
            .contains("Command contains shell metacharacters with non-allowlisted commands"));
    }

    #[test]
    fn shell_program_matches_target() {
        #[cfg(unix)]
        {
            assert_eq!(shell_program(), "sh");
            assert_eq!(shell_arg_flag(), "-c");
        }
        #[cfg(windows)]
        {
            assert_eq!(shell_program(), "cmd");
        }
        #[cfg(not(any(unix, windows)))]
        {
            assert_eq!(shell_program(), "sh");
            assert_eq!(shell_arg_flag(), "-c");
        }
    }

    #[test]
    fn git_discovery_requires_complete_installation_and_respects_order() {
        let temp = std::env::temp_dir().join(format!("portal-git-test-{}", uuid::Uuid::new_v4()));
        let first = temp.join("custom Git");
        let second = temp.join("Program Files/Git");
        std::fs::create_dir_all(first.join("usr/bin")).unwrap();
        assert!(git_usr_bin([first.clone()]).is_none());
        for root in [&first, &second] {
            std::fs::create_dir_all(root.join("cmd")).unwrap();
            std::fs::create_dir_all(root.join("usr/bin")).unwrap();
            for file in ["cmd/git.exe", "usr/bin/ls.exe", "usr/bin/bash.exe"] {
                std::fs::write(root.join(file), b"fixture").unwrap();
            }
        }
        assert_eq!(
            git_usr_bin([first.clone(), second.clone()]),
            Some(first.join("usr/bin"))
        );
        assert_eq!(
            git_usr_bin([second.clone(), first]),
            Some(second.join("usr/bin"))
        );
        assert!(git_usr_bin([std::path::PathBuf::from("relative/Git")]).is_none());
        std::fs::remove_dir_all(temp).unwrap();
    }
}
