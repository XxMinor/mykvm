//! Linux input backend.
//!
//! The small environment/key-map model is compiled on every platform so its
//! behavior stays covered by the ordinary macOS test run. X11 I/O remains
//! target-gated below.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LinuxSession {
    X11,
    Wayland,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CapabilityState {
    Ready,
    Unsupported,
    Error,
}

fn detect_session(
    xdg_session_type: Option<&str>,
    wayland_display: Option<&str>,
    x11_display: Option<&str>,
) -> LinuxSession {
    let session = xdg_session_type
        .map(str::trim)
        .unwrap_or_default()
        .to_ascii_lowercase();
    if session == "wayland" || wayland_display.is_some_and(|display| !display.trim().is_empty()) {
        // A Wayland session normally exposes DISPLAY for XWayland as well. XTEST
        // only reaches XWayland clients, so falling through to X11 here would
        // advertise a backend that silently misses native applications.
        return LinuxSession::Wayland;
    }
    if session == "x11" || x11_display.is_some_and(|display| !display.trim().is_empty()) {
        return LinuxSession::X11;
    }
    LinuxSession::Unknown
}

#[cfg(target_os = "linux")]
pub(super) fn current_session() -> LinuxSession {
    detect_session(
        std::env::var("XDG_SESSION_TYPE").ok().as_deref(),
        std::env::var("WAYLAND_DISPLAY").ok().as_deref(),
        std::env::var("DISPLAY").ok().as_deref(),
    )
}

fn capability_state(
    session: LinuxSession,
    x11_probe: Result<(), impl std::fmt::Display>,
) -> CapabilityState {
    match session {
        LinuxSession::X11 if x11_probe.is_ok() => CapabilityState::Ready,
        LinuxSession::X11 => CapabilityState::Error,
        LinuxSession::Wayland | LinuxSession::Unknown => CapabilityState::Unsupported,
    }
}

#[cfg(any(target_os = "linux", test))]
fn end_x11_clipboard_session(
    target: &std::sync::Arc<std::sync::Mutex<Option<super::ClipboardTarget>>>,
) {
    super::clear_clipboard_target(target);
}

#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Default, PartialEq, Eq)]
struct ReceiveCursorState {
    parked: Option<(i32, i32)>,
}

#[cfg(any(target_os = "linux", test))]
impl ReceiveCursorState {
    fn park(&mut self, point: (i32, i32)) -> bool {
        let was_hidden = self.parked.is_some();
        self.parked = Some(point);
        !was_hidden
    }

    fn should_reveal_for_pointer(&self, point: (i32, i32), threshold: i32) -> bool {
        self.parked
            .map(|parked| {
                (point.0 - parked.0).abs() > threshold || (point.1 - parked.1).abs() > threshold
            })
            .unwrap_or(false)
    }

    fn reveal(&mut self) -> bool {
        self.parked.take().is_some()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct KeyMapEntry {
    xkb_name: [u8; 4],
    evdev: u16,
    vk: u16,
    keysym: u32,
}

macro_rules! key {
    ($name:literal, $evdev:expr, $vk:expr, $keysym:expr) => {
        KeyMapEntry {
            xkb_name: *$name,
            evdev: $evdev,
            vk: $vk,
            keysym: $keysym,
        }
    };
}

// XKB names describe physical keys and therefore keep shortcuts stable across
// AZERTY/Dvorak layouts. `keysym` is retained as a fallback for unusual XKB
// maps that omit names; the normal path never derives physical position from a
// localized symbol.
const KEY_MAP: &[KeyMapEntry] = &[
    key!(b"ESC\0", 1, 0x1B, 0xFF1B),
    key!(b"AE01", 2, 0x31, b'1' as u32),
    key!(b"AE02", 3, 0x32, b'2' as u32),
    key!(b"AE03", 4, 0x33, b'3' as u32),
    key!(b"AE04", 5, 0x34, b'4' as u32),
    key!(b"AE05", 6, 0x35, b'5' as u32),
    key!(b"AE06", 7, 0x36, b'6' as u32),
    key!(b"AE07", 8, 0x37, b'7' as u32),
    key!(b"AE08", 9, 0x38, b'8' as u32),
    key!(b"AE09", 10, 0x39, b'9' as u32),
    key!(b"AE10", 11, 0x30, b'0' as u32),
    key!(b"AE11", 12, 0xBD, b'-' as u32),
    key!(b"AE12", 13, 0xBB, b'=' as u32),
    key!(b"BKSP", 14, 0x08, 0xFF08),
    key!(b"TAB\0", 15, 0x09, 0xFF09),
    key!(b"AD01", 16, 0x51, b'q' as u32),
    key!(b"AD02", 17, 0x57, b'w' as u32),
    key!(b"AD03", 18, 0x45, b'e' as u32),
    key!(b"AD04", 19, 0x52, b'r' as u32),
    key!(b"AD05", 20, 0x54, b't' as u32),
    key!(b"AD06", 21, 0x59, b'y' as u32),
    key!(b"AD07", 22, 0x55, b'u' as u32),
    key!(b"AD08", 23, 0x49, b'i' as u32),
    key!(b"AD09", 24, 0x4F, b'o' as u32),
    key!(b"AD10", 25, 0x50, b'p' as u32),
    key!(b"AD11", 26, 0xDB, b'[' as u32),
    key!(b"AD12", 27, 0xDD, b']' as u32),
    key!(b"RTRN", 28, 0x0D, 0xFF0D),
    key!(b"LCTL", 29, 0xA2, 0xFFE3),
    key!(b"AC01", 30, 0x41, b'a' as u32),
    key!(b"AC02", 31, 0x53, b's' as u32),
    key!(b"AC03", 32, 0x44, b'd' as u32),
    key!(b"AC04", 33, 0x46, b'f' as u32),
    key!(b"AC05", 34, 0x47, b'g' as u32),
    key!(b"AC06", 35, 0x48, b'h' as u32),
    key!(b"AC07", 36, 0x4A, b'j' as u32),
    key!(b"AC08", 37, 0x4B, b'k' as u32),
    key!(b"AC09", 38, 0x4C, b'l' as u32),
    key!(b"AC10", 39, 0xBA, b';' as u32),
    key!(b"AC11", 40, 0xDE, b'\'' as u32),
    key!(b"TLDE", 41, 0xC0, b'`' as u32),
    key!(b"LFSH", 42, 0xA0, 0xFFE1),
    key!(b"BKSL", 43, 0xDC, b'\\' as u32),
    key!(b"AB01", 44, 0x5A, b'z' as u32),
    key!(b"AB02", 45, 0x58, b'x' as u32),
    key!(b"AB03", 46, 0x43, b'c' as u32),
    key!(b"AB04", 47, 0x56, b'v' as u32),
    key!(b"AB05", 48, 0x42, b'b' as u32),
    key!(b"AB06", 49, 0x4E, b'n' as u32),
    key!(b"AB07", 50, 0x4D, b'm' as u32),
    key!(b"AB08", 51, 0xBC, b',' as u32),
    key!(b"AB09", 52, 0xBE, b'.' as u32),
    key!(b"AB10", 53, 0xBF, b'/' as u32),
    key!(b"RTSH", 54, 0xA1, 0xFFE2),
    key!(b"KPMU", 55, 0x6A, 0xFFAA),
    key!(b"LALT", 56, 0xA4, 0xFFE9),
    key!(b"SPCE", 57, 0x20, b' ' as u32),
    key!(b"CAPS", 58, 0x14, 0xFFE5),
    key!(b"FK01", 59, 0x70, 0xFFBE),
    key!(b"FK02", 60, 0x71, 0xFFBF),
    key!(b"FK03", 61, 0x72, 0xFFC0),
    key!(b"FK04", 62, 0x73, 0xFFC1),
    key!(b"FK05", 63, 0x74, 0xFFC2),
    key!(b"FK06", 64, 0x75, 0xFFC3),
    key!(b"FK07", 65, 0x76, 0xFFC4),
    key!(b"FK08", 66, 0x77, 0xFFC5),
    key!(b"FK09", 67, 0x78, 0xFFC6),
    key!(b"FK10", 68, 0x79, 0xFFC7),
    key!(b"NMLK", 69, 0x90, 0xFF7F),
    key!(b"SCLK", 70, 0x91, 0xFF14),
    key!(b"KP7\0", 71, 0x67, 0xFFB7),
    key!(b"KP8\0", 72, 0x68, 0xFFB8),
    key!(b"KP9\0", 73, 0x69, 0xFFB9),
    key!(b"KPSU", 74, 0x6D, 0xFFAD),
    key!(b"KP4\0", 75, 0x64, 0xFFB4),
    key!(b"KP5\0", 76, 0x65, 0xFFB5),
    key!(b"KP6\0", 77, 0x66, 0xFFB6),
    key!(b"KPAD", 78, 0x6B, 0xFFAB),
    key!(b"KP1\0", 79, 0x61, 0xFFB1),
    key!(b"KP2\0", 80, 0x62, 0xFFB2),
    key!(b"KP3\0", 81, 0x63, 0xFFB3),
    key!(b"KP0\0", 82, 0x60, 0xFFB0),
    key!(b"KPDL", 83, 0x6E, 0xFFAE),
    key!(b"FK11", 87, 0x7A, 0xFFC8),
    key!(b"FK12", 88, 0x7B, 0xFFC9),
    key!(b"KPEN", 96, 0x0D, 0xFF8D),
    key!(b"RCTL", 97, 0xA3, 0xFFE4),
    key!(b"KPDV", 98, 0x6F, 0xFFAF),
    key!(b"PRSC", 99, 0x2C, 0xFF61),
    key!(b"RALT", 100, 0xA5, 0xFFEA),
    key!(b"HOME", 102, 0x24, 0xFF50),
    key!(b"UP\0\0", 103, 0x26, 0xFF52),
    key!(b"PGUP", 104, 0x21, 0xFF55),
    key!(b"LEFT", 105, 0x25, 0xFF51),
    key!(b"RGHT", 106, 0x27, 0xFF53),
    key!(b"END\0", 107, 0x23, 0xFF57),
    key!(b"DOWN", 108, 0x28, 0xFF54),
    key!(b"PGDN", 109, 0x22, 0xFF56),
    key!(b"INS\0", 110, 0x2D, 0xFF63),
    key!(b"DELE", 111, 0x2E, 0xFFFF),
    key!(b"MUTE", 113, 0xAD, 0x1008FF12),
    key!(b"VOL-", 114, 0xAE, 0x1008FF11),
    key!(b"VOL+", 115, 0xAF, 0x1008FF13),
    key!(b"PAUS", 119, 0x13, 0xFF13),
    key!(b"LWIN", 125, 0x5B, 0xFFEB),
    key!(b"RWIN", 126, 0x5C, 0xFFEC),
    key!(b"MENU", 127, 0x5D, 0xFF67),
    key!(b"I171", 163, 0xB0, 0x1008FF17),
    key!(b"I172", 164, 0xB3, 0x1008FF14),
    key!(b"I173", 165, 0xB1, 0x1008FF16),
    key!(b"I174", 166, 0xB2, 0x1008FF15),
    key!(b"FK13", 183, 0x7C, 0xFFCA),
    key!(b"FK14", 184, 0x7D, 0xFFCB),
    key!(b"FK15", 185, 0x7E, 0xFFCC),
    key!(b"FK16", 186, 0x7F, 0xFFCD),
    key!(b"FK17", 187, 0x80, 0xFFCE),
    key!(b"FK18", 188, 0x81, 0xFFCF),
    key!(b"FK19", 189, 0x82, 0xFFD0),
    key!(b"FK20", 190, 0x83, 0xFFD1),
    key!(b"FK21", 191, 0x84, 0xFFD2),
    key!(b"FK22", 192, 0x85, 0xFFD3),
    key!(b"FK23", 193, 0x86, 0xFFD4),
    key!(b"FK24", 194, 0x87, 0xFFD5),
];

#[cfg(test)]
fn key_entry_for_vk(vk: u16) -> Option<&'static KeyMapEntry> {
    KEY_MAP.iter().find(|entry| entry.vk == vk)
}

fn key_entry_for_xkb_name(name: [u8; 4]) -> Option<&'static KeyMapEntry> {
    KEY_MAP.iter().find(|entry| entry.xkb_name == name)
}

#[cfg(test)]
fn key_entry_for_evdev(evdev: u16) -> Option<&'static KeyMapEntry> {
    KEY_MAP.iter().find(|entry| entry.evdev == evdev)
}

fn key_entry_for_keysym(keysym: u32) -> Option<&'static KeyMapEntry> {
    KEY_MAP.iter().find(|entry| entry.keysym == keysym)
}

fn normalize_injected_modifier_vk(vk: u16) -> u16 {
    match vk {
        0x10 => 0xA0,
        0x11 => 0xA2,
        0x12 => 0xA4,
        _ => vk,
    }
}

#[cfg(target_os = "linux")]
use std::{
    collections::HashMap,
    ffi::{c_int, c_short},
    io,
    os::fd::{AsRawFd, RawFd},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc, Mutex, OnceLock,
    },
    thread,
    time::{Duration, Instant},
};

#[cfg(target_os = "linux")]
use x11rb::{
    connection::Connection,
    protocol::{
        xfixes::ConnectionExt as _,
        xinput::{self, ConnectionExt as _, Fp3232, XIEventMask},
        xkb::{self, ConnectionExt as _},
        xproto::{
            ConnectionExt as _, EventMask as CoreEventMask, GrabMode, GrabStatus,
            BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, KEY_PRESS_EVENT, KEY_RELEASE_EVENT,
            MOTION_NOTIFY_EVENT,
        },
        xtest::ConnectionExt as _,
        Event,
    },
    rust_connection::RustConnection,
    CURRENT_TIME, NONE,
};

#[cfg(target_os = "linux")]
use super::{
    active_target_input_failed, clear_clipboard_target, crossing_target, current_input_targets,
    local_anchor_point, local_hotkey_return_point, local_return_point, modifier_mask_for_key,
    modifier_mask_for_keys, modifier_snapshot_transitions, next_mouse_sequence,
    release_remote_buttons, remembered_local_screen_point, request_screen_switch_from_point,
    reset_mouse_move_timer, reset_remote_button_mask, screen_switch_hotkey_matches_vk,
    send_key_packet, send_packet, send_packet_with_modifier_snapshot, send_remote_cursor_park,
    send_remote_input_heartbeat, send_remote_mouse_move, send_remote_mouse_move_with_drag,
    set_control_clipboard_target, should_send_mouse_move, track_forwarded_key,
    update_active_remote_screen, update_remote_button_mask, ActiveTarget, ClipboardTarget,
    HotkeyModifiers, InputEvent, InputTarget, LayoutState, MouseButton, NativeStageStatus,
    SwitchDirection, SwitchOutcome,
};

#[cfg(target_os = "linux")]
use super::quic_transport;

#[cfg(target_os = "linux")]
const X11_CAPTURE_IDLE_WAIT: Duration = Duration::from_millis(10);

#[cfg(target_os = "linux")]
const UNKNOWN_KEY_LOG_WINDOW: Duration = Duration::from_secs(30);

#[cfg(target_os = "linux")]
const UNKNOWN_KEY_LOG_BURST: u8 = 4;

#[cfg(target_os = "linux")]
#[repr(C)]
struct PollFd {
    fd: c_int,
    events: c_short,
    revents: c_short,
}

#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn poll(fds: *mut PollFd, nfds: usize, timeout: c_int) -> c_int;
}

#[cfg(target_os = "linux")]
fn poll_timeout_ms(timeout: Duration) -> c_int {
    if timeout.is_zero() {
        return 0;
    }
    let rounded_up = timeout
        .as_millis()
        .saturating_add(u128::from(timeout.subsec_nanos() % 1_000_000 != 0));
    rounded_up.min(c_int::MAX as u128) as c_int
}

#[cfg(target_os = "linux")]
fn wait_for_fd_activity(fd: RawFd, timeout: Duration) -> io::Result<bool> {
    const POLLIN: c_short = 0x0001;
    const POLLERR: c_short = 0x0008;
    const POLLHUP: c_short = 0x0010;
    const POLLNVAL: c_short = 0x0020;

    let deadline = Instant::now() + timeout;
    loop {
        let mut descriptor = PollFd {
            fd,
            events: POLLIN,
            revents: 0,
        };
        let remaining = deadline.saturating_duration_since(Instant::now());
        // SAFETY: `descriptor` is a live pollfd for the duration of the call,
        // and the array length passed to libc matches the one element.
        let result = unsafe { poll(&mut descriptor, 1, poll_timeout_ms(remaining)) };
        if result > 0 {
            return Ok(descriptor.revents & (POLLIN | POLLERR | POLLHUP | POLLNVAL) != 0);
        }
        if result == 0 {
            return Ok(false);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
    }
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
struct DiagnosticLogThrottle {
    window_started: Instant,
    emitted: u8,
    suppressed: u64,
}

#[cfg(target_os = "linux")]
impl DiagnosticLogThrottle {
    fn new(now: Instant) -> Self {
        Self {
            window_started: now,
            emitted: 0,
            suppressed: 0,
        }
    }

    /// `Some(n)` permits one log entry and reports how many messages the
    /// previous window suppressed. `None` suppresses this entry.
    fn decision(&mut self, now: Instant) -> Option<u64> {
        let previous_suppressed =
            if now.duration_since(self.window_started) >= UNKNOWN_KEY_LOG_WINDOW {
                let suppressed = self.suppressed;
                self.window_started = now;
                self.emitted = 0;
                self.suppressed = 0;
                suppressed
            } else {
                0
            };
        if self.emitted < UNKNOWN_KEY_LOG_BURST {
            self.emitted += 1;
            Some(previous_suppressed)
        } else {
            self.suppressed = self.suppressed.saturating_add(1);
            None
        }
    }
}

#[cfg(target_os = "linux")]
fn diagnostic_log_decision(state: &'static OnceLock<Mutex<DiagnosticLogThrottle>>) -> Option<u64> {
    state
        .get_or_init(|| Mutex::new(DiagnosticLogThrottle::new(Instant::now())))
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .decision(Instant::now())
}

#[cfg(target_os = "linux")]
fn unknown_injected_key_log_decision() -> Option<u64> {
    static STATE: OnceLock<Mutex<DiagnosticLogThrottle>> = OnceLock::new();
    diagnostic_log_decision(&STATE)
}

#[cfg(target_os = "linux")]
fn unknown_captured_key_log_decision() -> Option<u64> {
    static STATE: OnceLock<Mutex<DiagnosticLogThrottle>> = OnceLock::new();
    diagnostic_log_decision(&STATE)
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Default)]
struct X11KeyMap {
    keycode_to_vk: HashMap<u8, u16>,
    vk_to_keycode: HashMap<u16, u8>,
}

#[cfg(target_os = "linux")]
impl X11KeyMap {
    fn from_xkb_names(first_keycode: u8, names: impl IntoIterator<Item = [u8; 4]>) -> Self {
        let mut result = Self::default();
        for (offset, name) in names.into_iter().enumerate() {
            let Some(keycode) = usize::from(first_keycode)
                .checked_add(offset)
                .and_then(|value| u8::try_from(value).ok())
            else {
                break;
            };
            let Some(entry) = key_entry_for_xkb_name(name) else {
                continue;
            };
            result.keycode_to_vk.insert(keycode, entry.vk);
            result.vk_to_keycode.entry(entry.vk).or_insert(keycode);
        }
        result
    }

    fn add_standard_evdev_fallbacks(&mut self) {
        // The standard Xorg evdev keycode is Linux input code + 8. XKB names
        // remain authoritative; this fills only keys omitted by a minimal or
        // unusual server key-name table (including common Xvfb setups).
        for entry in KEY_MAP {
            let Some(keycode) = entry
                .evdev
                .checked_add(8)
                .and_then(|value| u8::try_from(value).ok())
            else {
                continue;
            };
            self.keycode_to_vk.entry(keycode).or_insert(entry.vk);
            self.vk_to_keycode.entry(entry.vk).or_insert(keycode);
        }
    }

    fn add_keysym_fallbacks(
        &mut self,
        first_keycode: u8,
        keysyms_per_keycode: u8,
        keysyms: &[u32],
    ) {
        let stride = usize::from(keysyms_per_keycode);
        if stride == 0 {
            return;
        }
        for (offset, symbols) in keysyms.chunks(stride).enumerate() {
            let Some(keycode) = usize::from(first_keycode)
                .checked_add(offset)
                .and_then(|value| u8::try_from(value).ok())
            else {
                break;
            };
            if self.keycode_to_vk.contains_key(&keycode) {
                continue;
            }
            let Some(entry) = symbols
                .iter()
                .find_map(|keysym| key_entry_for_keysym(*keysym))
            else {
                continue;
            };
            self.keycode_to_vk.insert(keycode, entry.vk);
            self.vk_to_keycode.entry(entry.vk).or_insert(keycode);
        }
    }

    fn vk_for_keycode(&self, keycode: u8) -> Option<u16> {
        self.keycode_to_vk.get(&keycode).copied()
    }

    fn keycode_for_vk(&self, vk: u16) -> Option<u8> {
        self.vk_to_keycode.get(&vk).copied()
    }
}

#[cfg(target_os = "linux")]
fn x11_root(connection: &RustConnection, screen_number: usize) -> Result<u32, String> {
    connection
        .setup()
        .roots
        .get(screen_number)
        .map(|screen| screen.root)
        .ok_or_else(|| format!("X11 screen {screen_number} is missing"))
}

#[cfg(target_os = "linux")]
fn connect_x11() -> Result<(RustConnection, usize, u32), String> {
    const ATTEMPTS: usize = 3;
    let mut last_error = String::new();
    for attempt in 0..ATTEMPTS {
        match x11rb::connect(None) {
            Ok((connection, screen_number)) => {
                let root = x11_root(&connection, screen_number)?;
                return Ok((connection, screen_number, root));
            }
            Err(error) => last_error = error.to_string(),
        }
        if attempt + 1 < ATTEMPTS {
            // Xvfb and a freshly restarted login X server can accept one
            // client and briefly reset the next while their extensions finish
            // initializing. Keep startup bounded while tolerating that race.
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
    Err(format!("无法连接 X11 DISPLAY：{last_error}"))
}

#[cfg(target_os = "linux")]
fn load_x11_keymap(connection: &RustConnection) -> Result<X11KeyMap, String> {
    let xkb_version = connection
        .xkb_use_extension(1, 0)
        .map_err(|error| format!("XKB 初始化失败：{error}"))?
        .reply()
        .map_err(|error| format!("XKB 初始化失败：{error}"))?;
    if !xkb_version.supported {
        return Err("X11 服务器不支持 XKB 1.0，无法可靠映射键盘".into());
    }

    let names = connection
        .xkb_get_names(u16::from(xkb::ID::USE_CORE_KBD), xkb::NameDetail::KEY_NAMES)
        .map_err(|error| format!("读取 XKB 键名失败：{error}"))?
        .reply()
        .map_err(|error| format!("读取 XKB 键名失败：{error}"))?;
    let mut keymap = X11KeyMap::from_xkb_names(
        names.first_key,
        names
            .value_list
            .key_names
            .unwrap_or_default()
            .into_iter()
            .map(|name| name.name),
    );

    let setup = connection.setup();
    let first_keycode = setup.min_keycode;
    let count = setup
        .max_keycode
        .saturating_sub(setup.min_keycode)
        .saturating_add(1);
    if count > 0 {
        if let Ok(cookie) = connection.get_keyboard_mapping(first_keycode, count) {
            if let Ok(reply) = cookie.reply() {
                keymap.add_keysym_fallbacks(
                    first_keycode,
                    reply.keysyms_per_keycode,
                    &reply.keysyms,
                );
            }
        }
    }
    keymap.add_standard_evdev_fallbacks();

    for vk in [0x41_u16, 0xA2, 0x25, 0x26] {
        if keymap.keycode_for_vk(vk).is_none() {
            return Err(format!(
                "XKB 键盘映射不完整（缺少 Windows VK {vk:#04x} 对应键）"
            ));
        }
    }
    Ok(keymap)
}

#[cfg(target_os = "linux")]
fn check_xtest(connection: &RustConnection) -> Result<(), String> {
    connection
        .xtest_get_version(2, 2)
        .map_err(|error| format!("XTEST 初始化失败：{error}"))?
        .reply()
        .map_err(|error| format!("XTEST 扩展不可用：{error}"))?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn check_capture_extensions(connection: &RustConnection) -> Result<(), String> {
    let xi = connection
        .xinput_xi_query_version(2, 0)
        .map_err(|error| format!("XI2 初始化失败：{error}"))?
        .reply()
        .map_err(|error| format!("XI2 扩展不可用：{error}"))?;
    if xi.major_version < 2 {
        return Err(format!(
            "X11 服务器的 XI2 版本过旧（{}.{}）",
            xi.major_version, xi.minor_version
        ));
    }
    check_xfixes(connection)
}

#[cfg(target_os = "linux")]
fn check_xfixes(connection: &RustConnection) -> Result<(), String> {
    connection
        .xfixes_query_version(5, 0)
        .map_err(|error| format!("XFixes 初始化失败：{error}"))?
        .reply()
        .map_err(|error| format!("XFixes 扩展不可用：{error}"))?;
    Ok(())
}

#[cfg(target_os = "linux")]
struct X11Injector {
    connection: RustConnection,
    root: u32,
    keymap: X11KeyMap,
    pressed_keys: Vec<u16>,
    pressed_buttons: Vec<u8>,
}

#[cfg(target_os = "linux")]
impl X11Injector {
    fn connect() -> Result<Self, String> {
        let (connection, _, root) = connect_x11()?;
        check_xtest(&connection)?;
        check_xfixes(&connection)?;
        let keymap = load_x11_keymap(&connection)?;
        Ok(Self {
            connection,
            root,
            keymap,
            pressed_keys: Vec::new(),
            pressed_buttons: Vec::new(),
        })
    }

    fn fake_input(&self, event_type: u8, detail: u8, x: i32, y: i32) -> Result<(), String> {
        self.connection
            .xtest_fake_input(
                event_type,
                detail,
                CURRENT_TIME,
                self.root,
                clamp_x11_coordinate(x),
                clamp_x11_coordinate(y),
                0,
            )
            .map_err(|error| format!("XTEST 注入失败：{error}"))?;
        Ok(())
    }

    fn flush(&self) -> Result<(), String> {
        self.connection
            .flush()
            .map_err(|error| format!("X11 刷新失败：{error}"))
    }

    fn mouse_move(&self, x: i32, y: i32) -> Result<(), String> {
        self.fake_input(MOTION_NOTIFY_EVENT, 0, x, y)?;
        self.flush()
    }

    fn mouse_button(
        &mut self,
        button: MouseButton,
        down: bool,
        x: i32,
        y: i32,
    ) -> Result<(), String> {
        let detail = match button {
            MouseButton::Left => 1,
            MouseButton::Middle => 2,
            MouseButton::Right => 3,
        };
        self.fake_input(MOTION_NOTIFY_EVENT, 0, x, y)?;
        self.fake_input(
            if down {
                BUTTON_PRESS_EVENT
            } else {
                BUTTON_RELEASE_EVENT
            },
            detail,
            x,
            y,
        )?;
        if down {
            if !self.pressed_buttons.contains(&detail) {
                self.pressed_buttons.push(detail);
            }
        } else {
            self.pressed_buttons.retain(|pressed| *pressed != detail);
        }
        self.flush()
    }

    fn scroll(&self, delta_x: i32, delta_y: i32) -> Result<(), String> {
        // X11 represents wheel ticks as button press/release pairs. Bound the
        // loop so a corrupt peer cannot monopolise the receive worker.
        for (detail, ticks) in [
            (if delta_y >= 0 { 4 } else { 5 }, delta_y.unsigned_abs()),
            (if delta_x >= 0 { 7 } else { 6 }, delta_x.unsigned_abs()),
        ] {
            for _ in 0..ticks.min(32) {
                self.fake_input(BUTTON_PRESS_EVENT, detail, 0, 0)?;
                self.fake_input(BUTTON_RELEASE_EVENT, detail, 0, 0)?;
            }
        }
        self.flush()
    }

    fn key(&mut self, vk: u16, down: bool) -> Result<(), String> {
        let vk = normalize_injected_modifier_vk(vk);
        let Some(keycode) = self.keymap.keycode_for_vk(vk) else {
            if let Some(suppressed) = unknown_injected_key_log_decision() {
                log::warn!(
                    "Linux X11 injection ignored unmapped Windows VK {vk:#04x}{}",
                    if suppressed == 0 {
                        String::new()
                    } else {
                        format!(" ({suppressed} similar diagnostics suppressed)")
                    }
                );
            }
            return Ok(());
        };
        self.fake_input(
            if down {
                KEY_PRESS_EVENT
            } else {
                KEY_RELEASE_EVENT
            },
            keycode,
            0,
            0,
        )?;
        if down {
            if !self.pressed_keys.contains(&vk) {
                self.pressed_keys.push(vk);
            }
        } else {
            self.pressed_keys.retain(|pressed| *pressed != vk);
        }
        self.flush()
    }

    fn reconcile_modifier_snapshot(&mut self, mask: u8) -> Result<(), String> {
        let transitions = modifier_snapshot_transitions(&self.pressed_keys, mask);
        if !transitions.is_empty() {
            log::info!(
                "reconciled remote Linux modifiers from snapshot mask={mask:#04x}: {transitions:?}"
            );
        }
        for (vk, down) in transitions {
            self.key(vk, down)?;
        }
        Ok(())
    }

    fn release_keys(&mut self) -> Result<(), String> {
        for vk in std::mem::take(&mut self.pressed_keys) {
            if let Some(keycode) = self.keymap.keycode_for_vk(vk) {
                self.fake_input(KEY_RELEASE_EVENT, keycode, 0, 0)?;
            }
        }
        self.flush()
    }

    fn release_all(&mut self) -> Result<(), String> {
        self.release_keys()?;
        for detail in std::mem::take(&mut self.pressed_buttons) {
            self.fake_input(BUTTON_RELEASE_EVENT, detail, 0, 0)?;
        }
        self.flush()
    }
}

#[cfg(target_os = "linux")]
fn clamp_x11_coordinate(value: i32) -> i16 {
    value.clamp(i16::MIN as i32, i16::MAX as i32) as i16
}

#[cfg(target_os = "linux")]
fn x11_injector() -> &'static Mutex<Option<X11Injector>> {
    static INJECTOR: OnceLock<Mutex<Option<X11Injector>>> = OnceLock::new();
    INJECTOR.get_or_init(|| Mutex::new(None))
}

#[cfg(target_os = "linux")]
fn with_x11_injector<T>(
    operation: impl FnOnce(&mut X11Injector) -> Result<T, String>,
) -> Result<T, String> {
    let mut state = x11_injector()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if state.is_none() {
        *state = Some(X11Injector::connect()?);
    }
    let result = operation(state.as_mut().expect("injector was initialized"));
    if result.is_err() {
        // An X11 connection is not recoverable after an I/O failure. Drop it
        // so the next packet can reconnect after an X server restart/login.
        *state = None;
    }
    result
}

#[cfg(target_os = "linux")]
pub(super) fn receive_status(port: u16) -> NativeStageStatus {
    match current_session() {
        LinuxSession::Wayland => wayland_unsupported_status("接收/注入"),
        LinuxSession::Unknown => unknown_session_status("接收/注入"),
        LinuxSession::X11 => match with_x11_injector(|_| Ok(())) {
            Ok(()) => NativeStageStatus {
                state: "ready".into(),
                detail: format!(
                    "Linux X11 接收端已就绪：XTEST 键鼠注入已初始化，QUIC UDP {port}。"
                ),
            },
            Err(error) => NativeStageStatus {
                state: "error".into(),
                detail: format!("Linux X11 接收端不可用：{error}"),
            },
        },
    }
}

#[cfg(target_os = "linux")]
pub(super) fn capture_status(target_count: usize) -> NativeStageStatus {
    match current_session() {
        LinuxSession::Wayland => wayland_unsupported_status("控制/捕获"),
        LinuxSession::Unknown => unknown_session_status("控制/捕获"),
        LinuxSession::X11 => {
            let probe = (|| {
                let (connection, _, _) = connect_x11()?;
                check_capture_extensions(&connection)?;
                load_x11_keymap(&connection)?;
                Ok::<(), String>(())
            })();
            match capability_state(
                LinuxSession::X11,
                probe.as_ref().map(|_| ()).map_err(|e| e.as_str()),
            ) {
                CapabilityState::Ready => NativeStageStatus {
                    state: "ready".into(),
                    detail: format!(
                        "Linux X11 控制端可用，{target_count} 条远端贴边可用于鼠标和键盘切换。"
                    ),
                },
                CapabilityState::Error => NativeStageStatus {
                    state: "error".into(),
                    detail: format!(
                        "Linux X11 控制端不可用：{}",
                        probe.err().unwrap_or_else(|| "未知错误".into())
                    ),
                },
                CapabilityState::Unsupported => unreachable!(),
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn wayland_unsupported_status(capability: &str) -> NativeStageStatus {
    NativeStageStatus {
        state: "error".into(),
        detail: format!(
            "Linux Wayland 当前不支持全局键鼠{capability}。请在登录界面选择 Xorg/X11 会话后重启 MyKVM；原生 Wayland 后续需要 xdg-desktop-portal InputCapture/RemoteDesktop 的用户授权。"
        ),
    }
}

#[cfg(target_os = "linux")]
fn unknown_session_status(capability: &str) -> NativeStageStatus {
    NativeStageStatus {
        state: "error".into(),
        detail: format!(
            "未检测到可用的 Linux 图形会话，无法进行全局键鼠{capability}。请确认在 Xorg/X11 桌面内启动，且 DISPLAY 环境变量可用。"
        ),
    }
}

#[cfg(target_os = "linux")]
pub(super) fn inject_mouse_move(x: i32, y: i32) {
    if let Err(error) = with_x11_injector(|injector| injector.mouse_move(x, y)) {
        log::warn!("Linux mouse move injection failed: {error}");
    }
}

#[cfg(target_os = "linux")]
pub(super) fn inject_mouse_button(button: MouseButton, down: bool, x: i32, y: i32) {
    if let Err(error) = with_x11_injector(|injector| injector.mouse_button(button, down, x, y)) {
        log::warn!("Linux mouse button injection failed: {error}");
    }
}

#[cfg(target_os = "linux")]
pub(super) fn inject_scroll(delta_x: i32, delta_y: i32) {
    if let Err(error) = with_x11_injector(|injector| injector.scroll(delta_x, delta_y)) {
        log::warn!("Linux scroll injection failed: {error}");
    }
}

#[cfg(target_os = "linux")]
pub(super) fn inject_key(vk: u16, down: bool) {
    if let Err(error) = with_x11_injector(|injector| injector.key(vk, down)) {
        log::warn!("Linux key injection failed for VK {vk:#04x}: {error}");
    }
}

#[cfg(target_os = "linux")]
pub(super) fn inject_key_with_modifier_snapshot(vk: u16, down: bool, mask: Option<u8>) {
    if let Err(error) = with_x11_injector(|injector| {
        if modifier_mask_for_key(vk).is_some() {
            injector.key(vk, down)?;
            if let Some(mask) = mask {
                injector.reconcile_modifier_snapshot(mask)?;
            }
            return Ok(());
        }
        if let Some(mask) = mask {
            injector.reconcile_modifier_snapshot(mask)?;
        }
        injector.key(vk, down)
    }) {
        log::warn!("Linux key injection failed for VK {vk:#04x}: {error}");
    }
}

#[cfg(target_os = "linux")]
pub(super) fn reconcile_injected_modifier_snapshot(mask: u8) {
    if let Err(error) = with_x11_injector(|injector| injector.reconcile_modifier_snapshot(mask)) {
        log::warn!("Linux modifier snapshot reconciliation failed: {error}");
    }
}

#[cfg(target_os = "linux")]
pub(super) fn reset_injected_keys() {
    let mut state = x11_injector()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if let Some(injector) = state.as_mut() {
        if let Err(error) = injector.release_keys() {
            log::warn!("Linux injected key release failed: {error}");
            *state = None;
        }
    }
}

#[cfg(target_os = "linux")]
pub(super) fn reset_injected_inputs() {
    let mut state = x11_injector()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if let Some(injector) = state.as_mut() {
        if let Err(error) = injector.release_all() {
            log::warn!("Linux injected input release failed: {error}");
            *state = None;
        }
    }
}

#[cfg(target_os = "linux")]
fn receive_cursor_state() -> &'static Mutex<ReceiveCursorState> {
    static STATE: OnceLock<Mutex<ReceiveCursorState>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(ReceiveCursorState::default()))
}

#[cfg(target_os = "linux")]
pub(super) fn receive_hide_cursor(x: i32, y: i32) {
    let mut state = receive_cursor_state()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let first_hide = state.park((x, y));
    let result = with_x11_injector(|injector| {
        injector.mouse_move(x, y)?;
        if first_hide {
            injector
                .connection
                .xfixes_hide_cursor(injector.root)
                .map_err(|error| format!("隐藏 Linux 接收端指针失败：{error}"))?
                .check()
                .map_err(|error| format!("隐藏 Linux 接收端指针失败：{error}"))?;
            injector.flush()?;
        }
        Ok(())
    });
    if let Err(error) = result {
        state.reveal();
        log::warn!("Linux receive cursor park failed: {error}");
    }
}

#[cfg(target_os = "linux")]
pub(super) fn receive_show_cursor_if_hidden() {
    let mut state = receive_cursor_state()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if !state.reveal() {
        return;
    }
    if let Err(error) = with_x11_injector(|injector| {
        injector
            .connection
            .xfixes_show_cursor(injector.root)
            .map_err(|error| format!("显示 Linux 接收端指针失败：{error}"))?
            .check()
            .map_err(|error| format!("显示 Linux 接收端指针失败：{error}"))?;
        injector.flush()
    }) {
        log::warn!("Linux receive cursor reveal failed: {error}");
    }
}

#[cfg(target_os = "linux")]
pub(super) fn start_receive_monitor(stop: Arc<AtomicBool>) {
    thread::spawn(move || {
        const DRIFT_THRESHOLD_PX: i32 = 3;
        while !stop.load(Ordering::Relaxed) {
            let parked = receive_cursor_state()
                .lock()
                .ok()
                .and_then(|state| state.parked);
            if parked.is_some() {
                let current = with_x11_injector(|injector| {
                    query_pointer_point(&injector.connection, injector.root)
                        .map(|(x, y)| (x.round() as i32, y.round() as i32))
                });
                if let Ok(current) = current {
                    let should_reveal = receive_cursor_state()
                        .lock()
                        .map(|state| state.should_reveal_for_pointer(current, DRIFT_THRESHOLD_PX))
                        .unwrap_or(false);
                    if should_reveal {
                        receive_show_cursor_if_hidden();
                    }
                }
            }
            thread::sleep(Duration::from_millis(50));
        }
        receive_show_cursor_if_hidden();
    });
}

#[cfg(target_os = "linux")]
struct X11CaptureContext {
    connection: Arc<RustConnection>,
    root: u32,
    keymap: X11KeyMap,
    quic_transport: quic_transport::TransportHandle,
    layout_state: Arc<Mutex<LayoutState>>,
    native_layout: LayoutState,
    stop: Arc<AtomicBool>,
    send_gate: Mutex<()>,
    active: Mutex<Option<ActiveTarget>>,
    remote_active: Arc<AtomicBool>,
    clipboard_target: Arc<Mutex<Option<ClipboardTarget>>>,
    input_events: Arc<AtomicU64>,
    switch_request: Arc<Mutex<Option<SwitchDirection>>>,
    anchor: Mutex<Option<(f64, f64)>>,
    hotkey_return_point: Mutex<Option<(f64, f64)>>,
    last_local_point: Mutex<Option<(f64, f64)>>,
    last_pointer_sync: Mutex<Instant>,
    last_mouse_move_sent: Mutex<Option<Instant>>,
    last_heartbeat_sent: Mutex<Option<Instant>>,
    remote_button_mask: AtomicU64,
    physical_pressed_keys: Mutex<Vec<u16>>,
    forwarded_pressed_keys: Mutex<Vec<u16>>,
    local_screen_points: Mutex<HashMap<String, (f64, f64)>>,
    grabbed: AtomicBool,
    cursor_hidden: AtomicBool,
}

#[cfg(target_os = "linux")]
fn x11_capture_slot() -> &'static Mutex<Option<Arc<X11CaptureContext>>> {
    static CONTEXT: OnceLock<Mutex<Option<Arc<X11CaptureContext>>>> = OnceLock::new();
    CONTEXT.get_or_init(|| Mutex::new(None))
}

#[cfg(target_os = "linux")]
fn x11_capture_context() -> Option<Arc<X11CaptureContext>> {
    x11_capture_slot()
        .lock()
        .ok()
        .and_then(|context| context.clone())
}

#[cfg(target_os = "linux")]
fn clear_x11_capture_context(expected: &Arc<X11CaptureContext>) {
    if let Ok(mut context) = x11_capture_slot().lock() {
        if context
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, expected))
        {
            *context = None;
        }
    }
}

#[cfg(target_os = "linux")]
fn connect_x11_capture() -> Result<(Arc<RustConnection>, u32, X11KeyMap), String> {
    let (connection, _, root) = connect_x11()?;
    check_capture_extensions(&connection)?;
    let keymap = load_x11_keymap(&connection)?;
    let mask = XIEventMask::RAW_KEY_PRESS
        | XIEventMask::RAW_KEY_RELEASE
        | XIEventMask::RAW_BUTTON_PRESS
        | XIEventMask::RAW_BUTTON_RELEASE
        | XIEventMask::RAW_MOTION;
    connection
        .xinput_xi_select_events(
            root,
            &[xinput::EventMask {
                deviceid: u16::from(xinput::Device::ALL_MASTER),
                mask: vec![mask],
            }],
        )
        .map_err(|error| format!("订阅 XI2 原始输入失败：{error}"))?
        .check()
        .map_err(|error| format!("订阅 XI2 原始输入失败：{error}"))?;
    connection
        .flush()
        .map_err(|error| format!("刷新 X11 捕获连接失败：{error}"))?;
    Ok((Arc::new(connection), root, keymap))
}

#[cfg(target_os = "linux")]
pub(super) fn start_capture(
    targets: Vec<InputTarget>,
    layout_state: Arc<Mutex<LayoutState>>,
    native_layout: LayoutState,
    quic_transport: quic_transport::TransportHandle,
    stop: Arc<AtomicBool>,
    remote_active: Arc<AtomicBool>,
    clipboard_target: Arc<Mutex<Option<ClipboardTarget>>>,
    input_events: Arc<AtomicU64>,
    switch_request: Arc<Mutex<Option<SwitchDirection>>>,
) -> NativeStageStatus {
    match current_session() {
        LinuxSession::Wayland => {
            remote_active.store(false, Ordering::Relaxed);
            clear_clipboard_target(&clipboard_target);
            return wayland_unsupported_status("控制/捕获");
        }
        LinuxSession::Unknown => {
            remote_active.store(false, Ordering::Relaxed);
            clear_clipboard_target(&clipboard_target);
            return unknown_session_status("控制/捕获");
        }
        LinuxSession::X11 => {}
    }

    let target_count = targets.len();
    let (ready_tx, ready_rx) = mpsc::channel();
    thread::spawn(move || {
        let (connection, root, keymap) = match connect_x11_capture() {
            Ok(result) => result,
            Err(error) => {
                remote_active.store(false, Ordering::Relaxed);
                clear_clipboard_target(&clipboard_target);
                let _ = ready_tx.send(Err(error));
                return;
            }
        };
        let initial_point = query_pointer_point(&connection, root).ok();
        let context = Arc::new(X11CaptureContext {
            connection,
            root,
            keymap,
            quic_transport,
            layout_state,
            native_layout,
            stop: Arc::clone(&stop),
            send_gate: Mutex::new(()),
            active: Mutex::new(None),
            remote_active,
            clipboard_target,
            input_events,
            switch_request,
            anchor: Mutex::new(None),
            hotkey_return_point: Mutex::new(None),
            last_local_point: Mutex::new(initial_point),
            last_pointer_sync: Mutex::new(Instant::now()),
            last_mouse_move_sent: Mutex::new(None),
            last_heartbeat_sent: Mutex::new(None),
            remote_button_mask: AtomicU64::new(0),
            physical_pressed_keys: Mutex::new(Vec::new()),
            forwarded_pressed_keys: Mutex::new(Vec::new()),
            local_screen_points: Mutex::new(HashMap::new()),
            grabbed: AtomicBool::new(false),
            cursor_hidden: AtomicBool::new(false),
        });
        if let Ok(mut current) = x11_capture_slot().lock() {
            *current = Some(Arc::clone(&context));
        }
        let _ = ready_tx.send(Ok(()));

        while !context.stop.load(Ordering::Relaxed) {
            {
                let _send_guard = context
                    .send_gate
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                if !context.stop.load(Ordering::Relaxed) {
                    if active_target_input_failed(&context.quic_transport, &context.active) {
                        log::warn!(
                            "remote input transport failed; releasing Linux X11 grab and cursor"
                        );
                        release_x11_remote_control_inner(&context, None);
                    } else if !send_remote_input_heartbeat(
                        &context.quic_transport,
                        &context.active,
                        &context.remote_button_mask,
                        context
                            .physical_pressed_keys
                            .lock()
                            .map(|pressed| modifier_mask_for_keys(&pressed))
                            .unwrap_or_default(),
                        &context.last_heartbeat_sent,
                        &context.layout_state,
                        &context.input_events,
                    ) {
                        log::warn!(
                            "remote input heartbeat failed; releasing Linux X11 grab and cursor"
                        );
                        release_x11_remote_control_inner(&context, None);
                    } else {
                        drain_switch_request_x11(&context);
                    }
                }
            }

            loop {
                if context.stop.load(Ordering::Relaxed)
                    || context
                        .switch_request
                        .lock()
                        .map(|request| request.is_some())
                        .unwrap_or(false)
                {
                    break;
                }
                match context.connection.poll_for_event() {
                    Ok(Some(event)) => {
                        let _send_guard = context
                            .send_gate
                            .lock()
                            .unwrap_or_else(|poison| poison.into_inner());
                        if !context.stop.load(Ordering::Relaxed) {
                            handle_x11_event(&context, event);
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        log::warn!("Linux XI2 capture connection failed: {error}");
                        context.stop.store(true, Ordering::Relaxed);
                        break;
                    }
                }
            }
            let control_pending = context.stop.load(Ordering::Relaxed)
                || context
                    .switch_request
                    .lock()
                    .map(|request| request.is_some())
                    .unwrap_or(false);
            if !control_pending {
                // Block on the X11 socket instead of waking at a fixed 4 ms
                // cadence. The short timeout is only for non-X11 control
                // signals (stop and requested screen switches), bounding their
                // idle latency below one 16 ms display frame.
                if let Err(error) = wait_for_fd_activity(
                    context.connection.stream().as_raw_fd(),
                    X11_CAPTURE_IDLE_WAIT,
                ) {
                    log::warn!("Linux XI2 capture wait failed: {error}");
                    context.stop.store(true, Ordering::Relaxed);
                }
            }
        }

        {
            let _send_guard = context
                .send_gate
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            release_x11_remote_control_inner(&context, None);
            clear_physical_keys(&context);
        }
        clear_x11_capture_context(&context);
    });

    match ready_rx.recv_timeout(Duration::from_secs(2)) {
        Ok(Ok(())) => NativeStageStatus {
            state: "ready".into(),
            detail: format!(
                "Linux X11 控制端已就绪，{target_count} 条远端贴边可用于鼠标和键盘切换。"
            ),
        },
        Ok(Err(error)) => NativeStageStatus {
            state: "error".into(),
            detail: format!("Linux X11 控制端启动失败：{error}"),
        },
        Err(_) => NativeStageStatus {
            state: "error".into(),
            detail: "Linux X11 输入捕获在 2 秒内没有完成初始化。".into(),
        },
    }
}

#[cfg(target_os = "linux")]
fn query_pointer_point(connection: &RustConnection, root: u32) -> Result<(f64, f64), String> {
    let reply = connection
        .query_pointer(root)
        .map_err(|error| format!("读取 X11 指针位置失败：{error}"))?
        .reply()
        .map_err(|error| format!("读取 X11 指针位置失败：{error}"))?;
    Ok((f64::from(reply.root_x), f64::from(reply.root_y)))
}

#[cfg(target_os = "linux")]
fn warp_pointer(context: &X11CaptureContext, point: (f64, f64)) -> Result<(), String> {
    context
        .connection
        .warp_pointer(
            NONE,
            context.root,
            0,
            0,
            0,
            0,
            clamp_x11_coordinate(point.0.round() as i32),
            clamp_x11_coordinate(point.1.round() as i32),
        )
        .map_err(|error| format!("移动 X11 指针失败：{error}"))?;
    context
        .connection
        .flush()
        .map_err(|error| format!("刷新 X11 指针失败：{error}"))
}

#[cfg(target_os = "linux")]
fn grab_x11_input(context: &X11CaptureContext) -> Result<(), String> {
    if context.grabbed.load(Ordering::Relaxed) {
        return Ok(());
    }
    let pointer = context
        .connection
        .grab_pointer(
            false,
            context.root,
            CoreEventMask::NO_EVENT,
            GrabMode::ASYNC,
            GrabMode::ASYNC,
            NONE,
            NONE,
            CURRENT_TIME,
        )
        .map_err(|error| format!("X11 鼠标独占请求失败：{error}"))?
        .reply()
        .map_err(|error| format!("X11 鼠标独占请求失败：{error}"))?;
    if pointer.status != GrabStatus::SUCCESS {
        return Err(format!(
            "X11 鼠标正被其他程序独占（状态码 {}）",
            u8::from(pointer.status)
        ));
    }

    let keyboard = context
        .connection
        .grab_keyboard(
            false,
            context.root,
            CURRENT_TIME,
            GrabMode::ASYNC,
            GrabMode::ASYNC,
        )
        .map_err(|error| format!("X11 键盘独占请求失败：{error}"))?
        .reply()
        .map_err(|error| format!("X11 键盘独占请求失败：{error}"))?;
    if keyboard.status != GrabStatus::SUCCESS {
        let _ = context.connection.ungrab_pointer(CURRENT_TIME);
        let _ = context.connection.flush();
        return Err(format!(
            "X11 键盘正被其他程序独占（状态码 {}）",
            u8::from(keyboard.status)
        ));
    }
    context.grabbed.store(true, Ordering::Relaxed);
    Ok(())
}

#[cfg(target_os = "linux")]
fn ungrab_x11_input(context: &X11CaptureContext) {
    if !context.grabbed.swap(false, Ordering::Relaxed) {
        return;
    }
    let _ = context.connection.ungrab_keyboard(CURRENT_TIME);
    let _ = context.connection.ungrab_pointer(CURRENT_TIME);
    let _ = context.connection.flush();
}

#[cfg(target_os = "linux")]
fn hide_x11_cursor(context: &X11CaptureContext) -> Result<(), String> {
    if context.cursor_hidden.load(Ordering::Relaxed) {
        return Ok(());
    }
    context
        .connection
        .xfixes_hide_cursor(context.root)
        .map_err(|error| format!("隐藏 X11 指针失败：{error}"))?
        .check()
        .map_err(|error| format!("隐藏 X11 指针失败：{error}"))?;
    context.cursor_hidden.store(true, Ordering::Relaxed);
    Ok(())
}

#[cfg(target_os = "linux")]
fn show_x11_cursor(context: &X11CaptureContext) {
    if !context.cursor_hidden.swap(false, Ordering::Relaxed) {
        return;
    }
    if let Ok(cookie) = context.connection.xfixes_show_cursor(context.root) {
        let _ = cookie.check();
    }
    let _ = context.connection.flush();
}

#[cfg(target_os = "linux")]
fn enter_x11_remote(
    context: &X11CaptureContext,
    active_target: ActiveTarget,
    local_point: Option<(f64, f64)>,
) -> bool {
    let anchor = local_anchor_point(&active_target);
    if let Err(error) = grab_x11_input(context) {
        log::warn!("Linux X11 edge switch cancelled: {error}");
        return false;
    }
    if let Err(error) = hide_x11_cursor(context) {
        log::warn!("Linux X11 edge switch cancelled: {error}");
        ungrab_x11_input(context);
        return false;
    }
    if let Err(error) = warp_pointer(context, anchor) {
        log::warn!("Linux X11 edge switch cancelled: {error}");
        show_x11_cursor(context);
        ungrab_x11_input(context);
        return false;
    }
    if !send_remote_mouse_move(
        &context.quic_transport,
        &active_target,
        &context.layout_state,
        &context.input_events,
    ) {
        if let Some(point) = local_point {
            let _ = warp_pointer(context, point);
        }
        show_x11_cursor(context);
        ungrab_x11_input(context);
        reset_mouse_move_timer(&context.last_mouse_move_sent);
        reset_remote_button_mask(&context.remote_button_mask);
        return false;
    }

    reset_mouse_move_timer(&context.last_mouse_move_sent);
    reset_remote_button_mask(&context.remote_button_mask);
    context.remote_active.store(true, Ordering::Relaxed);
    sync_held_modifiers_x11(context, &active_target.target);
    set_control_clipboard_target(&context.clipboard_target, &active_target);
    if let Ok(mut active) = context.active.lock() {
        *active = Some(active_target);
    }
    if let Ok(mut state) = context.anchor.lock() {
        *state = Some(anchor);
    }
    if let Ok(mut state) = context.hotkey_return_point.lock() {
        *state = local_point;
    }
    true
}

#[cfg(target_os = "linux")]
fn release_forwarded_keys_x11(context: &X11CaptureContext, target: &InputTarget) {
    let held = context
        .forwarded_pressed_keys
        .lock()
        .map(|pressed| pressed.clone())
        .unwrap_or_default();
    for key_code in held {
        let _ = send_packet(
            &context.quic_transport,
            target,
            InputEvent::Key {
                key_code,
                down: false,
            },
            &context.layout_state,
            &context.input_events,
        );
    }
    if let Ok(mut pressed) = context.forwarded_pressed_keys.lock() {
        pressed.clear();
    }
}

#[cfg(target_os = "linux")]
fn release_x11_remote_control_inner(context: &X11CaptureContext, return_point: Option<(f64, f64)>) {
    let active = context
        .active
        .lock()
        .ok()
        .and_then(|mut active| active.take());
    if let Some(active) = active {
        release_forwarded_keys_x11(context, &active.target);
        release_remote_buttons(
            &context.quic_transport,
            &active.target,
            &context.remote_button_mask,
            &context.layout_state,
            &context.input_events,
        );
        let _ = send_remote_cursor_park(
            &context.quic_transport,
            &active,
            &context.layout_state,
            &context.input_events,
        );
        let point = return_point.unwrap_or_else(|| local_return_point(&active));
        let _ = warp_pointer(context, point);
        if let Ok(mut local) = context.last_local_point.lock() {
            *local = Some(point);
        }
    } else {
        reset_remote_button_mask(&context.remote_button_mask);
        if let Ok(mut pressed) = context.forwarded_pressed_keys.lock() {
            pressed.clear();
        }
    }
    context.remote_active.store(false, Ordering::Relaxed);
    reset_mouse_move_timer(&context.last_mouse_move_sent);
    if let Ok(mut anchor) = context.anchor.lock() {
        *anchor = None;
    }
    if let Ok(mut point) = context.hotkey_return_point.lock() {
        *point = None;
    }
    show_x11_cursor(context);
    ungrab_x11_input(context);
    end_x11_clipboard_session(&context.clipboard_target);
}

#[cfg(target_os = "linux")]
pub(super) fn release_active_remote_control() {
    let Some(context) = x11_capture_context() else {
        return;
    };
    let _send_guard = context
        .send_gate
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    release_x11_remote_control_inner(&context, None);
}

#[cfg(target_os = "linux")]
fn clear_physical_keys(context: &X11CaptureContext) {
    if let Ok(mut pressed) = context.physical_pressed_keys.lock() {
        pressed.clear();
    }
}

#[cfg(target_os = "linux")]
fn update_physical_key(context: &X11CaptureContext, key_code: u16, down: bool) {
    if let Ok(mut pressed) = context.physical_pressed_keys.lock() {
        if down {
            if !pressed.contains(&key_code) {
                pressed.push(key_code);
            }
        } else {
            pressed.retain(|pressed| *pressed != key_code);
        }
    }
}

#[cfg(target_os = "linux")]
fn x11_hotkey_modifiers(context: &X11CaptureContext) -> HotkeyModifiers {
    let pressed = context
        .physical_pressed_keys
        .lock()
        .map(|pressed| pressed.clone())
        .unwrap_or_default();
    let any = |keys: &[u16]| keys.iter().any(|key| pressed.contains(key));
    HotkeyModifiers {
        ctrl: any(&[0x11, 0xA2, 0xA3]),
        alt: any(&[0x12, 0xA4, 0xA5]),
        shift: any(&[0x10, 0xA0, 0xA1]),
        meta: any(&[0x5B, 0x5C]),
    }
}

#[cfg(target_os = "linux")]
fn sync_held_modifiers_x11(context: &X11CaptureContext, target: &InputTarget) {
    let held = context
        .physical_pressed_keys
        .lock()
        .map(|pressed| pressed.clone())
        .unwrap_or_default();
    for key_code in held
        .into_iter()
        .filter(|key| matches!(*key, 0x10 | 0x11 | 0x12 | 0x5B | 0x5C | 0xA0..=0xA5))
    {
        if send_packet(
            &context.quic_transport,
            target,
            InputEvent::Key {
                key_code,
                down: true,
            },
            &context.layout_state,
            &context.input_events,
        ) {
            track_forwarded_key(&context.forwarded_pressed_keys, key_code, true);
        }
    }
}

#[cfg(target_os = "linux")]
fn fp3232_to_f64(value: Fp3232) -> f64 {
    f64::from(value.integral) + f64::from(value.frac) / 4_294_967_296.0
}

#[cfg(target_os = "linux")]
fn raw_valuator(mask: &[u32], values: &[Fp3232], wanted_axis: usize) -> f64 {
    let mut value_index = 0;
    for (word_index, word) in mask.iter().copied().enumerate() {
        for bit in 0..32 {
            if word & (1_u32 << bit) == 0 {
                continue;
            }
            let axis = word_index * 32 + bit;
            let Some(value) = values.get(value_index).copied() else {
                return 0.0;
            };
            if axis == wanted_axis {
                return fp3232_to_f64(value);
            }
            value_index += 1;
        }
    }
    0.0
}

#[cfg(target_os = "linux")]
fn local_pointer_after_motion(context: &X11CaptureContext, dx: f64, dy: f64) -> (f64, f64) {
    let should_sync = context
        .last_pointer_sync
        .lock()
        .map(|last| last.elapsed() >= Duration::from_millis(8))
        .unwrap_or(true);
    let queried = should_sync
        .then(|| query_pointer_point(&context.connection, context.root).ok())
        .flatten();
    let point = queried.unwrap_or_else(|| {
        context
            .last_local_point
            .lock()
            .ok()
            .and_then(|point| *point)
            .map(|point| (point.0 + dx, point.1 + dy))
            .unwrap_or((dx, dy))
    });
    if queried.is_some() {
        if let Ok(mut last) = context.last_pointer_sync.lock() {
            *last = Instant::now();
        }
    }
    if let Ok(mut last) = context.last_local_point.lock() {
        *last = Some(point);
    }
    point
}

#[cfg(target_os = "linux")]
fn handle_x11_event(context: &X11CaptureContext, event: Event) {
    match event {
        Event::XinputRawMotion(event) => {
            let dx = raw_valuator(&event.valuator_mask, &event.axisvalues, 0);
            let dy = raw_valuator(&event.valuator_mask, &event.axisvalues, 1);
            handle_x11_motion(context, dx, dy);
        }
        Event::XinputRawKeyPress(event) => {
            handle_x11_key(context, event.detail, true);
        }
        Event::XinputRawKeyRelease(event) => {
            handle_x11_key(context, event.detail, false);
        }
        Event::XinputRawButtonPress(event) => {
            handle_x11_button(context, event.detail, true);
        }
        Event::XinputRawButtonRelease(event) => {
            handle_x11_button(context, event.detail, false);
        }
        Event::Error(error) => log::warn!("Linux X11 capture protocol error: {error:?}"),
        _ => {}
    }
}

#[cfg(target_os = "linux")]
fn handle_x11_motion(context: &X11CaptureContext, dx: f64, dy: f64) {
    if dx.abs() < f64::EPSILON && dy.abs() < f64::EPSILON {
        return;
    }

    let mut active = match context.active.lock() {
        Ok(active) => active,
        Err(_) => return,
    };
    if let Some(active_target) = active.as_mut() {
        active_target.x += dx;
        active_target.y += dy;
        if update_active_remote_screen(active_target, dx, dy, &context.layout_state) {
            let point = local_return_point(active_target);
            drop(active);
            release_x11_remote_control_inner(context, Some(point));
            return;
        }
        active_target.x = active_target
            .x
            .clamp(0.0, (active_target.current_screen.width - 1).max(0) as f64);
        active_target.y = active_target
            .y
            .clamp(0.0, (active_target.current_screen.height - 1).max(0) as f64);
        let button_mask = context.remote_button_mask.load(Ordering::Relaxed);
        if should_send_mouse_move(&context.last_mouse_move_sent, button_mask != 0)
            && !send_remote_mouse_move_with_drag(
                &context.quic_transport,
                active_target,
                button_mask,
                &context.layout_state,
                &context.input_events,
            )
        {
            let point = local_return_point(active_target);
            drop(active);
            release_x11_remote_control_inner(context, Some(point));
            return;
        }
        let anchor = context
            .anchor
            .lock()
            .ok()
            .and_then(|anchor| *anchor)
            .unwrap_or_else(|| local_anchor_point(active_target));
        drop(active);
        if let Err(error) = warp_pointer(context, anchor) {
            log::warn!("Linux X11 pointer re-pin failed: {error}");
            release_x11_remote_control_inner(context, None);
        }
        return;
    }
    drop(active);

    let (x, y) = local_pointer_after_motion(context, dx, dy);
    let targets = current_input_targets(&context.layout_state, &context.native_layout);
    if let Some(active_target) = crossing_target(&targets, x, y, dx, dy, &context.layout_state) {
        let _ = enter_x11_remote(context, active_target, Some((x, y)));
    }
}

#[cfg(target_os = "linux")]
fn handle_x11_key(context: &X11CaptureContext, detail: u32, down: bool) {
    let Some(keycode) = u8::try_from(detail).ok() else {
        if let Some(suppressed) = unknown_captured_key_log_decision() {
            log::warn!(
                "Linux X11 capture ignored out-of-range keycode {detail}{}",
                if suppressed == 0 {
                    String::new()
                } else {
                    format!(" ({suppressed} similar diagnostics suppressed)")
                }
            );
        }
        return;
    };
    let Some(vk) = context.keymap.vk_for_keycode(keycode) else {
        if let Some(suppressed) = unknown_captured_key_log_decision() {
            log::warn!(
                "Linux X11 capture ignored unmapped keycode {keycode}{}",
                if suppressed == 0 {
                    String::new()
                } else {
                    format!(" ({suppressed} similar diagnostics suppressed)")
                }
            );
        }
        return;
    };
    update_physical_key(context, vk, down);
    let active = context
        .active
        .lock()
        .ok()
        .and_then(|active| active.as_ref().cloned());
    let Some(active) = active else {
        return;
    };
    if down
        && screen_switch_hotkey_matches_vk(&context.layout_state, vk, x11_hotkey_modifiers(context))
    {
        let recorded = context
            .hotkey_return_point
            .lock()
            .ok()
            .and_then(|point| *point);
        let point = local_hotkey_return_point(&active, recorded);
        release_x11_remote_control_inner(context, Some(point));
        return;
    }
    let modifier_snapshot = context
        .physical_pressed_keys
        .lock()
        .map(|pressed| modifier_mask_for_keys(&pressed))
        .unwrap_or_default();
    let sent = send_key_packet(
        &context.quic_transport,
        &active.target,
        vk,
        down,
        modifier_snapshot,
        &context.layout_state,
        &context.input_events,
    );
    if sent {
        track_forwarded_key(&context.forwarded_pressed_keys, vk, down);
    } else {
        let point = local_return_point(&active);
        release_x11_remote_control_inner(context, Some(point));
    }
}

#[cfg(target_os = "linux")]
fn handle_x11_button(context: &X11CaptureContext, detail: u32, down: bool) {
    let active = context
        .active
        .lock()
        .ok()
        .and_then(|active| active.as_ref().cloned());
    let Some(active) = active else {
        return;
    };
    if (4..=7).contains(&detail) {
        if !down {
            return;
        }
        let (delta_x, delta_y) = match detail {
            4 => (0, 1),
            5 => (0, -1),
            6 => (-1, 0),
            7 => (1, 0),
            _ => unreachable!(),
        };
        let modifier_snapshot = context
            .physical_pressed_keys
            .lock()
            .map(|pressed| modifier_mask_for_keys(&pressed))
            .unwrap_or_default();
        if !send_packet_with_modifier_snapshot(
            &context.quic_transport,
            &active.target,
            InputEvent::Scroll {
                delta_x,
                delta_y,
                sequence: next_mouse_sequence(),
            },
            Some(modifier_snapshot),
            &context.layout_state,
            &context.input_events,
        ) {
            release_x11_remote_control_inner(context, None);
        }
        return;
    }
    let button = match detail {
        1 => MouseButton::Left,
        2 => MouseButton::Middle,
        3 => MouseButton::Right,
        _ => return,
    };
    let modifier_snapshot = context
        .physical_pressed_keys
        .lock()
        .map(|pressed| modifier_mask_for_keys(&pressed))
        .unwrap_or_default();
    let sent = send_packet_with_modifier_snapshot(
        &context.quic_transport,
        &active.target,
        InputEvent::MouseButton {
            button,
            down,
            screen_id: active.current_screen_id.clone(),
            x: Some(active.x.round() as i32),
            y: Some(active.y.round() as i32),
            sequence: next_mouse_sequence(),
        },
        Some(modifier_snapshot),
        &context.layout_state,
        &context.input_events,
    );
    if sent {
        update_remote_button_mask(&context.remote_button_mask, button, down);
    } else {
        release_x11_remote_control_inner(context, None);
    }
}

#[cfg(target_os = "linux")]
fn drain_switch_request_x11(context: &X11CaptureContext) {
    let direction = match context.switch_request.lock() {
        Ok(mut request) => request.take(),
        Err(_) => return,
    };
    let Some(direction) = direction else {
        return;
    };
    let current_point = query_pointer_point(&context.connection, context.root).ok();
    match request_screen_switch_from_point(
        direction,
        &context.layout_state,
        &context.native_layout,
        &context.active,
        current_point,
    ) {
        SwitchOutcome::Enter(active_target) => {
            let _ = enter_x11_remote(context, active_target, current_point);
        }
        SwitchOutcome::Return => {
            let active = context
                .active
                .lock()
                .ok()
                .and_then(|active| active.as_ref().cloned());
            let recorded = context
                .hotkey_return_point
                .lock()
                .ok()
                .and_then(|point| *point);
            let point = active
                .as_ref()
                .map(|active| local_hotkey_return_point(active, recorded));
            release_x11_remote_control_inner(context, point);
        }
        SwitchOutcome::LocalMove {
            from_screen_id,
            to_screen_id,
            x,
            y,
        } => {
            let point = remembered_local_screen_point(
                &context.local_screen_points,
                &from_screen_id,
                &to_screen_id,
                current_point,
                (x, y),
            );
            let _ = warp_pointer(context, point);
            if let Ok(mut local) = context.last_local_point.lock() {
                *local = Some(point);
            }
        }
        SwitchOutcome::Noop => {
            log::warn!("Linux screen switch {direction:?} ignored: no matching online target");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "linux")]
    use crate::shared_input::ALT_MODIFIER_MASK;

    #[test]
    fn wayland_wins_when_xwayland_also_sets_display() {
        assert_eq!(
            detect_session(Some("wayland"), Some("wayland-0"), Some(":0")),
            LinuxSession::Wayland
        );
    }

    #[test]
    fn display_without_wayland_selects_x11() {
        assert_eq!(detect_session(None, None, Some(":99")), LinuxSession::X11);
    }

    #[test]
    fn representative_vk_key_mappings_round_trip() {
        for vk in [0x41, 0x31, 0xA2, 0xA5, 0x5B, 0x25, 0x26, 0x70, 0x6F] {
            let entry = key_entry_for_vk(vk).expect("representative VK must be mapped");
            assert_eq!(
                key_entry_for_xkb_name(entry.xkb_name).map(|item| item.vk),
                Some(vk)
            );
            assert_eq!(
                key_entry_for_evdev(entry.evdev).map(|item| item.vk),
                Some(vk)
            );
            assert_eq!(
                key_entry_for_keysym(entry.keysym).map(|item| item.vk),
                Some(vk)
            );
        }
    }

    #[test]
    fn generic_modifier_vks_normalize_to_left_sided_linux_keys() {
        assert_eq!(normalize_injected_modifier_vk(0x10), 0xA0);
        assert_eq!(normalize_injected_modifier_vk(0x11), 0xA2);
        assert_eq!(normalize_injected_modifier_vk(0x12), 0xA4);
        assert_eq!(normalize_injected_modifier_vk(0x5B), 0x5B);
        assert_eq!(normalize_injected_modifier_vk(0x41), 0x41);
    }

    #[test]
    fn extended_and_media_key_mappings_round_trip() {
        let expected = [
            (*b"PRSC", 99, 0x2C, 0xFF61),
            (*b"PAUS", 119, 0x13, 0xFF13),
            (*b"MUTE", 113, 0xAD, 0x1008FF12),
            (*b"VOL-", 114, 0xAE, 0x1008FF11),
            (*b"VOL+", 115, 0xAF, 0x1008FF13),
            (*b"I171", 163, 0xB0, 0x1008FF17),
            (*b"I172", 164, 0xB3, 0x1008FF14),
            (*b"I173", 165, 0xB1, 0x1008FF16),
            (*b"I174", 166, 0xB2, 0x1008FF15),
            (*b"FK13", 183, 0x7C, 0xFFCA),
            (*b"FK14", 184, 0x7D, 0xFFCB),
            (*b"FK15", 185, 0x7E, 0xFFCC),
            (*b"FK16", 186, 0x7F, 0xFFCD),
            (*b"FK17", 187, 0x80, 0xFFCE),
            (*b"FK18", 188, 0x81, 0xFFCF),
            (*b"FK19", 189, 0x82, 0xFFD0),
            (*b"FK20", 190, 0x83, 0xFFD1),
            (*b"FK21", 191, 0x84, 0xFFD2),
            (*b"FK22", 192, 0x85, 0xFFD3),
            (*b"FK23", 193, 0x86, 0xFFD4),
            (*b"FK24", 194, 0x87, 0xFFD5),
        ];
        for (name, evdev, vk, keysym) in expected {
            let entry = key_entry_for_vk(vk).expect("extended VK must be mapped");
            assert_eq!(
                entry,
                &KeyMapEntry {
                    xkb_name: name,
                    evdev,
                    vk,
                    keysym,
                }
            );
            assert_eq!(key_entry_for_xkb_name(name), Some(entry));
            assert_eq!(key_entry_for_evdev(evdev), Some(entry));
            assert_eq!(key_entry_for_keysym(keysym), Some(entry));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn capture_wait_uses_fd_and_bounds_control_latency() {
        use std::{io::Write as _, os::unix::net::UnixStream};

        assert!(X11_CAPTURE_IDLE_WAIT <= Duration::from_millis(16));
        let (reader, mut writer) = UnixStream::pair().expect("Unix socket pair");
        assert!(
            !wait_for_fd_activity(reader.as_raw_fd(), Duration::from_millis(1))
                .expect("idle fd poll"),
            "an idle descriptor should time out"
        );

        writer.write_all(&[1]).expect("make descriptor readable");
        assert!(
            wait_for_fd_activity(reader.as_raw_fd(), X11_CAPTURE_IDLE_WAIT)
                .expect("readable fd poll"),
            "X11 activity should wake the capture loop without polling delay"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unknown_key_diagnostics_are_burst_limited() {
        let start = Instant::now();
        let mut throttle = DiagnosticLogThrottle::new(start);
        for _ in 0..UNKNOWN_KEY_LOG_BURST {
            assert_eq!(throttle.decision(start), Some(0));
        }
        assert_eq!(throttle.decision(start), None);
        assert_eq!(throttle.decision(start), None);
        assert_eq!(
            throttle.decision(start + UNKNOWN_KEY_LOG_WINDOW),
            Some(2),
            "the next diagnostic should report the suppressed count"
        );
    }

    #[test]
    fn backend_status_never_calls_wayland_ready() {
        assert_eq!(
            capability_state(LinuxSession::Wayland, Ok::<(), &str>(())),
            CapabilityState::Unsupported
        );
        assert_eq!(
            capability_state(LinuxSession::X11, Ok::<(), &str>(())),
            CapabilityState::Ready
        );
        assert_eq!(
            capability_state(LinuxSession::X11, Err("XTEST missing")),
            CapabilityState::Error
        );
    }

    #[test]
    fn ending_x11_control_always_unbinds_the_clipboard_peer() {
        let target =
            std::sync::Arc::new(std::sync::Mutex::new(Some(super::super::ClipboardTarget {
                device_id: "peer-device".into(),
                addr: "10.0.0.2:47834".into(),
                transport_public_key: "peer-public-key".into(),
                protocol_version: crate::quic_transport::PROTOCOL_VERSION,
                cluster_id: "cluster-test".into(),
                pair_secret: "secret-test".into(),
                push_on_bind: true,
                expires_at: None,
            })));

        end_x11_clipboard_session(&target);

        assert!(target.lock().expect("clipboard target lock").is_none());
    }

    #[test]
    fn receive_cursor_park_state_reveals_once_on_real_drift() {
        let mut state = ReceiveCursorState::default();
        assert!(state.park((100, 200)), "first park needs a native hide");
        assert!(!state.park((100, 200)), "repeat park must not stack hides");
        assert!(!state.should_reveal_for_pointer((103, 197), 3));
        assert!(state.should_reveal_for_pointer((104, 200), 3));
        assert!(state.reveal(), "first reveal balances the hide");
        assert!(!state.reveal(), "repeat reveal must be a no-op");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn xkb_physical_names_build_bidirectional_keymap() {
        let keymap =
            X11KeyMap::from_xkb_names(8, [*b"ESC\0", *b"AE01", *b"LCTL", *b"LEFT", *b"UP\0\0"]);
        for (keycode, vk) in [(8, 0x1B), (9, 0x31), (10, 0xA2), (11, 0x25), (12, 0x26)] {
            assert_eq!(keymap.vk_for_keycode(keycode), Some(vk));
            assert_eq!(keymap.keycode_for_vk(vk), Some(keycode));
        }
    }

    /// Real protocol smoke test. CI runs this under Xvfb; keeping it ignored
    /// prevents an ordinary unit-test run from touching a developer's cursor.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires an isolated Xvfb display"]
    fn x11_xtest_grab_round_trip() {
        use std::collections::HashSet;
        use x11rb::{
            connection::Connection as _,
            protocol::xproto::{
                ConnectionExt as _, CreateWindowAux, EventMask, InputFocus, WindowClass,
            },
        };

        assert!(std::env::var_os("DISPLAY").is_some(), "DISPLAY is required");
        let capture = capture_status(1);
        assert_eq!(capture.state, "ready", "{}", capture.detail);
        let receive = receive_status(24800);
        assert_eq!(receive.state, "ready", "{}", receive.detail);

        let mut injector = X11Injector::connect().expect("XTEST/XKB must initialize");
        check_capture_extensions(&injector.connection).expect("XI2/XFixes must initialize");
        let (observer, _) = x11rb::connect(None).expect("observer X11 connection");
        let window = observer.generate_id().expect("observer window id");
        observer
            .create_window(
                x11rb::COPY_FROM_PARENT as u8,
                window,
                injector.root,
                10,
                10,
                200,
                200,
                0,
                WindowClass::INPUT_OUTPUT,
                x11rb::COPY_FROM_PARENT,
                &CreateWindowAux::new().event_mask(
                    EventMask::POINTER_MOTION
                        | EventMask::KEY_PRESS
                        | EventMask::KEY_RELEASE
                        | EventMask::BUTTON_PRESS
                        | EventMask::BUTTON_RELEASE,
                ),
            )
            .expect("create observer window")
            .check()
            .expect("create observer window reply");
        observer
            .map_window(window)
            .expect("map observer window")
            .check()
            .expect("map observer window reply");
        observer
            .set_input_focus(InputFocus::PARENT, window, CURRENT_TIME)
            .expect("focus observer window")
            .check()
            .expect("focus observer window reply");
        observer.flush().expect("flush observer setup");

        injector.mouse_move(73, 91).expect("XTEST motion");
        let point =
            query_pointer_point(&injector.connection, injector.root).expect("query pointer");
        assert_eq!(point, (73.0, 91.0));

        injector.key(0x41, true).expect("XTEST key down");
        injector
            .mouse_button(MouseButton::Left, true, 73, 91)
            .expect("XTEST button down");
        assert_eq!(injector.pressed_keys, vec![0x41]);
        assert_eq!(injector.pressed_buttons, vec![1]);
        injector.release_all().expect("release held XTEST inputs");
        assert!(injector.pressed_keys.is_empty());
        assert!(injector.pressed_buttons.is_empty());

        injector
            .key(0x11, true)
            .expect("generic Control must normalize to left Control");
        assert_eq!(injector.pressed_keys, vec![0xA2]);
        injector
            .reconcile_modifier_snapshot(0)
            .expect("authoritative empty snapshot must release Control");
        assert!(injector.pressed_keys.is_empty());

        injector
            .key(0xA5, true)
            .expect("right Alt must inject before snapshot reconciliation");
        injector
            .reconcile_modifier_snapshot(ALT_MODIFIER_MASK)
            .expect("right Alt snapshot must not synthesize left Alt");
        assert_eq!(injector.pressed_keys, vec![0xA5]);
        injector.key(0xA5, false).expect("release right Alt");

        let extended_keycodes = [0x2C, 0x13, 0x7C, 0x87, 0xAD, 0xB0, 0xB1, 0xB3]
            .into_iter()
            .map(|vk| {
                let keycode = injector
                    .keymap
                    .keycode_for_vk(vk)
                    .expect("extended X11 keycode");
                injector.key(vk, true).expect("extended XTEST key down");
                injector.key(vk, false).expect("extended XTEST key up");
                keycode
            })
            .collect::<HashSet<_>>();

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut saw_motion = false;
        let mut saw_key_down = false;
        let mut saw_key_up = false;
        let mut saw_button_down = false;
        let mut saw_button_up = false;
        let mut extended_key_down = HashSet::new();
        let mut extended_key_up = HashSet::new();
        while Instant::now() < deadline
            && !(saw_motion
                && saw_key_down
                && saw_key_up
                && saw_button_down
                && saw_button_up
                && extended_key_down == extended_keycodes
                && extended_key_up == extended_keycodes)
        {
            match observer.poll_for_event().expect("poll observer event") {
                Some(Event::MotionNotify(_)) => saw_motion = true,
                Some(Event::KeyPress(event)) => {
                    saw_key_down = true;
                    if extended_keycodes.contains(&event.detail) {
                        extended_key_down.insert(event.detail);
                    }
                }
                Some(Event::KeyRelease(event)) => {
                    saw_key_up = true;
                    if extended_keycodes.contains(&event.detail) {
                        extended_key_up.insert(event.detail);
                    }
                }
                Some(Event::ButtonPress(_)) => saw_button_down = true,
                Some(Event::ButtonRelease(_)) => saw_button_up = true,
                Some(_) => {}
                None => thread::sleep(Duration::from_millis(5)),
            }
        }
        assert!(saw_motion, "second connection did not observe XTEST motion");
        assert!(
            saw_key_down,
            "second connection did not observe XTEST key down"
        );
        assert!(saw_key_up, "release_all did not emit XTEST key up");
        assert!(
            saw_button_down,
            "second connection did not observe XTEST button down"
        );
        assert!(saw_button_up, "release_all did not emit XTEST button up");
        assert_eq!(
            extended_key_down, extended_keycodes,
            "second connection did not observe every extended key down"
        );
        assert_eq!(
            extended_key_up, extended_keycodes,
            "second connection did not observe every extended key up"
        );

        let pointer = injector
            .connection
            .grab_pointer(
                false,
                injector.root,
                CoreEventMask::NO_EVENT,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
                NONE,
                NONE,
                CURRENT_TIME,
            )
            .expect("pointer grab request")
            .reply()
            .expect("pointer grab reply");
        assert!(pointer.status == GrabStatus::SUCCESS);
        let keyboard = injector
            .connection
            .grab_keyboard(
                false,
                injector.root,
                CURRENT_TIME,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
            )
            .expect("keyboard grab request")
            .reply()
            .expect("keyboard grab reply");
        assert!(keyboard.status == GrabStatus::SUCCESS);

        injector
            .connection
            .xfixes_hide_cursor(injector.root)
            .expect("hide cursor request")
            .check()
            .expect("hide cursor reply");
        injector
            .connection
            .xfixes_show_cursor(injector.root)
            .expect("show cursor request")
            .check()
            .expect("show cursor reply");
        injector
            .connection
            .ungrab_keyboard(CURRENT_TIME)
            .expect("keyboard ungrab");
        injector
            .connection
            .ungrab_pointer(CURRENT_TIME)
            .expect("pointer ungrab");
        injector.connection.flush().expect("flush cleanup");
        observer
            .destroy_window(window)
            .expect("destroy observer window")
            .check()
            .expect("destroy observer window reply");
    }
}
