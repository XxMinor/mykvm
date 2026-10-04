#![cfg(target_os = "windows")]

use std::{ptr, sync::OnceLock};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    System::LibraryLoader::GetModuleHandleW,
    UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, GetCursorInfo, RegisterClassW, SetCursor,
        SetLayeredWindowAttributes, SetWindowPos, ShowWindow, CURSORINFO, HTCLIENT, HWND_TOPMOST,
        LWA_ALPHA, MA_NOACTIVATE, SWP_NOACTIVATE, SWP_NOSIZE, SWP_SHOWWINDOW, SW_HIDE,
        WM_ERASEBKGND, WM_MOUSEACTIVATE, WM_NCHITTEST, WM_SETCURSOR, WNDCLASSW, WS_EX_LAYERED,
        WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
    },
};

static CLASS_REGISTERED: OnceLock<bool> = OnceLock::new();

/// A non-activating three-pixel window at the pinned cursor position owns
/// mouse input while sharing. This makes SetCursor(NULL) and the capture
/// thread's ShowCursor counter apply to the actual visible cursor, even after
/// a screenshot overlay changed the foreground input queue. Cursor schemes
/// and the foreground application's focus are left intact.
pub(crate) struct CursorGuard(HWND);
impl CursorGuard {
    pub(crate) fn new() -> Option<Self> {
        let instance = unsafe { GetModuleHandleW(ptr::null()) };
        if !*CLASS_REGISTERED.get_or_init(|| unsafe {
            RegisterClassW(&WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                hInstance: instance,
                lpszClassName: windows_sys::w!("MyKVMRemoteCursorOwner"),
                ..Default::default()
            }) != 0
        }) {
            return None;
        }
        let window = unsafe {
            CreateWindowExW(
                WS_EX_LAYERED | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
                windows_sys::w!("MyKVMRemoteCursorOwner"),
                ptr::null(),
                WS_POPUP,
                -10_000,
                -10_000,
                3,
                3,
                ptr::null_mut(),
                ptr::null_mut(),
                instance,
                ptr::null(),
            )
        };
        if window.is_null() {
            return None;
        }
        // Nonzero alpha retains hit testing; the window never receives focus.
        if unsafe { SetLayeredWindowAttributes(window, 0, 1, LWA_ALPHA) } == 0 {
            unsafe {
                DestroyWindow(window);
            }
            return None;
        }
        Some(Self(window))
    }
    pub(crate) fn handle(&self) -> usize {
        self.0 as usize
    }
}
impl Drop for CursorGuard {
    fn drop(&mut self) {
        unsafe {
            DestroyWindow(self.0);
        }
    }
}
pub(crate) fn show_at(handle: usize, x: i32, y: i32) {
    if handle != 0 {
        unsafe {
            SetWindowPos(
                handle as HWND,
                HWND_TOPMOST,
                x - 1,
                y - 1,
                0,
                0,
                SWP_NOACTIVATE | SWP_NOSIZE | SWP_SHOWWINDOW,
            );
        }
    }
}
pub(crate) fn hide(handle: usize) {
    if handle != 0 {
        unsafe {
            ShowWindow(handle as HWND, SW_HIDE);
        }
    }
}
pub(crate) fn visibility() -> Option<u32> {
    let mut info = CURSORINFO {
        cbSize: std::mem::size_of::<CURSORINFO>() as u32,
        ..Default::default()
    };
    (unsafe { GetCursorInfo(&mut info) } != 0).then_some(info.flags)
}
unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_SETCURSOR => {
            SetCursor(ptr::null_mut());
            1
        }
        WM_MOUSEACTIVATE => MA_NOACTIVATE as isize,
        WM_NCHITTEST => HTCLIENT as isize,
        WM_ERASEBKGND => 1,
        _ => DefWindowProcW(window, message, wparam, lparam),
    }
}
