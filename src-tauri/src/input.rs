use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc, Mutex, OnceLock, TryLockError,
    },
    thread,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

use crate::{
    quic_transport,
    shared_input::{
        button_from_mask, mouse_button_mask, InputCommand, InputEvent, MouseButton,
        LEFT_BUTTON_MASK, MIDDLE_BUTTON_MASK, RIGHT_BUTTON_MASK,
    },
    Device, LayoutState, NativeStageStatus, Screen,
};

const INPUT_PROTOCOL: &str = "mykvm.input.v1";
const INPUT_CONTROL_PROTOCOL: &str = "mykvm.input-control.v1";
const EDGE_TOLERANCE: i32 = 80;
// The cursor must reach the very edge pixel before a crossing is considered.
// macOS clamps the pointer to the screen, so the furthest it can sit is
// width-1 (the last pixel); x >= right-1 means "pushed flush against the edge",
// matching how a real extended display only hands off once the cursor is on the
// boundary. CGEvent deltas are raw HID movement, so a positive dx with the
// pointer already pinned at the edge still reads as the user pushing outward —
// that push is what triggers the handoff.
const CROSSING_MARGIN: f64 = 1.0;
const MIN_CROSSING_DELTA: f64 = 1.0;
const CROSSING_AXIS_DOMINANCE: f64 = 0.5;
const CROSSING_ACTIVATION_BAND: f64 = EDGE_TOLERANCE as f64 * 2.0;
// On return to the local machine, land the cursor flush against the entry edge
// (0px inset) for a seamless extended-display feel, mirroring how the cursor
// would sit at the edge of a real second monitor. Re-bounce after a fast
// back-flick is prevented by a time-based return cooldown (last_return), not by
// inset distance, so the cursor can sit exactly on the edge.
const RETURN_EDGE_INSET: f64 = 0.0;
// After returning to local, refuse to cross back into the remote for this long.
// Lets a fast back-flick settle at the edge without bouncing into the remote.
const RETURN_COOLDOWN_MS: u64 = 150;
const MOUSE_MOVE_SEND_INTERVAL_MS: u64 = 8;
const DRAG_MOVE_SEND_INTERVAL_MS: u64 = 8;
#[cfg(target_os = "macos")]
const MACOS_IDLE_CAPTURE_LOOP_MS: u64 = 100;
#[cfg(target_os = "macos")]
const MACOS_VISIBLE_REMOTE_CAPTURE_LOOP_MS: u64 = 16;
#[cfg(target_os = "macos")]
const MACOS_HIDDEN_REMOTE_CAPTURE_LOOP_MS: u64 = 50;
#[cfg(target_os = "macos")]
const MACOS_HIDDEN_WINDOW_CURSOR_HIDE_REASSERT_MS: u64 = 250;
#[cfg(target_os = "macos")]
const MACOS_NSEVENT_TYPE_SYSTEM_DEFINED: u32 = 14;
#[cfg(target_os = "macos")]
const MACOS_NSEVENT_TYPE_ROTATE: u32 = 18;
#[cfg(target_os = "macos")]
const MACOS_NSEVENT_TYPE_BEGIN_GESTURE: u32 = 19;
#[cfg(target_os = "macos")]
const MACOS_NSEVENT_TYPE_END_GESTURE: u32 = 20;
#[cfg(target_os = "macos")]
const MACOS_NSEVENT_TYPE_GESTURE: u32 = 29;
#[cfg(target_os = "macos")]
const MACOS_NSEVENT_TYPE_MAGNIFY: u32 = 30;
#[cfg(target_os = "macos")]
const MACOS_NSEVENT_TYPE_SWIPE: u32 = 31;
#[cfg(target_os = "macos")]
const MACOS_NSEVENT_TYPE_SMART_MAGNIFY: u32 = 32;
#[cfg(target_os = "macos")]
const MACOS_NSEVENT_TYPE_QUICK_LOOK: u32 = 33;
#[cfg(target_os = "macos")]
const MACOS_NSEVENT_TYPE_PRESSURE: u32 = 34;
#[cfg(target_os = "macos")]
const MACOS_NSEVENT_TYPE_DIRECT_TOUCH: u32 = 37;
#[cfg(target_os = "macos")]
const MACOS_NSEVENT_TYPE_CHANGE_MODE: u32 = 38;
#[cfg(target_os = "macos")]
const MACOS_RAW_EVENT_TAP_DISABLED_BY_TIMEOUT: u32 = 0xFFFF_FFFE;
#[cfg(target_os = "macos")]
const MACOS_RAW_EVENT_TAP_DISABLED_BY_USER_INPUT: u32 = 0xFFFF_FFFF;
#[cfg(target_os = "macos")]
const MACOS_RAW_GESTURE_EVENT_TYPES: &[u32] = &[
    MACOS_NSEVENT_TYPE_SYSTEM_DEFINED,
    MACOS_NSEVENT_TYPE_ROTATE,
    MACOS_NSEVENT_TYPE_BEGIN_GESTURE,
    MACOS_NSEVENT_TYPE_END_GESTURE,
    MACOS_NSEVENT_TYPE_GESTURE,
    MACOS_NSEVENT_TYPE_MAGNIFY,
    MACOS_NSEVENT_TYPE_SWIPE,
    MACOS_NSEVENT_TYPE_SMART_MAGNIFY,
    MACOS_NSEVENT_TYPE_QUICK_LOOK,
    MACOS_NSEVENT_TYPE_PRESSURE,
    MACOS_NSEVENT_TYPE_DIRECT_TOUCH,
    MACOS_NSEVENT_TYPE_CHANGE_MODE,
];
#[cfg(target_os = "windows")]
const WINDOWS_DESKTOP_CHECK_INTERVAL_MS: u64 = 250;

static REMOTE_MOUSE_STATE: OnceLock<Mutex<RemoteMouseState>> = OnceLock::new();
#[cfg(target_os = "macos")]
static MACOS_ACCESSIBILITY_PROMPTED: AtomicBool = AtomicBool::new(false);
#[cfg(target_os = "windows")]
static WINDOWS_INPUT_DESKTOP_DEFAULT_CACHE: AtomicBool = AtomicBool::new(true);

#[derive(Debug, Clone, Copy, PartialEq)]
enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

#[derive(Debug, Clone)]
struct InputTarget {
    device_id: String,
    origin_device_id: String,
    cluster_id: String,
    pair_secret: String,
    target_addr: String,
    target_platform: String,
    transport_public_key: String,
    protocol_version: u16,
    screen_id: String,
    local_screen: Screen,
    layout_local_screen: Screen,
    remote_screen: Screen,
    edge: Edge,
}

#[derive(Debug, Clone)]
struct ActiveTarget {
    target: InputTarget,
    // The remote screen the cursor is currently over and the wire id we send for
    // it. These start as the screen we crossed into and change as the cursor
    // roams across the remote device's other screens. `x`/`y` are coordinates
    // local to `current_screen`.
    current_screen: Screen,
    current_screen_id: String,
    x: f64,
    y: f64,
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    invert_y: bool,
}

#[derive(Debug, Clone)]
pub struct ClipboardTarget {
    pub device_id: String,
    pub addr: String,
    pub transport_public_key: String,
    pub protocol_version: u16,
    pub cluster_id: String,
    pub pair_secret: String,
    pub expires_at: Option<Instant>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InputPacket {
    protocol: String,
    #[serde(default)]
    target_device_id: String,
    #[serde(default)]
    origin_device_id: String,
    #[serde(default)]
    origin_port: u16,
    #[serde(default)]
    origin_transport_public_key: String,
    #[serde(default = "default_protocol_version")]
    origin_protocol_version: u16,
    #[serde(default)]
    cluster_id: String,
    #[serde(default)]
    pair_secret: String,
    event: InputEvent,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InputControlPacket {
    protocol: String,
    #[serde(default)]
    target_device_id: String,
    #[serde(default)]
    origin_device_id: String,
    #[serde(default)]
    origin_transport_public_key: String,
    #[serde(default = "default_protocol_version")]
    origin_protocol_version: u16,
    #[serde(default)]
    cluster_id: String,
    #[serde(default)]
    pair_secret: String,
    command: InputControlCommand,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
enum InputControlCommand {
    SecureAttention,
}

#[derive(Debug, Default, Clone, Copy)]
struct RemoteMouseState {
    x: i32,
    y: i32,
    buttons: u64,
}

pub fn stopped_capture_status() -> NativeStageStatus {
    NativeStageStatus {
        state: "stubbed".into(),
        detail: "Input sharing is stopped.".into(),
    }
}

pub fn stopped_inject_status() -> NativeStageStatus {
    NativeStageStatus {
        state: "stubbed".into(),
        detail: "Input injection is stopped.".into(),
    }
}

/// Direction requested by a screen-switch hotkey. Maps onto the `Edge` that a
/// mouse crossing would follow: `Right` means "the remote sits to the right of
/// the local screen", matching `Edge::Right` on the `InputTarget`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchDirection {
    Left,
    Right,
    Up,
    Down,
}

impl SwitchDirection {
    fn matches_edge(self, edge: Edge) -> bool {
        matches!(
            (self, edge),
            (SwitchDirection::Left, Edge::Left)
                | (SwitchDirection::Right, Edge::Right)
                | (SwitchDirection::Up, Edge::Top)
                | (SwitchDirection::Down, Edge::Bottom)
        )
    }
}

/// Outcome of a hotkey-driven switch request. The capture loop acts on it: an
/// `Enter` builds an `ActiveTarget` and runs the enter sequence; a `Return`
/// hands control back to the local machine.
enum SwitchOutcome {
    Enter(ActiveTarget),
    LocalMove {
        from_screen_id: String,
        to_screen_id: String,
        x: f64,
        y: f64,
    },
    Return,
    Noop,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct HotkeyModifiers {
    ctrl: bool,
    alt: bool,
    shift: bool,
    meta: bool,
}

fn screen_switch_hotkey_matches_vk(
    layout_state: &Arc<Mutex<LayoutState>>,
    key_code: u16,
    modifiers: HotkeyModifiers,
) -> bool {
    let Ok(layout) = layout_state.lock() else {
        return false;
    };
    if layout.machine_role != "server" {
        return false;
    }

    screen_switch_hotkeys_match_vk(&layout.screen_switch_hotkeys, key_code, modifiers)
}

fn screen_switch_hotkeys_match_vk(
    hotkeys: &crate::ScreenSwitchHotkeys,
    key_code: u16,
    modifiers: HotkeyModifiers,
) -> bool {
    [
        hotkeys.left.as_str(),
        hotkeys.right.as_str(),
        hotkeys.up.as_str(),
        hotkeys.down.as_str(),
    ]
    .into_iter()
    .any(|hotkey| hotkey_matches_vk(hotkey, key_code, modifiers))
}

fn hotkey_matches_vk(value: &str, key_code: u16, modifiers: HotkeyModifiers) -> bool {
    let normalized = value.trim().to_ascii_lowercase().replace(' ', "");
    if normalized.is_empty()
        || matches!(normalized.as_str(), "disabled" | "disable" | "off" | "none")
    {
        return false;
    }

    let mut required = HotkeyModifiers::default();
    let mut main_key = None;
    for part in normalized.split('+').filter(|part| !part.is_empty()) {
        match part {
            "ctrl" | "control" => required.ctrl = true,
            "alt" | "option" => required.alt = true,
            "shift" => required.shift = true,
            "meta" | "cmd" | "command" | "win" | "windows" | "super" | "os" => {
                required.meta = true;
            }
            key => {
                if main_key.is_some() {
                    return false;
                }
                main_key = hotkey_key_to_windows_vk(key);
            }
        }
    }

    main_key == Some(key_code) && required == modifiers
}

fn hotkey_key_to_windows_vk(key: &str) -> Option<u16> {
    if key.len() == 1 {
        let byte = key.as_bytes()[0];
        if byte.is_ascii_alphabetic() {
            return Some(byte.to_ascii_uppercase() as u16);
        }
        if byte.is_ascii_digit() {
            return Some(byte as u16);
        }
    }

    if let Some(function_number) = key
        .strip_prefix('f')
        .and_then(|value| value.parse::<u16>().ok())
    {
        if (1..=24).contains(&function_number) {
            return Some(0x70 + function_number - 1);
        }
    }

    Some(match key {
        "space" | "spacebar" => 0x20,
        "tab" => 0x09,
        "enter" | "return" => 0x0D,
        "esc" | "escape" => 0x1B,
        "scrolllock" | "scroll" | "scrlk" => 0x91,
        "up" | "arrowup" => 0x26,
        "down" | "arrowdown" => 0x28,
        "left" | "arrowleft" => 0x25,
        "right" | "arrowright" => 0x27,
        _ => return None,
    })
}

/// Resolve a hotkey switch request against the current targets and active
/// state. Called from the capture thread's poll loop.
///
/// - If we are currently local (`active` is `None`): move to a local screen in
///   that direction when one exists, otherwise pick the first online remote
///   target whose `edge` matches the requested direction.
/// - If we are already controlling a remote (`active` is `Some`): request a
///   return to local. The user can then press the direction key again to cross
///   into a different remote.
#[cfg(test)]
fn request_screen_switch(
    direction: SwitchDirection,
    layout_state: &Arc<Mutex<LayoutState>>,
    native_layout: &LayoutState,
    active: &Mutex<Option<ActiveTarget>>,
) -> SwitchOutcome {
    request_screen_switch_from_point(direction, layout_state, native_layout, active, None)
}

fn request_screen_switch_from_point(
    direction: SwitchDirection,
    layout_state: &Arc<Mutex<LayoutState>>,
    native_layout: &LayoutState,
    active: &Mutex<Option<ActiveTarget>>,
    current_point: Option<(f64, f64)>,
) -> SwitchOutcome {
    let currently_remote = active.lock().map(|a| a.is_some()).unwrap_or(false);
    if currently_remote {
        return SwitchOutcome::Return;
    }

    // Rebuild targets from the live layout every time: peers come and go after
    // the capture thread started, so the static snapshot built at startup would
    // miss a device that appeared later.
    let Ok(layout) = layout_state.lock() else {
        return SwitchOutcome::Noop;
    };
    let source_screen_id =
        source_local_screen(&layout, native_layout, current_point).map(|screen| screen.id.clone());
    if let Some(local_move) = local_screen_switch_point(
        direction,
        &layout,
        native_layout,
        source_screen_id.as_deref(),
    ) {
        return SwitchOutcome::LocalMove {
            from_screen_id: local_move.from_screen_id,
            to_screen_id: local_move.to_screen_id,
            x: local_move.x,
            y: local_move.y,
        };
    }
    let targets = build_input_targets(&layout, native_layout);
    drop(layout);

    let target = targets
        .iter()
        .filter(|target| {
            source_screen_id
                .as_deref()
                .map(|id| target.layout_local_screen.id == id)
                .unwrap_or(true)
        })
        .find(|target| direction.matches_edge(target.edge))
        .or_else(|| {
            targets
                .iter()
                .find(|target| direction.matches_edge(target.edge))
        });
    let Some(target) = target else {
        return SwitchOutcome::Noop;
    };

    // Land the remote cursor at the centre of the entry screen — there is no
    // mouse trajectory to derive an entry offset from, so the middle is the
    // least surprising landing spot.
    let remote_x = (target.remote_screen.width as f64 / 2.0)
        .clamp(0.0, (target.remote_screen.width - 1) as f64);
    let remote_y = (target.remote_screen.height as f64 / 2.0)
        .clamp(0.0, (target.remote_screen.height - 1) as f64);

    let mut current_screen = target.remote_screen.clone();
    current_screen.id = target.screen_id.clone();

    SwitchOutcome::Enter(ActiveTarget {
        target: target.clone(),
        current_screen,
        current_screen_id: target.screen_id.clone(),
        x: remote_x,
        y: remote_y,
        invert_y: false,
    })
}

fn source_local_screen<'a>(
    layout: &'a LayoutState,
    native_layout: &LayoutState,
    current_point: Option<(f64, f64)>,
) -> Option<&'a Screen> {
    let local = local_device(layout)?;
    if let Some((x, y)) = current_point {
        if let Some(native_local) = local_device(native_layout) {
            for native_screen in &native_local.screens {
                let native_screen = platform_native_screen(native_screen);
                if point_in_screen(&native_screen, x, y) {
                    if let Some(screen) = local
                        .screens
                        .iter()
                        .find(|screen| screen.id == native_screen.id)
                    {
                        return Some(screen);
                    }
                }
            }
        }
        if let Some(screen) = local
            .screens
            .iter()
            .find(|screen| point_in_screen(screen, x, y))
        {
            return Some(screen);
        }
    }

    local
        .screens
        .iter()
        .find(|screen| screen.is_primary)
        .or_else(|| local.screens.first())
}

struct LocalScreenMove {
    from_screen_id: String,
    to_screen_id: String,
    x: f64,
    y: f64,
}

fn local_screen_switch_point(
    direction: SwitchDirection,
    layout: &LayoutState,
    native_layout: &LayoutState,
    source_screen_id: Option<&str>,
) -> Option<LocalScreenMove> {
    let local = local_device(layout)?;
    let source = source_screen_id
        .and_then(|id| local.screens.iter().find(|screen| screen.id == id))
        .or_else(|| local.screens.iter().find(|screen| screen.is_primary))
        .or_else(|| local.screens.first())?;

    let target = local.screens.iter().find(|screen| {
        screen.id != source.id
            && !screens_overlap(source, screen)
            && touching_edge(source, screen)
                .map(|edge| direction.matches_edge(edge))
                .unwrap_or(false)
    })?;

    let native_target = local_device(native_layout)
        .and_then(|device| device.screens.iter().find(|screen| screen.id == target.id))
        .map(platform_native_screen)
        .unwrap_or_else(|| platform_native_screen(target));
    let (x, y) = screen_center_point(&native_target);
    Some(LocalScreenMove {
        from_screen_id: source.id.clone(),
        to_screen_id: target.id.clone(),
        x,
        y,
    })
}

fn screen_center_point(screen: &Screen) -> (f64, f64) {
    (
        screen.x as f64 + (screen.width as f64 / 2.0).clamp(0.0, (screen.width - 1).max(0) as f64),
        screen.y as f64
            + (screen.height as f64 / 2.0).clamp(0.0, (screen.height - 1).max(0) as f64),
    )
}

fn remembered_local_screen_point(
    points: &Mutex<HashMap<String, (f64, f64)>>,
    from_screen_id: &str,
    to_screen_id: &str,
    current_point: Option<(f64, f64)>,
    fallback: (f64, f64),
) -> (f64, f64) {
    let Ok(mut points) = points.lock() else {
        return fallback;
    };
    if let Some(point) = current_point {
        points.insert(from_screen_id.to_string(), point);
    }
    points.get(to_screen_id).copied().unwrap_or(fallback)
}

pub fn start_input_runtime(
    layout: LayoutState,
    layout_state: Arc<Mutex<LayoutState>>,
    native_layout: LayoutState,
    quic_transport: quic_transport::TransportHandle,
    stop: Arc<AtomicBool>,
    remote_active: Arc<AtomicBool>,
    main_window_visible: Arc<AtomicBool>,
    main_window_focused: Arc<AtomicBool>,
    clipboard_target: Arc<Mutex<Option<ClipboardTarget>>>,
    input_events: Arc<AtomicU64>,
    switch_request: Arc<Mutex<Option<SwitchDirection>>>,
) -> (NativeStageStatus, NativeStageStatus) {
    let inject_status = input_receive_status(&layout, true);
    if layout.input_mode == "receive" {
        remote_active.store(false, Ordering::Relaxed);
        clear_clipboard_target(&clipboard_target);
        start_platform_receive_monitor(stop);
        return (receive_only_status(), inject_status);
    }

    let targets = build_input_targets(&layout, &native_layout);
    let capture_status = start_input_capture(
        targets,
        layout_state,
        native_layout,
        quic_transport,
        stop,
        remote_active,
        main_window_visible,
        main_window_focused,
        clipboard_target,
        input_events,
        switch_request,
    );

    (capture_status, inject_status)
}

pub fn input_runtime_status(
    layout: &LayoutState,
    native_layout: &LayoutState,
) -> (NativeStageStatus, NativeStageStatus) {
    let targets = build_input_targets(layout, native_layout);
    let capture = if layout.input_mode == "receive" {
        receive_only_status()
    } else if targets.is_empty() {
        no_target_status(layout)
    } else if cfg!(any(target_os = "macos", target_os = "windows")) {
        NativeStageStatus {
            state: "ready".into(),
            detail: format!(
                "控制端已就绪，{} 条远端贴边可用于鼠标和键盘切换。",
                targets.len()
            ),
        }
    } else {
        unsupported_capture_status()
    };

    (capture, input_receive_status(layout, false))
}

fn input_receive_status(layout: &LayoutState, request_permission: bool) -> NativeStageStatus {
    let _ = request_permission;

    #[cfg(target_os = "macos")]
    if !macos_accessibility_trusted(request_permission) {
        return NativeStageStatus {
            state: "error".into(),
            detail: "macOS 需要给 MyKVM 辅助功能权限才能注入远端点击和键盘。请到 系统设置 > 隐私与安全性 > 辅助功能 启用 MyKVM，然后完全退出并重新打开应用。".into(),
        };
    }

    // When Secure Keyboard Entry is active anywhere on the system, macOS silently
    // drops *every* synthetic key event while still delivering synthetic mouse
    // events. That is exactly the "clicks work but the keyboard does nothing"
    // symptom, so we surface it instead of failing silently.
    #[cfg(target_os = "macos")]
    if macos_secure_input_enabled() {
        return NativeStageStatus {
            state: "error".into(),
            detail: "检测到 macOS 安全键盘输入(Secure Keyboard Entry)已开启，系统会拦截所有注入的键盘事件（鼠标点击不受影响）。请退出正在占用安全输入的应用——常见来源：终端里勾选的“安全键盘输入”、聚焦中的密码输入框、部分密码管理器；必要时注销重新登录，然后重试。".into(),
        };
    }

    NativeStageStatus {
        state: "ready".into(),
        detail: format!(
            "Receiving shared input on QUIC datagrams at UDP {}.",
            normalize_quic_port(layout.transport_port, layout.quic_port)
        ),
    }
}

#[cfg(target_os = "macos")]
fn macos_accessibility_trusted(request_permission: bool) -> bool {
    use core_foundation::{
        base::TCFType, boolean::CFBoolean, dictionary::CFDictionary, string::CFString,
    };

    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn AXIsProcessTrusted() -> bool;
        fn AXIsProcessTrustedWithOptions(
            options: core_foundation::dictionary::CFDictionaryRef,
        ) -> bool;
    }

    if !request_permission || MACOS_ACCESSIBILITY_PROMPTED.swap(true, Ordering::Relaxed) {
        return unsafe { AXIsProcessTrusted() };
    }

    let key = CFString::from_static_string("AXTrustedCheckOptionPrompt");
    let value = CFBoolean::true_value();
    let options = CFDictionary::from_CFType_pairs(&[(key, value)]);
    unsafe { AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef()) }
}

/// Reports whether macOS Secure Keyboard Entry is currently enabled by any
/// process. While it is on, synthetic keyboard events posted via CGEvent are
/// discarded by the window server (mouse events are unaffected).
#[cfg(target_os = "macos")]
fn macos_secure_input_enabled() -> bool {
    #[link(name = "Carbon", kind = "framework")]
    extern "C" {
        // Returns a Carbon `Boolean` (unsigned char); read it as u8 to avoid
        // relying on a non-0/1 value being a valid Rust bool.
        fn IsSecureEventInputEnabled() -> u8;
    }

    unsafe { IsSecureEventInputEnabled() != 0 }
}

fn start_input_capture(
    targets: Vec<InputTarget>,
    layout_state: Arc<Mutex<LayoutState>>,
    native_layout: LayoutState,
    quic_transport: quic_transport::TransportHandle,
    stop: Arc<AtomicBool>,
    remote_active: Arc<AtomicBool>,
    main_window_visible: Arc<AtomicBool>,
    main_window_focused: Arc<AtomicBool>,
    clipboard_target: Arc<Mutex<Option<ClipboardTarget>>>,
    input_events: Arc<AtomicU64>,
    switch_request: Arc<Mutex<Option<SwitchDirection>>>,
) -> NativeStageStatus {
    start_platform_capture(
        targets,
        layout_state,
        native_layout,
        quic_transport,
        stop,
        remote_active,
        main_window_visible,
        main_window_focused,
        clipboard_target,
        input_events,
        switch_request,
    )
}

#[cfg(target_os = "macos")]
fn start_platform_capture(
    targets: Vec<InputTarget>,
    layout_state: Arc<Mutex<LayoutState>>,
    native_layout: LayoutState,
    quic_transport: quic_transport::TransportHandle,
    stop: Arc<AtomicBool>,
    remote_active: Arc<AtomicBool>,
    main_window_visible: Arc<AtomicBool>,
    _main_window_focused: Arc<AtomicBool>,
    clipboard_target: Arc<Mutex<Option<ClipboardTarget>>>,
    input_events: Arc<AtomicU64>,
    switch_request: Arc<Mutex<Option<SwitchDirection>>>,
) -> NativeStageStatus {
    use core_foundation::runloop::{kCFRunLoopCommonModes, kCFRunLoopDefaultMode, CFRunLoop};
    use core_graphics::event::{
        CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventType,
    };

    let (ready_tx, ready_rx) = mpsc::channel();
    let target_count = targets.len();

    thread::spawn(move || {
        let local_y_bounds = local_y_bounds(&targets);
        let display_snapshots = mac_display_snapshots();
        enable_macos_background_cursor_hide();
        let context = Arc::new(MacCaptureContext {
            quic_transport,
            layout_state,
            native_layout,
            active: Mutex::new(None),
            remote_active,
            main_window_visible,
            clipboard_target,
            input_events,
            targets,
            switch_request,
            anchor: Mutex::new(None),
            cursor_hidden: Mutex::new(false),
            cursor_hide_depth: Mutex::new(0),
            last_cursor_hide_reassert: Mutex::new(None),
            last_mouse_move_sent: Mutex::new(None),
            last_cursor_repin: Mutex::new(None),
            last_return: Mutex::new(None),
            remote_button_mask: AtomicU64::new(0),
            pressed_modifiers: Mutex::new(Vec::new()),
            pressed_keys: Mutex::new(Vec::new()),
            tap_disabled: AtomicBool::new(false),
            just_crossed: AtomicBool::new(false),
            suppress_next_mouse_delta: AtomicBool::new(false),
            hotkey_return_point: Mutex::new(None),
            local_screen_points: Mutex::new(HashMap::new()),
            local_y_bounds,
            display_snapshots,
        });
        let callback_context = Arc::clone(&context);
        let event_types = vec![
            CGEventType::MouseMoved,
            CGEventType::LeftMouseDragged,
            CGEventType::RightMouseDragged,
            CGEventType::OtherMouseDragged,
            CGEventType::LeftMouseDown,
            CGEventType::LeftMouseUp,
            CGEventType::RightMouseDown,
            CGEventType::RightMouseUp,
            CGEventType::OtherMouseDown,
            CGEventType::OtherMouseUp,
            CGEventType::ScrollWheel,
            CGEventType::KeyDown,
            CGEventType::KeyUp,
            CGEventType::FlagsChanged,
        ];

        // SAFETY: the tap is created, used, and dropped on this same thread; the
        // callback only borrows `callback_context` (an Arc that outlives the
        // tap), so it never runs after this thread unwinds.
        let tap = match unsafe {
            CGEventTap::new_unchecked(
                CGEventTapLocation::HID,
                CGEventTapPlacement::HeadInsertEventTap,
                CGEventTapOptions::Default,
                event_types,
                move |_proxy, event_type, event| {
                    handle_macos_event(&callback_context, event_type, event)
                },
            )
        } {
            Ok(tap) => tap,
            Err(_) => {
                let _ = ready_tx.send(Err(
                    "macOS 生产包需要单独授权辅助功能和输入监控。请到 系统设置 > 隐私与安全性 > 辅助功能 / 输入监控 启用 MyKVM，然后完全退出并重新打开应用。".into(),
                ));
                return;
            }
        };

        let loop_source = match tap.mach_port().create_runloop_source(0) {
            Ok(source) => source,
            Err(_) => {
                let _ = ready_tx.send(Err("failed to attach macOS event tap to run loop".into()));
                return;
            }
        };
        CFRunLoop::get_current().add_source(&loop_source, unsafe { kCFRunLoopCommonModes });
        let mut raw_gesture_taps = Vec::new();
        let mut _raw_gesture_loop_sources = Vec::new();
        for location in [CGEventTapLocation::HID, CGEventTapLocation::Session] {
            match RawMacosGestureTap::new(location, Arc::clone(&context)) {
                Ok(raw_tap) => match raw_tap.mach_port().create_runloop_source(0) {
                    Ok(source) => {
                        CFRunLoop::get_current()
                            .add_source(&source, unsafe { kCFRunLoopCommonModes });
                        raw_tap.enable();
                        _raw_gesture_loop_sources.push(source);
                        raw_gesture_taps.push(raw_tap);
                    }
                    Err(_) => {
                        log::warn!(
                            "failed to attach raw macOS gesture event tap {:?} to run loop",
                            location
                        );
                    }
                },
                Err(_) => {
                    log::warn!(
                        "failed to create raw macOS gesture event tap {:?}",
                        location
                    );
                }
            }
        }
        tap.enable();
        let _ = ready_tx.send(Ok(()));
        let mut app_nap_suppressed = false;

        while !stop.load(Ordering::Relaxed) {
            let was_remote_active = context.remote_active.load(Ordering::Relaxed);
            if app_nap_suppressed != was_remote_active {
                set_macos_app_nap_suppressed(was_remote_active);
                app_nap_suppressed = was_remote_active;
            }
            let _ = CFRunLoop::run_in_mode(
                unsafe { kCFRunLoopDefaultMode },
                Duration::from_millis(macos_capture_loop_ms(
                    was_remote_active,
                    context.main_window_visible.load(Ordering::Relaxed),
                )),
                false,
            );
            drain_switch_request_macos(&context);
            // macOS disables a tap whose callback ran too long or that idled out.
            // Without re-enabling it the mouse and keyboard silently freeze until
            // the app restarts, which is the classic "works, then sticks after a
            // while" failure. Re-arm it as soon as we notice.
            if context.tap_disabled.swap(false, Ordering::Relaxed) {
                tap.enable();
                for raw_tap in &raw_gesture_taps {
                    raw_tap.enable();
                }
                log::debug!("[diag] event tap re-enabled after being disabled");
            }
            // While controlling a remote, macOS can re-associate the physical
            // mouse with the local cursor (especially when backgrounded),
            // making the server pointer reappear and follow the mouse.
            // Re-pin it to the anchor and re-assert hide while active.
            let is_remote_active = context.remote_active.load(Ordering::Relaxed);
            if app_nap_suppressed != is_remote_active {
                set_macos_app_nap_suppressed(is_remote_active);
                app_nap_suppressed = is_remote_active;
            }
            if is_remote_active {
                repin_macos_cursor_while_remote(&context);
            }
        }

        // Critical safety: never leave the cursor decoupled after capture stops,
        // otherwise the user's mouse stays frozen until the app restarts.
        set_macos_cursor_decoupled(false);
        set_macos_warp_suppression_interval(MACOS_DEFAULT_WARP_SUPPRESSION_SECS);
        show_macos_cursor_if_needed(&context);
        if app_nap_suppressed {
            set_macos_app_nap_suppressed(false);
        }
        context.remote_active.store(false, Ordering::Relaxed);
        clear_clipboard_target(&context.clipboard_target);
    });

    match ready_rx.recv_timeout(Duration::from_secs(1)) {
        Ok(Ok(())) => NativeStageStatus {
            state: "ready".into(),
            detail: format!("控制端已就绪，{target_count} 条远端贴边可用于鼠标和键盘切换。"),
        },
        Ok(Err(error)) => NativeStageStatus {
            state: "error".into(),
            detail: error,
        },
        Err(_) => NativeStageStatus {
            state: "error".into(),
            detail: "macOS input capture did not become ready.".into(),
        },
    }
}

#[cfg(target_os = "windows")]
fn start_platform_capture(
    targets: Vec<InputTarget>,
    layout_state: Arc<Mutex<LayoutState>>,
    native_layout: LayoutState,
    quic_transport: quic_transport::TransportHandle,
    stop: Arc<AtomicBool>,
    remote_active: Arc<AtomicBool>,
    _main_window_visible: Arc<AtomicBool>,
    main_window_focused: Arc<AtomicBool>,
    clipboard_target: Arc<Mutex<Option<ClipboardTarget>>>,
    input_events: Arc<AtomicU64>,
    switch_request: Arc<Mutex<Option<SwitchDirection>>>,
) -> NativeStageStatus {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        PeekMessageW, SetWindowsHookExW, UnhookWindowsHookEx, MSG, PM_REMOVE, WH_KEYBOARD_LL,
        WH_MOUSE_LL,
    };

    let target_count = targets.len();
    let (ready_tx, ready_rx) = mpsc::channel();

    thread::spawn(move || {
        refresh_windows_input_desktop_cache();
        let context = Arc::new(WindowsCaptureContext {
            quic_transport,
            layout_state,
            native_layout,
            active: Mutex::new(None),
            remote_active,
            main_window_focused,
            clipboard_target,
            input_events,
            targets,
            switch_request,
            anchor: Mutex::new(None),
            last_point: Mutex::new(None),
            last_mouse_move_sent: Mutex::new(None),
            remote_button_mask: AtomicU64::new(0),
            pressed_keys: Mutex::new(Vec::new()),
            cursor_hide_calls: Mutex::new(0),
            just_crossed: AtomicBool::new(false),
            local_screen_points: Mutex::new(HashMap::new()),
        });

        if let Ok(mut current) = WINDOWS_CAPTURE_CONTEXT.lock() {
            *current = Some(Arc::clone(&context));
        }

        let mouse_hook = unsafe {
            SetWindowsHookExW(
                WH_MOUSE_LL,
                Some(windows_mouse_proc),
                std::ptr::null_mut(),
                0,
            )
        };
        if mouse_hook.is_null() {
            context.remote_active.store(false, Ordering::Relaxed);
            clear_clipboard_target(&context.clipboard_target);
            clear_windows_capture_context();
            let _ = ready_tx.send(Err("failed to install Windows mouse hook".into()));
            return;
        }

        let keyboard_hook = unsafe {
            SetWindowsHookExW(
                WH_KEYBOARD_LL,
                Some(windows_keyboard_proc),
                std::ptr::null_mut(),
                0,
            )
        };
        if keyboard_hook.is_null() {
            unsafe {
                let _ = UnhookWindowsHookEx(mouse_hook);
            }
            context.remote_active.store(false, Ordering::Relaxed);
            clear_clipboard_target(&context.clipboard_target);
            clear_windows_capture_context();
            let _ = ready_tx.send(Err("failed to install Windows keyboard hook".into()));
            return;
        }

        let _ = ready_tx.send(Ok(()));
        let mut message = MSG::default();
        let mut last_desktop_check = Instant::now() - Duration::from_millis(200);
        while !stop.load(Ordering::Relaxed) {
            if last_desktop_check.elapsed() >= Duration::from_millis(100) {
                last_desktop_check = Instant::now();
                if !refresh_windows_input_desktop_cache() {
                    release_windows_remote_control(&context, true);
                }
            }
            drain_switch_request_windows(&context);
            unsafe {
                while PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {}
            }
            thread::sleep(Duration::from_millis(10));
        }

        unsafe {
            let _ = UnhookWindowsHookEx(mouse_hook);
            let _ = UnhookWindowsHookEx(keyboard_hook);
        }
        show_windows_cursor_if_needed(&context);
        context.remote_active.store(false, Ordering::Relaxed);
        clear_clipboard_target(&context.clipboard_target);
        clear_windows_capture_context();
    });

    match ready_rx.recv_timeout(Duration::from_secs(1)) {
        Ok(Ok(())) => NativeStageStatus {
            state: "ready".into(),
            detail: format!("控制端已就绪，{target_count} 条远端贴边可用于鼠标和键盘切换。"),
        },
        Ok(Err(error)) => NativeStageStatus {
            state: "error".into(),
            detail: error,
        },
        Err(_) => NativeStageStatus {
            state: "error".into(),
            detail: "Windows input capture did not become ready.".into(),
        },
    }
}

#[cfg(not(target_os = "macos"))]
#[cfg(not(target_os = "windows"))]
fn start_platform_capture(
    _targets: Vec<InputTarget>,
    _layout_state: Arc<Mutex<LayoutState>>,
    _native_layout: LayoutState,
    _quic_transport: quic_transport::TransportHandle,
    _stop: Arc<AtomicBool>,
    remote_active: Arc<AtomicBool>,
    _main_window_visible: Arc<AtomicBool>,
    _main_window_focused: Arc<AtomicBool>,
    clipboard_target: Arc<Mutex<Option<ClipboardTarget>>>,
    _input_events: Arc<AtomicU64>,
    _switch_request: Arc<Mutex<Option<SwitchDirection>>>,
) -> NativeStageStatus {
    remote_active.store(false, Ordering::Relaxed);
    clear_clipboard_target(&clipboard_target);
    unsupported_capture_status()
}

#[cfg(target_os = "windows")]
fn start_platform_receive_monitor(stop: Arc<AtomicBool>) {
    thread::spawn(move || {
        refresh_windows_input_desktop_cache();
        while !stop.load(Ordering::Relaxed) {
            refresh_windows_input_desktop_cache();
            thread::sleep(Duration::from_millis(WINDOWS_DESKTOP_CHECK_INTERVAL_MS));
        }
    });
}

#[cfg(not(target_os = "windows"))]
fn start_platform_receive_monitor(_stop: Arc<AtomicBool>) {}

fn no_target_status(layout: &LayoutState) -> NativeStageStatus {
    let remote_count = layout
        .devices
        .iter()
        .filter(|device| device.role != "local")
        .count();
    let online_remote_count = layout
        .devices
        .iter()
        .filter(|device| device.role != "local" && device.online)
        .count();
    let detail = if remote_count == 0 {
        "控制模式已开启，但布局里还没有远端设备。先让对方电脑运行 mykvm，再在 LAN devices 里 Scan 并 Add。"
    } else if online_remote_count == 0 {
        "控制模式已开启，但远端设备都被标记为离线。把要控制的设备切回 online 后再启动运行时。"
    } else {
        "控制模式已开启，且已有在线远端设备，但屏幕还没有和本机贴边。拖动远端显示器贴住本机边缘后才会生成切屏目标。"
    };

    NativeStageStatus {
        state: "idle".into(),
        detail: detail.into(),
    }
}

fn receive_only_status() -> NativeStageStatus {
    NativeStageStatus {
        state: "idle".into(),
        detail: "当前是仅接收模式：会接收远端输入，但不会捕获本机鼠标和键盘。".into(),
    }
}

fn unsupported_capture_status() -> NativeStageStatus {
    NativeStageStatus {
        state: "stubbed".into(),
        detail: "Global input capture is not implemented on this platform.".into(),
    }
}

fn build_input_targets(layout: &LayoutState, native_layout: &LayoutState) -> Vec<InputTarget> {
    let Some(local_device) = layout.devices.iter().find(|device| device.role == "local") else {
        return Vec::new();
    };
    let native_device = native_layout
        .devices
        .iter()
        .find(|device| device.role == "local")
        .or_else(|| native_layout.devices.first());

    let local_screens = &local_device.screens;
    let origin_device_id = crate::local_peer_from_layout(layout).id;
    let mut targets = Vec::new();

    for device in layout.devices.iter().filter(|device| {
        device.role != "local"
            && device.online
            && device.input_ready
            && device.protocol_version == quic_transport::PROTOCOL_VERSION
            && !device.transport_public_key.trim().is_empty()
    }) {
        let quic_port = normalize_quic_port(device.transport_port, device.quic_port);
        for layout_local_screen in local_screens {
            let native_local_screen = native_device
                .and_then(|device| {
                    device
                        .screens
                        .iter()
                        .find(|screen| screen.id == layout_local_screen.id)
                })
                .unwrap_or(layout_local_screen);
            let native_local_screen = platform_native_screen(native_local_screen);

            for remote_screen in &device.screens {
                if screens_overlap(layout_local_screen, remote_screen) {
                    continue;
                }

                if let Some(edge) = touching_edge(layout_local_screen, remote_screen) {
                    targets.push(InputTarget {
                        device_id: device.id.clone(),
                        origin_device_id: origin_device_id.clone(),
                        cluster_id: layout.cluster_id.clone(),
                        pair_secret: layout.pair_secret.clone(),
                        target_addr: format!("{}:{}", device.host, quic_port),
                        target_platform: device.platform.clone(),
                        transport_public_key: device.transport_public_key.clone(),
                        protocol_version: device.protocol_version,
                        screen_id: peer_screen_id(device, remote_screen),
                        local_screen: native_local_screen.clone(),
                        layout_local_screen: layout_local_screen.clone(),
                        remote_screen: remote_screen.clone(),
                        edge,
                    });
                }
            }
        }
    }

    targets
}

fn current_input_targets(
    layout_state: &Arc<Mutex<LayoutState>>,
    native_layout: &LayoutState,
) -> Vec<InputTarget> {
    layout_state
        .lock()
        .map(|layout| build_input_targets(&layout, native_layout))
        .unwrap_or_default()
}

fn touching_edge(local: &Screen, remote: &Screen) -> Option<Edge> {
    if near(local.x + local.width, remote.x)
        && ranges_overlap(
            local.y,
            local.y + local.height,
            remote.y,
            remote.y + remote.height,
        )
    {
        return Some(Edge::Right);
    }

    if near(local.x, remote.x + remote.width)
        && ranges_overlap(
            local.y,
            local.y + local.height,
            remote.y,
            remote.y + remote.height,
        )
    {
        return Some(Edge::Left);
    }

    if near(local.y + local.height, remote.y)
        && ranges_overlap(
            local.x,
            local.x + local.width,
            remote.x,
            remote.x + remote.width,
        )
    {
        return Some(Edge::Bottom);
    }

    if near(local.y, remote.y + remote.height)
        && ranges_overlap(
            local.x,
            local.x + local.width,
            remote.x,
            remote.x + remote.width,
        )
    {
        return Some(Edge::Top);
    }

    None
}

fn screens_overlap(local: &Screen, remote: &Screen) -> bool {
    local.x < remote.x + remote.width
        && local.x + local.width > remote.x
        && local.y < remote.y + remote.height
        && local.y + local.height > remote.y
}

fn near(a: i32, b: i32) -> bool {
    (a - b).abs() <= EDGE_TOLERANCE
}

fn ranges_overlap(a_start: i32, a_end: i32, b_start: i32, b_end: i32) -> bool {
    i32::min(a_end, b_end) - i32::max(a_start, b_start) > EDGE_TOLERANCE
}

fn peer_screen_id(device: &Device, screen: &Screen) -> String {
    screen
        .id
        .strip_prefix(&format!("{}-", device.id))
        .unwrap_or(&screen.id)
        .to_string()
}

fn send_packet(
    quic_transport: &quic_transport::TransportHandle,
    target: &InputTarget,
    event: InputEvent,
    layout_state: &Arc<Mutex<LayoutState>>,
    input_events: &Arc<AtomicU64>,
) -> bool {
    let packet_context = input_packet_context(target, event, layout_state);
    let event = packet_context.event;
    let packet = InputPacket {
        protocol: INPUT_PROTOCOL.into(),
        target_device_id: target.device_id.clone(),
        origin_device_id: packet_context.origin_device_id,
        origin_port: quic_transport.port(),
        origin_transport_public_key: quic_transport.public_key().to_string(),
        origin_protocol_version: quic_transport::PROTOCOL_VERSION,
        cluster_id: packet_context.cluster_id,
        pair_secret: packet_context.pair_secret,
        event,
    };
    let Some(peer) = packet_context.peer else {
        return false;
    };

    let payload = match rmp_serde::to_vec_named(&packet) {
        Ok(payload) => payload,
        Err(error) => {
            log::warn!(
                "input tx encode failed target={} error={}",
                peer.addr,
                error
            );
            return false;
        }
    };

    match quic_transport.send_datagram(peer, payload) {
        Ok(()) => {
            input_events.fetch_add(1, Ordering::Relaxed);
            true
        }
        Err(error) => {
            mark_target_offline(layout_state, target, &error);
            false
        }
    }
}

pub fn send_secure_attention_control(
    layout: &LayoutState,
    quic_transport: &quic_transport::TransportHandle,
    device_id: &str,
) -> Result<(), String> {
    let Some(target) = layout
        .devices
        .iter()
        .find(|device| device.id == device_id && device.role != "local")
    else {
        return Err("target device is not in the layout".into());
    };
    if target.platform != "windows" {
        return Err("Ctrl+Alt+Del control is only available for Windows targets.".into());
    }
    if !target.online || !target.input_ready {
        return Err("target device is not online and input-ready".into());
    }
    if target.transport_public_key.trim().is_empty() {
        return Err("target device has no QUIC transport key; re-pair it first".into());
    }
    if layout.cluster_id.trim().is_empty() || layout.pair_secret.trim().is_empty() {
        return Err("this device is not paired with the target".into());
    }

    let origin_device_id = origin_peer_id(layout);
    let packet = InputControlPacket {
        protocol: INPUT_CONTROL_PROTOCOL.into(),
        target_device_id: target.id.clone(),
        origin_device_id,
        origin_transport_public_key: quic_transport.public_key().to_string(),
        origin_protocol_version: quic_transport::PROTOCOL_VERSION,
        cluster_id: layout.cluster_id.clone(),
        pair_secret: layout.pair_secret.clone(),
        command: InputControlCommand::SecureAttention,
    };
    let payload = rmp_serde::to_vec_named(&packet)
        .map_err(|error| format!("encode input control packet: {error}"))?;
    let peer = quic_transport.peer(
        format!(
            "{}:{}",
            target.host,
            normalize_quic_port(target.transport_port, target.quic_port)
        ),
        target.transport_public_key.clone(),
        target.protocol_version,
    );

    quic_transport.send_datagram(peer, payload)
}

struct InputPacketContext {
    origin_device_id: String,
    cluster_id: String,
    pair_secret: String,
    peer: Option<quic_transport::PeerEndpoint>,
    event: InputEvent,
}

fn input_packet_context(
    target: &InputTarget,
    event: InputEvent,
    layout_state: &Arc<Mutex<LayoutState>>,
) -> InputPacketContext {
    let fallback_peer = || quic_transport::PeerEndpoint {
        addr: target.target_addr.clone(),
        public_key: target.transport_public_key.clone(),
        protocol_version: target.protocol_version,
    };

    let fallback_context = |event| InputPacketContext {
        origin_device_id: target.origin_device_id.clone(),
        cluster_id: target.cluster_id.clone(),
        pair_secret: target.pair_secret.clone(),
        peer: Some(fallback_peer()),
        event,
    };

    let layout = match layout_state.try_lock() {
        Ok(layout) => layout,
        Err(TryLockError::WouldBlock) => return fallback_context(event),
        Err(TryLockError::Poisoned(_)) => return fallback_context(event),
    };

    let origin_device_id = origin_peer_id(&layout);
    let peer = layout
        .devices
        .iter()
        .find(|device| device.id == target.device_id)
        .and_then(|device| {
            (device.online && device.input_ready).then(|| quic_transport::PeerEndpoint {
                addr: format!(
                    "{}:{}",
                    device.host,
                    normalize_quic_port(device.transport_port, device.quic_port)
                ),
                public_key: device.transport_public_key.clone(),
                protocol_version: device.protocol_version,
            })
        });
    let event = remap_event_for_target_layout(event, target, &layout);

    InputPacketContext {
        origin_device_id,
        cluster_id: layout.cluster_id.clone(),
        pair_secret: layout.pair_secret.clone(),
        peer,
        event,
    }
}

/// Rewrites modifier keys on key events when the controlling machine and the
/// target run different operating systems, so platform shortcut conventions
/// line up (default: Ctrl <-> Cmd). Non-key events and same-platform targets
/// pass through untouched. The wire format is always Windows virtual-key codes.
fn remap_event_for_target_layout(
    event: InputEvent,
    target: &InputTarget,
    layout: &LayoutState,
) -> InputEvent {
    let InputEvent::Key { key_code, down } = event else {
        return event;
    };

    let target_platform = target.target_platform.as_str();
    if target_platform != "macos" && target_platform != "windows" {
        return InputEvent::Key { key_code, down };
    }
    if target_platform == crate::current_platform() {
        return InputEvent::Key { key_code, down };
    }

    let remapped = if layout.modifier_remap {
        remap_modifier_vk(
            key_code,
            &layout.modifier_map.control,
            &layout.modifier_map.alt,
            &layout.modifier_map.meta,
        )
    } else {
        key_code
    };

    InputEvent::Key {
        key_code: remapped,
        down,
    }
}

#[cfg(test)]
fn remap_event_for_target(
    event: InputEvent,
    target: &InputTarget,
    layout_state: &Arc<Mutex<LayoutState>>,
) -> InputEvent {
    match layout_state.lock() {
        Ok(layout) => remap_event_for_target_layout(event, target, &layout),
        Err(_) => event,
    }
}

/// Classifies a Windows virtual-key code into a logical modifier group:
/// 0 = Control, 1 = Alt, 2 = Meta (Windows key / macOS Command).
fn classify_modifier_vk(vk: u16) -> Option<u8> {
    match vk {
        0x11 | 0xA2 | 0xA3 => Some(0),
        0x12 | 0xA4 | 0xA5 => Some(1),
        0x5B | 0x5C => Some(2),
        _ => None,
    }
}

/// Resolves a configured logical target to its canonical Windows virtual-key
/// code. "same" (or any unknown value) returns None so the original key, with
/// its left/right distinction, is preserved.
fn logical_target_vk(target: &str) -> Option<u16> {
    match target {
        "control" => Some(0x11),
        "alt" => Some(0x12),
        "meta" => Some(0x5B),
        _ => None,
    }
}

fn remap_modifier_vk(vk: u16, control: &str, alt: &str, meta: &str) -> u16 {
    let target = match classify_modifier_vk(vk) {
        Some(0) => control,
        Some(1) => alt,
        Some(2) => meta,
        _ => return vk,
    };
    logical_target_vk(target).unwrap_or(vk)
}

fn mark_target_offline(
    layout_state: &Arc<Mutex<LayoutState>>,
    target: &InputTarget,
    _reason: &str,
) {
    let Ok(mut layout) = layout_state.lock() else {
        return;
    };
    let Some(device) = layout
        .devices
        .iter_mut()
        .find(|device| device.id == target.device_id)
    else {
        return;
    };
    if !device.online {
        return;
    }

    device.online = false;
}

fn target_is_online(target: &InputTarget, layout_state: &Arc<Mutex<LayoutState>>) -> bool {
    layout_state
        .lock()
        .ok()
        .and_then(|layout| {
            layout
                .devices
                .iter()
                .find(|device| device.id == target.device_id)
                .map(|device| device.online && device.input_ready)
        })
        .unwrap_or(false)
}

pub fn try_inject_packet_from_source(
    layout: &LayoutState,
    native_layout: &LayoutState,
    payload: &[u8],
    source: SocketAddr,
    input_events: &Arc<AtomicU64>,
    local_peer_id: &str,
    clipboard_target: &Arc<Mutex<Option<ClipboardTarget>>>,
) -> bool {
    let Some(packet) = decode_input_packet(payload) else {
        return false;
    };

    if packet.protocol != INPUT_PROTOCOL {
        return false;
    }

    if !packet_authorized(layout, &packet) {
        warn_unauthorized_packet(layout, &packet);
        return true;
    }

    if !packet_targets_local(layout, &packet.target_device_id, local_peer_id) {
        return true;
    }

    if packet.origin_port != 0 && !packet.origin_transport_public_key.trim().is_empty() {
        let device_id = if packet.origin_device_id.trim().is_empty() {
            source.ip().to_string()
        } else {
            packet.origin_device_id.clone()
        };
        // Persist the controller as our clipboard peer so a copy made on this
        // machine syncs back to it immediately, without needing the remote
        // cursor to re-enter. Refreshed on every input packet; cleared when
        // input/clipboard stops.
        set_clipboard_target(
            clipboard_target,
            device_id,
            format!("{}:{}", source.ip(), packet.origin_port),
            packet.origin_transport_public_key.clone(),
            packet.origin_protocol_version,
            layout.cluster_id.clone(),
            layout.pair_secret.clone(),
            None,
        );
    }

    let injected = inject_input_event(layout, native_layout, packet.event);
    if injected {
        input_events.fetch_add(1, Ordering::Relaxed);
    }

    true
}

pub fn try_handle_control_packet_from_source(
    layout: &LayoutState,
    payload: &[u8],
    source: SocketAddr,
    local_peer_id: &str,
) -> bool {
    let Some(packet) = decode_input_control_packet(payload) else {
        return false;
    };

    if packet.protocol != INPUT_CONTROL_PROTOCOL {
        return false;
    }

    if !control_packet_authorized(layout, &packet) {
        warn_unauthorized_control_packet(layout, &packet);
        return true;
    }

    if !packet_targets_local(layout, &packet.target_device_id, local_peer_id) {
        return true;
    }

    match packet.command {
        InputControlCommand::SecureAttention => {
            #[cfg(target_os = "windows")]
            if let Err(error) = send_secure_attention_to_helper() {
                log::warn!(
                    "SecureAttention control from {} could not reach input service: {}",
                    source,
                    error
                );
            }

            #[cfg(not(target_os = "windows"))]
            log::warn!(
                "SecureAttention control from {} ignored on non-Windows target",
                source
            );
        }
    }

    true
}

fn packet_authorized(layout: &LayoutState, packet: &InputPacket) -> bool {
    packet_authorized_fields(
        layout,
        &packet.cluster_id,
        &packet.pair_secret,
        &packet.origin_transport_public_key,
        &packet.origin_device_id,
    )
}

fn control_packet_authorized(layout: &LayoutState, packet: &InputControlPacket) -> bool {
    packet_authorized_fields(
        layout,
        &packet.cluster_id,
        &packet.pair_secret,
        &packet.origin_transport_public_key,
        &packet.origin_device_id,
    )
}

fn packet_authorized_fields(
    layout: &LayoutState,
    cluster_id: &str,
    pair_secret: &str,
    origin_transport_public_key: &str,
    origin_device_id: &str,
) -> bool {
    if layout.cluster_id.trim().is_empty() || layout.pair_secret.trim().is_empty() {
        return false;
    }
    if cluster_id != layout.cluster_id || pair_secret != layout.pair_secret {
        return false;
    }

    if layout.paired_controllers.iter().any(|controller| {
        (!origin_transport_public_key.trim().is_empty()
            && controller.transport_public_key == origin_transport_public_key)
            || (!origin_device_id.trim().is_empty() && controller.id == origin_device_id)
    }) {
        return true;
    }

    legacy_local_device_origin_allowed(layout, origin_device_id, origin_transport_public_key)
}

fn legacy_local_device_origin_allowed(
    layout: &LayoutState,
    origin_device_id: &str,
    origin_transport_public_key: &str,
) -> bool {
    layout.machine_role == "client"
        && layout.paired_controllers.len() == 1
        && origin_device_id == "local-device"
        && !origin_transport_public_key.trim().is_empty()
}

fn origin_peer_id(layout: &LayoutState) -> String {
    crate::local_peer_from_layout(layout).id
}

static LAST_UNAUTHORIZED_WARN: OnceLock<Mutex<Instant>> = OnceLock::new();

/// Log (at most once every few seconds, since a single mouse move floods many
/// packets) why a controller's input was rejected. Without this the packets
/// were dropped silently while the device still showed "online", which makes a
/// pairing-credential mismatch impossible to diagnose — exactly the "shows
/// online but the cursor can't cross" trap.
fn warn_unauthorized_packet(layout: &LayoutState, packet: &InputPacket) {
    let reason = if layout.cluster_id.trim().is_empty() || layout.pair_secret.trim().is_empty() {
        "this device has no pairing configured (empty cluster/secret) — pair it with the controller"
    } else if packet.cluster_id != layout.cluster_id || packet.pair_secret != layout.pair_secret {
        "pairing secret/cluster mismatch — controller and this device are not paired with the same credentials; re-pair them (removing/re-adding the device does NOT re-pair)"
    } else {
        "controller is not in this device's paired-controllers list (likely a rotated transport key) — re-pair"
    };

    let cell =
        LAST_UNAUTHORIZED_WARN.get_or_init(|| Mutex::new(Instant::now() - Duration::from_secs(60)));
    if let Ok(mut last) = cell.lock() {
        if last.elapsed() < Duration::from_secs(3) {
            return;
        }
        *last = Instant::now();
    }

    log::warn!(
        "rejected input from controller id={} key={}: {}",
        if packet.origin_device_id.trim().is_empty() {
            "<none>"
        } else {
            packet.origin_device_id.as_str()
        },
        if packet.origin_transport_public_key.trim().is_empty() {
            "<none>"
        } else {
            "<set>"
        },
        reason
    );
}

fn warn_unauthorized_control_packet(layout: &LayoutState, packet: &InputControlPacket) {
    let reason = if layout.cluster_id.trim().is_empty() || layout.pair_secret.trim().is_empty() {
        "this device has no pairing configured"
    } else if packet.cluster_id != layout.cluster_id || packet.pair_secret != layout.pair_secret {
        "pairing secret/cluster mismatch"
    } else {
        "controller is not in this device's paired-controllers list"
    };

    log::warn!(
        "rejected input control from controller id={} key={}: {}",
        if packet.origin_device_id.trim().is_empty() {
            "<none>"
        } else {
            packet.origin_device_id.as_str()
        },
        if packet.origin_transport_public_key.trim().is_empty() {
            "<none>"
        } else {
            "<set>"
        },
        reason
    );
}

fn packet_targets_local(layout: &LayoutState, target_device_id: &str, local_peer_id: &str) -> bool {
    if target_device_id.trim().is_empty() {
        return true;
    }
    if target_device_id == local_peer_id {
        return true;
    }

    layout
        .devices
        .iter()
        .any(|device| device.role == "local" && device.id == target_device_id)
}

fn decode_input_packet(payload: &[u8]) -> Option<InputPacket> {
    rmp_serde::from_slice::<InputPacket>(payload).ok()
}

fn decode_input_control_packet(payload: &[u8]) -> Option<InputControlPacket> {
    rmp_serde::from_slice::<InputControlPacket>(payload).ok()
}

fn default_protocol_version() -> u16 {
    quic_transport::PROTOCOL_VERSION
}

fn normalize_quic_port(transport_port: u16, quic_port: u16) -> u16 {
    if quic_port == 0 {
        transport_port
    } else {
        quic_port
    }
}

fn local_device(layout: &LayoutState) -> Option<&Device> {
    layout
        .devices
        .iter()
        .find(|device| device.role == "local")
        .or_else(|| layout.devices.first())
}

fn local_screen_for_event<'a>(layout: &'a LayoutState, screen_id: &str) -> Option<&'a Screen> {
    let device = local_device(layout)?;
    device
        .screens
        .iter()
        .find(|screen| screen.id == screen_id)
        .or_else(|| device.screens.iter().find(|screen| screen.is_primary))
        .or_else(|| device.screens.first())
}

fn map_relative_to_native_axis(
    relative: i32,
    logical_size: i32,
    native_start: i32,
    native_size: i32,
) -> i32 {
    let ratio = relative as f64 / logical_size.max(1) as f64;
    (native_start as f64 + ratio * native_size.max(1) as f64).round() as i32
}

#[cfg(target_os = "windows")]
fn platform_native_screen(screen: &Screen) -> Screen {
    let scale = if screen.scale.is_finite() && screen.scale > 0.0 {
        screen.scale
    } else {
        1.0
    };

    Screen {
        x: scale_position(screen.x, scale),
        y: scale_position(screen.y, scale),
        width: scale_size(screen.width, scale),
        height: scale_size(screen.height, scale),
        ..screen.clone()
    }
}

#[cfg(not(target_os = "windows"))]
fn platform_native_screen(screen: &Screen) -> Screen {
    screen.clone()
}

#[cfg(target_os = "windows")]
fn scale_position(value: i32, scale: f64) -> i32 {
    (value as f64 * scale)
        .round()
        .clamp(i32::MIN as f64, i32::MAX as f64) as i32
}

#[cfg(target_os = "windows")]
fn scale_size(value: i32, scale: f64) -> i32 {
    (value.max(1) as f64 * scale)
        .round()
        .clamp(1.0, i32::MAX as f64) as i32
}

fn remote_mouse_state() -> &'static Mutex<RemoteMouseState> {
    REMOTE_MOUSE_STATE.get_or_init(|| Mutex::new(RemoteMouseState::default()))
}

fn update_remote_mouse_position(x: i32, y: i32) -> Option<MouseButton> {
    let Ok(mut state) = remote_mouse_state().lock() else {
        return None;
    };
    state.x = x;
    state.y = y;
    primary_button_from_mask(state.buttons)
}

fn update_remote_mouse_button(button: MouseButton, down: bool) -> (i32, i32) {
    let Ok(mut state) = remote_mouse_state().lock() else {
        return (0, 0);
    };
    if down {
        state.buttons |= mouse_button_mask(button);
    } else {
        state.buttons &= !mouse_button_mask(button);
    }
    (state.x, state.y)
}

fn primary_button_from_mask(mask: u64) -> Option<MouseButton> {
    button_from_mask(mask)
}

fn inject_input_event(
    layout: &LayoutState,
    native_layout: &LayoutState,
    event: InputEvent,
) -> bool {
    let Some(command) = input_event_to_command(layout, native_layout, event) else {
        return false;
    };

    #[cfg(target_os = "windows")]
    {
        // Inject locally on the normal desktop; hand off to the privileged SYSTEM
        // helper only for the secure desktop (lock screen / UAC) or Ctrl+Alt+Del.
        //
        // The helper is REQUIRED on the secure desktop — the user-mode app has no
        // access to the Winlogon desktop — but it must NOT be used on the normal
        // desktop: the helper's worker runs as SYSTEM, and Windows rejects a
        // SYSTEM-integrity process's synthetic button/key events with
        // ERROR_ACCESS_DENIED when the foreground window is a normal
        // Medium-integrity app (cursor MOVE still lands because it only
        // repositions the window-station-global cursor). That is the "cursor
        // slides but can't click or type" symptom. Local injection runs as the
        // logged-in user at the foreground window's own integrity, so it clicks
        // and types normally. On the secure desktop the foreground is LogonUI
        // (System integrity), so the worker's equal-integrity injection works.
        if should_route_to_windows_helper(&command) {
            match windows_pipe_dispatcher().send(&command) {
                Ok(()) => return true,
                Err(error) => note_windows_helper_unavailable(&error),
            }
        }
        inject_input_command(command);
        return true;
    }

    #[cfg(not(target_os = "windows"))]
    {
        inject_input_command(command);
        true
    }
}

/// Logs (at most once every 10s, since a single mouse move floods many packets)
/// that the privileged input helper could not be reached, so injection fell back
/// to the user-mode path. On the normal desktop the local fallback works; on the
/// secure desktop (lock screen / UAC) it cannot deliver clicks or keystrokes, so
/// this is the breadcrumb that explains a dead lock screen.
#[cfg(target_os = "windows")]
fn note_windows_helper_unavailable(error: &str) {
    static LAST_WARN: OnceLock<Mutex<Instant>> = OnceLock::new();
    let cell = LAST_WARN.get_or_init(|| Mutex::new(Instant::now() - Duration::from_secs(60)));
    if let Ok(mut last) = cell.lock() {
        if last.elapsed() < Duration::from_secs(10) {
            return;
        }
        *last = Instant::now();
    }
    log::info!(
        "input helper unavailable ({error}); injecting locally. Lock-screen / UAC \
         input needs the MyKVM input service — install it from Settings if clicks \
         and keys stop working while the screen is locked."
    );
}

#[cfg(target_os = "windows")]
fn should_route_to_windows_helper(command: &InputCommand) -> bool {
    // SecureAttention (Ctrl+Alt+Del) always needs the privileged helper —
    // SendSAS requires SYSTEM context and cannot be issued from the user app.
    if matches!(command, InputCommand::SecureAttention) {
        return true;
    }
    // Otherwise only the secure desktop (lock screen / UAC) needs the helper.
    // On the normal "Default" desktop we inject locally as the logged-in user,
    // which is the only path that can click/type into Medium-integrity windows
    // (the SYSTEM helper is denied there with ERROR_ACCESS_DENIED).
    !windows_inject_desktop_is_default()
}

/// Cached check of whether the current input desktop is "Default", for the
/// inject path. Probing `OpenInputDesktop` from the mouse/datagram hot path is
/// expensive enough to show up as periodic dropped frames, so capture/receive
/// monitor threads refresh this cache out of band.
#[cfg(target_os = "windows")]
fn windows_inject_desktop_is_default() -> bool {
    cached_windows_input_desktop_is_default()
}

fn input_event_to_command(
    layout: &LayoutState,
    native_layout: &LayoutState,
    event: InputEvent,
) -> Option<InputCommand> {
    match event {
        InputEvent::MouseMove { screen_id, x, y } => {
            if let Some(screen) = local_screen_for_event(layout, &screen_id) {
                let native_screen = local_screen_for_event(native_layout, &screen_id)
                    .map(platform_native_screen)
                    .unwrap_or_else(|| platform_native_screen(screen));
                let absolute_x = map_relative_to_native_axis(
                    x,
                    screen.width,
                    native_screen.x,
                    native_screen.width,
                );
                let absolute_y = map_relative_to_native_axis(
                    y,
                    screen.height,
                    native_screen.y,
                    native_screen.height,
                );
                let drag_button = update_remote_mouse_position(absolute_x, absolute_y);
                return Some(InputCommand::MouseMove {
                    x: absolute_x,
                    y: absolute_y,
                    drag_button,
                });
            }
            None
        }
        InputEvent::MouseButton { button, down } => {
            let (x, y) = update_remote_mouse_button(button, down);
            Some(InputCommand::MouseButton { button, down, x, y })
        }
        InputEvent::Scroll { delta_x, delta_y } => Some(InputCommand::Scroll { delta_x, delta_y }),
        InputEvent::Key { key_code, down } => Some(InputCommand::Key { key_code, down }),
    }
}

fn inject_input_command(command: InputCommand) {
    match command {
        InputCommand::MouseMove { x, y, drag_button } => inject_mouse_move(x, y, drag_button),
        InputCommand::MouseButton { button, down, x, y } => inject_mouse_button(button, down, x, y),
        InputCommand::Scroll { delta_x, delta_y } => inject_scroll(delta_x, delta_y),
        InputCommand::Key { key_code, down } => inject_key(key_code, down),
        InputCommand::ReleaseAll | InputCommand::SecureAttention => {}
    }
}

#[cfg(target_os = "windows")]
fn windows_pipe_dispatcher() -> &'static WindowsInputDispatcher {
    static DISPATCHER: OnceLock<WindowsInputDispatcher> = OnceLock::new();
    DISPATCHER.get_or_init(WindowsInputDispatcher::new)
}

#[cfg(target_os = "windows")]
pub fn windows_input_pipe_available() -> bool {
    open_current_session_input_pipe().is_ok()
}

#[cfg(not(target_os = "windows"))]
pub fn windows_input_pipe_available() -> bool {
    false
}

#[cfg(target_os = "windows")]
pub fn send_secure_attention_to_helper() -> Result<(), String> {
    windows_pipe_dispatcher().send(&InputCommand::SecureAttention)
}

#[cfg(not(target_os = "windows"))]
pub fn send_secure_attention_to_helper() -> Result<(), String> {
    Err("Secure Attention Sequence is only available through the Windows input service.".into())
}

#[cfg(target_os = "windows")]
struct WindowsInputDispatcher {
    pipe: Mutex<Option<std::fs::File>>,
    retry_after: Mutex<Instant>,
}

#[cfg(target_os = "windows")]
impl WindowsInputDispatcher {
    fn new() -> Self {
        Self {
            pipe: Mutex::new(None),
            retry_after: Mutex::new(Instant::now()),
        }
    }

    fn send(&self, command: &InputCommand) -> Result<(), String> {
        use std::io::Write;

        let framed = crate::shared_input::encode_input_command(command)?;
        let mut pipe_guard = self
            .pipe
            .lock()
            .map_err(|_| "input helper pipe lock poisoned".to_string())?;

        if pipe_guard.is_none() {
            *pipe_guard = Some(self.open_pipe_with_backoff()?);
        }

        let Some(pipe) = pipe_guard.as_mut() else {
            return Err("input helper pipe unavailable".into());
        };

        if let Err(error) = pipe.write_all(&framed).and_then(|_| pipe.flush()) {
            *pipe_guard = None;
            return Err(format!("write input helper pipe: {error}"));
        }

        Ok(())
    }

    fn open_pipe_with_backoff(&self) -> Result<std::fs::File, String> {
        let now = Instant::now();
        {
            let retry_after = self
                .retry_after
                .lock()
                .map_err(|_| "input helper retry lock poisoned".to_string())?;
            if now < *retry_after {
                return Err("input helper pipe retry is cooling down".into());
            }
        }

        match open_current_session_input_pipe() {
            Ok(file) => Ok(file),
            Err(error) => {
                if let Ok(mut retry_after) = self.retry_after.lock() {
                    *retry_after = Instant::now() + Duration::from_secs(1);
                }
                Err(error)
            }
        }
    }
}

#[cfg(target_os = "windows")]
fn open_current_session_input_pipe() -> Result<std::fs::File, String> {
    use std::fs::OpenOptions;

    let session_id = current_windows_session_id()?;

    let pipe_name = crate::shared_input::input_pipe_name(session_id);
    OpenOptions::new()
        .write(true)
        .open(&pipe_name)
        .map_err(|error| format!("open input helper pipe {pipe_name}: {error}"))
}

#[cfg(target_os = "windows")]
fn current_windows_session_id() -> Result<u32, String> {
    use windows_sys::Win32::System::{
        RemoteDesktop::ProcessIdToSessionId, Threading::GetCurrentProcessId,
    };

    let mut session_id = 0_u32;
    let ok = unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session_id) } != 0;
    if ok {
        Ok(session_id)
    } else {
        Err("failed to resolve current Windows session id".into())
    }
}

#[cfg(target_os = "macos")]
struct MacCaptureContext {
    quic_transport: quic_transport::TransportHandle,
    layout_state: Arc<Mutex<LayoutState>>,
    native_layout: LayoutState,
    active: Mutex<Option<ActiveTarget>>,
    remote_active: Arc<AtomicBool>,
    main_window_visible: Arc<AtomicBool>,
    clipboard_target: Arc<Mutex<Option<ClipboardTarget>>>,
    input_events: Arc<AtomicU64>,
    targets: Vec<InputTarget>,
    switch_request: Arc<Mutex<Option<SwitchDirection>>>,
    anchor: Mutex<Option<(f64, f64)>>,
    cursor_hidden: Mutex<bool>,
    cursor_hide_depth: Mutex<usize>,
    last_cursor_hide_reassert: Mutex<Option<Instant>>,
    last_mouse_move_sent: Mutex<Option<Instant>>,
    last_cursor_repin: Mutex<Option<Instant>>,
    // Instant we last returned control to the local machine. We now land the
    // cursor flush against the edge (RETURN_EDGE_INSET=0) for a seamless
    // extended-display feel, so without a cooldown a fast back-flick would
    // immediately re-satisfy the crossing test and bounce to the remote. During
    // the cooldown window we refuse to cross, letting the user's slide settle.
    last_return: Mutex<Option<Instant>>,
    remote_button_mask: AtomicU64,
    pressed_modifiers: Mutex<Vec<u16>>,
    // Regular (non-modifier) keys we have forwarded as held, so they can be
    // released if the cursor crosses back to local while a key is still down.
    pressed_keys: Mutex<Vec<u16>>,
    tap_disabled: AtomicBool,
    just_crossed: AtomicBool,
    suppress_next_mouse_delta: AtomicBool,
    hotkey_return_point: Mutex<Option<(f64, f64)>>,
    local_screen_points: Mutex<HashMap<String, (f64, f64)>>,
    local_y_bounds: Option<(f64, f64)>,
    display_snapshots: Vec<MacDisplaySnapshot>,
}

#[cfg(target_os = "macos")]
struct RawMacosGestureTap {
    mach_port: core_foundation::mach_port::CFMachPort,
    _context: Arc<MacCaptureContext>,
}

#[cfg(target_os = "macos")]
impl RawMacosGestureTap {
    fn new(
        location: core_graphics::event::CGEventTapLocation,
        context: Arc<MacCaptureContext>,
    ) -> Result<Self, ()> {
        use core_foundation::base::TCFType;
        use core_foundation::mach_port::CFMachPort;
        use core_graphics::event::{CGEventTapOptions, CGEventTapPlacement};

        let mach_port = unsafe {
            macos_raw_event_tap_create(
                location,
                CGEventTapPlacement::HeadInsertEventTap,
                CGEventTapOptions::Default,
                macos_raw_gesture_event_mask(),
                macos_raw_gesture_event_callback,
                Arc::as_ptr(&context).cast(),
            )
        };
        if mach_port.is_null() {
            return Err(());
        }

        Ok(Self {
            mach_port: unsafe { CFMachPort::wrap_under_create_rule(mach_port) },
            _context: context,
        })
    }

    fn mach_port(&self) -> &core_foundation::mach_port::CFMachPort {
        &self.mach_port
    }

    fn enable(&self) {
        use core_foundation::base::TCFType;

        unsafe {
            macos_raw_event_tap_enable(self.mach_port.as_concrete_TypeRef(), true);
        }
    }
}

#[cfg(target_os = "macos")]
impl Drop for RawMacosGestureTap {
    fn drop(&mut self) {
        use core_foundation::base::TCFType;
        use core_foundation::mach_port::CFMachPortInvalidate;

        unsafe {
            CFMachPortInvalidate(self.mach_port.as_CFTypeRef() as *mut _);
        }
    }
}

#[cfg(target_os = "macos")]
type MacosRawEventTapCallback = unsafe extern "C" fn(
    proxy: core_graphics::event::CGEventTapProxy,
    event_type: u32,
    event: core_graphics::sys::CGEventRef,
    user_info: *const std::ffi::c_void,
) -> core_graphics::sys::CGEventRef;

#[cfg(target_os = "macos")]
#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    #[link_name = "CGEventTapCreate"]
    fn macos_raw_event_tap_create(
        tap: core_graphics::event::CGEventTapLocation,
        place: core_graphics::event::CGEventTapPlacement,
        options: core_graphics::event::CGEventTapOptions,
        events_of_interest: u64,
        callback: MacosRawEventTapCallback,
        user_info: *const std::ffi::c_void,
    ) -> core_foundation::mach_port::CFMachPortRef;

    #[link_name = "CGEventTapEnable"]
    fn macos_raw_event_tap_enable(tap: core_foundation::mach_port::CFMachPortRef, enable: bool);
}

#[cfg(target_os = "macos")]
fn macos_raw_gesture_event_mask() -> u64 {
    MACOS_RAW_GESTURE_EVENT_TYPES
        .iter()
        .fold(0_u64, |mask, event_type| mask | (1_u64 << *event_type))
}

#[cfg(target_os = "macos")]
unsafe extern "C" fn macos_raw_gesture_event_callback(
    _proxy: core_graphics::event::CGEventTapProxy,
    event_type: u32,
    event: core_graphics::sys::CGEventRef,
    user_info: *const std::ffi::c_void,
) -> core_graphics::sys::CGEventRef {
    if user_info.is_null() {
        return event;
    }

    let context = unsafe { &*(user_info as *const MacCaptureContext) };
    if matches!(
        event_type,
        MACOS_RAW_EVENT_TAP_DISABLED_BY_TIMEOUT | MACOS_RAW_EVENT_TAP_DISABLED_BY_USER_INPUT
    ) {
        context.tap_disabled.store(true, Ordering::Relaxed);
        return event;
    }

    if context.remote_active.load(Ordering::Relaxed) {
        repin_macos_cursor_while_remote(context);
        log::debug!(
            "remote-active macOS gesture/system event {} was dropped",
            event_type
        );
        return std::ptr::null_mut();
    }

    event
}

#[cfg(target_os = "macos")]
#[derive(Clone)]
struct MacDisplaySnapshot {
    id: core_graphics::display::CGDirectDisplayID,
    origin_x: f64,
    origin_y: f64,
    max_x: f64,
    max_y: f64,
}

#[cfg(target_os = "windows")]
static WINDOWS_CAPTURE_CONTEXT: Mutex<Option<Arc<WindowsCaptureContext>>> = Mutex::new(None);

#[cfg(target_os = "windows")]
struct WindowsCaptureContext {
    quic_transport: quic_transport::TransportHandle,
    layout_state: Arc<Mutex<LayoutState>>,
    native_layout: LayoutState,
    active: Mutex<Option<ActiveTarget>>,
    remote_active: Arc<AtomicBool>,
    main_window_focused: Arc<AtomicBool>,
    clipboard_target: Arc<Mutex<Option<ClipboardTarget>>>,
    input_events: Arc<AtomicU64>,
    targets: Vec<InputTarget>,
    switch_request: Arc<Mutex<Option<SwitchDirection>>>,
    anchor: Mutex<Option<(f64, f64)>>,
    last_point: Mutex<Option<(f64, f64)>>,
    last_mouse_move_sent: Mutex<Option<Instant>>,
    remote_button_mask: AtomicU64,
    pressed_keys: Mutex<Vec<u16>>,
    cursor_hide_calls: Mutex<u8>,
    // Swallow the first post-crossing delta so a fast flick across the edge
    // does not shove the cursor inward on Windows, where we pin by warping.
    just_crossed: AtomicBool,
    local_screen_points: Mutex<HashMap<String, (f64, f64)>>,
}

#[cfg(target_os = "windows")]
fn windows_capture_context() -> Option<Arc<WindowsCaptureContext>> {
    WINDOWS_CAPTURE_CONTEXT
        .lock()
        .ok()
        .and_then(|context| context.clone())
}

#[cfg(target_os = "windows")]
fn clear_windows_capture_context() {
    if let Ok(mut context) = WINDOWS_CAPTURE_CONTEXT.lock() {
        *context = None;
    }
}

fn should_send_mouse_move(last_sent: &Mutex<Option<Instant>>, dragging: bool) -> bool {
    let interval = Duration::from_millis(if dragging {
        DRAG_MOVE_SEND_INTERVAL_MS
    } else {
        MOUSE_MOVE_SEND_INTERVAL_MS
    });
    let Ok(mut last_sent) = last_sent.lock() else {
        return true;
    };
    let now = Instant::now();
    if last_sent
        .as_ref()
        .map(|sent| now.duration_since(*sent) < interval)
        .unwrap_or(false)
    {
        return false;
    }
    *last_sent = Some(now);
    true
}

fn mark_mouse_move_sent(last_sent: &Mutex<Option<Instant>>) {
    if let Ok(mut last_sent) = last_sent.lock() {
        *last_sent = Some(Instant::now());
    }
}

fn reset_mouse_move_timer(last_sent: &Mutex<Option<Instant>>) {
    if let Ok(mut last_sent) = last_sent.lock() {
        *last_sent = None;
    }
}

fn remote_button_is_down(mask: &AtomicU64) -> bool {
    mask.load(Ordering::Relaxed) != 0
}

fn update_remote_button_mask(mask: &AtomicU64, button: MouseButton, down: bool) {
    let bit = mouse_button_mask(button);
    if down {
        mask.fetch_or(bit, Ordering::Relaxed);
    } else {
        mask.fetch_and(!bit, Ordering::Relaxed);
    }
}

fn reset_remote_button_mask(mask: &AtomicU64) {
    mask.store(0, Ordering::Relaxed);
}

/// Sends button-up for every mouse button still marked down on the remote, then
/// clears the mask. Prevents a button getting stuck pressed on the controlled
/// machine when the cursor leaves mid-drag.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn release_remote_buttons(
    quic_transport: &quic_transport::TransportHandle,
    target: &InputTarget,
    mask: &AtomicU64,
    layout_state: &Arc<Mutex<LayoutState>>,
    input_events: &Arc<AtomicU64>,
) {
    let bits = mask.swap(0, Ordering::Relaxed);
    for (bit, button) in [
        (LEFT_BUTTON_MASK, MouseButton::Left),
        (RIGHT_BUTTON_MASK, MouseButton::Right),
        (MIDDLE_BUTTON_MASK, MouseButton::Middle),
    ] {
        if bits & bit != 0 {
            send_packet(
                quic_transport,
                target,
                InputEvent::MouseButton {
                    button,
                    down: false,
                },
                layout_state,
                input_events,
            );
        }
    }
}

/// Releases everything we are currently holding down on the remote — forwarded
/// modifier keys and mouse buttons — so crossing back to the local machine can
/// never leave a stuck Ctrl/Cmd/Shift or pressed button on the controlled side.
#[cfg(target_os = "macos")]
fn release_held_remote_inputs_macos(context: &MacCaptureContext, target: &InputTarget) {
    let held = context
        .pressed_modifiers
        .lock()
        .map(|modifiers| modifiers.clone())
        .unwrap_or_default();
    for key_code in held {
        send_packet(
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
    if let Ok(mut modifiers) = context.pressed_modifiers.lock() {
        modifiers.clear();
    }
    let held_keys = context
        .pressed_keys
        .lock()
        .map(|keys| keys.clone())
        .unwrap_or_default();
    for key_code in held_keys {
        send_packet(
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
    if let Ok(mut pressed) = context.pressed_keys.lock() {
        pressed.clear();
    }
    release_remote_buttons(
        &context.quic_transport,
        target,
        &context.remote_button_mask,
        &context.layout_state,
        &context.input_events,
    );
}

pub fn clear_clipboard_target(target: &Arc<Mutex<Option<ClipboardTarget>>>) {
    if let Ok(mut target) = target.lock() {
        *target = None;
    }
}

pub fn current_clipboard_target(
    target: &Arc<Mutex<Option<ClipboardTarget>>>,
) -> Option<ClipboardTarget> {
    let Ok(mut target) = target.lock() else {
        return None;
    };
    if target
        .as_ref()
        .and_then(|target| target.expires_at)
        .map(|expires_at| Instant::now() >= expires_at)
        .unwrap_or(false)
    {
        *target = None;
        return None;
    }

    target.clone()
}

fn set_clipboard_target(
    target: &Arc<Mutex<Option<ClipboardTarget>>>,
    device_id: String,
    addr: String,
    transport_public_key: String,
    protocol_version: u16,
    cluster_id: String,
    pair_secret: String,
    expires_in: Option<Duration>,
) {
    if let Ok(mut target) = target.lock() {
        *target = Some(ClipboardTarget {
            device_id,
            addr,
            transport_public_key,
            protocol_version,
            cluster_id,
            pair_secret,
            expires_at: expires_in.map(|duration| Instant::now() + duration),
        });
    }
}

fn set_control_clipboard_target(
    target: &Arc<Mutex<Option<ClipboardTarget>>>,
    active: &ActiveTarget,
    layout_state: &Arc<Mutex<LayoutState>>,
) {
    let Ok(layout) = layout_state.lock() else {
        return;
    };
    let Some(device) = layout
        .devices
        .iter()
        .find(|device| device.id == active.target.device_id && device.online && device.input_ready)
    else {
        return;
    };

    set_clipboard_target(
        target,
        active.target.device_id.clone(),
        format!(
            "{}:{}",
            device.host,
            normalize_quic_port(device.transport_port, device.quic_port)
        ),
        device.transport_public_key.clone(),
        device.protocol_version,
        layout.cluster_id.clone(),
        layout.pair_secret.clone(),
        None,
    );
}

#[cfg(target_os = "windows")]
unsafe extern "system" fn windows_mouse_proc(code: i32, wparam: usize, lparam: isize) -> isize {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, MSLLHOOKSTRUCT, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP,
        WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_RBUTTONDOWN, WM_RBUTTONUP,
    };

    if code < 0 {
        return unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) };
    }

    let Some(context) = windows_capture_context() else {
        return unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) };
    };
    if !cached_windows_input_desktop_is_default() {
        release_windows_remote_control(&context, true);
        return unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) };
    }

    let event = unsafe { *(lparam as *const MSLLHOOKSTRUCT) };
    let message = wparam as u32;
    let handled = match message {
        WM_MOUSEMOVE => handle_windows_mouse_move(&context, event.pt.x as f64, event.pt.y as f64),
        WM_LBUTTONDOWN | WM_LBUTTONUP | WM_RBUTTONDOWN | WM_RBUTTONUP | WM_MBUTTONDOWN
        | WM_MBUTTONUP => handle_windows_mouse_button(&context, message),
        WM_MOUSEWHEEL | WM_MOUSEHWHEEL => handle_windows_scroll(&context, message, event.mouseData),
        _ => false,
    };

    if handled {
        1
    } else {
        unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
    }
}

#[cfg(target_os = "windows")]
unsafe extern "system" fn windows_keyboard_proc(code: i32, wparam: usize, lparam: isize) -> isize {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, KBDLLHOOKSTRUCT, WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
    };

    if code < 0 {
        return unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) };
    }

    let Some(context) = windows_capture_context() else {
        return unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) };
    };
    if !cached_windows_input_desktop_is_default() {
        release_windows_remote_control(&context, true);
        return unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) };
    }

    let message = wparam as u32;

    let active = context
        .active
        .lock()
        .ok()
        .and_then(|active| active.as_ref().map(|active| active.target.clone()));
    let Some(target) = active else {
        return unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) };
    };

    if matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP) {
        let event = unsafe { *(lparam as *const KBDLLHOOKSTRUCT) };
        let key_code = event.vkCode as u16;
        let down = matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN);
        if down && windows_event_matches_screen_switch_hotkey(&context, key_code) {
            log::info!("screen switch hotkey returning to local from keyboard hook");
            release_windows_remote_control(&context, false);
            return 1;
        }
        if send_packet(
            &context.quic_transport,
            &target,
            InputEvent::Key { key_code, down },
            &context.layout_state,
            &context.input_events,
        ) {
            track_forwarded_key(&context.pressed_keys, key_code, down);
            return 1;
        }
    }

    unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
}

#[cfg(target_os = "windows")]
fn windows_event_matches_screen_switch_hotkey(
    context: &WindowsCaptureContext,
    key_code: u16,
) -> bool {
    screen_switch_hotkey_matches_vk(
        &context.layout_state,
        key_code,
        windows_current_hotkey_modifiers(),
    )
}

#[cfg(target_os = "windows")]
fn windows_current_hotkey_modifiers() -> HotkeyModifiers {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
    };

    fn down(vk: u16) -> bool {
        unsafe { (GetAsyncKeyState(vk as i32) as u16 & 0x8000) != 0 }
    }

    HotkeyModifiers {
        ctrl: down(VK_CONTROL),
        alt: down(VK_MENU),
        shift: down(VK_SHIFT),
        meta: down(VK_LWIN) || down(VK_RWIN),
    }
}

/// Remembers which keys we have forwarded as pressed so they can be released if
/// the cursor returns to the local machine while a key is still held.
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn track_forwarded_key(pressed: &Mutex<Vec<u16>>, key_code: u16, down: bool) {
    if let Ok(mut pressed) = pressed.lock() {
        if down {
            if !pressed.contains(&key_code) {
                pressed.push(key_code);
            }
        } else {
            pressed.retain(|code| *code != key_code);
        }
    }
}

/// Sends key-up for every key still marked pressed on the remote, then clears
/// the set. Stops a held Ctrl/Alt/Shift from sticking on the controlled machine
/// after the cursor crosses back.
#[cfg(target_os = "windows")]
fn release_forwarded_keys_windows(context: &WindowsCaptureContext, target: &InputTarget) {
    let held = context
        .pressed_keys
        .lock()
        .map(|pressed| pressed.clone())
        .unwrap_or_default();
    for key_code in held {
        send_packet(
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
    if let Ok(mut pressed) = context.pressed_keys.lock() {
        pressed.clear();
    }
}

#[cfg(target_os = "windows")]
fn release_windows_remote_control(context: &WindowsCaptureContext, clear_clipboard: bool) {
    let target = context
        .active
        .lock()
        .ok()
        .and_then(|mut active| active.take().map(|active| active.target));

    if let Some(target) = target {
        release_forwarded_keys_windows(context, &target);
        release_remote_buttons(
            &context.quic_transport,
            &target,
            &context.remote_button_mask,
            &context.layout_state,
            &context.input_events,
        );
    } else {
        reset_remote_button_mask(&context.remote_button_mask);
        if let Ok(mut pressed) = context.pressed_keys.lock() {
            pressed.clear();
        }
    }

    context.remote_active.store(false, Ordering::Relaxed);
    context.just_crossed.store(false, Ordering::Relaxed);
    reset_mouse_move_timer(&context.last_mouse_move_sent);
    show_windows_cursor_if_needed(context);
    if let Ok(mut anchor) = context.anchor.lock() {
        *anchor = None;
    }
    if let Ok(mut last_point) = context.last_point.lock() {
        *last_point = None;
    }
    if clear_clipboard {
        clear_clipboard_target(&context.clipboard_target);
    }
}

#[cfg(target_os = "windows")]
fn cached_windows_input_desktop_is_default() -> bool {
    WINDOWS_INPUT_DESKTOP_DEFAULT_CACHE.load(Ordering::Relaxed)
}

#[cfg(target_os = "windows")]
fn refresh_windows_input_desktop_cache() -> bool {
    let value = windows_input_desktop_is_default();
    WINDOWS_INPUT_DESKTOP_DEFAULT_CACHE.store(value, Ordering::Relaxed);
    value
}

#[cfg(target_os = "windows")]
fn windows_input_desktop_is_default() -> bool {
    use windows_sys::Win32::System::StationsAndDesktops::{
        CloseDesktop, GetUserObjectInformationW, OpenInputDesktop, DESKTOP_READOBJECTS, UOI_NAME,
    };

    unsafe {
        let desktop = OpenInputDesktop(0, 0, DESKTOP_READOBJECTS);
        if desktop.is_null() {
            return false;
        }

        let mut needed = 0_u32;
        let mut buffer = [0_u16; 256];
        let ok = GetUserObjectInformationW(
            desktop as _,
            UOI_NAME,
            buffer.as_mut_ptr() as *mut _,
            (buffer.len() * std::mem::size_of::<u16>()) as u32,
            &mut needed,
        ) != 0;
        let _ = CloseDesktop(desktop);

        if !ok || needed == 0 {
            return false;
        }

        let mut units = ((needed as usize) / std::mem::size_of::<u16>()).min(buffer.len());
        if units > 0 && buffer[units - 1] == 0 {
            units -= 1;
        }
        let name = String::from_utf16_lossy(&buffer[..units]);

        name.eq_ignore_ascii_case("default")
    }
}

#[cfg(target_os = "windows")]
fn handle_windows_mouse_move(context: &WindowsCaptureContext, x: f64, y: f64) -> bool {
    let mut active = match context.active.lock() {
        Ok(active) => active,
        Err(_) => return false,
    };

    if let Some(active_target) = active.as_mut() {
        let anchor = context
            .anchor
            .lock()
            .ok()
            .and_then(|anchor| *anchor)
            .unwrap_or((x, y));
        let dx = x - anchor.0;
        let dy = y - anchor.1;

        if dx.abs() < 0.1 && dy.abs() < 0.1 {
            return true;
        }

        if context.just_crossed.swap(false, Ordering::Relaxed) {
            // First real movement after crossing carries the flick's residual
            // velocity; re-pin to the anchor and swallow it so the cursor stays
            // at the entry edge instead of darting inward.
            set_windows_cursor(anchor.0.round() as i32, anchor.1.round() as i32);
            return true;
        }

        active_target.x += dx;
        active_target.y += dy;

        if update_active_remote_screen(active_target, dx, dy, &context.layout_state) {
            let point = local_return_point(active_target);
            let target = active_target.target.clone();
            // Control is returning to the local machine: park the controlled
            // cursor in a corner so it doesn't visibly linger at the shared edge.
            let _ = send_remote_cursor_park(
                &context.quic_transport,
                active_target,
                &context.layout_state,
                &context.input_events,
            );
            *active = None;
            context.remote_active.store(false, Ordering::Relaxed);
            // Keep the clipboard peer so copies still sync after returning.
            release_forwarded_keys_windows(context, &target);
            release_remote_buttons(
                &context.quic_transport,
                &target,
                &context.remote_button_mask,
                &context.layout_state,
                &context.input_events,
            );
            reset_mouse_move_timer(&context.last_mouse_move_sent);
            show_windows_cursor_if_needed(context);
            set_windows_cursor(point.0.round() as i32, point.1.round() as i32);
            if let Ok(mut anchor) = context.anchor.lock() {
                *anchor = None;
            }
            return true;
        }

        active_target.x = active_target
            .x
            .clamp(0.0, (active_target.current_screen.width - 1) as f64);
        active_target.y = active_target
            .y
            .clamp(0.0, (active_target.current_screen.height - 1) as f64);
        let dragging = remote_button_is_down(&context.remote_button_mask);
        if should_send_mouse_move(&context.last_mouse_move_sent, dragging) {
            if !send_remote_mouse_move(
                &context.quic_transport,
                active_target,
                &context.layout_state,
                &context.input_events,
            ) {
                *active = None;
                context.remote_active.store(false, Ordering::Relaxed);
                clear_clipboard_target(&context.clipboard_target);
                reset_mouse_move_timer(&context.last_mouse_move_sent);
                reset_remote_button_mask(&context.remote_button_mask);
                if let Ok(mut pressed) = context.pressed_keys.lock() {
                    pressed.clear();
                }
                show_windows_cursor_if_needed(context);
                if let Ok(mut anchor) = context.anchor.lock() {
                    *anchor = None;
                }
                return false;
            }
        }
        hide_windows_cursor_if_needed(context);
        set_windows_cursor(anchor.0.round() as i32, anchor.1.round() as i32);
        return true;
    }

    let previous = context
        .last_point
        .lock()
        .ok()
        .and_then(|last_point| *last_point);
    let (dx, dy) = previous
        .map(|point| (x - point.0, y - point.1))
        .unwrap_or((0.0, 0.0));

    if let Ok(mut last_point) = context.last_point.lock() {
        *last_point = Some((x, y));
    }

    let targets = current_input_targets(&context.layout_state, &context.native_layout);
    if let Some(active_target) = crossing_target(&targets, x, y, dx, dy, &context.layout_state) {
        let anchor = local_anchor_point(&active_target);
        hide_windows_cursor_if_needed(context);
        set_windows_cursor(anchor.0.round() as i32, anchor.1.round() as i32);
        if !send_remote_mouse_move(
            &context.quic_transport,
            &active_target,
            &context.layout_state,
            &context.input_events,
        ) {
            reset_mouse_move_timer(&context.last_mouse_move_sent);
            reset_remote_button_mask(&context.remote_button_mask);
            show_windows_cursor_if_needed(context);
            return false;
        }
        mark_mouse_move_sent(&context.last_mouse_move_sent);
        reset_remote_button_mask(&context.remote_button_mask);
        context.remote_active.store(true, Ordering::Relaxed);
        set_control_clipboard_target(
            &context.clipboard_target,
            &active_target,
            &context.layout_state,
        );
        *active = Some(active_target);
        if let Ok(mut anchor_state) = context.anchor.lock() {
            *anchor_state = Some(anchor);
        }
        context.just_crossed.store(true, Ordering::Relaxed);
        return true;
    }

    false
}

#[cfg(target_os = "windows")]
fn handle_windows_mouse_button(context: &WindowsCaptureContext, message: u32) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_RBUTTONDOWN, WM_RBUTTONUP,
    };

    let active = context
        .active
        .lock()
        .ok()
        .and_then(|active| active.as_ref().cloned());
    let Some(active_target) = active else {
        return false;
    };
    let (button, down) = match message {
        WM_LBUTTONDOWN => (MouseButton::Left, true),
        WM_LBUTTONUP => (MouseButton::Left, false),
        WM_RBUTTONDOWN => (MouseButton::Right, true),
        WM_RBUTTONUP => (MouseButton::Right, false),
        WM_MBUTTONDOWN => (MouseButton::Middle, true),
        WM_MBUTTONUP => (MouseButton::Middle, false),
        _ => return false,
    };

    if !send_remote_mouse_move(
        &context.quic_transport,
        &active_target,
        &context.layout_state,
        &context.input_events,
    ) {
        return false;
    }
    mark_mouse_move_sent(&context.last_mouse_move_sent);

    let sent = send_packet(
        &context.quic_transport,
        &active_target.target,
        InputEvent::MouseButton { button, down },
        &context.layout_state,
        &context.input_events,
    );
    if sent {
        update_remote_button_mask(&context.remote_button_mask, button, down);
    }
    sent
}

#[cfg(target_os = "windows")]
fn handle_windows_scroll(context: &WindowsCaptureContext, message: u32, mouse_data: u32) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{WM_MOUSEHWHEEL, WM_MOUSEWHEEL};

    let active = context
        .active
        .lock()
        .ok()
        .and_then(|active| active.as_ref().cloned());
    let Some(active_target) = active else {
        return false;
    };
    let delta = ((mouse_data >> 16) as i16 / 120) as i32;
    let (delta_x, delta_y) = if message == WM_MOUSEHWHEEL {
        (delta, 0)
    } else if message == WM_MOUSEWHEEL {
        (0, delta)
    } else {
        return false;
    };

    if !send_remote_mouse_move(
        &context.quic_transport,
        &active_target,
        &context.layout_state,
        &context.input_events,
    ) {
        return false;
    }
    mark_mouse_move_sent(&context.last_mouse_move_sent);

    send_packet(
        &context.quic_transport,
        &active_target.target,
        InputEvent::Scroll { delta_x, delta_y },
        &context.layout_state,
        &context.input_events,
    )
}

#[cfg(target_os = "windows")]
fn set_windows_cursor(x: i32, y: i32) {
    unsafe {
        let _ = windows_sys::Win32::UI::WindowsAndMessaging::SetCursorPos(x, y);
    }
}

#[cfg(target_os = "windows")]
fn windows_current_cursor_point() -> Option<(f64, f64)> {
    use windows_sys::Win32::{Foundation::POINT, UI::WindowsAndMessaging::GetCursorPos};

    unsafe {
        let mut point = POINT { x: 0, y: 0 };
        if GetCursorPos(&mut point) == 0 {
            return None;
        }
        Some((point.x as f64, point.y as f64))
    }
}

#[cfg(target_os = "windows")]
fn hide_windows_cursor_if_needed(context: &WindowsCaptureContext) {
    let Ok(mut calls) = context.cursor_hide_calls.lock() else {
        return;
    };
    if *calls != 0 {
        return;
    }

    for _ in 0..8 {
        let count = unsafe { windows_sys::Win32::UI::WindowsAndMessaging::ShowCursor(0) };
        *calls += 1;
        if count < 0 {
            break;
        }
    }
}

#[cfg(target_os = "windows")]
fn show_windows_cursor_if_needed(context: &WindowsCaptureContext) {
    let Ok(mut calls) = context.cursor_hide_calls.lock() else {
        return;
    };

    for _ in 0..*calls {
        unsafe {
            let _ = windows_sys::Win32::UI::WindowsAndMessaging::ShowCursor(1);
        }
    }
    *calls = 0;
}

#[cfg(target_os = "macos")]
fn send_macos_mouse_button(
    context: &MacCaptureContext,
    active_target: &ActiveTarget,
    button: MouseButton,
    down: bool,
) -> bool {
    if !send_remote_mouse_move(
        &context.quic_transport,
        active_target,
        &context.layout_state,
        &context.input_events,
    ) {
        return false;
    }
    mark_mouse_move_sent(&context.last_mouse_move_sent);

    let sent = send_packet(
        &context.quic_transport,
        &active_target.target,
        InputEvent::MouseButton { button, down },
        &context.layout_state,
        &context.input_events,
    );
    if sent {
        update_remote_button_mask(&context.remote_button_mask, button, down);
    }
    sent
}

#[cfg(target_os = "macos")]
fn handle_macos_event(
    context: &MacCaptureContext,
    event_type: core_graphics::event::CGEventType,
    event: &core_graphics::event::CGEvent,
) -> core_graphics::event::CallbackResult {
    use core_graphics::event::{CGEventType, CallbackResult, EventField};

    if matches!(
        event_type,
        CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput
    ) {
        // Flag for the run-loop thread to re-enable; the cursor and remote state
        // are reset there too so we don't get stuck mid-control.
        context.tap_disabled.store(true, Ordering::Relaxed);
        log::info!(
            "[diag] event tap disabled by {:?} — mouse/key events are now DROPPED until re-enabled",
            event_type
        );
        return CallbackResult::Keep;
    }

    let dx = event.get_integer_value_field(EventField::MOUSE_EVENT_DELTA_X) as f64;
    let dy = event.get_integer_value_field(EventField::MOUSE_EVENT_DELTA_Y) as f64;

    if matches!(
        event_type,
        CGEventType::MouseMoved
            | CGEventType::LeftMouseDragged
            | CGEventType::RightMouseDragged
            | CGEventType::OtherMouseDragged
    ) {
        return handle_macos_mouse_move(context, event, dx, dy);
    }

    let Ok(active) = context.active.lock() else {
        return CallbackResult::Keep;
    };
    let Some(active_target) = active.as_ref().cloned() else {
        drop(active);
        return handle_macos_modifier_event(context, event_type, event);
    };
    drop(active);
    let target = active_target.target.clone();

    let sent = match event_type {
        CGEventType::LeftMouseDown => {
            send_macos_mouse_button(context, &active_target, MouseButton::Left, true)
        }
        CGEventType::LeftMouseUp => {
            send_macos_mouse_button(context, &active_target, MouseButton::Left, false)
        }
        CGEventType::RightMouseDown => {
            send_macos_mouse_button(context, &active_target, MouseButton::Right, true)
        }
        CGEventType::RightMouseUp => {
            send_macos_mouse_button(context, &active_target, MouseButton::Right, false)
        }
        CGEventType::OtherMouseDown => {
            send_macos_mouse_button(context, &active_target, MouseButton::Middle, true)
        }
        CGEventType::OtherMouseUp => {
            send_macos_mouse_button(context, &active_target, MouseButton::Middle, false)
        }
        CGEventType::ScrollWheel => {
            let delta_y =
                event.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1) as i32;
            let delta_x =
                event.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_2) as i32;
            if !send_remote_mouse_move(
                &context.quic_transport,
                &active_target,
                &context.layout_state,
                &context.input_events,
            ) {
                repin_macos_cursor_while_remote(context);
                return CallbackResult::Drop;
            }
            mark_mouse_move_sent(&context.last_mouse_move_sent);
            send_packet(
                &context.quic_transport,
                &target,
                InputEvent::Scroll { delta_x, delta_y },
                &context.layout_state,
                &context.input_events,
            )
        }
        CGEventType::KeyDown | CGEventType::KeyUp => {
            if matches!(event_type, CGEventType::KeyDown)
                && macos_event_matches_screen_switch_hotkey(context, event)
            {
                log::info!("screen switch hotkey returning to local from input tap");
                return_to_local_macos(context);
                return CallbackResult::Drop;
            }
            let mac_code = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16;
            if let Some(key_code) = mac_key_to_windows_vk(mac_code) {
                let down = matches!(event_type, CGEventType::KeyDown);
                let sent = send_packet(
                    &context.quic_transport,
                    &target,
                    InputEvent::Key { key_code, down },
                    &context.layout_state,
                    &context.input_events,
                );
                if sent {
                    track_forwarded_key(&context.pressed_keys, key_code, down);
                }
                sent
            } else {
                false
            }
        }
        CGEventType::FlagsChanged => {
            send_modifier_changes(context, &target, event);
            true
        }
        _ => false,
    };

    repin_macos_cursor_while_remote(context);
    if !sent {
        log::debug!(
            "remote-active local event {:?} was dropped after remote send miss",
            event_type
        );
    }
    CallbackResult::Drop
}

#[cfg(target_os = "macos")]
fn macos_event_matches_screen_switch_hotkey(
    context: &MacCaptureContext,
    event: &core_graphics::event::CGEvent,
) -> bool {
    use core_graphics::event::{CGEventFlags, EventField};

    let mac_code = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16;
    let Some(key_code) = mac_key_to_windows_vk(mac_code) else {
        return false;
    };
    let flags = event.get_flags();
    let modifiers = HotkeyModifiers {
        ctrl: flags.contains(CGEventFlags::CGEventFlagControl),
        alt: flags.contains(CGEventFlags::CGEventFlagAlternate),
        shift: flags.contains(CGEventFlags::CGEventFlagShift),
        meta: flags.contains(CGEventFlags::CGEventFlagCommand),
    };

    screen_switch_hotkey_matches_vk(&context.layout_state, key_code, modifiers)
}

#[cfg(target_os = "macos")]
fn handle_macos_mouse_move(
    context: &MacCaptureContext,
    event: &core_graphics::event::CGEvent,
    dx: f64,
    dy: f64,
) -> core_graphics::event::CallbackResult {
    use core_graphics::{event::CallbackResult, geometry::CGPoint};

    let location = event.location();
    if let Ok(mut active) = context.active.lock() {
        if let Some(active_target) = active.as_mut() {
            let dy = if active_target.invert_y { -dy } else { dy };
            if context
                .suppress_next_mouse_delta
                .swap(false, Ordering::Relaxed)
            {
                repin_macos_cursor_if_drifted(context, location);
                return CallbackResult::Drop;
            }
            if context.just_crossed.swap(false, Ordering::Relaxed)
                && should_ignore_initial_anchor_warp_delta(active_target.target.edge, dx, dy)
            {
                return CallbackResult::Drop;
            }
            active_target.x += dx;
            active_target.y += dy;

            if update_active_remote_screen(active_target, dx, dy, &context.layout_state) {
                let point = local_return_point(active_target);
                let invert_y = active_target.invert_y;
                let target = active_target.target.clone();
                // Control is returning to the local machine: park the controlled
                // cursor in a corner so it doesn't visibly linger at the shared
                // edge of the controlled (client) screen.
                let _ = send_remote_cursor_park(
                    &context.quic_transport,
                    active_target,
                    &context.layout_state,
                    &context.input_events,
                );
                *active = None;
                context.remote_active.store(false, Ordering::Relaxed);
                context.just_crossed.store(false, Ordering::Relaxed);
                context
                    .suppress_next_mouse_delta
                    .store(false, Ordering::Relaxed);
                // Record the return instant so the crossing test can enforce a
                // short cooldown — without it, landing flush on the edge (inset
                // 0) would let a fast back-flick immediately re-cross.
                if let Ok(mut last_return) = context.last_return.lock() {
                    *last_return = Some(Instant::now());
                }
                // Keep the clipboard peer so copies still sync after returning.
                release_held_remote_inputs_macos(context, &target);
                reset_mouse_move_timer(&context.last_mouse_move_sent);
                reset_cursor_repin_timer(context);
                if let Ok(mut anchor) = context.anchor.lock() {
                    *anchor = None;
                }
                let point = mac_cursor_point(context, point, invert_y);
                // Smooth slide-back: drop the post-warp local-events suppression
                // for just this final warp so the local pointer tracks the mouse
                // immediately instead of freezing for ~0.25s. Re-associating then
                // flushes any suppression still pending from the last re-pin, and
                // the default is restored right after so re-pins keep parking the
                // cursor on the next remote session (a persistent 0 makes the
                // server cursor follow the mouse while not frontmost).
                set_macos_warp_suppression_interval(0.0);
                move_macos_cursor_without_event(context, CGPoint::new(point.0, point.1));
                set_macos_cursor_decoupled(false);
                set_macos_warp_suppression_interval(MACOS_DEFAULT_WARP_SUPPRESSION_SECS);
                log::debug!("[diag] cross BACK to local — showing cursor now");
                show_macos_cursor_if_needed(context);
                return CallbackResult::Drop;
            }

            active_target.x = active_target
                .x
                .clamp(0.0, (active_target.current_screen.width - 1) as f64);
            active_target.y = active_target
                .y
                .clamp(0.0, (active_target.current_screen.height - 1) as f64);
            let dragging = remote_button_is_down(&context.remote_button_mask);
            if should_send_mouse_move(&context.last_mouse_move_sent, dragging) {
                if !send_remote_mouse_move(
                    &context.quic_transport,
                    active_target,
                    &context.layout_state,
                    &context.input_events,
                ) {
                    *active = None;
                    context.remote_active.store(false, Ordering::Relaxed);
                    context.just_crossed.store(false, Ordering::Relaxed);
                    context
                        .suppress_next_mouse_delta
                        .store(false, Ordering::Relaxed);
                    clear_clipboard_target(&context.clipboard_target);
                    reset_mouse_move_timer(&context.last_mouse_move_sent);
                    reset_cursor_repin_timer(context);
                    reset_remote_button_mask(&context.remote_button_mask);
                    if let Ok(mut modifiers) = context.pressed_modifiers.lock() {
                        modifiers.clear();
                    }
                    if let Ok(mut anchor) = context.anchor.lock() {
                        *anchor = None;
                    }
                    set_macos_warp_suppression_interval(MACOS_DEFAULT_WARP_SUPPRESSION_SECS);
                    set_macos_cursor_decoupled(false);
                    show_macos_cursor_if_needed(context);
                    return CallbackResult::Keep;
                }
            }
            repin_macos_cursor_if_drifted(context, location);
            // Re-pinning also runs from the capture run loop because mouse-move
            // callbacks can stop arriving once the pointer is over the client.
            return CallbackResult::Drop;
        }
    }

    let targets = current_input_targets(&context.layout_state, &context.native_layout);
    // Return cooldown: the cursor now lands flush on the edge (inset 0) on
    // return, so without this gate a fast back-flick immediately re-satisfies
    // the crossing test and bounces into the remote. Ignore crossings for a
    // short window after returning so the user's slide settles locally.
    if let Ok(last_return) = context.last_return.lock() {
        if let Some(when) = *last_return {
            if when.elapsed() < Duration::from_millis(RETURN_COOLDOWN_MS) {
                return CallbackResult::Keep;
            }
        }
    }
    if let Some(active_target) =
        mac_crossing_target(context, &targets, location.x, location.y, dx, dy)
    {
        let anchor = mac_cursor_point(
            context,
            local_anchor_point(&active_target),
            active_target.invert_y,
        );
        set_macos_cursor_decoupled(true);
        set_macos_warp_suppression_interval(0.0);
        // Hide BEFORE the anchor warp: when MyKVM is hidden/minimized it runs as a
        // background process, and the WindowServer services a background process's
        // cursor-warp and cursor-hide calls lazily. If we warp first the user sees
        // the pointer flick to the screen edge and linger there until the delayed
        // hide lands — the "cursor sticks at the edge, hides late" stutter, whose
        // visible offset scales with flick speed. Hiding first means the pointer
        // vanishes where it is, then jumps to the anchor invisibly, so no edge
        // stick is ever visible regardless of scheduling latency.
        log::debug!("[diag] cross INTO remote — hiding+decoupling now");
        hide_macos_cursor_if_needed(context);
        move_macos_cursor_without_event(context, CGPoint::new(anchor.0, anchor.1));
        if !send_remote_mouse_move(
            &context.quic_transport,
            &active_target,
            &context.layout_state,
            &context.input_events,
        ) {
            reset_mouse_move_timer(&context.last_mouse_move_sent);
            reset_remote_button_mask(&context.remote_button_mask);
            reset_cursor_repin_timer(context);
            set_macos_warp_suppression_interval(MACOS_DEFAULT_WARP_SUPPRESSION_SECS);
            set_macos_cursor_decoupled(false);
            show_macos_cursor_if_needed(context);
            context.just_crossed.store(false, Ordering::Relaxed);
            return CallbackResult::Keep;
        }
        reset_mouse_move_timer(&context.last_mouse_move_sent);
        reset_cursor_repin_timer(context);
        reset_remote_button_mask(&context.remote_button_mask);
        context.remote_active.store(true, Ordering::Relaxed);
        set_control_clipboard_target(
            &context.clipboard_target,
            &active_target,
            &context.layout_state,
        );
        if let Ok(mut active) = context.active.lock() {
            *active = Some(active_target.clone());
        }
        if let Ok(mut anchor_state) = context.anchor.lock() {
            *anchor_state = Some(anchor);
        }
        context.just_crossed.store(true, Ordering::Relaxed);
        return CallbackResult::Drop;
    }

    CallbackResult::Keep
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn crossing_target(
    targets: &[InputTarget],
    x: f64,
    y: f64,
    dx: f64,
    dy: f64,
    layout_state: &Arc<Mutex<LayoutState>>,
) -> Option<ActiveTarget> {
    crossing_target_with_transform(targets, x, y, dx, dy, false, layout_state)
}

fn crossing_target_with_transform(
    targets: &[InputTarget],
    x: f64,
    y: f64,
    dx: f64,
    dy: f64,
    invert_y: bool,
    layout_state: &Arc<Mutex<LayoutState>>,
) -> Option<ActiveTarget> {
    targets
        .iter()
        .find_map(|target| {
            if !target_is_online(target, layout_state) {
                return None;
            }

            crossing_layout_point(target, x, y, dx, dy).map(|point| (target, point))
        })
        .map(|(target, (mapped_x, mapped_y))| {
            let entry_dx = dx * target.layout_local_screen.width.max(1) as f64
                / target.local_screen.width.max(1) as f64;
            let entry_dy = dy * target.layout_local_screen.height.max(1) as f64
                / target.local_screen.height.max(1) as f64;
            let remote_x = match target.edge {
                Edge::Right => 1.0 + entry_dx.max(0.0),
                Edge::Left => (target.remote_screen.width - 2) as f64 + entry_dx.min(0.0),
                _ => (mapped_x - target.remote_screen.x as f64)
                    .clamp(0.0, (target.remote_screen.width - 1) as f64),
            }
            .clamp(0.0, (target.remote_screen.width - 1) as f64);
            let remote_y = match target.edge {
                Edge::Bottom => 1.0 + entry_dy.max(0.0),
                Edge::Top => (target.remote_screen.height - 2) as f64 + entry_dy.min(0.0),
                _ => (mapped_y - target.remote_screen.y as f64)
                    .clamp(0.0, (target.remote_screen.height - 1) as f64),
            }
            .clamp(0.0, (target.remote_screen.height - 1) as f64);

            // The screen we cross into is the entry screen; carry it (with its
            // wire id) as the initial "current" screen so the cursor can later
            // roam onto the remote device's other screens.
            let mut current_screen = target.remote_screen.clone();
            current_screen.id = target.screen_id.clone();

            ActiveTarget {
                target: target.clone(),
                current_screen,
                current_screen_id: target.screen_id.clone(),
                x: remote_x,
                y: remote_y,
                invert_y,
            }
        })
}

fn crossing_layout_point(
    target: &InputTarget,
    x: f64,
    y: f64,
    dx: f64,
    dy: f64,
) -> Option<(f64, f64)> {
    if is_crossing_screen(&target.local_screen, target.edge, x, y, dx, dy) {
        return Some(native_to_layout_point(target, x, y));
    }

    let mapped = native_to_layout_point(target, x, y);
    let mapped_dx = dx * target.layout_local_screen.width.max(1) as f64
        / target.local_screen.width.max(1) as f64;
    let mapped_dy = dy * target.layout_local_screen.height.max(1) as f64
        / target.local_screen.height.max(1) as f64;
    if is_crossing_screen(
        &target.layout_local_screen,
        target.edge,
        mapped.0,
        mapped.1,
        mapped_dx,
        mapped_dy,
    ) {
        return Some(mapped);
    }

    None
}

fn native_to_layout_point(target: &InputTarget, x: f64, y: f64) -> (f64, f64) {
    let native = &target.local_screen;
    let layout = &target.layout_local_screen;
    let ratio_x = (x - native.x as f64) / native.width.max(1) as f64;
    let ratio_y = (y - native.y as f64) / native.height.max(1) as f64;

    (
        layout.x as f64 + ratio_x * layout.width.max(1) as f64,
        layout.y as f64 + ratio_y * layout.height.max(1) as f64,
    )
}

fn is_crossing_screen(screen: &Screen, edge: Edge, x: f64, y: f64, dx: f64, dy: f64) -> bool {
    let left = screen.x as f64;
    let right = (screen.x + screen.width) as f64;
    let top = screen.y as f64;
    let bottom = (screen.y + screen.height) as f64;
    let previous_x = x - dx;
    let previous_y = y - dy;

    // Require the previous reconstructed point to already be near the shared
    // edge. This still permits fast edge flicks, but rejects a single huge jump
    // from the middle of the screen that merely lands near the boundary.
    match edge {
        Edge::Right => {
            dx >= MIN_CROSSING_DELTA
                && dx.abs() >= dy.abs() * CROSSING_AXIS_DOMINANCE
                && previous_x >= right - CROSSING_ACTIVATION_BAND
                && x >= right - CROSSING_MARGIN
                && y >= top - CROSSING_MARGIN
                && y <= bottom + CROSSING_MARGIN
        }
        Edge::Left => {
            dx <= -MIN_CROSSING_DELTA
                && dx.abs() >= dy.abs() * CROSSING_AXIS_DOMINANCE
                && previous_x <= left + CROSSING_ACTIVATION_BAND
                && x <= left + CROSSING_MARGIN
                && y >= top - CROSSING_MARGIN
                && y <= bottom + CROSSING_MARGIN
        }
        Edge::Bottom => {
            dy >= MIN_CROSSING_DELTA
                && dy.abs() >= dx.abs() * CROSSING_AXIS_DOMINANCE
                && previous_y >= bottom - CROSSING_ACTIVATION_BAND
                && y >= bottom - CROSSING_MARGIN
                && x >= left - CROSSING_MARGIN
                && x <= right + CROSSING_MARGIN
        }
        Edge::Top => {
            dy <= -MIN_CROSSING_DELTA
                && dy.abs() >= dx.abs() * CROSSING_AXIS_DOMINANCE
                && previous_y <= top + CROSSING_ACTIVATION_BAND
                && y <= top + CROSSING_MARGIN
                && x >= left - CROSSING_MARGIN
                && x <= right + CROSSING_MARGIN
        }
    }
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn local_y_bounds(targets: &[InputTarget]) -> Option<(f64, f64)> {
    let mut min_y: Option<i32> = None;
    let mut max_y: Option<i32> = None;

    for target in targets {
        let top = target.local_screen.y;
        let bottom = target.local_screen.y + target.local_screen.height;
        min_y = Some(min_y.map_or(top, |current| current.min(top)));
        max_y = Some(max_y.map_or(bottom, |current| current.max(bottom)));
    }

    Some((min_y? as f64, max_y? as f64))
}

#[cfg(target_os = "macos")]
fn mac_crossing_target(
    context: &MacCaptureContext,
    targets: &[InputTarget],
    x: f64,
    y: f64,
    dx: f64,
    dy: f64,
) -> Option<ActiveTarget> {
    if let Some(target) =
        crossing_target_with_transform(targets, x, y, dx, dy, false, &context.layout_state)
    {
        return Some(target);
    }

    let Some((min_y, max_y)) = local_y_bounds(targets).or(context.local_y_bounds) else {
        return None;
    };
    let flipped_y = min_y + max_y - y;
    if (flipped_y - y).abs() < 0.5 {
        return None;
    }

    crossing_target_with_transform(targets, x, flipped_y, dx, -dy, true, &context.layout_state)
}

#[cfg(target_os = "macos")]
fn mac_cursor_point(context: &MacCaptureContext, point: (f64, f64), invert_y: bool) -> (f64, f64) {
    if !invert_y {
        return point;
    }

    local_y_bounds(&current_input_targets(
        &context.layout_state,
        &context.native_layout,
    ))
    .or(context.local_y_bounds)
    .map(|(min_y, max_y)| (point.0, min_y + max_y - point.1))
    .unwrap_or(point)
}

/// After a raw delta has been applied to `active.x`/`active.y`, reconcile which
/// remote screen the cursor is on. If it has crossed onto another screen of the
/// same remote device, switch to it so control roams across the remote's whole
/// desktop (e.g. onto a client's secondary monitor). Returns `true` when the
/// cursor has left the remote desktop back toward the local machine, in which
/// case the caller should hand control back.
fn update_active_remote_screen(
    active: &mut ActiveTarget,
    dx: f64,
    dy: f64,
    layout_state: &Arc<Mutex<LayoutState>>,
) -> bool {
    // Still within the screen we're already on: nothing to reconcile.
    if point_in_local_bounds(&active.current_screen, active.x, active.y) {
        return false;
    }

    let screens = layout_state
        .lock()
        .map(|layout| remote_device_screens(&layout, &active.target.device_id))
        .unwrap_or_default();

    // Position of the cursor in the remote device's shared layout space.
    let global_x = active.current_screen.x as f64 + active.x;
    let global_y = active.current_screen.y as f64 + active.y;

    // Roam onto an adjacent screen of the same device that holds this point.
    if let Some(screen) = screens.iter().find(|screen| {
        screen.id != active.current_screen.id && point_in_screen(screen, global_x, global_y)
    }) {
        active.x = global_x - screen.x as f64;
        active.y = global_y - screen.y as f64;
        active.current_screen_id = screen.id.clone();
        active.current_screen = screen.clone();
        return false;
    }

    // Off the edge with no neighbor there. Only the entry screen borders the
    // local machine, so only it can hand control back; every other outer edge
    // just clamps the cursor in place.
    let returned_to_local = active.current_screen_id == active.target.screen_id
        && exited_entry_edge(
            active.target.edge,
            &active.current_screen,
            active.x,
            active.y,
            dx,
            dy,
        );
    if returned_to_local {
        pin_active_to_entry_edge(active);
    }

    returned_to_local
}

fn should_ignore_initial_anchor_warp_delta(edge: Edge, dx: f64, dy: f64) -> bool {
    match edge {
        Edge::Right => dx <= -MIN_CROSSING_DELTA && dx.abs() >= dy.abs() * CROSSING_AXIS_DOMINANCE,
        Edge::Left => dx >= MIN_CROSSING_DELTA && dx.abs() >= dy.abs() * CROSSING_AXIS_DOMINANCE,
        Edge::Bottom => dy <= -MIN_CROSSING_DELTA && dy.abs() >= dx.abs() * CROSSING_AXIS_DOMINANCE,
        Edge::Top => dy >= MIN_CROSSING_DELTA && dy.abs() >= dx.abs() * CROSSING_AXIS_DOMINANCE,
    }
}

/// True when local coordinates `x`/`y` are inside `screen`'s bounds.
fn point_in_local_bounds(screen: &Screen, x: f64, y: f64) -> bool {
    x >= 0.0 && x <= (screen.width - 1) as f64 && y >= 0.0 && y <= (screen.height - 1) as f64
}

/// True when a point in shared layout space falls on `screen`.
fn point_in_screen(screen: &Screen, global_x: f64, global_y: f64) -> bool {
    global_x >= screen.x as f64
        && global_x <= (screen.x + screen.width - 1) as f64
        && global_y >= screen.y as f64
        && global_y <= (screen.y + screen.height - 1) as f64
}

/// Whether the cursor has crossed back over the edge it originally entered from
/// (the side bordering the local machine). Mirrors the classic single-screen
/// return-to-local test, applied to the entry screen.
fn exited_entry_edge(edge: Edge, screen: &Screen, x: f64, y: f64, dx: f64, dy: f64) -> bool {
    match edge {
        Edge::Right => x <= 0.0 && dx < 0.0,
        Edge::Left => x >= (screen.width - 1) as f64 && dx > 0.0,
        Edge::Bottom => y <= 0.0 && dy < 0.0,
        Edge::Top => y >= (screen.height - 1) as f64 && dy > 0.0,
    }
}

fn pin_active_to_entry_edge(active: &mut ActiveTarget) {
    active.x = active
        .x
        .clamp(0.0, (active.current_screen.width - 1) as f64);
    active.y = active
        .y
        .clamp(0.0, (active.current_screen.height - 1) as f64);

    match active.target.edge {
        Edge::Right => active.x = 0.0,
        Edge::Left => active.x = (active.current_screen.width - 1) as f64,
        Edge::Bottom => active.y = 0.0,
        Edge::Top => active.y = (active.current_screen.height - 1) as f64,
    }
}

/// The remote device's screens, each carrying the wire screen id that the
/// receiving side matches against (the device-prefixed layout id stripped back
/// to the peer's own screen id).
fn remote_device_screens(layout: &LayoutState, device_id: &str) -> Vec<Screen> {
    layout
        .devices
        .iter()
        .find(|device| device.id == device_id)
        .map(|device| {
            device
                .screens
                .iter()
                .map(|screen| {
                    let mut copy = screen.clone();
                    copy.id = peer_screen_id(device, screen);
                    copy
                })
                .collect()
        })
        .unwrap_or_default()
}

fn local_return_point(active: &ActiveTarget) -> (f64, f64) {
    let local = &active.target.local_screen;
    let layout_local = &active.target.layout_local_screen;
    let remote = &active.target.remote_screen;
    let global_x = remote.x as f64 + active.x;
    let global_y = remote.y as f64 + active.y;
    let ratio_x = (global_x - layout_local.x as f64) / layout_local.width.max(1) as f64;
    let ratio_y = (global_y - layout_local.y as f64) / layout_local.height.max(1) as f64;
    let native_x = local.x as f64 + ratio_x * local.width.max(1) as f64;
    let native_y = local.y as f64 + ratio_y * local.height.max(1) as f64;

    // Land the cursor flush on the entry edge (RETURN_EDGE_INSET=0) for a
    // seamless extended-display feel. A fast back-flick can no longer bounce
    // straight back into the remote because the crossing test is gated by a
    // time-based cooldown (RETURN_COOLDOWN_MS), not by inset distance.
    let inset = RETURN_EDGE_INSET.min((local.width.max(1) - 1) as f64 / 2.0);
    let inset_v = RETURN_EDGE_INSET.min((local.height.max(1) - 1) as f64 / 2.0);
    match active.target.edge {
        Edge::Right => (
            (local.x + local.width - 1) as f64 - inset,
            native_y.clamp(local.y as f64, (local.y + local.height - 1) as f64),
        ),
        Edge::Left => (
            local.x as f64 + inset,
            native_y.clamp(local.y as f64, (local.y + local.height - 1) as f64),
        ),
        Edge::Bottom => (
            native_x.clamp(local.x as f64, (local.x + local.width - 1) as f64),
            (local.y + local.height - 1) as f64 - inset_v,
        ),
        Edge::Top => (
            native_x.clamp(local.x as f64, (local.x + local.width - 1) as f64),
            local.y as f64 + inset_v,
        ),
    }
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn local_center_point(active: &ActiveTarget) -> (f64, f64) {
    let local = &active.target.local_screen;
    (
        local.x as f64 + (local.width as f64 / 2.0).clamp(0.0, (local.width - 1).max(0) as f64),
        local.y as f64 + (local.height as f64 / 2.0).clamp(0.0, (local.height - 1).max(0) as f64),
    )
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn local_hotkey_return_point(
    active: &ActiveTarget,
    recorded_point: Option<(f64, f64)>,
) -> (f64, f64) {
    recorded_point.unwrap_or_else(|| local_center_point(active))
}

fn send_remote_mouse_move(
    quic_transport: &quic_transport::TransportHandle,
    active: &ActiveTarget,
    layout_state: &Arc<Mutex<LayoutState>>,
    input_events: &Arc<AtomicU64>,
) -> bool {
    send_packet(
        quic_transport,
        &active.target,
        InputEvent::MouseMove {
            screen_id: active.current_screen_id.clone(),
            x: active.x.round() as i32,
            y: active.y.round() as i32,
        },
        layout_state,
        input_events,
    )
}

fn local_anchor_point(active: &ActiveTarget) -> (f64, f64) {
    local_return_point(active)
}

/// When control returns to the local machine, move the controlled cursor into
/// the far (bottom-right) corner of the remote screen instead of leaving it
/// parked at the shared edge. True cursor hiding isn't reliably possible on the
/// controlled side, so tucking it into a corner is the seamless-feeling
/// approximation.
#[cfg_attr(not(any(target_os = "windows", target_os = "macos")), allow(dead_code))]
fn send_remote_cursor_park(
    quic_transport: &quic_transport::TransportHandle,
    active: &ActiveTarget,
    layout_state: &Arc<Mutex<LayoutState>>,
    input_events: &Arc<AtomicU64>,
) -> bool {
    send_packet(
        quic_transport,
        &active.target,
        InputEvent::MouseMove {
            screen_id: active.current_screen_id.clone(),
            x: (active.current_screen.width - 1).max(0),
            y: (active.current_screen.height - 1).max(0),
        },
        layout_state,
        input_events,
    )
}

#[cfg(target_os = "macos")]
fn enter_remote_target_macos(context: &MacCaptureContext, active_target: ActiveTarget) {
    use core_graphics::geometry::CGPoint;

    let return_point = macos_current_cursor_location().map(|point| (point.x, point.y));
    let anchor = mac_cursor_point(
        context,
        local_anchor_point(&active_target),
        active_target.invert_y,
    );
    if !send_remote_mouse_move(
        &context.quic_transport,
        &active_target,
        &context.layout_state,
        &context.input_events,
    ) {
        reset_mouse_move_timer(&context.last_mouse_move_sent);
        reset_remote_button_mask(&context.remote_button_mask);
        reset_cursor_repin_timer(context);
        set_macos_warp_suppression_interval(MACOS_DEFAULT_WARP_SUPPRESSION_SECS);
        set_macos_cursor_decoupled(false);
        show_macos_cursor_if_needed(context);
        context.just_crossed.store(false, Ordering::Relaxed);
        context
            .suppress_next_mouse_delta
            .store(false, Ordering::Relaxed);
        if let Ok(mut hotkey_return_point) = context.hotkey_return_point.lock() {
            *hotkey_return_point = None;
        }
        return;
    }
    set_macos_cursor_decoupled(true);
    set_macos_warp_suppression_interval(0.0);
    hide_macos_cursor_if_needed(context);
    move_macos_cursor_without_event(context, CGPoint::new(anchor.0, anchor.1));
    reset_mouse_move_timer(&context.last_mouse_move_sent);
    reset_cursor_repin_timer(context);
    reset_remote_button_mask(&context.remote_button_mask);
    context.remote_active.store(true, Ordering::Relaxed);
    set_control_clipboard_target(
        &context.clipboard_target,
        &active_target,
        &context.layout_state,
    );
    if let Ok(mut active) = context.active.lock() {
        *active = Some(active_target);
    }
    if let Ok(mut anchor_state) = context.anchor.lock() {
        *anchor_state = Some(anchor);
    }
    if let Ok(mut hotkey_return_point) = context.hotkey_return_point.lock() {
        *hotkey_return_point = return_point;
    }
    // Hotkey entry lands at the remote screen centre. macOS can still emit one
    // synthetic delta from the local anchor warp; drop only that next delta.
    context.just_crossed.store(false, Ordering::Relaxed);
    context
        .suppress_next_mouse_delta
        .store(true, Ordering::Relaxed);
}

#[cfg(target_os = "macos")]
fn return_to_local_macos(context: &MacCaptureContext) {
    use core_graphics::geometry::CGPoint;

    let active_target = match context.active.lock().ok().and_then(|mut a| a.take()) {
        Some(target) => target,
        None => return,
    };
    let recorded_point = context
        .hotkey_return_point
        .lock()
        .ok()
        .and_then(|mut point| point.take());
    let point = local_hotkey_return_point(&active_target, recorded_point);
    let invert_y = active_target.invert_y;
    let target = active_target.target.clone();
    let _ = send_remote_cursor_park(
        &context.quic_transport,
        &active_target,
        &context.layout_state,
        &context.input_events,
    );
    context.remote_active.store(false, Ordering::Relaxed);
    context.just_crossed.store(false, Ordering::Relaxed);
    context
        .suppress_next_mouse_delta
        .store(false, Ordering::Relaxed);
    if let Ok(mut last_return) = context.last_return.lock() {
        *last_return = Some(Instant::now());
    }
    release_held_remote_inputs_macos(context, &target);
    reset_mouse_move_timer(&context.last_mouse_move_sent);
    reset_cursor_repin_timer(context);
    if let Ok(mut anchor) = context.anchor.lock() {
        *anchor = None;
    }
    let point = if recorded_point.is_some() {
        point
    } else {
        mac_cursor_point(context, point, invert_y)
    };
    set_macos_warp_suppression_interval(0.0);
    move_macos_cursor_without_event(context, CGPoint::new(point.0, point.1));
    set_macos_cursor_decoupled(false);
    set_macos_warp_suppression_interval(MACOS_DEFAULT_WARP_SUPPRESSION_SECS);
    show_macos_cursor_if_needed(context);
}

/// Re-assert cursor decouple + position lock while a remote session is active.
///
/// When MyKVM is backgrounded (the normal state while controlling a remote),
/// macOS can silently re-associate the physical mouse with the on-screen cursor
/// despite an earlier `CGAssociateMouseAndMouseCursorPosition(false)`. The
/// pointer then follows the mouse. Reuse the same drift-limited repin path used
/// by the mouse callback, because the callback can stop firing while the main
/// window is hidden. Do not repeatedly push hide/transparent cursor state here:
/// those APIs are stack-based and must stay one enter paired with one return.
#[cfg(target_os = "macos")]
fn repin_macos_cursor_while_remote(context: &MacCaptureContext) {
    set_macos_cursor_decoupled(true);
    if !context.main_window_visible.load(Ordering::Relaxed) {
        if let Some(location) = macos_current_cursor_location() {
            repin_macos_cursor_if_drifted(context, location);
        } else {
            force_repin_macos_cursor_to_anchor(context);
        }
        reassert_macos_hidden_window_cursor(context);
        return;
    }

    if let Some(location) = macos_current_cursor_location() {
        repin_macos_cursor_if_drifted(context, location);
    }
}

#[cfg(target_os = "macos")]
fn macos_capture_loop_ms(remote_active: bool, main_window_visible: bool) -> u64 {
    if !remote_active {
        return MACOS_IDLE_CAPTURE_LOOP_MS;
    }
    if main_window_visible {
        MACOS_VISIBLE_REMOTE_CAPTURE_LOOP_MS
    } else {
        MACOS_HIDDEN_REMOTE_CAPTURE_LOOP_MS
    }
}

/// Poll the shared switch-request slot and act on it. Called from the capture
/// loop on each iteration. Centralises the macOS enter/return side effects so
/// both the mouse-crossing path and the hotkey path stay in sync.
#[cfg(target_os = "macos")]
fn drain_switch_request_macos(context: &MacCaptureContext) {
    let direction = match context.switch_request.lock() {
        Ok(mut req) => req.take(),
        Err(_) => return,
    };
    let Some(direction) = direction else { return };
    let current_point = macos_current_cursor_location().map(|point| (point.x, point.y));
    match request_screen_switch_from_point(
        direction,
        &context.layout_state,
        &context.native_layout,
        &context.active,
        current_point,
    ) {
        SwitchOutcome::Enter(active_target) => {
            log::info!(
                "screen switch entering device={}",
                active_target.target.device_id
            );
            enter_remote_target_macos(context, active_target);
        }
        SwitchOutcome::Return => {
            log::info!("screen switch returning to local");
            return_to_local_macos(context);
        }
        SwitchOutcome::LocalMove {
            from_screen_id,
            to_screen_id,
            x,
            y,
        } => {
            let (x, y) = remembered_local_screen_point(
                &context.local_screen_points,
                &from_screen_id,
                &to_screen_id,
                current_point,
                (x, y),
            );
            log::info!("screen switch moving local cursor to ({x:.0}, {y:.0})");
            set_macos_cursor_decoupled(false);
            set_macos_warp_suppression_interval(0.0);
            move_macos_cursor_without_event(context, core_graphics::geometry::CGPoint::new(x, y));
            set_macos_warp_suppression_interval(MACOS_DEFAULT_WARP_SUPPRESSION_SECS);
            show_macos_cursor_if_needed(context);
        }
        SwitchOutcome::Noop => {
            log::warn!("screen switch {direction:?} ignored: no matching online target");
        }
    }
}

#[cfg(target_os = "windows")]
fn drain_switch_request_windows(context: &WindowsCaptureContext) {
    let direction = match context.switch_request.lock() {
        Ok(mut req) => req.take(),
        Err(_) => return,
    };
    let Some(direction) = direction else { return };
    let current_point = windows_current_cursor_point();
    match request_screen_switch_from_point(
        direction,
        &context.layout_state,
        &context.native_layout,
        &context.active,
        current_point,
    ) {
        SwitchOutcome::Enter(active_target) => {
            log::info!(
                "screen switch entering device={}",
                active_target.target.device_id
            );
            // Mirror the Windows mouse-crossing enter path. Hotkey entry has no
            // physical mouse position at the edge, so we explicitly pin to the
            // local anchor and start sending deltas from there.
            let anchor = local_anchor_point(&active_target);
            hide_windows_cursor_if_needed(context);
            set_windows_cursor(anchor.0.round() as i32, anchor.1.round() as i32);
            if send_remote_mouse_move(
                &context.quic_transport,
                &active_target,
                &context.layout_state,
                &context.input_events,
            ) {
                context.remote_active.store(true, Ordering::Relaxed);
                set_control_clipboard_target(
                    &context.clipboard_target,
                    &active_target,
                    &context.layout_state,
                );
                if let Ok(mut active) = context.active.lock() {
                    *active = Some(active_target);
                }
                if let Ok(mut anchor_state) = context.anchor.lock() {
                    *anchor_state = Some(anchor);
                }
                // Hotkey entry lands at the remote centre. The edge-crossing
                // first-delta guard would eat the user's first real movement.
                context.just_crossed.store(false, Ordering::Relaxed);
            } else {
                reset_mouse_move_timer(&context.last_mouse_move_sent);
                reset_remote_button_mask(&context.remote_button_mask);
                show_windows_cursor_if_needed(context);
            }
        }
        SwitchOutcome::Return => {
            log::info!("screen switch returning to local");
            release_windows_remote_control(context, false);
        }
        SwitchOutcome::LocalMove {
            from_screen_id,
            to_screen_id,
            x,
            y,
        } => {
            let (x, y) = remembered_local_screen_point(
                &context.local_screen_points,
                &from_screen_id,
                &to_screen_id,
                current_point,
                (x, y),
            );
            log::info!("screen switch moving local cursor to ({x:.0}, {y:.0})");
            set_windows_cursor(x.round() as i32, y.round() as i32);
        }
        SwitchOutcome::Noop => {
            log::warn!("screen switch {direction:?} ignored: no matching online target");
        }
    }
}

/// Disconnects (or reconnects) the on-screen cursor from the physical mouse.
/// While controlling a remote screen we decouple them: the mouse keeps emitting
/// HID deltas to our event tap, but the local cursor stays frozen, so we never
/// have to warp it back each event. Warping every move triggers macOS's
/// post-warp local-event suppression (~0.25s), which drops motion and makes the
/// remote cursor drift and stutter. Decoupling is how a real extended display
/// feels seamless. MUST be re-coupled on every exit path or the user's cursor
/// stays frozen.
#[cfg(target_os = "macos")]
fn set_macos_cursor_decoupled(decoupled: bool) {
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGAssociateMouseAndMouseCursorPosition(connected: i32) -> i32;
    }

    let connected = if decoupled { 0 } else { 1 };
    unsafe {
        let _ = CGAssociateMouseAndMouseCursorPosition(connected);
    }
}

/// macOS default: local hardware events stay suppressed for 0.25s after a warp.
#[cfg(target_os = "macos")]
const MACOS_DEFAULT_WARP_SUPPRESSION_SECS: f64 = 0.25;

/// Set how long macOS suppresses local hardware mouse events after a cursor
/// warp (`CGWarpMouseCursorPosition` / `CGDisplayMoveCursorToPoint`).
///
/// This is process-wide. Keep it at `0` only while remote control is active so
/// macOS does not swallow hardware deltas after our anchor/re-pin warps, then
/// restore the default on every exit path.
#[cfg(target_os = "macos")]
fn set_macos_warp_suppression_interval(seconds: f64) {
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGSetLocalEventsSuppressionInterval(seconds: f64) -> i32;
    }
    unsafe {
        let _ = CGSetLocalEventsSuppressionInterval(seconds);
    }
}

/// Opt the process out of macOS App Nap while input is being captured.
///
/// When MyKVM is not the frontmost app (another window is focused) or the
/// window is minimized, macOS throttles our background capture thread's run
/// loop and coalesces its timers. That throttling is exactly what makes the
/// cursor "stutter" when it slides back from a remote device: forwarded events
/// and cursor re-pinning fall behind, then catch up in a burst at the edge.
///
/// `NSProcessInfo -beginActivityWithOptions:reason:` with a latency-critical,
/// user-initiated activity tells the OS to keep us scheduled normally. We hold
/// the returned (retained) activity token for the whole capture lifetime and
/// end it on teardown. The option set still allows the machine to idle-sleep.
#[cfg(target_os = "macos")]
fn set_macos_app_nap_suppressed(suppress: bool) {
    use std::ffi::c_void;
    use std::os::raw::c_char;
    use std::sync::atomic::AtomicUsize;

    // Retained NSProcessInfo activity token (as usize) held between begin/end.
    // 0 means "no activity currently held".
    static ACTIVITY_TOKEN: AtomicUsize = AtomicUsize::new(0);

    #[link(name = "objc")]
    extern "C" {
        fn objc_getClass(name: *const c_char) -> *mut c_void;
        fn sel_registerName(name: *const c_char) -> *mut c_void;
        fn objc_msgSend();
    }

    // NSActivityOptions, from <Foundation/NSProcessInfo.h>:
    //   NSActivityUserInitiatedAllowingIdleSystemSleep = 0x00EFFFFF
    //   NSActivityLatencyCritical                      = 0xFF00000000
    const NS_ACTIVITY_USER_INITIATED_ALLOWING_IDLE_SYSTEM_SLEEP: u64 = 0x00EF_FFFF;
    const NS_ACTIVITY_LATENCY_CRITICAL: u64 = 0xFF_0000_0000;

    unsafe {
        let process_info_class = objc_getClass(b"NSProcessInfo\0".as_ptr() as *const c_char);
        if process_info_class.is_null() {
            return;
        }
        let process_info_sel = sel_registerName(b"processInfo\0".as_ptr() as *const c_char);
        let shared: extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void =
            std::mem::transmute(objc_msgSend as *const ());
        let process_info = shared(process_info_class, process_info_sel);
        if process_info.is_null() {
            return;
        }

        if suppress {
            if ACTIVITY_TOKEN.load(Ordering::Relaxed) != 0 {
                return; // already suppressing
            }
            let string_class = objc_getClass(b"NSString\0".as_ptr() as *const c_char);
            let string_sel = sel_registerName(b"stringWithUTF8String:\0".as_ptr() as *const c_char);
            let make_string: extern "C" fn(*mut c_void, *mut c_void, *const c_char) -> *mut c_void =
                std::mem::transmute(objc_msgSend as *const ());
            let reason = make_string(
                string_class,
                string_sel,
                b"MyKVM forwarding keyboard and mouse\0".as_ptr() as *const c_char,
            );

            let begin_sel =
                sel_registerName(b"beginActivityWithOptions:reason:\0".as_ptr() as *const c_char);
            let begin: extern "C" fn(*mut c_void, *mut c_void, u64, *mut c_void) -> *mut c_void =
                std::mem::transmute(objc_msgSend as *const ());
            let options = NS_ACTIVITY_USER_INITIATED_ALLOWING_IDLE_SYSTEM_SLEEP
                | NS_ACTIVITY_LATENCY_CRITICAL;
            let activity = begin(process_info, begin_sel, options, reason);
            if activity.is_null() {
                return;
            }
            // The returned activity is autoreleased; retain it so it survives
            // past the current autorelease pool until we explicitly end it.
            let retain_sel = sel_registerName(b"retain\0".as_ptr() as *const c_char);
            let retain: extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void =
                std::mem::transmute(objc_msgSend as *const ());
            let retained = retain(activity, retain_sel);
            ACTIVITY_TOKEN.store(retained as usize, Ordering::Relaxed);
        } else {
            let token = ACTIVITY_TOKEN.swap(0, Ordering::Relaxed);
            if token == 0 {
                return;
            }
            let activity = token as *mut c_void;
            let end_sel = sel_registerName(b"endActivity:\0".as_ptr() as *const c_char);
            let end: extern "C" fn(*mut c_void, *mut c_void, *mut c_void) =
                std::mem::transmute(objc_msgSend as *const ());
            end(process_info, end_sel, activity);
            let release_sel = sel_registerName(b"release\0".as_ptr() as *const c_char);
            let release: extern "C" fn(*mut c_void, *mut c_void) =
                std::mem::transmute(objc_msgSend as *const ());
            release(activity, release_sel);
        }
    }
}

#[cfg(target_os = "macos")]
fn set_macos_cursor_hidden_with_appkit(hidden: bool) {
    use std::ffi::c_void;
    use std::os::raw::c_char;

    #[link(name = "objc")]
    extern "C" {
        fn objc_getClass(name: *const c_char) -> *mut c_void;
        fn sel_registerName(name: *const c_char) -> *mut c_void;
        fn objc_msgSend();
    }

    unsafe {
        let class = objc_getClass(b"NSCursor\0".as_ptr() as *const c_char);
        if class.is_null() {
            return;
        }
        let selector = if hidden {
            sel_registerName(b"hide\0".as_ptr() as *const c_char)
        } else {
            sel_registerName(b"unhide\0".as_ptr() as *const c_char)
        };
        let msg_void: extern "C" fn(*mut c_void, *mut c_void) =
            std::mem::transmute(objc_msgSend as *const ());
        msg_void(class, selector);
    }
}

/// Push a fully-transparent cursor onto the AppKit cursor stack while a remote
/// session is active, then pop it on return.
///
/// `CGDisplayHideCursor` / `NSCursor hide` proved unreliable for a background
/// app: WindowServer services them lazily, so the pointer visibly lingers at the
/// shared edge for a fraction of a second on every crossing — even when we
/// re-issue hide every 50ms. A transparent cursor has no hidden/visible state
/// to flip: it just paints nothing, so there is nothing for WindowServer to
/// "un-hide". `push`/`pop` modify this app's active cursor image, which is far
/// more robust than the global hide counter when MyKVM is not frontmost.
#[cfg(target_os = "macos")]
fn set_macos_cursor_transparent(transparent: bool) {
    use std::ffi::c_void;
    use std::os::raw::{c_char, c_double};

    #[link(name = "objc")]
    extern "C" {
        fn objc_getClass(name: *const c_char) -> *mut c_void;
        fn sel_registerName(name: *const c_char) -> *mut c_void;
        fn objc_msgSend();
    }

    // A 16x16 fully-transparent RGBA bitmap. NSImage created from this paints
    // nothing, so the cursor is visually absent without a hide/show call.
    const SIZE: usize = 16;
    static TRANSPARENT_BYTES: [u8; SIZE * SIZE * 4] = [0; SIZE * SIZE * 4];

    unsafe {
        let nscursor = objc_getClass(b"NSCursor\0".as_ptr() as *const c_char);
        let nsimage = objc_getClass(b"NSImage\0".as_ptr() as *const c_char);
        let nsdata = objc_getClass(b"NSData\0".as_ptr() as *const c_char);
        let nssize = objc_getClass(b"NSSize\0".as_ptr() as *const c_char);
        if nscursor.is_null() || nsimage.is_null() || nsdata.is_null() || nssize.is_null() {
            return;
        }

        if !transparent {
            // Pop our transparent cursor to restore the previous cursor image.
            let pop_sel = sel_registerName(b"pop\0".as_ptr() as *const c_char);
            let pop: extern "C" fn(*mut c_void, *mut c_void) =
                std::mem::transmute(objc_msgSend as *const ());
            pop(nscursor, pop_sel);
            return;
        }

        // NSData dataWithBytes:length:
        let data_sel = sel_registerName(b"dataWithBytes:length:\0".as_ptr() as *const c_char);
        let data_with: extern "C" fn(*mut c_void, *mut c_void, *const u8, usize) -> *mut c_void =
            std::mem::transmute(objc_msgSend as *const ());
        let data = data_with(
            nsdata,
            data_sel,
            TRANSPARENT_BYTES.as_ptr(),
            TRANSPARENT_BYTES.len(),
        );
        if data.is_null() {
            return;
        }

        // NSImage initWithData:
        let alloc_sel = sel_registerName(b"alloc\0".as_ptr() as *const c_char);
        let init_sel = sel_registerName(b"initWithData:\0".as_ptr() as *const c_char);
        let alloc: extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void =
            std::mem::transmute(objc_msgSend as *const ());
        let init: extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> *mut c_void =
            std::mem::transmute(objc_msgSend as *const ());
        let image_obj = alloc(nsimage, alloc_sel);
        let image = init(image_obj, init_sel, data);
        if image.is_null() {
            return;
        }

        // NSSize { width, height } value type, laid out as two doubles.
        let size_sel = sel_registerName(b"setSize:\0".as_ptr() as *const c_char);
        let set_size: extern "C" fn(*mut c_void, *mut c_void, c_double, c_double) =
            std::mem::transmute(objc_msgSend as *const ());
        set_size(image, size_sel, SIZE as c_double, SIZE as c_double);

        // NSCursor initWithImage:hotSpot: — hot spot at (0,0), anywhere is fine
        // since the cursor is invisible. hotSpot is an NSPoint (two CGFloats),
        // passed by value; on arm64 that lands in the float argument registers as
        // two doubles after the object-pointer argument.
        let cursor_init_sel =
            sel_registerName(b"initWithImage:hotSpot:\0".as_ptr() as *const c_char);
        let cursor_init: extern "C" fn(
            *mut c_void,
            *mut c_void,
            *mut c_void,
            c_double,
            c_double,
        ) -> *mut c_void = std::mem::transmute(objc_msgSend as *const ());
        let cursor_obj = alloc(nscursor, alloc_sel);
        let cursor = cursor_init(cursor_obj, cursor_init_sel, image, 0.0, 0.0);
        if cursor.is_null() {
            return;
        }

        // [NSCursor push] — make it the active cursor.
        let push_sel = sel_registerName(b"push\0".as_ptr() as *const c_char);
        let push: extern "C" fn(*mut c_void, *mut c_void) =
            std::mem::transmute(objc_msgSend as *const ());
        push(cursor, push_sel);
    }
}

#[cfg(target_os = "macos")]
fn repin_macos_cursor_if_drifted(
    context: &MacCaptureContext,
    location: core_graphics::geometry::CGPoint,
) {
    const DRIFT_THRESHOLD_PX: f64 = 1.5;
    const REPIN_INTERVAL_MS: u64 = 8;

    let Ok(anchor) = context.anchor.lock() else {
        return;
    };
    let Some((x, y)) = *anchor else {
        return;
    };
    drop(anchor);

    let dx = location.x - x;
    let dy = location.y - y;
    if dx.abs() <= DRIFT_THRESHOLD_PX && dy.abs() <= DRIFT_THRESHOLD_PX {
        return;
    }

    if !macos_cursor_repin_due(context, Duration::from_millis(REPIN_INTERVAL_MS)) {
        return;
    }

    // When MyKVM is not frontmost, macOS can re-associate the cursor with the
    // physical mouse despite CGAssociateMouseAndMouseCursorPosition(false).
    // Re-pin only after actual drift and at a capped rate.
    set_macos_cursor_decoupled(true);
    move_macos_cursor_without_event(context, core_graphics::geometry::CGPoint::new(x, y));
}

#[cfg(target_os = "macos")]
fn force_repin_macos_cursor_to_anchor(context: &MacCaptureContext) {
    let Ok(anchor) = context.anchor.lock() else {
        return;
    };
    let Some((x, y)) = *anchor else {
        return;
    };
    drop(anchor);

    move_macos_cursor_without_event(context, core_graphics::geometry::CGPoint::new(x, y));
}

#[cfg(target_os = "macos")]
fn macos_cursor_repin_due(context: &MacCaptureContext, interval: Duration) -> bool {
    let Ok(mut last_repin) = context.last_cursor_repin.lock() else {
        return true;
    };
    let now = Instant::now();
    if last_repin
        .as_ref()
        .map(|last| now.duration_since(*last) < interval)
        .unwrap_or(false)
    {
        return false;
    }
    *last_repin = Some(now);
    true
}

#[cfg(target_os = "macos")]
fn macos_current_cursor_location() -> Option<core_graphics::geometry::CGPoint> {
    use core_graphics::{
        event::CGEvent,
        event_source::{CGEventSource, CGEventSourceStateID},
    };

    let source = CGEventSource::new(CGEventSourceStateID::CombinedSessionState).ok()?;
    CGEvent::new(source).ok().map(|event| event.location())
}

#[cfg(target_os = "macos")]
fn reset_cursor_repin_timer(context: &MacCaptureContext) {
    if let Ok(mut last_repin) = context.last_cursor_repin.lock() {
        *last_repin = None;
    }
}

#[cfg(target_os = "macos")]
fn reassert_macos_hidden_window_cursor(context: &MacCaptureContext) {
    let Ok(hidden) = context.cursor_hidden.lock() else {
        return;
    };
    if !*hidden {
        return;
    }
    drop(hidden);

    let Ok(mut last_reassert) = context.last_cursor_hide_reassert.lock() else {
        return;
    };
    let now = Instant::now();
    if last_reassert
        .as_ref()
        .map(|last| {
            now.duration_since(*last)
                < Duration::from_millis(MACOS_HIDDEN_WINDOW_CURSOR_HIDE_REASSERT_MS)
        })
        .unwrap_or(false)
    {
        return;
    }
    *last_reassert = Some(now);
    drop(last_reassert);

    push_macos_cursor_hide(context);
}

#[cfg(target_os = "macos")]
fn mac_display_snapshots() -> Vec<MacDisplaySnapshot> {
    use core_graphics::display::CGDisplay;

    CGDisplay::active_displays()
        .unwrap_or_default()
        .into_iter()
        .map(|display_id| {
            let display = CGDisplay::new(display_id);
            let bounds = display.bounds();
            MacDisplaySnapshot {
                id: display_id,
                origin_x: bounds.origin.x,
                origin_y: bounds.origin.y,
                max_x: bounds.origin.x + bounds.size.width,
                max_y: bounds.origin.y + bounds.size.height,
            }
        })
        .collect()
}

#[cfg(target_os = "macos")]
fn move_macos_cursor_without_event(
    context: &MacCaptureContext,
    point: core_graphics::geometry::CGPoint,
) {
    move_macos_cursor_without_event_on_displays(point, &context.display_snapshots);
}

#[cfg(target_os = "macos")]
fn move_macos_cursor_without_event_on_displays(
    point: core_graphics::geometry::CGPoint,
    displays: &[MacDisplaySnapshot],
) {
    use core_graphics::display::CGDisplay;

    for display in displays {
        if point.x >= display.origin_x
            && point.x <= display.max_x
            && point.y >= display.origin_y
            && point.y <= display.max_y
        {
            let local_point = core_graphics::geometry::CGPoint::new(
                point.x - display.origin_x,
                point.y - display.origin_y,
            );
            if CGDisplay::new(display.id)
                .move_cursor_to_point(local_point)
                .is_ok()
            {
                return;
            }
        }
    }

    let _ = CGDisplay::warp_mouse_cursor_position(point);
}

/// Arms macOS to hide the pointer even when MyKVM is NOT the frontmost app.
///
/// `CGDisplayHideCursor` / `[NSCursor hide]` are normally honored only while the
/// calling app is frontmost, so once MyKVM is minimized / backgrounded / its
/// window is closed, the local cursor reappears at the screen edge during a
/// crossing — the "not seamless, cursor shows up" symptom. Setting the private
/// CGS connection property `SetsCursorInBackground` to true makes the hide stick
/// regardless of focus. The symbols are resolved at runtime via `dlsym` so a
/// macOS build that has moved/removed them (they live in CoreGraphics today,
/// SkyLight on newer systems) degrades gracefully instead of failing to link.
#[cfg(target_os = "macos")]
fn enable_macos_background_cursor_hide() {
    use core_foundation::{base::TCFType, boolean::CFBoolean, string::CFString};
    use std::os::raw::{c_char, c_int, c_void};

    extern "C" {
        fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    }
    // RTLD_DEFAULT on macOS searches every already-loaded image.
    const RTLD_DEFAULT: *mut c_void = -2isize as *mut c_void;

    static ENABLED: AtomicBool = AtomicBool::new(false);
    if ENABLED.swap(true, Ordering::Relaxed) {
        return;
    }

    unsafe {
        let main_conn = dlsym(
            RTLD_DEFAULT,
            b"CGSMainConnectionID\0".as_ptr() as *const c_char,
        );
        let set_prop = dlsym(
            RTLD_DEFAULT,
            b"CGSSetConnectionProperty\0".as_ptr() as *const c_char,
        );
        if main_conn.is_null() || set_prop.is_null() {
            return;
        }

        let main_conn: extern "C" fn() -> c_int = std::mem::transmute(main_conn);
        let set_prop: extern "C" fn(c_int, c_int, *const c_void, *const c_void) -> c_int =
            std::mem::transmute(set_prop);

        let cid = main_conn();
        let key = CFString::from_static_string("SetsCursorInBackground");
        let value = CFBoolean::true_value();
        let _ = set_prop(
            cid,
            cid,
            key.as_concrete_TypeRef() as *const c_void,
            value.as_CFTypeRef() as *const c_void,
        );
        // Hold the CF objects until the call returns.
        drop(key);
        drop(value);
    }
}

#[cfg(target_os = "macos")]
fn hide_macos_cursor_if_needed(context: &MacCaptureContext) {
    let Ok(mut hidden) = context.cursor_hidden.lock() else {
        return;
    };
    if *hidden {
        return;
    }

    // The PRIMARY mechanism is a transparent cursor (set_macos_cursor_transparent):
    // CGDisplayHideCursor / NSCursor hide are unreliable for a background app
    // (WindowServer services them lazily, pointer flickers at the edge). The
    // transparent cursor paints nothing with no hide/show state to flip. We keep
    // the hide calls as a secondary belt-and-suspenders, but they are no longer
    // the thing we rely on.
    enable_macos_background_cursor_hide();
    set_macos_cursor_transparent(true);
    push_macos_cursor_hide(context);
    if let Ok(mut last_reassert) = context.last_cursor_hide_reassert.lock() {
        *last_reassert = None;
    }
    log::debug!("[diag] transparent cursor pushed + hide issued (cursor_hidden false->true)");
    *hidden = true;
}

#[cfg(target_os = "macos")]
fn push_macos_cursor_hide(context: &MacCaptureContext) {
    let Ok(mut depth) = context.cursor_hide_depth.lock() else {
        return;
    };

    set_macos_cursor_hidden_with_appkit(true);
    if context.display_snapshots.is_empty() {
        let _ = core_graphics::display::CGDisplay::main().hide_cursor();
    } else {
        for display in &context.display_snapshots {
            let _ = core_graphics::display::CGDisplay::new(display.id).hide_cursor();
        }
    }
    *depth = depth.saturating_add(1);
}

#[cfg(target_os = "macos")]
fn show_macos_cursor_if_needed(context: &MacCaptureContext) {
    let Ok(mut hidden) = context.cursor_hidden.lock() else {
        return;
    };
    if !*hidden {
        return;
    }

    // Pop the transparent cursor first — this restores the real cursor image
    // and is the reliable inverse of the hide. The CGDisplay/NSCursor show calls
    // balance the secondary hide calls.
    set_macos_cursor_transparent(false);
    drain_macos_cursor_hide(context);
    if let Ok(mut last_reassert) = context.last_cursor_hide_reassert.lock() {
        *last_reassert = None;
    }
    *hidden = false;
    log::debug!("[diag] transparent cursor popped + show issued (cursor_hidden true->false)");
}

#[cfg(target_os = "macos")]
fn drain_macos_cursor_hide(context: &MacCaptureContext) {
    let count = context
        .cursor_hide_depth
        .lock()
        .map(|mut depth| {
            let count = *depth;
            *depth = 0;
            count
        })
        .unwrap_or(0);

    for _ in 0..count {
        if context.display_snapshots.is_empty() {
            let _ = core_graphics::display::CGDisplay::main().show_cursor();
        } else {
            for display in &context.display_snapshots {
                let _ = core_graphics::display::CGDisplay::new(display.id).show_cursor();
            }
        }
        set_macos_cursor_hidden_with_appkit(false);
    }
}

#[cfg(target_os = "macos")]
fn handle_macos_modifier_event(
    context: &MacCaptureContext,
    event_type: core_graphics::event::CGEventType,
    event: &core_graphics::event::CGEvent,
) -> core_graphics::event::CallbackResult {
    if matches!(event_type, core_graphics::event::CGEventType::FlagsChanged) {
        if let Ok(mut pressed) = context.pressed_modifiers.lock() {
            *pressed = mac_modifier_vks(event);
        }
    }

    core_graphics::event::CallbackResult::Keep
}

#[cfg(target_os = "macos")]
fn send_modifier_changes(
    context: &MacCaptureContext,
    target: &InputTarget,
    event: &core_graphics::event::CGEvent,
) {
    use core_graphics::event::EventField;

    let mac_code = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16;
    if mac_code == 57 {
        if let Some(key_code) = mac_key_to_windows_vk(mac_code) {
            send_packet(
                &context.quic_transport,
                target,
                InputEvent::Key {
                    key_code,
                    down: true,
                },
                &context.layout_state,
                &context.input_events,
            );
            send_packet(
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
        return;
    }

    let next = mac_modifier_vks(event);
    let Ok(mut previous) = context.pressed_modifiers.lock() else {
        return;
    };

    for key_code in next.iter().filter(|key_code| !previous.contains(key_code)) {
        send_packet(
            &context.quic_transport,
            target,
            InputEvent::Key {
                key_code: *key_code,
                down: true,
            },
            &context.layout_state,
            &context.input_events,
        );
    }

    for key_code in previous.iter().filter(|key_code| !next.contains(key_code)) {
        send_packet(
            &context.quic_transport,
            target,
            InputEvent::Key {
                key_code: *key_code,
                down: false,
            },
            &context.layout_state,
            &context.input_events,
        );
    }

    *previous = next;
}

#[cfg(target_os = "macos")]
fn mac_modifier_vks(event: &core_graphics::event::CGEvent) -> Vec<u16> {
    use core_graphics::event::CGEventFlags;

    let flags = event.get_flags();
    let mut keys = Vec::new();
    if flags.contains(CGEventFlags::CGEventFlagShift) {
        keys.push(0x10);
    }
    if flags.contains(CGEventFlags::CGEventFlagControl) {
        keys.push(0x11);
    }
    if flags.contains(CGEventFlags::CGEventFlagAlternate) {
        keys.push(0x12);
    }
    if flags.contains(CGEventFlags::CGEventFlagCommand) {
        keys.push(0x5B);
    }
    keys
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn mac_key_to_windows_vk(code: u16) -> Option<u16> {
    Some(match code {
        0 => 0x41,
        1 => 0x53,
        2 => 0x44,
        3 => 0x46,
        4 => 0x48,
        5 => 0x47,
        6 => 0x5A,
        7 => 0x58,
        8 => 0x43,
        9 => 0x56,
        11 => 0x42,
        12 => 0x51,
        13 => 0x57,
        14 => 0x45,
        15 => 0x52,
        16 => 0x59,
        17 => 0x54,
        18 => 0x31,
        19 => 0x32,
        20 => 0x33,
        21 => 0x34,
        22 => 0x36,
        23 => 0x35,
        24 => 0xBB,
        25 => 0x39,
        26 => 0x37,
        27 => 0xBD,
        28 => 0x38,
        29 => 0x30,
        30 => 0xDD,
        31 => 0x4F,
        32 => 0x55,
        33 => 0xDB,
        34 => 0x49,
        35 => 0x50,
        36 => 0x0D,
        37 => 0x4C,
        38 => 0x4A,
        39 => 0xDE,
        40 => 0x4B,
        41 => 0xBA,
        42 => 0xDC,
        43 => 0xBC,
        44 => 0xBF,
        45 => 0x4E,
        46 => 0x4D,
        47 => 0xBE,
        48 => 0x09,
        49 => 0x20,
        50 => 0xC0,
        51 => 0x08,
        53 => 0x1B,
        54 => 0x5C,
        55 => 0x5B,
        56 => 0xA0,
        57 => 0x14,
        58 => 0xA4,
        59 => 0xA2,
        60 => 0xA1,
        61 => 0xA5,
        62 => 0xA3,
        63 => 0x5B,
        64 => 0x80,
        65 => 0x6E,
        67 => 0x6A,
        69 => 0x6B,
        71 => 0x90,
        75 => 0x6F,
        76 => 0x0D,
        78 => 0x6D,
        81 => 0x6D,
        82 => 0x60,
        83 => 0x61,
        84 => 0x62,
        85 => 0x63,
        86 => 0x64,
        87 => 0x65,
        88 => 0x66,
        89 => 0x67,
        91 => 0x68,
        92 => 0x69,
        96 => 0x74,
        97 => 0x75,
        98 => 0x76,
        99 => 0x72,
        100 => 0x77,
        101 => 0x78,
        103 => 0x7A,
        105 => 0x7C,
        106 => 0x7F,
        107 => 0x7D,
        109 => 0x79,
        111 => 0x7B,
        114 => 0x2D,
        115 => 0x24,
        116 => 0x21,
        117 => 0x2E,
        118 => 0x73,
        119 => 0x23,
        120 => 0x71,
        121 => 0x22,
        122 => 0x70,
        123 => 0x25,
        124 => 0x27,
        125 => 0x28,
        126 => 0x26,
        _ => return None,
    })
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn windows_vk_to_mac_key(code: u16) -> Option<u16> {
    mac_key_to_windows_vk_pairs()
        .iter()
        .find(|(_, vk)| *vk == code)
        .map(|(mac, _)| *mac)
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn mac_key_to_windows_vk_pairs() -> &'static [(u16, u16)] {
    &[
        (0, 0x41),
        (1, 0x53),
        (2, 0x44),
        (3, 0x46),
        (4, 0x48),
        (5, 0x47),
        (6, 0x5A),
        (7, 0x58),
        (8, 0x43),
        (9, 0x56),
        (11, 0x42),
        (12, 0x51),
        (13, 0x57),
        (14, 0x45),
        (15, 0x52),
        (16, 0x59),
        (17, 0x54),
        (18, 0x31),
        (19, 0x32),
        (20, 0x33),
        (21, 0x34),
        (22, 0x36),
        (23, 0x35),
        (24, 0xBB),
        (25, 0x39),
        (26, 0x37),
        (27, 0xBD),
        (28, 0x38),
        (29, 0x30),
        (30, 0xDD),
        (31, 0x4F),
        (32, 0x55),
        (33, 0xDB),
        (34, 0x49),
        (35, 0x50),
        (36, 0x0D),
        (37, 0x4C),
        (38, 0x4A),
        (39, 0xDE),
        (40, 0x4B),
        (41, 0xBA),
        (42, 0xDC),
        (43, 0xBC),
        (44, 0xBF),
        (45, 0x4E),
        (46, 0x4D),
        (47, 0xBE),
        (48, 0x09),
        (49, 0x20),
        (50, 0xC0),
        (51, 0x08),
        (53, 0x1B),
        (54, 0x5C),
        (55, 0x5B),
        (56, 0x10),
        (56, 0xA0),
        (57, 0x14),
        (58, 0x12),
        (58, 0xA4),
        (59, 0x11),
        (59, 0xA2),
        (60, 0xA1),
        (61, 0xA5),
        (62, 0xA3),
        (63, 0x5B),
        (64, 0x80),
        (65, 0x6E),
        (67, 0x6A),
        (69, 0x6B),
        (71, 0x90),
        (75, 0x6F),
        (76, 0x0D),
        (78, 0x6D),
        (81, 0x6D),
        (82, 0x60),
        (83, 0x61),
        (84, 0x62),
        (85, 0x63),
        (86, 0x64),
        (87, 0x65),
        (88, 0x66),
        (89, 0x67),
        (91, 0x68),
        (92, 0x69),
        (96, 0x74),
        (97, 0x75),
        (98, 0x76),
        (99, 0x72),
        (100, 0x77),
        (101, 0x78),
        (103, 0x7A),
        (105, 0x7C),
        (106, 0x7F),
        (107, 0x7D),
        (109, 0x79),
        (111, 0x7B),
        (114, 0x2D),
        (115, 0x24),
        (116, 0x21),
        (117, 0x2E),
        (118, 0x73),
        (119, 0x23),
        (120, 0x71),
        (121, 0x22),
        (122, 0x70),
        (123, 0x25),
        (124, 0x27),
        (125, 0x28),
        (126, 0x26),
    ]
}

#[cfg(target_os = "macos")]
fn inject_mouse_move(x: i32, y: i32, drag_button: Option<MouseButton>) {
    use core_graphics::{
        display::CGDisplay,
        event::{CGEvent, CGEventTapLocation, CGEventType, CGMouseButton},
        event_source::{CGEventSource, CGEventSourceStateID},
        geometry::CGPoint,
    };

    let point = CGPoint::new(x as f64, y as f64);
    let (event_type, mouse_button) = match drag_button {
        Some(MouseButton::Left) => (CGEventType::LeftMouseDragged, CGMouseButton::Left),
        Some(MouseButton::Right) => (CGEventType::RightMouseDragged, CGMouseButton::Right),
        Some(MouseButton::Middle) => (CGEventType::OtherMouseDragged, CGMouseButton::Center),
        None => (CGEventType::MouseMoved, CGMouseButton::Left),
    };

    // Posted mouse-move events do not always update the visible macOS cursor.
    let _ = CGDisplay::warp_mouse_cursor_position(point);

    if let Ok(source) = CGEventSource::new(CGEventSourceStateID::HIDSystemState) {
        if let Ok(event) = CGEvent::new_mouse_event(source, event_type, point, mouse_button) {
            event.post(CGEventTapLocation::HID);
        }
    }
}

#[cfg(target_os = "macos")]
fn inject_mouse_button(button: MouseButton, down: bool, x: i32, y: i32) {
    use core_graphics::{
        display::CGDisplay,
        event::{CGEvent, CGEventTapLocation, CGEventType, CGMouseButton},
        event_source::{CGEventSource, CGEventSourceStateID},
        geometry::CGPoint,
    };

    let Ok(source) = CGEventSource::new(CGEventSourceStateID::HIDSystemState) else {
        return;
    };
    let (event_type, mouse_button) = match (button, down) {
        (MouseButton::Left, true) => (CGEventType::LeftMouseDown, CGMouseButton::Left),
        (MouseButton::Left, false) => (CGEventType::LeftMouseUp, CGMouseButton::Left),
        (MouseButton::Right, true) => (CGEventType::RightMouseDown, CGMouseButton::Right),
        (MouseButton::Right, false) => (CGEventType::RightMouseUp, CGMouseButton::Right),
        (MouseButton::Middle, true) => (CGEventType::OtherMouseDown, CGMouseButton::Center),
        (MouseButton::Middle, false) => (CGEventType::OtherMouseUp, CGMouseButton::Center),
    };
    let point = CGPoint::new(x as f64, y as f64);

    let _ = CGDisplay::warp_mouse_cursor_position(point);

    if let Ok(event) = CGEvent::new_mouse_event(source, event_type, point, mouse_button) {
        event.post(CGEventTapLocation::HID);
    }
}

#[cfg(target_os = "macos")]
fn inject_scroll(delta_x: i32, delta_y: i32) {
    use core_graphics::{
        event::{CGEvent, CGEventTapLocation, ScrollEventUnit},
        event_source::{CGEventSource, CGEventSourceStateID},
    };

    let Ok(source) = CGEventSource::new(CGEventSourceStateID::HIDSystemState) else {
        return;
    };
    if let Ok(event) =
        CGEvent::new_scroll_event(source, ScrollEventUnit::LINE, 2, delta_y, delta_x, 0)
    {
        event.post(CGEventTapLocation::HID);
    }
}

/// Held modifier flags to stamp on injected macOS events. Posting a bare
/// modifier *keycode* does not make the window server apply that modifier to the
/// key events posted after it, so capitals, shifted symbols and every shortcut
/// (including the Ctrl<->Cmd remap) silently failed. We instead track the
/// modifier key-downs/ups we inject and set the matching CGEventFlags on each
/// event.
#[cfg(target_os = "macos")]
static MAC_INJECT_FLAGS: AtomicU64 = AtomicU64::new(0);

/// Clears the tracked injected-modifier flags. Called when receiving stops so a
/// dropped modifier key-up cannot leave Shift/Ctrl/Cmd stuck on for later keys.
#[cfg(target_os = "macos")]
pub fn reset_injected_modifiers() {
    MAC_INJECT_FLAGS.store(0, Ordering::Relaxed);
}

#[cfg(not(target_os = "macos"))]
pub fn reset_injected_modifiers() {}

/// Maps a Windows virtual-key modifier (the wire format) to its macOS event
/// flag bits, or `None` for non-modifier keys.
#[cfg(target_os = "macos")]
fn windows_vk_to_mac_flag(vk: u16) -> Option<u64> {
    use core_graphics::event::CGEventFlags;
    let flag = match vk {
        0x10 | 0xA0 | 0xA1 => CGEventFlags::CGEventFlagShift,
        0x11 | 0xA2 | 0xA3 => CGEventFlags::CGEventFlagControl,
        0x12 | 0xA4 | 0xA5 => CGEventFlags::CGEventFlagAlternate,
        0x5B | 0x5C => CGEventFlags::CGEventFlagCommand,
        _ => return None,
    };
    Some(flag.bits())
}

#[cfg(target_os = "macos")]
fn inject_key(key_code: u16, down: bool) {
    use core_graphics::{
        event::{CGEvent, CGEventFlags, CGEventTapLocation},
        event_source::{CGEventSource, CGEventSourceStateID},
    };

    // Keep the running modifier state in sync, so the modifier event itself and
    // every later key carry the right flags.
    if let Some(flag) = windows_vk_to_mac_flag(key_code) {
        let mut flags = MAC_INJECT_FLAGS.load(Ordering::Relaxed);
        if down {
            flags |= flag;
        } else {
            flags &= !flag;
        }
        MAC_INJECT_FLAGS.store(flags, Ordering::Relaxed);
    }

    let Some(mac_code) = windows_vk_to_mac_key(key_code) else {
        log::debug!("inject_key: no mac keycode for windows vk {key_code:#04x}; dropping");
        return;
    };
    let Ok(source) = CGEventSource::new(CGEventSourceStateID::HIDSystemState) else {
        log::warn!("inject_key: failed to create CGEventSource");
        return;
    };
    match CGEvent::new_keyboard_event(source, mac_code, down) {
        Ok(event) => {
            event.set_flags(CGEventFlags::from_bits_truncate(
                MAC_INJECT_FLAGS.load(Ordering::Relaxed),
            ));
            event.post(CGEventTapLocation::HID);
        }
        Err(_) => log::warn!("inject_key: failed to build keyboard event for mac code {mac_code}"),
    }
}

#[cfg(target_os = "windows")]
fn inject_mouse_move(x: i32, y: i32, drag_button: Option<MouseButton>) {
    crate::windows_input::inject_mouse_move(x, y, drag_button);
}

#[cfg(target_os = "windows")]
fn inject_mouse_button(button: MouseButton, down: bool, x: i32, y: i32) {
    crate::windows_input::inject_mouse_button(button, down, x, y);
}

#[cfg(target_os = "windows")]
fn inject_scroll(delta_x: i32, delta_y: i32) {
    crate::windows_input::inject_scroll(delta_x, delta_y);
}

#[cfg(target_os = "windows")]
fn inject_key(key_code: u16, down: bool) {
    crate::windows_input::inject_key(key_code, down);
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn inject_mouse_move(_x: i32, _y: i32, _drag_button: Option<MouseButton>) {}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn inject_mouse_button(_button: MouseButton, _down: bool, _x: i32, _y: i32) {}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn inject_scroll(_delta_x: i32, _delta_y: i32) {}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn inject_key(_key_code: u16, _down: bool) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn windows_vk_to_mac_flag_covers_modifiers() {
        // Modifiers (incl. sided variants and LWin/RWin -> Command) map to a flag.
        assert!(windows_vk_to_mac_flag(0x10).is_some()); // Shift
        assert!(windows_vk_to_mac_flag(0xA1).is_some()); // Right Shift
        assert!(windows_vk_to_mac_flag(0x11).is_some()); // Control
        assert!(windows_vk_to_mac_flag(0x12).is_some()); // Alt -> Option
        assert!(windows_vk_to_mac_flag(0x5B).is_some()); // LWin -> Command

        // Ordinary keys carry no modifier flag.
        assert!(windows_vk_to_mac_flag(0x41).is_none()); // 'A'
        assert!(windows_vk_to_mac_flag(0x20).is_none()); // Space
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_raw_gesture_mask_covers_trackpad_system_gestures() {
        let mask = macos_raw_gesture_event_mask();

        for event_type in MACOS_RAW_GESTURE_EVENT_TYPES {
            assert_ne!(mask & (1_u64 << *event_type), 0);
        }
        assert_ne!(mask & (1_u64 << MACOS_NSEVENT_TYPE_SWIPE), 0);
        assert_ne!(mask & (1_u64 << MACOS_NSEVENT_TYPE_SYSTEM_DEFINED), 0);
        assert_eq!(mask & (1_u64 << 22), 0);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_hidden_remote_loop_is_slower_than_visible_remote_loop() {
        assert_eq!(
            macos_capture_loop_ms(false, false),
            MACOS_IDLE_CAPTURE_LOOP_MS
        );
        assert_eq!(
            macos_capture_loop_ms(true, true),
            MACOS_VISIBLE_REMOTE_CAPTURE_LOOP_MS
        );
        assert_eq!(
            macos_capture_loop_ms(true, false),
            MACOS_HIDDEN_REMOTE_CAPTURE_LOOP_MS
        );
        assert!(MACOS_HIDDEN_REMOTE_CAPTURE_LOOP_MS > MACOS_VISIBLE_REMOTE_CAPTURE_LOOP_MS);
    }

    fn screen(device_id: &str, id: &str, x: i32, y: i32, width: i32, height: i32) -> Screen {
        Screen {
            id: id.into(),
            device_id: device_id.into(),
            name: id.into(),
            x,
            y,
            width,
            height,
            scale: 1.0,
            is_primary: true,
        }
    }

    fn target_for_coordinate_tests() -> InputTarget {
        InputTarget {
            device_id: "peer-device".into(),
            origin_device_id: "peer-local-192-168-66-92".into(),
            cluster_id: "cluster-test".into(),
            pair_secret: "secret-test".into(),
            target_addr: "10.0.0.2:47833".into(),
            target_platform: "windows".into(),
            transport_public_key: "test-public-key".into(),
            protocol_version: quic_transport::PROTOCOL_VERSION,
            screen_id: "local-display-1".into(),
            local_screen: screen("local-device", "local-display-1", 0, 0, 1920, 1080),
            layout_local_screen: screen(
                "local-device",
                "local-display-1",
                -11960,
                -9000,
                2560,
                1440,
            ),
            remote_screen: screen(
                "peer-device",
                "peer-device-local-display-1",
                -9400,
                -9000,
                2560,
                1440,
            ),
            edge: Edge::Right,
        }
    }

    fn layout_for_target_tests() -> LayoutState {
        LayoutState {
            devices: vec![
                Device {
                    id: "local-device".into(),
                    name: "Local".into(),
                    platform: "macos".into(),
                    host: "192.168.66.92".into(),
                    transport_port: 47833,
                    quic_port: 47834,
                    transport_public_key: "local-public-key".into(),
                    protocol_version: quic_transport::PROTOCOL_VERSION,
                    color: "#2f7af8".into(),
                    online: true,
                    input_ready: false,
                    upgrading: false,
                    upgrading_until_ms: 0,
                    role: "local".into(),
                    source: "detected".into(),
                    screens: vec![screen("local-device", "local-display-1", 0, 0, 1920, 1080)],
                },
                Device {
                    id: "peer-device".into(),
                    name: "Client".into(),
                    platform: "windows".into(),
                    host: "10.0.0.2".into(),
                    transport_port: 52000,
                    quic_port: 52001,
                    transport_public_key: "peer-public-key".into(),
                    protocol_version: quic_transport::PROTOCOL_VERSION,
                    color: "#0f766e".into(),
                    online: true,
                    input_ready: true,
                    upgrading: false,
                    upgrading_until_ms: 0,
                    role: "client".into(),
                    source: "detected".into(),
                    screens: vec![screen(
                        "peer-device",
                        "peer-device-local-display-1",
                        1920,
                        0,
                        1920,
                        1080,
                    )],
                },
            ],
            active_device_id: "local-device".into(),
            selected_screen_id: "local-display-1".into(),
            input_mode: "control".into(),
            machine_role: "server".into(),
            cluster_id: "cluster-test".into(),
            pair_secret: "secret-test".into(),
            paired_controllers: Vec::new(),
            clipboard_sync: false,
            file_transfer_enabled: true,
            language: "cn".into(),
            theme_mode: "system".into(),
            performance_monitor: false,
            transport_port_mode: "auto".into(),
            transport_port: 47833,
            quic_port: 47834,
            modifier_remap: true,
            modifier_map: crate::default_modifier_map(),
            edge_switch_hotkey: crate::default_edge_switch_hotkey(),
            screen_switch_hotkeys: crate::ScreenSwitchHotkeys::default(),
        }
    }

    #[test]
    fn cursor_roams_across_remote_device_screens() {
        // Remote device with two stacked screens: a primary and a secondary
        // directly below it (the screenshot's #10086 / #41039 arrangement).
        let device = Device {
            id: "peer-device".into(),
            name: "Client".into(),
            platform: "windows".into(),
            host: "10.0.0.2".into(),
            transport_port: 47833,
            quic_port: 47834,
            transport_public_key: "peer-public-key".into(),
            protocol_version: quic_transport::PROTOCOL_VERSION,
            color: "#0f766e".into(),
            online: true,
            input_ready: true,
            upgrading: false,
            upgrading_until_ms: 0,
            role: "client".into(),
            source: "detected".into(),
            screens: vec![
                screen("peer-device", "peer-device-scr-1", 1920, 0, 1920, 1080),
                screen("peer-device", "peer-device-scr-2", 1920, 1080, 1920, 1080),
            ],
        };
        let mut layout = layout_for_target_tests();
        layout.devices.retain(|device| device.id != "peer-device");
        layout.devices.push(device);
        let layout_state = Arc::new(Mutex::new(layout));

        let entry = screen("peer-device", "peer-device-scr-1", 1920, 0, 1920, 1080);
        let target = InputTarget {
            device_id: "peer-device".into(),
            origin_device_id: "peer-local-192-168-66-92".into(),
            cluster_id: "cluster-test".into(),
            pair_secret: "secret-test".into(),
            target_addr: "10.0.0.2:47834".into(),
            target_platform: "windows".into(),
            transport_public_key: "peer-public-key".into(),
            protocol_version: quic_transport::PROTOCOL_VERSION,
            screen_id: "scr-1".into(),
            local_screen: screen("local-device", "local-display-1", 0, 0, 1920, 1080),
            layout_local_screen: screen("local-device", "local-display-1", 0, 0, 1920, 1080),
            remote_screen: entry.clone(),
            edge: Edge::Right,
        };
        let mut current_screen = entry.clone();
        current_screen.id = "scr-1".into();
        let mut active = ActiveTarget {
            target,
            current_screen,
            current_screen_id: "scr-1".into(),
            x: 100.0,
            y: 1079.0,
            invert_y: false,
        };

        // Pushing down past the primary's bottom edge roams onto the secondary.
        active.y += 5.0;
        let returned = update_active_remote_screen(&mut active, 0.0, 5.0, &layout_state);
        assert!(
            !returned,
            "crossing onto a sibling screen must not return to local"
        );
        assert_eq!(active.current_screen_id, "scr-2");
        assert!((0.0..1080.0).contains(&active.y));
        assert_eq!(active.x, 100.0);

        // Moving back up crosses back onto the primary screen.
        active.y -= 6.0;
        let returned = update_active_remote_screen(&mut active, 0.0, -6.0, &layout_state);
        assert!(!returned);
        assert_eq!(active.current_screen_id, "scr-1");
    }

    #[test]
    fn cursor_returns_to_local_only_from_entry_edge() {
        let layout_state = Arc::new(Mutex::new(layout_for_target_tests()));
        let entry = screen(
            "peer-device",
            "peer-device-local-display-1",
            1920,
            0,
            1920,
            1080,
        );
        let target = InputTarget {
            device_id: "peer-device".into(),
            origin_device_id: "peer-local-192-168-66-92".into(),
            cluster_id: "cluster-test".into(),
            pair_secret: "secret-test".into(),
            target_addr: "10.0.0.2:47834".into(),
            target_platform: "windows".into(),
            transport_public_key: "peer-public-key".into(),
            protocol_version: quic_transport::PROTOCOL_VERSION,
            screen_id: "local-display-1".into(),
            local_screen: screen("local-device", "local-display-1", 0, 0, 1920, 1080),
            layout_local_screen: screen("local-device", "local-display-1", 0, 0, 1920, 1080),
            remote_screen: entry.clone(),
            edge: Edge::Right,
        };
        let mut current_screen = entry.clone();
        current_screen.id = "local-display-1".into();
        let mut active = ActiveTarget {
            target,
            current_screen,
            current_screen_id: "local-display-1".into(),
            x: 0.0,
            y: 500.0,
            invert_y: false,
        };

        // Crossed in via the right edge; moving back left off the entry edge
        // hands control back to the local machine.
        active.x -= 2.0;
        assert!(update_active_remote_screen(
            &mut active,
            -2.0,
            0.0,
            &layout_state
        ));
    }

    #[test]
    fn initial_anchor_warp_delta_does_not_return_to_local() {
        let layout_state = Arc::new(Mutex::new(layout_for_target_tests()));
        let entry = screen(
            "peer-device",
            "peer-device-local-display-1",
            1920,
            0,
            1920,
            1080,
        );
        let target = InputTarget {
            device_id: "peer-device".into(),
            origin_device_id: "peer-local-192-168-66-92".into(),
            cluster_id: "cluster-test".into(),
            pair_secret: "secret-test".into(),
            target_addr: "10.0.0.2:47834".into(),
            target_platform: "windows".into(),
            transport_public_key: "peer-public-key".into(),
            protocol_version: quic_transport::PROTOCOL_VERSION,
            screen_id: "local-display-1".into(),
            local_screen: screen("local-device", "local-display-1", 0, 0, 1920, 1080),
            layout_local_screen: screen("local-device", "local-display-1", 0, 0, 1920, 1080),
            remote_screen: entry.clone(),
            edge: Edge::Right,
        };
        let mut current_screen = entry.clone();
        current_screen.id = "local-display-1".into();
        let active = ActiveTarget {
            target,
            current_screen,
            current_screen_id: "local-display-1".into(),
            x: 1.0,
            y: 500.0,
            invert_y: false,
        };
        // Simulate the small leftward delta the entry-anchor warp can inject.
        // (Was -RETURN_EDGE_INSET; now that the inset is 0 for edge-flush returns,
        // use a small fixed delta that still represents the warp's momentum.)
        let dx = -8.0;
        let dy = 0.0;

        let mut unguarded = active.clone();
        unguarded.x += dx;
        assert!(
            update_active_remote_screen(&mut unguarded, dx, dy, &layout_state),
            "without the initial warp guard, the anchor warp delta is mistaken for returning"
        );

        let mut guarded = active.clone();
        let returned = if should_ignore_initial_anchor_warp_delta(guarded.target.edge, dx, dy) {
            false
        } else {
            guarded.x += dx;
            update_active_remote_screen(&mut guarded, dx, dy, &layout_state)
        };

        assert!(!returned);
        assert_eq!(guarded.x, 1.0);
    }

    #[test]
    fn screen_switch_hotkey_matching_requires_exact_modifiers() {
        let hotkeys = crate::ScreenSwitchHotkeys {
            left: "alt+left".into(),
            right: "alt+arrowright".into(),
            up: "disabled".into(),
            down: "alt+shift+down".into(),
        };

        assert!(screen_switch_hotkeys_match_vk(
            &hotkeys,
            0x25,
            HotkeyModifiers {
                alt: true,
                ..HotkeyModifiers::default()
            },
        ));
        assert!(screen_switch_hotkeys_match_vk(
            &hotkeys,
            0x27,
            HotkeyModifiers {
                alt: true,
                ..HotkeyModifiers::default()
            },
        ));
        assert!(screen_switch_hotkeys_match_vk(
            &hotkeys,
            0x28,
            HotkeyModifiers {
                alt: true,
                shift: true,
                ..HotkeyModifiers::default()
            },
        ));
        assert!(!screen_switch_hotkeys_match_vk(
            &hotkeys,
            0x25,
            HotkeyModifiers {
                alt: true,
                shift: true,
                ..HotkeyModifiers::default()
            },
        ));
        assert!(!screen_switch_hotkeys_match_vk(
            &hotkeys,
            0x26,
            HotkeyModifiers {
                alt: true,
                ..HotkeyModifiers::default()
            },
        ));
    }

    #[test]
    fn screen_switch_request_enters_remote_at_screen_center() {
        let layout = layout_for_target_tests();
        let layout_state = Arc::new(Mutex::new(layout.clone()));
        let active = Mutex::new(None);

        match request_screen_switch(SwitchDirection::Right, &layout_state, &layout, &active) {
            SwitchOutcome::Enter(active_target) => {
                assert_eq!(active_target.target.device_id, "peer-device");
                assert_eq!(active_target.x, 960.0);
                assert_eq!(active_target.y, 540.0);
            }
            _ => panic!("expected right quick switch to enter the online client"),
        }
    }

    #[test]
    fn screen_switch_request_moves_between_local_screens() {
        let mut layout = layout_for_target_tests();
        layout.devices[0].screens.push(screen(
            "local-device",
            "local-display-2",
            512,
            1080,
            1512,
            982,
        ));
        let layout_state = Arc::new(Mutex::new(layout.clone()));
        let active = Mutex::new(None);

        match request_screen_switch_from_point(
            SwitchDirection::Down,
            &layout_state,
            &layout,
            &active,
            Some((960.0, 540.0)),
        ) {
            SwitchOutcome::LocalMove {
                from_screen_id,
                to_screen_id,
                x,
                y,
            } => {
                assert_eq!(from_screen_id, "local-display-1");
                assert_eq!(to_screen_id, "local-display-2");
                assert_eq!(x, 1268.0);
                assert_eq!(y, 1571.0);
            }
            _ => panic!("expected down quick switch to move to the lower local screen"),
        }

        match request_screen_switch_from_point(
            SwitchDirection::Up,
            &layout_state,
            &layout,
            &active,
            Some((1268.0, 1571.0)),
        ) {
            SwitchOutcome::LocalMove {
                from_screen_id,
                to_screen_id,
                x,
                y,
            } => {
                assert_eq!(from_screen_id, "local-display-2");
                assert_eq!(to_screen_id, "local-display-1");
                assert_eq!(x, 960.0);
                assert_eq!(y, 540.0);
            }
            _ => panic!("expected up quick switch to move back to the upper local screen"),
        }
    }

    #[test]
    fn local_screen_switch_remembers_points_by_screen_id() {
        let points = Mutex::new(HashMap::new());

        let first_target = remembered_local_screen_point(
            &points,
            "local-display-1",
            "local-display-2",
            Some((333.0, 444.0)),
            (1268.0, 1571.0),
        );
        assert_eq!(first_target, (1268.0, 1571.0));

        let return_target = remembered_local_screen_point(
            &points,
            "local-display-2",
            "local-display-1",
            Some((1200.0, 1500.0)),
            (960.0, 540.0),
        );
        assert_eq!(return_target, (333.0, 444.0));

        let points = points.lock().unwrap();
        assert_eq!(points.get("local-display-1"), Some(&(333.0, 444.0)));
        assert_eq!(points.get("local-display-2"), Some(&(1200.0, 1500.0)));
    }

    #[test]
    fn hotkey_return_uses_recorded_point_then_local_screen_center() {
        let active = crossing_target(
            &[target_for_coordinate_tests()],
            1919.0,
            500.0,
            40.0,
            0.0,
            &Arc::new(Mutex::new(layout_for_target_tests())),
        )
        .expect("target should be active");

        assert_eq!(
            local_hotkey_return_point(&active, Some((321.0, 654.0))),
            (321.0, 654.0)
        );
        assert_eq!(local_hotkey_return_point(&active, None), (960.0, 540.0));
    }

    #[test]
    fn fast_return_pins_remote_cursor_to_entry_edge() {
        let layout_state = Arc::new(Mutex::new(layout_for_target_tests()));
        let entry = screen(
            "peer-device",
            "peer-device-local-display-1",
            1920,
            0,
            1920,
            1080,
        );

        for (edge, x, y, dx, dy, expected_x, expected_y) in [
            (Edge::Right, 240.0, 400.0, -260.0, 18.0, 0.0, 418.0),
            (Edge::Left, 1680.0, 400.0, 260.0, 18.0, 1919.0, 418.0),
            (Edge::Bottom, 500.0, 260.0, 16.0, -300.0, 516.0, 0.0),
            (Edge::Top, 500.0, 820.0, 16.0, 300.0, 516.0, 1079.0),
        ] {
            let target = InputTarget {
                device_id: "peer-device".into(),
                origin_device_id: "peer-local-192-168-66-92".into(),
                cluster_id: "cluster-test".into(),
                pair_secret: "secret-test".into(),
                target_addr: "10.0.0.2:47834".into(),
                target_platform: "windows".into(),
                transport_public_key: "peer-public-key".into(),
                protocol_version: quic_transport::PROTOCOL_VERSION,
                screen_id: "local-display-1".into(),
                local_screen: screen("local-device", "local-display-1", 0, 0, 1920, 1080),
                layout_local_screen: screen("local-device", "local-display-1", 0, 0, 1920, 1080),
                remote_screen: entry.clone(),
                edge,
            };
            let mut current_screen = entry.clone();
            current_screen.id = "local-display-1".into();
            let mut active = ActiveTarget {
                target,
                current_screen,
                current_screen_id: "local-display-1".into(),
                x: x + dx,
                y: y + dy,
                invert_y: false,
            };

            assert!(update_active_remote_screen(
                &mut active,
                dx,
                dy,
                &layout_state
            ));
            assert_eq!(active.x, expected_x);
            assert_eq!(active.y, expected_y);
        }
    }

    #[test]
    fn input_packet_round_trips_as_messagepack() {
        let packet = InputPacket {
            protocol: INPUT_PROTOCOL.into(),
            target_device_id: "peer-device".into(),
            origin_device_id: "local-device".into(),
            origin_port: 47833,
            origin_transport_public_key: "local-public-key".into(),
            origin_protocol_version: quic_transport::PROTOCOL_VERSION,
            cluster_id: "cluster-test".into(),
            pair_secret: "secret-test".into(),
            event: InputEvent::MouseMove {
                screen_id: "display-1".into(),
                x: 320,
                y: 240,
            },
        };
        let payload = rmp_serde::to_vec_named(&packet).expect("encode input packet");
        let decoded = decode_input_packet(&payload).expect("decode input packet");

        assert_eq!(decoded.protocol, INPUT_PROTOCOL);
        assert_eq!(decoded.target_device_id, "peer-device");
        assert_eq!(decoded.origin_device_id, "local-device");
        assert_eq!(decoded.origin_port, 47833);
        match decoded.event {
            InputEvent::MouseMove { screen_id, x, y } => {
                assert_eq!(screen_id, "display-1");
                assert_eq!(x, 320);
                assert_eq!(y, 240);
            }
            _ => panic!("decoded the wrong input event"),
        }
    }

    #[test]
    fn input_packet_context_uses_stable_peer_origin_id() {
        let layout = layout_for_target_tests();
        let expected_origin_id = crate::local_peer_from_layout(&layout).id;
        let layout_state = Arc::new(Mutex::new(layout));
        let target = target_for_coordinate_tests();

        let context = input_packet_context(
            &target,
            InputEvent::MouseMove {
                screen_id: "local-display-1".into(),
                x: 10,
                y: 20,
            },
            &layout_state,
        );

        assert_ne!(expected_origin_id, "local-device");
        assert_eq!(context.origin_device_id, expected_origin_id);
    }

    #[test]
    fn input_packet_context_uses_cached_target_when_layout_lock_is_busy() {
        let layout_state = Arc::new(Mutex::new(layout_for_target_tests()));
        let _held_layout = layout_state.lock().expect("hold layout lock");
        let target = target_for_coordinate_tests();
        let layout_state_for_thread = Arc::clone(&layout_state);
        let (tx, rx) = std::sync::mpsc::channel();

        thread::spawn(move || {
            let context = input_packet_context(
                &target,
                InputEvent::MouseMove {
                    screen_id: "local-display-1".into(),
                    x: 10,
                    y: 20,
                },
                &layout_state_for_thread,
            );
            tx.send(context).expect("send packet context");
        });

        let context = rx
            .recv_timeout(Duration::from_millis(50))
            .expect("packet context should not block on the layout lock");
        assert_eq!(context.origin_device_id, "peer-local-192-168-66-92");
        assert_eq!(context.cluster_id, "cluster-test");
        assert_eq!(context.pair_secret, "secret-test");
        assert!(context.peer.is_some());
    }

    #[test]
    fn input_packet_requires_pair_secret() {
        let mut layout = layout_for_target_tests();
        layout.machine_role = "client".into();
        layout.paired_controllers = vec![crate::PairedController {
            id: "server".into(),
            name: "Server".into(),
            host: "server".into(),
            ip: "10.0.0.1".into(),
            transport_public_key: "server-key".into(),
            protocol_version: quic_transport::PROTOCOL_VERSION,
            cluster_id: layout.cluster_id.clone(),
            paired_at_ms: 1,
        }];
        let mut packet = InputPacket {
            protocol: INPUT_PROTOCOL.into(),
            target_device_id: "local-device".into(),
            origin_device_id: "server".into(),
            origin_port: 47834,
            origin_transport_public_key: "server-key".into(),
            origin_protocol_version: quic_transport::PROTOCOL_VERSION,
            cluster_id: layout.cluster_id.clone(),
            pair_secret: "wrong".into(),
            event: InputEvent::MouseMove {
                screen_id: "local-display-1".into(),
                x: 1,
                y: 1,
            },
        };

        assert!(!packet_authorized(&layout, &packet));
        packet.pair_secret = layout.pair_secret.clone();
        assert!(packet_authorized(&layout, &packet));
        packet.origin_transport_public_key = "attacker-key".into();
        packet.origin_device_id = "attacker".into();
        assert!(!packet_authorized(&layout, &packet));
        packet.origin_transport_public_key.clear();
        packet.origin_device_id = "server".into();
        assert!(packet_authorized(&layout, &packet));
    }

    #[test]
    fn input_packet_accepts_legacy_origin_after_transport_key_rotation() {
        let mut layout = layout_for_target_tests();
        layout.machine_role = "client".into();
        layout.paired_controllers = vec![crate::PairedController {
            id: "peer-server-local-10-0-0-1".into(),
            name: "Server".into(),
            host: "server.local".into(),
            ip: "10.0.0.1".into(),
            transport_public_key: "server-old-key".into(),
            protocol_version: quic_transport::PROTOCOL_VERSION,
            cluster_id: layout.cluster_id.clone(),
            paired_at_ms: 1,
        }];
        let packet = InputPacket {
            protocol: INPUT_PROTOCOL.into(),
            target_device_id: "local-device".into(),
            origin_device_id: "local-device".into(),
            origin_port: 47834,
            origin_transport_public_key: "server-rotated-key".into(),
            origin_protocol_version: quic_transport::PROTOCOL_VERSION,
            cluster_id: layout.cluster_id.clone(),
            pair_secret: layout.pair_secret.clone(),
            event: InputEvent::MouseMove {
                screen_id: "local-display-1".into(),
                x: 1,
                y: 1,
            },
        };

        assert!(packet_authorized(&layout, &packet));

        layout.paired_controllers.push(crate::PairedController {
            id: "peer-other-server".into(),
            name: "Other".into(),
            host: "other.local".into(),
            ip: "10.0.0.3".into(),
            transport_public_key: "other-key".into(),
            protocol_version: quic_transport::PROTOCOL_VERSION,
            cluster_id: layout.cluster_id.clone(),
            paired_at_ms: 2,
        });
        assert!(!packet_authorized(&layout, &packet));
    }

    #[test]
    fn input_event_maps_relative_coordinates_to_native_command() {
        let layout = layout_for_target_tests();
        let mut native_layout = layout.clone();
        native_layout.devices[0].screens[0].width = 3840;
        native_layout.devices[0].screens[0].height = 2160;

        let command = input_event_to_command(
            &layout,
            &native_layout,
            InputEvent::MouseMove {
                screen_id: "local-display-1".into(),
                x: 960,
                y: 540,
            },
        )
        .expect("mouse move should map to command");

        assert_eq!(
            command,
            InputCommand::MouseMove {
                x: 1920,
                y: 1080,
                drag_button: None,
            }
        );
    }

    #[test]
    fn input_control_packet_round_trips_as_messagepack() {
        let packet = InputControlPacket {
            protocol: INPUT_CONTROL_PROTOCOL.into(),
            target_device_id: "local-device".into(),
            origin_device_id: "server".into(),
            origin_transport_public_key: "server-key".into(),
            origin_protocol_version: quic_transport::PROTOCOL_VERSION,
            cluster_id: "cluster-test".into(),
            pair_secret: "secret-test".into(),
            command: InputControlCommand::SecureAttention,
        };
        let payload = rmp_serde::to_vec_named(&packet).expect("encode input control packet");
        let decoded = decode_input_control_packet(&payload).expect("decode input control packet");

        assert_eq!(decoded.protocol, INPUT_CONTROL_PROTOCOL);
        assert_eq!(decoded.target_device_id, "local-device");
        assert_eq!(decoded.command, InputControlCommand::SecureAttention);
    }

    #[test]
    fn input_control_packet_uses_pairing_authorization() {
        let mut layout = layout_for_target_tests();
        layout.machine_role = "client".into();
        layout.paired_controllers = vec![crate::PairedController {
            id: "server".into(),
            name: "Server".into(),
            host: "server".into(),
            ip: "10.0.0.1".into(),
            transport_public_key: "server-key".into(),
            protocol_version: quic_transport::PROTOCOL_VERSION,
            cluster_id: layout.cluster_id.clone(),
            paired_at_ms: 1,
        }];
        let mut packet = InputControlPacket {
            protocol: INPUT_CONTROL_PROTOCOL.into(),
            target_device_id: "local-device".into(),
            origin_device_id: "server".into(),
            origin_transport_public_key: "server-key".into(),
            origin_protocol_version: quic_transport::PROTOCOL_VERSION,
            cluster_id: layout.cluster_id.clone(),
            pair_secret: "wrong".into(),
            command: InputControlCommand::SecureAttention,
        };

        assert!(!control_packet_authorized(&layout, &packet));
        packet.pair_secret = layout.pair_secret.clone();
        assert!(control_packet_authorized(&layout, &packet));
        packet.origin_transport_public_key = "attacker-key".into();
        packet.origin_device_id = "attacker".into();
        assert!(!control_packet_authorized(&layout, &packet));
    }

    #[test]
    fn clipboard_target_expires() {
        let target = Arc::new(Mutex::new(Some(ClipboardTarget {
            device_id: "peer-device".into(),
            addr: "10.0.0.2:47833".into(),
            transport_public_key: "peer-public-key".into(),
            protocol_version: quic_transport::PROTOCOL_VERSION,
            cluster_id: "cluster-test".into(),
            pair_secret: "secret-test".into(),
            expires_at: Some(Instant::now() - Duration::from_millis(1)),
        })));

        assert!(current_clipboard_target(&target).is_none());
        assert!(target.lock().expect("target lock").is_none());
    }

    #[test]
    fn crossing_accepts_native_screen_coordinates() {
        let target = target_for_coordinate_tests();

        // Native width 1920, so the cursor must reach the edge pixel x=1919
        // (CROSSING_MARGIN=1) before a crossing is accepted.
        let mapped = crossing_layout_point(&target, 1919.0, 500.0, 5.0, 0.0)
            .expect("native edge should cross");

        assert!(mapped.0 > -9404.0);
        assert!(mapped.0 <= -9400.0);
    }

    #[test]
    fn fast_crossing_carries_entry_delta_into_remote() {
        let target = target_for_coordinate_tests();
        let layout_state = Arc::new(Mutex::new(layout_for_target_tests()));
        let active = crossing_target(&[target], 1919.0, 500.0, 40.0, 0.0, &layout_state)
            .expect("fast edge movement should cross");

        assert!(
            active.x > 1.0,
            "dropping the crossing delta makes the cursor feel stuck at the edge"
        );
    }

    #[test]
    fn crossing_rejects_raw_layout_coordinates() {
        let target = target_for_coordinate_tests();

        assert!(crossing_layout_point(&target, -9401.0, -8500.0, 5.0, 0.0).is_none());
    }

    #[test]
    fn crossing_uses_native_edge_before_mapping_to_layout() {
        let target = InputTarget {
            device_id: "peer-device".into(),
            origin_device_id: "peer-local-192-168-66-92".into(),
            cluster_id: "cluster-test".into(),
            pair_secret: "secret-test".into(),
            target_addr: "10.0.0.2:47833".into(),
            target_platform: "windows".into(),
            transport_public_key: "test-public-key".into(),
            protocol_version: quic_transport::PROTOCOL_VERSION,
            screen_id: "local-display-1".into(),
            local_screen: screen("local-device", "local-display-1", 0, 0, 3840, 2160),
            layout_local_screen: screen("local-device", "local-display-1", 0, 0, 1920, 1080),
            remote_screen: screen(
                "peer-device",
                "peer-device-local-display-1",
                1920,
                0,
                1728,
                1117,
            ),
            edge: Edge::Right,
        };

        assert!(crossing_layout_point(&target, 1918.0, 600.0, 5.0, 0.0).is_none());

        // Native width 3840, so the edge pixel is x=3839; the cursor must reach
        // it (CROSSING_MARGIN=1) before crossing.
        let mapped = crossing_layout_point(&target, 3839.0, 1200.0, 5.0, 0.0)
            .expect("native edge should cross");

        assert!(mapped.0 > 1916.0);
        assert!(mapped.0 <= 1920.0);
    }

    #[test]
    fn crossing_rejects_fast_jump_from_middle() {
        let target = InputTarget {
            device_id: "peer-device".into(),
            origin_device_id: "peer-local-192-168-66-92".into(),
            cluster_id: "cluster-test".into(),
            pair_secret: "secret-test".into(),
            target_addr: "10.0.0.2:47833".into(),
            target_platform: "windows".into(),
            transport_public_key: "test-public-key".into(),
            protocol_version: quic_transport::PROTOCOL_VERSION,
            screen_id: "local-display-1".into(),
            local_screen: screen("local-device", "local-display-1", 0, 0, 3840, 2160),
            layout_local_screen: screen("local-device", "local-display-1", 0, 0, 1920, 1080),
            remote_screen: screen(
                "peer-device",
                "peer-device-local-display-1",
                1920,
                0,
                1728,
                1117,
            ),
            edge: Edge::Right,
        };

        assert!(crossing_layout_point(&target, 3838.0, 1200.0, 900.0, 0.0).is_none());
    }

    #[test]
    fn modifier_key_mapping_handles_sided_keys_and_caps_lock() {
        assert_eq!(windows_vk_to_mac_key(0x10), Some(56));
        assert_eq!(windows_vk_to_mac_key(0xA0), Some(56));
        assert_eq!(windows_vk_to_mac_key(0xA1), Some(60));
        assert_eq!(windows_vk_to_mac_key(0x11), Some(59));
        assert_eq!(windows_vk_to_mac_key(0xA2), Some(59));
        assert_eq!(windows_vk_to_mac_key(0xA3), Some(62));
        assert_eq!(windows_vk_to_mac_key(0x12), Some(58));
        assert_eq!(windows_vk_to_mac_key(0xA4), Some(58));
        assert_eq!(windows_vk_to_mac_key(0xA5), Some(61));
        assert_eq!(windows_vk_to_mac_key(0x14), Some(57));
        assert_eq!(windows_vk_to_mac_key(0x5B), Some(55));
        assert_eq!(windows_vk_to_mac_key(0x5C), Some(54));

        assert_eq!(mac_key_to_windows_vk(56), Some(0xA0));
        assert_eq!(mac_key_to_windows_vk(60), Some(0xA1));
        assert_eq!(mac_key_to_windows_vk(57), Some(0x14));
        assert_eq!(mac_key_to_windows_vk(58), Some(0xA4));
        assert_eq!(mac_key_to_windows_vk(61), Some(0xA5));
        assert_eq!(mac_key_to_windows_vk(59), Some(0xA2));
        assert_eq!(mac_key_to_windows_vk(62), Some(0xA3));
    }

    #[test]
    fn key_mapping_handles_space_numpad_and_function_keys() {
        assert_eq!(windows_vk_to_mac_key(0x20), Some(49));
        assert_eq!(mac_key_to_windows_vk(49), Some(0x20));

        for (vk, mac) in [
            (0x60, 82),
            (0x61, 83),
            (0x62, 84),
            (0x63, 85),
            (0x64, 86),
            (0x65, 87),
            (0x66, 88),
            (0x67, 89),
            (0x68, 91),
            (0x69, 92),
            (0x6A, 67),
            (0x6B, 69),
            (0x6D, 78),
            (0x6E, 65),
            (0x6F, 75),
        ] {
            assert_eq!(windows_vk_to_mac_key(vk), Some(mac));
        }

        for (vk, mac) in [
            (0x70, 122),
            (0x71, 120),
            (0x72, 99),
            (0x73, 118),
            (0x74, 96),
            (0x75, 97),
            (0x76, 98),
            (0x77, 100),
            (0x78, 101),
            (0x79, 109),
            (0x7A, 103),
            (0x7B, 111),
        ] {
            assert_eq!(windows_vk_to_mac_key(vk), Some(mac));
            assert_eq!(mac_key_to_windows_vk(mac), Some(vk));
        }
    }

    #[test]
    fn default_modifier_map_swaps_control_and_meta() {
        let map = crate::default_modifier_map();

        // Control (any side) -> Meta (Windows key / macOS Command)
        assert_eq!(
            remap_modifier_vk(0x11, &map.control, &map.alt, &map.meta),
            0x5B
        );
        assert_eq!(
            remap_modifier_vk(0xA2, &map.control, &map.alt, &map.meta),
            0x5B
        );
        assert_eq!(
            remap_modifier_vk(0xA3, &map.control, &map.alt, &map.meta),
            0x5B
        );
        // Meta -> Control
        assert_eq!(
            remap_modifier_vk(0x5B, &map.control, &map.alt, &map.meta),
            0x11
        );
        assert_eq!(
            remap_modifier_vk(0x5C, &map.control, &map.alt, &map.meta),
            0x11
        );
        // Alt stays as itself (left/right preserved via "same")
        assert_eq!(
            remap_modifier_vk(0xA4, &map.control, &map.alt, &map.meta),
            0xA4
        );
        // Non-modifier keys are untouched (e.g. the letter C)
        assert_eq!(
            remap_modifier_vk(0x43, &map.control, &map.alt, &map.meta),
            0x43
        );
    }

    #[test]
    fn custom_modifier_map_is_honored() {
        // User keeps Ctrl literal but maps the Windows/Command key to Alt.
        assert_eq!(remap_modifier_vk(0x11, "same", "same", "alt"), 0x11);
        assert_eq!(remap_modifier_vk(0x5B, "same", "same", "alt"), 0x12);
    }

    #[test]
    fn remap_skips_unknown_target_platform() {
        let layout = Arc::new(Mutex::new(layout_for_target_tests()));
        let mut target = {
            let guard = layout.lock().expect("layout lock");
            build_input_targets(&guard, &guard)
                .into_iter()
                .next()
                .expect("one target")
        };

        // An unknown target platform must never be remapped, regardless of the
        // configured map, so we cannot accidentally mangle keys for peers we
        // cannot classify.
        target.target_platform = "unknown".into();
        let event = remap_event_for_target(
            InputEvent::Key {
                key_code: 0x11,
                down: true,
            },
            &target,
            &layout,
        );
        match event {
            InputEvent::Key { key_code, .. } => assert_eq!(key_code, 0x11),
            _ => panic!("expected key event"),
        }
    }

    #[test]
    fn remap_passes_through_non_key_events() {
        let layout = Arc::new(Mutex::new(layout_for_target_tests()));
        let target = {
            let guard = layout.lock().expect("layout lock");
            build_input_targets(&guard, &guard)
                .into_iter()
                .next()
                .expect("one target")
        };

        let event = remap_event_for_target(
            InputEvent::Scroll {
                delta_x: 1,
                delta_y: -2,
            },
            &target,
            &layout,
        );
        assert!(matches!(
            event,
            InputEvent::Scroll {
                delta_x: 1,
                delta_y: -2
            }
        ));
    }

    #[test]
    fn input_targets_use_peer_quic_port() {
        let layout = layout_for_target_tests();
        let targets = build_input_targets(&layout, &layout);

        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].target_addr, "10.0.0.2:52001");
    }

    #[test]
    fn input_targets_cache_pairing_context_for_hot_path() {
        let layout = layout_for_target_tests();
        let expected_origin_id = crate::local_peer_from_layout(&layout).id;
        let targets = build_input_targets(&layout, &layout);

        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].origin_device_id, expected_origin_id);
        assert_eq!(targets[0].cluster_id, "cluster-test");
        assert_eq!(targets[0].pair_secret, "secret-test");
    }

    #[test]
    fn input_targets_require_peer_input_ready() {
        let mut layout = layout_for_target_tests();
        layout.devices[1].input_ready = false;

        let targets = build_input_targets(&layout, &layout);

        assert!(targets.is_empty());
    }

    #[test]
    fn input_targets_ignore_overlapping_remote_screens() {
        let mut layout = layout_for_target_tests();
        layout.devices[1].screens[0].x = 1860;

        let targets = build_input_targets(&layout, &layout);

        assert!(targets.is_empty());
    }
}
