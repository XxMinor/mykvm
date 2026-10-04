use super::ForegroundApplication;
use std::cell::RefCell;
use windows_sys::Win32::Foundation::{CloseHandle, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetClassNameW, GetForegroundWindow, GetWindowLongPtrW, GetWindowRect, GetWindowThreadProcessId,
    GWL_STYLE, WS_CAPTION,
};

thread_local! {
    static PROCESS_NAME: RefCell<(u32, Vec<String>)> = RefCell::new((0, Vec::new()));
}

pub(super) fn foreground_application() -> ForegroundApplication {
    unsafe {
        let window = GetForegroundWindow();
        if window.is_null() {
            return ForegroundApplication::default();
        }
        let mut pid = 0;
        GetWindowThreadProcessId(window, &mut pid);
        if pid == 0 || pid == GetCurrentProcessId() {
            return ForegroundApplication::default();
        }
        let names = PROCESS_NAME.with(|cache| {
            let mut cache = cache.borrow_mut();
            if cache.0 != pid {
                cache.0 = pid;
                cache.1.clear();
                let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
                if !process.is_null() {
                    let mut path = [0_u16; 4096];
                    let mut len = path.len() as u32;
                    if QueryFullProcessImageNameW(process, 0, path.as_mut_ptr(), &mut len) != 0 {
                        let path = String::from_utf16_lossy(&path[..len as usize]);
                        if let Some(name) = path.rsplit(['/', '\\']).next() {
                            cache.1.push(name.to_string());
                        }
                    }
                    CloseHandle(process);
                }
            }
            cache.1.clone()
        });
        let mut class = [0_u16; 128];
        let class_len = GetClassNameW(window, class.as_mut_ptr(), class.len() as i32);
        let class = String::from_utf16_lossy(&class[..class_len.max(0) as usize]);
        let desktop = matches!(
            class.as_str(),
            "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd"
        );
        let mut bounds = RECT::default();
        let mut monitor = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let fullscreen = !desktop
            && GetWindowRect(window, &mut bounds) != 0
            && GetMonitorInfoW(
                MonitorFromWindow(window, MONITOR_DEFAULTTONEAREST),
                &mut monitor,
            ) != 0
            && GetWindowLongPtrW(window, GWL_STYLE) as u32 & WS_CAPTION == 0
            && bounds.left <= monitor.rcMonitor.left
            && bounds.top <= monitor.rcMonitor.top
            && bounds.right >= monitor.rcMonitor.right
            && bounds.bottom >= monitor.rcMonitor.bottom;
        ForegroundApplication { names, fullscreen }
    }
}
