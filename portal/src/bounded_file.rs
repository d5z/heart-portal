//! Bounded regular-file reads. Kit-controlled references cannot escape their root.
use std::{
    fs::{File, OpenOptions},
    io::{self, Read},
    path::{Component, Path},
};

pub(super) const CONFIG_LIMIT: usize = 1024 * 1024;

pub(super) fn metadata_bytes(path: impl AsRef<Path>) -> io::Result<Vec<u8>> {
    read(path.as_ref(), CONFIG_LIMIT)
}

pub(super) fn metadata_text(path: impl AsRef<Path>) -> io::Result<String> {
    text(path.as_ref(), CONFIG_LIMIT)
}

fn invalid() -> io::Error {
    io::Error::other("Expected a regular file within the allowed directory and size limit")
}

fn open(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    options.open(path)
}

fn read_handle(file: File, limit: usize) -> io::Result<Vec<u8>> {
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > limit as u64 {
        return Err(invalid());
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(invalid());
        }
    }
    let mut bytes = Vec::new();
    file.take((limit as u64).saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(invalid());
    }
    Ok(bytes)
}

pub(super) fn read(path: &Path, limit: usize) -> io::Result<Vec<u8>> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.len() > limit as u64 {
        return Err(invalid());
    }
    read_handle(open(path)?, limit)
}

/// A path check cannot establish which other names refer to a hard-linked object.
pub(super) fn read_single_link(path: &Path, limit: usize) -> io::Result<Vec<u8>> {
    let file = open(path)?;
    #[cfg(unix)]
    let links = {
        use std::os::unix::fs::MetadataExt;
        file.metadata()?.nlink()
    };
    #[cfg(windows)]
    let links = {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION};
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 { return Err(io::Error::last_os_error()); }
        info.nNumberOfLinks as u64
    };
    #[cfg(not(any(unix, windows)))]
    let links = 0;
    if links != 1 { return Err(io::Error::other("Hard-linked files require full access; use an ordinary copy instead")); }
    read_handle(file, limit)
}

/// Only ordinary relative components are accepted. Unix walks directory handles
/// with NOFOLLOW so swapping an intermediate directory cannot redirect the read.
pub(super) fn read_beneath(root: &Path, relative: &Path, limit: usize) -> io::Result<Vec<u8>> {
    let components: Vec<_> = relative.components().collect();
    if components.is_empty()
        || components
            .iter()
            .any(|c| !matches!(c, Component::Normal(_)))
        || relative.as_os_str().to_string_lossy().starts_with('~')
    {
        return Err(invalid());
    }
    #[cfg(unix)]
    {
        use std::{
            ffi::CString,
            os::{
                fd::{AsRawFd, FromRawFd},
                unix::{ffi::OsStrExt, fs::OpenOptionsExt},
            },
        };
        let mut parent = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(root)?;
        for (index, component) in components.iter().enumerate() {
            let name = CString::new(component.as_os_str().as_bytes()).map_err(|_| invalid())?;
            let directory = index + 1 < components.len();
            let flags = libc::O_RDONLY
                | libc::O_NOFOLLOW
                | libc::O_NONBLOCK
                | libc::O_CLOEXEC
                | if directory { libc::O_DIRECTORY } else { 0 };
            let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let file = unsafe { File::from_raw_fd(fd) };
            if !directory {
                return read_handle(file, limit);
            }
            parent = file;
        }
        Err(invalid())
    }
    #[cfg(windows)]
    {
        // Validate the path of the opened object, not a second path lookup.
        // Parent reparse points cannot redirect reads outside the canonical root.
        use std::os::windows::{ffi::OsStringExt, io::AsRawHandle};
        use windows_sys::Win32::Storage::FileSystem::GetFinalPathNameByHandleW;
        let root = root.canonicalize()?;
        let file = open(&root.join(relative))?;
        let mut buffer = vec![0u16; 32768];
        let count = unsafe {
            GetFinalPathNameByHandleW(
                file.as_raw_handle(),
                buffer.as_mut_ptr(),
                buffer.len() as u32,
                0,
            )
        };
        if count == 0 || count as usize >= buffer.len() {
            return Err(invalid());
        }
        let actual =
            std::path::PathBuf::from(std::ffi::OsString::from_wide(&buffer[..count as usize]));
        if !actual.starts_with(&root) {
            return Err(invalid());
        }
        read_handle(file, limit)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (root, limit);
        Err(invalid())
    }
}

pub(super) fn text(path: &Path, limit: usize) -> io::Result<String> {
    String::from_utf8(read(path, limit)?).map_err(|_| io::Error::other("File must be UTF-8"))
}

/// Only absence is optional; corrupt, oversized or linked metadata is an error.
pub(super) fn optional_text(path: &Path) -> io::Result<Option<String>> {
    match metadata_text(path) {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_oversized_and_outside_files() {
        let root = crate::kits::tests::TestKits::new();
        std::fs::create_dir(root.0.join("nested")).unwrap();
        std::fs::write(root.0.join("nested/token"), "test").unwrap();
        assert_eq!(
            read_beneath(&root.0, Path::new("nested/token"), 4).unwrap(),
            b"test"
        );
        assert!(read_beneath(&root.0, Path::new("nested/token"), 3).is_err());
        for path in [
            Path::new("../token"),
            Path::new("~/token"),
            &root.0.join("nested/token"),
        ] {
            assert!(read_beneath(&root.0, path, 100).is_err());
        }
        assert_eq!(
            read(&root.0.join("nested/token"), usize::MAX).unwrap(),
            b"test"
        );
    }
    #[cfg(unix)]
    #[test]
    fn rejects_symlink_files_and_directories() {
        use std::os::unix::fs::symlink;
        let root = crate::kits::tests::TestKits::new();
        let external = crate::kits::tests::TestKits::new();
        std::fs::write(external.0.join("token"), "test").unwrap();
        symlink(&external.0, root.0.join("directory")).unwrap();
        symlink(external.0.join("token"), root.0.join("file")).unwrap();
        assert!(read(&root.0.join("file"), 100).is_err());
        assert!(read_beneath(&root.0, Path::new("directory/token"), 100).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn rejects_windows_reparse_points_and_junction_escape() {
        let root = crate::kits::tests::TestKits::new();
        let external = crate::kits::tests::TestKits::new();
        std::fs::write(external.0.join("token"), "test").unwrap();
        let junction = root.0.join("directory");
        assert!(std::process::Command::new("cmd.exe")
            .args(["/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&external.0)
            .output()
            .unwrap()
            .status
            .success());
        assert!(read_beneath(&root.0, Path::new("directory/token"), 100).is_err());
        std::fs::remove_dir(junction).unwrap();

        let link = root.0.join("file");
        match std::os::windows::fs::symlink_file(external.0.join("token"), &link) {
            Ok(()) => {
                assert!(read(&link, 100).is_err());
                // Bypass the preliminary path check to exercise opened-handle validation.
                assert!(open(&link).and_then(|file| read_handle(file, 100)).is_err());
            }
            Err(error) if error.raw_os_error() == Some(1314) => {
                eprintln!("File symlink case requires Windows developer mode or symlink privilege");
            }
            Err(error) => panic!("Cannot create symlink fixture: {error}"),
        }
    }
}
