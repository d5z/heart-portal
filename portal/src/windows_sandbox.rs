//! Per-invocation AppContainer: no network capabilities, explicit workspace ACL,
//! restricted inherited handles, and a kill-on-close job for the entire tree.
//! Does not change integrity labels or grant access to Everyone/All App Packages.
use anyhow::{Context, Result};
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::{ffi::c_void, os::windows::{ffi::OsStrExt, io::{AsRawHandle, FromRawHandle, OwnedHandle}}, path::{Path, PathBuf}, ptr::{null, null_mut}};
use windows_sys::Win32::{Foundation::*, Security::{*, Authorization::*, Isolation::*},
    Storage::FileSystem::*, System::{Console::*, JobObjects::*, Threading::*}};

struct AclLock(OwnedHandle);
impl AclLock {
    fn acquire() -> Result<Self> {
        let handle = unsafe { CreateMutexW(null(), 0, wide("Local\\Town.Portal.SandboxACL").as_ptr()) };
        anyhow::ensure!(!handle.is_null(), "Cannot create sandbox ACL lock");
        let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
        let result = unsafe { WaitForSingleObject(handle.as_raw_handle(), 60000) };
        anyhow::ensure!(result == WAIT_OBJECT_0 || result == WAIT_ABANDONED, "Sandbox is busy; retry after the active command completes");
        Ok(Self(handle))
    }
}
impl Drop for AclLock { fn drop(&mut self) { unsafe { ReleaseMutex(self.0.as_raw_handle()); } } }

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request { pub root: PathBuf, pub cwd: PathBuf, pub source: String, pub powershell: bool,
    #[serde(default)] pub protected_paths: Vec<PathBuf> }
fn wide(value: impl AsRef<std::ffi::OsStr>) -> Vec<u16> { value.as_ref().encode_wide().chain(Some(0)).collect() }
fn check(ok: i32, label: &str) -> Result<()> {
    if ok == 0 { return Err(std::io::Error::last_os_error()).context(label.to_owned()); } Ok(())
}

struct Profile { name: Vec<u16>, sid: *mut c_void, acls: Vec<SavedAcl>, modified: bool }
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
    fn create() -> Result<Self> {
        let name = wide(format!("Town.Portal.{}", uuid::Uuid::new_v4().simple()));
        let mut sid = null_mut();
        let hr = unsafe { CreateAppContainerProfile(name.as_ptr(), name.as_ptr(), name.as_ptr(), null(), 0, &mut sid) };
        anyhow::ensure!(hr >= 0 && !sid.is_null(), "CreateAppContainerProfile failed: {hr:#x}");
        Ok(Self { name, sid, acls: vec![], modified: false })
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
        if self.modified {
            for acl in &self.acls {
                if let Err(error) = acl.restore(root) { eprintln!("Sandbox ACL cleanup failed: {error:#}"); }
            }
        }
        unsafe { FreeSid(self.sid); DeleteAppContainerProfile(self.name.as_ptr()); }
    }
}

struct SavedAcl { handle: Option<OwnedHandle>, id: i64, descriptor: *mut c_void, dacl: *mut ACL, protected: bool, reparse: bool }
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
        let mut saved = Self { handle: Some(handle), id: 0, descriptor, dacl, protected: false, reparse: false };
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        check(unsafe { GetFileInformationByHandle(saved.handle.as_ref().unwrap().as_raw_handle(), &mut info) }, "Read sandbox file identity")?;
        saved.id = (((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64) as i64;
        saved.reparse = info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0;
        anyhow::ensure!(!dacl.is_null(), "Refusing workspace with a NULL DACL");
        let mut control = 0; let mut revision = 0;
        check(unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) }, "Read ACL inheritance")?;
        saved.protected = control & SE_DACL_PROTECTED != 0;
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
    fn apply_kernel(&self, handle: HANDLE, protected: bool) -> Result<()> {
        let size = unsafe { GetSecurityDescriptorLength(self.descriptor) } as usize;
        let mut copy = vec![0usize; size.div_ceil(std::mem::size_of::<usize>())];
        unsafe { std::ptr::copy_nonoverlapping(self.descriptor.cast::<u8>(), copy.as_mut_ptr().cast::<u8>(), size); }
        let descriptor = copy.as_mut_ptr().cast();
        check(unsafe { SetSecurityDescriptorControl(descriptor, SE_DACL_PROTECTED,
            if protected { SE_DACL_PROTECTED } else { 0 }) }, "Set restored ACL inheritance")?;
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
    let root = request.root.canonicalize()?;
    let cwd = request.cwd.canonicalize()?;
    anyhow::ensure!(root.is_dir() && cwd.is_dir() && cwd.starts_with(&root) && root.parent().is_some(), "Invalid sandbox workspace");
    anyhow::ensure!(!request.source.contains('\0'), "Command contains NUL");
    let cwd_text = cwd.to_string_lossy();
    let cwd_native = cwd_text.strip_prefix(r"\\?\").unwrap_or(&cwd_text);
    anyhow::ensure!(!cwd_native.starts_with("UNC\\"), "Network workspaces are not supported by the offline sandbox");
    let _acl_lock = AclLock::acquire()?;
    let mut profile = Profile::create()?;
    // Resolve once and fail closed if an explicitly protected configuration is
    // missing. Silently dropping it would turn its old location into writable data.
    let protected_paths = request.protected_paths.iter().map(|p| p.canonicalize()
        .context("Resolve protected sandbox configuration")).collect::<Result<Vec<_>>>()?;
    let mut objects = vec![(root.clone(), true, 0x001201bfu32)]; // root cannot be moved
    let mut boundaries = protected_paths.clone();
    let mut directories = vec![(root.clone(), false)];
    while let Some((directory, protected)) = directories.pop() {
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?; let path = entry.path();
            use std::os::windows::fs::MetadataExt;
            let metadata = std::fs::symlink_metadata(&path)?;
            anyhow::ensure!(objects.len() < 20000, "Workspace exceeds sandbox ACL budget; select a smaller workspace");
            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            let hidden = matches!(name.as_str(), ".ssh" | ".portal-client.json" | "connection.json") || name.starts_with(".env");
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
            let access = if hidden || reparse { 0 } else if protected { 0x001200a9 } else { 0x001301bf };
            if protected || access == 0 { boundaries.push(path.clone()); }
            objects.push((path.clone(), metadata.is_dir(), access));
            if metadata.is_dir() && !hidden && !reparse { directories.push((path, protected)); }
        }
    }
    // Protect every existing child from inheritance BEFORE granting inheritable
    // access on ordinary directories. New objects can inherit the package SID,
    // while metadata, secrets and junctions never receive that grant.
    // Capture all descriptors before changing any inheritance settings.
    for (path, _, _) in &objects { profile.capture(path)?; }
    let root_handle = profile.acls[0].handle.as_ref().unwrap().as_raw_handle();
    for acl in &profile.acls { drop(acl.reopen(root_handle).context("Sandbox requires file-ID based ACL restoration")?); }
    profile.modified = true;
    for acl in &profile.acls { acl.apply(acl.dacl, PROTECTED_DACL_SECURITY_INFORMATION)?; }
    for ((path, directory, access), acl) in objects.iter().zip(&profile.acls) {
        let mut access = *access;
        // DELETE on an ancestor can move protected contents without opening them.
        if *directory && boundaries.iter().any(|p| p.starts_with(path)) { access &= !DELETE; }
        if access != 0 { acl.grant(profile.sid, access, *directory)?; }
    }
    // Open descendant handles can block directory renames on Windows even with
    // FILE_SHARE_DELETE. Retain only the immovable root and restore by file ID.
    for acl in profile.acls.iter_mut().skip(1) { acl.handle = None; }
    let system = PathBuf::from(std::env::var_os("SystemRoot").context("SystemRoot unavailable")?);
    let exe = if request.powershell { system.join(r"System32\WindowsPowerShell\v1.0\powershell.exe") } else { system.join(r"System32\cmd.exe") };
    let source = if request.powershell {
        let script = format!("try {{ New-PSDrive -Name Workspace -PSProvider FileSystem -Root '{}' -ErrorAction Stop | Out-Null; Set-Location Workspace:\\ -ErrorAction Stop }} catch {{ Write-Error $_; exit 126 }};\n{}", cwd_native.replace('\'', "''"), request.source);
        format!("\"{}\" -NoLogo -NoProfile -NonInteractive -EncodedCommand {}", exe.display(),
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
    for key in ["USERPROFILE", "LOCALAPPDATA", "APPDATA", "ALLUSERSPROFILE", "ProgramData", "TEMP", "TMP"] {
        if let Ok(value) = std::env::var(key) { environment.insert(key.to_uppercase(), value); }
    }
    environment.insert("PATH".into(), format!("{};{}", system.join("System32").display(), system.display()));
    environment.insert("SYSTEMROOT".into(), system.display().to_string());
    environment.insert("WINDIR".into(), system.display().to_string());
    environment.insert("COMSPEC".into(), system.join(r"System32\cmd.exe").display().to_string());
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
    anyhow::ensure!(unsafe { WaitForSingleObject(process.as_raw_handle(), INFINITE) } == WAIT_OBJECT_0, "Wait for sandbox failed");
    let mut code = 126;
    check(unsafe { GetExitCodeProcess(process.as_raw_handle(), &mut code) }, "Read sandbox exit code")?;
    unsafe { TerminateJobObject(job.as_raw_handle(), code); }
    drop(job); // Stop descendants before ACL revocation.
    Ok(code)
}
