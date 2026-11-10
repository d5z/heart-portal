//! Adopt the original foreground process without changing its TCC responsibility.
//! The detached watcher inherits that same launch origin and restarts signed code.
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::OnceLock;

use anyhow::{Context, Result};

static TOKEN: OnceLock<String> = OnceLock::new();

pub fn attached() -> bool {
    TOKEN.get().is_some()
}

pub async fn start(config: &str, connect: Option<&str>, name: Option<&str>, overrides: &[String]) -> Result<()> {
    if std::env::var("HEART_PORTAL_SUPERVISED").as_deref() == Ok("1") {
        return Ok(());
    }
    let mut arguments = Vec::new();
    if std::path::Path::new(config).is_file() {
        arguments.extend([
            "--config".to_owned(),
            std::path::absolute(config)?.to_string_lossy().into_owned(),
        ]);
    }
    if let Some(name) = name {
        arguments.extend(["--name".to_owned(), name.to_owned()]);
    }
    arguments.extend_from_slice(overrides);
    let token = run("start", arguments, connect).await?;
    TOKEN.set(token.trim().to_owned()).ok();
    Ok(())
}

pub async fn action(action: &str) -> Result<()> {
    print!("{}", run(action, Vec::new(), None).await?);
    Ok(())
}

pub fn stop_on_interrupt() {
    let token = TOKEN
        .get()
        .cloned()
        .or_else(|| std::env::var("HEART_PORTAL_MACOS_SUPERVISOR").ok());
    if let (Some(token), Ok(exe)) = (token, std::env::current_exe()) {
        if let Ok(root) = crate::macos_upgrade::installation_root(&exe) {
            let _ = private_write(&root.join(".portal-supervisor-stop"), token.as_bytes());
        }
    }
}

fn private_write(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4().simple()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(temporary, path)?;
    Ok(())
}

async fn run(action: &str, arguments: Vec<String>, connect: Option<&str>) -> Result<String> {
    let target = std::env::current_exe()?.canonicalize()?;
    let root = crate::macos_upgrade::installation_root(&target)?;
    let token = uuid::Uuid::new_v4().simple().to_string();
    let support = root.join(".portal-supervisor");
    std::fs::create_dir_all(&support)
        .context("Portal supervision needs a writable installation directory")?;
    std::fs::set_permissions(&support, std::fs::Permissions::from_mode(0o700))?;
    for (name, bytes) in [
        ("portal-macos.py", include_bytes!("../../scripts/portal-macos.py").as_slice()),
        ("portal-macos-supervisor.py", include_bytes!("../../scripts/portal-macos-supervisor.py").as_slice()),
    ] {
        let path = support.join(name);
        if crate::bounded_file::metadata_bytes(&path).ok().as_deref() != Some(bytes) {
            private_write(&path, bytes)?;
        }
    }
    let request = serde_json::to_vec(&serde_json::json!({
        "root": root, "target": target, "arguments": arguments, "token": token,
        "cwd": std::env::current_dir()?, "runtime_pid": std::process::id()
    }))?;
    let python = crate::bounded_file::optional_text(&root.join(".portal-python"))?
        .map(|s| PathBuf::from(s.trim()))
        .unwrap_or_else(|| PathBuf::from("/usr/bin/python3"));
    anyhow::ensure!(
        python.is_absolute() && python.is_file(),
        "Portal supervision requires Python 3.9+; install it or update .portal-python"
    );
    let mut command = tokio::process::Command::new(python);
    command
        .arg(support.join("portal-macos-supervisor.py"))
        .arg(action)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(link) = connect {
        command.env("PORTAL_CONNECT_LINK", link);
    }
    use tokio::io::AsyncWriteExt;
    let mut child = command.spawn()?;
    child.stdin.take().context("Supervisor input is unavailable")?.write_all(&request).await?;
    let output = tokio::time::timeout(std::time::Duration::from_secs(40), child.wait_with_output()).await
        .context("Portal supervision timed out; inspect portal-supervisor.log and run heart-portal status")??;
    anyhow::ensure!(
        output.status.success(),
        "Portal supervision failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8(output.stdout)?)
}
