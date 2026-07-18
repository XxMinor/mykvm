//! Edge drop catcher for Windows→remote file drags (ShareMouse-style).
//!
//! This Windows machine is the controller. When the user drags files toward a
//! screen edge that borders a controlled machine, we must read the drag payload
//! — but a source app's in-progress OLE drag can't be read from a global hook,
//! and once the cursor "crosses" to the remote the capture hook swallows the
//! mouse events the source app's `DoDragDrop` needs, so it never delivers a drop
//! here. The sequence therefore is:
//!
//!   1. The capture hook, seeing a LEFT-BUTTON drag reach a remote edge, does
//!      NOT cross yet — it `arm`s this invisible OLE drop-target window right at
//!      the edge under the (un-warped) cursor.
//!   2. The source app's drag moves onto our window → `IDropTarget::DragEnter`
//!      fires with the data object. We read `CF_HDROP`, hand the files to the
//!      sink (which transfers them to the controlled machine's Desktop), inject
//!      Escape to end the local drag, and set a hand-off flag.
//!   3. The capture hook sees the hand-off flag and crosses for real, so the
//!      cursor slides onto the remote screen.
//!
//! NOTE: type-checked for the Windows target (cargo xwin) but NOT exercised on
//! Windows hardware. The DragEnter→transfer→Escape→cross timing is the part that
//! needs on-device verification; every step logs so a single real drag pinpoints
//! where it breaks.
#![cfg(target_os = "windows")]

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use windows::core::{implement, Ref, PCWSTR};
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
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, RegisterClassW,
    SetLayeredWindowAttributes, SetWindowPos, ShowWindow, TranslateMessage, HWND_TOPMOST, LWA_ALPHA,
    MSG, SWP_NOACTIVATE, SW_HIDE, SW_SHOWNOACTIVATE, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP,
};

/// Called with `(device_id, files)` when a drag's files are read at the edge.
pub type DropSink = Box<dyn Fn(String, Vec<PathBuf>) + Send + Sync>;

static SINK: OnceLock<DropSink> = OnceLock::new();
static CATCHER: OnceLock<Catcher> = OnceLock::new();
// Set by DragEnter once the files are read; the capture hook consumes it to
// cross to the remote so the cursor slides onto the controlled screen.
static HANDOFF: OnceLock<Mutex<Option<String>>> = OnceLock::new();

fn handoff_slot() -> &'static Mutex<Option<String>> {
    HANDOFF.get_or_init(|| Mutex::new(None))
}

/// The capture hook calls this each move; a non-None value is the device the
/// just-read drag targets, meaning "cross now".
pub fn take_handoff() -> Option<String> {
    handoff_slot().lock().ok().and_then(|mut slot| slot.take())
}

struct Catcher {
    hwnd: isize,
    device_id: Mutex<Option<String>>,
}

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

/// Park the invisible catcher over the edge at `(x, y)` (physical pixels, where
/// the un-warped dragging cursor sits) so the source app's drag moves onto it.
pub fn arm(device_id: &str, x: i32, y: i32) {
    let Some(catcher) = catcher() else {
        log::warn!("edge catcher: arm before init");
        return;
    };
    if let Ok(mut slot) = catcher.device_id.lock() {
        *slot = Some(device_id.to_string());
    }
    let hwnd = HWND(catcher.hwnd as *mut _);
    // A tall band hugging the edge, so a drag sliding toward the edge reliably
    // lands on it regardless of the exact vertical position.
    unsafe {
        let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), x - 20, y - 160, 40, 320, SWP_NOACTIVATE);
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    }
    log::info!("edge catcher: armed at ({x},{y}) for {device_id}");
}

/// Hide the catcher (control crossed, or the drag ended).
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
        if OleInitialize(None).is_err() {
            log::warn!("edge catcher: OleInitialize failed");
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
            40,
            320,
            None,
            None,
            Some(instance.into()),
            None,
        );
        let Ok(hwnd) = hwnd else {
            log::warn!("edge catcher: CreateWindowExW failed");
            return;
        };
        let _ = SetLayeredWindowAttributes(hwnd, windows::Win32::Foundation::COLORREF(0), 1, LWA_ALPHA);

        let target: IDropTarget = EdgeDropTarget.into();
        if RegisterDragDrop(hwnd, &target).is_err() {
            log::warn!("edge catcher: RegisterDragDrop failed");
            return;
        }

        let _ = CATCHER.set(Catcher {
            hwnd: hwnd.0 as isize,
            device_id: Mutex::new(None),
        });
        log::info!("edge catcher: ready");

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

impl IDropTarget_Impl for EdgeDropTarget_Impl {
    fn DragEnter(
        &self,
        pdataobj: Ref<'_, IDataObject>,
        _grfkeystate: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        pdweffect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        // The drag reached the edge. Read its files NOW (the user is still
        // holding the button), transfer them, end the local drag, and hand off
        // to the capture hook so the cursor crosses to the remote.
        let files = pdataobj
            .as_ref()
            .map(read_hdrop_files)
            .unwrap_or_default();
        if !pdweffect.is_null() {
            unsafe { *pdweffect = DROPEFFECT_NONE };
        }
        if files.is_empty() {
            log::info!("edge catcher: DragEnter with no files (not a file drag)");
            return Ok(());
        }

        let device_id = catcher()
            .and_then(|catcher| catcher.device_id.lock().ok().and_then(|slot| slot.clone()));
        let Some(device_id) = device_id else {
            log::warn!("edge catcher: DragEnter but no armed device");
            return Ok(());
        };

        log::info!(
            "edge catcher: DragEnter read {} file(s) -> {}; transferring + handing off",
            files.len(),
            device_id
        );
        if let Some(sink) = SINK.get() {
            sink(device_id.clone(), files);
        }
        // End the source app's local drag so it doesn't drop a copy here.
        inject_escape();
        // Tell the capture hook to cross so the cursor slides onto the remote.
        if let Ok(mut slot) = handoff_slot().lock() {
            *slot = Some(device_id);
        }
        disarm();
        Ok(())
    }

    fn DragOver(
        &self,
        _grfkeystate: MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        pdweffect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        if !pdweffect.is_null() {
            unsafe { *pdweffect = DROPEFFECT_NONE };
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
        // Fallback: if the user actually released ON the edge band (no cross),
        // still deliver the files.
        let files = pdataobj
            .as_ref()
            .map(read_hdrop_files)
            .unwrap_or_default();
        if !pdweffect.is_null() {
            unsafe { *pdweffect = DROPEFFECT_COPY };
        }
        let device_id = catcher()
            .and_then(|catcher| catcher.device_id.lock().ok().and_then(|slot| slot.clone()));
        disarm();
        if let (Some(device_id), Some(sink)) = (device_id, SINK.get()) {
            if !files.is_empty() {
                log::info!("edge catcher: Drop delivered {} file(s) -> {}", files.len(), device_id);
                sink(device_id, files);
            }
        }
        Ok(())
    }
}

fn hdrop_format() -> FORMATETC {
    FORMATETC {
        cfFormat: CF_HDROP.0,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    }
}

fn read_hdrop_files(data_object: &IDataObject) -> Vec<PathBuf> {
    let format = hdrop_format();
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

/// End the source app's in-progress OLE drag (Escape is the OLE drag cancel).
fn inject_escape() {
    const VK_ESCAPE: u16 = 0x1B;
    let mut inputs = [
        key_input(VK_ESCAPE, false),
        key_input(VK_ESCAPE, true),
    ];
    unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_mut_ptr(),
            std::mem::size_of::<INPUT>() as i32,
        );
    }
}

fn key_input(vk: u16, up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: if up { KEYEVENTF_KEYUP } else { 0 },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}
