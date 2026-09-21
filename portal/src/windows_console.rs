/// Reuse the invoking terminal without allocating one on Explorer launch.
/// AttachConsole may replace std handles, so preserve redirected pipes/files.
pub fn attach_parent() {
    use windows_sys::Win32::{
        Foundation::INVALID_HANDLE_VALUE,
        Storage::FileSystem::{GetFileType, FILE_TYPE_DISK, FILE_TYPE_PIPE},
        System::Console::{
            AttachConsole, GetConsoleWindow, GetStdHandle, SetStdHandle, ATTACH_PARENT_PROCESS,
            STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
        },
    };
    unsafe {
        if !GetConsoleWindow().is_null() {
            return;
        }
        let saved = [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE]
            .map(|id| (id, GetStdHandle(id)));
        if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
            return;
        }
        for (id, handle) in saved {
            if !handle.is_null()
                && handle != INVALID_HANDLE_VALUE
                && matches!(GetFileType(handle), FILE_TYPE_DISK | FILE_TYPE_PIPE)
            {
                SetStdHandle(id, handle);
            }
        }
    }
}
