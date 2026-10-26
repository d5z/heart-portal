//! All shell entry points use this constructor. Failure never retries on the host.
use crate::{config::{PermissionMode, PortalConfig}, exec_policy::{configure_shell_command, ExecShell}};
use anyhow::Result;
use tokio::process::Command;

pub fn shell_command(config: &PortalConfig, source: &str, workdir: &str, shell: ExecShell) -> Result<Command> {
    if config.security.permission_mode == PermissionMode::FullAccess {
        let mut command = Command::new(shell.program());
        configure_shell_command(&mut command, source, config, workdir, shell);
        return Ok(command);
    }
    anyhow::ensure!(config.security.permission_mode != PermissionMode::ReadOnly, "Shell execution is disabled in read-only mode");
    #[cfg(windows)]
    {
        use base64::Engine;
        let root = config.security.workspace_root.canonicalize()?;
        let cwd = std::path::Path::new(workdir).canonicalize()?;
        anyhow::ensure!(cwd.starts_with(&root), "Sandbox workdir must be inside the workspace");
        let payload = crate::windows_sandbox::Request { root, cwd, source: source.into(),
            powershell: shell == ExecShell::PowerShell, protected_paths: config.config_path.iter().cloned().collect() };
        let encoded = base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(&payload)?);
        anyhow::ensure!(encoded.len() < 28000, "Sandbox command is too large; use a workspace script");
        let mut command = Command::new(std::env::current_exe()?);
        command.args(["sandbox-run", &encoded]).env_clear();
        for key in ["SystemRoot", "WINDIR", "USERPROFILE", "LOCALAPPDATA", "APPDATA", "TEMP", "TMP"] {
            if let Some(value) = std::env::var_os(key) { command.env(key, value); }
        }
        command.creation_flags(0x08000000);
        return Ok(command);
    }
    #[cfg(not(windows))]
    anyhow::bail!("OS sandbox is not available on this platform; no unsandboxed fallback")
}
