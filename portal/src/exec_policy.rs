//! Shared shell exec policy: allowlist validation and environment for shell execution.

use crate::config::PortalConfig;
use anyhow::Result;
use tokio::process::Command;

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

#[cfg(windows)]
fn shell_arg_flag() -> &'static str {
    "/C"
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
                anyhow::bail!("Command contains shell metacharacters with non-allowlisted commands");
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
) {
    // On Windows, Command::arg() auto-quotes arguments containing spaces/pipes,
    // which breaks cmd.exe metacharacter processing (|, &, &&, etc.).
    // Use raw_arg to pass the command string unquoted so cmd /C sees it verbatim.
    #[cfg(windows)]
    {
        cmd.raw_arg("/D /C").raw_arg(command).current_dir(workdir);
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW for foreground/background exec
    }
    #[cfg(not(windows))]
    {
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
            config.security.workspace_root.to_string_lossy().into_owned()
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
    let default_path = r"C:\Windows\System32;C:\Windows;C:\Windows\System32\WindowsPowerShell\v1.0";
    #[cfg(not(windows))]
    let default_path = "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin";
    #[cfg(windows)]
    let separator = ";";
    #[cfg(not(windows))]
    let separator = ":";
    let path = std::env::var("PATH").unwrap_or_default();
    cmd.env("PATH", format!("{}{}{}", default_path, separator, path));
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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(
            err.to_string()
                .contains("Command contains shell metacharacters with non-allowlisted commands")
        );
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
            assert_eq!(shell_arg_flag(), "/C");
        }
        #[cfg(not(any(unix, windows)))]
        {
            assert_eq!(shell_program(), "sh");
            assert_eq!(shell_arg_flag(), "-c");
        }
    }
}
