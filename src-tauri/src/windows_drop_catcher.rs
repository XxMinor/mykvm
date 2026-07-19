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
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINTL, WPARAM};
use windows::Win32::Graphics::Gdi::CreateSolidBrush;
use windows::Win32::System::Com::{IDataObject, DVASPECT_CONTENT, FORMATETC, TYMED_HGLOBAL};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Ole::{
    OleInitialize, RegisterDragDrop, ReleaseStgMedium, CF_HDROP, DROPEFFECT, DROPEFFECT_COPY,
    DROPEFFECT_NONE, IDropTarget, IDropTarget_Impl,
};
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, PostMessageW, RegisterClassW,
    SetWindowPos, ShowWindow, TranslateMessage, HWND_TOPMOST, MSG, SWP_NOACTIVATE, SW_HIDE,
    SW_SHOWNOACTIVATE, WM_APP, WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

// The drop-target box is ~invisible-brief but must be a real, non-transparent,
// on-screen window or OLE drag hit-testing (WindowFromPoint) skips it.
const CATCH_W: i32 = 36;
const CATCH_H: i32 = 220;
// Arm/disarm are posted to the window's own (STA) thread; doing SetWindowPos /
// ShowWindow there — not cross-thread from the capture hook — is what actually
// moves and shows the window in time for the drag to land on it.
const WM_ARM: u32 = WM_APP + 1;
const WM_DISARM: u32 = WM_APP + 2;

use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_KEYUP,
    MOUSEEVENTF_LEFTUP, MOUSEINPUT,
};

/// Called with `(device_id, files)` when a drag's files are read at the edge.
pub type DropSink = Box<dyn Fn(String, Vec<PathBuf>) + Send + Sync>;

static SINK: OnceLock<DropSink> = OnceLock::new();
static CATCHER: OnceLock<Catcher> = OnceLock::new();
// Set by DragEnter once the files are read; the capture hook consumes it to
// cross to the remote so the cursor slides onto the controlled screen.
static HANDOFF: OnceLock<Mutex<Option<String>>> = OnceLock::new();
// True once a drag has been captured for the current left-button hold, so
// shuttling the cursor back and forth across the edge can't read + transfer the
// same drag again (each round used to make another copy). Reset on button-up.
static HOLD_CONSUMED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// True while the current button hold has already handed off one drag; the
/// capture hook then skips re-arming so no duplicate transfer happens.
pub fn hold_consumed() -> bool {
    HOLD_CONSUMED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Clear the per-hold guard — called when the left button is released.
pub fn reset_hold() {
    HOLD_CONSUMED.store(false, std::sync::atomic::Ordering::Relaxed);
}

/// Inject a left-button up to clear Windows' held-drag state (used when the
/// cursor crosses back to this machine mid-drag so it isn't stuck dragging).
pub fn inject_left_up() {
    let mut input = [left_up_input()];
    unsafe {
        SendInput(1, input.as_mut_ptr(), std::mem::size_of::<INPUT>() as i32);
    }
}

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

/// Park the catcher over the edge at `(x, y)` (physical pixels, where the
/// un-warped dragging cursor sits) so the source app's drag moves onto it.
/// The move+show is posted to the window's own thread (see WM_ARM).
pub fn arm(device_id: &str, x: i32, y: i32) {
    let Some(catcher) = catcher() else {
        log::warn!("edge catcher: arm before init");
        return;
    };
    if let Ok(mut slot) = catcher.device_id.lock() {
        *slot = Some(device_id.to_string());
    }
    let hwnd = HWND(catcher.hwnd as *mut _);
    // Pack the (possibly negative on multi-monitor) coordinates through the
    // message params; the window thread unpacks and positions itself.
    let wparam = WPARAM((x as i32) as u32 as usize);
    let lparam = LPARAM((y as i32) as isize);
    unsafe {
        let _ = PostMessageW(Some(hwnd), WM_ARM, wparam, lparam);
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
        let _ = PostMessageW(Some(hwnd), WM_DISARM, WPARAM(0), LPARAM(0));
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
        // A solid background so the window has real, hit-testable pixels — a
        // transparent/layered window is skipped by OLE drag hit-testing. It is
        // only on-screen for the instant a drag rests on the edge before the
        // cursor crosses, so a faint tint is fine.
        let brush = CreateSolidBrush(COLORREF(0x00F0_A030));
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: instance.into(),
            lpszClassName: class_name,
            hbrBackground: brush,
            ..Default::default()
        };
        RegisterClassW(&wc);

        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            class_name,
            PCWSTR::null(),
            WS_POPUP,
            -10_000,
            -10_000,
            CATCH_W,
            CATCH_H,
            None,
            None,
            Some(instance.into()),
            None,
        );
        let Ok(hwnd) = hwnd else {
            log::warn!("edge catcher: CreateWindowExW failed");
            return;
        };

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
    // Position/show on THIS (the window's) thread; cross-thread SetWindowPos
    // from the capture hook did not reliably move it under the dragging cursor.
    match msg {
        WM_ARM => {
            let x = (wparam.0 as u32) as i32;
            let y = lparam.0 as i32;
            unsafe {
                let _ = SetWindowPos(
                    hwnd,
                    Some(HWND_TOPMOST),
                    x - CATCH_W / 2,
                    y - CATCH_H / 2,
                    CATCH_W,
                    CATCH_H,
                    SWP_NOACTIVATE,
                );
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            }
            LRESULT(0)
        }
        WM_DISARM => {
            unsafe {
                let _ = ShowWindow(hwnd, SW_HIDE);
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
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
        if !pdweffect.is_null() {
            unsafe { *pdweffect = DROPEFFECT_NONE };
        }
        // One capture per button hold: shuttling back and forth across the edge
        // must not read + transfer the drag again (that made a copy each time).
        if HOLD_CONSUMED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            disarm();
            return Ok(());
        }
        let files = pdataobj
            .as_ref()
            .map(read_hdrop_files)
            .unwrap_or_default();
        if files.is_empty() {
            HOLD_CONSUMED.store(false, std::sync::atomic::Ordering::Relaxed);
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
        // End the source app's local drag (and clear the held button) before
        // the hand-off, so Windows doesn't come back stuck in a drag state.
        inject_end_drag();
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

/// End the source app's in-progress OLE drag: Escape cancels the drag, and a
/// left-button up clears Windows' held-button state. Without the button-up the
/// physical press stays "down" from Windows' view (its real release is later
/// swallowed and forwarded to the Mac), so the cursor comes back to Windows
/// still in a dragging state. Injected before the hand-off, while we are not yet
/// remote-active, so the events reach Windows instead of the Mac.
fn inject_end_drag() {
    const VK_ESCAPE: u16 = 0x1B;
    let mut inputs = [
        key_input(VK_ESCAPE, false),
        key_input(VK_ESCAPE, true),
        left_up_input(),
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

fn left_up_input() -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: 0,
                dwFlags: MOUSEEVENTF_LEFTUP,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}
