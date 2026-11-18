//! All shell entry points use this constructor. Failure never retries on the host.
use crate::{config::{PermissionMode, PortalConfig}, exec_policy::{configure_shell_command, ExecShell}};
use anyhow::Result;
use tokio::process::Command;

pub fn shell_command(config: &PortalConfig, source: &str, workdir: &str, shell: ExecShell) -> Result<Command> {
    let path = std::path::Path::new(workdir);
    let directory = if path.is_absolute() { path.to_owned() } else { config.security.workspace_root.join(path) };
    let workdir = directory.to_str().ok_or_else(|| anyhow::anyhow!("Working directory must be UTF-8"))?;
    if config.security.permission_mode == PermissionMode::FullAccess {
        let mut command = Command::new(shell.program());
        configure_shell_command(&mut command, source, config, workdir, shell);
        if let Some(runtime) = crate::task_environment::bundled_runtime()? {
            let mut paths = vec![runtime.root.join(runtime.python.path).parent().unwrap().to_owned(),
                runtime.root.join(runtime.node.path).parent().unwrap().to_owned()];
            // Preserve the platform paths configure_shell_command already supplied.
            let configured_path = command.as_std().get_envs()
                .find_map(|(key, value)| (key == "PATH").then_some(value).flatten())
                .unwrap_or_default();
            paths.extend(std::env::split_paths(configured_path));
            command.env("PATH", std::env::join_paths(paths)?);
        }
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
        for key in ["HEART_PORTAL_CLIENT_MANAGED", "SystemRoot", "WINDIR", "USERPROFILE", "LOCALAPPDATA", "APPDATA", "TEMP", "TMP"] {
            if let Some(value) = std::env::var_os(key) { command.env(key, value); }
        }
        command.creation_flags(0x08000000);
        return Ok(command);
    }
    #[cfg(target_os = "macos")]
    return crate::macos_sandbox::shell_command(config, source, workdir);
    #[cfg(not(any(windows, target_os = "macos")))]
    anyhow::bail!("OS sandbox is not available on this platform; no unsandboxed fallback")
}
