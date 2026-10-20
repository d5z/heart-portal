//! Each stdio Kit owns a job; Portal itself and lifecycle workers are not members.
//! Closing Portal (including TerminateProcess) closes the non-inherited job handle
//! and kills the Kit tree, even while MCP initialization is still pending.

use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

use anyhow::{Context, Result};
use tokio::process::{Child, Command};
use windows_sys::Win32::{
    Foundation::INVALID_HANDLE_VALUE,
    System::{
        Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
        },
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
            SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        },
        Threading::{
            GetProcessIdOfThread, OpenThread, ResumeThread, CREATE_NO_WINDOW, CREATE_SUSPENDED,
            THREAD_QUERY_LIMITED_INFORMATION, THREAD_SUSPEND_RESUME,
        },
    },
};

pub struct KitJob(OwnedHandle);

impl KitJob {
    pub fn spawn(command: &mut Command) -> Result<(Child, Self)> {
        Self::spawn_owned(command, true)
    }

    pub(crate) fn spawn_exec(command: &mut Command) -> Result<(Child, Self)> {
        Self::spawn_owned(command, false)
    }

    fn spawn_owned(command: &mut Command, kit: bool) -> Result<(Child, Self)> {
        // NULL security attributes make the handle non-inheritable. Otherwise a
        // Kit could keep its own job alive after Portal has been terminated.
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        anyhow::ensure!(
            !handle.is_null(),
            "Creating Kit job: {}",
            std::io::Error::last_os_error()
        );
        let job = Self(unsafe { OwnedHandle::from_raw_handle(handle) });
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        // Kit descendants cannot detach from ownership. Portal lifecycle work
        // belongs to the host management channel, never a kit process.
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if kit {
            limits.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
            limits.BasicLimitInformation.ActiveProcessLimit = 32;
        }
        if unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of_val(&limits) as u32,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error()).context("Configuring Kit job");
        }

        // Suspend before user code executes so launchers cannot create children
        // between spawn and job assignment. Keep Rust's command quoting, PATH,
        // environment and .cmd support instead of rebuilding CreateProcess args.
        if kit {
            command.env("HEART_PORTAL_KIT_JOB", "1");
        }
        let mut child = command
            .creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED)
            .kill_on_drop(true)
            .spawn()
            .context("Spawning suspended Kit")?;
        let attach = (|| {
            let process = child.raw_handle().context("Kit has no process handle")?;
            if unsafe { AssignProcessToJobObject(job.0.as_raw_handle(), process) } == 0 {
                return Err(std::io::Error::last_os_error()).context("Assigning Kit to job");
            }
            resume_initial_thread(child.id().context("Kit has no PID")?)
        })();
        if let Err(error) = attach {
            // Fail closed: do not run a Kit without ownership. Drop also closes
            // the job if assignment succeeded but resuming the thread failed.
            let _ = child.start_kill();
            return Err(error);
        }
        Ok((child, job))
    }

    pub fn terminate(&self) {
        unsafe {
            TerminateJobObject(self.0.as_raw_handle(), 1);
        }
    }
}

fn resume_initial_thread(process_id: u32) -> Result<()> {
    // Tokio does not expose the primary thread handle. Capture every matching
    // handle before resuming: an injected runtime/AV thread may precede the
    // suspended primary thread in the snapshot. Never rely on enumeration order.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error()).context("Finding suspended Kit thread");
    }
    let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot) };
    let mut entry = THREADENTRY32 {
        dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
        ..Default::default()
    };
    let mut found = unsafe { Thread32First(snapshot.as_raw_handle(), &mut entry) };
    let mut threads = Vec::new();
    while found != 0 {
        if entry.th32OwnerProcessID == process_id {
            let thread = unsafe {
                OpenThread(
                    THREAD_SUSPEND_RESUME | THREAD_QUERY_LIMITED_INFORMATION,
                    0,
                    entry.th32ThreadID,
                )
            };
            if thread.is_null() {
                return Err(std::io::Error::last_os_error())
                    .context("Opening suspended Kit thread");
            }
            let thread = unsafe { OwnedHandle::from_raw_handle(thread) };
            anyhow::ensure!(
                unsafe { GetProcessIdOfThread(thread.as_raw_handle()) } == process_id,
                "Kit thread ownership changed during snapshot"
            );
            threads.push(thread);
        }
        entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
        found = unsafe { Thread32Next(snapshot.as_raw_handle(), &mut entry) };
    }
    resume_threads(threads, |thread| {
        let count = unsafe { ResumeThread(thread.as_raw_handle()) };
        if count == u32::MAX {
            return Err(std::io::Error::last_os_error()).context("Resuming Kit thread");
        }
        Ok(count)
    })
}

fn resume_threads<T>(
    threads: impl IntoIterator<Item = T>,
    mut resume: impl FnMut(T) -> Result<u32>,
) -> Result<()> {
    let mut resumed = false;
    for thread in threads {
        let count = resume(thread)?;
        anyhow::ensure!(count <= 1, "Kit thread remains suspended");
        resumed |= count == 1;
    }
    // Zero is acceptable only for auxiliary threads. CREATE_SUSPENDED must
    // contribute at least one 1 -> 0 transition or this spawn fails closed.
    anyhow::ensure!(resumed, "Suspended Kit initial thread was not found");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auxiliary_threads_do_not_hide_the_suspended_primary_thread() {
        for counts in [[0, 1, 0], [1, 0, 1]] {
            let mut visited = Vec::new();
            resume_threads(counts, |count| {
                visited.push(count);
                Ok(count)
            })
            .unwrap();
            assert_eq!(visited, counts);
        }
        for counts in [vec![], vec![0, 0], vec![1, 2]] {
            assert!(resume_threads(counts, Ok).is_err());
        }
    }

    #[tokio::test]
    async fn suspended_process_runs_only_after_job_assignment_and_resume() {
        let mut command = Command::new("cmd.exe");
        command.args(["/C", "exit", "0"]);
        let (mut child, _job) = KitJob::spawn(&mut command).unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(10), child.wait())
                .await
                .unwrap()
                .unwrap()
                .success()
        );
    }
}
