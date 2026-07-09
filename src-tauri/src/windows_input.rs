#![cfg(target_os = "windows")]

use std::ptr;

use crate::shared_input::{mouse_button_mask, InputCommand, MouseButton};

pub fn inject_command(command: &InputCommand, pressed_keys: &mut Vec<u16>, button_mask: &mut u64) {
    if matches!(command, InputCommand::ReleaseAll) {
        release_pressed_inputs(pressed_keys, button_mask);
        return;
    }

    track_pressed_inputs(command, pressed_keys, button_mask);
    inject_command_without_tracking(command);
}

pub fn release_pressed_inputs_on_fresh_input_desktop(
    pressed_keys: &mut Vec<u16>,
    button_mask: &mut u64,
) -> Result<String, String> {
    let mut desktop = DesktopAttachment::new();
    let name = desktop.attach_current_input_desktop()?;
    release_pressed_inputs(pressed_keys, button_mask);
    Ok(name)
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

pub fn inject_command_without_tracking(command: &InputCommand) {
    match *command {
        InputCommand::MouseMove { x, y, .. } => inject_mouse_move(x, y, None),
        InputCommand::MouseButton { button, down, x, y } => inject_mouse_button(button, down, x, y),
        InputCommand::Scroll { delta_x, delta_y } => inject_scroll(delta_x, delta_y),
        InputCommand::Key { key_code, down } => inject_key(key_code, down),
        // Windows has no reliable background cursor-hide; just tuck it into the
        // corner, matching the previous park behaviour.
        InputCommand::CursorPark { x, y } => inject_mouse_move(x, y, None),
        InputCommand::ReleaseAll => {}
        InputCommand::SecureAttention => {
            let _ = send_secure_attention();
        }
    }
}

pub fn release_pressed_inputs(pressed_keys: &mut Vec<u16>, button_mask: &mut u64) {
    let keys = std::mem::take(pressed_keys);
    for key_code in keys.into_iter().rev() {
        inject_key(key_code, false);
    }

    for button in [MouseButton::Left, MouseButton::Right, MouseButton::Middle] {
        let mask = mouse_button_mask(button);
        if *button_mask & mask != 0 {
            inject_mouse_button(button, false, 0, 0);
        }
    }
    *button_mask = 0;
}

pub struct DesktopAttachment {
    desktop: windows_sys::Win32::System::StationsAndDesktops::HDESK,
    name: String,
}

impl DesktopAttachment {
    pub fn new() -> Self {
        Self {
            desktop: ptr::null_mut(),
            name: String::new(),
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
            self.name = name.clone();
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
                    dwExtraInfo: 0,
                },
            },
        };
        if SendInput(1, &input, std::mem::size_of::<INPUT>() as i32) == 0 {
            let _ = SetCursorPos(x, y);
        }
    }
}

pub fn inject_mouse_button(button: MouseButton, down: bool, x: i32, y: i32) {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
        MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
        MOUSEINPUT,
    };

    if x != 0 || y != 0 {
        inject_mouse_move(x, y, None);
    }

    // Use SendInput instead of the deprecated mouse_event wrapper: mouse_event
    // was silently dropping button events when called from the helper service's
    // spawned injection thread on some desktops, which produced the "cursor
    // moves but cannot click" symptom. SendInput reports failures via its
    // return value and is the recommended injection API.
    let flag = match (button, down) {
        (MouseButton::Left, true) => MOUSEEVENTF_LEFTDOWN,
        (MouseButton::Left, false) => MOUSEEVENTF_LEFTUP,
        (MouseButton::Right, true) => MOUSEEVENTF_RIGHTDOWN,
        (MouseButton::Right, false) => MOUSEEVENTF_RIGHTUP,
        (MouseButton::Middle, true) => MOUSEEVENTF_MIDDLEDOWN,
        (MouseButton::Middle, false) => MOUSEEVENTF_MIDDLEUP,
    };

    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: 0,
                dwFlags: flag,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    unsafe {
        let sent = SendInput(1, &input, std::mem::size_of::<INPUT>() as i32);
        if sent == 0 {
            let err = windows_sys::Win32::Foundation::GetLastError();
            std::fs::write(
                "C:\\ProgramData\\MyKVM\\helper-btn-err.txt",
                format!("mouse button {flag:?} error {err}\n"),
            )
            .ok();
        }
    }
}

pub fn inject_scroll(delta_x: i32, delta_y: i32) {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_WHEEL, MOUSEINPUT,
    };

    for (flag, delta) in [(MOUSEEVENTF_WHEEL, delta_y), (MOUSEEVENTF_HWHEEL, delta_x)] {
        if delta == 0 {
            continue;
        }

        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: 0,
                    dy: 0,
                    mouseData: (delta * 120) as u32,
                    dwFlags: flag,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };

        unsafe {
            let _ = SendInput(1, &input, std::mem::size_of::<INPUT>() as i32);
        }
    }
}

pub fn inject_key(key_code: u16, down: bool) {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        MapVirtualKeyW, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT,
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
                dwExtraInfo: 0,
            },
        },
    };
    unsafe {
        let sent = SendInput(1, &input, std::mem::size_of::<INPUT>() as i32);
        if sent == 0 {
            let err = windows_sys::Win32::Foundation::GetLastError();
            std::fs::write(
                "C:\\ProgramData\\MyKVM\\helper-key-err.txt",
                format!("key {key_code:#04x} down={down} error {err}\n"),
            )
            .ok();
        }
    }
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
