//! Edge drop catcher for Windows→remote file drags.
//!
//! When this Windows machine is the controller and the user drags files toward
//! a screen edge that borders a controlled machine, the file drag lives inside
//! the source app's `DoDragDrop` loop — a global hook can't read its payload.
//! So we register a small, invisible OLE drop-target window and, while a drag
//! crosses the edge, park it under the (pinned) cursor. The source app's drag
//! loop delivers the drop to it; we read the `CF_HDROP` file list and hand it to
//! a sink that transfers the files to the controlled machine.
//!
//! NOTE: type-checked for the Windows target (cargo xwin) but NOT exercised on
//! Windows hardware. The interaction between the source app's DoDragDrop loop
//! and this app's cursor-warp handoff is the main thing that needs on-device
//! verification.
#![cfg(target_os = "windows")]

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use windows::core::{implement, PCWSTR, Ref};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINTL, WPARAM};
use windows::Win32::System::Com::{IDataObject, DVASPECT_CONTENT, FORMATETC, TYMED_HGLOBAL};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Ole::{
    OleInitialize, RegisterDragDrop, ReleaseStgMedium, CF_HDROP, DROPEFFECT, DROPEFFECT_COPY,
    DROPEFFECT_NONE, IDropTarget, IDropTarget_Impl,
};
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, RegisterClassW, SetWindowPos,
    SetLayeredWindowAttributes, ShowWindow, TranslateMessage, HWND_TOPMOST, LWA_ALPHA, MSG,
    SWP_NOACTIVATE, SW_HIDE, SW_SHOWNOACTIVATE, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

/// Called with `(device_id, files)` when files are dropped on the edge catcher.
pub type DropSink = Box<dyn Fn(String, Vec<PathBuf>) + Send + Sync>;

static SINK: OnceLock<DropSink> = OnceLock::new();
static CATCHER: OnceLock<Catcher> = OnceLock::new();

struct Catcher {
    hwnd: isize,
    // The controlled device the current edge drag targets.
    device_id: Mutex<Option<String>>,
}

// HWND is just a handle; sending it across threads (hook thread arms, STA thread
// owns) is fine for the SetWindowPos/ShowWindow calls we make.
unsafe impl Send for Catcher {}
unsafe impl Sync for Catcher {}

fn catcher() -> Option<&'static Catcher> {
    CATCHER.get()
}

/// Install the drop sink and start the catcher thread. Idempotent.
pub fn init(sink: DropSink) {
    let _ = SINK.set(sink);
    if CATCHER.get().is_some() {
        return;
    }
    std::thread::spawn(run_catcher_thread);
}

/// Park the invisible catcher under the pinned cursor at `(x, y)` (physical
/// pixels) so the source app's drag loop drops onto it; remember which device
/// the files should go to. Called from the capture hook on an edge drag.
pub fn arm(device_id: &str, x: i32, y: i32) {
    let Some(catcher) = catcher() else {
        return;
    };
    if let Ok(mut slot) = catcher.device_id.lock() {
        *slot = Some(device_id.to_string());
    }
    let hwnd = HWND(catcher.hwnd as *mut _);
    // A tall, thin box straddling the cursor at the edge — big enough to catch
    // the drop, small enough to rarely sit under a real click.
    unsafe {
        let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), x - 8, y - 32, 16, 64, SWP_NOACTIVATE);
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    }
}

/// Hide the catcher (control returned to the local machine, or the drag ended).
pub fn disarm() {
    let Some(catcher) = catcher() else {
        return;
    };
    if let Ok(mut slot) = catcher.device_id.lock() {
        *slot = None;
    }
    let hwnd = HWND(catcher.hwnd as *mut _);
    unsafe {
        let _ = ShowWindow(hwnd, SW_HIDE);
    }
}

fn run_catcher_thread() {
    unsafe {
        // OLE drop targets require an OLE-initialized STA thread with a pump.
        if OleInitialize(None).is_err() {
            return;
        }

        let instance = match GetModuleHandleW(None) {
            Ok(instance) => instance,
            Err(_) => return,
        };
        let class_name = windows::core::w!("MyKvmEdgeDropCatcher");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: instance.into(),
            lpszClassName: class_name,
            ..Default::default()
        };
        RegisterClassW(&wc);

        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            class_name,
            PCWSTR::null(),
            WS_POPUP,
            0,
            0,
            16,
            64,
            None,
            None,
            Some(instance.into()),
            None,
        );
        let Ok(hwnd) = hwnd else {
            return;
        };
        // Near-invisible: present enough to be a drop target, not visible.
        let _ = SetLayeredWindowAttributes(hwnd, windows::Win32::Foundation::COLORREF(0), 1, LWA_ALPHA);

        let target: IDropTarget = EdgeDropTarget.into();
        if RegisterDragDrop(hwnd, &target).is_err() {
            return;
        }

        let _ = CATCHER.set(Catcher {
            hwnd: hwnd.0 as isize,
            device_id: Mutex::new(None),
        });

        // Pump messages so OLE can deliver drag events to the target.
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

#[implement(IDropTarget)]
struct EdgeDropTarget;

impl EdgeDropTarget {
    fn accept(data_object: Ref<'_, IDataObject>) -> bool {
        let Some(data_object) = data_object.as_ref() else {
            return false;
        };
        let format = FORMATETC {
            cfFormat: CF_HDROP.0,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0,
            lindex: -1,
            tymed: TYMED_HGLOBAL.0 as u32,
        };
        unsafe { data_object.QueryGetData(&format).is_ok() }
    }
}

impl IDropTarget_Impl for EdgeDropTarget_Impl {
    fn DragEnter(
        &self,
        pdataobj: Ref<'_, IDataObject>,
        _grfkeystate: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        pdweffect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        let effect = if EdgeDropTarget::accept(pdataobj) {
            DROPEFFECT_COPY
        } else {
            DROPEFFECT_NONE
        };
        if !pdweffect.is_null() {
            unsafe { *pdweffect = effect };
        }
        Ok(())
    }

    fn DragOver(
        &self,
        _grfkeystate: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        pdweffect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        if !pdweffect.is_null() {
            // We only ever advertise copy; the source decides the final effect.
            unsafe { *pdweffect = DROPEFFECT_COPY };
        }
        Ok(())
    }

    fn DragLeave(&self) -> windows::core::Result<()> {
        disarm();
        Ok(())
    }

    fn Drop(
        &self,
        pdataobj: Ref<'_, IDataObject>,
        _grfkeystate: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        pdweffect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        let files = pdataobj
            .as_ref()
            .map(|data_object| read_hdrop_files(data_object))
            .unwrap_or_default();
        if !pdweffect.is_null() {
            unsafe { *pdweffect = DROPEFFECT_COPY };
        }

        let device_id = catcher()
            .and_then(|catcher| catcher.device_id.lock().ok().and_then(|slot| slot.clone()));
        disarm();

        if let (Some(device_id), Some(sink)) = (device_id, SINK.get()) {
            if !files.is_empty() {
                log::info!("edge catcher: dropping {} file(s) -> {}", files.len(), device_id);
                sink(device_id, files);
            }
        }
        Ok(())
    }
}

fn read_hdrop_files(data_object: &IDataObject) -> Vec<PathBuf> {
    let format = FORMATETC {
        cfFormat: CF_HDROP.0,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    };
    let mut medium = match unsafe { data_object.GetData(&format) } {
        Ok(medium) => medium,
        Err(_) => return Vec::new(),
    };

    let mut paths = Vec::new();
    unsafe {
        let hdrop = HDROP(medium.u.hGlobal.0);
        let count = DragQueryFileW(hdrop, 0xFFFF_FFFF, None);
        for index in 0..count {
            let len = DragQueryFileW(hdrop, index, None);
            if len == 0 {
                continue;
            }
            let mut buffer = vec![0u16; len as usize + 1];
            let written = DragQueryFileW(hdrop, index, Some(&mut buffer));
            if written == 0 {
                continue;
            }
            let path = String::from_utf16_lossy(&buffer[..written as usize]);
            if !path.is_empty() {
                paths.push(PathBuf::from(path));
            }
        }
        ReleaseStgMedium(&mut medium);
    }
    paths
}
