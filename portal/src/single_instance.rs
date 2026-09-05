//! Best-effort per-relay single-instance guard.
//!
//! On Windows a named kernel mutex survives process crashes cleanly: the OS
//! releases it when the owning process exits. This prevents an old Portal
//! process and a newly started process from competing for the same relay.

#[cfg(windows)]
use std::ffi::c_void;

#[cfg(windows)]
const ERROR_ALREADY_EXISTS: u32 = 183;

/// Guard held for the lifetime of the Portal process.
pub struct Guard {
    #[cfg(windows)]
    handle: *mut c_void,
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
    #[cfg(not(windows))]
    {
        let _ = identity;
        Ok(Guard {})
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
#[cfg(any(windows, test))]
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

    #[cfg(windows)]
    #[test]
    fn duplicate_is_rejected_until_guard_is_dropped() {
        let identity = format!("test/{}", uuid::Uuid::new_v4());
        let guard = super::acquire(Some(&identity)).unwrap();
        assert!(super::acquire(Some(&identity)).is_err());
        drop(guard);
        assert!(super::acquire(Some(&identity)).is_ok());
    }
}
