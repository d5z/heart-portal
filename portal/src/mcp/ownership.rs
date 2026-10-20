//! Own only this MCP server's process tree. This is lifecycle containment,
//! not a filesystem/network sandbox or a boundary against hostile same-user code.
use anyhow::Result;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::process::{Child, Command};

pub(crate) struct ProcessOwner {
    stopped: AtomicBool,
    #[cfg(windows)]
    job: super::windows_job::KitJob,
    #[cfg(unix)]
    group: i32,
}

impl ProcessOwner {
    #[cfg(unix)]
    fn exit_observed(&self) -> std::io::Result<bool> {
        loop {
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let result = unsafe {
                libc::waitid(
                    libc::P_PID,
                    self.group as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if result == 0 {
                return Ok(unsafe { info.si_pid() } != 0);
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }

    #[cfg(unix)]
    pub async fn wait_unreaped(&self) -> std::io::Result<()> {
        while !self.exit_observed()? {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        Ok(())
    }

    pub fn spawn(command: &mut Command) -> Result<(Child, Self)> {
        Self::spawn_owned(command, true)
    }

    pub(crate) fn spawn_exec(command: &mut Command) -> Result<(Child, Self)> {
        Self::spawn_owned(command, false)
    }

    fn spawn_owned(command: &mut Command, _kit: bool) -> Result<(Child, Self)> {
        #[cfg(windows)]
        {
            let (child, job) = if _kit {
                super::windows_job::KitJob::spawn(command)?
            } else {
                super::windows_job::KitJob::spawn_exec(command)?
            };
            Ok((
                child,
                Self {
                    stopped: AtomicBool::new(false),
                    job,
                },
            ))
        }
        #[cfg(unix)]
        {
            // A dedicated group keeps ordinary launcher descendants together.
            command.process_group(0);
            let child = command.spawn()?;
            let group = child
                .id()
                .ok_or_else(|| anyhow::anyhow!("MCP process has no PID"))?
                as i32;
            Ok((
                child,
                Self {
                    stopped: AtomicBool::new(false),
                    group,
                },
            ))
        }
        #[cfg(not(any(windows, unix)))]
        anyhow::bail!("MCP process ownership is unsupported on this platform")
    }

    pub fn terminate(&self) {
        if self.stopped.swap(true, Ordering::AcqRel) {
            return;
        }
        #[cfg(windows)]
        self.job.terminate();
        #[cfg(unix)]
        {
            // McpConnection never waits/reaps before this one-shot cleanup.
            // The unreaped child pins its PID (and therefore its group number).
            // If ownership has already been lost, never signal a reusable ID.
            if self.exit_observed().is_ok() {
                unsafe {
                    libc::kill(-self.group, libc::SIGKILL);
                }
            }
        }
    }
}

impl Drop for ProcessOwner {
    fn drop(&mut self) {
        self.terminate();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn graceful_exit_keeps_the_leader_unreaped_until_group_cleanup() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "exit 0"]);
        let (mut child, owner) = ProcessOwner::spawn(&mut command).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), owner.wait_unreaped())
            .await
            .unwrap()
            .unwrap();
        assert!(owner.exit_observed().unwrap());
        owner.terminate();
        assert!(child.wait().await.unwrap().success());
        assert_eq!(
            owner.exit_observed().unwrap_err().raw_os_error(),
            Some(libc::ECHILD)
        );
        owner.terminate(); // Repeated cleanup cannot signal the now-reusable ID.
    }

    #[tokio::test]
    async fn lost_child_ownership_prevents_group_signalling() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "exit 0"]);
        let (mut child, owner) = ProcessOwner::spawn(&mut command).unwrap();
        child.wait().await.unwrap();
        assert_eq!(
            owner.exit_observed().unwrap_err().raw_os_error(),
            Some(libc::ECHILD)
        );
        owner.terminate();
    }
}
