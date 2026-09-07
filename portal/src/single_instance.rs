//! Best-effort per-relay single-instance guard.
//!
//! On Windows a named kernel mutex survives process crashes cleanly: the OS
//! releases it when the owning process exits. This prevents an old Portal
//! process and a newly started process from competing for the same relay.
//! macOS uses a per-user advisory file lock, also released by the OS on exit.

#[cfg(windows)]
use std::ffi::c_void;

#[cfg(windows)]
const ERROR_ALREADY_EXISTS: u32 = 183;

/// Guard held for the lifetime of the Portal process.
pub struct Guard {
    #[cfg(windows)]
    handle: *mut c_void,
    #[cfg(target_os = "macos")]
    _file: std::fs::File,
}

// The mutex handle is process-local and is only closed when the guard drops.
#[cfg(windows)]
unsafe impl Send for Guard {}
#[cfg(windows)]
unsafe impl Sync for Guard {}

impl Drop for Guard {
    fn drop(&mut self) {
        #[cfg(windows)]
        unsafe {
            if !self.handle.is_null() {
                CloseHandle(self.handle);
            }
        }
    }
}

/// Acquire a mutex scoped to the relay/Being identity.
///
/// The identity is hashed before use, so the URL/token is never present in the
/// mutex name or exposed through OS diagnostics.
pub fn acquire(identity: Option<&str>) -> Result<Guard, String> {
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = identity;
        Ok(Guard {})
    }

    #[cfg(target_os = "macos")]
    {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
        // Use a stable per-user location across checkouts, token rotations and
        // launchd/terminal environments. Never unlink a lock: another process
        // may already have the same inode open while waiting to acquire it.
        let uid = unsafe { libc::geteuid() };
        let dir = std::path::PathBuf::from(format!("/tmp/heart-portal-{uid}"));
        match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(format!("could not create instance lock directory: {e}")),
        }
        let metadata = std::fs::symlink_metadata(&dir).map_err(|e| e.to_string())?;
        if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
            return Err("instance lock directory must be private and owned by this user".into());
        }
        let path = dir.join(format!(
            "{:016x}.lock",
            stable_identity_hash(identity.unwrap_or("standalone"))
        ));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
            .map_err(|e| format!("could not open instance lock: {e}"))?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::WouldBlock {
                return Err(
                    "another Portal instance is already running for this relay/Being".into(),
                );
            }
            return Err(format!("could not acquire instance lock: {error}"));
        }
        Ok(Guard { _file: file })
    }

    #[cfg(windows)]
    {
        let identity = identity.unwrap_or("standalone");
        let name = format!(
            "Local\\heart-portal-{:016x}",
            stable_identity_hash(identity)
        );
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();

        let handle = unsafe { CreateMutexW(std::ptr::null_mut(), 0, wide.as_ptr()) };
        if handle.is_null() {
            return Err("could not create Windows single-instance mutex".to_string());
        }
        let already_exists = unsafe { GetLastError() == ERROR_ALREADY_EXISTS };
        if already_exists {
            unsafe { CloseHandle(handle) };
            return Err(
                "another Portal instance is already running for this relay/Being".to_string(),
            );
        }
        Ok(Guard { handle })
    }
}

/// FNV-1a is intentionally simple and stable across Rust/compiler versions.
/// That matters while an old binary and a newly upgraded binary overlap.
#[cfg(any(windows, target_os = "macos", test))]
fn stable_identity_hash(identity: &str) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in identity.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn CreateMutexW(attributes: *mut c_void, initial_owner: i32, name: *const u16) -> *mut c_void;
    fn GetLastError() -> u32;
    fn CloseHandle(handle: *mut c_void) -> i32;
}

#[cfg(test)]
mod tests {
    use super::stable_identity_hash;

    #[test]
    fn identity_hash_is_stable() {
        assert_eq!(stable_identity_hash("a"), 0xaf63dc4c8601ec8c);
    }

    #[test]
    fn identity_hash_changes_with_relay_identity() {
        assert_ne!(
            stable_identity_hash("relay/a"),
            stable_identity_hash("relay/b")
        );
    }

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn duplicate_is_rejected_until_guard_is_dropped() {
        let identity = format!("test/{}", uuid::Uuid::new_v4());
        let guard = super::acquire(Some(&identity)).unwrap();
        assert!(super::acquire(Some(&identity)).is_err());
        drop(guard);
        assert!(super::acquire(Some(&identity)).is_ok());
    }
}
