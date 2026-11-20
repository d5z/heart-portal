//! Per-invocation AppContainer: no network capabilities, explicit workspace ACL,
//! restricted inherited handles, and a kill-on-close job for the entire tree.
//! Does not change integrity labels or grant access to Everyone/All App Packages.
use anyhow::{Context, Result};
use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{ffi::c_void, os::windows::{ffi::OsStrExt, io::{AsRawHandle, FromRawHandle, OwnedHandle}}, path::{Path, PathBuf}, ptr::{null, null_mut}};
use windows_sys::Win32::{Foundation::*, Security::{*, Authorization::*, Isolation::*},
    Storage::FileSystem::*, System::{Console::*, JobObjects::*, SystemServices::{ACCESS_ALLOWED_ACE_TYPE, ACCESS_DENIED_ACE_TYPE}, Threading::*}};

struct AclLock(OwnedHandle);
impl AclLock {
    fn acquire(cancel: HANDLE, root: &Path) -> Result<Self> {
        // Preparation is serialized per canonical workspace. A global mutex
        // would make unrelated Being workspaces wait behind one another even
        // though their standing grants and AppContainer SIDs are independent.
        let identity = root.to_string_lossy().to_ascii_lowercase();
        let digest = format!("{:x}", Sha256::digest(identity.as_bytes()));
        let name = wide(format!("Local\\Town.Portal.SandboxACL.{}", &digest[..32]));
        let handle = unsafe { CreateMutexW(null(), 0, name.as_ptr()) };
        anyhow::ensure!(!handle.is_null(), "Cannot create sandbox ACL lock");
        let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
        let handles = [cancel, handle.as_raw_handle()];
        let result = unsafe { WaitForMultipleObjects(handles.len() as u32, handles.as_ptr(), 0, 60000) };
        anyhow::ensure!(result != WAIT_OBJECT_0, "Sandbox cancelled before execution");
        anyhow::ensure!(result == WAIT_OBJECT_0 + 1 || result == WAIT_ABANDONED + 1, "Sandbox is busy; retry after the active command completes");
        Ok(Self(handle))
    }
}
impl Drop for AclLock { fn drop(&mut self) { unsafe { ReleaseMutex(self.0.as_raw_handle()); } } }

/// Cancel through a host-only event, never through the command's stdin.
/// The wrapper stays alive to revoke ACLs after stopping its inner job.
pub async fn cancel(pid: u32) -> bool {
    let process = unsafe { OpenProcess(SYNCHRONIZE, 0, pid) };
    if process.is_null() { return false; }
    let process = unsafe { OwnedHandle::from_raw_handle(process) };
    let name = wide(format!("Local\\Town.Portal.SandboxCancel.{pid}"));
    // A zero-second timeout may beat the resumed wrapper to event creation.
    let ready = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
    loop {
        if unsafe { WaitForSingleObject(process.as_raw_handle(), 0) } == WAIT_OBJECT_0 { return true; }
        let event = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr()) };
        if !event.is_null() {
            let event = unsafe { OwnedHandle::from_raw_handle(event) };
            if unsafe { SetEvent(event.as_raw_handle()) } == 0 { return false; }
            break;
        }
        if tokio::time::Instant::now() >= ready { return false; }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let cleanup = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if unsafe { WaitForSingleObject(process.as_raw_handle(), 0) } == WAIT_OBJECT_0 { return true; }
        if tokio::time::Instant::now() >= cleanup { return false; }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request { pub root: PathBuf, pub cwd: PathBuf, pub source: String, pub powershell: bool,
    #[serde(default)] pub protected_paths: Vec<PathBuf> }
fn wide(value: impl AsRef<std::ffi::OsStr>) -> Vec<u16> { value.as_ref().encode_wide().chain(Some(0)).collect() }
fn check(ok: i32, label: &str) -> Result<()> {
    if ok == 0 { return Err(std::io::Error::last_os_error()).context(label.to_owned()); } Ok(())
}

struct Profile { sid: *mut c_void, acls: Vec<SavedAcl>, modified: bool, standing: bool }
struct Capability(*mut c_void);
impl Capability {
    fn named(name: &str) -> Result<Self> {
        let mut groups = null_mut(); let mut group_count = 0;
        let mut capabilities = null_mut(); let mut count = 0;
        check(unsafe { DeriveCapabilitySidsFromName(wide(name).as_ptr(), &mut groups, &mut group_count, &mut capabilities, &mut count) }, "Derive runtime capability")?;
        unsafe {
            for index in 0..group_count { LocalFree(*groups.add(index as usize)); }
            LocalFree(groups.cast());
        }
        let result = if count == 1 { Some(unsafe { *capabilities }) } else { None };
        if result.is_none() { unsafe { for index in 0..count { LocalFree(*capabilities.add(index as usize)); } } }
        unsafe { LocalFree(capabilities.cast()); }
        Ok(Self(result.context("Unexpected registry capability result")?))
    }
}
impl Drop for Capability { fn drop(&mut self) { unsafe { LocalFree(self.0); } } }
impl Profile {
    fn create(root: &Path) -> Result<Self> {
        // A stable profile name gives one capability SID per canonical
        // workspace. The SID is retained by the standing ACL cache, while the
        // process token is still created afresh for every command.
        let identity = root.to_string_lossy().to_ascii_lowercase();
        let digest = format!("{:x}", Sha256::digest(identity.as_bytes()));
        let name = wide(format!("Town.Portal.Workspace.{}", &digest[..32]));
        let mut sid = null_mut();
        let hr = unsafe { CreateAppContainerProfile(name.as_ptr(), name.as_ptr(), name.as_ptr(), null(), 0, &mut sid) };
        if hr < 0 {
            // CreateAppContainerProfile reports an existing stable profile as
            // an error. Resolve its SID instead of creating a new profile or
            // retrying the command with a host token.
            let derive = unsafe { DeriveAppContainerSidFromAppContainerName(name.as_ptr(), &mut sid) };
            anyhow::ensure!(derive >= 0 && !sid.is_null(), "CreateAppContainerProfile failed: {hr:#x}");
        }
        anyhow::ensure!(!sid.is_null(), "CreateAppContainerProfile returned no SID");
        Ok(Self { sid, acls: vec![], modified: false, standing: false })
    }
    fn capture(&mut self, path: &Path) -> Result<()> {
        self.acls.push(SavedAcl::open(path)?);
        Ok(())
    }
}
impl Drop for Profile {
    fn drop(&mut self) {
        // Parents first: remove inheritable grants from new descendants before
        // restoring existing children's original inheritance settings. File IDs
        // keep cleanup valid when an ordinary file/directory has been renamed.
        let root = self.acls.first().and_then(|acl| acl.handle.as_ref()).map(|h| h.as_raw_handle());
        if self.modified && !self.standing {
            for acl in &self.acls {
                if let Err(error) = acl.restore(root) { eprintln!("Sandbox ACL cleanup failed: {error:#}"); }
            }
        }
        // The profile is deliberately retained with its workspace capability
        // SID. Deleting it would make the standing ACL grant unusable and
        // force a full tree rewrite on the next command.
        unsafe { FreeSid(self.sid); }
    }
}

struct SavedAcl { handle: Option<OwnedHandle>, id: i64, descriptor: *mut c_void, dacl: *mut ACL, protected: bool, auto_inherited: bool, reparse: bool }
impl SavedAcl {
    fn open(path: &Path) -> Result<Self> {
        // Never follow reparse points while changing security descriptors.
        let handle = unsafe { CreateFileW(wide(path).as_ptr(), READ_CONTROL | WRITE_DAC,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, null(), OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT, null_mut()) };
        anyhow::ensure!(handle != INVALID_HANDLE_VALUE, "Cannot open sandbox ACL: {}", std::io::Error::last_os_error());
        let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
        let mut descriptor = null_mut(); let mut dacl = null_mut();
        let code = unsafe { GetSecurityInfo(handle.as_raw_handle(), SE_FILE_OBJECT, DACL_SECURITY_INFORMATION,
            null_mut(), null_mut(), &mut dacl, null_mut(), &mut descriptor) };
        anyhow::ensure!(code == 0, "Read sandbox ACL failed: {code}");
        let mut saved = Self { handle: Some(handle), id: 0, descriptor, dacl, protected: false, auto_inherited: false, reparse: false };
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        check(unsafe { GetFileInformationByHandle(saved.handle.as_ref().unwrap().as_raw_handle(), &mut info) }, "Read sandbox file identity")?;
        saved.id = (((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64) as i64;
        saved.reparse = info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0;
        anyhow::ensure!(!dacl.is_null(), "Refusing workspace with a NULL DACL");
        let mut control = 0; let mut revision = 0;
        check(unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) }, "Read ACL inheritance")?;
        saved.protected = control & SE_DACL_PROTECTED != 0;
        saved.auto_inherited = control & SE_DACL_AUTO_INHERITED != 0;
        Ok(saved)
    }
    fn apply(&self, dacl: *mut ACL, flags: u32) -> Result<()> {
        if self.reparse {
            return self.apply_kernel(self.handle.as_ref().unwrap().as_raw_handle(), true);
        }
        Self::apply_handle(self.handle.as_ref().unwrap().as_raw_handle(), dacl, flags)
    }
    fn apply_handle(handle: HANDLE, dacl: *mut ACL, flags: u32) -> Result<()> {
        // Pass a scratch ACL to the propagation API, keeping the captured
        // descriptor immutable for exact restoration.
        let size = unsafe { (*dacl).AclSize as usize };
        let mut copy = vec![0usize; size.div_ceil(std::mem::size_of::<usize>())];
        unsafe { std::ptr::copy_nonoverlapping(dacl.cast::<u8>(), copy.as_mut_ptr().cast::<u8>(), size); }
        let code = unsafe { SetSecurityInfo(handle, SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | flags, null_mut(), null_mut(), copy.as_mut_ptr().cast(), null_mut()) };
        anyhow::ensure!(code == 0, "Apply sandbox ACL failed: {code}");
        Ok(())
    }
    fn grant(&self, sid: *mut c_void, access: u32, inherit: bool) -> Result<()> {
        // Stable workspace SIDs make preparation idempotent after a deleted
        // marker or an interrupted first run; do not accumulate duplicate ACEs.
        if acl_has_grant(self.dacl, sid, access) { return Ok(()); }
        let entry = EXPLICIT_ACCESS_W { grfAccessPermissions: access, grfAccessMode: GRANT_ACCESS,
            grfInheritance: 0,
            Trustee: TRUSTEE_W { pMultipleTrustee: null_mut(), MultipleTrusteeOperation: 0,
                TrusteeForm: TRUSTEE_IS_SID, TrusteeType: TRUSTEE_IS_UNKNOWN, ptstrName: sid.cast() } };
        let mut entries = vec![entry];
        if inherit {
            // An immovable parent must still let newly created children rename
            // themselves. Existing protected children have inheritance blocked.
            entries.push(EXPLICIT_ACCESS_W {
                grfAccessPermissions: if access & FILE_WRITE_DATA != 0 { 0x001301bf } else { access },
                grfInheritance: SUB_CONTAINERS_AND_OBJECTS_INHERIT | INHERIT_ONLY,
                ..entry
            });
        }
        let mut new = null_mut();
        let code = unsafe { SetEntriesInAclW(entries.len() as u32, entries.as_ptr(), self.dacl, &mut new) };
        anyhow::ensure!(code == 0, "Build sandbox ACL failed: {code}");
        let result = self.apply(new, PROTECTED_DACL_SECURITY_INFORMATION);
        unsafe { LocalFree(new.cast()); }
        result
    }
    fn deny(&self, sid: *mut c_void) -> Result<()> {
        // A stable workspace SID may already have arrived through an
        // inheritable grant before this path was created. Build a filtered ACL
        // so inherited allow entries for that SID cannot survive alongside the
        // boundary deny.
        let source_size = unsafe { (*self.dacl).AclSize as usize };
        let mut storage = vec![0u8; source_size + 256];
        let acl = storage.as_mut_ptr().cast::<ACL>();
        check(unsafe { InitializeAcl(acl, storage.len() as u32, ACL_REVISION) }, "Initialize boundary ACL")?;
        for index in 0..unsafe { (*self.dacl).AceCount } {
            let mut ace: *mut c_void = null_mut();
            check(unsafe { GetAce(self.dacl, index as u32, &mut ace) }, "Read boundary ACL entry")?;
            let header = unsafe { &*(ace.cast::<ACE_HEADER>()) };
            let same_sid = match header.AceType as u32 {
                ACCESS_ALLOWED_ACE_TYPE => unsafe { EqualSid((&(*(ace.cast::<ACCESS_ALLOWED_ACE>())).SidStart as *const u32).cast_mut().cast(), sid) != 0 },
                ACCESS_DENIED_ACE_TYPE => unsafe { EqualSid((&(*(ace.cast::<ACCESS_DENIED_ACE>())).SidStart as *const u32).cast_mut().cast(), sid) != 0 },
                _ => false,
            };
            if !same_sid { check(unsafe { AddAce(acl, ACL_REVISION, u32::MAX, ace, header.AceSize as u32) }, "Copy boundary ACL entry")?; }
        }
        let deny = EXPLICIT_ACCESS_W {
            grfAccessPermissions: FILE_ALL_ACCESS,
            grfAccessMode: DENY_ACCESS,
            grfInheritance: OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE,
            Trustee: TRUSTEE_W { pMultipleTrustee: null_mut(), MultipleTrusteeOperation: 0,
                TrusteeForm: TRUSTEE_IS_SID, TrusteeType: TRUSTEE_IS_UNKNOWN, ptstrName: sid.cast() },
        };
        let mut denied = null_mut();
        let code = unsafe { SetEntriesInAclW(1, &deny, acl, &mut denied) };
        anyhow::ensure!(code == 0, "Add sandbox boundary deny failed: {code}");
        let result = self.apply(denied, PROTECTED_DACL_SECURITY_INFORMATION);
        unsafe { LocalFree(denied.cast()); }
        result
    }
    fn apply_kernel(&self, handle: HANDLE, protected: bool) -> Result<()> {
        let size = unsafe { GetSecurityDescriptorLength(self.descriptor) } as usize;
        let mut copy = vec![0usize; size.div_ceil(std::mem::size_of::<usize>())];
        unsafe { std::ptr::copy_nonoverlapping(self.descriptor.cast::<u8>(), copy.as_mut_ptr().cast::<u8>(), size); }
        let descriptor = copy.as_mut_ptr().cast();
        // The low-level setter consumes AUTO_INHERIT_REQ to preserve the stored
        // AUTO_INHERITED bit; copying AUTO_INHERITED alone clears it on NTFS.
        let inheritance = if self.auto_inherited { SE_DACL_AUTO_INHERIT_REQ | SE_DACL_AUTO_INHERITED } else { 0 };
        check(unsafe { SetSecurityDescriptorControl(descriptor, SE_DACL_PROTECTED | SE_DACL_AUTO_INHERIT_REQ | SE_DACL_AUTO_INHERITED,
            inheritance | if protected { SE_DACL_PROTECTED } else { 0 }) }, "Set restored ACL inheritance")?;
        check(unsafe { SetKernelObjectSecurity(handle, DACL_SECURITY_INFORMATION, descriptor) }, "Restore original sandbox ACL")
    }
    fn restore(&self, root: Option<HANDLE>) -> Result<()> {
        let flags = if self.protected { PROTECTED_DACL_SECURITY_INFORMATION } else { UNPROTECTED_DACL_SECURITY_INFORMATION };
        let restore = |handle| -> Result<()> {
            // First propagate removal to new descendants. Then restore the
            // captured descriptor verbatim without rebuilding inherited ACEs
            // from a possibly renamed parent.
            if !self.reparse { Self::apply_handle(handle, self.dacl, flags)?; }
            self.apply_kernel(handle, self.protected)
        };
        if let Some(handle) = &self.handle { return restore(handle.as_raw_handle()); }
        let handle = match self.reopen(root.context("Missing sandbox root handle")?) {
            Ok(handle) => handle,
            Err(error) => {
                // NTFS returns INVALID_PARAMETER for a deleted file ID. Reopen
                // support was verified for every ID before changing any ACL.
                if matches!(error.raw_os_error(), Some(code) if code == ERROR_FILE_NOT_FOUND as i32 || code == ERROR_INVALID_PARAMETER as i32) { return Ok(()); }
                return Err(error).context("Reopen sandbox object for ACL restoration");
            }
        };
        restore(handle.as_raw_handle())
    }
    fn reopen(&self, root: HANDLE) -> std::io::Result<OwnedHandle> {
        let id = FILE_ID_DESCRIPTOR { dwSize: std::mem::size_of::<FILE_ID_DESCRIPTOR>() as u32,
            Type: FileIdType, Anonymous: FILE_ID_DESCRIPTOR_0 { FileId: self.id } };
        let handle = unsafe { OpenFileById(root, &id, READ_CONTROL | WRITE_DAC,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, null(), FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT) };
        if handle == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error());
        }
        Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
    }
}
impl Drop for SavedAcl { fn drop(&mut self) { unsafe { LocalFree(self.descriptor); } } }

const WORKSPACE_GRANT: u32 = 0x001201bf;
const RUNTIME_GRANT: u32 = 0x001200a9;

fn acl_has_grant(dacl: *mut ACL, sid: *mut c_void, required: u32) -> bool {
    if dacl.is_null() { return false; }
    let count = unsafe { (*dacl).AceCount };
    for index in 0..count {
        let mut ace: *mut c_void = null_mut();
        if unsafe { GetAce(dacl, index as u32, &mut ace) } == 0 || ace.is_null() { continue; }
        let header = unsafe { &*(ace.cast::<ACE_HEADER>()) };
        if header.AceType as u32 != ACCESS_ALLOWED_ACE_TYPE { continue; }
        let allowed = unsafe { &*(ace.cast::<ACCESS_ALLOWED_ACE>()) };
        let ace_sid = (&allowed.SidStart as *const u32).cast_mut().cast();
        if unsafe { EqualSid(ace_sid, sid) } != 0 && allowed.Mask & required == required { return true; }
    }
    false
}

fn cache_marker(root: &Path) -> PathBuf { root.join(".portal-runtime").join(".portal-acl-cache") }

fn cache_matches(root: &Path, runtime: Option<&Path>, sid: *mut c_void) -> Result<bool> {
    let marker = cache_marker(root);
    let expected = runtime.map(|path| path.to_string_lossy().into_owned()).unwrap_or_default();
    let actual = match std::fs::read_to_string(&marker) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).context("Read sandbox ACL cache marker"),
    };
    if actual.trim() != expected { return Ok(false); }
    let workspace = SavedAcl::open(root)?;
    if !acl_has_grant(workspace.dacl, sid, WORKSPACE_GRANT) { return Ok(false); }
    if let Some(runtime) = runtime {
        let runtime_acl = SavedAcl::open(runtime)?;
        if !acl_has_grant(runtime_acl.dacl, sid, RUNTIME_GRANT) { return Ok(false); }
    }
    Ok(true)
}

fn boundary_paths(root: &Path, protected_paths: &[PathBuf], sid: *mut c_void) -> Result<Vec<PathBuf>> {
    let mut result = Vec::new();
    // Standing grants make ordinary invocations cheap. Look for newly
    // created sensitive paths, but prune generated trees that can contain
    // hundreds of thousands of files. A path already restricted by the full
    // preparation pass has no package grant and therefore does not invalidate
    // the standing cache again.
    let mut directories = vec![root.to_owned()];
    while let Some(directory) = directories.pop() {
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            let sensitive = crate::sensitive_paths::is_secret_name(&name) || protected_paths.contains(&path);
            let metadata = std::fs::symlink_metadata(&path)?;
            if sensitive {
                let acl = SavedAcl::open(&path)?;
                if acl_has_grant(acl.dacl, sid, WORKSPACE_GRANT) { result.push(path.clone()); }
            }
            if metadata.is_dir() && !matches!(name.as_str(), ".portal-runtime" | "node_modules" | ".venv" | "__pycache__") {
                directories.push(path);
            }
        }
    }
    Ok(result)
}

struct Attributes { storage: Vec<usize>, pointer: LPPROC_THREAD_ATTRIBUTE_LIST }
impl Attributes {
    fn new() -> Result<Self> {
        let mut size = 0;
        unsafe { InitializeProcThreadAttributeList(null_mut(), 3, 0, &mut size); }
        anyhow::ensure!(size > 0 && size < 65536, "Invalid process attribute allocation");
        let mut storage = vec![0usize; size.div_ceil(std::mem::size_of::<usize>())];
        let pointer = storage.as_mut_ptr().cast();
        check(unsafe { InitializeProcThreadAttributeList(pointer, 3, 0, &mut size) }, "Initialize process attributes")?;
        Ok(Self { storage, pointer })
    }
    fn set<T>(&mut self, kind: u32, value: &mut T) -> Result<()> {
        check(unsafe { UpdateProcThreadAttribute(self.pointer, 0, kind as usize,
            (value as *mut T).cast(), std::mem::size_of::<T>(), null_mut(), null()) }, "Set sandbox process attribute")
    }
}
impl Drop for Attributes { fn drop(&mut self) { unsafe { DeleteProcThreadAttributeList(self.pointer); } let _ = &self.storage; } }

pub fn run_payload(encoded: &str) -> Result<u32> {
    anyhow::ensure!(encoded.len() < 28000, "Invalid sandbox payload size");
    let request: Request = serde_json::from_slice(&base64::engine::general_purpose::STANDARD.decode(encoded)?)?;
    run(&request)
}

pub fn run(request: &Request) -> Result<u32> {
    // Default host DACL, non-inherited handle, no AppContainer grant.
    let event = unsafe { CreateEventW(null(), 1, 0, wide(format!("Local\\Town.Portal.SandboxCancel.{}", std::process::id())).as_ptr()) };
    anyhow::ensure!(!event.is_null(), "Cannot create sandbox cancellation event");
    let cancel = unsafe { OwnedHandle::from_raw_handle(event) };
    let root = request.root.canonicalize()?;
    let cwd = request.cwd.canonicalize()?;
    anyhow::ensure!(root.is_dir() && cwd.is_dir() && cwd.starts_with(&root) && root.parent().is_some(), "Invalid sandbox workspace");
    anyhow::ensure!(!request.source.contains('\0'), "Command contains NUL");
    let cwd_text = cwd.to_string_lossy();
    let cwd_native = cwd_text.strip_prefix(r"\\?\").unwrap_or(&cwd_text);
    anyhow::ensure!(!cwd_native.starts_with("UNC\\"), "Network workspaces are not supported by the offline sandbox");
    let mut profile = Profile::create(&root)?;
    let mut config = crate::config::PortalConfig::default();
    config.security.workspace_root = root.clone();
    config.security.permission_mode = crate::config::PermissionMode::Supervised;
    config.config_path = request.protected_paths.first().cloned();
    let mut task_environment = crate::task_environment::prepare(&config)?;
    // A command may target a nested project. Put only that directory's local
    // shims in front of the bundled tools; do not scan ancestors or inherit a
    // host PATH into the AppContainer.
    if let Some(path) = task_environment.variables.get_mut("PATH") {
        let mut local = vec![cwd.join("node_modules/.bin"), cwd.join(".venv/Scripts")];
        local.retain(|entry| entry.is_dir());
        if !local.is_empty() {
            local.extend(std::env::split_paths(path));
            *path = std::env::join_paths(local)?.to_string_lossy().into_owned();
        }
    }
    // Resolve once and fail closed if an explicitly protected configuration is
    // missing. Silently dropping it would turn its old location into writable data.
    let protected_paths = request.protected_paths.iter().map(|p| p.canonicalize()
        .context("Resolve protected sandbox configuration")).collect::<Result<Vec<_>>>()?;
    let runtime_root = task_environment.runtime.as_ref().map(|runtime| runtime.root.as_path());
    let mut cache_ready = cache_matches(&root, runtime_root, profile.sid)?;
    // Name-based secret/config exclusions need a fresh boundary pass. Keep the
    // fast standing-grant path for ordinary projects; once a boundary object
    // exists, use the original full preparation so every descendant is checked.
    if cache_ready && !boundary_paths(&root, &protected_paths, profile.sid)?.is_empty() { cache_ready = false; }
    // Only the first preparation (or an invalidated cache) needs the global
    // ACL mutex. Commands using an already materialized grant do not wait for
    // another command to finish.
    let acl_lock = if cache_ready { None } else { Some(AclLock::acquire(cancel.as_raw_handle(), &root)?) };
    if acl_lock.is_some() {
        cache_ready = cache_matches(&root, runtime_root, profile.sid)?;
        // Another invocation may have materialized the standing grant while
        // this call waited. Recheck boundaries after the lock handoff too;
        // otherwise a newly-created secret path could bypass the deny pass.
        if cache_ready && !boundary_paths(&root, &protected_paths, profile.sid)?.is_empty() { cache_ready = false; }
    }
    let mut objects = vec![(root.clone(), true, WORKSPACE_GRANT)]; // root cannot be moved
    let mut boundaries = protected_paths.clone();
    let mut secret_boundaries = Vec::new();
    if !cache_ready {
      let mut directories = vec![(root.clone(), false)];
      while let Some((directory, protected)) = directories.pop() {
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?; let path = entry.path();
            use std::os::windows::fs::MetadataExt;
            let metadata = std::fs::symlink_metadata(&path)?;
            anyhow::ensure!(objects.len() < 20000, "Workspace exceeds sandbox ACL budget; select a smaller workspace");
            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            let secret_name = crate::sensitive_paths::is_secret_name(&name);
            let hidden = secret_name || protected_paths.contains(&path);
            let reparse = metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0;
            if metadata.is_file() && !reparse {
                let file = std::fs::File::open(&path)?;
                let mut info = BY_HANDLE_FILE_INFORMATION::default();
                check(unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) }, "Inspect sandbox file links")?;
                // A hard link shares its security descriptor with paths outside
                // the workspace. Do not change even its inheritance flags.
                anyhow::ensure!(info.nNumberOfLinks == 1, "Sandbox workspace contains a hardlinked file; remove the alias before running commands");
            }
            let protected = protected || protected_paths.contains(&path)
                || matches!(name.as_str(), ".git" | ".codex" | ".agents" | "portal.toml" | "desktop-portal.toml" | "mcp.toml");
            if secret_name { secret_boundaries.push(path.clone()); }
            let secret_tree = secret_boundaries.iter().any(|boundary| path.starts_with(boundary));
            let access = if hidden || reparse || secret_tree { 0 } else if protected { 0x001200a9 } else { 0x001301bf };
            if protected || access == 0 { boundaries.push(path.clone()); }
            objects.push((path.clone(), metadata.is_dir(), access));
            if metadata.is_dir() && !reparse { directories.push((path, protected || hidden)); }
        }
      }
      let workspace_objects = objects.len();
      if let Some(runtime) = &task_environment.runtime {
          anyhow::ensure!(!runtime.root.starts_with(&root), "Bundled runtime must be outside the writable workspace");
          // Grant this invocation read/execute only. Keep handles for this separate
          // tree so cleanup also works when it lives on another volume.
          for entry in walkdir::WalkDir::new(&runtime.root).follow_links(false) {
              let entry = entry.context("Inspect bundled runtime")?;
              let metadata = entry.metadata()?;
              anyhow::ensure!(!metadata.file_type().is_symlink(), "Bundled runtime must contain ordinary files");
              objects.push((entry.path().to_owned(), metadata.is_dir(), RUNTIME_GRANT));
          }
      }
      // Protect every existing child from inheritance BEFORE granting inheritable
      // access on ordinary directories. New objects can inherit the package SID,
      // while metadata, secrets and junctions never receive that grant.
      // Capture all descriptors before changing any inheritance settings.
      for (path, _, _) in &objects { profile.capture(path)?; }
      let root_handle = profile.acls[0].handle.as_ref().unwrap().as_raw_handle();
      for acl in profile.acls.iter().take(workspace_objects) { drop(acl.reopen(root_handle).context("Sandbox requires file-ID based ACL restoration")?); }
      if unsafe { WaitForSingleObject(cancel.as_raw_handle(), 0) } == WAIT_OBJECT_0 { return Ok(137); }
      profile.modified = true;
      for acl in &profile.acls { acl.apply(acl.dacl, PROTECTED_DACL_SECURITY_INFORMATION)?; }
      for ((path, directory, access), acl) in objects.iter().zip(&profile.acls) {
          let mut access = *access;
          // DELETE on an ancestor can move protected contents without opening them.
          if *directory && boundaries.iter().any(|p| p.starts_with(path)) { access &= !DELETE; }
          if access != 0 { acl.grant(profile.sid, access, *directory)?; }
          else { acl.deny(profile.sid)?; }
      }
      // Open descendant handles can block directory renames on Windows even with
      // FILE_SHARE_DELETE. Retain only the immovable root and restore by file ID.
      for acl in profile.acls.iter_mut().take(workspace_objects).skip(1) { acl.handle = None; }
      if unsafe { WaitForSingleObject(cancel.as_raw_handle(), 0) } == WAIT_OBJECT_0 { return Ok(137); }
      std::fs::write(cache_marker(&root), runtime_root.map(|path| path.to_string_lossy().into_owned()).unwrap_or_default())
          .context("Write sandbox ACL cache marker")?;
      profile.standing = true;
    }
    drop(acl_lock);
    let system = PathBuf::from(std::env::var_os("SystemRoot").context("SystemRoot unavailable")?);
    let exe = if request.powershell { system.join(r"System32\WindowsPowerShell\v1.0\powershell.exe") } else { system.join(r"System32\cmd.exe") };
    let source = if request.powershell {
        let script = format!("try {{ New-PSDrive -Name Workspace -PSProvider FileSystem -Root '{}' -ErrorAction Stop | Out-Null; Set-Location Workspace:\\ -ErrorAction Stop }} catch {{ [Console]::Error.WriteLine($_.ToString()); exit 126 }};\n{}", cwd_native.replace('\'', "''"), request.source);
        format!("\"{}\" -NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -EncodedCommand {}", exe.display(),
            crate::exec_policy::powershell_encoded_command(&script))
    } else { format!("\"{}\" /D /S /C \"{}\"", exe.display(), request.source) };
    let mut command = wide(source);
    let runtime_capabilities = [Capability::named("registryRead")?, Capability::named("lpacInstrumentation")?, Capability::named("lpacCom")?];
    let mut entries: Vec<_> = runtime_capabilities.iter().map(|capability| SID_AND_ATTRIBUTES { Sid: capability.0, Attributes: 0x4 /* SE_GROUP_ENABLED */ }).collect();
    let mut capabilities = SECURITY_CAPABILITIES { AppContainerSid: profile.sid, Capabilities: entries.as_mut_ptr(), CapabilityCount: entries.len() as u32, Reserved: 0 };
    let mut attributes = Attributes::new()?;
    attributes.set(PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, &mut capabilities)?;
    let mut lpac = 1u32; // PROCESS_CREATION_ALL_APPLICATION_PACKAGES_OPT_OUT
    attributes.set(PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY, &mut lpac)?;
    let current = unsafe { GetCurrentProcess() };
    let mut owned = vec![];
    for kind in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        let original = unsafe { GetStdHandle(kind) };
        let mut copied = null_mut();
        check(unsafe { DuplicateHandle(current, original, current, &mut copied, 0, 1, DUPLICATE_SAME_ACCESS) }, "Duplicate sandbox stdio")?;
        owned.push(unsafe { OwnedHandle::from_raw_handle(copied) });
    }
    let mut handles = [owned[0].as_raw_handle(), owned[1].as_raw_handle(), owned[2].as_raw_handle()];
    attributes.set(PROC_THREAD_ATTRIBUTE_HANDLE_LIST, &mut handles)?;
    let mut startup = STARTUPINFOEXW::default();
    startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = handles[0]; startup.StartupInfo.hStdOutput = handles[1]; startup.StartupInfo.hStdError = handles[2];
    startup.lpAttributeList = attributes.pointer;
    // No credentials, client capability paths, proxy configuration or host PATH.
    let mut environment = std::collections::BTreeMap::new();
    for (key, value) in task_environment.variables { environment.insert(key.to_uppercase(), value); }
    environment.insert("SYSTEMROOT".into(), system.display().to_string());
    environment.insert("WINDIR".into(), system.display().to_string());
    environment.insert("COMSPEC".into(), system.join(r"System32\cmd.exe").display().to_string());
    // The isolated environment has no host module search path. Resolve the
    // built-in Windows PowerShell cmdlets from the shell we actually launch.
    environment.insert("PSMODULEPATH".into(), system.join(r"System32\WindowsPowerShell\v1.0\Modules").display().to_string());
    let mut env = vec![];
    for (key, value) in environment {
        env.extend(wide(format!("{key}={value}")));
    }
    env.push(0);
    let job_handle = unsafe { CreateJobObjectW(null(), null()) };
    anyhow::ensure!(!job_handle.is_null(), "Cannot create sandbox job");
    let job = unsafe { OwnedHandle::from_raw_handle(job_handle) };
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
    limits.BasicLimitInformation.ActiveProcessLimit = 32;
    check(unsafe { SetInformationJobObject(job.as_raw_handle(), JobObjectExtendedLimitInformation,
        (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(), std::mem::size_of_val(&limits) as u32) }, "Configure sandbox job")?;
    let mut info = PROCESS_INFORMATION::default();
    check(unsafe { CreateProcessW(wide(&exe).as_ptr(), command.as_mut_ptr(), null(), null(), 1,
        CREATE_NO_WINDOW | CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT,
        env.as_ptr().cast(), wide(cwd_native).as_ptr(), &startup.StartupInfo, &mut info) }, "Create AppContainer process")?;
    let process = unsafe { OwnedHandle::from_raw_handle(info.hProcess) };
    let thread = unsafe { OwnedHandle::from_raw_handle(info.hThread) };
    let initialized = (|| -> Result<()> {
        check(unsafe { AssignProcessToJobObject(job.as_raw_handle(), process.as_raw_handle()) }, "Assign sandbox job")?;
        let mut token = null_mut();
        check(unsafe { OpenProcessToken(process.as_raw_handle(), TOKEN_QUERY, &mut token) }, "Inspect sandbox token")?;
        let token = unsafe { OwnedHandle::from_raw_handle(token) };
        let mut is_container = 0u32; let mut size = 0;
        check(unsafe { GetTokenInformation(token.as_raw_handle(), TokenIsAppContainer,
            (&mut is_container as *mut u32).cast(), 4, &mut size) }, "Verify AppContainer token")?;
        anyhow::ensure!(is_container == 1, "Process is not an AppContainer");
        anyhow::ensure!(unsafe { ResumeThread(thread.as_raw_handle()) } == 1, "Cannot resume sandbox process");
        Ok(())
    })();
    if let Err(error) = initialized { unsafe { TerminateProcess(process.as_raw_handle(), 126); } return Err(error); }
    let handles = [cancel.as_raw_handle(), process.as_raw_handle()];
    let completed = unsafe { WaitForMultipleObjects(handles.len() as u32, handles.as_ptr(), 0, INFINITE) };
    if completed == WAIT_OBJECT_0 {
        check(unsafe { TerminateJobObject(job.as_raw_handle(), 137) }, "Cancel sandbox job")?;
        anyhow::ensure!(unsafe { WaitForSingleObject(process.as_raw_handle(), INFINITE) } == WAIT_OBJECT_0, "Wait for cancelled sandbox failed");
        // A cancelled first preparation must not leave standing grants behind.
        // Let Profile restore the captured descriptors; the next invocation
        // will notice the missing root grant and prepare again.
        profile.standing = false;
    } else { anyhow::ensure!(completed == WAIT_OBJECT_0 + 1, "Wait for sandbox failed"); }
    let mut code = 126;
    check(unsafe { GetExitCodeProcess(process.as_raw_handle(), &mut code) }, "Read sandbox exit code")?;
    unsafe { TerminateJobObject(job.as_raw_handle(), code); }
    drop(job); // Stop descendants before ACL revocation.
    Ok(code)
}
