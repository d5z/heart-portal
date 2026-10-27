//! Per-command Seatbelt policy, inherited by descendants. Never runs a host fallback.
use crate::config::PortalConfig;
use anyhow::{Context, Result};
use std::{os::unix::fs::MetadataExt, path::Path};
use tokio::process::Command;

// Avoid broad /System or /private grants: they contain aliases to user data.
const POLICY: &str = r#"
(version 1)
(deny default)
(allow process-exec process-fork)
(allow signal (target same-sandbox))
(allow process-info* (target same-sandbox))
(allow sysctl-read)
(allow file-read-metadata)
(allow file-read*
    (literal "/")
    (subpath "/bin") (subpath "/sbin")
    (subpath "/usr/bin") (subpath "/usr/sbin")
    (subpath "/usr/lib") (subpath "/usr/libexec") (subpath "/usr/share")
    (subpath "/System/Library") (subpath "/Library/Apple")
    (subpath "/private/var/db/dyld") (subpath "/private/var/db/timezone")
    (literal "/private/etc/localtime")
    (literal "/dev/random") (literal "/dev/urandom")
    (subpath "/opt/homebrew/bin") (subpath "/opt/homebrew/lib")
    (subpath "/opt/homebrew/Cellar") (subpath "/opt/homebrew/opt") (subpath "/opt/homebrew/share")
    (subpath "/usr/local/bin") (subpath "/usr/local/lib")
    (subpath "/usr/local/Cellar") (subpath "/usr/local/opt") (subpath "/usr/local/share")
    (subpath (param "WORKSPACE")))
(allow file-read* file-write-data (literal "/dev/null") (literal "/dev/zero"))
(allow file-read-data file-write-data (subpath "/dev/fd"))
(allow file-write* (subpath (param "WORKSPACE")))
; A new alias must not make an outside or protected inode writable/readable.
(deny file-link)
"#;

fn insensitive(name: &str) -> String {
    name.chars().map(|c| {
        if c.is_ascii_alphabetic() { format!("[{}{}]", c.to_ascii_lowercase(), c.to_ascii_uppercase()) }
        else if c == '.' { "[.]".into() } else { c.to_string() }
    }).collect()
}

pub fn shell_command(config: &PortalConfig, source: &str, workdir: &str) -> Result<Command> {
    let root = config.security.workspace_root.canonicalize().context("Resolve sandbox workspace")?;
    let cwd = Path::new(workdir).canonicalize().context("Resolve sandbox workdir")?;
    anyhow::ensure!(root.parent().is_some() && root.is_dir(), "Sandbox requires a workspace directory, not the filesystem root");
    anyhow::ensure!(cwd.starts_with(&root), "Sandbox workdir must be inside the workspace");
    // Seatbelt checks paths, not inode ownership. Refuse existing hardlinks instead
    // of allowing an alias to a credential or an outside file. No symlink traversal.
    for (index, entry) in walkdir::WalkDir::new(&root).follow_links(false).into_iter().enumerate() {
        anyhow::ensure!(index < 20_000, "Sandbox workspace exceeds 20,000 entries; choose a smaller workspace");
        let entry = entry.context("Inspect sandbox workspace")?;
        let metadata = entry.metadata().context("Inspect sandbox workspace entry")?;
        anyhow::ensure!(!metadata.is_file() || metadata.nlink() == 1,
            "Sandbox workspace contains a hardlinked file; remove the alias before running commands");
    }
    let mut policy = POLICY.to_owned();
    let mut command = Command::new("/usr/bin/sandbox-exec");
    let root_text = root.to_str().context("Sandbox workspace must be UTF-8")?;
    command.arg("-D").arg(format!("WORKSPACE={root_text}"));
    // Match reserved components relative to the selected workspace. A checkout
    // underneath ~/.codex/worktrees must still be writable. Pass regexes as data.
    let escaped_root: String = root_text.chars().map(|c| {
        if r".[]{}()^$*+?|\".contains(c) { format!("\\{c}") } else { c.to_string() }
    }).collect();
    for (index, (operation, names)) in [
        ("file-write*", &[".git", ".codex", ".agents", "portal.toml", "desktop-portal.toml", "mcp.toml"][..]),
        ("file-read* file-write*", &[".ssh", ".portal-client.json", "connection.json"][..]),
    ].into_iter().enumerate() {
        let key = format!("PROTECTED_{index}");
        let names = names.iter().map(|name| insensitive(name)).collect::<Vec<_>>().join("|");
        command.arg("-D").arg(format!("{key}=^{escaped_root}/(.*/)?({names})(/|$)"));
        policy.push_str(&format!("(deny {operation} (regex (param \"{key}\")))\n"));
    }
    command.arg("-D").arg(format!("ENV_FILES=^{escaped_root}/(.*/)?{}[^/]*(/|$)", insensitive(".env")));
    policy.push_str("(deny file-read* file-write* (regex (param \"ENV_FILES\")))\n");
    // Protect a custom active config even when its filename is not reserved.
    // Also prevent moving an ancestor and escaping the literal path restriction.
    if let Some(config_path) = &config.config_path {
        let protected = config_path.canonicalize().context("Resolve protected sandbox config")?;
        command.arg("-D").arg(format!("CONFIG={}", protected.to_str().context("Sandbox config path must be UTF-8")?));
        policy.push_str("(deny file-read* file-write* (literal (param \"CONFIG\")))\n");
        for (index, ancestor) in protected.ancestors().skip(1).take_while(|p| p.starts_with(&root)).enumerate() {
            let key = format!("CONFIG_PARENT_{index}");
            command.arg("-D").arg(format!("{key}={}", ancestor.to_str().context("Sandbox path must be UTF-8")?));
            policy.push_str(&format!("(deny file-write-unlink (literal (param \"{key}\")))\n"));
        }
    }
    command.args(["-p", &policy, "/bin/sh", "-c", source]).current_dir(&cwd).env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin:/usr/local/bin")
        .env("HOME", &root).env("TMPDIR", &root).env("LANG", "en_US.UTF-8");
    Ok(command)
}

#[cfg(test)]
#[path = "macos_sandbox_tests.rs"]
mod tests;
