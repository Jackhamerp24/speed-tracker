//! Which processes are running. Names and, for the collector, start times and command lines.
//! A process's environment is never read: it can hold API keys.

pub struct ProcessInfo {
    pub pid: u32,
    /// The executable's file name without `.exe`.
    pub name: String,
}

/// The names of all running processes.
pub fn names() -> Vec<String> {
    list().into_iter().map(|process| process.name).collect()
}

/// What each running process is, for the harness classifier: the full path of anything named like a
/// harness, so a desktop app that shares a harness's file name can be told apart, and otherwise the name.
pub fn executables() -> Vec<String> {
    list()
        .into_iter()
        .map(|process| {
            if crate::harness::classify(&process.name, None).is_some() {
                image_path(process.pid).unwrap_or(process.name)
            } else {
                process.name
            }
        })
        .collect()
}

#[cfg(not(windows))]
pub fn image_path(_pid: u32) -> Option<String> {
    None
}

#[cfg(not(windows))]
pub fn list() -> Vec<ProcessInfo> {
    Vec::new()
}

#[cfg(windows)]
pub use windows::*;

#[cfg(windows)]
mod windows {
    use super::ProcessInfo;
    use std::ffi::c_void;
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, QueryFullProcessImageNameW,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };

    #[link(name = "ntdll")]
    extern "system" {
        fn NtQueryInformationProcess(
            process: HANDLE,
            class: u32,
            information: *mut c_void,
            length: u32,
            returned: *mut u32,
        ) -> i32;
    }
    const PROCESS_COMMAND_LINE_INFORMATION: u32 = 60;

    pub fn list() -> Vec<ProcessInfo> {
        let mut processes = Vec::new();
        unsafe {
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snapshot == INVALID_HANDLE_VALUE {
                return processes;
            }
            let mut entry: PROCESSENTRY32W = std::mem::zeroed();
            entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
            let mut more = Process32FirstW(snapshot, &mut entry) != 0;
            while more {
                let length = entry
                    .szExeFile
                    .iter()
                    .position(|unit| *unit == 0)
                    .unwrap_or(entry.szExeFile.len());
                let mut name = String::from_utf16_lossy(&entry.szExeFile[..length]);
                if name.len() > 4 && name[name.len() - 4..].eq_ignore_ascii_case(".exe") {
                    name.truncate(name.len() - 4);
                }
                processes.push(ProcessInfo {
                    pid: entry.th32ProcessID,
                    name,
                });
                more = Process32NextW(snapshot, &mut entry) != 0;
            }
            CloseHandle(snapshot);
        }
        processes
    }

    struct Process(HANDLE);

    impl Process {
        fn open(pid: u32) -> Option<Process> {
            let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
            (!handle.is_null()).then_some(Process(handle))
        }
    }

    impl Drop for Process {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }

    /// When the process started, as a Windows file time. Tells a reused process id from the original.
    pub fn start_time(pid: u32) -> Option<u64> {
        let process = Process::open(pid)?;
        let mut times = [FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        }; 4];
        let [created, exited, kernel, user] = &mut times;
        (unsafe { GetProcessTimes(process.0, created, exited, kernel, user) } != 0)
            .then(|| (u64::from(times[0].dwHighDateTime) << 32) | u64::from(times[0].dwLowDateTime))
    }

    /// Where the process's executable is. None for a process this user may not query.
    pub fn image_path(pid: u32) -> Option<String> {
        let process = Process::open(pid)?;
        let mut buffer = [0u16; 1024];
        let mut length = buffer.len() as u32;
        (unsafe { QueryFullProcessImageNameW(process.0, 0, buffer.as_mut_ptr(), &mut length) } != 0)
            .then(|| String::from_utf16_lossy(&buffer[..length as usize]))
    }

    /// The process's command line: its arguments only.
    pub fn command_line(pid: u32) -> Option<String> {
        #[repr(C)]
        struct UnicodeString {
            length: u16,
            maximum_length: u16,
            buffer: *const u16,
        }
        let process = Process::open(pid)?;
        unsafe {
            let mut needed = 0u32;
            NtQueryInformationProcess(
                process.0,
                PROCESS_COMMAND_LINE_INFORMATION,
                std::ptr::null_mut(),
                0,
                &mut needed,
            );
            if needed == 0 || needed > 1024 * 1024 {
                return None;
            }
            // Eight-byte units keep the returned structure aligned.
            let mut buffer = vec![0u64; (needed as usize).div_ceil(8)];
            if NtQueryInformationProcess(
                process.0,
                PROCESS_COMMAND_LINE_INFORMATION,
                buffer.as_mut_ptr().cast(),
                needed,
                &mut needed,
            ) < 0
            {
                return None;
            }
            let text = &*(buffer.as_ptr() as *const UnicodeString);
            if text.buffer.is_null() {
                return None;
            }
            Some(String::from_utf16_lossy(std::slice::from_raw_parts(
                text.buffer,
                usize::from(text.length) / 2,
            )))
        }
    }
}
