//! Small Win32 helpers shared by the tray, the collector and the windows.

use std::ffi::c_void;
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, LocalFree, ERROR_ALREADY_EXISTS, HANDLE, POINT, RECT,
};
use windows_sys::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
use windows_sys::Win32::System::Threading::{
    CreateEventW, CreateMutexW, GetCurrentProcess, OpenProcessToken, SetEvent,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, MessageBoxW, MB_ICONERROR, MB_ICONINFORMATION, MB_OK,
};

/// A NUL-terminated UTF-16 copy of `text`, as Win32 wants strings.
pub fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Reads a NUL-terminated UTF-16 string.
///
/// # Safety
/// `pointer` must be null or point at a NUL-terminated UTF-16 string.
pub unsafe fn from_wide(pointer: *const u16) -> String {
    if pointer.is_null() {
        return String::new();
    }
    let mut length = 0;
    while *pointer.add(length) != 0 {
        length += 1;
    }
    String::from_utf16_lossy(std::slice::from_raw_parts(pointer, length))
}

pub fn message_box(text: &str, error: bool) {
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            wide(text).as_ptr(),
            wide("Speed Tracker").as_ptr(),
            MB_OK
                | if error {
                    MB_ICONERROR
                } else {
                    MB_ICONINFORMATION
                },
        )
    };
}

/// The signed-in user's security identifier, such as `S-1-5-21-…`. Names per-user kernel objects
/// and the collector pipe's access list.
pub fn user_sid() -> Option<String> {
    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return None;
        }
        let mut needed = 0u32;
        GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed);
        // Eight-byte units keep the returned structure aligned.
        let mut buffer = vec![0u64; (needed as usize).div_ceil(8).max(1)];
        let read = GetTokenInformation(
            token,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            needed,
            &mut needed,
        ) != 0;
        CloseHandle(token);
        if !read {
            return None;
        }
        let user = &*(buffer.as_ptr() as *const TOKEN_USER);
        let mut text: *mut u16 = std::ptr::null_mut();
        if ConvertSidToStringSidW(user.User.Sid, &mut text) == 0 {
            return None;
        }
        let sid = from_wide(text);
        LocalFree(text as *mut c_void);
        Some(sid)
    }
}

/// Held for the life of the first instance. A second launch finds it and asks the first to open
/// the dashboard instead of starting again.
pub struct SingleInstance {
    mutex: HANDLE,
    pub reopen: HANDLE,
    pub first: bool,
}

impl SingleInstance {
    pub fn acquire() -> SingleInstance {
        let user = user_sid()
            .or_else(|| std::env::var("USERNAME").ok())
            .unwrap_or_default();
        unsafe {
            let mutex = CreateMutexW(
                std::ptr::null(),
                1,
                wide(&format!("Local\\SpeedTracker.{user}")).as_ptr(),
            );
            let first = GetLastError() != ERROR_ALREADY_EXISTS;
            let reopen = CreateEventW(
                std::ptr::null(),
                0,
                0,
                wide(&format!("Local\\SpeedTracker.Open.{user}")).as_ptr(),
            );
            SingleInstance {
                mutex,
                reopen,
                first,
            }
        }
    }
    pub fn signal_first(&self) {
        unsafe { SetEvent(self.reopen) };
    }
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.reopen);
            CloseHandle(self.mutex);
        }
    }
}

/// The usable area (without the taskbar) of the monitor the pointer is on, in physical pixels:
/// left, top, right, bottom.
pub fn work_area_at_cursor() -> Option<[i32; 4]> {
    unsafe {
        let mut point = POINT { x: 0, y: 0 };
        if GetCursorPos(&mut point) == 0 {
            return None;
        }
        let mut info: MONITORINFO = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST), &mut info) == 0 {
            return None;
        }
        let RECT {
            left,
            top,
            right,
            bottom,
        } = info.rcWork;
        Some([left, top, right, bottom])
    }
}
