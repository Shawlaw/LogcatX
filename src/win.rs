//! Small Windows window helpers used by the scrcpy long-recording path.
//!
//! scrcpy's `--no-playback` mode still creates its SDL window (closing that
//! window is the graceful stop that finalizes the MP4). We hide the window
//! right after launch and post `WM_CLOSE` when the user stops recording.

#[cfg(windows)]
pub fn hide_windows_for_pid(pid: u32) {
    use windows_sys::{
        Win32::{
            Foundation::{HWND, LPARAM},
            UI::WindowsAndMessaging::{EnumWindows, GetWindowThreadProcessId, SW_HIDE, ShowWindow},
        },
        core::BOOL,
    };

    unsafe extern "system" fn callback(window: HWND, lparam: LPARAM) -> BOOL {
        let target = lparam as u32;
        let mut owner = 0u32;
        // SAFETY: the callback only passes a valid HWND supplied by EnumWindows.
        unsafe {
            GetWindowThreadProcessId(window, &mut owner);
            if owner == target {
                ShowWindow(window, SW_HIDE);
            }
        }
        1
    }

    // SAFETY: callback matches the WNDENUMPROC signature and lives for the call.
    unsafe {
        EnumWindows(Some(callback), pid as LPARAM);
    }
}

/// Post `WM_CLOSE` to every top-level window owned by `pid`.
#[cfg(windows)]
pub fn close_windows_for_pid(pid: u32) {
    use windows_sys::{
        Win32::{
            Foundation::{HWND, LPARAM},
            UI::WindowsAndMessaging::{
                EnumWindows, GetWindowThreadProcessId, PostMessageW, WM_CLOSE,
            },
        },
        core::BOOL,
    };

    unsafe extern "system" fn callback(window: HWND, lparam: LPARAM) -> BOOL {
        let target = lparam as u32;
        let mut owner = 0u32;
        // SAFETY: the callback only passes a valid HWND supplied by EnumWindows.
        unsafe {
            GetWindowThreadProcessId(window, &mut owner);
            if owner == target {
                PostMessageW(window, WM_CLOSE, 0, 0);
            }
        }
        1
    }

    // SAFETY: callback matches the WNDENUMPROC signature and lives for the call.
    unsafe {
        EnumWindows(Some(callback), pid as LPARAM);
    }
}

#[cfg(not(windows))]
pub fn hide_windows_for_pid(_pid: u32) {}

#[cfg(not(windows))]
pub fn close_windows_for_pid(_pid: u32) {}
