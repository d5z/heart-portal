//! Production logging: metadata only, plain files, bounded runtime retention.
use std::fs::{File, OpenOptions};
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::sync::Mutex;

const LIMIT: u64 = 10 * 1024 * 1024;

struct RotatingFile {
    path: PathBuf,
    limit: u64,
}

impl Write for RotatingFile {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        // Open per event: no stale descriptor after rename, including Windows.
        // Keep one previous generation, matching the existing log exporter.
        let size = std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);
        if size > 0 && size.saturating_add(bytes.len() as u64) > self.limit {
            let previous = PathBuf::from(format!("{}.previous", self.path.display()));
            match std::fs::remove_file(&previous) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            std::fs::rename(&self.path, previous)?;
        }
        let mut file = private_append(&self.path)?;
        // A single pathological event must not bypass the file cap.
        file.write_all(&bytes[..bytes.len().min(self.limit as usize)])?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn private_append(path: &PathBuf) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

// Legacy launchd registrations redirect stdout without setting an environment
// variable. Only adopt regular files; terminals, sockets and supervisor pipes
// must keep their original stream behavior.
#[cfg(target_os = "macos")]
fn redirected_stdout() -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: both output buffers have the sizes required by the libc APIs.
    unsafe {
        if libc::fstat(libc::STDOUT_FILENO, stat.as_mut_ptr()) != 0 {
            return None;
        }
        if stat.assume_init().st_mode & libc::S_IFMT != libc::S_IFREG {
            return None;
        }
        let mut path = [0u8; libc::PATH_MAX as usize];
        if libc::fcntl(libc::STDOUT_FILENO, libc::F_GETPATH, path.as_mut_ptr()) != 0 {
            return None;
        }
        let end = path.iter().position(|b| *b == 0)?;
        Some(PathBuf::from(std::ffi::OsStr::from_bytes(&path[..end])))
    }
}

#[cfg(not(target_os = "macos"))]
fn redirected_stdout() -> Option<PathBuf> {
    None
}

pub fn init() -> anyhow::Result<()> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let subscriber = tracing_subscriber::fmt().with_env_filter(filter);
    let file = std::env::var_os("HEART_PORTAL_LOG_FILE")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(redirected_stdout);
    if let Some(path) = file {
        private_append(&path)?;
        subscriber
            .with_ansi(false)
            .with_writer(Mutex::new(RotatingFile { path, limit: LIMIT }))
            .init();
    } else {
        subscriber
            .with_ansi(io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none())
            .init();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "macos")]
    #[test]
    fn detects_legacy_launchd_file_redirect() {
        const CHILD: &str = "PORTAL_LOG_REDIRECT_TEST";
        if let Some(expected) = std::env::var_os(CHILD) {
            assert_eq!(redirected_stdout(), Some(PathBuf::from(expected)));
            return;
        }
        let path =
            std::env::temp_dir().join(format!("portal-redirect-{}.log", uuid::Uuid::new_v4()));
        let path = path
            .parent()
            .unwrap()
            .canonicalize()
            .unwrap()
            .join(path.file_name().unwrap());
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "logging::tests::detects_legacy_launchd_file_redirect",
                "--nocapture",
            ])
            .env(CHILD, &path)
            .stdout(File::create(&path).unwrap())
            .status()
            .unwrap();
        std::fs::remove_file(path).unwrap();
        assert!(status.success());
    }

    #[test]
    fn rotates_during_one_run_and_preserves_latest_generation() {
        let root = std::env::temp_dir().join(format!("portal-log-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("portal.log");
        let mut writer = RotatingFile {
            path: path.clone(),
            limit: 12,
        };
        writer.write_all(b"first event\n").unwrap();
        writer.write_all(b"next event\n").unwrap();
        assert_eq!(
            std::fs::read(root.join("portal.log.previous")).unwrap(),
            b"first event\n"
        );
        writer.write_all(b"last event\n").unwrap();
        assert_eq!(
            std::fs::read(root.join("portal.log.previous")).unwrap(),
            b"next event\n"
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"last event\n");
        writer.write_all(&[b'x'; 100]).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 12);
        assert_eq!(
            std::fs::read(root.join("portal.log.previous")).unwrap(),
            b"last event\n"
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
