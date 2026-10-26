//! Per-invocation AppContainer: no network capabilities, explicit workspace ACL,
//! restricted inherited handles, and a kill-on-close job for the entire tree.
//! Does not change integrity labels or grant access to Everyone/All App Packages.
use anyhow::{Context, Result};
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::{ffi::c_void, os::windows::{ffi::OsStrExt, io::{AsRawHandle, FromRawHandle, OwnedHandle}}, path::{Path, PathBuf}, ptr::{null, null_mut}};
use windows_sys::Win32::{Foundation::*, Security::{*, Authorization::*, Isolation::*},
    System::{Console::*, JobObjects::*, Threading::*}};

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

struct Profile { name: Vec<u16>, sid: *mut c_void, paths: Vec<PathBuf> }
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
        Ok(Self { name, sid, paths: vec![] })
    }
    fn acl(&mut self, path: &Path, access: u32, inherit: bool) -> Result<()> {
        // Record before mutation so partial initialization also revokes the grant.
        self.paths.push(path.to_owned());
        acl_entry(path, self.sid, access, GRANT_ACCESS, inherit)
    }
}
impl Drop for Profile {
    fn drop(&mut self) {
        for path in self.paths.iter().rev() {
            if let Err(error) = acl_entry(path, self.sid, 0, REVOKE_ACCESS, false) { eprintln!("Sandbox ACL cleanup failed: {error}"); }
        }
        unsafe { FreeSid(self.sid); DeleteAppContainerProfile(self.name.as_ptr()); }
    }
}

fn acl_entry(path: &Path, sid: *mut c_void, access: u32, mode: ACCESS_MODE, inherit: bool) -> Result<()> {
    let path_w = wide(path);
    let mut descriptor = null_mut();
    let mut old = null_mut();
    let code = unsafe { GetNamedSecurityInfoW(path_w.as_ptr(), SE_FILE_OBJECT, DACL_SECURITY_INFORMATION,
        null_mut(), null_mut(), &mut old, null_mut(), &mut descriptor) };
    anyhow::ensure!(code == 0, "Reading sandbox directory ACL failed: {code}");
    struct Local(*mut c_void);
    impl Drop for Local { fn drop(&mut self) { unsafe { LocalFree(self.0); } } }
    let _descriptor = Local(descriptor);
    anyhow::ensure!(!old.is_null(), "Refusing workspace with a NULL DACL");
    let entry = EXPLICIT_ACCESS_W { grfAccessPermissions: access, grfAccessMode: mode,
        grfInheritance: if inherit { SUB_CONTAINERS_AND_OBJECTS_INHERIT } else { 0 },
        Trustee: TRUSTEE_W { pMultipleTrustee: null_mut(), MultipleTrusteeOperation: 0,
            TrusteeForm: TRUSTEE_IS_SID, TrusteeType: TRUSTEE_IS_UNKNOWN, ptstrName: sid.cast() } };
    let mut new = null_mut();
    let code = unsafe { SetEntriesInAclW(1, &entry, old, &mut new) };
    anyhow::ensure!(code == 0, "Building sandbox ACL failed: {code}");
    let _new = Local(new.cast());
    let code = unsafe { SetNamedSecurityInfoW(path_w.as_ptr(), SE_FILE_OBJECT, DACL_SECURITY_INFORMATION,
        null_mut(), null_mut(), new, null()) };
    anyhow::ensure!(code == 0, "Applying sandbox ACL failed: {code}");
    Ok(())
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
    let root = request.root.canonicalize()?;
    let cwd = request.cwd.canonicalize()?;
    anyhow::ensure!(root.is_dir() && cwd.is_dir() && cwd.starts_with(&root) && root.parent().is_some(), "Invalid sandbox workspace");
    anyhow::ensure!(!request.source.contains('\0'), "Command contains NUL");
    let cwd_text = cwd.to_string_lossy();
    let cwd_native = cwd_text.strip_prefix(r"\\?\").unwrap_or(&cwd_text);
    anyhow::ensure!(!cwd_native.starts_with("UNC\\"), "Network workspaces are not supported by the offline sandbox");
    let _acl_lock = AclLock::acquire()?;
    let mut profile = Profile::create()?;
    // No WRITE_DAC, WRITE_OWNER, or FILE_DELETE_CHILD. The AppContainer cannot
    // extend its own authority. Access-checked traversal follows the target ACL.
    // Package SID DENY ACEs are NOT a boundary in AppContainer access checks.
    // Use positive, non-inheriting grants instead, never an inherited write grant
    // above protected metadata. Newly created objects get the lowbox default DACL.
    let mut directories = vec![(root.clone(), false)];
    profile.acl(&root, 0x001301bf, false)?;
    let mut count = 0;
    while let Some((directory, protected)) = directories.pop() {
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?; let path = entry.path();
            use std::os::windows::fs::MetadataExt;
            let metadata = std::fs::symlink_metadata(&path)?;
            // Never modify a junction/symlink target's ACL, even inside the workspace.
            if metadata.file_attributes() & 0x400 != 0 { continue; }
            if metadata.is_file() {
                // An ACL belongs to the file object, so granting a hard link
                // could authorize an unrelated path outside the workspace.
                use windows_sys::Win32::Storage::FileSystem::{GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION};
                let file = std::fs::File::open(&path)?;
                let mut info = BY_HANDLE_FILE_INFORMATION::default();
                check(unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) }, "Inspect sandbox file links")?;
                if info.nNumberOfLinks > 1 { continue; }
            }
            count += 1;
            anyhow::ensure!(count <= 20000, "Workspace exceeds sandbox ACL budget; select a smaller workspace");
            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            if matches!(name.as_str(), ".ssh" | ".portal-client.json" | "connection.json") || name.starts_with(".env") { continue; }
            let protected = protected || request.protected_paths.iter().any(|p| p.canonicalize().ok().as_ref() == Some(&path))
                || matches!(name.as_str(), ".git" | ".codex" | ".agents" | "portal.toml" | "desktop-portal.toml" | "mcp.toml");
            profile.acl(&path, if protected { 0x001200a9 } else { 0x001301bf }, false)?;
            if metadata.is_dir() { directories.push((path, protected)); }
        }
    }
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
