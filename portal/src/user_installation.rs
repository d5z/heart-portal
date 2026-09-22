//! Keep the executable and its transactional runtime on the same filesystem.
//! Downloaded executables delegate to this stable, per-user installation.
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

pub fn root() -> Result<PathBuf> {
    Ok(crate::paths::data_dir()?.join("runtime"))
}

pub fn executable() -> Result<PathBuf> {
    Ok(root()?.join(if cfg!(windows) {
        "heart-portal.exe"
    } else {
        "heart-portal"
    }))
}

pub fn is_managed(exe: &Path) -> Result<bool> {
    // Also accept an explicitly installed binary elsewhere inside the user data
    // directory. Resolve links so a junction cannot hide an external install.
    let data = crate::paths::data_dir()?;
    Ok(data.is_dir() && exe.canonicalize()?.starts_with(data.canonicalize()?))
}

pub fn legacy_root(exe: &Path) -> Result<PathBuf> {
    #[cfg(windows)]
    return crate::windows_upgrade::installation_root(exe);
    #[cfg(target_os = "macos")]
    return crate::macos_upgrade::installation_root(exe);
}

pub fn has_legacy_state(exe: &Path) -> Result<bool> {
    let root = legacy_root(exe)?;
    Ok([
        ".portal-launch.json",
        ".portal-runtime.json",
        ".portal-supervisor.json",
        ".portal-launchagent-label",
        ".portal-task-name",
        ".portal-upgrade.json",
    ]
    .iter()
    .any(|name| root.join(name).exists()))
}

pub fn migrated_source(exe: &Path) -> Result<bool> {
    if !executable()?.is_file() {
        return Ok(false);
    }
    let old = legacy_root(exe)?;
    Ok(crate::bounded_file::text(&root()?.join(".portal-origin"), crate::bounded_file::CONFIG_LIMIT)
        .ok()
        .is_some_and(|saved| Path::new(saved.trim()) == old))
}

fn private_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)?;
        let metadata = std::fs::symlink_metadata(path)?;
        anyhow::ensure!(
            metadata.is_dir() && metadata.uid() == unsafe { libc::geteuid() },
            "Portal runtime directory must belong to the current user"
        );
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(windows)]
    {
        std::fs::create_dir_all(path)?;
        crate::windows_private::protect_directory(path)?;
    }
    anyhow::ensure!(
        !std::fs::symlink_metadata(path)?.file_type().is_symlink(),
        "Portal runtime directory must not be a symbolic link"
    );
    Ok(())
}

/// Called only for a normal launch or explicit installation, never --version,
/// config inspection, or an updater's candidate validation/runtime export.
pub fn prepare(source: &Path, explicit_config: Option<&str>) -> Result<PathBuf> {
    if is_managed(source)? {
        return Ok(source.to_path_buf());
    }
    if let Some(config) = explicit_config {
        crate::paths::locate_config(Some(Path::new(config)), &[])?;
    }
    let root = root()?;
    private_directory(&root)?;
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join(".install.lock"))?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        match lock.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(50))
            }
            Err(error) => return Err(error).context("Another Portal installation is in progress"),
        }
    }
    let target = executable()?;
    let old = legacy_root(source)?;
    anyhow::ensure!(
        !old.join(".portal-upgrade.json").exists(),
        "Recover the interrupted legacy upgrade before moving Portal into the user directory"
    );
    if has_legacy_state(source)? {
        ensure_legacy_stopped(&old)?;
    }
    // Freeze relative paths and saved identity through the checked config
    // migration. An already migrated source cannot replace later user edits.
    let location = crate::paths::locate_config(
        explicit_config.map(Path::new),
        &crate::paths::legacy_dirs()?,
    )?;
    let data = crate::paths::data_dir()?;
    if explicit_config.is_none()
        && !migrated_source(source)?
        && location.path.is_file()
        && location.path != data.join("portal.toml")
    {
        crate::paths::plan_migration_with_installation(&location.path, &data, None, Some(&old))?
            .apply()?;
    }
    // A stale download must never overwrite a running or upgraded executable.
    if target.exists() {
        anyhow::ensure!(
            target.is_file() && !std::fs::symlink_metadata(&target)?.file_type().is_symlink(),
            "Installed Portal executable must be a regular file"
        );
        return Ok(target);
    }

    let stage = root.join(format!(".install-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        #[cfg(windows)]
        {
            let mut output = crate::windows_private::create(&stage)?;
            std::io::copy(&mut std::fs::File::open(source)?, &mut output)?;
            output.sync_all()?;
        }
        #[cfg(target_os = "macos")]
        {
            // Preserve the signed image, permissions and extended attributes.
            // exec below retains the original Terminal/app launch responsibility.
            let status = std::process::Command::new("/bin/cp")
                .arg("-p")
                .arg(source)
                .arg(&stage)
                .status()?;
            anyhow::ensure!(
                status.success(),
                "Could not copy Portal into the user directory"
            );
            std::fs::File::open(&stage)?.sync_all()?;
        }
        std::fs::write(
            root.join(".portal-origin"),
            old.to_str().context("Installation path must be Unicode")?,
        )?;
        std::fs::hard_link(&stage, &target).context("Publishing the user Portal executable")?;
        Ok(())
    })();
    let _ = std::fs::remove_file(&stage);
    result?;
    Ok(target)
}

fn ensure_legacy_stopped(root: &Path) -> Result<()> {
    #[cfg(windows)]
    let output = {
        use std::os::windows::process::CommandExt;
        let script = format!(
            "{}\n{}",
            include_str!("../../scripts/portal-lifecycle.ps1"),
            r#"
$ErrorActionPreference = 'Stop'
$root = $env:HEART_PORTAL_MIGRATION_SOURCE
$runtime = Read-PortalJson (Join-Path $root '.portal-runtime.json')
$process = Get-PortalRecordedProcess $root $runtime
if ($process) { $process.Dispose(); exit 1 }
if (Test-SavedSupervisor $runtime) { exit 1 }
$taskFile = Join-Path $root '.portal-task-name'
if (Test-Path -LiteralPath $taskFile) {
    $name = (Read-PortalText $taskFile).Trim()
    $task = Get-ScheduledTask -ErrorAction Stop | Where-Object TaskName -eq $name
    if ($task -and $task.State -ne 'Disabled') { exit 1 }
}
"#
        );
        use std::io::Write;
        use std::process::Stdio;
        let mut child = std::process::Command::new(
            PathBuf::from(std::env::var_os("SystemRoot").context("SystemRoot is missing")?)
                .join("System32/WindowsPowerShell/v1.0/powershell.exe"),
        )
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "& ([ScriptBlock]::Create([Console]::In.ReadToEnd()))",
        ])
        .env("HEART_PORTAL_MIGRATION_SOURCE", root)
        .creation_flags(0x0800_0000)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
        child
            .stdin
            .take()
            .context("Migration check input is unavailable")?
            .write_all(script.as_bytes())?;
        child.wait_with_output()?
    };
    #[cfg(target_os = "macos")]
    let output = {
        let source = include_str!("../../scripts/portal-macos.py");
        let definitions = source.split("if __name__ == '__main__':").next().unwrap();
        let script = format!(
            "{definitions}\n{}",
            r#"
root = Path(sys.argv[1])
label = label_for(root)
domain = f'gui/{os.getuid()}'
service = domain + '/' + label
if supervisor_state(root) or checkout_pids(root, exclude=(int(sys.argv[2]),)) or launchctl('print', service, check=False).returncode == 0:
    sys.exit(1)
plist = Path.home() / 'Library/LaunchAgents' / (label + '.plist')
assert_owned(plist, root, label)
if plist.exists():
    disabled = launchctl('print-disabled', domain).stdout
    if not re.search('"' + re.escape(label) + r'"\s*=>\s*true', disabled):
        sys.exit(1)
"#
        );
        std::process::Command::new("/usr/bin/python3")
            .arg("-c")
            .arg(script)
            .arg(root)
            .arg(std::process::id().to_string())
            .output()?
    };
    anyhow::ensure!(output.status.success(),
        "The legacy Portal or its login supervisor is still active. Run this executable with stop, then start it again to migrate safely");
    Ok(())
}

pub fn delegate(target: &Path, cli: &crate::Cli) -> Result<()> {
    let mut command = std::process::Command::new(target);
    let explicit_config = cli.config.as_deref().or(cli.config_positional.as_deref());
    // Explicit configs retain their caller's working directory: legacy relative
    // kit paths are resolved there, and these configs are not migrated/frozen.
    // Default configs have frozen paths after migration, so their restarts can
    // use the durable runtime even after the download directory is removed.
    if let Some(config) = explicit_config {
        command
            .arg("--config")
            .arg(std::path::absolute(crate::paths::expand_home(Path::new(
                config,
            ))?)?);
    }
    if let Some(name) = &cli.name {
        command.arg("--name").arg(name);
    }
    if cli.logs { command.arg("--logs"); }
    if let Some(connect) = &cli.connect {
        command.env("PORTAL_CONNECT_LINK", connect);
    }
    match &cli.command {
        Some(crate::Commands::Stop) => {
            command.arg("stop");
        }
        Some(crate::Commands::Status) => {
            command.arg("status");
        }
        Some(crate::Commands::Upgrade {
            file,
            status,
            target,
        }) => {
            command.arg("upgrade");
            if *status {
                command.arg("--status");
            }
            for (flag, path) in [("--file", file), ("--target", target)] {
                if let Some(path) = path {
                    command
                        .arg(flag)
                        .arg(std::path::absolute(crate::paths::expand_home(path)?)?);
                }
            }
        }
        None if cli.legacy_upgrade => {
            command.arg("upgrade");
        }
        None => {}
        _ => anyhow::bail!("Only lifecycle commands delegate to the installed Portal"),
    }
    if explicit_config.is_none() {
        command.current_dir(root()?);
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::process::CommandExt;
        Err(command.exec()).context("Starting the user Portal installation")
    }
    #[cfg(windows)]
    {
        let status = command
            .status()
            .context("Starting the user Portal installation")?;
        std::process::exit(status.code().unwrap_or(1));
    }
}
