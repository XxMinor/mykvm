//! OLE drop catcher used for both directions of a cross-screen file drag.
//!
//! CONTROLLER SIDE (this machine drives the cursor). When the user drags files
//! toward a screen edge that borders a controlled machine, we must read the
//! drag payload
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
//! CONTROLLED SIDE (a remote controller drives our cursor). The user drags on
//! this machine with the controller's injected input, then crosses back to the
//! controller's own screen. The controller sends a drag-control "pull" and
//! `handoff_to_controller` runs the same read-at-DragEnter path, except the
//! cursor has already stopped moving here — so the catcher parks under the
//! cursor and nudges the pointer to force one more OLE hit-test. The files then
//! transfer to the controller, which places them where its button is released.
//!
//! NOTE: type-checked for the Windows target (cargo xwin) but NOT exercised on
//! Windows hardware. The DragEnter→transfer→Escape→cross timing is the part that
//! needs on-device verification; every step logs so a single real drag pinpoints
//! where it breaks.
#![cfg(target_os = "windows")]

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use windows::core::{implement, Ref, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, POINTL, WPARAM};
use windows::Win32::Graphics::Gdi::CreateSolidBrush;
use windows::Win32::System::Com::{IDataObject, DVASPECT_CONTENT, FORMATETC, TYMED_HGLOBAL};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Ole::{
    OleInitialize, RegisterDragDrop, ReleaseStgMedium, CF_HDROP, DROPEFFECT, DROPEFFECT_COPY,
    DROPEFFECT_NONE, IDropTarget, IDropTarget_Impl,
};
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use windows::Win32::UI::Shell::{DragAcceptFiles, DragFinish, DragQueryFileW, HDROP};
use windows::Win32::UI::WindowsAndMessaging::{
    ChangeWindowMessageFilterEx, CreateWindowExW, DefWindowProcW, DispatchMessageW, GetClassNameW,
    GetCursorPos, GetForegroundWindow, GetGUIThreadInfo, GetMessageW, GetWindowThreadProcessId,
    KillTimer, PostMessageW, RegisterClassW, SetTimer, SetWindowPos, ShowWindow, TranslateMessage,
    WindowFromPoint, GUITHREADINFO, HWND_TOPMOST, MSG, MSGFLT_ALLOW,
    SWP_NOACTIVATE, SW_HIDE, SW_SHOWNOACTIVATE, WM_APP, WM_COPYDATA, WM_DROPFILES, WM_TIMER,
    WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

// The drop-target box is ~invisible-brief but must be a real, non-transparent,
// on-screen window or OLE drag hit-testing (WindowFromPoint) skips it.
const CATCH_W: i32 = 80;
const CATCH_H: i32 = 220;
/// How long a drop that was asked for may take to arrive. Shorter than the
/// hook's own wait at the edge and than the hand-off's retraction, so both
/// find the matter settled.
const DROP_WAIT_MS: u32 = 300;
const DROP_TIMER: usize = 1;
// Arm/disarm are posted to the window's own (STA) thread; doing SetWindowPos /
// ShowWindow there — not cross-thread from the capture hook — is what actually
// moves and shows the window in time for the drag to land on it.
const WM_ARM: u32 = WM_APP + 1;
const WM_DISARM: u32 = WM_APP + 2;

use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_KEYUP,
    MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MOVE, MOUSEINPUT,
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
// MyKVM runs elevated, and Windows does not let a program that is not
// (Explorer) hand an OLE drag to one that is: DragEnter never came, and every
// drag toward the other computer ended as "catcher missed". The older
// file-drop message can be let through, but it only arrives on a drop, so the
// catcher asks for one (see `force_drop`).
static LEGACY_DROP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// True from asking the drag to drop until the drop arrives or `DROP_WAIT_MS`
/// is over. The catcher stays where it is for that long: see `drop_pending`.
static DROP_FORCED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Where the catcher was last armed (x << 32 | y), i64::MIN while disarmed.
static ARMED_AT: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(i64::MIN);
// Set while this machine is the CONTROLLED one handing its drag to a remote
// controller. The hand-off flag is meaningless then (only a controller's
// capture hook consumes it), so DragEnter must not leave one behind.
static RELAY_TO_CONTROLLER: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// True while the current button hold has already handed off one drag; the
/// capture hook then skips re-arming so no duplicate transfer happens.
pub fn hold_consumed() -> bool {
    HOLD_CONSUMED.load(std::sync::atomic::Ordering::Relaxed)
}

/// This hold's drag is over without having been caught: do not wait at the
/// edge for one.
pub fn consume_hold() {
    HOLD_CONSUMED.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// True while a drop the catcher asked for is on its way. Its button-up reads
/// as a released button everywhere, the capture hook included: crossing on the
/// very next movement hid the catcher before the drag had dropped on it, and
/// two drags in three were lost that way.
pub fn drop_pending() -> bool {
    DROP_FORCED.load(std::sync::atomic::Ordering::Relaxed)
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
    // A drop already asked for lands where the catcher is: it stays put.
    if drop_pending() {
        return;
    }
    // The hook arms on every mouse event of a button held against the edge
    // (130 in one second in a real log): only the first, or a new place, has
    // anything to move.
    let at = (i64::from(x) << 32) | i64::from(y as u32);
    let moved = ARMED_AT.swap(at, std::sync::atomic::Ordering::Relaxed) != at;
    if let Ok(mut slot) = catcher.device_id.lock() {
        if !moved && slot.as_deref() == Some(device_id) {
            return;
        }
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

/// A controller crossed back to its own screen mid-drag and sent a drag-control
/// "pull": hand it the file drag in flight on this (controlled) machine.
///
/// Same read-at-DragEnter path as the controller-side edge flow, with one
/// difference — the controller has stopped driving our cursor, so no further
/// mouse movement will ever reach the source app's modal `DoDragDrop` loop and
/// DragEnter would never fire. Parking the catcher under the cursor and nudging
/// the pointer (see `WM_ARM`) forces the one extra hit-test that lands the drag
/// on us.
pub fn handoff_to_controller(controller_device_id: &str) {
    // Every pull is a fresh handshake. `reset_hold` only ever runs in the
    // capture hook, which is not active on a controlled machine, so without
    // this the first hand-off would block every later one.
    reset_hold();
    let mut point = POINT::default();
    if unsafe { GetCursorPos(&mut point) }.is_err() {
        log::warn!("drag handoff: GetCursorPos failed");
        return;
    }
    // After the read: set before it, a failed read left the flag up with
    // nothing to take it down.
    RELAY_TO_CONTROLLER.store(true, std::sync::atomic::Ordering::Relaxed);
    log::info!("drag handoff: pull from {controller_device_id}, arming under the cursor");
    arm(controller_device_id, point.x, point.y);

    // A held button is not necessarily a file drag — it could be a window drag
    // or a text selection, in which case DragEnter never fires and the catcher
    // would sit parked on screen forever (nothing here retracts it: DragLeave
    // needs a drag, and no capture hook runs on a controlled machine). Retract
    // it once the nudge has had its chance.
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_millis(400));
        if RELAY_TO_CONTROLLER.swap(false, std::sync::atomic::Ordering::Relaxed) {
            log::info!("drag handoff: no file drag under the cursor, retracting");
            disarm();
        }
    });
}

/// Hide the catcher (control crossed, or the drag ended). Not while a drop it
/// asked for is on its way: that drop, or the wait for it running out, ends it.
pub fn disarm() {
    if !drop_pending() {
        retract();
    }
}

fn retract() {
    let Some(catcher) = catcher() else {
        return;
    };
    ARMED_AT.store(i64::MIN, std::sync::atomic::Ordering::Relaxed);
    DROP_FORCED.store(false, std::sync::atomic::Ordering::Relaxed);
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

        if crate::is_windows_process_elevated().unwrap_or(false) {
            // Not registered with OLE as well: it would be asked first, and
            // refused.
            DragAcceptFiles(hwnd, true);
            const WM_COPYGLOBALDATA: u32 = 0x0049;
            for message in [WM_DROPFILES, WM_COPYDATA, WM_COPYGLOBALDATA] {
                if let Err(error) = ChangeWindowMessageFilterEx(hwnd, message, MSGFLT_ALLOW, None) {
                    log::warn!("edge catcher: message {message:#06x} could not be let through: {error}");
                }
            }
            LEGACY_DROP.store(true, std::sync::atomic::Ordering::Relaxed);
        } else {
            let target: IDropTarget = EdgeDropTarget.into();
            if RegisterDragDrop(hwnd, &target).is_err() {
                log::warn!("edge catcher: RegisterDragDrop failed");
                return;
            }
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
            // Around the cursor itself, which is not where the hook says: its
            // point is where the cursor was heading, and the movement it
            // swallowed left the cursor short of that. A fast drag stopped
            // well inside the edge, beside a window centred on the edge.
            let mut at = POINT { x: (wparam.0 as u32) as i32, y: lparam.0 as i32 };
            unsafe {
                let _ = GetCursorPos(&mut at);
                let _ = SetWindowPos(
                    hwnd,
                    Some(HWND_TOPMOST),
                    at.x - CATCH_W / 2,
                    at.y - CATCH_H / 2,
                    CATCH_W,
                    CATCH_H,
                    SWP_NOACTIVATE,
                );
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            }
            // Only the controlled side needs a nudge: nothing is moving its
            // cursor, so without one OLE never hit-tests the window we just
            // showed and DragEnter can't fire. On the controller the user's own
            // motion does that, and injecting moves there would feed synthetic
            // events straight back into the capture hook.
            if LEGACY_DROP.load(std::sync::atomic::Ordering::Relaxed) {
                // Marked as pending before the button-up goes out, so the hook
                // never sees a released button without it.
                if !DROP_FORCED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    if force_drop(hwnd, at) {
                        unsafe { SetTimer(Some(hwnd), DROP_TIMER, DROP_WAIT_MS, None) };
                    } else {
                        DROP_FORCED.store(false, std::sync::atomic::Ordering::Relaxed);
                    }
                }
            } else if RELAY_TO_CONTROLLER.load(std::sync::atomic::Ordering::Relaxed) {
                nudge_cursor();
            }
            LRESULT(0)
        }
        WM_DROPFILES => {
            let hdrop = HDROP(wparam.0 as *mut _);
            let files = unsafe { hdrop_paths(hdrop) };
            unsafe {
                DragFinish(hdrop);
                let _ = KillTimer(Some(hwnd), DROP_TIMER);
            }
            // The drop that brought this already ended the drag.
            caught(files, false);
            // Let go of at the edge while the drop was on its way: the hook
            // has already seen that release, and no other will clear the hold.
            if !crate::input::windows_local_left_button_down() {
                reset_hold();
            }
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == DROP_TIMER => {
            unsafe {
                let _ = KillTimer(Some(hwnd), DROP_TIMER);
            }
            if drop_pending() {
                log::info!("edge catcher: the drag did not drop here (not files, or nothing was being dragged)");
                retract();
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
        caught(pdataobj.as_ref().map(read_hdrop_files).unwrap_or_default(), true);
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

/// A file drag reached the catcher. Send its files, end the local drag when
/// it is still going (`end_drag`), and hand the crossing to the capture hook.
fn caught(files: Vec<PathBuf>, end_drag: bool) {
    // One capture per button hold: shuttling back and forth across the edge
    // must not read + transfer the drag again (that made a copy each time).
    if HOLD_CONSUMED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        retract();
        return;
    }
    if files.is_empty() {
        HOLD_CONSUMED.store(false, std::sync::atomic::Ordering::Relaxed);
        log::info!("edge catcher: a drag with no files (not a file drag)");
        retract();
        return;
    }
    let device_id = catcher()
        .and_then(|catcher| catcher.device_id.lock().ok().and_then(|slot| slot.clone()));
    let Some(device_id) = device_id else {
        log::warn!("edge catcher: a drag arrived but no device is armed");
        retract();
        return;
    };
    log::info!(
        "edge catcher: read {} file(s) -> {}; transferring + handing off",
        files.len(),
        device_id
    );
    if let Some(sink) = SINK.get() {
        sink(device_id.clone(), files);
    }
    // End the source app's local drag (and clear the held button) before
    // the hand-off, so Windows doesn't come back stuck in a drag state.
    if end_drag {
        inject_end_drag();
    }
    // Tell the capture hook to cross so the cursor slides onto the remote.
    // Handing a drag UP to a controller is the opposite case: the cursor is
    // already back on that machine and no capture hook runs here to consume
    // the flag, so setting one would only strand a stale value.
    if RELAY_TO_CONTROLLER.swap(false, std::sync::atomic::Ordering::Relaxed) {
        log::info!("drag handoff: files sent up to the controller");
    } else if let Ok(mut slot) = handoff_slot().lock() {
        *slot = Some(device_id);
    }
    retract();
}

/// Make the drag in flight drop on the catcher now: the file-drop message is
/// the only word an elevated window gets of it. A move first, so the drag
/// looks at what is under the cursor; then the button-up that drops it there.
/// Only a drag: its source holds the mouse with OLE's own window, where a
/// selection, a slider or a window being moved holds it with theirs and would
/// only be let go of. And not unless the catcher is what the drop would land
/// on, or the files go to whatever else is there, to be moved or opened by it.
/// Returns whether the drop was asked for.
fn force_drop(hwnd: HWND, at: POINT) -> bool {
    let mut info = GUITHREADINFO { cbSize: std::mem::size_of::<GUITHREADINFO>() as u32, ..Default::default() };
    let dragging = unsafe {
        let thread = GetWindowThreadProcessId(GetForegroundWindow(), None);
        GetGUIThreadInfo(thread, &mut info).is_ok() && class_of(info.hwndCapture) == "CLIPBRDWNDCLASS"
    };
    if !dragging {
        return false;
    }
    let under = unsafe { WindowFromPoint(at) };
    if under != hwnd {
        log::warn!(
            "edge catcher: \"{}\" is under the cursor at ({},{}), not the catcher; the drag is left alone",
            class_of(under),
            at.x,
            at.y
        );
        return false;
    }
    log::info!("edge catcher: asking the drag to drop here, at ({},{})", at.x, at.y);
    let mut inputs = [move_input(-1, 0), move_input(1, 0), left_up_input()];
    unsafe {
        SendInput(inputs.len() as u32, inputs.as_mut_ptr(), std::mem::size_of::<INPUT>() as i32);
    }
    true
}

fn class_of(hwnd: HWND) -> String {
    let mut class = [0u16; 64];
    let len = unsafe { GetClassNameW(hwnd, &mut class) }.max(0) as usize;
    String::from_utf16_lossy(&class[..len])
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
    unsafe {
        let paths = hdrop_paths(HDROP(medium.u.hGlobal.0));
        ReleaseStgMedium(&mut medium);
        paths
    }
}

unsafe fn hdrop_paths(hdrop: HDROP) -> Vec<PathBuf> {
    let mut paths = Vec::new();
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

/// One pixel right then back. A relative move is what makes OLE's drag loop
/// hit-test again; the pair leaves the cursor where it was.
fn nudge_cursor() {
    let mut inputs = [move_input(1, 0), move_input(-1, 0)];
    unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_mut_ptr(),
            std::mem::size_of::<INPUT>() as i32,
        );
    }
}

fn move_input(dx: i32, dy: i32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: 0,
                dwFlags: MOUSEEVENTF_MOVE,
                time: 0,
                // Marked: the capture hook lets it through and does not take it
                // for the user's own (a real button-up there ends the hold).
                dwExtraInfo: crate::windows_drag::DRAG_INPUT_MARKER,
            },
        },
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
                // Marked: the capture hook lets it through and does not take it
                // for the user's own (a real button-up there ends the hold).
                dwExtraInfo: crate::windows_drag::DRAG_INPUT_MARKER,
            },
        },
    }
}
