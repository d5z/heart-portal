//! Shared shell exec policy: allowlist validation and environment for `sh -c`.

use crate::config::PortalConfig;
use anyhow::Result;
use tokio::process::Command;

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

/// Configure `sh -c` the same way for sync exec and background spawn (HOME, PATH, etc.).
pub(crate) fn configure_shell_command(
    cmd: &mut Command,
    command: &str,
    config: &PortalConfig,
    workdir: &str,
) {
    #[cfg(unix)]
    cmd.arg("-c").arg(command);
    #[cfg(windows)]
    cmd.arg("/C").arg(command);
    cmd.current_dir(workdir);

    #[cfg(unix)]
    {
        if std::env::var_os("HOME").is_none() {
            let home = std::env::var("HOME").ok().unwrap_or_else(|| {
                let uid = unsafe { libc::getuid() };
                let pw = unsafe { libc::getpwuid(uid) };
                if !pw.is_null() {
                    let dir = unsafe { std::ffi::CStr::from_ptr((*pw).pw_dir) };
                    if let Ok(s) = dir.to_str() {
                        return s.to_string();
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
    }
    #[cfg(windows)]
    {
        // On Windows, inherit USERPROFILE and USERNAME as-is
    }
    #[cfg(unix)]
    {
        let default_path = "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin";
        let path = std::env::var("PATH").unwrap_or_default();
        cmd.env("PATH", format!("{}:{}", default_path, path));
    }
    #[cfg(windows)]
    {
        // On Windows, inherit PATH as-is (system PATH includes cmd.exe, powershell, etc.)
        if let Ok(path) = std::env::var("PATH") {
            cmd.env("PATH", path);
        }
    }
    if let Ok(tz) = std::env::var("TZ") {
        cmd.env("TZ", tz);
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
}
