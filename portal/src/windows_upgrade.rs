//! Windows must release the calling exe before replacing it. A detached
//! PowerShell worker owns the transaction; the CLI reports acceptance, while
//! `upgrade --status` reports the eventual result.

use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

const LIFECYCLE: &str = include_str!("../../scripts/portal-lifecycle.ps1");
const WORKER: &str = include_str!("../../scripts/portal-upgrade-worker.ps1");

struct DetachedWorker(OwnedHandle);

impl DetachedWorker {
    fn try_wait(&self) -> Result<Option<u32>> {
        use windows_sys::Win32::{
            Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT},
            System::Threading::{GetExitCodeProcess, WaitForSingleObject},
        };
        let handle = self.0.as_raw_handle();
        match unsafe { WaitForSingleObject(handle, 0) } {
            WAIT_TIMEOUT => Ok(None),
            WAIT_OBJECT_0 => {
                let mut code = 0;
                if unsafe { GetExitCodeProcess(handle, &mut code) } == 0 {
                    return Err(std::io::Error::last_os_error().into());
                }
                Ok(Some(code))
            }
            _ => Err(std::io::Error::last_os_error().into()),
        }
    }

    fn terminate(&self) {
        use windows_sys::Win32::System::Threading::{TerminateProcess, WaitForSingleObject};
        unsafe {
            TerminateProcess(self.0.as_raw_handle(), 1);
            WaitForSingleObject(self.0.as_raw_handle(), 5000);
        }
    }
}

fn spawn_worker(worker: &Path, root: &Path, recover: bool) -> Result<DetachedWorker> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::System::Threading::{
        CreateProcessW, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, PROCESS_INFORMATION,
        STARTUPINFOW,
    };
    let powershell =
        PathBuf::from(std::env::var_os("SystemRoot").context("SystemRoot is missing")?)
            .join("System32/WindowsPowerShell/v1.0/powershell.exe");
    // These are file paths (Windows forbids quotes in their components), not
    // shell source. Explicit application name prevents ambiguous path parsing.
    let mut line = format!(
        "\"{}\" -NoProfile -NonInteractive -ExecutionPolicy Bypass -File \"{}\"",
        powershell.to_str().context("PowerShell path must be Unicode")?,
        worker.to_str().context("Worker path must be Unicode")?
    );
    if recover {
        line.push_str(&format!(" -Recover -ParentProcessId {}", std::process::id()));
    }
    let mut line: Vec<u16> = line.encode_utf16().chain(Some(0)).collect();
    let application: Vec<u16> = powershell.as_os_str().encode_wide().chain(Some(0)).collect();
    let directory: Vec<u16> = root.as_os_str().encode_wide().chain(Some(0)).collect();
    let startup = STARTUPINFOW { cb: std::mem::size_of::<STARTUPINFOW>() as u32, ..Default::default() };
    let mut process = PROCESS_INFORMATION::default();
    // Command::spawn inherits other inheritable handles on Windows even when
    // its stdio is null. A detached updater must inherit NO CLI pipe or lock.
    // https://learn.microsoft.com/windows/win32/api/processthreadsapi/nf-processthreadsapi-createprocessw
    if unsafe {
        CreateProcessW(application.as_ptr(), line.as_mut_ptr(), std::ptr::null(),
            std::ptr::null(), 0, CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP,
            std::ptr::null(), directory.as_ptr(), &startup, &mut process)
    } == 0 {
        return Err(std::io::Error::last_os_error()).context("Starting Windows upgrade worker");
    }
    // CreateProcess returned valid owned handles; closing them does not stop it.
    unsafe {
        drop(OwnedHandle::from_raw_handle(process.hThread));
        Ok(DetachedWorker(OwnedHandle::from_raw_handle(process.hProcess)))
    }
}

pub fn export_launcher(path: &Path) -> Result<()> {
    std::fs::write(path, include_bytes!(concat!(env!("OUT_DIR"), "/portal-background-v1.exe")))?;
    Ok(())
}

pub fn export_runtime(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)?;
    export_launcher(&path.join("portal-background-v1.exe"))?;
    for (name, source) in [
        ("portal-lifecycle.ps1", LIFECYCLE),
        (
            "portal-supervisor.ps1",
            include_str!("../../scripts/portal-supervisor.ps1"),
        ),
        (
            "portal-supervisor-hidden.vbs",
            include_str!("../../scripts/portal-supervisor-hidden.vbs"),
        ),
        (
            "portal-supervisor-bootstrap.ps1",
            include_str!("../../scripts/portal-supervisor-bootstrap.ps1"),
        ),
    ] {
        std::fs::write(path.join(name), source)?;
    }
    Ok(())
}

pub fn installation_root(exe: &Path) -> Result<PathBuf> {
    let parent = exe.parent().context("Executable has no parent directory")?;
    if !matches!(
        exe.file_name().and_then(|s| s.to_str()),
        Some(
            "heart-portal.exe"
                | "heart-portal-windows-x86_64.exe"
                | "heart-portal-windows-aarch64.exe"
        )
    ) {
        bail!("Use the release executable name or heart-portal.exe");
    }
    if parent.file_name().and_then(|s| s.to_str()) == Some("release")
        && parent
            .parent()
            .and_then(Path::file_name)
            .and_then(|s| s.to_str())
            == Some("target")
    {
        return Ok(parent
            .parent()
            .and_then(Path::parent)
            .context("Invalid installation path")?
            .to_path_buf());
    }
    Ok(parent.to_path_buf())
}

/// Normal direct launches obey the same gate as the PowerShell supervisor.
/// A manual launch during replacement exits safely instead of reopening the exe.
pub fn startup_guard() -> Result<Option<std::fs::File>> {
    // The supervisor already owns the gate across CreateProcess. Acquiring it
    // again in that child would race the supervisor publishing its PID.
    if std::env::var("HEART_PORTAL_SUPERVISED").as_deref() == Ok("1") {
        return Ok(None);
    }
    let root = installation_root(&std::env::current_exe()?)?;
    std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false)
        .share_mode(0).open(root.join(".portal-lifecycle.lock"))
        .map(Some).context("Portal maintenance is in progress, or the executable directory is not writable; retry after upgrade completes")
}

pub fn recover_interrupted() -> Result<bool> {
    if std::env::var("HEART_PORTAL_SUPERVISED").as_deref() == Ok("1") {
        return Ok(false);
    }
    let root = installation_root(&std::env::current_exe()?)?;
    let journal_path = root.join(".portal-upgrade.json");
    if !journal_path.exists() {
        return Ok(false);
    }
    crate::windows_private::protect_installation(&root)?;
    let owner = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .open(root.join(".portal-upgrade.lock"))
    {
        Ok(file) => file,
        Err(error) if matches!(error.raw_os_error(), Some(32 | 33)) => return Ok(false),
        Err(error) => return Err(error).context("Checking interrupted upgrade"),
    };
    let journal: serde_json::Value = serde_json::from_slice(&crate::bounded_file::metadata_bytes(&journal_path)?)?;
    let worker = PathBuf::from(
        journal["recovery_script"]
            .as_str()
            .context("Upgrade journal has no recovery worker")?,
    )
    .canonicalize()?;
    let stage = root.join(".portal-upgrades").canonicalize()?;
    anyhow::ensure!(
        worker.starts_with(&stage)
            && worker.file_name().and_then(|name| name.to_str())
                == Some("portal-upgrade-worker.ps1"),
        "Invalid recovery worker path"
    );
    spawn_worker(&worker, &root, true).context("Starting interrupted-upgrade recovery")?;
    drop(owner);
    eprintln!("Recovering an interrupted upgrade. Portal will restart; check upgrade --status.");
    Ok(true)
}

pub(crate) fn write_json(path: &Path, value: &serde_json::Value) -> Result<()> {
    use std::io::Write;
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };
    let temp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4().simple()));
    let result = (|| -> Result<()> {
        let mut file = crate::windows_private::create(&temp)?;
        file.write_all(&serde_json::to_vec(value)?)?;
        file.sync_all()?;
        Ok(())
    })();
    if let Err(error) = result {
        let _ = std::fs::remove_file(&temp);
        return Err(error);
    }
    let from: Vec<u16> = temp.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    if unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        let error = std::io::Error::last_os_error();
        let _ = std::fs::remove_file(&temp);
        return Err(error).context("Publishing Portal runtime state");
    }
    Ok(())
}

pub fn publish_ready() -> Result<()> {
    let exe = std::env::current_exe()?;
    let root = installation_root(&exe)?;
    // The first upgrade may be driven by old code and start us directly under
    // its supervisor. Repair legacy copies on this path as well as CLI starts.
    crate::windows_private::protect_installation(&root)?;
    let nonce = std::env::var("HEART_PORTAL_READY_NONCE")
        .unwrap_or_else(|_| uuid::Uuid::new_v4().simple().to_string());
    let ready_path = std::env::var_os("HEART_PORTAL_READY_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join(".portal-ready.json"));
    if std::env::var("HEART_PORTAL_SUPERVISED").as_deref() != Ok("1") {
        use windows_sys::Win32::{
            Foundation::FILETIME,
            System::Threading::{GetCurrentProcess, GetProcessTimes},
        };
        let mut created = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let mut exited = created;
        let mut kernel = created;
        let mut user = created;
        if unsafe {
            GetProcessTimes(
                GetCurrentProcess(),
                &mut created,
                &mut exited,
                &mut kernel,
                &mut user,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error()).context("Reading Portal creation time");
        }
        let started = ((u64::from(created.dwHighDateTime) << 32)
            | u64::from(created.dwLowDateTime))
            + 504_911_232_000_000_000;
        let environment: std::collections::BTreeMap<String, String> = [
            "PORTAL_CONNECT_LINK",
            "PORTAL_MCP_TOKEN",
            "PATH",
            "RUST_LOG",
        ]
        .iter()
        .filter_map(|key| {
            std::env::var(key)
                .ok()
                .map(|value| (key.to_string(), value))
        })
        .collect();
        write_json(
            &root.join(".portal-direct.json"),
            &serde_json::json!({
                "arguments": std::env::args().skip(1).collect::<Vec<_>>(),
                "working_directory": std::env::current_dir()?, "environment": environment, "nonce": nonce
            }),
        )?;
        write_json(
            &root.join(".portal-runtime.json"),
            &serde_json::json!({
                "protocol": 1, "pid": std::process::id(), "started": started, "nonce": nonce
            }),
        )?;
        let relative = exe
            .strip_prefix(&root)?
            .to_str()
            .context("Executable path must be Unicode")?;
        std::fs::write(root.join(".portal-executable"), relative)?;
    }
    write_json(
        &ready_path,
        &serde_json::json!({ "pid": std::process::id(), "nonce": nonce, "version": crate::upgrade::PORTAL_VERSION }),
    )
}

pub fn show_status() -> Result<()> {
    let root = installation_root(&std::env::current_exe()?)?;
    let path = root.join(".portal-upgrade-status.json");
    if !path.exists() {
        println!("No upgrade has been recorded for {}", root.display());
        return Ok(());
    }
    let status: serde_json::Value = serde_json::from_slice(&crate::bounded_file::metadata_bytes(&path)?)?;
    println!("{}", serde_json::to_string_pretty(&status)?);
    if matches!(
        status["state"].as_str(),
        Some("failed" | "rolled_back" | "recovery_required")
    ) {
        bail!("The last upgrade did not succeed; see the status above");
    }
    Ok(())
}

pub async fn upgrade_file(path: &Path) -> Result<()> {
    let path = path
        .canonicalize()
        .context("Finding the downloaded upgrade exe")?;
    let mut command = tokio::process::Command::new(&path);
    command
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(0x0800_0000)
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(15), command.output())
        .await
        .context("Candidate version check timed out")?
        .context("Candidate is not a runnable Windows exe")?;
    anyhow::ensure!(output.status.success(), "Candidate version check failed");
    let text = String::from_utf8(output.stdout).context("Invalid candidate version output")?;
    let version = text
        .trim()
        .strip_prefix("heart-portal ")
        .context("Candidate is not a Heart Portal release")?;
    anyhow::ensure!(
        crate::upgrade::compare_versions(version, crate::upgrade::PORTAL_VERSION)
            == std::cmp::Ordering::Greater,
        "Candidate {version} must be newer than the running version {}",
        crate::upgrade::PORTAL_VERSION
    );
    let bytes = tokio::fs::read(&path)
        .await
        .context("Reading the downloaded upgrade")?;
    handoff(&bytes, version).await
}

pub async fn handoff(bytes: &[u8], version: &str) -> Result<()> {
    let target = std::env::current_exe().context("Finding installed executable")?;
    let root = installation_root(&target)?;
    crate::windows_private::protect_installation(&root)?;
    let staging = root
        .join(".portal-upgrades")
        .join(uuid::Uuid::new_v4().simple().to_string());
    std::fs::create_dir_all(&staging)
        .context("Creating upgrade staging directory; run as the installing user")?;
    let candidate = staging.join("heart-portal.exe");
    let ack = staging.join("accepted.json");
    let error = staging.join("error.json");
    std::fs::write(&candidate, bytes)?;
    std::fs::write(staging.join("portal-lifecycle.ps1"), LIFECYCLE)?;
    let worker = staging.join("portal-upgrade-worker.ps1");
    std::fs::write(&worker, WORKER)?;
    let request = serde_json::json!({
        "root": root, "target": target, "candidate": candidate,
        "version": version, "sha256": format!("{:x}", Sha256::digest(bytes)),
        "parent_pid": std::process::id(), "ack": ack, "error": error,
    });
    write_json(&staging.join("request.json"), &request)?;
    let child = spawn_worker(&worker, &root, false)?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        if error.exists() {
            let body: serde_json::Value = serde_json::from_slice(&crate::bounded_file::metadata_bytes(&error)?)?;
            bail!(
                "Upgrade rejected: {}",
                body["message"].as_str().unwrap_or("unknown error")
            );
        }
        if ack.exists() {
            eprintln!(
                "Upgrade accepted. This CLI will exit so Windows can replace the executable."
            );
            eprintln!("Check the result with: heart-portal.exe upgrade --status");
            eprintln!(
                "Status file: {}",
                root.join(".portal-upgrade-status.json").display()
            );
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            bail!(
                "Upgrade worker exited before accepting the request ({status}); inspect {}",
                staging.display()
            );
        }
        if tokio::time::Instant::now() >= deadline {
            // No accepted handoff: prevent a late worker from applying after
            // reporting rejection. Before ack, it has not stopped the runtime.
            child.terminate();
            bail!("Timed out waiting for updater acceptance; retry upgrade");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upgrade_uses_installed_exe_instead_of_home_or_current_directory() {
        assert_eq!(
            installation_root(Path::new(r"C:\Portal\heart-portal.exe")).unwrap(),
            Path::new(r"C:\Portal")
        );
        assert_eq!(
            installation_root(Path::new(r"D:\代码 test\target\release\heart-portal.exe")).unwrap(),
            Path::new(r"D:\代码 test")
        );
        assert_eq!(
            installation_root(Path::new(r"C:\Downloads\heart-portal-windows-x86_64.exe")).unwrap(),
            Path::new(r"C:\Downloads")
        );
    }
}
