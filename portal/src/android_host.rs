//! The APK owns the runtime; never survive its parent or compete with another launch.
use anyhow::{Context, Result};
use std::os::fd::AsRawFd;

pub fn attach() -> Result<Option<std::fs::File>> {
    // Existing Android launchers and standalone CLI use remain supported.
    // APK hosts opt into parent lifetime ownership by supplying this variable.
    let Some(value) = std::env::var_os("HEART_PORTAL_PARENT_PID") else {
        return Ok(None);
    };
    let parent: libc::pid_t = value.to_str().context("Invalid Android parent")?.parse()?;
    anyhow::ensure!(parent > 1, "Invalid Android parent");
    // A killed app must not leave a relay client behind. Set before checking
    // getppid to cover death between ProcessBuilder.start and this instruction.
    anyhow::ensure!(
        unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) } == 0,
        "Cannot attach Portal lifetime"
    );
    anyhow::ensure!(
        unsafe { libc::getppid() } == parent,
        "Android parent exited"
    );
    let home = std::env::var("HOME").context("Missing private runtime home")?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(std::path::Path::new(&home).join("runtime.lock"))?;
    anyhow::ensure!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "Android Portal is already running"
    );
    Ok(Some(lock))
}
