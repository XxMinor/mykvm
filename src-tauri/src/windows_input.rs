#![cfg(target_os = "windows")]

use std::{
    ptr,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

use crate::shared_input::{mouse_button_mask, InputCommand, MouseButton};

// Input injected by our own receiver/shortcut dispatcher remains local, even
// if the controller resumes sharing before a queued injection is delivered.
pub const KEYBOARD_INPUT_MARKER: usize = 0x4D59_4B53;

/// Stamped on the mouse input injected here for a peer. Under peer control
/// this machine captures its own mouse as well, and its hook must tell a peer
/// driving it from the user at its desk.
pub const PEER_MOUSE_MARKER: usize = 0x4D59_4B4D;

/// Explains a refused button/key injection, throttled to one line per 10s.
///
/// Windows blocks `SendInput` from a standard-user process into an elevated or
/// uiAccess foreground window (Task Manager — which ships `uiAccess="true"` — a
/// UAC-elevated app, etc.) with `ERROR_ACCESS_DENIED`; the events are dropped
/// silently, which reads to the user as "remote control just stopped" even
/// though MyKVM is still running. Cursor MOVE keeps working because it falls
/// back to SetCursorPos when SendInput is refused — so only clicks and keys
/// die, exactly the reported symptom. Surface it instead of failing mutely (this replaces a
/// leftover per-event debug write to C:\ProgramData\MyKVM\*.txt).
fn note_injection_refused(kind: &str, error: u32) {
    use windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED;

    static LAST_WARN: OnceLock<Mutex<Instant>> = OnceLock::new();
    let cell = LAST_WARN.get_or_init(|| Mutex::new(crate::instant_ago(Duration::from_secs(60))));
    if let Ok(mut last) = cell.lock() {
        if last.elapsed() < Duration::from_secs(10) {
            return;
        }
        *last = Instant::now();
    }

    if error == ERROR_ACCESS_DENIED || error == 0 {
        log::warn!(
            "injected {kind} refused by Windows (error {error}): the foreground window \
             may run at a higher integrity level (Task Manager, a UAC prompt, an elevated app). \
             The app retries such input through the MyKVM input service; if that service \
             is not installed, install it in Settings or run MyKVM as administrator."
        );
    } else {
        log::warn!("injected {kind} was refused: SendInput failed with error {error}");
    }
}

pub fn inject_command(command: &InputCommand, pressed_keys: &mut Vec<u16>, button_mask: &mut u64) {
    if matches!(command, InputCommand::ReleaseAll) {
        release_pressed_inputs(pressed_keys, button_mask);
        return;
    }

    track_pressed_inputs(command, pressed_keys, button_mask);
    let _ = inject_command_without_tracking(command);
}

pub fn track_pressed_inputs(
    command: &InputCommand,
    pressed_keys: &mut Vec<u16>,
    button_mask: &mut u64,
) {
    match *command {
        InputCommand::MouseButton { button, down, .. } => {
            if down {
                *button_mask |= mouse_button_mask(button);
            } else {
                *button_mask &= !mouse_button_mask(button);
            }
        }
        InputCommand::Key { key_code, down } => {
            if down {
                if !pressed_keys.contains(&key_code) {
                    pressed_keys.push(key_code);
                }
            } else {
                pressed_keys.retain(|pressed| *pressed != key_code);
            }
        }
        _ => {}
    }
}

/// Injects `command`; `Err` carries the Win32 error of a refused SendInput.
pub fn inject_command_without_tracking(command: &InputCommand) -> Result<(), u32> {
    match *command {
        InputCommand::MouseMove { x, y, .. } => {
            inject_mouse_move(x, y, None);
            Ok(())
        }
        InputCommand::MouseButton { button, down, x, y } => inject_mouse_button(button, down, x, y),
        InputCommand::Scroll { delta_x, delta_y, fine_x, fine_y } => inject_scroll(
            crate::shared_input::scroll_fine(delta_x, fine_x),
            crate::shared_input::scroll_fine(delta_y, fine_y),
        ),
        InputCommand::Key { key_code, down } => inject_key(key_code, down),
        InputCommand::ReleaseAll => Ok(()),
        InputCommand::SecureAttention => {
            let _ = send_secure_attention();
            Ok(())
        }
    }
}

pub fn release_pressed_inputs(pressed_keys: &mut Vec<u16>, button_mask: &mut u64) {
    let keys = std::mem::take(pressed_keys);
    for key_code in keys.into_iter().rev() {
        let _ = inject_key(key_code, false);
    }

    for button in [
        MouseButton::Left,
        MouseButton::Right,
        MouseButton::Middle,
        MouseButton::Back,
        MouseButton::Forward,
    ] {
        let mask = mouse_button_mask(button);
        if *button_mask & mask != 0 {
            let _ = inject_mouse_button(button, false, 0, 0);
        }
    }
    *button_mask = 0;
}

pub struct DesktopAttachment {
    desktop: windows_sys::Win32::System::StationsAndDesktops::HDESK,
}

impl DesktopAttachment {
    pub fn new() -> Self {
        Self {
            desktop: ptr::null_mut(),
        }
    }

    pub fn attach_current_input_desktop(&mut self) -> Result<String, String> {
        use windows_sys::Win32::System::StationsAndDesktops::{
            CloseDesktop, OpenInputDesktop, SetThreadDesktop, DESKTOP_CREATEWINDOW,
            DESKTOP_JOURNALPLAYBACK, DESKTOP_JOURNALRECORD, DESKTOP_READOBJECTS,
            DESKTOP_SWITCHDESKTOP, DESKTOP_WRITEOBJECTS,
        };

        unsafe {
            // DESKTOP_JOURNALPLAYBACK is REQUIRED for SendInput to be accepted on
            // the attached desktop: without it the worker's synthetic clicks/keys
            // are refused with ERROR_ACCESS_DENIED (only mouse-move, which uses a
            // different path, slips through). This is why the SYSTEM worker could
            // move the cursor on the lock screen but not click or type.
            let desktop = OpenInputDesktop(
                0,
                0,
                DESKTOP_READOBJECTS
                    | DESKTOP_WRITEOBJECTS
                    | DESKTOP_SWITCHDESKTOP
                    | DESKTOP_CREATEWINDOW
                    | DESKTOP_JOURNALPLAYBACK
                    | DESKTOP_JOURNALRECORD,
            );
            if desktop.is_null() {
                return Err("OpenInputDesktop failed".into());
            }

            let name = desktop_name(desktop).unwrap_or_else(|| "<unknown>".into());

            // Always re-attach to the freshly opened input desktop. Caching by
            // name is unsafe: a secure-desktop transition (e.g. clicking
            // "I forgot my PIN" / "Reset password" on the lock screen) switches
            // to a DIFFERENT desktop object that often carries the SAME name
            // ("Winlogon"). A name-equality cache would then skip SetThreadDesktop
            // and leave the worker bound to the old, now-inactive desktop, so
            // clicks/keys silently stop until the worker restarts. OpenInputDesktop
            // is already called every time here, so re-attaching is essentially
            // free.
            if SetThreadDesktop(desktop) == 0 {
                let _ = CloseDesktop(desktop);
                return Err(format!("SetThreadDesktop failed for {name}"));
            }

            if !self.desktop.is_null() {
                let _ = CloseDesktop(self.desktop);
            }
            self.desktop = desktop;
            return Ok(name);
        }
    }
}

unsafe fn desktop_name(
    desktop: windows_sys::Win32::System::StationsAndDesktops::HDESK,
) -> Option<String> {
    use windows_sys::Win32::System::StationsAndDesktops::{GetUserObjectInformationW, UOI_NAME};

    let mut needed = 0_u32;
    let mut buffer = [0_u16; 256];
    let ok = GetUserObjectInformationW(
        desktop as _,
        UOI_NAME,
        buffer.as_mut_ptr() as *mut _,
        (buffer.len() * std::mem::size_of::<u16>()) as u32,
        &mut needed,
    ) != 0;
    if !ok || needed == 0 {
        return None;
    }
    let len = buffer
        .iter()
        .position(|ch| *ch == 0)
        .unwrap_or(buffer.len());
    Some(String::from_utf16_lossy(&buffer[..len]))
}

impl Drop for DesktopAttachment {
    fn drop(&mut self) {
        if !self.desktop.is_null() {
            unsafe {
                let _ = windows_sys::Win32::System::StationsAndDesktops::CloseDesktop(self.desktop);
            }
        }
    }
}

pub fn inject_mouse_move(x: i32, y: i32, _drag_button: Option<MouseButton>) {
    use windows_sys::Win32::UI::{
        Input::KeyboardAndMouse::{
            SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_MOVE,
            MOUSEEVENTF_VIRTUALDESK, MOUSEINPUT,
        },
        WindowsAndMessaging::{
            GetSystemMetrics, SetCursorPos, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
            SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
        },
    };

    unsafe {
        let virtual_x = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let virtual_y = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let virtual_width = GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1);
        let virtual_height = GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1);
        let normalized_x =
            ((x - virtual_x) as i64 * 65_535 / (virtual_width - 1).max(1) as i64) as i32;
        let normalized_y =
            ((y - virtual_y) as i64 * 65_535 / (virtual_height - 1).max(1) as i64) as i32;
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: normalized_x.clamp(0, 65_535),
                    dy: normalized_y.clamp(0, 65_535),
                    mouseData: 0,
                    dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                    time: 0,
                    dwExtraInfo: PEER_MOUSE_MARKER,
                },
            },
        };
        if SendInput(1, &input, std::mem::size_of::<INPUT>() as i32) == 0 {
            let _ = SetCursorPos(x, y);
        }
    }
}

pub fn inject_mouse_button(button: MouseButton, down: bool, x: i32, y: i32) -> Result<(), u32> {
    use windows_sys::Win32::UI::{
        Input::KeyboardAndMouse::{
            INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
            MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
            MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT,
        },
        WindowsAndMessaging::{XBUTTON1, XBUTTON2},
    };

    if x != 0 || y != 0 {
        inject_mouse_move(x, y, None);
    }

    // Use SendInput instead of the deprecated mouse_event wrapper: mouse_event
    // was silently dropping button events when called from the helper service's
    // spawned injection thread on some desktops, which produced the "cursor
    // moves but cannot click" symptom. SendInput reports failures via its
    // return value and is the recommended injection API.
    //
    // The side buttons ride MOUSEEVENTF_X* with the button in mouseData
    // (XBUTTON1 = back, XBUTTON2 = forward).
    let (flag, mouse_data) = match (button, down) {
        (MouseButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
        (MouseButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
        (MouseButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
        (MouseButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
        (MouseButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
        (MouseButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
        (MouseButton::Back, true) => (MOUSEEVENTF_XDOWN, XBUTTON1 as i32),
        (MouseButton::Back, false) => (MOUSEEVENTF_XUP, XBUTTON1 as i32),
        (MouseButton::Forward, true) => (MOUSEEVENTF_XDOWN, XBUTTON2 as i32),
        (MouseButton::Forward, false) => (MOUSEEVENTF_XUP, XBUTTON2 as i32),
    };

    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: mouse_data as u32,
                dwFlags: flag,
                time: 0,
                dwExtraInfo: PEER_MOUSE_MARKER,
            },
        },
    };
    sent_or_refused(&input, "mouse button")
}

/// SendInput of one event; a refusal is noted and its error returned.
fn sent_or_refused(
    input: &windows_sys::Win32::UI::Input::KeyboardAndMouse::INPUT,
    kind: &str,
) -> Result<(), u32> {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{SendInput, INPUT};

    if unsafe { SendInput(1, input, std::mem::size_of::<INPUT>() as i32) } != 0 {
        return Ok(());
    }
    let error = unsafe { windows_sys::Win32::Foundation::GetLastError() };
    note_injection_refused(kind, error);
    Err(error)
}

/// Scrolls by `fine_*` 1/120ths of a notch, the unit Windows itself uses.
pub fn inject_scroll(fine_x: i32, fine_y: i32) -> Result<(), u32> {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_WHEEL, MOUSEINPUT,
    };

    for (flag, delta) in [(MOUSEEVENTF_WHEEL, fine_y), (MOUSEEVENTF_HWHEEL, fine_x)] {
        if delta == 0 {
            continue;
        }

        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: 0,
                    dy: 0,
                    // The wheel amount is a signed 16-bit value once it reaches a window.
                    mouseData: delta.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as u32,
                    dwFlags: flag,
                    time: 0,
                    dwExtraInfo: PEER_MOUSE_MARKER,
                },
            },
        };

        sent_or_refused(&input, "scroll")?;
    }
    Ok(())
}

pub fn inject_key(key_code: u16, down: bool) -> Result<(), u32> {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        MapVirtualKeyW, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT,
        KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, MAPVK_VK_TO_VSC,
    };

    let mut dw_flags = if down { 0 } else { KEYEVENTF_KEYUP };
    if is_extended_key_vk(key_code) {
        dw_flags |= KEYEVENTF_EXTENDEDKEY;
    }

    let scan = unsafe { MapVirtualKeyW(key_code as u32, MAPVK_VK_TO_VSC) } as u16;

    // Use SendInput instead of keybd_event: same reason as inject_mouse_button
    // — keybd_event was silently dropping key events from the helper's spawned
    // thread, leaving the keyboard dead while mouse moves still worked.
    let input = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: key_code,
                wScan: scan,
                dwFlags: dw_flags,
                time: 0,
                dwExtraInfo: KEYBOARD_INPUT_MARKER,
            },
        },
    };
    sent_or_refused(&input, "key")
}

fn is_extended_key_vk(vk: u16) -> bool {
    matches!(
        vk,
        0x21 | 0x22
            | 0x23
            | 0x24
            | 0x25
            | 0x26
            | 0x27
            | 0x28
            | 0x2C
            | 0x2D
            | 0x2E
            | 0x5B
            | 0x5C
            | 0x5D
            | 0x6F
            | 0x90
            | 0xA3
            | 0xA5
            // browser, volume and media keys (E0-prefixed on a keyboard)
            | 0xA6..=0xB7
    )
}

pub fn send_secure_attention() -> Result<(), String> {
    use windows_sys::Win32::{
        Foundation::FreeLibrary,
        System::LibraryLoader::{GetProcAddress, LoadLibraryW},
    };

    type SendSasFn = unsafe extern "system" fn(windows_sys::core::BOOL);

    unsafe {
        let dll = LoadLibraryW(crate::wide_null("sas.dll").as_ptr());
        if dll.is_null() {
            return Err("SAS.dll is not available on this Windows installation".into());
        }
        let Some(proc) = GetProcAddress(dll, c"SendSAS".as_ptr() as *const u8) else {
            let _ = FreeLibrary(dll);
            return Err("SendSAS entry point is not available".into());
        };
        let send_sas: SendSasFn = std::mem::transmute(proc);
        send_sas(0);
        let _ = FreeLibrary(dll);
    }
    Ok(())
}
