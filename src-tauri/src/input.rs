use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Condvar, Mutex, OnceLock, TryLockError,
    },
    thread,
    time::{Duration, Instant},
};

#[cfg(any(target_os = "macos", target_os = "windows"))]
use std::sync::mpsc;

use serde::{Deserialize, Serialize};

use crate::{
    quic_transport,
    shared_input::{
        button_from_mask, modifier_mask_for_key, modifier_mask_for_keys,
        modifier_snapshot_transitions, mouse_button_mask, InputCommand, InputEvent, MouseButton,
        ALT_MODIFIER_MASK, CONTROL_MODIFIER_MASK, LEFT_BUTTON_MASK, META_MODIFIER_MASK,
        MIDDLE_BUTTON_MASK, RIGHT_BUTTON_MASK, SHIFT_MODIFIER_MASK,
    },
    Device, LayoutState, NativeStageStatus, Screen,
};

#[cfg(any(target_os = "linux", test))]
#[path = "linux_input.rs"]
mod linux_input;

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
const RETURN_AXIS_DOMINANCE: f64 = 1.0;
const CROSSING_ACTIVATION_BAND: f64 = EDGE_TOLERANCE as f64 * 2.0;
// A tiny spatial re-arm after returning is imperceptible but avoids immediately
// bouncing back across the same edge. Unlike the old 150ms time gate it never
// freezes deliberate movement.
const RETURN_EDGE_INSET: f64 = 4.0;
const MOUSE_MOVE_SEND_INTERVAL_MS: u64 = 8;
const DRAG_MOVE_SEND_INTERVAL_MS: u64 = 8;
#[cfg(target_os = "macos")]
const MACOS_IDLE_CAPTURE_LOOP_MS: u64 = 100;
#[cfg(target_os = "macos")]
const MACOS_VISIBLE_REMOTE_CAPTURE_LOOP_MS: u64 = 16;
#[cfg(target_os = "macos")]
const MACOS_HIDDEN_REMOTE_CAPTURE_LOOP_MS: u64 = MACOS_VISIBLE_REMOTE_CAPTURE_LOOP_MS;
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
static REMOTE_KEY_SEQUENCE_STATE: OnceLock<Mutex<RemoteKeySequenceState>> = OnceLock::new();
static REMOTE_INPUT_INJECT_LOCK: Mutex<()> = Mutex::new(());
static REMOTE_MOUSE_INJECT_LOCK: Mutex<()> = Mutex::new(());
static REMOTE_INPUT_ORIGIN: Mutex<String> = Mutex::new(String::new());
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
    /// Controller crossings push their current clipboard immediately. The
    /// controlled side first baselines its existing clipboard so both peers do
    /// not race to overwrite each other with unrelated pre-session content.
    pub push_on_bind: bool,
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
    /// Target-semantic modifier state carried with ordinary key events. This
    /// repairs a lost modifier Up before the key is injected. `Some(0)` means
    /// no modifiers are physically held; old peers omit the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    modifier_snapshot: Option<u8>,
    /// Rejects a stale/duplicate key frame that arrives from an older QUIC
    /// connection after a reconnect. Zero is reserved for older peers.
    #[serde(default, skip_serializing_if = "is_zero")]
    key_sequence: u64,
    event: InputEvent,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
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

#[derive(Debug, Default)]
struct RemoteMouseState {
    x: i32,
    y: i32,
    buttons: u64,
    last_origin_id: String,
    sequence_by_origin: HashMap<String, RemoteMouseSequenceState>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct RemoteMouseSequenceState {
    last_position_sequence: u64,
    last_scroll_sequence: u64,
    last_boundary_sequence: u64,
    last_button_sequence: [u64; 3],
}

#[derive(Debug, Default)]
struct RemoteKeySequenceState {
    by_origin: HashMap<String, RemoteOriginKeySequenceState>,
}

#[derive(Debug, Default)]
struct RemoteOriginKeySequenceState {
    boundary_sequence: u64,
    latest_event_sequence: u64,
    snapshot_sequence: u64,
    last_by_key: HashMap<u16, u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RemoteInputAdmission {
    inject_event: bool,
    effective_modifier_snapshot: Option<u8>,
    origin_changed: bool,
    release_keys: bool,
    carried_buttons: Option<(u64, i32, i32)>,
    mouse: Option<RemoteMouseAdmission>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RemoteMouseAdmission {
    button_reconciliation: Option<(u64, u64, i32, i32)>,
    park_accepted: bool,
}

impl RemoteKeySequenceState {
    fn accept_key(&mut self, origin_id: &str, key_code: u16, sequence: u64) -> bool {
        if sequence == 0 {
            return true;
        }
        let state = self.by_origin.entry(origin_id.to_string()).or_default();
        let is_modifier = modifier_mask_for_key(key_code).is_some();
        let key_code = remote_semantic_key(key_code);
        let last = state
            .last_by_key
            .get(&key_code)
            .copied()
            .unwrap_or_default();
        if sequence <= state.boundary_sequence
            || sequence <= last
            // A newer authoritative snapshot already describes every modifier
            // family. A delayed Ctrl/Shift/Alt/Meta transition from an older
            // QUIC stream must not undo that newer state.
            || (is_modifier && sequence <= state.snapshot_sequence)
        {
            return false;
        }
        state.last_by_key.insert(key_code, sequence);
        state.latest_event_sequence = state.latest_event_sequence.max(sequence);
        true
    }

    fn accept_boundary(&mut self, origin_id: &str, sequence: u64) -> bool {
        if sequence == 0 {
            return true;
        }
        let state = self.by_origin.entry(origin_id.to_string()).or_default();
        if sequence <= state.boundary_sequence || sequence <= state.latest_event_sequence {
            return false;
        }
        state.boundary_sequence = sequence;
        state.latest_event_sequence = sequence;
        true
    }

    fn accept_snapshot(&mut self, origin_id: &str, sequence: u64) -> bool {
        if sequence == 0 {
            return true;
        }
        let state = self.by_origin.entry(origin_id.to_string()).or_default();
        if sequence <= state.boundary_sequence || sequence <= state.snapshot_sequence {
            return false;
        }
        state.snapshot_sequence = sequence;
        state.latest_event_sequence = state.latest_event_sequence.max(sequence);
        true
    }
}

fn remote_semantic_key(key_code: u16) -> u16 {
    match modifier_mask_for_key(key_code) {
        Some(SHIFT_MODIFIER_MASK) => 0x10,
        Some(CONTROL_MODIFIER_MASK) => 0x11,
        Some(ALT_MODIFIER_MASK) => 0x12,
        Some(META_MODIFIER_MASK) => 0x5B,
        _ => key_code,
    }
}

fn next_mouse_sequence() -> u64 {
    static SEQUENCE: OnceLock<AtomicU64> = OnceLock::new();
    let sequence = SEQUENCE.get_or_init(|| {
        let base = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros()
            .min(u128::from(u64::MAX - 1)) as u64;
        AtomicU64::new(base.max(1))
    });
    sequence.fetch_add(1, Ordering::Relaxed)
}

fn next_key_sequence() -> u64 {
    static SEQUENCE: OnceLock<AtomicU64> = OnceLock::new();
    let sequence = SEQUENCE.get_or_init(|| AtomicU64::new(next_mouse_sequence()));
    sequence.fetch_add(1, Ordering::Relaxed)
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
        #[cfg(target_os = "linux")]
        {
            linux_input::capture_status(targets.len())
        }
        #[cfg(not(target_os = "linux"))]
        {
            unsupported_capture_status()
        }
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

    #[cfg(target_os = "linux")]
    {
        linux_input::receive_status(normalize_quic_port(layout.transport_port, layout.quic_port))
    }

    #[cfg(not(target_os = "linux"))]
    {
        NativeStageStatus {
            state: "ready".into(),
            detail: format!(
                "Receiving shared input on QUIC datagrams at UDP {}.",
                normalize_quic_port(layout.transport_port, layout.quic_port)
            ),
        }
    }
}

#[cfg(target_os = "linux")]
fn start_platform_capture(
    targets: Vec<InputTarget>,
    layout_state: Arc<Mutex<LayoutState>>,
    native_layout: LayoutState,
    quic_transport: quic_transport::TransportHandle,
    stop: Arc<AtomicBool>,
    remote_active: Arc<AtomicBool>,
    _main_window_visible: Arc<AtomicBool>,
    _main_window_focused: Arc<AtomicBool>,
    clipboard_target: Arc<Mutex<Option<ClipboardTarget>>>,
    input_events: Arc<AtomicU64>,
    switch_request: Arc<Mutex<Option<SwitchDirection>>>,
) -> NativeStageStatus {
    linux_input::start_capture(
        targets,
        layout_state,
        native_layout,
        quic_transport,
        stop,
        remote_active,
        clipboard_target,
        input_events,
        switch_request,
    )
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
            stop: Arc::clone(&stop),
            send_gate: Mutex::new(()),
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
        if let Ok(mut current) = MAC_CAPTURE_CONTEXT.lock() {
            *current = Some(Arc::clone(&context));
        }
        tap.enable();
        let _ = ready_tx.send(Ok(()));
        // Belt-and-braces: ensure App Nap is suppressed from the control-side
        // capture thread too. The process-wide arm in lib.rs setup runs earlier
        // and could silently no-op if NSProcessInfo was not ready yet; without
        // suppression the background capture loop gets throttled and crossings
        // stutter. Idempotent — skips if already armed (the diag log says which).
        set_macos_app_nap_suppressed(true);

        // App Nap suppression is held process-wide for the app lifetime (see
        // lib.rs setup) — toggling it per remote_active left the QUIC/discovery
        // /clipboard timers napped between controls and on receive-only peers.
        while !stop.load(Ordering::Relaxed) {
            let was_remote_active = context.remote_active.load(Ordering::Relaxed);
            let _ = CFRunLoop::run_in_mode(
                unsafe { kCFRunLoopDefaultMode },
                Duration::from_millis(macos_capture_loop_ms(
                    was_remote_active,
                    context.main_window_visible.load(Ordering::Relaxed),
                )),
                false,
            );
            if stop.load(Ordering::Relaxed) {
                break;
            }
            {
                let _send_guard = context
                    .send_gate
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                if !context.stop.load(Ordering::Relaxed) {
                    if active_target_input_failed(&context.quic_transport, &context.active) {
                        log::warn!(
                            "remote input transport failed; returning macOS cursor to local control"
                        );
                        return_to_local_macos(&context);
                        clear_clipboard_target(&context.clipboard_target);
                    } else {
                        drain_switch_request_macos(&context);
                    }
                }
            }
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
            {
                let _send_guard = context
                    .send_gate
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                if !context.stop.load(Ordering::Relaxed)
                    && context.remote_active.load(Ordering::Relaxed)
                {
                    repin_macos_cursor_while_remote(&context);
                }
            }
        }

        // Best-effort remote boundary before the capture thread disappears.
        // App shutdown normally performs the same release synchronously, but
        // runtime restarts and capture failures must not leave a remote button
        // or key held when this loop is the last owner of the context.
        {
            let _send_guard = context
                .send_gate
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            return_to_local_macos(&context);
        }
        // Critical safety: never leave the cursor decoupled after capture stops,
        // otherwise the user's mouse stays frozen until the app restarts.
        set_macos_cursor_decoupled(false);
        set_macos_warp_suppression_interval(MACOS_DEFAULT_WARP_SUPPRESSION_SECS);
        show_macos_cursor_if_needed(&context);
        context.remote_active.store(false, Ordering::Relaxed);
        clear_clipboard_target(&context.clipboard_target);
        clear_macos_capture_context(&context);
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
        MsgWaitForMultipleObjects, PeekMessageW, MSG, PM_REMOVE, QS_ALLINPUT,
    };

    let target_count = targets.len();
    let (ready_tx, ready_rx) = mpsc::channel();

    thread::spawn(move || {
        set_windows_capture_thread_priority();
        refresh_windows_input_desktop_cache();
        let context = Arc::new(WindowsCaptureContext {
            quic_transport,
            layout_state,
            native_layout,
            stop: Arc::clone(&stop),
            send_gate: Mutex::new(()),
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
            last_hook_event_ms: AtomicU64::new(0),
        });

        if let Ok(mut current) = WINDOWS_CAPTURE_CONTEXT.lock() {
            *current = Some(Arc::clone(&context));
        }

        let mut hooks = match WindowsInputHooks::install() {
            Ok(hooks) => hooks,
            Err(error) => {
                context.remote_active.store(false, Ordering::Relaxed);
                clear_clipboard_target(&context.clipboard_target);
                clear_windows_capture_context(&context);
                let _ = ready_tx.send(Err(error));
                return;
            }
        };

        let _ = ready_tx.send(Ok(()));
        let mut message = MSG::default();
        let mut last_desktop_check = Instant::now() - Duration::from_millis(200);
        let mut last_watchdog_check = Instant::now();
        while !stop.load(Ordering::Relaxed) {
            if last_desktop_check.elapsed() >= Duration::from_millis(100) {
                last_desktop_check = Instant::now();
                if !refresh_windows_input_desktop_cache() {
                    release_windows_remote_control(&context, true);
                }
            }
            if stop.load(Ordering::Relaxed) {
                break;
            }
            {
                let _send_guard = context
                    .send_gate
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                if !context.stop.load(Ordering::Relaxed) {
                    if active_target_input_failed(&context.quic_transport, &context.active) {
                        log::warn!(
                            "remote input transport failed; releasing Windows remote control"
                        );
                        release_windows_remote_control_inner(&context, true);
                    } else {
                        drain_switch_request_windows(&context);
                    }
                }
            }
            if last_watchdog_check.elapsed() >= Duration::from_secs(1) {
                last_watchdog_check = Instant::now();
                if windows_hooks_look_dead(&context, &hooks) {
                    log::warn!("low-level input hooks stopped receiving events; reinstalling them");
                    hooks.reinstall(&context);
                }
            }
            // Hook callbacks are dispatched while this thread services its
            // message queue. Waiting on the queue instead of sleeping 10 ms
            // between polls removes up to 10 ms of added latency per input
            // event and all idle wakeups; slow servicing is also what makes
            // Windows silently drop low-level hooks.
            unsafe {
                let _ = MsgWaitForMultipleObjects(0, std::ptr::null(), 0, 20, QS_ALLINPUT);
                while PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {}
            }
        }

        release_windows_remote_control(&context, true);
        hooks.uninstall();
        show_windows_cursor_if_needed(&context);
        context.remote_active.store(false, Ordering::Relaxed);
        clear_clipboard_target(&context.clipboard_target);
        clear_windows_capture_context(&context);
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

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
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

#[cfg(target_os = "macos")]
static MACOS_RECEIVE_CURSOR_HIDDEN: AtomicBool = AtomicBool::new(false);
#[cfg(target_os = "macos")]
static MACOS_RECEIVE_PARK_POINT: Mutex<Option<(f64, f64)>> = Mutex::new(None);
#[cfg(target_os = "macos")]
static MACOS_RECEIVE_CURSOR_TRANSITION: Mutex<()> = Mutex::new(());

/// Control just left this macOS client: hide the local cursor where it was
/// tucked, mirroring how the server hides its own cursor while driving a remote.
#[cfg(target_os = "macos")]
fn macos_receive_hide_cursor(x: i32, y: i32) {
    use core_graphics::{display::CGDisplay, geometry::CGPoint};

    let _transition = MACOS_RECEIVE_CURSOR_TRANSITION
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());

    // Warp only — no posted mouse-move event, which the system would service by
    // re-showing the cursor and undoing the hide below.
    let _ = CGDisplay::warp_mouse_cursor_position(CGPoint::new(x as f64, y as f64));
    if let Ok(mut point) = MACOS_RECEIVE_PARK_POINT.lock() {
        *point = Some((x as f64, y as f64));
    }
    if let Ok(mut tracker) = macos_click_tracker().lock() {
        *tracker = MacClickTracker::default();
    }
    // Only run the hide primitives once per hide/show cycle — CGDisplayHideCursor
    // is counted and must pair 1:1 with show, so a repeated CursorPark must not
    // re-hide (it just re-parks above).
    if MACOS_RECEIVE_CURSOR_HIDDEN.swap(true, Ordering::Relaxed) {
        return;
    }
    // Full hide, matching the server: SetsCursorInBackground so it sticks while
    // not frontmost, transparent cursor, NSCursor hide, and hide on every display.
    enable_macos_background_cursor_hide();
    set_macos_cursor_transparent(true);
    set_macos_cursor_hidden_with_appkit(true);
    for display_id in CGDisplay::active_displays().unwrap_or_default() {
        let _ = CGDisplay::new(display_id).hide_cursor();
    }
    log::info!("[diag] receive hide cursor at ({x},{y})");
}

/// Reveal the cursor hidden by `macos_receive_hide_cursor`. The swap ensures the
/// balancing show/unhide runs exactly once per hide, so the counted CGDisplay
/// and stack-based NSCursor calls stay paired.
#[cfg(target_os = "macos")]
fn macos_receive_show_cursor_if_hidden() {
    use core_graphics::display::CGDisplay;

    let _transition = MACOS_RECEIVE_CURSOR_TRANSITION
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());

    if !MACOS_RECEIVE_CURSOR_HIDDEN.swap(false, Ordering::Relaxed) {
        return;
    }
    set_macos_cursor_transparent(false);
    set_macos_cursor_hidden_with_appkit(false);
    for display_id in CGDisplay::active_displays().unwrap_or_default() {
        let _ = CGDisplay::new(display_id).show_cursor();
    }
    if let Ok(mut point) = MACOS_RECEIVE_PARK_POINT.lock() {
        *point = None;
    }
    log::info!("[diag] receive show cursor");
}

#[cfg(target_os = "macos")]
fn start_platform_receive_monitor(stop: Arc<AtomicBool>) {
    thread::spawn(move || {
        // While the cursor is hidden (control has left), watch for the local
        // user physically moving the mouse — the cursor drifts off the parked
        // point — and reveal it so they can use this machine again. An injected
        // move from the server reveals it directly (see inject_input_command).
        const RECEIVE_CURSOR_DRIFT_PX: f64 = 3.0;
        while !stop.load(Ordering::Relaxed) {
            if MACOS_RECEIVE_CURSOR_HIDDEN.load(Ordering::Relaxed) {
                let parked = MACOS_RECEIVE_PARK_POINT
                    .lock()
                    .ok()
                    .and_then(|point| *point);
                if let (Some((px, py)), Some(location)) = (parked, macos_current_cursor_location())
                {
                    if (location.x - px).abs() > RECEIVE_CURSOR_DRIFT_PX
                        || (location.y - py).abs() > RECEIVE_CURSOR_DRIFT_PX
                    {
                        log::info!(
                            "[diag] monitor drift show: cur=({:.0},{:.0}) park=({px:.0},{py:.0})",
                            location.x,
                            location.y
                        );
                        macos_receive_show_cursor_if_hidden();
                    }
                }
            }
            thread::sleep(Duration::from_millis(50));
        }
        // Never leave the cursor hidden after the runtime stops.
        macos_receive_show_cursor_if_hidden();
    });
}

#[cfg(target_os = "linux")]
fn start_platform_receive_monitor(stop: Arc<AtomicBool>) {
    linux_input::start_receive_monitor(stop);
}

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
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

fn active_target_input_failed(
    quic_transport: &quic_transport::TransportHandle,
    active: &Mutex<Option<ActiveTarget>>,
) -> bool {
    let target = active
        .lock()
        .ok()
        .and_then(|active| active.as_ref().map(|active| active.target.clone()));
    let Some(target) = target else {
        return false;
    };
    let peer = quic_transport.peer(
        target.target_addr,
        target.transport_public_key,
        target.protocol_version,
    );
    quic_transport.peer_input_failed(&peer)
}

fn send_packet(
    quic_transport: &quic_transport::TransportHandle,
    target: &InputTarget,
    event: InputEvent,
    layout_state: &Arc<Mutex<LayoutState>>,
    input_events: &Arc<AtomicU64>,
) -> bool {
    send_packet_with_modifier_snapshot(
        quic_transport,
        target,
        event,
        None,
        layout_state,
        input_events,
    )
}

fn send_key_packet(
    quic_transport: &quic_transport::TransportHandle,
    target: &InputTarget,
    key_code: u16,
    down: bool,
    modifier_snapshot: u8,
    layout_state: &Arc<Mutex<LayoutState>>,
    input_events: &Arc<AtomicU64>,
) -> bool {
    send_packet_with_modifier_snapshot(
        quic_transport,
        target,
        InputEvent::Key { key_code, down },
        Some(modifier_snapshot),
        layout_state,
        input_events,
    )
}

fn send_packet_with_modifier_snapshot(
    quic_transport: &quic_transport::TransportHandle,
    target: &InputTarget,
    event: InputEvent,
    modifier_snapshot: Option<u8>,
    layout_state: &Arc<Mutex<LayoutState>>,
    input_events: &Arc<AtomicU64>,
) -> bool {
    let packet_context = input_packet_context(target, event, modifier_snapshot, layout_state);
    let event = packet_context.event;
    // State transitions ride a per-peer reliable queue. Releases can evict
    // stale presses under back-pressure, while CursorPark is an authoritative
    // reset boundary that discards older Down/transient state after preserving
    // explicit Up frames for mixed v2 peers. Drag motion carries
    // an authoritative button_mask, so it remains latest-wins just like hover
    // motion: replaying hundreds of old drag points after a Wi-Fi pause is worse
    // than dropping intermediate positions, and the receiver can reconcile a
    // transition that arrives on the other QUIC channel.
    let reliable_class = input_event_reliable_class(&event);
    let send_latest = matches!(event, InputEvent::MouseMove { .. });
    let key_sequence = input_packet_needs_key_sequence(&event, packet_context.modifier_snapshot)
        .then(next_key_sequence)
        .unwrap_or(0);
    let packet = InputPacket {
        protocol: INPUT_PROTOCOL.into(),
        target_device_id: target.device_id.clone(),
        origin_device_id: packet_context.origin_device_id,
        origin_port: quic_transport.port(),
        origin_transport_public_key: quic_transport.public_key().to_string(),
        origin_protocol_version: quic_transport::PROTOCOL_VERSION,
        cluster_id: packet_context.cluster_id,
        pair_secret: packet_context.pair_secret,
        modifier_snapshot: packet_context.modifier_snapshot,
        key_sequence,
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

    let send_result = if let Some(class) = reliable_class {
        quic_transport.send_reliable_input_with_class(peer, payload, class)
    } else if send_latest {
        quic_transport.send_latest_datagram(peer, payload)
    } else {
        quic_transport.send_datagram(peer, payload)
    };

    match send_result {
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

fn input_packet_needs_key_sequence(event: &InputEvent, modifier_snapshot: Option<u8>) -> bool {
    matches!(
        event,
        InputEvent::Key { .. } | InputEvent::CursorPark { .. }
    ) || modifier_snapshot.is_some()
}

fn input_event_reliable_class(event: &InputEvent) -> Option<quic_transport::ReliableInputClass> {
    use quic_transport::ReliableInputClass;

    match event {
        InputEvent::Key { down: true, .. } | InputEvent::MouseButton { down: true, .. } => {
            Some(ReliableInputClass::State)
        }
        InputEvent::Key { down: false, .. } | InputEvent::MouseButton { down: false, .. } => {
            Some(ReliableInputClass::Release)
        }
        InputEvent::CursorPark { .. } => Some(ReliableInputClass::ResetBoundary),
        InputEvent::Scroll { .. } => Some(ReliableInputClass::Transient),
        InputEvent::MouseMove { .. } => None,
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
    modifier_snapshot: Option<u8>,
    event: InputEvent,
}

fn input_packet_context(
    target: &InputTarget,
    event: InputEvent,
    modifier_snapshot: Option<u8>,
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
        modifier_snapshot,
        event,
    };

    let layout = match layout_state.try_lock() {
        Ok(layout) => layout,
        Err(TryLockError::WouldBlock) if matches!(&event, InputEvent::Key { .. }) => {
            // Mouse motion must never wait on the UI/layout lock, but modifier
            // down/up must use one consistent remap. Falling back to the raw
            // key here could map the down and not the up (or vice versa),
            // leaving Ctrl/Cmd stuck on the controlled machine.
            match layout_state.lock() {
                Ok(layout) => layout,
                Err(_) => return fallback_context(event),
            }
        }
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
    let modifier_snapshot = modifier_snapshot
        .map(|mask| remap_modifier_snapshot_for_target_layout(mask, target, &layout));

    InputPacketContext {
        origin_device_id,
        cluster_id: layout.cluster_id.clone(),
        pair_secret: layout.pair_secret.clone(),
        peer,
        modifier_snapshot,
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

fn remap_modifier_snapshot_for_target_layout(
    mask: u8,
    target: &InputTarget,
    layout: &LayoutState,
) -> u8 {
    let target_platform = target.target_platform.as_str();
    if !layout.modifier_remap
        || (target_platform != "macos" && target_platform != "windows")
        || target_platform == crate::current_platform()
    {
        return mask;
    }
    remap_modifier_mask(
        mask,
        &layout.modifier_map.control,
        &layout.modifier_map.alt,
        &layout.modifier_map.meta,
    )
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

#[cfg(any(target_os = "windows", test))]
fn reconcile_windows_modifier_events(
    physical_down: &[u16],
    forwarded_pressed: &[u16],
) -> Vec<InputEvent> {
    let mut releases = Vec::new();
    let mut presses = Vec::new();
    for family in 0..4 {
        let physical_present = physical_down
            .iter()
            .any(|key| windows_modifier_family(*key) == Some(family));
        let forwarded_present = forwarded_pressed
            .iter()
            .any(|key| windows_modifier_family(*key) == Some(family));

        if !forwarded_present {
            for key_code in physical_down
                .iter()
                .copied()
                .filter(|key| windows_modifier_family(*key) == Some(family))
            {
                if !presses.iter().any(|event| {
                    matches!(event, InputEvent::Key { key_code: queued, down: true } if *queued == key_code)
                }) {
                    presses.push(InputEvent::Key {
                        key_code,
                        down: true,
                    });
                }
            }
        } else if !physical_present {
            releases.extend(
                forwarded_pressed
                    .iter()
                    .copied()
                    .filter(|key| windows_modifier_family(*key) == Some(family))
                    .map(|key_code| InputEvent::Key {
                        key_code,
                        down: false,
                    }),
            );
        }
    }
    releases.extend(presses);
    releases
}

/// Groups generic and sided Windows virtual-key codes by physical modifier.
/// The low-level hook can report generic Control (0x11) while
/// GetAsyncKeyState reports Left Control (0xA2); treating them as one family
/// avoids sending a duplicate Down before the next ordinary key.
#[cfg(any(target_os = "windows", test))]
fn windows_modifier_family(vk: u16) -> Option<u8> {
    match vk {
        0x10 | 0xA0 | 0xA1 => Some(0), // Shift
        0x11 | 0xA2 | 0xA3 => Some(1), // Control
        0x12 | 0xA4 | 0xA5 => Some(2), // Alt
        0x5B | 0x5C => Some(3),        // Windows / Command
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

fn remap_modifier_mask(mask: u8, control: &str, alt: &str, meta: &str) -> u8 {
    let mut remapped = mask & SHIFT_MODIFIER_MASK;
    for (source_bit, target) in [
        (CONTROL_MODIFIER_MASK, control),
        (ALT_MODIFIER_MASK, alt),
        (META_MODIFIER_MASK, meta),
    ] {
        if mask & source_bit == 0 {
            continue;
        }
        let target_bit = logical_target_vk(target)
            .and_then(modifier_mask_for_key)
            .unwrap_or(source_bit);
        remapped |= target_bit;
    }
    remapped
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

    let clipboard_peer =
        if packet.origin_port != 0 && !packet.origin_transport_public_key.trim().is_empty() {
            let device_id = if packet.origin_device_id.trim().is_empty() {
                source.ip().to_string()
            } else {
                packet.origin_device_id.clone()
            };
            Some((
                device_id,
                format!("{}:{}", source.ip(), packet.origin_port),
                packet.origin_transport_public_key.clone(),
                packet.origin_protocol_version,
            ))
        } else {
            None
        };

    let mouse_origin_id = if packet.origin_device_id.trim().is_empty() {
        source.ip().to_string()
    } else {
        packet.origin_device_id.clone()
    };
    let (injected, accepted) = inject_input_event(
        layout,
        native_layout,
        &mouse_origin_id,
        packet.modifier_snapshot,
        packet.key_sequence,
        packet.event,
    );
    if accepted {
        if let Some((device_id, addr, public_key, protocol_version)) = clipboard_peer {
            // Only the controller that won sequence/origin admission owns the
            // clipboard return path. A stale or inactive paired controller's
            // packet must not redirect copies before that packet is discarded.
            set_clipboard_target(
                clipboard_target,
                device_id,
                addr,
                public_key,
                protocol_version,
                layout.cluster_id.clone(),
                layout.pair_secret.clone(),
                false,
                None,
            );
        }
    }
    if injected && accepted {
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

#[cfg(any(target_os = "windows", target_os = "linux"))]
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

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
fn platform_native_screen(screen: &Screen) -> Screen {
    screen.clone()
}

#[cfg(any(target_os = "windows", target_os = "linux"))]
fn scale_position(value: i32, scale: f64) -> i32 {
    (value as f64 * scale)
        .round()
        .clamp(i32::MIN as f64, i32::MAX as f64) as i32
}

#[cfg(any(target_os = "windows", target_os = "linux"))]
fn scale_size(value: i32, scale: f64) -> i32 {
    (value.max(1) as f64 * scale)
        .round()
        .clamp(1.0, i32::MAX as f64) as i32
}

fn remote_mouse_state() -> &'static Mutex<RemoteMouseState> {
    REMOTE_MOUSE_STATE.get_or_init(|| Mutex::new(RemoteMouseState::default()))
}

fn remote_key_sequence_state() -> &'static Mutex<RemoteKeySequenceState> {
    REMOTE_KEY_SEQUENCE_STATE.get_or_init(|| Mutex::new(RemoteKeySequenceState::default()))
}

fn admit_remote_input_with_state(
    sequence_state: &mut RemoteKeySequenceState,
    mouse_state: &mut RemoteMouseState,
    active_origin: &mut String,
    origin_id: &str,
    modifier_snapshot: Option<u8>,
    key_sequence: u64,
    event: &mut InputEvent,
) -> Option<RemoteInputAdmission> {
    let is_park = matches!(event, InputEvent::CursorPark { .. });
    let event_sequence_accepted = match &*event {
        InputEvent::Key { key_code, .. } => {
            sequence_state.accept_key(origin_id, *key_code, key_sequence)
        }
        InputEvent::CursorPark { .. } => sequence_state.accept_boundary(origin_id, key_sequence),
        _ => true,
    };
    if !event_sequence_accepted {
        return None;
    }

    let active_is_current = active_origin.as_str() == origin_id;
    let can_claim = active_origin.is_empty() && input_event_can_claim_origin(event);
    let owns_event = active_is_current || can_claim;
    let release_keys = is_park && active_is_current;
    let (mouse, mut carried_buttons) = if input_event_mouse_sequence(event).is_some() {
        let (accepted, carried_buttons) =
            prepare_remote_mouse_event_for_origin(mouse_state, origin_id, event, owns_event);
        if !accepted {
            if !release_keys {
                return None;
            }
            return Some(RemoteInputAdmission {
                inject_event: false,
                effective_modifier_snapshot: None,
                origin_changed: false,
                release_keys: true,
                carried_buttons: None,
                mouse: Some(RemoteMouseAdmission {
                    button_reconciliation: None,
                    park_accepted: false,
                }),
            });
        } else if owns_event {
            let (button_reconciliation, park_accepted) =
                authoritative_mouse_button_state(mouse_state, origin_id, event, true);
            (
                Some(RemoteMouseAdmission {
                    button_reconciliation,
                    park_accepted,
                }),
                carried_buttons,
            )
        } else {
            (
                Some(RemoteMouseAdmission {
                    button_reconciliation: None,
                    park_accepted: false,
                }),
                None,
            )
        }
    } else {
        (None, None)
    };

    let accepted_modifier_snapshot =
        modifier_snapshot.filter(|_| sequence_state.accept_snapshot(origin_id, key_sequence));
    let inject_event = owns_event
        && (!is_park || active_is_current)
        && mouse.is_none_or(|mouse| mouse.park_accepted || !is_park);
    let origin_changed = inject_event && can_claim;
    if origin_changed && mouse.is_none() {
        carried_buttons = switch_remote_mouse_origin(mouse_state, origin_id);
    }
    if inject_event && mouse.is_some_and(|mouse| mouse.park_accepted) {
        // Park is the reliable end-of-session boundary. Leaving the origin
        // unclaimed lets another controller's first sequenced MouseMove take
        // over, while the per-origin high-water below rejects pre-park moves.
        active_origin.clear();
    } else if origin_changed {
        active_origin.clear();
        active_origin.push_str(origin_id);
    }
    Some(RemoteInputAdmission {
        inject_event,
        effective_modifier_snapshot: if inject_event {
            accepted_modifier_snapshot
        } else {
            None
        },
        origin_changed,
        release_keys,
        carried_buttons,
        mouse,
    })
}

fn admit_remote_input(
    origin_id: &str,
    modifier_snapshot: Option<u8>,
    key_sequence: u64,
    event: &mut InputEvent,
) -> Option<RemoteInputAdmission> {
    let mut sequence_state = remote_key_sequence_state()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let mut mouse_state = remote_mouse_state()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let mut active_origin = REMOTE_INPUT_ORIGIN
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    admit_remote_input_with_state(
        &mut sequence_state,
        &mut mouse_state,
        &mut active_origin,
        origin_id,
        modifier_snapshot,
        key_sequence,
        event,
    )
}

fn update_remote_mouse_position(x: i32, y: i32) -> Option<MouseButton> {
    let Ok(mut state) = remote_mouse_state().lock() else {
        return None;
    };
    state.x = x;
    state.y = y;
    primary_button_from_mask(state.buttons)
}

fn update_remote_mouse_button(
    button: MouseButton,
    down: bool,
    transmitted_position: Option<(i32, i32)>,
) -> (i32, i32) {
    let Ok(mut state) = remote_mouse_state().lock() else {
        return (0, 0);
    };
    if let Some((x, y)) = transmitted_position {
        state.x = x;
        state.y = y;
    }
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
    origin_id: &str,
    modifier_snapshot: Option<u8>,
    key_sequence: u64,
    mut event: InputEvent,
) -> (bool, bool) {
    // QUIC datagrams and each reliable uni stream run on separate tasks. The
    // runtime receive gate serialises their common callback, and this inner
    // guard also makes direct callers atomic from sequence admission through
    // the final OS event: a Park cannot reset between an older Down's admission
    // and injection and let that Down re-latch the key afterwards.
    let _inject_guard = REMOTE_INPUT_INJECT_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let is_mouse_event = input_event_mouse_sequence(&event).is_some();
    let _mouse_guard = is_mouse_event.then(|| {
        REMOTE_MOUSE_INJECT_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    });
    let Some(admission) =
        admit_remote_input(origin_id, modifier_snapshot, key_sequence, &mut event)
    else {
        log::debug!(
            "discarded stale or inactive-origin remote input sequence {key_sequence} from {origin_id}"
        );
        return (true, false);
    };
    let modifier_snapshot = admission.effective_modifier_snapshot;
    if let Some((buttons, x, y)) = admission.carried_buttons {
        release_injected_mouse_buttons(buttons, x, y);
    }
    if admission.origin_changed {
        reset_injected_keys();
        #[cfg(target_os = "macos")]
        if let Ok(mut tracker) = macos_click_tracker().lock() {
            *tracker = MacClickTracker::default();
        }
    }
    if admission.release_keys && !admission.origin_changed {
        // Keyboard and mouse sequence channels advance independently. A newer
        // mouse session may overtake this park and make its cursor coordinates
        // stale, but the accepted keyboard boundary must still release old
        // keys without releasing buttons that already belong to the new drag.
        reset_injected_keys();
    }

    if let Some(mouse) = admission.mouse {
        if let Some((previous, authoritative, x, y)) = mouse.button_reconciliation {
            reconcile_injected_mouse_buttons(previous, authoritative, x, y);
        }
        #[cfg(target_os = "macos")]
        if mouse.park_accepted {
            if let Ok(mut tracker) = macos_click_tracker().lock() {
                *tracker = MacClickTracker::default();
            }
        }
    }
    if !admission.inject_event {
        // A stale same-origin Park can still be an accepted key-only boundary;
        // foreign/inactive input has both flags false and owns no side effects.
        return (true, admission.release_keys);
    }

    if let InputEvent::Key { key_code, down } = &event {
        #[cfg(target_os = "macos")]
        {
            inject_macos_key_with_modifier_snapshot(*key_code, *down, modifier_snapshot);
            return (true, true);
        }
        #[cfg(target_os = "linux")]
        {
            linux_input::inject_key_with_modifier_snapshot(*key_code, *down, modifier_snapshot);
            return (true, true);
        }
        #[cfg(target_os = "windows")]
        {
            let is_modifier = modifier_mask_for_key(*key_code).is_some();
            if !is_modifier {
                reconcile_windows_injected_modifier_snapshot(modifier_snapshot);
            }
            let injected = inject_input_command_with_platform_routing(InputCommand::Key {
                key_code: *key_code,
                down: *down,
            });
            track_windows_injected_key(*key_code, *down);
            if is_modifier {
                reconcile_windows_injected_modifier_snapshot(modifier_snapshot);
            }
            return (injected, true);
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
        {
            let _ = (key_code, down, modifier_snapshot);
        }
    }

    reconcile_non_key_injected_modifier_snapshot(modifier_snapshot);

    let Some(command) = input_event_to_command(layout, native_layout, event) else {
        return (false, true);
    };

    (inject_input_command_with_platform_routing(command), true)
}

fn input_event_mouse_sequence(event: &InputEvent) -> Option<u64> {
    Some(match event {
        InputEvent::MouseMove { sequence, .. }
        | InputEvent::MouseButton { sequence, .. }
        | InputEvent::CursorPark { sequence, .. }
        | InputEvent::Scroll { sequence, .. } => *sequence,
        InputEvent::Key { .. } => return None,
    })
}

fn input_event_can_claim_origin(event: &InputEvent) -> bool {
    matches!(
        event,
        InputEvent::MouseMove { .. }
            | InputEvent::Key { down: true, .. }
            | InputEvent::MouseButton { down: true, .. }
    )
}

fn prepare_remote_mouse_event(
    state: &mut RemoteMouseState,
    origin_id: &str,
    event: &mut InputEvent,
) -> (bool, Option<(u64, i32, i32)>) {
    prepare_remote_mouse_event_for_origin(state, origin_id, event, true)
}

fn prepare_remote_mouse_event_for_origin(
    state: &mut RemoteMouseState,
    origin_id: &str,
    event: &mut InputEvent,
    activate: bool,
) -> (bool, Option<(u64, i32, i32)>) {
    let Some(sequence) = input_event_mouse_sequence(event) else {
        return (
            true,
            activate
                .then(|| switch_remote_mouse_origin(state, origin_id))
                .flatten(),
        );
    };
    if sequence == 0 {
        return (
            true,
            activate
                .then(|| switch_remote_mouse_origin(state, origin_id))
                .flatten(),
        );
    }

    // Sequence high-water belongs to the sender, not the currently active
    // mouse. Validate before switching origin so a pre-handoff datagram cannot
    // erase the new origin's buttons and high-water state.
    let origin_sequence = state
        .sequence_by_origin
        .entry(origin_id.to_string())
        .or_default();
    match event {
        InputEvent::MouseMove { .. } => {
            if sequence <= origin_sequence.last_position_sequence {
                return (false, None);
            }
            origin_sequence.last_position_sequence = sequence;
        }
        InputEvent::CursorPark { .. } => {
            if sequence <= origin_sequence.last_position_sequence {
                return (false, None);
            }
            origin_sequence.last_position_sequence = sequence;
            origin_sequence.last_boundary_sequence = sequence;
        }
        InputEvent::Scroll { .. } => {
            if sequence <= origin_sequence.last_scroll_sequence
                || sequence <= origin_sequence.last_boundary_sequence
            {
                return (false, None);
            }
            origin_sequence.last_scroll_sequence = sequence;
        }
        InputEvent::MouseButton {
            button,
            down,
            screen_id,
            x,
            y,
            ..
        } => {
            // Reliable button state is never discarded merely because a newer
            // datagram overtook it. If its coordinates are stale, release/press
            // at the latest tracked point instead of warping backward.
            let button_index = match button {
                MouseButton::Left => 0,
                MouseButton::Right => 1,
                MouseButton::Middle => 2,
            };
            if sequence <= origin_sequence.last_button_sequence[button_index] {
                return (false, None);
            }
            origin_sequence.last_button_sequence[button_index] = sequence;
            if sequence > origin_sequence.last_position_sequence {
                origin_sequence.last_position_sequence = sequence;
            } else if *down {
                // A press older than an already-applied park/re-entry boundary
                // belongs to the previous control epoch. Releases still land
                // below so a button can never remain stuck.
                return (false, None);
            } else {
                screen_id.clear();
                *x = None;
                *y = None;
            }
        }
        InputEvent::Key { .. } => unreachable!(),
    }

    (
        true,
        activate
            .then(|| switch_remote_mouse_origin(state, origin_id))
            .flatten(),
    )
}

fn authoritative_mouse_button_state(
    state: &mut RemoteMouseState,
    origin_id: &str,
    event: &InputEvent,
    accepted: bool,
) -> (Option<(u64, u64, i32, i32)>, bool) {
    if !accepted {
        return (None, false);
    }
    let (authoritative, sequence, park) = match event {
        InputEvent::MouseMove {
            button_mask: Some(mask),
            sequence,
            ..
        } => (*mask, *sequence, false),
        InputEvent::CursorPark { sequence, .. } => (0, *sequence, true),
        _ => return (None, false),
    };
    let reconciliation = (state.buttons != authoritative).then_some((
        state.buttons,
        authoritative,
        state.x,
        state.y,
    ));
    state.buttons = authoritative;
    if sequence != 0 {
        let origin_sequence = state
            .sequence_by_origin
            .entry(origin_id.to_string())
            .or_default();
        for last_sequence in &mut origin_sequence.last_button_sequence {
            *last_sequence = (*last_sequence).max(sequence);
        }
    }
    (reconciliation, park)
}

fn switch_remote_mouse_origin(
    state: &mut RemoteMouseState,
    origin_id: &str,
) -> Option<(u64, i32, i32)> {
    if state.last_origin_id == origin_id {
        return None;
    }
    let carried = (state.buttons != 0).then_some((state.buttons, state.x, state.y));
    state.last_origin_id.clear();
    state.last_origin_id.push_str(origin_id);
    state.buttons = 0;
    carried
}

fn release_injected_mouse_buttons(buttons: u64, x: i32, y: i32) {
    for (bit, button) in [
        (LEFT_BUTTON_MASK, MouseButton::Left),
        (RIGHT_BUTTON_MASK, MouseButton::Right),
        (MIDDLE_BUTTON_MASK, MouseButton::Middle),
    ] {
        if buttons & bit != 0 {
            let _ = inject_input_command_with_platform_routing(InputCommand::MouseButton {
                button,
                down: false,
                x,
                y,
            });
        }
    }
}

fn reconcile_injected_mouse_buttons(previous: u64, authoritative: u64, x: i32, y: i32) {
    release_injected_mouse_buttons(previous & !authoritative, x, y);
    let pressed = authoritative & !previous;
    for (bit, button) in [
        (LEFT_BUTTON_MASK, MouseButton::Left),
        (RIGHT_BUTTON_MASK, MouseButton::Right),
        (MIDDLE_BUTTON_MASK, MouseButton::Middle),
    ] {
        if pressed & bit != 0 {
            let _ = inject_input_command_with_platform_routing(InputCommand::MouseButton {
                button,
                down: true,
                x,
                y,
            });
        }
    }
}

/// Releases receiver-side mouse/modifier state when input sharing stops. This
/// is deliberately broader than the per-origin reset: a missing final up event
/// must not leave the OS latched after the runtime has been disabled.
pub fn reset_injected_input_state() {
    let _inject_guard = REMOTE_INPUT_INJECT_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let _mouse_guard = REMOTE_MOUSE_INJECT_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let carried_buttons = remote_mouse_state().lock().ok().and_then(|mut state| {
        let carried = (state.buttons != 0).then_some((state.buttons, state.x, state.y));
        *state = RemoteMouseState::default();
        carried
    });
    if let Some((buttons, x, y)) = carried_buttons {
        release_injected_mouse_buttons(buttons, x, y);
    }
    if let Ok(mut origin) = REMOTE_INPUT_ORIGIN.lock() {
        origin.clear();
    }
    if let Ok(mut sequence) = remote_key_sequence_state().lock() {
        *sequence = RemoteKeySequenceState::default();
    }
    reset_injected_modifiers();
}

#[cfg(any(target_os = "windows", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowsInputRoute {
    Local,
    Helper,
}

#[cfg(any(target_os = "windows", test))]
#[derive(Debug, Default)]
struct WindowsInputRouteState {
    current: Option<WindowsInputRoute>,
}

#[cfg(any(target_os = "windows", test))]
impl WindowsInputRouteState {
    /// Switching injectors while an input is held can split Down and Up across
    /// independent state trackers. The caller must release both routes before
    /// it delivers the first event to a different route.
    fn requires_release_before(&self, next: WindowsInputRoute) -> bool {
        self.current.is_some_and(|current| current != next)
    }

    fn commit(&mut self, route: WindowsInputRoute) {
        self.current = Some(route);
    }

    fn clear(&mut self) {
        self.current = None;
    }
}

fn inject_input_command_with_platform_routing(command: InputCommand) -> bool {
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
        let mut route_state = windows_input_route_state()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if matches!(command, InputCommand::ReleaseAll) {
            release_windows_injected_inputs_both_routes();
            route_state.clear();
            return true;
        }

        let desired_route = if should_route_to_windows_helper(&command) {
            WindowsInputRoute::Helper
        } else {
            WindowsInputRoute::Local
        };
        if route_state.requires_release_before(desired_route) {
            release_windows_injected_inputs_both_routes();
        }

        if desired_route == WindowsInputRoute::Helper {
            match windows_pipe_dispatcher().send(&command) {
                Ok(()) => {
                    route_state.commit(WindowsInputRoute::Helper);
                    return true;
                }
                Err(error) => {
                    note_windows_helper_unavailable(&error);
                    // The desired helper route did not receive this event. If
                    // prior events did, release that tracker before falling
                    // back to the independent local injector.
                    if route_state.requires_release_before(WindowsInputRoute::Local) {
                        release_windows_injected_inputs_both_routes();
                    }
                }
            }
        }
        inject_windows_local_command(&command);
        route_state.commit(WindowsInputRoute::Local);
        return true;
    }

    #[cfg(not(target_os = "windows"))]
    {
        inject_input_command(command);
        true
    }
}

#[cfg(target_os = "windows")]
#[derive(Default)]
struct WindowsLocalInjectedState {
    pressed_keys: Vec<u16>,
    button_mask: u64,
}

#[cfg(any(target_os = "windows", test))]
#[derive(Debug, Default)]
struct WindowsInjectedKeyState {
    pressed_keys: Vec<u16>,
}

#[cfg(any(target_os = "windows", test))]
impl WindowsInjectedKeyState {
    fn track(&mut self, key_code: u16, down: bool) {
        let key_code = match key_code {
            0x10 => 0xA0,
            0x11 => 0xA2,
            0x12 => 0xA4,
            _ => key_code,
        };
        if down {
            if !self.pressed_keys.contains(&key_code) {
                self.pressed_keys.push(key_code);
            }
            return;
        }
        if self.pressed_keys.contains(&key_code) {
            self.pressed_keys.retain(|pressed| *pressed != key_code);
        } else if let Some(family) = modifier_mask_for_key(key_code) {
            // A hook/helper can report a generic Up after a sided Down. Treat
            // the single held member of that family as the matching key.
            let family_keys = self
                .pressed_keys
                .iter()
                .copied()
                .filter(|pressed| modifier_mask_for_key(*pressed) == Some(family))
                .collect::<Vec<_>>();
            if family_keys.len() == 1 {
                self.pressed_keys
                    .retain(|pressed| *pressed != family_keys[0]);
            }
        }
    }

    fn transitions(&self, mask: u8) -> Vec<(u16, bool)> {
        modifier_snapshot_transitions(&self.pressed_keys, mask)
    }

    fn take_pressed_keys(&mut self) -> Vec<u16> {
        std::mem::take(&mut self.pressed_keys)
    }
}

#[cfg(target_os = "windows")]
fn windows_injected_key_state() -> &'static Mutex<WindowsInjectedKeyState> {
    static STATE: OnceLock<Mutex<WindowsInjectedKeyState>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(WindowsInjectedKeyState::default()))
}

#[cfg(target_os = "windows")]
fn track_windows_injected_key(key_code: u16, down: bool) {
    windows_injected_key_state()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .track(key_code, down);
}

#[cfg(target_os = "windows")]
fn reconcile_windows_injected_modifier_snapshot(mask: Option<u8>) {
    let Some(mask) = mask else {
        return;
    };
    let transitions = windows_injected_key_state()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .transitions(mask);
    if !transitions.is_empty() {
        log::info!(
            "reconciled remote Windows modifiers from snapshot mask={mask:#04x}: {transitions:?}"
        );
    }
    for (key_code, down) in transitions {
        let _ = inject_input_command_with_platform_routing(InputCommand::Key { key_code, down });
        track_windows_injected_key(key_code, down);
    }
}

#[cfg(target_os = "windows")]
fn windows_local_injected_state() -> &'static Mutex<WindowsLocalInjectedState> {
    static STATE: OnceLock<Mutex<WindowsLocalInjectedState>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(WindowsLocalInjectedState::default()))
}

#[cfg(target_os = "windows")]
fn windows_input_route_state() -> &'static Mutex<WindowsInputRouteState> {
    static STATE: OnceLock<Mutex<WindowsInputRouteState>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(WindowsInputRouteState::default()))
}

#[cfg(target_os = "windows")]
fn inject_windows_local_command(command: &InputCommand) {
    // Mouse motion is the hot path and has no held state; avoid taking a mutex
    // for it. Keys/buttons and ReleaseAll share the helper's existing tracker so
    // stop/origin-change can emit actual Up events on the normal desktop too.
    if matches!(
        command,
        InputCommand::Key { .. } | InputCommand::MouseButton { .. } | InputCommand::ReleaseAll
    ) {
        let mut state = windows_local_injected_state()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let WindowsLocalInjectedState {
            pressed_keys,
            button_mask,
        } = &mut *state;
        crate::windows_input::inject_command(command, pressed_keys, button_mask);
    } else {
        crate::windows_input::inject_command_without_tracking(command);
    }
}

#[cfg(target_os = "windows")]
fn release_windows_injected_inputs_both_routes() {
    inject_windows_local_command(&InputCommand::ReleaseAll);
    // The secure-desktop worker owns a separate tracker. Failure simply means
    // there is no helper state to release; the local release above still lands.
    let _ = windows_pipe_dispatcher().send(&InputCommand::ReleaseAll);
    *windows_injected_key_state()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner()) = WindowsInjectedKeyState::default();
}

#[cfg(target_os = "windows")]
fn release_windows_injected_keys_both_routes() {
    let pressed = windows_injected_key_state()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .take_pressed_keys();
    for key_code in pressed.into_iter().rev() {
        let command = InputCommand::Key {
            key_code,
            down: false,
        };
        // Send idempotent key-up events to both independent trackers. Unlike
        // ReleaseAll, these preserve a mouse button already held by a newer
        // session on either the normal or secure desktop route.
        inject_windows_local_command(&command);
        let _ = windows_pipe_dispatcher().send(&command);
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
        InputEvent::MouseMove {
            screen_id,
            x,
            y,
            drag_button,
            button_mask: _,
            sequence,
        } => {
            let (absolute_x, absolute_y) =
                map_event_point_to_native(layout, native_layout, &screen_id, x, y)?;
            let tracked_button = update_remote_mouse_position(absolute_x, absolute_y);
            Some(InputCommand::MouseMove {
                x: absolute_x,
                y: absolute_y,
                drag_button: if sequence == 0 {
                    drag_button.or(tracked_button)
                } else {
                    // New senders carry authoritative button state on every
                    // move. Do not turn a late pre-click datagram into a drag.
                    drag_button
                },
            })
        }
        InputEvent::CursorPark {
            screen_id, x, y, ..
        } => {
            let (absolute_x, absolute_y) =
                map_event_point_to_native(layout, native_layout, &screen_id, x, y)?;
            Some(InputCommand::CursorPark {
                x: absolute_x,
                y: absolute_y,
            })
        }
        InputEvent::MouseButton {
            button,
            down,
            screen_id,
            x,
            y,
            ..
        } => {
            let transmitted_position = match (x, y) {
                (Some(x), Some(y)) if !screen_id.is_empty() => {
                    map_event_point_to_native(layout, native_layout, &screen_id, x, y)
                }
                _ => None,
            };
            let (x, y) = update_remote_mouse_button(button, down, transmitted_position);
            Some(InputCommand::MouseButton { button, down, x, y })
        }
        InputEvent::Scroll {
            delta_x, delta_y, ..
        } => Some(InputCommand::Scroll { delta_x, delta_y }),
        InputEvent::Key { key_code, down } => Some(InputCommand::Key { key_code, down }),
    }
}

/// Maps a remote screen-relative point to this machine's native pixel coords.
fn map_event_point_to_native(
    layout: &LayoutState,
    native_layout: &LayoutState,
    screen_id: &str,
    x: i32,
    y: i32,
) -> Option<(i32, i32)> {
    let screen = local_screen_for_event(layout, screen_id)?;
    let native_screen = local_screen_for_event(native_layout, screen_id)
        .map(platform_native_screen)
        .unwrap_or_else(|| platform_native_screen(screen));
    let absolute_x =
        map_relative_to_native_axis(x, screen.width, native_screen.x, native_screen.width);
    let absolute_y =
        map_relative_to_native_axis(y, screen.height, native_screen.y, native_screen.height);
    Some((absolute_x, absolute_y))
}

fn inject_input_command(command: InputCommand) {
    // A reliable button/scroll/key can overtake the first best-effort move when
    // control re-enters this Mac. Any accepted active-control input therefore
    // reveals the cursor; waiting only for MouseMove leaves it hidden during a
    // perfectly normal cross-stream reorder followed by a quick click.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    if input_command_reveals_parked_cursor(&command) {
        #[cfg(target_os = "macos")]
        macos_receive_show_cursor_if_hidden();
        #[cfg(target_os = "linux")]
        linux_input::receive_show_cursor_if_hidden();
    }

    match command {
        InputCommand::MouseMove { x, y, drag_button } => {
            inject_mouse_move(x, y, drag_button);
        }
        InputCommand::MouseButton { button, down, x, y } => inject_mouse_button(button, down, x, y),
        InputCommand::Scroll { delta_x, delta_y } => inject_scroll(delta_x, delta_y),
        InputCommand::Key { key_code, down } => inject_key(key_code, down),
        InputCommand::CursorPark { x, y } => inject_cursor_park(x, y),
        InputCommand::ReleaseAll | InputCommand::SecureAttention => {}
    }
}

fn input_command_reveals_parked_cursor(command: &InputCommand) -> bool {
    matches!(
        command,
        InputCommand::MouseMove { .. }
            | InputCommand::MouseButton { down: true, .. }
            | InputCommand::Scroll { .. }
    )
}

/// Control has left this client. On macOS, hide the cursor (it reappears on the
/// next injected move or when the local user moves the mouse, via the
/// receive-monitor drift check). Elsewhere, just tuck it into the corner.
fn inject_cursor_park(x: i32, y: i32) {
    #[cfg(target_os = "macos")]
    macos_receive_hide_cursor(x, y);
    #[cfg(target_os = "linux")]
    linux_input::receive_hide_cursor(x, y);
    // Other platforms tuck the cursor without a native hide implementation.
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    inject_mouse_move(x, y, None);
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

    let own_session = current_windows_session_id()?;
    // After an RDP hand-off the service moves the worker to the *active*
    // session (which can be the console logon-screen session) while this
    // process may still live in the now-detached user session. Try both pipe
    // names so lock-screen input keeps working through RDP transitions.
    let console_session =
        unsafe { windows_sys::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId() };

    let mut candidates = vec![own_session];
    if console_session != u32::MAX && console_session != own_session {
        candidates.push(console_session);
    }

    let mut last_error = format!("input helper pipe for session {own_session} unavailable");
    for session_id in candidates {
        let pipe_name = crate::shared_input::input_pipe_name(session_id);
        match OpenOptions::new().write(true).open(&pipe_name) {
            Ok(file) => return Ok(file),
            Err(error) => last_error = format!("open input helper pipe {pipe_name}: {error}"),
        }
    }
    Err(last_error)
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
    stop: Arc<AtomicBool>,
    /// Serializes all sender-side packet production with final release. Once
    /// `stop` is set, cleanup takes this gate after every in-flight callback and
    /// no later Down event can overtake the final Park/Ups.
    send_gate: Mutex<()>,
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
static MAC_CAPTURE_CONTEXT: Mutex<Option<Arc<MacCaptureContext>>> = Mutex::new(None);

#[cfg(target_os = "macos")]
fn macos_capture_context() -> Option<Arc<MacCaptureContext>> {
    MAC_CAPTURE_CONTEXT
        .lock()
        .ok()
        .and_then(|context| context.clone())
}

#[cfg(target_os = "macos")]
fn clear_macos_capture_context(expected: &Arc<MacCaptureContext>) {
    if let Ok(mut context) = MAC_CAPTURE_CONTEXT.lock() {
        if context
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, expected))
        {
            *context = None;
        }
    }
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

    if context.stop.load(Ordering::Relaxed) {
        return event;
    }

    // The regular event callback may already hold send_gate and repins at its
    // tail. Avoid a re-entrant deadlock by skipping this redundant raw-tap
    // repin when the gate is busy. During stop, final release owns the same gate
    // so a late gesture can never decouple/hide the cursor again afterwards.
    let Ok(_send_guard) = context.send_gate.try_lock() else {
        // A regular remote-input callback already owns the gate and performs
        // the repin at its tail. Still suppress this system gesture; only final
        // stop (which sets `stop` before taking the gate) should let it through.
        return if !context.stop.load(Ordering::Relaxed)
            && context.remote_active.load(Ordering::Relaxed)
        {
            std::ptr::null_mut()
        } else {
            event
        };
    };
    if !context.stop.load(Ordering::Relaxed) && context.remote_active.load(Ordering::Relaxed) {
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
    stop: Arc<AtomicBool>,
    send_gate: Mutex<()>,
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
    // GetTickCount64 of the last time either low-level hook callback fired.
    // The pump loop compares this against system-wide input activity to detect
    // hooks Windows silently removed (slow-callback timeout, working-set trim
    // after hours in the tray) — the "works, then goes dead until restart" bug.
    last_hook_event_ms: AtomicU64,
}

#[cfg(target_os = "windows")]
fn windows_capture_context() -> Option<Arc<WindowsCaptureContext>> {
    WINDOWS_CAPTURE_CONTEXT
        .lock()
        .ok()
        .and_then(|context| context.clone())
}

#[cfg(target_os = "windows")]
fn clear_windows_capture_context(expected: &Arc<WindowsCaptureContext>) {
    if let Ok(mut context) = WINDOWS_CAPTURE_CONTEXT.lock() {
        if context
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, expected))
        {
            *context = None;
        }
    }
}

#[cfg(target_os = "windows")]
fn windows_tick_ms() -> u64 {
    unsafe { windows_sys::Win32::System::SystemInformation::GetTickCount64() }
}

/// Late hook servicing is what makes Windows silently remove low-level hooks;
/// keep the pump thread ahead of ordinary load.
#[cfg(target_os = "windows")]
fn set_windows_capture_thread_priority() {
    use windows_sys::Win32::System::Threading::{
        GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST,
    };
    let _ = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST) };
}

#[cfg(target_os = "windows")]
struct WindowsInputHooks {
    mouse: windows_sys::Win32::UI::WindowsAndMessaging::HHOOK,
    keyboard: windows_sys::Win32::UI::WindowsAndMessaging::HHOOK,
    installed_at_ms: u64,
}

#[cfg(target_os = "windows")]
impl WindowsInputHooks {
    fn install() -> Result<Self, String> {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL, WH_MOUSE_LL,
        };

        let mouse = unsafe {
            SetWindowsHookExW(
                WH_MOUSE_LL,
                Some(windows_mouse_proc),
                std::ptr::null_mut(),
                0,
            )
        };
        if mouse.is_null() {
            return Err("failed to install Windows mouse hook".into());
        }
        let keyboard = unsafe {
            SetWindowsHookExW(
                WH_KEYBOARD_LL,
                Some(windows_keyboard_proc),
                std::ptr::null_mut(),
                0,
            )
        };
        if keyboard.is_null() {
            unsafe {
                let _ = UnhookWindowsHookEx(mouse);
            }
            return Err("failed to install Windows keyboard hook".into());
        }

        Ok(Self {
            mouse,
            keyboard,
            installed_at_ms: windows_tick_ms(),
        })
    }

    fn uninstall(&mut self) {
        use windows_sys::Win32::UI::WindowsAndMessaging::UnhookWindowsHookEx;

        for hook in [&mut self.mouse, &mut self.keyboard] {
            if !hook.is_null() {
                unsafe {
                    let _ = UnhookWindowsHookEx(*hook);
                }
                *hook = std::ptr::null_mut();
            }
        }
    }

    /// Drops and re-adds both hooks. On failure the null handles make the next
    /// watchdog tick retry; `installed_at_ms` paces the staleness clock so a
    /// reinstall is not immediately re-triggered.
    fn reinstall(&mut self, context: &WindowsCaptureContext) {
        self.uninstall();
        match Self::install() {
            Ok(hooks) => *self = hooks,
            Err(error) => {
                self.installed_at_ms = windows_tick_ms();
                log::warn!("failed to reinstall Windows input hooks: {error}");
            }
        }
        context.last_hook_event_ms.store(0, Ordering::Relaxed);
    }
}

/// True when the system keeps seeing input but our hooks have gone quiet:
/// Windows removed them silently (callback timeout, thread starvation after a
/// working-set trim, ...). A genuinely idle machine trips neither condition.
#[cfg(target_os = "windows")]
fn windows_hooks_look_dead(context: &WindowsCaptureContext, hooks: &WindowsInputHooks) -> bool {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};

    if hooks.mouse.is_null() || hooks.keyboard.is_null() {
        return true;
    }

    const STALE_MS: u64 = 3000;
    let now_ms = windows_tick_ms();
    let last_hook = context
        .last_hook_event_ms
        .load(Ordering::Relaxed)
        .max(hooks.installed_at_ms);
    if now_ms.saturating_sub(last_hook) < STALE_MS {
        return false;
    }

    let mut info = LASTINPUTINFO {
        cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    if unsafe { GetLastInputInfo(&mut info) } == 0 {
        return false;
    }
    // dwTime is a 32-bit GetTickCount value; compare in 32-bit tick space.
    let since_system_input = (now_ms as u32).wrapping_sub(info.dwTime);
    u64::from(since_system_input) < STALE_MS
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

#[cfg(target_os = "windows")]
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

#[cfg(target_os = "windows")]
fn physical_windows_modifiers() -> Vec<u16> {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;

    // Query sided keys so two physically-held modifiers remain distinguishable.
    [0xA0_u16, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0x5B, 0x5C]
        .into_iter()
        .filter(|key_code| (unsafe { GetAsyncKeyState(i32::from(*key_code)) } as u16 & 0x8000) != 0)
        .collect()
}

#[cfg(target_os = "windows")]
fn sync_held_modifiers_windows(
    context: &WindowsCaptureContext,
    target: &InputTarget,
) -> Option<u8> {
    let physical = physical_windows_modifiers();
    let forwarded = context
        .pressed_keys
        .lock()
        .map(|pressed| pressed.clone())
        .unwrap_or_default();

    for event in reconcile_windows_modifier_events(&physical, &forwarded) {
        let InputEvent::Key { key_code, down } = event else {
            continue;
        };
        if !send_packet(
            &context.quic_transport,
            target,
            InputEvent::Key { key_code, down },
            &context.layout_state,
            &context.input_events,
        ) {
            return None;
        }
        track_forwarded_key(&context.pressed_keys, key_code, down);
    }
    Some(modifier_mask_for_keys(&physical))
}

#[cfg(target_os = "macos")]
fn sync_held_modifiers_macos(context: &MacCaptureContext, target: &InputTarget) {
    let held = context
        .pressed_modifiers
        .lock()
        .map(|modifiers| modifiers.clone())
        .unwrap_or_default();
    for key_code in held {
        let _ = send_packet(
            &context.quic_transport,
            target,
            InputEvent::Key {
                key_code,
                down: true,
            },
            &context.layout_state,
            &context.input_events,
        );
    }
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
                    screen_id: String::new(),
                    x: None,
                    y: None,
                    sequence: next_mouse_sequence(),
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
    // `pressed_modifiers` is the physical state cache. Keep it intact while
    // returning local so re-entry before key release can re-send held modifiers;
    // the next local FlagsChanged event refreshes it naturally.
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

// Wakes the clipboard sync thread the moment the target changes, so a crossing
// pushes the clipboard immediately instead of after the idle-poll sleep.
// ponytail: process-global — there is exactly one clipboard sync loop.
static CLIPBOARD_TARGET_WAKE: OnceLock<(Mutex<u64>, Condvar)> = OnceLock::new();

fn clipboard_target_wake() -> &'static (Mutex<u64>, Condvar) {
    CLIPBOARD_TARGET_WAKE.get_or_init(|| (Mutex::new(0), Condvar::new()))
}

fn notify_clipboard_target_changed() {
    let (generation, condvar) = clipboard_target_wake();
    if let Ok(mut generation) = generation.lock() {
        *generation = generation.wrapping_add(1);
    }
    condvar.notify_all();
}

/// Blocks until the clipboard target changes or `timeout` elapses.
pub fn wait_for_clipboard_target_change(timeout: Duration) {
    let (generation, condvar) = clipboard_target_wake();
    let Ok(guard) = generation.lock() else {
        thread::sleep(timeout);
        return;
    };
    let start = *guard;
    let _ = condvar.wait_timeout_while(guard, timeout, |generation| *generation == start);
}

pub fn clear_clipboard_target(target: &Arc<Mutex<Option<ClipboardTarget>>>) {
    if let Ok(mut target) = target.lock() {
        *target = None;
    }
    notify_clipboard_target_changed();
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
    push_on_bind: bool,
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
            push_on_bind,
            expires_at: expires_in.map(|duration| Instant::now() + duration),
        });
    }
    notify_clipboard_target_changed();
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
        true,
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
    context
        .last_hook_event_ms
        .store(windows_tick_ms(), Ordering::Relaxed);
    if !cached_windows_input_desktop_is_default() {
        release_windows_remote_control(&context, true);
        return unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) };
    }
    let _send_guard = context
        .send_gate
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if context.stop.load(Ordering::Relaxed) {
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
    context
        .last_hook_event_ms
        .store(windows_tick_ms(), Ordering::Relaxed);
    if !cached_windows_input_desktop_is_default() {
        release_windows_remote_control(&context, true);
        return unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) };
    }
    let _send_guard = context
        .send_gate
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if context.stop.load(Ordering::Relaxed) {
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
            release_windows_remote_control_inner(&context, false);
            return 1;
        }
        // Windows may consume the Win-key hook callback while still delivering
        // the Arrow callback (shell shortcuts and keyboard utilities can do
        // this). Reconcile the physical modifier state before the ordinary
        // KeyDown so the reliable input queue always contains Win-down before
        // Arrow-down. The target-layout remapper then turns Win into macOS
        // Control according to the user's mapping.
        let modifier_snapshot = if down && windows_modifier_family(key_code).is_none() {
            let Some(mask) = sync_held_modifiers_windows(&context, &target) else {
                return 1;
            };
            Some(mask)
        } else {
            None
        };
        let sent = if let Some(snapshot) = modifier_snapshot {
            send_key_packet(
                &context.quic_transport,
                &target,
                key_code,
                down,
                snapshot,
                &context.layout_state,
                &context.input_events,
            )
        } else {
            send_packet(
                &context.quic_transport,
                &target,
                InputEvent::Key { key_code, down },
                &context.layout_state,
                &context.input_events,
            )
        };
        if sent {
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
#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
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
    let _send_guard = context
        .send_gate
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    release_windows_remote_control_inner(context, clear_clipboard);
}

#[cfg(target_os = "windows")]
fn release_windows_remote_control_inner(context: &WindowsCaptureContext, clear_clipboard: bool) {
    let active = context
        .active
        .lock()
        .ok()
        .and_then(|mut active| active.take());

    if let Some(active) = active {
        release_forwarded_keys_windows(context, &active.target);
        release_remote_buttons(
            &context.quic_transport,
            &active.target,
            &context.remote_button_mask,
            &context.layout_state,
            &context.input_events,
        );
        // Park last: it is the receiver's authoritative handoff boundary and
        // must remain the final reliable mouse event after any synthetic Ups.
        let _ = send_remote_cursor_park(
            &context.quic_transport,
            &active,
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

/// Synchronously enqueue the final remote boundary before a runtime stop or
/// process exit can tear down the QUIC transport. Capture-thread cleanup calls
/// the same functions as a fallback, but this path runs while the transport is
/// still known to be alive.
pub fn release_active_remote_control() {
    #[cfg(target_os = "macos")]
    if let Some(context) = macos_capture_context() {
        let _send_guard = context
            .send_gate
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        return_to_local_macos(&context);
        context.remote_active.store(false, Ordering::Relaxed);
        set_macos_cursor_decoupled(false);
        set_macos_warp_suppression_interval(MACOS_DEFAULT_WARP_SUPPRESSION_SECS);
        show_macos_cursor_if_needed(&context);
        clear_clipboard_target(&context.clipboard_target);
    }

    #[cfg(target_os = "windows")]
    if let Some(context) = windows_capture_context() {
        release_windows_remote_control(&context, true);
    }

    #[cfg(target_os = "linux")]
    linux_input::release_active_remote_control();
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
            // This is the SetCursorPos anchor event, not user motion. Consume
            // the one-shot guard here so the next real movement is never lost.
            context.just_crossed.store(false, Ordering::Relaxed);
            return true;
        }

        if context.just_crossed.swap(false, Ordering::Relaxed)
            && should_ignore_initial_anchor_warp_delta(active_target.target.edge, dx, dy)
        {
            // Only suppress a synthetic delta that points straight back toward
            // the entry edge. Forward genuine movement into the remote.
            set_windows_cursor(anchor.0.round() as i32, anchor.1.round() as i32);
            return true;
        }

        active_target.x += dx;
        active_target.y += dy;

        if update_active_remote_screen(active_target, dx, dy, &context.layout_state) {
            let point = local_return_point(active_target);
            let target = active_target.target.clone();
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
            // Park after all releases so no trailing ButtonUp can re-show the
            // remote cursor or supersede this authoritative boundary.
            let _ = send_remote_cursor_park(
                &context.quic_transport,
                active_target,
                &context.layout_state,
                &context.input_events,
            );
            *active = None;
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
        let button_mask = context.remote_button_mask.load(Ordering::Relaxed);
        let dragging = button_mask != 0;
        if should_send_mouse_move(&context.last_mouse_move_sent, dragging) {
            if !send_remote_mouse_move_with_drag(
                &context.quic_transport,
                active_target,
                button_mask,
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
        let _ = sync_held_modifiers_windows(context, &active_target.target);
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

    let Some(modifier_snapshot) = sync_held_modifiers_windows(context, &active_target.target)
    else {
        return false;
    };
    let sent = send_packet_with_modifier_snapshot(
        &context.quic_transport,
        &active_target.target,
        InputEvent::MouseButton {
            button,
            down,
            screen_id: active_target.current_screen_id.clone(),
            x: Some(active_target.x.round() as i32),
            y: Some(active_target.y.round() as i32),
            sequence: next_mouse_sequence(),
        },
        Some(modifier_snapshot),
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

    let Some(modifier_snapshot) = sync_held_modifiers_windows(context, &active_target.target)
    else {
        return false;
    };
    send_packet_with_modifier_snapshot(
        &context.quic_transport,
        &active_target.target,
        InputEvent::Scroll {
            delta_x,
            delta_y,
            sequence: next_mouse_sequence(),
        },
        Some(modifier_snapshot),
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
    modifier_snapshot: u8,
) -> bool {
    let sent = send_packet_with_modifier_snapshot(
        &context.quic_transport,
        &active_target.target,
        InputEvent::MouseButton {
            button,
            down,
            screen_id: active_target.current_screen_id.clone(),
            x: Some(active_target.x.round() as i32),
            y: Some(active_target.y.round() as i32),
            sequence: next_mouse_sequence(),
        },
        Some(modifier_snapshot),
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

    let _send_guard = context
        .send_gate
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if context.stop.load(Ordering::Relaxed) {
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
    let event_modifier_snapshot = modifier_mask_for_keys(&mac_modifier_vks(event));

    let sent = match event_type {
        CGEventType::LeftMouseDown => send_macos_mouse_button(
            context,
            &active_target,
            MouseButton::Left,
            true,
            event_modifier_snapshot,
        ),
        CGEventType::LeftMouseUp => send_macos_mouse_button(
            context,
            &active_target,
            MouseButton::Left,
            false,
            event_modifier_snapshot,
        ),
        CGEventType::RightMouseDown => send_macos_mouse_button(
            context,
            &active_target,
            MouseButton::Right,
            true,
            event_modifier_snapshot,
        ),
        CGEventType::RightMouseUp => send_macos_mouse_button(
            context,
            &active_target,
            MouseButton::Right,
            false,
            event_modifier_snapshot,
        ),
        CGEventType::OtherMouseDown => send_macos_mouse_button(
            context,
            &active_target,
            MouseButton::Middle,
            true,
            event_modifier_snapshot,
        ),
        CGEventType::OtherMouseUp => send_macos_mouse_button(
            context,
            &active_target,
            MouseButton::Middle,
            false,
            event_modifier_snapshot,
        ),
        CGEventType::ScrollWheel => {
            let delta_y =
                event.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1) as i32;
            let delta_x =
                event.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_2) as i32;
            send_packet_with_modifier_snapshot(
                &context.quic_transport,
                &target,
                InputEvent::Scroll {
                    delta_x,
                    delta_y,
                    sequence: next_mouse_sequence(),
                },
                Some(event_modifier_snapshot),
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
                let sent = send_key_packet(
                    &context.quic_transport,
                    &target,
                    key_code,
                    down,
                    modifier_mask_for_keys(&mac_modifier_vks(event)),
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
                context.remote_active.store(false, Ordering::Relaxed);
                context.just_crossed.store(false, Ordering::Relaxed);
                context
                    .suppress_next_mouse_delta
                    .store(false, Ordering::Relaxed);
                // Keep the clipboard peer so copies still sync after returning.
                release_held_remote_inputs_macos(context, &target);
                let _ = send_remote_cursor_park(
                    &context.quic_transport,
                    active_target,
                    &context.layout_state,
                    &context.input_events,
                );
                *active = None;
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
            let button_mask = context.remote_button_mask.load(Ordering::Relaxed);
            let dragging = button_mask != 0;
            if should_send_mouse_move(&context.last_mouse_move_sent, dragging) {
                if !send_remote_mouse_move_with_drag(
                    &context.quic_transport,
                    active_target,
                    button_mask,
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
                    if let Ok(mut anchor) = context.anchor.lock() {
                        *anchor = None;
                    }
                    set_macos_warp_suppression_interval(MACOS_DEFAULT_WARP_SUPPRESSION_SECS);
                    set_macos_cursor_decoupled(false);
                    show_macos_cursor_if_needed(context);
                    return CallbackResult::Keep;
                }
            }
            if repin_macos_cursor_if_drifted(context, location)
                && !context.main_window_visible.load(Ordering::Relaxed)
            {
                reassert_macos_hidden_window_cursor(context, true);
            }
            // Re-pinning also runs from the capture run loop because mouse-move
            // callbacks can stop arriving once the pointer is over the client.
            return CallbackResult::Drop;
        }
    }

    let targets = current_input_targets(&context.layout_state, &context.native_layout);
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
        sync_held_modifiers_macos(context, &active_target.target);
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
        Edge::Right => {
            x <= 0.0 && dx <= -MIN_CROSSING_DELTA && dx.abs() >= dy.abs() * RETURN_AXIS_DOMINANCE
        }
        Edge::Left => {
            x >= (screen.width - 1) as f64
                && dx >= MIN_CROSSING_DELTA
                && dx.abs() >= dy.abs() * RETURN_AXIS_DOMINANCE
        }
        Edge::Bottom => {
            y <= 0.0 && dy <= -MIN_CROSSING_DELTA && dy.abs() >= dx.abs() * RETURN_AXIS_DOMINANCE
        }
        Edge::Top => {
            y >= (screen.height - 1) as f64
                && dy >= MIN_CROSSING_DELTA
                && dy.abs() >= dx.abs() * RETURN_AXIS_DOMINANCE
        }
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

    // Land just inside the entry edge. This is the spatial re-arm that prevents
    // an immediate bounce without imposing any time-based input freeze.
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
    send_remote_mouse_move_with_drag(quic_transport, active, 0, layout_state, input_events)
}

fn send_remote_mouse_move_with_drag(
    quic_transport: &quic_transport::TransportHandle,
    active: &ActiveTarget,
    button_mask: u64,
    layout_state: &Arc<Mutex<LayoutState>>,
    input_events: &Arc<AtomicU64>,
) -> bool {
    let drag_button = button_from_mask(button_mask);
    send_packet(
        quic_transport,
        &active.target,
        InputEvent::MouseMove {
            screen_id: active.current_screen_id.clone(),
            x: active.x.round() as i32,
            y: active.y.round() as i32,
            drag_button,
            button_mask: Some(button_mask),
            sequence: next_mouse_sequence(),
        },
        layout_state,
        input_events,
    )
}

fn local_anchor_point(active: &ActiveTarget) -> (f64, f64) {
    local_return_point(active)
}

/// When control returns to the local machine, tuck the controlled cursor into
/// the bottom-right *region* of the remote screen instead of leaving it parked
/// at the shared edge. True cursor hiding isn't reliably possible on the
/// controlled side, so tucking it away is the seamless-feeling approximation.
///
/// Deliberately NOT the exact last pixel: parking on the very corner triggers
/// the remote's hot corner (macOS Show Desktop / Mission Control) or Windows
/// Aero Peek, which yanked every window to the screen edge on each crossing.
/// The margin clears the corner-action trip zone while staying off the edge.
#[cfg_attr(not(any(target_os = "windows", target_os = "macos")), allow(dead_code))]
fn send_remote_cursor_park(
    quic_transport: &quic_transport::TransportHandle,
    active: &ActiveTarget,
    layout_state: &Arc<Mutex<LayoutState>>,
    input_events: &Arc<AtomicU64>,
) -> bool {
    const PARK_CORNER_MARGIN: i32 = 64;
    let park_x = (active.current_screen.width - 1 - PARK_CORNER_MARGIN)
        .max(active.current_screen.width / 2)
        .max(0);
    let park_y = (active.current_screen.height - 1 - PARK_CORNER_MARGIN)
        .max(active.current_screen.height / 2)
        .max(0);
    send_packet(
        quic_transport,
        &active.target,
        InputEvent::CursorPark {
            screen_id: active.current_screen_id.clone(),
            x: park_x,
            y: park_y,
            sequence: next_mouse_sequence(),
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
    sync_held_modifiers_macos(context, &active_target.target);
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
    context.remote_active.store(false, Ordering::Relaxed);
    context.just_crossed.store(false, Ordering::Relaxed);
    context
        .suppress_next_mouse_delta
        .store(false, Ordering::Relaxed);
    release_held_remote_inputs_macos(context, &target);
    let _ = send_remote_cursor_park(
        &context.quic_transport,
        &active_target,
        &context.layout_state,
        &context.input_events,
    );
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
        let drifted = if let Some(location) = macos_current_cursor_location() {
            repin_macos_cursor_if_drifted(context, location)
        } else {
            force_repin_macos_cursor_to_anchor(context);
            true
        };
        reassert_macos_hidden_window_cursor(context, drifted);
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
                let _ = sync_held_modifiers_windows(context, &active_target.target);
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
            // The capture-loop caller already holds send_gate.
            release_windows_remote_control_inner(context, false);
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
/// the returned (retained) activity token for the whole app lifetime (armed in
/// lib.rs setup — receive-only clients inject input from the background too).
/// The option set still allows the machine to idle-sleep.
#[cfg(target_os = "macos")]
pub fn set_macos_app_nap_suppressed(suppress: bool) {
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
            log::info!(
                "[diag] macOS App Nap suppression armed (activity={})",
                retained as usize
            );
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
    set_macos_cursor_transparent_inner(transparent, true);
}

#[cfg(target_os = "macos")]
fn set_macos_cursor_transparent_current() {
    set_macos_cursor_transparent_inner(true, false);
}

#[cfg(target_os = "macos")]
fn set_macos_cursor_transparent_inner(transparent: bool, push: bool) {
    use std::ffi::c_void;
    use std::os::raw::c_char;

    #[link(name = "objc")]
    extern "C" {
        fn objc_getClass(name: *const c_char) -> *mut c_void;
        fn sel_registerName(name: *const c_char) -> *mut c_void;
        fn objc_msgSend();
    }

    unsafe {
        let nscursor = objc_getClass(b"NSCursor\0".as_ptr() as *const c_char);
        if nscursor.is_null() {
            return;
        }

        if !transparent {
            // Never pop unless this process actually pushed the cached cursor.
            if !MACOS_TRANSPARENT_CURSOR_PUSHED.swap(false, Ordering::Relaxed) {
                return;
            }
            let pop_sel = sel_registerName(b"pop\0".as_ptr() as *const c_char);
            let pop: extern "C" fn(*mut c_void, *mut c_void) =
                std::mem::transmute(objc_msgSend as *const ());
            pop(nscursor, pop_sel);
            return;
        }

        let Some(cursor) = macos_transparent_cursor() else {
            return;
        };

        let first_push = push
            && MACOS_TRANSPARENT_CURSOR_PUSHED
                .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok();
        if !first_push && !MACOS_TRANSPARENT_CURSOR_PUSHED.load(Ordering::Relaxed) {
            return;
        }
        let apply_sel = if first_push {
            sel_registerName(b"push\0".as_ptr() as *const c_char)
        } else {
            sel_registerName(b"set\0".as_ptr() as *const c_char)
        };
        let apply: extern "C" fn(*mut c_void, *mut c_void) =
            std::mem::transmute(objc_msgSend as *const ());
        apply(cursor, apply_sel);
    }
}

#[cfg(target_os = "macos")]
static MACOS_TRANSPARENT_CURSOR_PUSHED: AtomicBool = AtomicBool::new(false);

/// PNG signature + 1x1 RGBA IHDR + zlib-compressed filter byte and transparent
/// pixel. Keep this outside the AppKit constructor so the unit test can decode
/// the exact bytes passed to `NSImage initWithData:`.
#[cfg(target_os = "macos")]
const MACOS_TRANSPARENT_PNG: [u8; 68] = [
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0b, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x60, 0x00, 0x02, 0x00,
    0x00, 0x05, 0x00, 0x01, 0xe9, 0xfa, 0xdc, 0xd8, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44,
    0xae, 0x42, 0x60, 0x82,
];

#[cfg(target_os = "macos")]
fn macos_transparent_cursor() -> Option<*mut std::ffi::c_void> {
    use std::ffi::c_void;
    use std::os::raw::{c_char, c_double};

    static CURSOR: OnceLock<usize> = OnceLock::new();

    let cursor = *CURSOR.get_or_init(|| unsafe {
        #[link(name = "objc")]
        extern "C" {
            fn objc_getClass(name: *const c_char) -> *mut c_void;
            fn sel_registerName(name: *const c_char) -> *mut c_void;
            fn objc_msgSend();
        }

        let nscursor = objc_getClass(b"NSCursor\0".as_ptr() as *const c_char);
        let nsimage = objc_getClass(b"NSImage\0".as_ptr() as *const c_char);
        let nsdata = objc_getClass(b"NSData\0".as_ptr() as *const c_char);
        if nscursor.is_null() || nsimage.is_null() || nsdata.is_null() {
            return 0;
        }

        let data_sel = sel_registerName(b"dataWithBytes:length:\0".as_ptr() as *const c_char);
        let data_with: extern "C" fn(*mut c_void, *mut c_void, *const u8, usize) -> *mut c_void =
            std::mem::transmute(objc_msgSend as *const ());
        let data = data_with(
            nsdata,
            data_sel,
            MACOS_TRANSPARENT_PNG.as_ptr(),
            MACOS_TRANSPARENT_PNG.len(),
        );
        if data.is_null() {
            return 0;
        }

        let alloc_sel = sel_registerName(b"alloc\0".as_ptr() as *const c_char);
        let alloc: extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void =
            std::mem::transmute(objc_msgSend as *const ());
        let init_image_sel = sel_registerName(b"initWithData:\0".as_ptr() as *const c_char);
        let init_image: extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> *mut c_void =
            std::mem::transmute(objc_msgSend as *const ());
        let image = init_image(alloc(nsimage, alloc_sel), init_image_sel, data);
        if image.is_null() {
            return 0;
        }

        let init_cursor_sel =
            sel_registerName(b"initWithImage:hotSpot:\0".as_ptr() as *const c_char);
        let init_cursor: extern "C" fn(
            *mut c_void,
            *mut c_void,
            *mut c_void,
            c_double,
            c_double,
        ) -> *mut c_void = std::mem::transmute(objc_msgSend as *const ());
        init_cursor(alloc(nscursor, alloc_sel), init_cursor_sel, image, 0.0, 0.0) as usize
    });

    (cursor != 0).then_some(cursor as *mut c_void)
}

#[cfg(target_os = "macos")]
fn repin_macos_cursor_if_drifted(
    context: &MacCaptureContext,
    location: core_graphics::geometry::CGPoint,
) -> bool {
    let (drift_threshold_px, repin_interval_ms) =
        macos_cursor_repin_policy(context.main_window_visible.load(Ordering::Relaxed));

    let Ok(anchor) = context.anchor.lock() else {
        return false;
    };
    let Some((x, y)) = *anchor else {
        return false;
    };
    drop(anchor);

    let dx = location.x - x;
    let dy = location.y - y;
    if dx.abs() <= drift_threshold_px && dy.abs() <= drift_threshold_px {
        return false;
    }

    if !macos_cursor_repin_due(context, Duration::from_millis(repin_interval_ms)) {
        return false;
    }

    // When MyKVM is not frontmost, macOS can re-associate the cursor with the
    // physical mouse despite CGAssociateMouseAndMouseCursorPosition(false).
    // Re-pin only after actual drift and at a capped rate.
    set_macos_cursor_decoupled(true);
    move_macos_cursor_without_event(context, core_graphics::geometry::CGPoint::new(x, y));
    true
}

#[cfg(target_os = "macos")]
fn macos_cursor_repin_policy(main_window_visible: bool) -> (f64, u64) {
    if main_window_visible {
        (1.5, 8)
    } else {
        // A hidden/background app can observe tiny WindowServer cursor drift
        // continuously. Re-warping for every 1-2px wobble creates the visible
        // edge hitch; only correct meaningful drift and cap it at 20Hz.
        (48.0, 50)
    }
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
fn reassert_macos_hidden_window_cursor(context: &MacCaptureContext, transparent_now: bool) {
    let Ok(hidden) = context.cursor_hidden.lock() else {
        return;
    };
    if !*hidden {
        return;
    }
    drop(hidden);

    if transparent_now {
        set_macos_cursor_transparent_current();
    }

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

    // SetsCursorInBackground and the global hide counter are armed exactly once
    // in hide_macos_cursor_if_needed. Reassert only the cached cursor image;
    // repeatedly pushing hide layers made return latency grow with session time.
    if !transparent_now {
        set_macos_cursor_transparent_current();
    }
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
    // CGDisplayMoveCursorToPoint re-shows a hidden pointer (documented side
    // effect), so when we've just hidden the cursor to cross into a remote it
    // would flash back at the anchor and linger until the next repin re-hides
    // it — the "cursor still shows for a beat at the edge" stutter. While the
    // cursor is hidden, warp instead: CGWarpMouseCursorPosition moves it in
    // global coordinates without changing visibility.
    let cursor_hidden = context
        .cursor_hidden
        .lock()
        .map(|hidden| *hidden)
        .unwrap_or(false);
    move_macos_cursor_without_event_on_displays(point, &context.display_snapshots, cursor_hidden);
}

#[cfg(target_os = "macos")]
fn move_macos_cursor_without_event_on_displays(
    point: core_graphics::geometry::CGPoint,
    displays: &[MacDisplaySnapshot],
    keep_hidden: bool,
) {
    use core_graphics::display::CGDisplay;

    if keep_hidden {
        let _ = CGDisplay::warp_mouse_cursor_position(point);
        return;
    }

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

    // Arm once: SetsCursorInBackground is a WindowServer connection property that
    // survives for the life of the connection, so it only needs to be set a
    // single time. Re-setting it on every hide made WindowServer re-evaluate the
    // cursor and briefly repaint it — the visible "cursor lingers at the edge on
    // crossing, then hides a moment later" stutter, worst while frontmost (where
    // the per-frame reassert that would otherwise mask it is skipped).
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
#[derive(Debug, Clone, Copy)]
struct MacClickDown {
    button: MouseButton,
    x: i32,
    y: i32,
    at: Instant,
    count: u8,
}

#[cfg(target_os = "macos")]
#[derive(Debug, Default)]
struct MacClickTracker {
    last_down: Option<MacClickDown>,
    pressed: [Option<MacClickDown>; 3],
}

#[cfg(target_os = "macos")]
impl MacClickTracker {
    const MAX_DISTANCE_PX: i32 = 8;

    fn event_count(
        &mut self,
        button: MouseButton,
        down: bool,
        x: i32,
        y: i32,
        now: Instant,
        double_click_interval: Duration,
    ) -> i64 {
        let index = match button {
            MouseButton::Left => 0,
            MouseButton::Right => 1,
            MouseButton::Middle => 2,
        };

        if down {
            let count = self
                .last_down
                .filter(|last| {
                    last.button == button
                        && now.saturating_duration_since(last.at) <= double_click_interval
                        && click_points_are_near(last.x, last.y, x, y, Self::MAX_DISTANCE_PX)
                })
                .map(|last| last.count.saturating_add(1).min(3))
                .unwrap_or(1);
            let click = MacClickDown {
                button,
                x,
                y,
                at: now,
                count,
            };
            self.last_down = Some(click);
            self.pressed[index] = Some(click);
            return i64::from(count);
        }

        let Some(click) = self.pressed[index].take() else {
            return 0;
        };
        if click_points_are_near(click.x, click.y, x, y, Self::MAX_DISTANCE_PX) {
            i64::from(click.count)
        } else {
            self.last_down = None;
            0
        }
    }
}

#[cfg(target_os = "macos")]
fn click_points_are_near(x1: i32, y1: i32, x2: i32, y2: i32, max_distance: i32) -> bool {
    let dx = i64::from(x1) - i64::from(x2);
    let dy = i64::from(y1) - i64::from(y2);
    let max = i64::from(max_distance);
    dx * dx + dy * dy <= max * max
}

#[cfg(target_os = "macos")]
fn macos_double_click_interval() -> Duration {
    static INTERVAL: OnceLock<Duration> = OnceLock::new();
    *INTERVAL.get_or_init(|| {
        use std::ffi::c_void;
        use std::os::raw::c_char;

        #[link(name = "objc")]
        extern "C" {
            fn objc_getClass(name: *const c_char) -> *mut c_void;
            fn sel_registerName(name: *const c_char) -> *mut c_void;
            fn objc_msgSend();
        }

        let seconds = unsafe {
            let class = objc_getClass(b"NSEvent\0".as_ptr() as *const c_char);
            if class.is_null() {
                0.5
            } else {
                let selector = sel_registerName(b"doubleClickInterval\0".as_ptr() as *const c_char);
                let get_interval: extern "C" fn(*mut c_void, *mut c_void) -> f64 =
                    std::mem::transmute(objc_msgSend as *const ());
                get_interval(class, selector)
            }
        };
        Duration::from_secs_f64(if seconds.is_finite() && (0.1..=2.0).contains(&seconds) {
            seconds
        } else {
            0.5
        })
    })
}

#[cfg(target_os = "macos")]
fn macos_click_state(button: MouseButton, down: bool, x: i32, y: i32) -> i64 {
    macos_click_tracker()
        .lock()
        .map(|mut tracker| {
            tracker.event_count(
                button,
                down,
                x,
                y,
                Instant::now(),
                macos_double_click_interval(),
            )
        })
        .unwrap_or(if down { 1 } else { 0 })
}

#[cfg(target_os = "macos")]
fn macos_click_tracker() -> &'static Mutex<MacClickTracker> {
    static TRACKER: OnceLock<Mutex<MacClickTracker>> = OnceLock::new();
    TRACKER.get_or_init(|| Mutex::new(MacClickTracker::default()))
}

#[cfg(target_os = "macos")]
fn inject_mouse_button(button: MouseButton, down: bool, x: i32, y: i32) {
    use core_graphics::{
        display::CGDisplay,
        event::{CGEvent, CGEventTapLocation, CGEventType, CGMouseButton, EventField},
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
        event.set_integer_value_field(
            EventField::MOUSE_EVENT_CLICK_STATE,
            macos_click_state(button, down, x, y),
        );
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

/// Held keys and modifier flags for injected macOS events. Posting a bare
/// modifier *keycode* does not make the window server apply that modifier to the
/// key events posted after it, so capitals, shifted symbols and every shortcut
/// (including the Ctrl<->Cmd remap) silently failed. Tracking every key also lets
/// an origin/park/runtime reset post the missing Up for an ordinary key after a
/// controller disappears mid-press.
#[cfg(target_os = "macos")]
static MAC_INJECT_FLAGS: AtomicU64 = AtomicU64::new(0);
#[cfg(target_os = "macos")]
static MAC_INJECT_KEY_LOCK: Mutex<()> = Mutex::new(());

#[cfg(target_os = "macos")]
#[derive(Debug, Default)]
struct MacInjectedKeyState {
    pressed_keys: Vec<u16>,
}

#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MacInjectedKeyTransition {
    tracked_flags: u64,
    device_flags: u32,
    should_post: bool,
}

#[cfg(target_os = "macos")]
impl MacInjectedKeyState {
    fn transition(
        &mut self,
        key_code: u16,
        down: bool,
        is_modifier: bool,
    ) -> MacInjectedKeyTransition {
        let tracked_key = if is_modifier {
            normalize_macos_injected_modifier_vk(key_code)
        } else {
            key_code
        };
        let tracked_key = if is_modifier && !down && !self.pressed_keys.contains(&tracked_key) {
            let family = modifier_mask_for_key(tracked_key);
            let mut held_family = self
                .pressed_keys
                .iter()
                .copied()
                .filter(|pressed| modifier_mask_for_key(*pressed) == family);
            match (held_family.next(), held_family.next()) {
                // A snapshot may synthesize the canonical left key while the
                // real Up later arrives as the right-side VK. If there is one
                // unambiguous held member, release that family member.
                (Some(held), None) => held,
                _ => tracked_key,
            }
        } else {
            tracked_key
        };
        let already_pressed = self.pressed_keys.contains(&tracked_key);
        if down && !already_pressed {
            self.pressed_keys.push(tracked_key);
        } else if !down && already_pressed {
            self.pressed_keys.retain(|pressed| *pressed != tracked_key);
        }

        let tracked_flags = self
            .pressed_keys
            .iter()
            .filter_map(|pressed| windows_vk_to_mac_flag(*pressed))
            .fold(0, |flags, flag| flags | flag);
        let device_flags = self
            .pressed_keys
            .iter()
            .filter_map(|pressed| windows_vk_to_mac_device_flag(*pressed))
            .fold(0, |flags, flag| flags | flag);
        MacInjectedKeyTransition {
            tracked_flags,
            device_flags,
            // Ordinary repeated KeyDown events carry native key repeat and must
            // still be posted. A modifier Up is also always posted: it is an
            // idempotent repair when the matching Down was tracked under a
            // generic/sided alias or WindowServer retained stale global flags.
            should_post: !is_modifier || !down || !already_pressed,
        }
    }

    fn pressed_keys(&self) -> &[u16] {
        &self.pressed_keys
    }
}

#[cfg(target_os = "macos")]
fn normalize_macos_injected_modifier_vk(key_code: u16) -> u16 {
    match key_code {
        0x10 => 0xA0,
        0x11 => 0xA2,
        0x12 => 0xA4,
        _ => key_code,
    }
}

#[cfg(target_os = "macos")]
fn macos_injected_key_state() -> &'static Mutex<MacInjectedKeyState> {
    static STATE: OnceLock<Mutex<MacInjectedKeyState>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(MacInjectedKeyState::default()))
}

/// Releases and clears every tracked injected key. Clearing Rust bookkeeping
/// alone is insufficient: WindowServer keeps a synthetic key latched until it
/// receives the matching key-up event.
#[cfg(target_os = "macos")]
fn reset_injected_keys() {
    let _inject_guard = MAC_INJECT_KEY_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let pressed = macos_injected_key_state()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .pressed_keys()
        .to_vec();
    // Release in reverse press order: for Ctrl+A this posts A-up while Control
    // is still held, then the final Control-up, matching a physical keyboard.
    for key_code in pressed.into_iter().rev() {
        inject_key_inner(key_code, false);
    }
    MAC_INJECT_FLAGS.store(0, Ordering::Relaxed);
    *macos_injected_key_state()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner()) = MacInjectedKeyState::default();
}

#[cfg(target_os = "macos")]
pub fn reset_injected_modifiers() {
    reset_injected_keys();
    *macos_click_tracker()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner()) = MacClickTracker::default();
}

/// Switch to the next enabled keyboard input source, replicating the user's
/// "Caps Lock switches to a different input source" setting. Injecting a caps
/// keycode does not trigger this on modern macOS (the OS only reacts to the
/// physical key), so we drive the Text Input Sources API directly.
///
/// TIS asserts it runs on the main dispatch queue, but injection happens on a
/// QUIC worker thread — calling it there traps (SIGTRAP). Hop to the main thread.
#[cfg(target_os = "macos")]
fn macos_switch_to_next_input_source() {
    use std::os::raw::c_void;
    extern "C" {
        fn dispatch_async_f(
            queue: *const c_void,
            context: *mut c_void,
            work: extern "C" fn(*mut c_void),
        );
        static _dispatch_main_q: c_void;
    }
    unsafe {
        dispatch_async_f(
            &_dispatch_main_q as *const c_void,
            std::ptr::null_mut(),
            macos_switch_input_source_thunk,
        );
    }
}

#[cfg(target_os = "macos")]
extern "C" fn macos_switch_input_source_thunk(_: *mut std::os::raw::c_void) {
    macos_do_switch_input_source();
}

#[cfg(target_os = "macos")]
fn macos_do_switch_input_source() {
    use std::os::raw::c_void;

    #[link(name = "Carbon", kind = "framework")]
    extern "C" {
        fn TISCreateInputSourceList(properties: *const c_void, include_all: bool) -> *const c_void;
        fn TISCopyCurrentKeyboardInputSource() -> *const c_void;
        fn TISSelectInputSource(source: *const c_void) -> i32;
        fn TISGetInputSourceProperty(source: *const c_void, key: *const c_void) -> *const c_void;
        static kTISPropertyInputSourceCategory: *const c_void;
        static kTISCategoryKeyboardInputSource: *const c_void;
        static kTISPropertyInputSourceIsASCIICapable: *const c_void;
        static kTISPropertyInputSourceIsSelectCapable: *const c_void;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFArrayGetCount(array: *const c_void) -> isize;
        fn CFArrayGetValueAtIndex(array: *const c_void, index: isize) -> *const c_void;
        fn CFEqual(a: *const c_void, b: *const c_void) -> u8;
        fn CFBooleanGetValue(boolean: *const c_void) -> u8;
        fn CFRelease(cf: *const c_void);
    }

    // TISGetInputSourceProperty(...) returns a borrowed CFBooleanRef; true iff set.
    let prop_true = |src: *const c_void, key: *const c_void| unsafe {
        let value = TISGetInputSourceProperty(src, key);
        !value.is_null() && CFBooleanGetValue(value) != 0
    };
    let is_keyboard = |src: *const c_void| unsafe {
        let category = TISGetInputSourceProperty(src, kTISPropertyInputSourceCategory);
        !category.is_null() && CFEqual(category, kTISCategoryKeyboardInputSource) != 0
    };

    unsafe {
        let list = TISCreateInputSourceList(std::ptr::null(), false);
        if list.is_null() {
            return;
        }
        let count = CFArrayGetCount(list);
        let current = TISCopyCurrentKeyboardInputSource();
        if current.is_null() {
            CFRelease(list);
            return;
        }

        // Toggle like a physical Caps Lock: pick the first *selectable* keyboard
        // source whose ASCII-ness is the opposite of the current one — English
        // (ASCII) <-> the CJK source — regardless of how many layouts are enabled.
        let current_ascii = prop_true(current, kTISPropertyInputSourceIsASCIICapable);
        let mut target: *const c_void = std::ptr::null();
        for i in 0..count {
            let src = CFArrayGetValueAtIndex(list, i);
            if src.is_null()
                || CFEqual(src, current) != 0
                || !is_keyboard(src)
                || !prop_true(src, kTISPropertyInputSourceIsSelectCapable)
            {
                continue;
            }
            if prop_true(src, kTISPropertyInputSourceIsASCIICapable) != current_ascii {
                target = src;
                break;
            }
        }

        if target.is_null() {
            log::info!("[diag] caps: no opposite-ASCII selectable keyboard (current_ascii={current_ascii}, count={count})");
        } else {
            let result = TISSelectInputSource(target);
            log::info!(
                "[diag] caps: toggled input source (was_ascii={current_ascii}, TISSelect={result})"
            );
        }

        CFRelease(current);
        CFRelease(list);
    }
}

#[cfg(target_os = "windows")]
fn reset_injected_keys() {
    release_windows_injected_keys_both_routes();
}

#[cfg(target_os = "windows")]
pub fn reset_injected_modifiers() {
    let mut route_state = windows_input_route_state()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    release_windows_injected_inputs_both_routes();
    route_state.clear();
}

#[cfg(target_os = "linux")]
fn reset_injected_keys() {
    linux_input::reset_injected_keys();
}

#[cfg(target_os = "linux")]
pub fn reset_injected_modifiers() {
    linux_input::reset_injected_inputs();
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
fn reset_injected_keys() {}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
pub fn reset_injected_modifiers() {}

fn reconcile_non_key_injected_modifier_snapshot(mask: Option<u8>) {
    let Some(mask) = mask else {
        return;
    };
    #[cfg(target_os = "macos")]
    reconcile_macos_injected_modifier_snapshot(mask);
    #[cfg(target_os = "windows")]
    reconcile_windows_injected_modifier_snapshot(Some(mask));
    #[cfg(target_os = "linux")]
    linux_input::reconcile_injected_modifier_snapshot(mask);
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    let _ = mask;
}

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

/// Device-dependent bits are required on NX_FLAGSCHANGED events. Without them,
/// WindowServer can show Control in a Quartz event while Mission Control and
/// Spaces still ignore the synthetic modifier.
#[cfg(target_os = "macos")]
fn windows_vk_to_mac_device_flag(vk: u16) -> Option<u32> {
    match vk {
        0x10 | 0xA0 => Some(0x0000_0002), // left Shift
        0xA1 => Some(0x0000_0004),        // right Shift
        0x11 | 0xA2 => Some(0x0000_0001), // left Control
        0xA3 => Some(0x0000_2000),        // right Control
        0x12 | 0xA4 => Some(0x0000_0020), // left Option
        0xA5 => Some(0x0000_0040),        // right Option
        0x5B => Some(0x0000_0008),        // left Command
        0x5C => Some(0x0000_0010),        // right Command
        _ => None,
    }
}

#[cfg(target_os = "macos")]
fn update_macos_injected_key(
    key_code: u16,
    down: bool,
    is_modifier: bool,
) -> MacInjectedKeyTransition {
    let transition = macos_injected_key_state()
        .lock()
        .map(|mut state| state.transition(key_code, down, is_modifier))
        .unwrap_or(MacInjectedKeyTransition {
            tracked_flags: MAC_INJECT_FLAGS.load(Ordering::Relaxed),
            device_flags: 0,
            should_post: true,
        });
    MAC_INJECT_FLAGS.store(transition.tracked_flags, Ordering::Relaxed);
    transition
}

#[cfg(target_os = "macos")]
fn merged_macos_event_flags(intrinsic: u64, tracked_modifiers: u64) -> u64 {
    intrinsic | tracked_modifiers
}

#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MacHidKeyEventPlan {
    event_type: u32,
    key_code: u16,
    event_flags: u32,
    options: u32,
}

#[cfg(target_os = "macos")]
fn macos_hid_key_event_plan(
    key_code: u16,
    down: bool,
    is_modifier: bool,
    tracked_flags: u64,
    device_flags: u32,
) -> MacHidKeyEventPlan {
    if is_modifier {
        MacHidKeyEventPlan {
            event_type: 12, // NX_FLAGSCHANGED
            key_code,
            event_flags: tracked_flags as u32 | device_flags,
            options: 1, // kIOHIDSetGlobalEventFlags
        }
    } else {
        MacHidKeyEventPlan {
            event_type: if down { 10 } else { 11 }, // NX_KEYDOWN / NX_KEYUP
            key_code,
            // IOHID applies the global state established by FLAGSCHANGED. This
            // mirrors a real keyboard and is what system shortcuts consume.
            event_flags: 0,
            options: 0,
        }
    }
}

#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Debug, Default)]
struct MacNxKeyEventData {
    orig_char_set: u16,
    repeat: i16,
    char_set: u16,
    char_code: u16,
    key_code: u16,
    orig_char_code: u16,
    reserved1: i32,
    keyboard_type: u32,
    reserved2: i32,
    reserved3: i32,
    reserved4: i32,
    reserved5: [i32; 4],
}

#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
struct MacIoGPoint {
    x: i16,
    y: i16,
}

#[cfg(target_os = "macos")]
#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOServiceMatching(name: *const std::os::raw::c_char) -> *mut std::ffi::c_void;
    fn IOServiceGetMatchingService(main_port: u32, matching: *mut std::ffi::c_void) -> u32;
    fn IOServiceOpen(service: u32, owning_task: u32, kind: u32, connect: *mut u32) -> i32;
    fn IOObjectRelease(object: u32) -> i32;
    fn IOHIDPostEvent(
        connect: u32,
        event_type: u32,
        location: MacIoGPoint,
        event_data: *const MacNxKeyEventData,
        event_data_version: u32,
        event_flags: u32,
        options: u32,
    ) -> i32;
}

#[cfg(target_os = "macos")]
extern "C" {
    static mach_task_self_: u32;
}

#[cfg(target_os = "macos")]
fn macos_iohid_connection() -> Option<u32> {
    static CONNECTION: OnceLock<Option<u32>> = OnceLock::new();
    *CONNECTION.get_or_init(|| unsafe {
        let matching = IOServiceMatching(c"IOHIDSystem".as_ptr());
        if matching.is_null() {
            return None;
        }
        // kIOMainPortDefault is the null Mach port. The matching dictionary is
        // consumed by IOServiceGetMatchingService.
        let service = IOServiceGetMatchingService(0, matching);
        if service == 0 {
            return None;
        }
        let mut connection = 0;
        let result = IOServiceOpen(service, mach_task_self_, 1, &mut connection);
        let _ = IOObjectRelease(service);
        (result == 0 && connection != 0).then_some(connection)
    })
}

#[cfg(target_os = "macos")]
fn post_macos_hid_key_event(plan: MacHidKeyEventPlan) -> bool {
    let Some(connection) = macos_iohid_connection() else {
        return false;
    };
    let event = MacNxKeyEventData {
        key_code: plan.key_code,
        ..MacNxKeyEventData::default()
    };
    unsafe {
        IOHIDPostEvent(
            connection,
            plan.event_type,
            MacIoGPoint::default(),
            &event,
            2, // kNXEventDataVersion
            plan.event_flags,
            plan.options,
        ) == 0
    }
}

#[cfg(target_os = "macos")]
fn inject_key(key_code: u16, down: bool) {
    inject_macos_key_with_modifier_snapshot(key_code, down, None);
}

#[cfg(target_os = "macos")]
fn inject_macos_key_with_modifier_snapshot(key_code: u16, down: bool, modifier_mask: Option<u8>) {
    let _inject_guard = MAC_INJECT_KEY_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if modifier_mask_for_key(key_code).is_some() {
        inject_key_inner(key_code, down);
        if let Some(mask) = modifier_mask {
            reconcile_macos_injected_modifier_snapshot_inner(mask);
        }
        return;
    }
    if let Some(mask) = modifier_mask {
        reconcile_macos_injected_modifier_snapshot_inner(mask);
    }
    inject_key_inner(key_code, down);
}

#[cfg(target_os = "macos")]
fn reconcile_macos_injected_modifier_snapshot_inner(mask: u8) {
    let pressed = macos_injected_key_state()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .pressed_keys()
        .to_vec();
    let repairs = modifier_snapshot_transitions(&pressed, mask);
    if !repairs.is_empty() {
        log::info!("reconciled remote macOS modifiers from snapshot mask={mask:#04x}: {repairs:?}");
    }
    for (modifier, modifier_down) in repairs {
        inject_key_inner(modifier, modifier_down);
    }
}

#[cfg(target_os = "macos")]
fn reconcile_macos_injected_modifier_snapshot(mask: u8) {
    let _inject_guard = MAC_INJECT_KEY_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    reconcile_macos_injected_modifier_snapshot_inner(mask);
}

#[cfg(target_os = "macos")]
fn inject_key_inner(key_code: u16, down: bool) {
    use core_graphics::{
        event::{CGEvent, CGEventFlags, CGEventTapLocation},
        event_source::{CGEventSource, CGEventSourceStateID},
    };

    // Caps Lock: neither injecting keycode 57 nor IOKit's IOHIDSetModifierLockState
    // works on modern macOS (the OS only reacts to the physical key / needs
    // privileges), so drive the user's "switch input source" behaviour directly.
    const VK_CAPITAL: u16 = 0x14;
    if key_code == VK_CAPITAL {
        if down {
            macos_switch_to_next_input_source();
        }
        return;
    }

    let Some(mac_code) = windows_vk_to_mac_key(key_code) else {
        log::debug!("inject_key: no mac keycode for windows vk {key_code:#04x}; dropping");
        return;
    };
    let is_modifier = windows_vk_to_mac_flag(key_code).is_some();
    // Track ordinary keys as well as modifiers so ReleaseAll can repair a lost
    // KeyUp. Repeated ordinary KeyDowns are still posted for native key repeat;
    // duplicate modifier transitions are suppressed.
    let transition = update_macos_injected_key(key_code, down, is_modifier);
    if !transition.should_post {
        return;
    }
    let tracked_flags = transition.tracked_flags;
    let fallback_flags = tracked_flags | u64::from(transition.device_flags);
    log::debug!(
        "[diag] inject key vk={key_code:#04x} down={down} mac={mac_code} flags={:#x}",
        tracked_flags
    );
    let hid_plan = macos_hid_key_event_plan(
        mac_code,
        down,
        is_modifier,
        tracked_flags,
        transition.device_flags,
    );
    if post_macos_hid_key_event(hid_plan) {
        return;
    }
    static HID_FALLBACK_LOGGED: AtomicBool = AtomicBool::new(false);
    if !HID_FALLBACK_LOGGED.swap(true, Ordering::Relaxed) {
        log::warn!(
            "IOHID keyboard injection unavailable; falling back to CGEvent (system shortcuts may be limited)"
        );
    }
    let Ok(source) = CGEventSource::new(CGEventSourceStateID::HIDSystemState) else {
        log::warn!("inject_key: failed to create CGEventSource");
        return;
    };
    match CGEvent::new_keyboard_event(source, mac_code, down) {
        Ok(event) => {
            // Keyboard constructors add semantic bits for arrows, function
            // keys, and keypad keys. Keep those intrinsic bits and layer the
            // remotely-held modifiers on top instead of erasing them.
            let flags = merged_macos_event_flags(event.get_flags().bits(), fallback_flags);
            event.set_flags(CGEventFlags::from_bits_retain(flags));
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

#[cfg(target_os = "linux")]
fn inject_mouse_move(x: i32, y: i32, _drag_button: Option<MouseButton>) {
    linux_input::inject_mouse_move(x, y);
}

#[cfg(target_os = "linux")]
fn inject_mouse_button(button: MouseButton, down: bool, x: i32, y: i32) {
    linux_input::inject_mouse_button(button, down, x, y);
}

#[cfg(target_os = "linux")]
fn inject_scroll(delta_x: i32, delta_y: i32) {
    linux_input::inject_scroll(delta_x, delta_y);
}

#[cfg(target_os = "linux")]
fn inject_key(key_code: u16, down: bool) {
    linux_input::inject_key(key_code, down);
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
fn inject_mouse_move(_x: i32, _y: i32, _drag_button: Option<MouseButton>) {}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
fn inject_mouse_button(_button: MouseButton, _down: bool, _x: i32, _y: i32) {}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
fn inject_scroll(_delta_x: i32, _delta_y: i32) {}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
fn inject_key(_key_code: u16, _down: bool) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_windows_key_hook_modifier_is_recovered_before_arrow() {
        let recovered = reconcile_windows_modifier_events(&[0x5B], &[]);
        assert_eq!(
            recovered,
            vec![InputEvent::Key {
                key_code: 0x5B,
                down: true,
            }]
        );

        let map = crate::default_modifier_map();
        let wire = recovered
            .into_iter()
            .chain([
                InputEvent::Key {
                    key_code: 0x25,
                    down: true,
                },
                InputEvent::Key {
                    key_code: 0x25,
                    down: false,
                },
                InputEvent::Key {
                    key_code: 0x5B,
                    down: false,
                },
            ])
            .map(|event| match event {
                InputEvent::Key { key_code, down } => InputEvent::Key {
                    key_code: remap_modifier_vk(key_code, &map.control, &map.alt, &map.meta),
                    down,
                },
                event => event,
            })
            .collect::<Vec<_>>();

        assert_eq!(
            wire,
            vec![
                InputEvent::Key {
                    key_code: 0x11,
                    down: true,
                },
                InputEvent::Key {
                    key_code: 0x25,
                    down: true,
                },
                InputEvent::Key {
                    key_code: 0x25,
                    down: false,
                },
                InputEvent::Key {
                    key_code: 0x11,
                    down: false,
                },
            ]
        );
    }

    #[test]
    fn stale_forwarded_windows_modifier_is_released_before_next_key() {
        assert_eq!(
            reconcile_windows_modifier_events(&[], &[0xA2]),
            vec![InputEvent::Key {
                key_code: 0xA2,
                down: false,
            }]
        );
    }

    #[test]
    fn authoritative_empty_modifier_snapshot_releases_stuck_command() {
        assert_eq!(
            crate::shared_input::modifier_snapshot_transitions(&[0x41, 0x5B], 0),
            vec![(0x5B, false)]
        );
    }

    #[test]
    fn authoritative_modifier_snapshot_releases_stale_before_restoring_missing_down() {
        assert_eq!(
            modifier_snapshot_transitions(&[0x41, 0x5B], CONTROL_MODIFIER_MASK),
            vec![(0x5B, false), (0x11, true)]
        );
        assert!(modifier_snapshot_transitions(&[0xA1], SHIFT_MODIFIER_MASK).is_empty());
    }

    #[test]
    fn windows_modifier_tracker_converges_generic_and_sided_events() {
        let mut state = WindowsInjectedKeyState::default();
        state.track(0x5B, true);
        assert_eq!(state.transitions(0), vec![(0x5B, false)]);
        state.track(0x5B, false);
        assert_eq!(state.transitions(CONTROL_MODIFIER_MASK), vec![(0x11, true)]);
        state.track(0x11, true);
        assert!(state.transitions(CONTROL_MODIFIER_MASK).is_empty());
        state.track(0x11, false);
        assert!(state.pressed_keys.is_empty());
    }

    #[test]
    fn windows_key_only_reset_tracks_plain_keys_without_mouse_state() {
        let mut state = WindowsInjectedKeyState::default();
        state.track(0xA2, true);
        state.track(0x41, true);
        assert_eq!(state.take_pressed_keys(), vec![0xA2, 0x41]);
        assert!(state.pressed_keys.is_empty());
    }

    #[test]
    fn key_sequence_rejects_reconnect_duplicates_and_resets_for_new_origin() {
        let mut state = RemoteKeySequenceState::default();
        assert!(state.accept_key("server-a", 0x41, 100));
        assert!(!state.accept_key("server-a", 0x41, 100));
        assert!(!state.accept_key("server-a", 0x41, 99));
        assert!(state.accept_key("server-a", 0x41, 101));
        assert!(state.accept_key("server-b", 0x41, 1));
        assert!(state.accept_key("legacy", 0x41, 0));
    }

    #[test]
    fn key_sequence_boundary_rejects_delayed_keys_without_rejecting_newer_input() {
        let mut state = RemoteKeySequenceState::default();
        assert!(state.accept_key("server-a", 0x41, 10));
        assert!(state.accept_boundary("server-a", 20));
        assert!(!state.accept_key("server-a", 0x42, 15));
        assert!(state.accept_key("server-a", 0x42, 21));
        assert!(!state.accept_boundary("server-a", 20));
    }

    #[test]
    fn key_sequence_high_water_is_kept_per_origin() {
        let mut state = RemoteKeySequenceState::default();
        assert!(state.accept_boundary("server-a", 20));
        assert!(state.accept_key("server-b", 0x41, 5));
        assert!(!state.accept_key("server-a", 0x41, 19));
        assert!(state.accept_key("server-a", 0x41, 21));
    }

    #[test]
    fn key_sequence_is_ordered_per_semantic_key_across_reconnected_streams() {
        let mut state = RemoteKeySequenceState::default();
        assert!(state.accept_key("server-a", 0xA2, 10)); // Ctrl Down
        assert!(state.accept_key("server-a", 0x41, 20)); // unrelated A
        assert!(state.accept_key("server-a", 0x11, 15)); // delayed Ctrl Up
        assert!(state.accept_key("server-a", 0xA2, 30)); // new Ctrl Down
        assert!(!state.accept_key("server-a", 0xA3, 15)); // old Ctrl Up
    }

    #[test]
    fn newer_modifier_snapshot_blocks_delayed_modifier_keys_but_not_plain_keys() {
        let mut state = RemoteKeySequenceState::default();
        assert!(state.accept_key("server-a", 0xA2, 10)); // Ctrl Down
        assert!(state.accept_snapshot("server-a", 30)); // authoritative modifier state

        // These transitions came from an older uni stream and would otherwise
        // undo the already-applied snapshot in either direction.
        assert!(!state.accept_key("server-a", 0xA3, 15));
        assert!(!state.accept_key("server-a", 0xA0, 29));

        // Snapshot ordering applies only to modifier families. Unrelated plain
        // keys retain their independent ordering across reconnected streams.
        assert!(state.accept_key("server-a", 0x41, 20));
        assert!(state.accept_key("server-a", 0xA2, 31));
    }

    #[test]
    fn dropped_key_event_does_not_consume_modifier_snapshot_sequence() {
        let mut sequence_state = RemoteKeySequenceState::default();
        assert!(sequence_state.accept_key("server-a", 0xA2, 30));
        let mut mouse_state = RemoteMouseState::default();
        let mut active_origin = "server-a".to_string();
        let mut stale_ctrl_up = InputEvent::Key {
            key_code: 0x11,
            down: false,
        };
        assert!(admit_remote_input_with_state(
            &mut sequence_state,
            &mut mouse_state,
            &mut active_origin,
            "server-a",
            Some(0),
            15,
            &mut stale_ctrl_up,
        )
        .is_none());
        assert_eq!(sequence_state.by_origin["server-a"].snapshot_sequence, 0);

        let mut delayed_other_key = InputEvent::Key {
            key_code: 0x41,
            down: false,
        };
        let admission = admit_remote_input_with_state(
            &mut sequence_state,
            &mut mouse_state,
            &mut active_origin,
            "server-a",
            Some(0),
            20,
            &mut delayed_other_key,
        )
        .expect("different-key event remains valid");
        assert_eq!(admission.effective_modifier_snapshot, Some(0));
    }

    #[test]
    fn button_up_uses_per_button_sequence_when_modifier_snapshot_is_old() {
        let mut sequence_state = RemoteKeySequenceState::default();
        assert!(sequence_state.accept_snapshot("server-a", 20));
        let mut active_origin = "server-a".to_string();
        let mut mouse_state = RemoteMouseState {
            x: 50,
            y: 60,
            buttons: LEFT_BUTTON_MASK,
            last_origin_id: "server-a".into(),
            sequence_by_origin: HashMap::from([(
                "server-a".into(),
                RemoteMouseSequenceState {
                    last_position_sequence: 10,
                    last_scroll_sequence: 0,
                    last_boundary_sequence: 0,
                    last_button_sequence: [10, 0, 0],
                },
            )]),
        };
        let mut button_up = InputEvent::MouseButton {
            button: MouseButton::Left,
            down: false,
            screen_id: "local-display-1".into(),
            x: Some(50),
            y: Some(60),
            sequence: 11,
        };
        let admission = admit_remote_input_with_state(
            &mut sequence_state,
            &mut mouse_state,
            &mut active_origin,
            "server-a",
            Some(0),
            15,
            &mut button_up,
        )
        .expect("per-button Up must survive an older snapshot sequence");
        assert!(admission.inject_event);
        assert_eq!(admission.effective_modifier_snapshot, None);
    }

    #[test]
    fn only_explicit_enter_events_can_claim_an_empty_origin() {
        assert!(input_event_can_claim_origin(&InputEvent::MouseMove {
            screen_id: String::new(),
            x: 0,
            y: 0,
            drag_button: None,
            button_mask: Some(0),
            sequence: 1,
        }));
        assert!(input_event_can_claim_origin(&InputEvent::Key {
            key_code: 0x41,
            down: true,
        }));
        assert!(input_event_can_claim_origin(&InputEvent::MouseButton {
            button: MouseButton::Left,
            down: true,
            screen_id: String::new(),
            x: None,
            y: None,
            sequence: 1,
        }));
        assert!(!input_event_can_claim_origin(&InputEvent::Key {
            key_code: 0x41,
            down: false,
        }));
        assert!(!input_event_can_claim_origin(&InputEvent::MouseButton {
            button: MouseButton::Left,
            down: false,
            screen_id: String::new(),
            x: None,
            y: None,
            sequence: 1,
        }));
        assert!(!input_event_can_claim_origin(&InputEvent::Scroll {
            delta_x: 0,
            delta_y: 1,
            sequence: 1,
        }));
        assert!(!input_event_can_claim_origin(&InputEvent::CursorPark {
            screen_id: String::new(),
            x: 0,
            y: 0,
            sequence: 1,
        }));

        let mut sequence_state = RemoteKeySequenceState::default();
        let mut mouse_state = RemoteMouseState::default();
        let mut active_origin = String::new();
        let mut key_up = InputEvent::Key {
            key_code: 0x41,
            down: false,
        };
        let cleanup = admit_remote_input_with_state(
            &mut sequence_state,
            &mut mouse_state,
            &mut active_origin,
            "server-a",
            Some(0),
            10,
            &mut key_up,
        )
        .expect("idle KeyUp should advance sequence state idempotently");
        assert!(!cleanup.inject_event);
        assert_eq!(cleanup.effective_modifier_snapshot, None);
        assert!(active_origin.is_empty());

        let mut key_down = InputEvent::Key {
            key_code: 0x41,
            down: true,
        };
        let entry = admit_remote_input_with_state(
            &mut sequence_state,
            &mut mouse_state,
            &mut active_origin,
            "server-a",
            Some(0),
            11,
            &mut key_down,
        )
        .expect("KeyDown should explicitly enter from idle");
        assert!(entry.inject_event);
        assert!(entry.origin_changed);
        assert_eq!(entry.effective_modifier_snapshot, Some(0));
        assert_eq!(active_origin, "server-a");
    }

    #[test]
    fn stale_origin_sequence_is_rejected_before_it_can_replace_the_active_origin() {
        let mut sequence_state = RemoteKeySequenceState::default();
        assert!(sequence_state.accept_key("server-a", 0x41, 100));
        assert!(sequence_state.accept_key("server-b", 0x41, 10));
        let mut active_origin = "server-b".to_string();
        let mut mouse_state = RemoteMouseState::default();
        let mut stale_key = InputEvent::Key {
            key_code: 0x41,
            down: false,
        };

        assert_eq!(
            admit_remote_input_with_state(
                &mut sequence_state,
                &mut mouse_state,
                &mut active_origin,
                "server-a",
                None,
                99,
                &mut stale_key,
            ),
            None
        );
        assert_eq!(active_origin, "server-b");
    }

    #[test]
    fn park_allows_new_origin_move_but_rejects_old_origin_datagram() {
        let mut sequence_state = RemoteKeySequenceState::default();
        let mut mouse_state = RemoteMouseState::default();
        let mut active_origin = "server-a".to_string();
        let mut park = InputEvent::CursorPark {
            screen_id: "local-display-1".into(),
            x: 100,
            y: 100,
            sequence: 100,
        };
        let park_admission = admit_remote_input_with_state(
            &mut sequence_state,
            &mut mouse_state,
            &mut active_origin,
            "server-a",
            None,
            100,
            &mut park,
        )
        .expect("A park should establish the release boundary");
        assert!(park_admission.release_keys);
        assert!(park_admission
            .mouse
            .is_some_and(|mouse| mouse.park_accepted));
        assert!(active_origin.is_empty());

        let mut first_b_move = InputEvent::MouseMove {
            screen_id: "local-display-1".into(),
            x: 1,
            y: 2,
            drag_button: None,
            button_mask: Some(0),
            sequence: 1,
        };
        let b_admission = admit_remote_input_with_state(
            &mut sequence_state,
            &mut mouse_state,
            &mut active_origin,
            "server-b",
            None,
            0,
            &mut first_b_move,
        )
        .expect("B's first sequenced move should claim the released session");
        assert!(b_admission.origin_changed);
        assert_eq!(active_origin, "server-b");
        assert_eq!(mouse_state.last_origin_id, "server-b");

        let mut delayed_a_move = InputEvent::MouseMove {
            screen_id: "local-display-1".into(),
            x: 99,
            y: 99,
            drag_button: None,
            button_mask: Some(0),
            sequence: 99,
        };
        assert_eq!(
            admit_remote_input_with_state(
                &mut sequence_state,
                &mut mouse_state,
                &mut active_origin,
                "server-a",
                None,
                0,
                &mut delayed_a_move,
            ),
            None
        );
        assert_eq!(active_origin, "server-b");
        assert_eq!(mouse_state.last_origin_id, "server-b");
        assert_eq!(
            mouse_state.sequence_by_origin["server-a"].last_boundary_sequence,
            100
        );
        assert_eq!(
            mouse_state.sequence_by_origin["server-b"].last_position_sequence,
            1
        );
    }

    #[test]
    fn accepted_key_boundary_survives_stale_mouse_park_without_ending_new_drag() {
        let mut sequence_state = RemoteKeySequenceState::default();
        let mut active_origin = "server-a".to_string();
        let mut mouse_state = RemoteMouseState {
            x: 200,
            y: 200,
            buttons: LEFT_BUTTON_MASK,
            last_origin_id: "server-a".into(),
            sequence_by_origin: HashMap::from([(
                "server-a".into(),
                RemoteMouseSequenceState {
                    last_position_sequence: 200,
                    last_scroll_sequence: 0,
                    last_boundary_sequence: 0,
                    last_button_sequence: [200, 0, 0],
                },
            )]),
        };
        let mut park = InputEvent::CursorPark {
            screen_id: "local-display-1".into(),
            x: 100,
            y: 100,
            sequence: 100,
        };
        let admission = admit_remote_input_with_state(
            &mut sequence_state,
            &mut mouse_state,
            &mut active_origin,
            "server-a",
            None,
            100,
            &mut park,
        )
        .expect("keyboard boundary should be accepted");
        assert!(!admission.origin_changed);
        assert!(admission.release_keys);
        assert!(!admission.inject_event);

        // A new drag on the mouse channel already overtook the park. Rejecting
        // only its stale coordinates must not erase that new held-button state.
        assert_eq!(mouse_state.buttons, LEFT_BUTTON_MASK);

        let mut continued_drag = InputEvent::MouseMove {
            screen_id: "local-display-1".into(),
            x: 201,
            y: 201,
            drag_button: Some(MouseButton::Left),
            button_mask: Some(LEFT_BUTTON_MASK),
            sequence: 201,
        };
        assert!(prepare_remote_mouse_event(&mut mouse_state, "server-a", &mut continued_drag,).0);
        assert_eq!(
            authoritative_mouse_button_state(&mut mouse_state, "server-a", &continued_drag, true,),
            (None, false)
        );
        assert!(matches!(
            continued_drag,
            InputEvent::MouseMove {
                drag_button: Some(MouseButton::Left),
                ..
            }
        ));
        assert_eq!(mouse_state.buttons, LEFT_BUTTON_MASK);
    }

    #[test]
    fn foreign_key_mouse_and_park_cannot_interrupt_active_drag() {
        let mut sequence_state = RemoteKeySequenceState::default();
        let mut active_origin = "server-b".to_string();
        let mut mouse_state = RemoteMouseState {
            x: 300,
            y: 400,
            buttons: LEFT_BUTTON_MASK,
            last_origin_id: "server-b".into(),
            sequence_by_origin: HashMap::from([(
                "server-b".into(),
                RemoteMouseSequenceState {
                    last_position_sequence: 200,
                    last_scroll_sequence: 0,
                    last_boundary_sequence: 0,
                    last_button_sequence: [200, 0, 0],
                },
            )]),
        };

        for (key_sequence, down) in [(1, true), (2, false)] {
            let mut foreign_key = InputEvent::Key {
                key_code: 0x41,
                down,
            };
            let admission = admit_remote_input_with_state(
                &mut sequence_state,
                &mut mouse_state,
                &mut active_origin,
                "server-a",
                Some(0),
                key_sequence,
                &mut foreign_key,
            )
            .expect("foreign key high-water should advance while the event is dropped");
            assert!(!admission.inject_event);
            assert!(!admission.origin_changed);
            assert!(!admission.release_keys);
        }

        let mut foreign_move = InputEvent::MouseMove {
            screen_id: "local-display-1".into(),
            x: 10,
            y: 20,
            drag_button: None,
            button_mask: Some(0),
            sequence: 10,
        };
        let move_admission = admit_remote_input_with_state(
            &mut sequence_state,
            &mut mouse_state,
            &mut active_origin,
            "server-a",
            None,
            0,
            &mut foreign_move,
        )
        .expect("foreign mouse high-water should advance while the event is dropped");
        assert!(!move_admission.inject_event);
        assert!(!move_admission.origin_changed);

        let mut delayed_park = InputEvent::CursorPark {
            screen_id: "local-display-1".into(),
            x: 100,
            y: 100,
            sequence: 11,
        };
        let park_admission = admit_remote_input_with_state(
            &mut sequence_state,
            &mut mouse_state,
            &mut active_origin,
            "server-a",
            None,
            3,
            &mut delayed_park,
        )
        .expect("foreign park high-water should advance while the event is dropped");
        assert!(!park_admission.inject_event);
        assert!(!park_admission.origin_changed);
        assert!(!park_admission.release_keys);

        assert_eq!(active_origin, "server-b");
        assert_eq!(mouse_state.last_origin_id, "server-b");
        assert_eq!(mouse_state.buttons, LEFT_BUTTON_MASK);
        assert_eq!((mouse_state.x, mouse_state.y), (300, 400));
        assert_eq!(sequence_state.by_origin["server-a"].boundary_sequence, 3);
        assert_eq!(
            mouse_state.sequence_by_origin["server-a"].last_boundary_sequence,
            11
        );
        assert_eq!(
            mouse_state.sequence_by_origin["server-b"].last_position_sequence,
            200
        );
    }

    #[test]
    fn reset_and_modifier_snapshots_share_the_key_sequence_high_water() {
        assert!(input_packet_needs_key_sequence(
            &InputEvent::CursorPark {
                screen_id: "main".into(),
                x: 1,
                y: 2,
                sequence: 1,
            },
            None,
        ));
        assert!(input_packet_needs_key_sequence(
            &InputEvent::MouseButton {
                button: MouseButton::Left,
                down: true,
                screen_id: "main".into(),
                x: Some(1),
                y: Some(2),
                sequence: 2,
            },
            Some(0),
        ));
        assert!(!input_packet_needs_key_sequence(
            &InputEvent::MouseMove {
                screen_id: "main".into(),
                x: 1,
                y: 2,
                drag_button: None,
                button_mask: Some(0),
                sequence: 3,
            },
            None,
        ));
    }

    #[test]
    fn default_modifier_snapshot_remap_swaps_control_and_meta() {
        let map = crate::default_modifier_map();
        assert_eq!(
            remap_modifier_mask(
                CONTROL_MODIFIER_MASK | SHIFT_MODIFIER_MASK,
                &map.control,
                &map.alt,
                &map.meta,
            ),
            META_MODIFIER_MASK | SHIFT_MODIFIER_MASK
        );
        assert_eq!(
            remap_modifier_mask(
                META_MODIFIER_MASK | ALT_MODIFIER_MASK,
                &map.control,
                &map.alt,
                &map.meta,
            ),
            CONTROL_MODIFIER_MASK | ALT_MODIFIER_MASK
        );
    }

    #[test]
    fn windows_modifier_reconciliation_treats_generic_and_sided_keys_as_one_family() {
        assert!(reconcile_windows_modifier_events(&[0xA2], &[0x11]).is_empty());
        assert!(reconcile_windows_modifier_events(&[0x5B], &[0x5B]).is_empty());
    }

    #[test]
    fn windows_modifier_reconciliation_releases_stale_state_before_new_downs() {
        assert_eq!(
            reconcile_windows_modifier_events(&[0x5B], &[0x11, 0x25]),
            vec![
                InputEvent::Key {
                    key_code: 0x11,
                    down: false,
                },
                InputEvent::Key {
                    key_code: 0x5B,
                    down: true,
                },
            ]
        );
    }

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
    fn macos_keyboard_flags_preserve_intrinsic_arrow_identity() {
        const ARROW_INTRINSIC_FLAGS: u64 = 0x20A0_0000;
        const CONTROL_FLAG: u64 = 0x0004_0000;

        let merged = merged_macos_event_flags(ARROW_INTRINSIC_FLAGS, CONTROL_FLAG);
        assert_eq!(merged & ARROW_INTRINSIC_FLAGS, ARROW_INTRINSIC_FLAGS);
        assert_eq!(merged & CONTROL_FLAG, CONTROL_FLAG);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_control_arrow_uses_hid_global_modifier_state() {
        let mut state = MacInjectedKeyState::default();
        let control_down = state.transition(0x11, true, true);
        let left_down = state.transition(0x25, true, false);
        let left_up = state.transition(0x25, false, false);
        let control_up = state.transition(0x11, false, true);

        assert_eq!(
            [
                macos_hid_key_event_plan(
                    59,
                    true,
                    true,
                    control_down.tracked_flags,
                    control_down.device_flags,
                ),
                macos_hid_key_event_plan(
                    123,
                    true,
                    false,
                    left_down.tracked_flags,
                    left_down.device_flags,
                ),
                macos_hid_key_event_plan(
                    123,
                    false,
                    false,
                    left_up.tracked_flags,
                    left_up.device_flags,
                ),
                macos_hid_key_event_plan(
                    59,
                    false,
                    true,
                    control_up.tracked_flags,
                    control_up.device_flags,
                ),
            ],
            [
                MacHidKeyEventPlan {
                    event_type: 12,
                    key_code: 59,
                    event_flags: 0x0004_0001,
                    options: 1,
                },
                MacHidKeyEventPlan {
                    event_type: 10,
                    key_code: 123,
                    event_flags: 0,
                    options: 0,
                },
                MacHidKeyEventPlan {
                    event_type: 11,
                    key_code: 123,
                    event_flags: 0,
                    options: 0,
                },
                MacHidKeyEventPlan {
                    event_type: 12,
                    key_code: 59,
                    event_flags: 0,
                    options: 1,
                },
            ]
        );
        assert!(state.pressed_keys().is_empty());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_hid_plain_key_repeat_remains_a_keydown() {
        let mut state = MacInjectedKeyState::default();
        let first = state.transition(0x41, true, false);
        let repeat = state.transition(0x41, true, false);
        let up = state.transition(0x41, false, false);

        let plans = [
            macos_hid_key_event_plan(0, true, false, first.tracked_flags, first.device_flags),
            macos_hid_key_event_plan(0, true, false, repeat.tracked_flags, repeat.device_flags),
            macos_hid_key_event_plan(0, false, false, up.tracked_flags, up.device_flags),
        ];
        assert_eq!(plans.map(|plan| plan.event_type), [10, 10, 11]);
        assert!(plans
            .iter()
            .all(|plan| plan.event_flags == 0 && plan.options == 0));
        assert!(state.pressed_keys().is_empty());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_iohid_ffi_layout_matches_xcode_sdk() {
        assert_eq!(std::mem::size_of::<MacIoGPoint>(), 4);
        assert_eq!(std::mem::align_of::<MacIoGPoint>(), 2);
        assert_eq!(std::mem::size_of::<MacNxKeyEventData>(), 48);
        assert_eq!(std::mem::align_of::<MacNxKeyEventData>(), 4);

        let data = MacNxKeyEventData::default();
        let base = std::ptr::addr_of!(data) as usize;
        let key_code = std::ptr::addr_of!(data.key_code) as usize;
        assert_eq!(key_code - base, 8);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn duplicate_macos_modifier_down_is_ignored_but_repair_up_is_posted() {
        let mut state = MacInjectedKeyState::default();
        let control = windows_vk_to_mac_flag(0x11).expect("Control flag");
        let transition = state.transition(0xA2, true, true);
        assert!(transition.should_post);
        assert_eq!(transition.tracked_flags, control);
        assert!(!state.transition(0xA2, true, true).should_post);
        assert!(state.transition(0xA3, true, true).should_post);
        assert_eq!(state.transition(0xA2, false, true).tracked_flags, control);
        assert_eq!(state.transition(0xA3, false, true).tracked_flags, 0);
        assert!(state.transition(0xA3, false, true).should_post);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_generic_and_left_modifier_aliases_clear_each_other() {
        let mut state = MacInjectedKeyState::default();
        assert!(state.transition(0x11, true, true).should_post);
        assert_eq!(state.pressed_keys(), &[0xA2]);
        let up = state.transition(0xA2, false, true);
        assert!(up.should_post);
        assert_eq!(up.tracked_flags, 0);
        assert!(state.pressed_keys().is_empty());

        assert!(state.transition(0x12, true, true).should_post);
        assert_eq!(state.pressed_keys(), &[0xA4]);
        assert!(state.transition(0xA4, false, true).should_post);
        assert!(state.pressed_keys().is_empty());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_unambiguous_modifier_family_up_clears_snapshot_synthetic_key() {
        let mut state = MacInjectedKeyState::default();
        assert!(state.transition(0x5B, true, true).should_post);
        assert_eq!(state.pressed_keys(), &[0x5B]);
        let right_command_up = state.transition(0x5C, false, true);
        assert!(right_command_up.should_post);
        assert_eq!(right_command_up.tracked_flags, 0);
        assert!(state.pressed_keys().is_empty());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_injected_key_state_tracks_plain_keys_without_suppressing_repeat() {
        let mut state = MacInjectedKeyState::default();

        let first = state.transition(0x41, true, false);
        let repeat = state.transition(0x41, true, false);

        assert!(first.should_post);
        assert!(
            repeat.should_post,
            "ordinary key repeat must still be posted"
        );
        assert_eq!(state.pressed_keys(), &[0x41]);
        assert!(state.transition(0x41, false, false).should_post);
        assert!(state.pressed_keys().is_empty());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_injected_key_state_deduplicates_modifier_downs_and_snapshots_every_key() {
        let mut state = MacInjectedKeyState::default();

        assert!(state.transition(0xA2, true, true).should_post);
        assert!(!state.transition(0xA2, true, true).should_post);
        assert!(state.transition(0x28, true, false).should_post);
        assert_eq!(state.pressed_keys(), &[0xA2, 0x28]);

        assert!(state.transition(0xA2, false, true).should_post);
        assert!(state.transition(0xA2, false, true).should_post);
        assert_eq!(state.pressed_keys(), &[0x28]);
    }

    #[test]
    fn windows_route_state_requires_release_before_switching_injectors() {
        let mut state = WindowsInputRouteState::default();

        assert!(!state.requires_release_before(WindowsInputRoute::Local));
        state.commit(WindowsInputRoute::Local);
        assert!(!state.requires_release_before(WindowsInputRoute::Local));
        assert!(state.requires_release_before(WindowsInputRoute::Helper));

        state.commit(WindowsInputRoute::Helper);
        assert!(!state.requires_release_before(WindowsInputRoute::Helper));
        assert!(state.requires_release_before(WindowsInputRoute::Local));

        state.clear();
        assert!(!state.requires_release_before(WindowsInputRoute::Local));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_click_tracker_emits_matching_double_click_counts() {
        let mut tracker = MacClickTracker::default();
        let start = Instant::now();
        let interval = Duration::from_millis(500);

        assert_eq!(
            tracker.event_count(MouseButton::Left, true, 100, 200, start, interval),
            1
        );
        assert_eq!(
            tracker.event_count(
                MouseButton::Left,
                false,
                100,
                200,
                start + Duration::from_millis(40),
                interval,
            ),
            1
        );
        assert_eq!(
            tracker.event_count(
                MouseButton::Left,
                true,
                102,
                201,
                start + Duration::from_millis(180),
                interval,
            ),
            2
        );
        assert_eq!(
            tracker.event_count(
                MouseButton::Left,
                false,
                102,
                201,
                start + Duration::from_millis(220),
                interval,
            ),
            2
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_click_tracker_resets_after_timeout_button_change_or_drag() {
        let mut tracker = MacClickTracker::default();
        let start = Instant::now();
        let interval = Duration::from_millis(500);

        assert_eq!(
            tracker.event_count(MouseButton::Left, true, 10, 10, start, interval),
            1
        );
        assert_eq!(
            tracker.event_count(
                MouseButton::Left,
                false,
                30,
                30,
                start + Duration::from_millis(40),
                interval,
            ),
            0,
            "a drag release is not a click"
        );
        assert_eq!(
            tracker.event_count(
                MouseButton::Right,
                true,
                10,
                10,
                start + Duration::from_millis(100),
                interval,
            ),
            1
        );
        assert_eq!(
            tracker.event_count(
                MouseButton::Left,
                true,
                10,
                10,
                start + Duration::from_millis(800),
                interval,
            ),
            1
        );
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
    fn macos_hidden_remote_loop_matches_visible_remote_loop() {
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
        assert_eq!(
            MACOS_HIDDEN_REMOTE_CAPTURE_LOOP_MS,
            MACOS_VISIBLE_REMOTE_CAPTURE_LOOP_MS
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn hidden_macos_window_uses_a_relaxed_cursor_repin_policy() {
        assert_eq!(macos_cursor_repin_policy(true), (1.5, 8));
        assert_eq!(macos_cursor_repin_policy(false), (48.0, 50));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_transparent_cursor_uses_valid_encoded_image_data() {
        let rgba = image::load_from_memory(&MACOS_TRANSPARENT_PNG)
            .expect("transparent cursor PNG must decode")
            .to_rgba8();
        assert_eq!(rgba.dimensions(), (1, 1));
        assert_eq!(rgba.as_raw(), &[0, 0, 0, 0]);
        assert!(macos_transparent_cursor().is_some());
    }

    #[test]
    fn only_active_mouse_input_reveals_a_parked_cursor() {
        assert!(input_command_reveals_parked_cursor(
            &InputCommand::MouseMove {
                x: 10,
                y: 20,
                drag_button: None,
            }
        ));
        assert!(input_command_reveals_parked_cursor(
            &InputCommand::MouseButton {
                button: MouseButton::Left,
                down: true,
                x: 10,
                y: 20,
            }
        ));
        assert!(!input_command_reveals_parked_cursor(
            &InputCommand::MouseButton {
                button: MouseButton::Left,
                down: false,
                x: 10,
                y: 20,
            }
        ));
        assert!(!input_command_reveals_parked_cursor(&InputCommand::Key {
            key_code: 0x11,
            down: false,
        }));
        assert!(!input_command_reveals_parked_cursor(
            &InputCommand::CursorPark { x: 10, y: 20 }
        ));
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
    fn tangential_edge_motion_does_not_return_to_local() {
        let entry = screen(
            "peer-device",
            "peer-device-local-display-1",
            1920,
            0,
            1920,
            1080,
        );

        assert!(
            !exited_entry_edge(Edge::Right, &entry, -1.0, 512.0, -1.0, 12.0),
            "a mostly vertical slide at the shared edge is not return intent"
        );
        assert!(!exited_entry_edge(
            Edge::Right,
            &entry,
            -2.0,
            512.0,
            -2.0,
            3.0
        ));
        assert!(exited_entry_edge(
            Edge::Right,
            &entry,
            -2.0,
            512.0,
            -2.0,
            1.0
        ));
        assert!(exited_entry_edge(
            Edge::Right,
            &entry,
            -1.0,
            512.0,
            -1.0,
            0.0
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
            modifier_snapshot: None,
            key_sequence: 0,
            event: InputEvent::MouseMove {
                screen_id: "display-1".into(),
                x: 320,
                y: 240,
                drag_button: None,
                button_mask: Some(0),
                sequence: 1,
            },
        };
        let payload = rmp_serde::to_vec_named(&packet).expect("encode input packet");
        let decoded = decode_input_packet(&payload).expect("decode input packet");
        assert_eq!(decoded.modifier_snapshot, None);
        assert_eq!(decoded.key_sequence, 0);

        assert_eq!(decoded.protocol, INPUT_PROTOCOL);
        assert_eq!(decoded.target_device_id, "peer-device");
        assert_eq!(decoded.origin_device_id, "local-device");
        assert_eq!(decoded.origin_port, 47833);
        match decoded.event {
            InputEvent::MouseMove {
                screen_id, x, y, ..
            } => {
                assert_eq!(screen_id, "display-1");
                assert_eq!(x, 320);
                assert_eq!(y, 240);
            }
            _ => panic!("decoded the wrong input event"),
        }
    }

    #[test]
    fn input_packet_modifier_fields_are_backward_compatible() {
        #[derive(Debug, Serialize, Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct LegacyInputPacket {
            protocol: String,
            event: InputEvent,
        }

        let legacy_payload = rmp_serde::to_vec_named(&LegacyInputPacket {
            protocol: INPUT_PROTOCOL.into(),
            event: InputEvent::Key {
                key_code: 0x41,
                down: true,
            },
        })
        .expect("encode legacy input packet");
        let decoded = decode_input_packet(&legacy_payload).expect("decode legacy input packet");
        assert_eq!(decoded.modifier_snapshot, None);
        assert_eq!(decoded.key_sequence, 0);

        let new_packet = InputPacket {
            protocol: INPUT_PROTOCOL.into(),
            target_device_id: String::new(),
            origin_device_id: String::new(),
            origin_port: 0,
            origin_transport_public_key: String::new(),
            origin_protocol_version: quic_transport::PROTOCOL_VERSION,
            cluster_id: String::new(),
            pair_secret: String::new(),
            modifier_snapshot: Some(META_MODIFIER_MASK),
            key_sequence: 9,
            event: InputEvent::Key {
                key_code: 0x41,
                down: true,
            },
        };
        let new_payload = rmp_serde::to_vec_named(&new_packet).expect("encode new input packet");
        let new_decoded = decode_input_packet(&new_payload).expect("decode new input packet");
        assert_eq!(new_decoded.modifier_snapshot, Some(META_MODIFIER_MASK));
        assert_eq!(new_decoded.key_sequence, 9);
        let legacy_decoded: LegacyInputPacket =
            rmp_serde::from_slice(&new_payload).expect("legacy decoder ignores new fields");
        assert_eq!(legacy_decoded.protocol, INPUT_PROTOCOL);
        assert_eq!(legacy_decoded.event, new_packet.event);
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
                drag_button: None,
                button_mask: Some(0),
                sequence: 1,
            },
            None,
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
                    drag_button: None,
                    button_mask: Some(0),
                    sequence: 1,
                },
                None,
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
    fn input_packet_context_never_skips_key_remapping_when_layout_is_busy() {
        let layout_state = Arc::new(Mutex::new(layout_for_target_tests()));
        let held_layout = layout_state.lock().expect("hold layout lock");
        let target = target_for_coordinate_tests();
        let layout_state_for_thread = Arc::clone(&layout_state);
        let (tx, rx) = std::sync::mpsc::channel();

        thread::spawn(move || {
            let context = input_packet_context(
                &target,
                InputEvent::Key {
                    key_code: 0x11,
                    down: true,
                },
                Some(CONTROL_MODIFIER_MASK),
                &layout_state_for_thread,
            );
            tx.send((context.event, context.modifier_snapshot))
                .expect("send remapped key");
        });

        thread::sleep(Duration::from_millis(10));
        drop(held_layout);
        let (event, modifier_snapshot) = rx
            .recv_timeout(Duration::from_millis(100))
            .expect("key context should finish after the short layout lock");
        assert_eq!(modifier_snapshot, Some(META_MODIFIER_MASK));

        assert_eq!(
            event,
            InputEvent::Key {
                key_code: 0x5B,
                down: true,
            },
            "Ctrl must not randomly bypass the configured Ctrl-to-Command mapping"
        );
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
            modifier_snapshot: None,
            key_sequence: 0,
            event: InputEvent::MouseMove {
                screen_id: "local-display-1".into(),
                x: 1,
                y: 1,
                drag_button: None,
                button_mask: Some(0),
                sequence: 1,
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
            modifier_snapshot: None,
            key_sequence: 0,
            event: InputEvent::MouseMove {
                screen_id: "local-display-1".into(),
                x: 1,
                y: 1,
                drag_button: None,
                button_mask: Some(0),
                sequence: 1,
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
                drag_button: None,
                button_mask: Some(0),
                sequence: 1,
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
    fn mouse_button_uses_its_reliable_coordinates_not_a_stale_datagram() {
        let layout = layout_for_target_tests();
        let mut native_layout = layout.clone();
        native_layout.devices[0].screens[0].width = 3840;
        native_layout.devices[0].screens[0].height = 2160;
        update_remote_mouse_position(12, 34);

        let command = input_event_to_command(
            &layout,
            &native_layout,
            InputEvent::MouseButton {
                button: MouseButton::Left,
                down: true,
                screen_id: "local-display-1".into(),
                x: Some(960),
                y: Some(540),
                sequence: 1,
            },
        )
        .expect("mouse button should map");

        assert_eq!(
            command,
            InputCommand::MouseButton {
                button: MouseButton::Left,
                down: true,
                x: 1920,
                y: 1080,
            }
        );
        update_remote_mouse_button(MouseButton::Left, false, None);
    }

    #[test]
    fn reliable_input_classes_prioritize_releases_and_keep_drag_latest_wins() {
        let normal_move = InputEvent::MouseMove {
            screen_id: "local-display-1".into(),
            x: 1,
            y: 2,
            drag_button: None,
            button_mask: Some(0),
            sequence: 1,
        };
        let drag_move = InputEvent::MouseMove {
            screen_id: "local-display-1".into(),
            x: 2,
            y: 3,
            drag_button: Some(MouseButton::Left),
            button_mask: Some(LEFT_BUTTON_MASK),
            sequence: 2,
        };
        let button = InputEvent::MouseButton {
            button: MouseButton::Left,
            down: true,
            screen_id: "local-display-1".into(),
            x: Some(1),
            y: Some(2),
            sequence: 3,
        };

        assert_eq!(input_event_reliable_class(&normal_move), None);
        assert_eq!(input_event_reliable_class(&drag_move), None);
        assert_eq!(
            input_event_reliable_class(&button),
            Some(quic_transport::ReliableInputClass::State)
        );

        let button_up = InputEvent::MouseButton {
            button: MouseButton::Left,
            down: false,
            screen_id: "remote-screen".into(),
            x: Some(12),
            y: Some(34),
            sequence: 4,
        };
        assert_eq!(
            input_event_reliable_class(&button_up),
            Some(quic_transport::ReliableInputClass::Release)
        );
        assert_eq!(
            input_event_reliable_class(&InputEvent::CursorPark {
                screen_id: "remote-screen".into(),
                x: 99,
                y: 99,
                sequence: 5,
            }),
            Some(quic_transport::ReliableInputClass::ResetBoundary)
        );
    }

    #[test]
    fn stale_mouse_events_cannot_cross_a_reliable_handoff_boundary() {
        let mut state = RemoteMouseState::default();
        let mut park = InputEvent::CursorPark {
            screen_id: "local-display-1".into(),
            x: 100,
            y: 100,
            sequence: 100,
        };
        assert!(prepare_remote_mouse_event(&mut state, "server-a", &mut park).0);
        assert_eq!(
            authoritative_mouse_button_state(&mut state, "server-a", &park, true),
            (None, true)
        );

        let mut stale_move = InputEvent::MouseMove {
            screen_id: "local-display-1".into(),
            x: 90,
            y: 90,
            drag_button: None,
            button_mask: Some(0),
            sequence: 99,
        };
        assert!(!prepare_remote_mouse_event(&mut state, "server-a", &mut stale_move).0);

        // Park is an authoritative input boundary, so older transitions cannot
        // re-press or re-release a button after control has left.
        let mut stale_button_up = InputEvent::MouseButton {
            button: MouseButton::Left,
            down: false,
            screen_id: "local-display-1".into(),
            x: Some(80),
            y: Some(80),
            sequence: 98,
        };
        assert!(!prepare_remote_mouse_event(&mut state, "server-a", &mut stale_button_up).0);

        let mut stale_button_down = InputEvent::MouseButton {
            button: MouseButton::Left,
            down: true,
            screen_id: "local-display-1".into(),
            x: Some(70),
            y: Some(70),
            sequence: 99,
        };
        assert!(!prepare_remote_mouse_event(&mut state, "server-a", &mut stale_button_down).0);

        let mut other_controller_move = InputEvent::MouseMove {
            screen_id: "local-display-1".into(),
            x: 1,
            y: 2,
            drag_button: None,
            button_mask: Some(0),
            sequence: 1,
        };
        state.buttons = LEFT_BUTTON_MASK;
        state.x = 80;
        state.y = 90;
        let (accepted, carried) =
            prepare_remote_mouse_event(&mut state, "server-b", &mut other_controller_move);
        assert!(accepted);
        assert_eq!(carried, Some((LEFT_BUTTON_MASK, 80, 90)));
        assert_eq!(state.buttons, 0);

        let mut legacy_move = InputEvent::MouseMove {
            screen_id: "local-display-1".into(),
            x: 3,
            y: 4,
            drag_button: None,
            button_mask: None,
            sequence: 0,
        };
        assert!(prepare_remote_mouse_event(&mut state, "legacy", &mut legacy_move).0);
    }

    #[test]
    fn authoritative_mouse_move_repairs_a_lost_button_transition() {
        let mut state = RemoteMouseState {
            buttons: LEFT_BUTTON_MASK,
            last_origin_id: "server-a".into(),
            sequence_by_origin: HashMap::from([(
                "server-a".into(),
                RemoteMouseSequenceState {
                    last_position_sequence: 10,
                    last_scroll_sequence: 0,
                    last_boundary_sequence: 0,
                    last_button_sequence: [10, 0, 0],
                },
            )]),
            x: 50,
            y: 60,
        };
        let mut released_move = InputEvent::MouseMove {
            screen_id: "local-display-1".into(),
            x: 51,
            y: 61,
            drag_button: None,
            button_mask: Some(0),
            sequence: 12,
        };
        assert!(prepare_remote_mouse_event(&mut state, "server-a", &mut released_move).0);
        assert_eq!(
            authoritative_mouse_button_state(&mut state, "server-a", &released_move, true),
            (Some((LEFT_BUTTON_MASK, 0, 50, 60)), false)
        );
        assert_eq!(state.buttons, 0);
        assert_eq!(
            state.sequence_by_origin["server-a"].last_button_sequence[0],
            12
        );

        let mut delayed_up = InputEvent::MouseButton {
            button: MouseButton::Left,
            down: false,
            screen_id: "local-display-1".into(),
            x: Some(50),
            y: Some(60),
            sequence: 11,
        };
        assert!(!prepare_remote_mouse_event(&mut state, "server-a", &mut delayed_up).0);
    }

    #[test]
    fn mouse_motion_does_not_discard_a_reliable_scroll() {
        let mut state = RemoteMouseState::default();
        let mut move_event = InputEvent::MouseMove {
            screen_id: "local-display-1".into(),
            x: 10,
            y: 20,
            drag_button: None,
            button_mask: Some(0),
            sequence: 20,
        };
        assert!(prepare_remote_mouse_event(&mut state, "server-a", &mut move_event).0);

        let mut earlier_scroll = InputEvent::Scroll {
            delta_x: 0,
            delta_y: 1,
            sequence: 19,
        };
        assert!(
            prepare_remote_mouse_event(&mut state, "server-a", &mut earlier_scroll).0,
            "a faster latest-move datagram must not erase a discrete scroll"
        );

        let mut park = InputEvent::CursorPark {
            screen_id: "local-display-1".into(),
            x: 100,
            y: 100,
            sequence: 21,
        };
        assert!(prepare_remote_mouse_event(&mut state, "server-a", &mut park).0);

        let mut stale_scroll = InputEvent::Scroll {
            delta_x: 0,
            delta_y: 1,
            sequence: 18,
        };
        assert!(!prepare_remote_mouse_event(&mut state, "server-a", &mut stale_scroll).0);
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
            push_on_bind: true,
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
                sequence: 1,
            },
            &target,
            &layout,
        );
        assert!(matches!(
            event,
            InputEvent::Scroll {
                delta_x: 1,
                delta_y: -2,
                ..
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
