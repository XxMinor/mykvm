use std::{
    collections::{hash_map::DefaultHasher, HashMap, HashSet},
    env, fs,
    hash::{Hash, Hasher},
    io::{Read, Write},
    net::{Ipv4Addr, SocketAddr, UdpSocket},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex, OnceLock,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, DragDropEvent, Emitter, Manager, Monitor, WebviewUrl, WebviewWindowBuilder,
    WindowEvent, Wry,
};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};

mod clipboard;
mod input;
mod performance;
mod quic_transport;
pub mod shared_input;
#[cfg(target_os = "windows")]
pub mod windows_input;

use clipboard::{ClipboardContent, ClipboardImage};
use performance::PerformanceSample;

const DISCOVERY_PORT: u16 = 47833;
const TRANSPORT_PORT_MIN: u16 = 1024;
const TRANSPORT_PORT_MAX: u16 = 65_535;
// A peer that wanted the discovery port but found it taken drifts upward (see
// `bind_available_udp_port`). We aim discovery traffic at this many consecutive
// ports starting from the configured base, so two peers that landed on different
// ports (e.g. 47833 and 47834) still reach each other.
const DISCOVERY_PORT_SPAN: u16 = 8;
const REPOSITORY_URL: &str = "https://github.com/XxMinor/mykvm";
const RELEASES_URL: &str = "https://github.com/XxMinor/mykvm/releases/latest";
const DISCOVERY_PROTOCOL: &str = "mykvm.discovery.v1";
// UDP discovery is a heartbeat, not the transport itself. Keep peers through
// short announce gaps so online clients do not flicker offline in the UI.
const PEER_TTL_MS: u64 = 90_000;
const MAX_DISCOVERY_PEERS: usize = 128;
const PAIRING_CODE_TTL_MS: u64 = 60_000;
const PAIRING_MAX_ATTEMPTS: u8 = 5;
const CLIPBOARD_PROTOCOL: &str = "mykvm.clipboard.v1";
// After we write clipboard content received from a peer, ignore our own
// clipboard for a short grace window. Reading an image back through the OS
// pasteboard is not always byte-identical to what we wrote (macOS re-encodes
// it), so a pure content-signature check can ping-pong; this window guarantees
// we never echo received content straight back.
const CLIPBOARD_ECHO_GRACE_MS: u64 = 1200;
const CLIPBOARD_POLL_INTERVAL_MS: u64 = 150;
const CLIPBOARD_IDLE_SLEEP_MS: u64 = 25;
const CLIPBOARD_RETRY_INTERVAL_MS: u64 = 2000;
const CLIPBOARD_WRITE_ATTEMPTS: usize = 5;
const CLIPBOARD_WRITE_RETRY_DELAY_MS: u64 = 30;
const FILE_TRANSFER_PROTOCOL: &str = "mykvm.file-transfer.v1";
const FILE_TRANSFER_CHUNK_BYTES: usize = 256 * 1024;
const FILE_TRANSFER_MAX_FILE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const FILE_TRANSFER_DESTINATION_POINTER: &str = "pointer";
const EDGE_DROP_WINDOWS_ENABLED: bool = false;
const EDGE_DROP_LABEL_PREFIX: &str = "mykvm-edge-drop-";
const EDGE_DROP_THICKNESS: i32 = 8;
const FILE_DROP_LANDING_LABEL: &str = "mykvm-file-drop-landing";
const FILE_DROP_LANDING_WIDTH: f64 = 96.0;
const FILE_DROP_LANDING_HEIGHT: f64 = 72.0;
const LOG_MAX_FILE_SIZE_BYTES: u128 = 1024 * 1024;
const AUTOSTART_ARG: &str = "--mykvm-autostart";
const QUIT_EXISTING_ARG: &str = "--mykvm-quit-existing";
const INSTALL_INPUT_SERVICE_ARG: &str = "--install-input-service";
const UNINSTALL_INPUT_SERVICE_ARG: &str = "--uninstall-input-service";
const HELPER_PATH_ARG: &str = "--helper-path";
const RUNTIME_STATE_EVENT: &str = "runtime-state-changed";

#[cfg(target_os = "windows")]
const SINGLE_INSTANCE_MUTEX_NAME: &str = "Local\\MyKVM_SingleInstance";
#[cfg(target_os = "windows")]
const ACTIVATE_INSTANCE_EVENT_NAME: &str = "Local\\MyKVM_ActivateWindow";
#[cfg(target_os = "windows")]
const QUIT_INSTANCE_EVENT_NAME: &str = "Local\\MyKVM_QuitExisting";

static HOSTNAME_CACHE: OnceLock<Option<String>> = OnceLock::new();

#[cfg(target_os = "windows")]
static WINDOWS_FIREWALL_ENSURED: AtomicBool = AtomicBool::new(false);

#[cfg(target_os = "windows")]
static SINGLE_INSTANCE_MUTEX: OnceLock<Mutex<Option<SingleInstanceGuard>>> = OnceLock::new();

#[cfg(target_os = "windows")]
struct SingleInstanceGuard {
    mutex: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(target_os = "windows")]
unsafe impl Send for SingleInstanceGuard {}
#[cfg(target_os = "windows")]
unsafe impl Sync for SingleInstanceGuard {}

#[cfg(target_os = "windows")]
#[derive(Clone, Copy)]
struct SendHandle(windows_sys::Win32::Foundation::HANDLE);

#[cfg(target_os = "windows")]
impl SendHandle {
    fn raw(self) -> windows_sys::Win32::Foundation::HANDLE {
        self.0
    }
}

#[cfg(target_os = "windows")]
unsafe impl Send for SendHandle {}
#[cfg(target_os = "windows")]
unsafe impl Sync for SendHandle {}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Screen {
    id: String,
    device_id: String,
    name: String,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    scale: f64,
    is_primary: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Device {
    id: String,
    name: String,
    platform: String,
    host: String,
    #[serde(default = "default_transport_port")]
    transport_port: u16,
    #[serde(default)]
    quic_port: u16,
    #[serde(default)]
    transport_public_key: String,
    #[serde(default = "default_protocol_version")]
    protocol_version: u16,
    color: String,
    online: bool,
    #[serde(default)]
    input_ready: bool,
    #[serde(default)]
    upgrading: bool,
    #[serde(default, skip_serializing)]
    upgrading_until_ms: u64,
    role: String,
    #[serde(default = "default_device_source")]
    source: String,
    screens: Vec<Screen>,
}

/// Per-direction hotkeys for jumping between adjacent screens without moving
/// the mouse to an edge. Each value is a canonical shortcut string
/// (`"alt+right"`, `"disabled"`, etc.) consumed by the global-shortcut plugin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScreenSwitchHotkeys {
    #[serde(default = "default_screen_switch_hotkey_left")]
    pub left: String,
    #[serde(default = "default_screen_switch_hotkey_right")]
    pub right: String,
    #[serde(default = "default_screen_switch_hotkey_up")]
    pub up: String,
    #[serde(default = "default_screen_switch_hotkey_down")]
    pub down: String,
}

impl Default for ScreenSwitchHotkeys {
    fn default() -> Self {
        Self {
            left: default_screen_switch_hotkey_left(),
            right: default_screen_switch_hotkey_right(),
            up: default_screen_switch_hotkey_up(),
            down: default_screen_switch_hotkey_down(),
        }
    }
}

fn default_screen_switch_hotkey_left() -> String {
    "alt+left".into()
}
fn default_screen_switch_hotkey_right() -> String {
    "alt+right".into()
}
fn default_screen_switch_hotkey_up() -> String {
    "alt+up".into()
}
fn default_screen_switch_hotkey_down() -> String {
    "alt+down".into()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LayoutState {
    devices: Vec<Device>,
    active_device_id: String,
    selected_screen_id: String,
    #[serde(default = "default_input_mode")]
    input_mode: String,
    #[serde(default = "default_machine_role")]
    machine_role: String,
    #[serde(default = "default_cluster_id")]
    cluster_id: String,
    #[serde(default = "default_pair_secret")]
    pair_secret: String,
    #[serde(default)]
    paired_controllers: Vec<PairedController>,
    #[serde(default = "default_clipboard_sync")]
    clipboard_sync: bool,
    #[serde(default = "default_file_transfer_enabled")]
    file_transfer_enabled: bool,
    #[serde(default = "default_language")]
    language: String,
    #[serde(default = "default_theme_mode")]
    theme_mode: String,
    #[serde(default = "default_performance_monitor")]
    performance_monitor: bool,
    #[serde(default = "default_transport_port_mode")]
    transport_port_mode: String,
    #[serde(default = "default_transport_port")]
    transport_port: u16,
    #[serde(default)]
    quic_port: u16,
    #[serde(default = "default_modifier_remap")]
    modifier_remap: bool,
    #[serde(default = "default_modifier_map")]
    modifier_map: ModifierMap,
    #[serde(default = "default_edge_switch_hotkey")]
    edge_switch_hotkey: String,
    #[serde(default)]
    screen_switch_hotkeys: ScreenSwitchHotkeys,
}

/// Cross-platform modifier remapping. Each field names the *logical* modifier
/// the source key should become on the remote when the two machines run
/// different operating systems. Values: "control" | "alt" | "meta" | "same".
/// Default swaps the primary shortcut modifier so Ctrl (Windows) and
/// Command (macOS) line up, e.g. Ctrl+C on Windows becomes Cmd+C on macOS.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModifierMap {
    #[serde(default = "default_modifier_control")]
    control: String,
    #[serde(default = "default_modifier_alt")]
    alt: String,
    #[serde(default = "default_modifier_meta")]
    meta: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PairedController {
    id: String,
    name: String,
    host: String,
    ip: String,
    transport_public_key: String,
    #[serde(default = "default_protocol_version")]
    protocol_version: u16,
    cluster_id: String,
    paired_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct NativeStageStatus {
    state: String,
    detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LanPeer {
    id: String,
    name: String,
    platform: String,
    #[serde(default)]
    machine_role: String,
    #[serde(default)]
    cluster_id: String,
    #[serde(default)]
    pairing_required: bool,
    host: String,
    ip: String,
    #[serde(default = "default_transport_port")]
    transport_port: u16,
    #[serde(default)]
    quic_port: u16,
    #[serde(default)]
    transport_public_key: String,
    #[serde(default = "default_protocol_version")]
    protocol_version: u16,
    screen_count: usize,
    #[serde(default)]
    input_ready: bool,
    #[serde(default)]
    upgrading: bool,
    #[serde(default)]
    screens: Vec<LanPeerScreen>,
    app_version: String,
    last_seen_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LanPeerScreen {
    id: String,
    name: String,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    scale: f64,
    is_primary: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DiscoveryStatus {
    state: String,
    detail: String,
    port: u16,
    local_peer: LanPeer,
    peers: Vec<LanPeer>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PairingStatus {
    state: String,
    code: String,
    requester_name: String,
    requester_ip: String,
    expires_at_ms: u64,
    detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeStatus {
    started: bool,
    transport: NativeStageStatus,
    capture: NativeStageStatus,
    inject: NativeStageStatus,
    clipboard: NativeStageStatus,
    discovery: DiscoveryStatus,
    pairing: PairingStatus,
    privilege: PrivilegeStatus,
    input_service: InputServiceStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AppStateSnapshot {
    layout: LayoutState,
    runtime: RuntimeStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DiagnosticInfo {
    report: String,
    app_version: String,
    platform: String,
    role: String,
    runtime_started: bool,
    local_name: String,
    local_ip: String,
    discovery_port: u16,
    quic_port: u16,
    peer_count: usize,
    known_devices: Vec<DiagnosticDevice>,
    log_dir: String,
    config_dir: String,
    network_hint: String,
    firewall_hint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DiagnosticDevice {
    name: String,
    host: String,
    role: String,
    online: bool,
    input_ready: bool,
    discovery_port: u16,
    quic_port: u16,
    same_subnet: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PrivilegeStatus {
    is_elevated: bool,
    can_elevate: bool,
    detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InputServiceStatus {
    installed: bool,
    running: bool,
    worker_session_id: Option<u32>,
    pipe_available: bool,
    sas_available: bool,
    detail: String,
}

struct PairingChallenge {
    code: String,
    requester_id: String,
    requester_name: String,
    requester_ip: String,
    requester_host: String,
    requester_public_key: String,
    requester_protocol_version: u16,
    expires_at: Instant,
    expires_at_ms: u64,
    attempts: u8,
}

#[derive(Debug, Clone)]
struct FileTransferTarget {
    device_id: String,
    name: String,
    addr: String,
    transport_public_key: String,
    protocol_version: u16,
    cluster_id: String,
    pair_secret: String,
}

#[derive(Debug, Clone)]
struct TransferFile {
    path: PathBuf,
    name: String,
    total_bytes: u64,
}

#[derive(Debug)]
struct IncomingFileTransfer {
    origin_id: String,
    target_id: String,
    file_name: String,
    total_bytes: u64,
    received_bytes: u64,
    next_chunk_index: u64,
    temp_path: PathBuf,
    final_path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileTransferSummary {
    target_name: String,
    file_count: usize,
    byte_count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileTransferPacket {
    protocol: String,
    kind: String,
    transfer_id: String,
    origin_id: String,
    target_id: String,
    cluster_id: String,
    pair_secret: String,
    file_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    destination_hint: Option<String>,
    total_bytes: u64,
    chunk_index: u64,
    offset: u64,
    #[serde(default)]
    data: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum EdgeDropSide {
    Left,
    Right,
    Top,
    Bottom,
}

impl EdgeDropSide {
    fn as_str(self) -> &'static str {
        match self {
            EdgeDropSide::Left => "left",
            EdgeDropSide::Right => "right",
            EdgeDropSide::Top => "top",
            EdgeDropSide::Bottom => "bottom",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct EdgeDropWindowSpec {
    label: String,
    target_device_id: String,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

struct AppRuntime {
    app_handle: AppHandle,
    layout: Arc<Mutex<LayoutState>>,
    native_layout: Mutex<LayoutState>,
    runtime: Mutex<RuntimeStatus>,
    peers: Arc<Mutex<Vec<LanPeer>>>,
    pairing_challenge: Arc<Mutex<Option<PairingChallenge>>>,
    file_transfers: Arc<Mutex<HashMap<String, IncomingFileTransfer>>>,
    edge_drop_targets: Arc<Mutex<HashMap<String, String>>>,
    quic_transport: Mutex<Option<quic_transport::TransportHandle>>,
    discovery_stop: Mutex<Option<Arc<AtomicBool>>>,
    input_stop: Mutex<Option<Arc<AtomicBool>>>,
    clipboard_stop: Mutex<Option<Arc<AtomicBool>>>,
    clipboard_seen_text: Arc<Mutex<Option<String>>>,
    clipboard_echo_until: Arc<Mutex<Option<Instant>>>,
    clipboard_last_sequences: Arc<Mutex<HashMap<String, u64>>>,
    remote_input_active: Arc<AtomicBool>,
    main_window_visible: Arc<AtomicBool>,
    main_window_focused: Arc<AtomicBool>,
    allow_explicit_quit: Arc<AtomicBool>,
    clipboard_target: Arc<Mutex<Option<input::ClipboardTarget>>>,
    input_receive_enabled: Arc<AtomicBool>,
    upgrading: Arc<AtomicBool>,
    clipboard_receive_enabled: Arc<AtomicBool>,
    transport_packets: Arc<AtomicU64>,
    input_events: Arc<AtomicU64>,
    clipboard_packets: Arc<AtomicU64>,
    runtime_toggle_shortcut: Mutex<Option<String>>,
    runtime_toggle_menu_item: Mutex<Option<MenuItem<Wry>>>,
    screen_switch_request: Arc<Mutex<Option<input::SwitchDirection>>>,
    screen_switch_shortcuts: Mutex<ScreenSwitchHotkeys>,
    config_path: PathBuf,
}

impl AppRuntime {
    fn new(app_handle: AppHandle, config_path: PathBuf, detected_layout: LayoutState) -> Self {
        let layout = load_layout_from_disk(&config_path)
            .map(|saved_layout| normalize_saved_layout(saved_layout, detected_layout.clone()))
            .unwrap_or_else(|| detected_layout.clone());
        Self {
            app_handle,
            layout: Arc::new(Mutex::new(layout)),
            native_layout: Mutex::new(detected_layout.clone()),
            runtime: Mutex::new(default_runtime(&detected_layout)),
            peers: Arc::new(Mutex::new(Vec::new())),
            pairing_challenge: Arc::new(Mutex::new(None)),
            file_transfers: Arc::new(Mutex::new(HashMap::new())),
            edge_drop_targets: Arc::new(Mutex::new(HashMap::new())),
            quic_transport: Mutex::new(None),
            discovery_stop: Mutex::new(None),
            input_stop: Mutex::new(None),
            clipboard_stop: Mutex::new(None),
            clipboard_seen_text: Arc::new(Mutex::new(None)),
            clipboard_echo_until: Arc::new(Mutex::new(None)),
            clipboard_last_sequences: Arc::new(Mutex::new(HashMap::new())),
            remote_input_active: Arc::new(AtomicBool::new(false)),
            main_window_visible: Arc::new(AtomicBool::new(false)),
            main_window_focused: Arc::new(AtomicBool::new(false)),
            allow_explicit_quit: Arc::new(AtomicBool::new(false)),
            clipboard_target: Arc::new(Mutex::new(None)),
            input_receive_enabled: Arc::new(AtomicBool::new(false)),
            upgrading: Arc::new(AtomicBool::new(false)),
            clipboard_receive_enabled: Arc::new(AtomicBool::new(false)),
            transport_packets: Arc::new(AtomicU64::new(0)),
            input_events: Arc::new(AtomicU64::new(0)),
            clipboard_packets: Arc::new(AtomicU64::new(0)),
            runtime_toggle_shortcut: Mutex::new(None),
            runtime_toggle_menu_item: Mutex::new(None),
            screen_switch_request: Arc::new(Mutex::new(None)),
            screen_switch_shortcuts: Mutex::new(empty_screen_switch_hotkeys()),
            config_path,
        }
    }

    fn snapshot(&self) -> AppStateSnapshot {
        let layout = self.layout_snapshot();
        let runtime = self.runtime_status_for_layout(&layout);

        AppStateSnapshot { layout, runtime }
    }

    fn refresh_layout_from_disk(&self) {
        let native_layout = self
            .native_layout
            .lock()
            .map(|layout| layout.clone())
            .unwrap_or_else(|_| detect_fallback_layout());
        let Some(saved_layout) = load_layout_from_disk(&self.config_path) else {
            return;
        };
        let disk_layout = normalize_saved_layout(saved_layout, native_layout);
        let merged = if let Ok(mut current) = self.layout.lock() {
            *current = merge_disk_layout_into_runtime(disk_layout, &current);
            true
        } else {
            false
        };
        if merged {
            sync_layout_peer_presence(&self.layout, &self.peers);
        }
    }

    fn runtime_status(&self) -> RuntimeStatus {
        let layout = self.layout_snapshot();

        self.runtime_status_for_layout(&layout)
    }

    fn runtime_status_for_layout(&self, layout: &LayoutState) -> RuntimeStatus {
        let mut runtime = self.runtime.lock().unwrap().clone();
        runtime.discovery = self.discovery_status_for_layout(layout);
        runtime.clipboard = self.clipboard_status(layout);
        runtime.pairing = self.pairing_status_for_layout(layout);
        runtime.privilege = current_privilege_status();

        runtime
    }

    fn discovery_status(&self) -> DiscoveryStatus {
        let layout = self.layout_snapshot();
        self.discovery_status_for_layout(&layout)
    }

    fn discovery_status_for_layout(&self, layout: &LayoutState) -> DiscoveryStatus {
        let mut local_peer = local_peer_from_layout(layout);
        if let Some(transport) = self.quic_transport_handle() {
            apply_transport_to_peer(&mut local_peer, &transport);
        }
        local_peer.input_ready =
            advertised_input_ready(layout, self.input_receive_enabled.load(Ordering::Relaxed));
        let peers = active_peers(&self.peers, &local_peer.id);
        let state = if self.discovery_stop.lock().unwrap().is_some() {
            "ready"
        } else {
            "idle"
        };

        DiscoveryStatus {
            state: state.into(),
            detail: discovery_detail(peers.len(), state == "ready", layout.transport_port),
            port: layout.transport_port,
            local_peer,
            peers,
        }
    }

    fn pairing_status_for_layout(&self, layout: &LayoutState) -> PairingStatus {
        if layout.machine_role != "client" {
            return idle_pairing_status();
        }

        if !layout.paired_controllers.is_empty() {
            return PairingStatus {
                state: "paired".into(),
                code: String::new(),
                requester_name: String::new(),
                requester_ip: String::new(),
                expires_at_ms: 0,
                detail: "客户端已配对，只对白名单服务端响应。".into(),
            };
        }

        let now = Instant::now();
        if let Ok(mut challenge) = self.pairing_challenge.lock() {
            if challenge
                .as_ref()
                .map(|challenge| challenge.expires_at <= now)
                .unwrap_or(false)
            {
                *challenge = None;
            }

            if let Some(challenge) = challenge.as_ref() {
                return PairingStatus {
                    state: "requested".into(),
                    code: challenge.code.clone(),
                    requester_name: challenge.requester_name.clone(),
                    requester_ip: challenge.requester_ip.clone(),
                    expires_at_ms: challenge.expires_at_ms,
                    detail: "服务端正在请求配对，请在服务端输入此验证码。".into(),
                };
            }
        }

        PairingStatus {
            state: "available".into(),
            code: String::new(),
            requester_name: String::new(),
            requester_ip: String::new(),
            expires_at_ms: 0,
            detail: "客户端等待服务端发起配对。".into(),
        }
    }

    fn quic_transport_handle(&self) -> Option<quic_transport::TransportHandle> {
        self.quic_transport
            .lock()
            .ok()
            .and_then(|transport| transport.clone())
    }

    fn start_quic_transport(
        &self,
        preferred_port: u16,
    ) -> Result<quic_transport::TransportHandle, String> {
        if let Some(transport) = self.quic_transport_handle() {
            return Ok(transport);
        }

        let layout_for_input = Arc::clone(&self.layout);
        let layout_for_clipboard = Arc::clone(&self.layout);
        let layout_for_file_transfer = Arc::clone(&self.layout);
        let layout_for_pairing = Arc::clone(&self.layout);
        let native_layout_for_input = self.native_layout();
        let input_receive_enabled = Arc::clone(&self.input_receive_enabled);
        let clipboard_receive_enabled = Arc::clone(&self.clipboard_receive_enabled);
        let clipboard_seen_text = Arc::clone(&self.clipboard_seen_text);
        let clipboard_echo_until = Arc::clone(&self.clipboard_echo_until);
        let clipboard_last_sequences = Arc::clone(&self.clipboard_last_sequences);
        let clipboard_target = Arc::clone(&self.clipboard_target);
        let app_handle_for_file_transfer = self.app_handle.clone();
        let file_transfers = Arc::clone(&self.file_transfers);
        let transport_packets_for_input = Arc::clone(&self.transport_packets);
        let transport_packets_for_stream = Arc::clone(&self.transport_packets);
        let input_events = Arc::clone(&self.input_events);
        let clipboard_packets = Arc::clone(&self.clipboard_packets);
        let pairing_challenge_for_stream = Arc::clone(&self.pairing_challenge);
        let config_path_for_pairing = self.config_path.clone();
        let peers_for_pairing = Arc::clone(&self.peers);

        let on_datagram = Arc::new(move |payload: Vec<u8>, source| {
            if !input_receive_enabled.load(Ordering::Relaxed) {
                return;
            }
            let Ok(layout) = layout_for_input.lock() else {
                return;
            };
            let current_peer = local_peer_from_layout(&layout);
            if input::try_handle_control_packet_from_source(
                &layout,
                &payload,
                source,
                &current_peer.id,
            ) {
                transport_packets_for_input.fetch_add(1, Ordering::Relaxed);
                return;
            }
            if input::try_inject_packet_from_source(
                &layout,
                &native_layout_for_input,
                &payload,
                source,
                &input_events,
                &current_peer.id,
                &clipboard_target,
            ) {
                transport_packets_for_input.fetch_add(1, Ordering::Relaxed);
            }
        });

        let on_stream = Arc::new(move |payload: Vec<u8>, source| {
            if handle_pairing_stream_packet(
                &payload,
                source,
                &layout_for_pairing,
                &pairing_challenge_for_stream,
                &config_path_for_pairing,
                &peers_for_pairing,
            ) {
                transport_packets_for_stream.fetch_add(1, Ordering::Relaxed);
                return true;
            }

            if let Ok(layout) = layout_for_file_transfer.lock() {
                let current_peer = local_peer_from_layout(&layout);
                if handle_file_transfer_packet(
                    &payload,
                    &layout,
                    &current_peer.id,
                    &file_transfers,
                    &app_handle_for_file_transfer,
                ) {
                    transport_packets_for_stream.fetch_add(1, Ordering::Relaxed);
                    return true;
                }
            }

            if !clipboard_receive_enabled.load(Ordering::Relaxed) {
                return false;
            }
            let Ok(layout) = layout_for_clipboard.lock() else {
                return false;
            };
            let current_peer = local_peer_from_layout(&layout);
            if handle_clipboard_packet(
                &payload,
                &layout,
                &current_peer.id,
                &clipboard_seen_text,
                &clipboard_echo_until,
                &clipboard_last_sequences,
            ) {
                transport_packets_for_stream.fetch_add(1, Ordering::Relaxed);
                clipboard_packets.fetch_add(1, Ordering::Relaxed);
                return true;
            }
            false
        });

        let identity_dir = self
            .config_path
            .parent()
            .map(|parent| parent.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        let transport =
            quic_transport::start(preferred_port, identity_dir, on_datagram, on_stream)?;
        let mut stored = self
            .quic_transport
            .lock()
            .map_err(|_| "QUIC transport lock poisoned".to_string())?;
        *stored = Some(transport.clone());
        Ok(transport)
    }

    fn start_discovery(&self) -> Result<(), String> {
        let mut discovery_stop = self
            .discovery_stop
            .lock()
            .map_err(|_| "discovery state lock poisoned".to_string())?;

        if discovery_stop.is_some() {
            return Ok(());
        }

        // Best-effort: make sure inbound UDP to this binary is allowed through
        // Windows Defender Firewall, which is the usual reason a Windows client
        // is invisible to (and unreachable from) a peer on the LAN.
        #[cfg(target_os = "windows")]
        ensure_windows_firewall_rule();

        let mut layout = self
            .layout
            .lock()
            .map_err(|_| "layout state lock poisoned".to_string())?
            .clone();
        let desired_port = if layout.transport_port_mode == "auto" {
            default_transport_port()
        } else {
            layout.transport_port
        };
        let (socket, actual_port) = bind_available_udp_port(desired_port)?;
        let quic_transport = self.start_quic_transport(preferred_quic_port(actual_port))?;
        layout.transport_port = actual_port;
        layout.quic_port = quic_transport.port();
        if let Ok(mut stored_layout) = self.layout.lock() {
            stored_layout.transport_port = actual_port;
            stored_layout.quic_port = quic_transport.port();
            for device in &mut stored_layout.devices {
                if device.role == "local" {
                    device.transport_port = actual_port;
                    device.quic_port = quic_transport.port();
                    device.transport_public_key = quic_transport.public_key().to_string();
                    device.protocol_version = quic_transport::PROTOCOL_VERSION;
                }
            }
        }

        let mut local_peer = local_peer_from_layout(&layout);
        apply_transport_to_peer(&mut local_peer, &quic_transport);
        local_peer.input_ready =
            advertised_input_ready(&layout, self.input_receive_enabled.load(Ordering::Relaxed));
        let peers = Arc::clone(&self.peers);
        let layout_state = Arc::clone(&self.layout);
        let pairing_challenge = Arc::clone(&self.pairing_challenge);
        let app_handle = self.app_handle.clone();
        let input_receive_enabled = Arc::clone(&self.input_receive_enabled);
        let upgrading = Arc::clone(&self.upgrading);
        let transport_packets = Arc::clone(&self.transport_packets);
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        socket
            .set_broadcast(true)
            .map_err(|error| format!("failed to enable UDP broadcast: {error}"))?;
        socket
            .set_read_timeout(Some(Duration::from_millis(500)))
            .map_err(|error| format!("failed to set discovery read timeout: {error}"))?;
        // Aim announces at the configured base port and the span above it, not
        // our own (possibly drifted) `actual_port`, so a peer that landed on a
        // neighbouring port still receives them.
        let broadcast_targets = broadcast_addrs(desired_port);
        let direct_targets = known_peer_discovery_targets(&layout, desired_port);
        log::info!(
            "discovery started desired_port={} actual_port={} quic_port={} broadcast_targets={} directed_targets={}",
            desired_port,
            actual_port,
            quic_transport.port(),
            broadcast_targets.len(),
            direct_targets.len()
        );
        sync_layout_peer_presence(&self.layout, &self.peers);

        thread::spawn(move || {
            let mut buffer = [0_u8; 4096];
            let mut last_announce = Instant::now() - Duration::from_secs(10);
            let mut last_input_ready = input_receive_enabled.load(Ordering::Relaxed);
            let mut last_upgrading = upgrading.load(Ordering::Relaxed);

            while !thread_stop.load(Ordering::Relaxed) {
                let current_input_ready = input_receive_enabled.load(Ordering::Relaxed);
                let current_upgrading = upgrading.load(Ordering::Relaxed);
                if last_announce.elapsed() >= Duration::from_secs(3)
                    || current_input_ready != last_input_ready
                    || current_upgrading != last_upgrading
                {
                    let announcement = layout_state
                        .lock()
                        .map(|layout| {
                            if !should_send_public_announce(&layout) {
                                return None;
                            }
                            let mut peer = local_peer_from_layout(&layout);
                            apply_transport_to_peer(&mut peer, &quic_transport);
                            peer.input_ready = advertised_input_ready(&layout, current_input_ready);
                            peer.upgrading = upgrading.load(Ordering::Relaxed);
                            let direct_targets =
                                known_peer_discovery_targets(&layout, desired_port);
                            Some((peer, direct_targets))
                        })
                        .unwrap_or_else(|_| Some((local_peer.clone(), Vec::new())));
                    if let Some((local_peer, direct_targets)) = announcement {
                        for target in &broadcast_targets {
                            let _ = send_discovery_packet(
                                &socket,
                                "announce",
                                &local_peer,
                                target.as_str(),
                            );
                        }
                        let probed_peers = probe_known_peer_targets(&local_peer, &direct_targets);
                        if !probed_peers.is_empty() {
                            for peer in probed_peers {
                                warm_quic_peer(&quic_transport, &peer);
                                merge_peer(&peers, peer);
                            }
                            sync_layout_peer_presence(&layout_state, &peers);
                        }
                    }
                    last_announce = Instant::now();
                    last_input_ready = current_input_ready;
                    last_upgrading = current_upgrading;
                }

                if let Ok((length, source)) = socket.recv_from(&mut buffer) {
                    transport_packets.fetch_add(1, Ordering::Relaxed);
                    let payload = &buffer[..length];

                    if let Some(packet) = decode_discovery_packet(payload) {
                        let current = layout_state
                            .lock()
                            .map(|layout| {
                                let mut peer = local_peer_from_layout(&layout);
                                apply_transport_to_peer(&mut peer, &quic_transport);
                                peer.input_ready = advertised_input_ready(
                                    &layout,
                                    input_receive_enabled.load(Ordering::Relaxed),
                                );
                                peer.upgrading = upgrading.load(Ordering::Relaxed);
                                (layout.clone(), peer)
                            })
                            .unwrap_or_else(|_| (detect_fallback_layout(), local_peer.clone()));
                        let (current_layout, current_peer) = current;

                        if let Some(incoming) = peer_from_discovery_packet(
                            packet,
                            source.ip().to_string(),
                            &current_peer.id,
                        ) {
                            if incoming.kind == "pair-request" {
                                if begin_pairing_challenge(
                                    &pairing_challenge,
                                    &current_layout,
                                    &incoming.peer,
                                    source.ip().to_string(),
                                ) {
                                    let handle = app_handle.clone();
                                    let _ = app_handle.run_on_main_thread(move || {
                                        let _ = show_main_window_handle(&handle);
                                    });
                                    let _ = send_discovery_packet(
                                        &socket,
                                        "pair-challenge",
                                        &current_peer,
                                        source,
                                    );
                                }
                                continue;
                            }

                            if incoming.kind == "pair-confirm" {
                                continue;
                            }

                            if peer_visible_to_layout(&current_layout, &incoming.peer) {
                                merge_peer(&peers, incoming.peer.clone());
                                sync_layout_peer_presence(&layout_state, &peers);
                                warm_quic_peer(&quic_transport, &incoming.peer);
                            }

                            if matches!(incoming.kind.as_str(), "announce" | "probe") {
                                let reply =
                                    should_reply_to_discovery(&current_layout, &incoming.peer);
                                log::debug!(
                                    "discovery {} from {} id={} key={} cluster={} pairing_required={} -> reply={}",
                                    incoming.kind,
                                    source,
                                    incoming.peer.id,
                                    if incoming.peer.transport_public_key.is_empty() { "empty" } else { "set" },
                                    if incoming.peer.cluster_id.is_empty() { "empty" } else { "set" },
                                    incoming.peer.pairing_required,
                                    reply
                                );
                                if reply {
                                    let _ = send_discovery_packet(
                                        &socket,
                                        "reply",
                                        &current_peer,
                                        source,
                                    );
                                }
                            }
                        }
                    }
                }

                prune_stale_peers(&peers);
                sync_layout_peer_presence(&layout_state, &peers);
            }
        });

        *discovery_stop = Some(stop);
        Ok(())
    }

    fn start_input(&self, layout: LayoutState) -> (NativeStageStatus, NativeStageStatus) {
        sync_layout_peer_presence(&self.layout, &self.peers);
        self.input_receive_enabled
            .store(layout.input_mode == "receive", Ordering::Relaxed);
        let native_layout = self.native_layout();
        let Ok(mut input_stop) = self.input_stop.lock() else {
            return (
                NativeStageStatus {
                    state: "error".into(),
                    detail: "input runtime lock poisoned".into(),
                },
                NativeStageStatus {
                    state: "error".into(),
                    detail: "input runtime lock poisoned".into(),
                },
            );
        };

        if input_stop.is_some() {
            return input::input_runtime_status(&layout, &native_layout);
        }

        let Some(quic_transport) = self.quic_transport_handle() else {
            return (
                NativeStageStatus {
                    state: "error".into(),
                    detail: "QUIC transport is not ready.".into(),
                },
                input::input_runtime_status(&layout, &native_layout).1,
            );
        };

        let stop = Arc::new(AtomicBool::new(false));
        let statuses = input::start_input_runtime(
            layout,
            Arc::clone(&self.layout),
            native_layout,
            quic_transport,
            Arc::clone(&stop),
            Arc::clone(&self.remote_input_active),
            Arc::clone(&self.main_window_visible),
            Arc::clone(&self.main_window_focused),
            Arc::clone(&self.clipboard_target),
            Arc::clone(&self.input_events),
            Arc::clone(&self.screen_switch_request),
        );
        *input_stop = Some(stop);
        statuses
    }

    fn start_clipboard(&self, layout: LayoutState) -> NativeStageStatus {
        if !layout.clipboard_sync {
            self.stop_clipboard();
            return clipboard_disabled_status();
        }

        let Ok(mut clipboard_stop) = self.clipboard_stop.lock() else {
            return NativeStageStatus {
                state: "error".into(),
                detail: "clipboard runtime lock poisoned".into(),
            };
        };

        if clipboard_stop.is_some() {
            return clipboard_ready_status();
        }

        let local_peer = local_peer_from_layout(&layout);
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let clipboard_seen_text = Arc::clone(&self.clipboard_seen_text);
        let clipboard_echo_until = Arc::clone(&self.clipboard_echo_until);
        let clipboard_target = Arc::clone(&self.clipboard_target);
        let transport_packets = Arc::clone(&self.transport_packets);
        let clipboard_packets = Arc::clone(&self.clipboard_packets);
        let Some(quic_transport) = self.quic_transport_handle() else {
            return NativeStageStatus {
                state: "error".into(),
                detail: "QUIC transport is not ready.".into(),
            };
        };

        thread::spawn(move || {
            run_clipboard_sync(
                quic_transport,
                local_peer.id,
                clipboard_seen_text,
                clipboard_echo_until,
                clipboard_target,
                transport_packets,
                clipboard_packets,
                thread_stop,
            );
        });

        *clipboard_stop = Some(stop);
        self.clipboard_receive_enabled
            .store(true, Ordering::Relaxed);
        clipboard_ready_status()
    }

    fn clipboard_status(&self, layout: &LayoutState) -> NativeStageStatus {
        if !layout.clipboard_sync {
            return clipboard_disabled_status();
        }

        if self
            .clipboard_stop
            .lock()
            .map(|stop| stop.is_some())
            .unwrap_or(false)
        {
            clipboard_ready_status()
        } else {
            NativeStageStatus {
                state: "idle".into(),
                detail: "剪贴板同步已开启，仅在鼠标切到远端设备后惰性发送文本/图片剪贴板。".into(),
            }
        }
    }

    fn layout_snapshot(&self) -> LayoutState {
        sync_layout_peer_presence(&self.layout, &self.peers);
        self.layout
            .lock()
            .map(|layout| layout.clone())
            .unwrap_or_else(|_| self.native_layout())
    }

    fn native_layout(&self) -> LayoutState {
        self.native_layout
            .lock()
            .map(|layout| layout.clone())
            .unwrap_or_else(|_| detect_fallback_layout())
    }

    fn stop_discovery(&self) {
        if let Ok(mut stop) = self.discovery_stop.lock() {
            if let Some(signal) = stop.take() {
                signal.store(true, Ordering::Relaxed);
            }
        }
        if let Ok(mut transport) = self.quic_transport.lock() {
            if let Some(handle) = transport.take() {
                handle.shutdown();
            }
        }
    }

    fn stop_input(&self) {
        self.input_receive_enabled.store(false, Ordering::Relaxed);
        if let Ok(mut stop) = self.input_stop.lock() {
            if let Some(signal) = stop.take() {
                signal.store(true, Ordering::Relaxed);
            }
        }
        self.remote_input_active.store(false, Ordering::Relaxed);
        input::clear_clipboard_target(&self.clipboard_target);
        // Drop any modifier flags we were holding for injection so a lost
        // key-up cannot leave Shift/Ctrl/Cmd stuck for the next session.
        input::reset_injected_modifiers();
    }

    fn stop_clipboard(&self) {
        self.clipboard_receive_enabled
            .store(false, Ordering::Relaxed);
        input::clear_clipboard_target(&self.clipboard_target);
        if let Ok(mut stop) = self.clipboard_stop.lock() {
            if let Some(signal) = stop.take() {
                signal.store(true, Ordering::Relaxed);
            }
        }
    }
}

#[tauri::command]
fn load_app_state(state: tauri::State<'_, AppRuntime>) -> AppStateSnapshot {
    state.refresh_layout_from_disk();
    state.snapshot()
}

#[tauri::command]
fn read_runtime_status(state: tauri::State<'_, AppRuntime>) -> RuntimeStatus {
    state.runtime_status()
}

#[tauri::command]
fn read_diagnostic_info(
    app: AppHandle,
    state: tauri::State<'_, AppRuntime>,
) -> Result<DiagnosticInfo, String> {
    diagnostic_info(&app, state.inner())
}

#[tauri::command]
fn open_log_directory(app: AppHandle) -> Result<(), String> {
    let log_dir = app
        .path()
        .app_log_dir()
        .map_err(|error| format!("failed to resolve log directory: {error}"))?;
    fs::create_dir_all(&log_dir).map_err(|error| {
        format!(
            "failed to create log directory {}: {error}",
            log_dir.display()
        )
    })?;
    open_external_path(&log_dir)
}

#[tauri::command]
fn save_layout(
    layout: LayoutState,
    state: tauri::State<'_, AppRuntime>,
) -> Result<AppStateSnapshot, String> {
    let (previous_layout, saved_layout) = {
        let mut stored_layout = state
            .layout
            .lock()
            .map_err(|_| "layout state lock poisoned".to_string())?;
        let previous_layout = stored_layout.clone();
        let saved_layout = merge_runtime_owned_layout_fields(layout, &previous_layout);
        write_layout_to_disk(&state.config_path, &saved_layout)?;
        *stored_layout = saved_layout.clone();
        (previous_layout, saved_layout)
    };

    if runtime_relevant_layout_changed(&previous_layout, &saved_layout) {
        if previous_layout.transport_port_mode != saved_layout.transport_port_mode
            || previous_layout.transport_port != saved_layout.transport_port
        {
            state.stop_discovery();
            thread::sleep(Duration::from_millis(200));
        }
        restart_runtime_if_running(&state)?;
        if !state
            .runtime
            .lock()
            .map_err(|_| "runtime state lock poisoned".to_string())?
            .started
        {
            state.start_discovery()?;
        }
    }
    sync_runtime_toggle_shortcut(&state.app_handle)?;
    sync_screen_switch_shortcuts(&state.app_handle)?;
    Ok(state.snapshot())
}

fn merge_runtime_owned_layout_fields(
    mut incoming: LayoutState,
    current: &LayoutState,
) -> LayoutState {
    // The frontend saves whole LayoutState snapshots, but pairing can complete
    // asynchronously in the backend through an encrypted QUIC stream. Treat the
    // pairing credentials as backend-owned so a stale settings snapshot cannot
    // clear them and force the client to be paired again.
    incoming.cluster_id = current.cluster_id.clone();
    incoming.pair_secret = current.pair_secret.clone();

    if current.machine_role == "client"
        && incoming.machine_role == "client"
        && !current.paired_controllers.is_empty()
    {
        incoming.paired_controllers = current.paired_controllers.clone();
    }

    merge_local_runtime_device_fields(&mut incoming, current);
    merge_remote_upgrading_fields(&mut incoming, current);
    incoming
}

/// A remote device's "upgrading" state is determined entirely by the backend
/// (discovery announces + the grace timer), and the frontend never carries the
/// internal `upgrading_until_ms`. So a whole-layout save from the frontend must
/// not clobber it — otherwise saving while a client is mid-upgrade resets
/// `upgrading_until_ms` to 0 and the very next presence pass clears the badge
/// early. Treat both fields as backend-owned for matching remote devices.
fn merge_remote_upgrading_fields(incoming: &mut LayoutState, current: &LayoutState) {
    for incoming_device in incoming.devices.iter_mut() {
        if incoming_device.role == "local" {
            continue;
        }
        if let Some(current_device) = current
            .devices
            .iter()
            .find(|device| device.id == incoming_device.id)
        {
            incoming_device.upgrading = current_device.upgrading;
            incoming_device.upgrading_until_ms = current_device.upgrading_until_ms;
        }
    }
}

fn merge_disk_layout_into_runtime(mut disk: LayoutState, current: &LayoutState) -> LayoutState {
    if current.machine_role == "client"
        && disk.machine_role == "client"
        && disk.paired_controllers.is_empty()
        && !current.paired_controllers.is_empty()
    {
        disk.cluster_id = current.cluster_id.clone();
        disk.pair_secret = current.pair_secret.clone();
        disk.paired_controllers = current.paired_controllers.clone();
    }

    merge_local_runtime_device_fields(&mut disk, current);
    disk
}

fn merge_local_runtime_device_fields(incoming: &mut LayoutState, current: &LayoutState) {
    let Some(current_local) = current.devices.iter().find(|device| device.role == "local") else {
        return;
    };
    if current_local.transport_public_key.trim().is_empty() {
        return;
    }

    if let Some(incoming_local) = incoming
        .devices
        .iter_mut()
        .find(|device| device.role == "local" || device.id == current_local.id)
    {
        incoming_local.transport_public_key = current_local.transport_public_key.clone();
        incoming_local.protocol_version = current_local.protocol_version;
    }
}

fn runtime_relevant_layout_changed(previous: &LayoutState, next: &LayoutState) -> bool {
    // Device list/position changes are intentionally NOT here: discovery and the
    // input-capture loop both read the shared layout live, so adding, removing,
    // or repositioning a device takes effect without tearing down the transport.
    // Restarting on every device edit is what forced users to stop/start the
    // server (and churned QUIC keys) before a freshly added client would work.
    previous.input_mode != next.input_mode
        || previous.machine_role != next.machine_role
        || previous.clipboard_sync != next.clipboard_sync
        || previous.transport_port_mode != next.transport_port_mode
        || previous.transport_port != next.transport_port
}

fn restart_runtime_if_running(state: &AppRuntime) -> Result<(), String> {
    let started = state
        .runtime
        .lock()
        .map_err(|_| "runtime state lock poisoned".to_string())?
        .started;

    if !started {
        return Ok(());
    }

    state.stop_input();
    state.stop_clipboard();
    // Keep discovery/QUIC alive across input/clipboard restarts. Rebuilding the
    // QUIC endpoint on the same UDP port can briefly race the old endpoint and
    // make peers see "server refused to accept a new connection".
    thread::sleep(Duration::from_millis(300));
    state.start_discovery()?;
    let layout = state.layout_snapshot();
    let (capture, inject) = state.start_input(layout.clone());
    let clipboard = state.start_clipboard(layout.clone());
    let discovery = state.discovery_status_for_layout(&layout);
    let mut runtime = state
        .runtime
        .lock()
        .map_err(|_| "runtime state lock poisoned".to_string())?;

    runtime.transport = ready_transport_status(&discovery);
    runtime.capture = capture;
    runtime.inject = inject;
    runtime.clipboard = clipboard;
    runtime.discovery = discovery;
    runtime.pairing = state.pairing_status_for_layout(&layout);
    Ok(())
}

fn start_runtime_inner(state: &AppRuntime) -> Result<RuntimeStatus, String> {
    state.refresh_layout_from_disk();
    let discovery_error = state.start_discovery().err();
    let layout = state.layout_snapshot();
    let mut discovery = state.discovery_status();
    if let Some(error) = discovery_error {
        discovery.state = "error".into();
        discovery.detail = error;
    }
    let (capture, inject) = state.start_input(layout.clone());
    let clipboard = state.start_clipboard(layout.clone());

    let mut runtime = state
        .runtime
        .lock()
        .map_err(|_| "runtime state lock poisoned".to_string())?;

    *runtime = RuntimeStatus {
        started: true,
        transport: ready_transport_status(&discovery),
        capture,
        inject,
        clipboard,
        discovery,
        pairing: state.pairing_status_for_layout(&layout),
        privilege: current_privilege_status(),
        input_service: current_input_service_status(),
    };

    Ok(runtime.clone())
}

#[tauri::command]
fn start_runtime(
    app: AppHandle,
    state: tauri::State<'_, AppRuntime>,
) -> Result<RuntimeStatus, String> {
    let runtime = start_runtime_inner(state.inner())?;
    notify_runtime_state_changed(&app, &runtime);
    Ok(runtime)
}

fn ready_transport_status(discovery: &DiscoveryStatus) -> NativeStageStatus {
    NativeStageStatus {
        state: "ready".into(),
        detail: format!(
            "UDP discovery is ready on {}; QUIC is ready on {} for input datagrams and clipboard streams.",
            discovery.port, discovery.local_peer.quic_port
        ),
    }
}

fn diagnostic_info(app: &AppHandle, state: &AppRuntime) -> Result<DiagnosticInfo, String> {
    let snapshot = state.snapshot();
    let layout = snapshot.layout;
    let runtime = snapshot.runtime;
    let local_peer = runtime.discovery.local_peer.clone();
    let log_dir = app
        .path()
        .app_log_dir()
        .map_err(|error| format!("failed to resolve log directory: {error}"))?;
    let config_dir = state
        .config_path
        .parent()
        .map(|path| path.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let known_devices = layout
        .devices
        .iter()
        .filter(|device| device.role != "local")
        .map(|device| DiagnosticDevice {
            name: device.name.clone(),
            host: device.host.clone(),
            role: device.role.clone(),
            online: device.online,
            input_ready: device.input_ready,
            discovery_port: device.transport_port,
            quic_port: normalize_quic_port(device.transport_port, device.quic_port),
            same_subnet: same_ipv4_24_subnet(&local_peer.ip, &device.host),
        })
        .collect::<Vec<_>>();
    let network_hint = diagnostic_network_hint(&known_devices);
    let firewall_hint = diagnostic_firewall_hint();

    let mut lines = vec![
        "MyKVM diagnostics".to_string(),
        format!("version: v{}", env!("CARGO_PKG_VERSION")),
        format!("platform: {}", current_platform()),
        format!("role: {}", layout.machine_role),
        format!(
            "runtime: {}",
            if runtime.started {
                "started"
            } else {
                "stopped"
            }
        ),
        format!("local: {} / {}", local_peer.name, local_peer.ip),
        format!(
            "ports: discovery UDP {}, QUIC {}",
            runtime.discovery.port, local_peer.quic_port
        ),
        format!("discovery peers: {}", runtime.discovery.peers.len()),
        format!("paired controllers: {}", layout.paired_controllers.len()),
        format!("privilege: {}", runtime.privilege.detail),
        format!("input service: {}", runtime.input_service.detail),
        format!("log dir: {}", log_dir.display()),
        format!("config dir: {}", config_dir.display()),
        format!("network hint: {network_hint}"),
        format!("firewall hint: {firewall_hint}"),
    ];
    if known_devices.is_empty() {
        lines.push("known devices: none".into());
    } else {
        lines.push("known devices:".into());
        for device in &known_devices {
            let subnet = match device.same_subnet {
                Some(true) => "same /24",
                Some(false) => "different /24",
                None => "subnet unknown",
            };
            lines.push(format!(
                "- {} {} host={} online={} inputReady={} UDP={} QUIC={} {}",
                device.role,
                device.name,
                device.host,
                device.online,
                device.input_ready,
                device.discovery_port,
                device.quic_port,
                subnet
            ));
        }
    }

    Ok(DiagnosticInfo {
        report: lines.join("\n"),
        app_version: env!("CARGO_PKG_VERSION").into(),
        platform: current_platform().into(),
        role: layout.machine_role,
        runtime_started: runtime.started,
        local_name: local_peer.name,
        local_ip: local_peer.ip,
        discovery_port: runtime.discovery.port,
        quic_port: local_peer.quic_port,
        peer_count: runtime.discovery.peers.len(),
        known_devices,
        log_dir: log_dir.to_string_lossy().into_owned(),
        config_dir: config_dir.to_string_lossy().into_owned(),
        network_hint,
        firewall_hint,
    })
}

fn diagnostic_network_hint(devices: &[DiagnosticDevice]) -> String {
    if devices.is_empty() {
        return "No remote devices are saved on this machine yet.".into();
    }

    let known = devices
        .iter()
        .filter_map(|device| device.same_subnet)
        .collect::<Vec<_>>();
    if known.iter().any(|same_subnet| !same_subnet) {
        return "At least one saved peer appears outside this machine's local /24; routing, VLAN, AP isolation, or firewall rules may be involved.".into();
    }
    if known.len() == devices.len() && known.iter().all(|same_subnet| *same_subnet) {
        return "Saved peer IPs appear to be on the same local /24 as this machine.".into();
    }
    "Some saved peer hosts are names or non-IPv4 addresses, so subnet matching could not be inferred.".into()
}

#[cfg(target_os = "windows")]
fn diagnostic_firewall_hint() -> String {
    if is_windows_process_elevated().unwrap_or(false) {
        "Running as administrator; MyKVM attempts to add a Windows Defender Firewall UDP allow rule for this executable at startup.".into()
    } else {
        "Running as a standard user; MyKVM cannot add its Windows Defender Firewall rule automatically. If discovery drops, allow MyKVM UDP on Private networks.".into()
    }
}

#[cfg(not(target_os = "windows"))]
fn diagnostic_firewall_hint() -> String {
    "Check the OS firewall if LAN discovery or QUIC traffic is blocked.".into()
}

fn same_ipv4_24_subnet(local_ip: &str, host_value: &str) -> Option<bool> {
    let local = local_ip.parse::<std::net::Ipv4Addr>().ok()?;
    let remote = ipv4_from_host_value(host_value)?;
    let local_octets = local.octets();
    let remote_octets = remote.octets();
    Some(local_octets[..3] == remote_octets[..3])
}

fn ipv4_from_host_value(host_value: &str) -> Option<std::net::Ipv4Addr> {
    host_candidates(host_value)
        .into_iter()
        .find_map(|candidate| {
            let (host, _) = split_host_port(&candidate);
            host.parse::<std::net::Ipv4Addr>().ok()
        })
}

fn stop_runtime_inner(state: &AppRuntime) -> Result<RuntimeStatus, String> {
    state.stop_input();
    state.stop_clipboard();
    state.start_discovery()?;

    let mut runtime = state
        .runtime
        .lock()
        .map_err(|_| "runtime state lock poisoned".to_string())?;
    let layout = state.layout_snapshot();
    let mut stopped_runtime = default_runtime(&layout);
    stopped_runtime.discovery = state.discovery_status_for_layout(&layout);
    stopped_runtime.pairing = state.pairing_status_for_layout(&layout);
    *runtime = stopped_runtime;
    Ok(runtime.clone())
}

#[tauri::command]
fn stop_runtime(
    app: AppHandle,
    state: tauri::State<'_, AppRuntime>,
) -> Result<RuntimeStatus, String> {
    let runtime = stop_runtime_inner(state.inner())?;
    notify_runtime_state_changed(&app, &runtime);
    Ok(runtime)
}

fn toggle_runtime_from_app(app: &AppHandle) -> Result<RuntimeStatus, String> {
    let state = app.state::<AppRuntime>();
    let started = state
        .runtime
        .lock()
        .map_err(|_| "runtime state lock poisoned".to_string())?
        .started;
    let runtime = if started {
        stop_runtime_inner(state.inner())?
    } else {
        start_runtime_inner(state.inner())?
    };
    notify_runtime_state_changed(app, &runtime);
    Ok(runtime)
}

fn notify_runtime_state_changed(app: &AppHandle, runtime: &RuntimeStatus) {
    update_runtime_tray_state(app, runtime.started);
    let _ = app.emit(RUNTIME_STATE_EVENT, runtime);
}

fn update_runtime_tray_state(app: &AppHandle, started: bool) {
    if let Some(state) = app.try_state::<AppRuntime>() {
        if let Ok(item) = state.runtime_toggle_menu_item.lock() {
            if let Some(item) = item.as_ref() {
                let _ = item.set_text(runtime_toggle_menu_label(started));
            }
        }
    }

    if let Some(tray) = app.tray_by_id("main") {
        let _ = tray.set_tooltip(Some(runtime_tray_tooltip(started)));
    }
}

fn runtime_toggle_menu_label(started: bool) -> &'static str {
    if started {
        "快捷启停：已启动"
    } else {
        "快捷启停：已停止"
    }
}

fn runtime_tray_tooltip(started: bool) -> &'static str {
    if started {
        "mykvm · 已启动"
    } else {
        "mykvm · 已停止"
    }
}

fn sync_runtime_toggle_shortcut(app: &AppHandle) -> Result<(), String> {
    let Some(state) = app.try_state::<AppRuntime>() else {
        return Ok(());
    };
    let shortcut = runtime_toggle_shortcut_for_layout(&state.layout_snapshot())?;
    let mut current = state
        .runtime_toggle_shortcut
        .lock()
        .map_err(|_| "runtime toggle shortcut lock poisoned".to_string())?;

    if current.as_deref() == shortcut.as_deref() {
        return Ok(());
    }

    if let Some(previous) = current.take() {
        if let Err(error) = app.global_shortcut().unregister(previous.as_str()) {
            log::warn!("failed to unregister quick start/stop shortcut {previous}: {error}");
        }
    }

    if let Some(next) = shortcut {
        app.global_shortcut()
            .register(next.as_str())
            .map_err(|error| {
                format!("failed to register quick start/stop shortcut {next}: {error}")
            })?;
        *current = Some(next);
    }

    Ok(())
}

fn runtime_toggle_shortcut_for_layout(layout: &LayoutState) -> Result<Option<String>, String> {
    if layout.machine_role != "server" {
        return Ok(None);
    }

    canonical_runtime_toggle_shortcut(&layout.edge_switch_hotkey)
}

/// Register/unregister the four direction hotkeys so they stay in sync with the
/// saved layout. Mirrors `sync_runtime_toggle_shortcut`: compares against the
/// stored values and only touches the ones that changed.
fn sync_screen_switch_shortcuts(app: &AppHandle) -> Result<(), String> {
    let Some(state) = app.try_state::<AppRuntime>() else {
        return Ok(());
    };
    let layout = state.layout_snapshot();
    let next = screen_switch_shortcuts_for_layout(&layout);

    let mut current = state
        .screen_switch_shortcuts
        .lock()
        .map_err(|_| "screen switch shortcuts lock poisoned".to_string())?;

    for (next_str, prev_str) in [
        (&next.left, &current.left),
        (&next.right, &current.right),
        (&next.up, &current.up),
        (&next.down, &current.down),
    ] {
        if next_str == prev_str {
            continue;
        }
        if !prev_str.is_empty() {
            if let Err(error) = app.global_shortcut().unregister(prev_str.as_str()) {
                log::warn!("failed to unregister screen switch shortcut {prev_str}: {error}");
            }
        }
        if !next_str.is_empty() {
            if let Err(error) = app.global_shortcut().register(next_str.as_str()) {
                log::warn!("failed to register screen switch shortcut {next_str}: {error}");
            } else {
                log::info!("registered screen switch shortcut: {next_str}");
            }
        }
    }

    *current = next;
    Ok(())
}

fn screen_switch_shortcuts_for_layout(layout: &LayoutState) -> ScreenSwitchHotkeys {
    if layout.machine_role != "server" {
        return empty_screen_switch_hotkeys();
    }

    ScreenSwitchHotkeys {
        left: canonical_runtime_toggle_shortcut(&layout.screen_switch_hotkeys.left)
            .unwrap_or(None)
            .unwrap_or_default(),
        right: canonical_runtime_toggle_shortcut(&layout.screen_switch_hotkeys.right)
            .unwrap_or(None)
            .unwrap_or_default(),
        up: canonical_runtime_toggle_shortcut(&layout.screen_switch_hotkeys.up)
            .unwrap_or(None)
            .unwrap_or_default(),
        down: canonical_runtime_toggle_shortcut(&layout.screen_switch_hotkeys.down)
            .unwrap_or(None)
            .unwrap_or_default(),
    }
}

fn empty_screen_switch_hotkeys() -> ScreenSwitchHotkeys {
    ScreenSwitchHotkeys {
        left: String::new(),
        right: String::new(),
        up: String::new(),
        down: String::new(),
    }
}

/// Dispatch a pressed global shortcut to its action. The runtime-toggle
/// shortcut starts/stops capture; the four direction shortcuts post a switch
/// request that the capture loop consumes.
fn route_global_shortcut(
    app: &AppHandle,
    shortcut: &tauri_plugin_global_shortcut::Shortcut,
) -> Result<(), String> {
    let Some(state) = app.try_state::<AppRuntime>() else {
        return Ok(());
    };
    if state.layout_snapshot().machine_role != "server" {
        return Ok(());
    }

    // Runtime toggle (quick start/stop).
    let toggle = state
        .runtime_toggle_shortcut
        .lock()
        .map_err(|_| "runtime toggle shortcut lock poisoned".to_string())?;
    if let Some(toggle_str) = toggle.as_ref() {
        if let Ok(toggle_shortcut) = toggle_str.parse::<tauri_plugin_global_shortcut::Shortcut>() {
            if shortcut == &toggle_shortcut {
                drop(toggle);
                toggle_runtime_from_app(app)?;
                return Ok(());
            }
        }
    }
    drop(toggle);

    // Direction switch hotkeys.
    let directions = state
        .screen_switch_shortcuts
        .lock()
        .map_err(|_| "screen switch shortcuts lock poisoned".to_string())?;
    let direction = [
        (directions.left.as_str(), input::SwitchDirection::Left),
        (directions.right.as_str(), input::SwitchDirection::Right),
        (directions.up.as_str(), input::SwitchDirection::Up),
        (directions.down.as_str(), input::SwitchDirection::Down),
    ]
    .into_iter()
    .find_map(|(stored, dir)| {
        if stored.is_empty() {
            return None;
        }
        stored
            .parse::<tauri_plugin_global_shortcut::Shortcut>()
            .ok()
            .filter(|parsed| parsed == shortcut)
            .map(|_| dir)
    });
    drop(directions);

    if let Some(direction) = direction {
        if let Ok(mut request) = state.screen_switch_request.lock() {
            // Only the latest request wins; a rapid double-tap overwrites.
            *request = Some(direction);
        }
    }

    Ok(())
}

fn canonical_runtime_toggle_shortcut(value: &str) -> Result<Option<String>, String> {
    let normalized = normalize_edge_switch_hotkey(value);
    if matches!(normalized.as_str(), "disabled" | "disable" | "off" | "none") {
        return Ok(None);
    }

    let mut ctrl = false;
    let mut alt = false;
    let mut shift = false;
    let mut meta = false;
    let mut key = None;

    for part in normalized.split('+').filter(|part| !part.is_empty()) {
        match part {
            "ctrl" | "control" => ctrl = true,
            "alt" | "option" => alt = true,
            "shift" => shift = true,
            "meta" | "cmd" | "command" | "win" | "windows" | "super" | "os" => meta = true,
            raw_key => {
                if key.is_some() {
                    return Err("快捷启停快捷键只能包含一个主按键。".into());
                }
                key = Some(
                    canonical_runtime_toggle_key(raw_key)
                        .ok_or_else(|| format!("无法识别快捷启停快捷键按键：{raw_key}"))?,
                );
            }
        }
    }

    let key = key.ok_or_else(|| "快捷启停快捷键缺少主按键。".to_string())?;
    if !(ctrl || alt || shift || meta) && !allows_single_runtime_toggle_key(&key) {
        return Err("快捷启停快捷键需要使用组合键，或使用 F1-F24 / ScrollLock。".into());
    }

    let mut parts = Vec::new();
    if ctrl {
        parts.push("control".to_string());
    }
    if alt {
        parts.push("alt".to_string());
    }
    if shift {
        parts.push("shift".to_string());
    }
    if meta {
        parts.push("super".to_string());
    }
    parts.push(key);
    let shortcut = parts.join("+");
    shortcut
        .parse::<tauri_plugin_global_shortcut::Shortcut>()
        .map_err(|error| format!("无法注册快捷启停快捷键 {normalized}: {error}"))?;

    Ok(Some(shortcut))
}

fn canonical_runtime_toggle_key(key: &str) -> Option<String> {
    if key.len() == 1 {
        let byte = key.as_bytes()[0];
        if byte.is_ascii_alphanumeric() {
            return Some(key.to_ascii_uppercase());
        }
    }

    if let Some(function_number) = key
        .strip_prefix('f')
        .and_then(|value| value.parse::<u8>().ok())
    {
        if (1..=24).contains(&function_number) {
            return Some(format!("F{function_number}"));
        }
    }

    Some(
        match key {
            "space" | "spacebar" => "space",
            "tab" => "tab",
            "enter" | "return" => "enter",
            "esc" | "escape" => "escape",
            "scrolllock" | "scroll" | "scrlk" => "scrolllock",
            "up" | "arrowup" => "arrowup",
            "down" | "arrowdown" => "arrowdown",
            "left" | "arrowleft" => "arrowleft",
            "right" | "arrowright" => "arrowright",
            _ => return None,
        }
        .into(),
    )
}

fn allows_single_runtime_toggle_key(key: &str) -> bool {
    key.starts_with('F') || key == "scrolllock"
}

#[tauri::command]
fn restart_as_admin(app: AppHandle, state: tauri::State<'_, AppRuntime>) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        if is_windows_process_elevated().unwrap_or(false) {
            return Ok(());
        }

        // Release our UDP discovery + QUIC sockets before handing off so the
        // elevated instance can rebind the SAME ports instead of racing this
        // dying process for them. When that race is lost the QUIC port drifts
        // upward (the discovery port is protected by SO_REUSEADDR, the QUIC port
        // is not) and the controller keeps targeting the stale endpoint — the
        // intermittent "device shows online after an admin-restart but the cursor
        // won't cross until you re-pair" symptom. The elevated copy starts its
        // own runtime on launch, so we are only tearing down, not restarting.
        state.stop_input();
        state.stop_discovery();

        release_single_instance();
        restart_current_process_as_admin()?;
        request_app_quit(&app);
        Ok(())
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = (app, state);
        Err("Administrator restart is only available on Windows.".into())
    }
}

#[tauri::command]
fn read_input_service_status(state: tauri::State<'_, AppRuntime>) -> InputServiceStatus {
    let status = current_input_service_status();
    update_runtime_input_service_status(state.inner(), &status);
    status
}

#[tauri::command]
fn install_input_service(
    state: tauri::State<'_, AppRuntime>,
) -> Result<InputServiceStatus, String> {
    #[cfg(target_os = "windows")]
    {
        let helper_path = resolve_input_helper_path()?;
        let status = if is_windows_process_elevated().unwrap_or(false) {
            install_windows_input_service(&helper_path)?;
            start_windows_input_service()?;
            current_input_service_status()
        } else {
            launch_current_process_as_admin(&[
                INSTALL_INPUT_SERVICE_ARG.into(),
                HELPER_PATH_ARG.into(),
                helper_path.to_string_lossy().into_owned(),
            ])?;
            InputServiceStatus {
                detail: "Administrator approval requested to install the input service.".into(),
                ..current_input_service_status()
            }
        };
        update_runtime_input_service_status(state.inner(), &status);
        Ok(status)
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = state;
        Err("Windows input service is only available on Windows.".into())
    }
}

#[tauri::command]
fn uninstall_input_service(
    state: tauri::State<'_, AppRuntime>,
) -> Result<InputServiceStatus, String> {
    #[cfg(target_os = "windows")]
    {
        let status = if is_windows_process_elevated().unwrap_or(false) {
            uninstall_windows_input_service()?;
            current_input_service_status()
        } else {
            launch_current_process_as_admin(&[UNINSTALL_INPUT_SERVICE_ARG.into()])?;
            InputServiceStatus {
                detail: "Administrator approval requested to uninstall the input service.".into(),
                ..current_input_service_status()
            }
        };
        update_runtime_input_service_status(state.inner(), &status);
        Ok(status)
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = state;
        Err("Windows input service is only available on Windows.".into())
    }
}

fn update_runtime_input_service_status(state: &AppRuntime, status: &InputServiceStatus) {
    if let Ok(mut runtime) = state.runtime.lock() {
        runtime.input_service = status.clone();
    }
}

#[tauri::command]
fn send_secure_attention(
    device_id: String,
    state: tauri::State<'_, AppRuntime>,
) -> Result<(), String> {
    let layout = state.layout_snapshot();
    let Some(quic_transport) = state.quic_transport_handle() else {
        return Err("QUIC transport is not ready; start the runtime first.".into());
    };

    input::send_secure_attention_control(&layout, &quic_transport, &device_id)?;
    state.transport_packets.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

#[tauri::command]
fn send_files_to_device(
    device_id: String,
    paths: Vec<String>,
    state: tauri::State<'_, AppRuntime>,
) -> Result<FileTransferSummary, String> {
    send_files_to_device_with_destination(state.inner(), &device_id, paths, None)
}

fn send_files_to_device_with_destination(
    state: &AppRuntime,
    device_id: &str,
    paths: Vec<String>,
    destination_hint: Option<&str>,
) -> Result<FileTransferSummary, String> {
    if paths.is_empty() {
        return Err("请选择要传输的文件。".into());
    }

    state.start_discovery()?;
    let layout = state.layout_snapshot();
    if !layout.file_transfer_enabled {
        return Err("文件传输未开启。".into());
    }
    let mut local_peer = local_peer_from_layout(&layout);
    let quic_transport = state
        .quic_transport_handle()
        .ok_or_else(|| "QUIC transport is not ready; start the runtime first.".to_string())?;
    apply_transport_to_peer(&mut local_peer, &quic_transport);

    let peers = active_peer_snapshot(&state.peers);
    let target = file_transfer_target_for_device(&layout, &peers, device_id)?;
    let files = collect_transfer_files(&paths)?;
    let mut file_count = 0_usize;
    let mut byte_count = 0_u64;

    for file in files {
        let packet_count = send_transfer_file(
            &quic_transport,
            &local_peer.id,
            &target,
            &file,
            destination_hint,
        )?;
        state
            .transport_packets
            .fetch_add(packet_count, Ordering::Relaxed);
        file_count += 1;
        byte_count = byte_count.saturating_add(file.total_bytes);
    }

    Ok(FileTransferSummary {
        target_name: target.name,
        file_count,
        byte_count,
    })
}

fn start_edge_drop_window_sync(app_handle: AppHandle) {
    thread::spawn(move || loop {
        let state = app_handle.state::<AppRuntime>();
        let runtime = state.inner();
        let layout = runtime.layout_snapshot();
        let peers = active_peer_snapshot(&runtime.peers);
        let specs = edge_drop_specs_for_window_visibility(
            &layout,
            &peers,
            runtime.main_window_visible.load(Ordering::Relaxed),
        );
        let next_labels = specs
            .iter()
            .map(|spec| spec.label.clone())
            .collect::<HashSet<_>>();
        let next_targets = specs
            .iter()
            .map(|spec| (spec.label.clone(), spec.target_device_id.clone()))
            .collect::<HashMap<_, _>>();
        let stale_labels = runtime
            .edge_drop_targets
            .lock()
            .map(|mut targets| {
                let stale = targets
                    .keys()
                    .filter(|label| !next_labels.contains(*label))
                    .cloned()
                    .collect::<Vec<_>>();
                *targets = next_targets;
                stale
            })
            .unwrap_or_default();

        let main_handle = app_handle.clone();
        let _ = app_handle.run_on_main_thread(move || {
            sync_edge_drop_windows_on_main(&main_handle, specs, stale_labels);
        });

        thread::sleep(Duration::from_millis(1500));
    });
}

fn sync_edge_drop_windows_on_main(
    app_handle: &AppHandle,
    specs: Vec<EdgeDropWindowSpec>,
    stale_labels: Vec<String>,
) {
    for label in stale_labels {
        if let Some(window) = app_handle.get_webview_window(&label) {
            let _ = window.hide();
        }
    }

    for spec in specs {
        if let Some(window) = app_handle.get_webview_window(&spec.label) {
            let _ = window.set_position(tauri::Position::Logical(tauri::LogicalPosition::new(
                spec.x, spec.y,
            )));
            let _ = window.set_size(tauri::Size::Logical(tauri::LogicalSize::new(
                spec.width,
                spec.height,
            )));
            let _ = window.set_always_on_top(true);
            let _ = window.set_focusable(false);
            if !window.is_visible().unwrap_or(false) {
                let _ = window.show();
            }
            continue;
        }

        let build_result = WebviewWindowBuilder::new(
            app_handle,
            &spec.label,
            WebviewUrl::App("edge-drop.html".into()),
        )
        .title("")
        .position(spec.x, spec.y)
        .inner_size(spec.width, spec.height)
        .decorations(false)
        .resizable(false)
        .maximizable(false)
        .minimizable(false)
        .closable(false)
        .shadow(false)
        .transparent(true)
        .always_on_top(true)
        .visible_on_all_workspaces(true)
        .skip_taskbar(true)
        .focused(false)
        .focusable(false)
        .build();

        if let Err(error) = build_result {
            log::warn!("edge drop window {} failed: {error}", spec.label);
        }
    }
}

fn handle_edge_drop_window_event(window: &tauri::Window, event: &WindowEvent) -> bool {
    let label = window.label();
    if !label.starts_with(EDGE_DROP_LABEL_PREFIX) {
        return false;
    }

    match event {
        WindowEvent::CloseRequested { api, .. } => {
            api.prevent_close();
            let _ = window.hide();
        }
        WindowEvent::DragDrop(DragDropEvent::Drop { paths, .. }) => {
            if paths.is_empty() {
                return true;
            }
            let app_handle = window.app_handle().clone();
            let label = label.to_string();
            let paths = paths
                .iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            thread::spawn(move || {
                let state = app_handle.state::<AppRuntime>();
                let target_device_id = state
                    .edge_drop_targets
                    .lock()
                    .ok()
                    .and_then(|targets| targets.get(&label).cloned());
                let Some(target_device_id) = target_device_id else {
                    log::warn!("edge drop ignored: no target for {label}");
                    return;
                };

                match send_files_to_device_with_destination(
                    state.inner(),
                    &target_device_id,
                    paths,
                    Some(FILE_TRANSFER_DESTINATION_POINTER),
                ) {
                    Ok(summary) => log::info!(
                        "edge drop sent {} file(s) to {} bytes={}",
                        summary.file_count,
                        summary.target_name,
                        summary.byte_count
                    ),
                    Err(error) => log::warn!("edge drop transfer failed: {error}"),
                }
            });
        }
        _ => {}
    }

    true
}

fn handle_file_drop_landing_window_event(window: &tauri::Window, event: &WindowEvent) -> bool {
    if window.label() != FILE_DROP_LANDING_LABEL {
        return false;
    }

    if let WindowEvent::CloseRequested { api, .. } = event {
        api.prevent_close();
        let _ = window.hide();
    }

    true
}

fn edge_drop_specs_for_layout(layout: &LayoutState, peers: &[LanPeer]) -> Vec<EdgeDropWindowSpec> {
    if !layout.file_transfer_enabled {
        return Vec::new();
    }

    let Some(local_device) = layout.devices.iter().find(|device| device.role == "local") else {
        return Vec::new();
    };

    let mut specs = match layout.machine_role.as_str() {
        "server" => server_edge_drop_specs(layout, local_device),
        "client" => client_edge_drop_specs(layout, local_device, peers),
        _ => Vec::new(),
    };
    specs.sort_by(|left, right| left.label.cmp(&right.label));
    specs
}

fn edge_drop_specs_for_window_visibility(
    layout: &LayoutState,
    peers: &[LanPeer],
    main_window_visible: bool,
) -> Vec<EdgeDropWindowSpec> {
    if main_window_visible {
        edge_drop_specs_for_layout(layout, peers)
    } else {
        Vec::new()
    }
}

fn server_edge_drop_specs(layout: &LayoutState, local_device: &Device) -> Vec<EdgeDropWindowSpec> {
    let mut specs = Vec::new();
    for device in layout.devices.iter().filter(|device| {
        device.role != "local"
            && device.online
            && device.input_ready
            && device.protocol_version == quic_transport::PROTOCOL_VERSION
            && !device.transport_public_key.trim().is_empty()
    }) {
        for local_screen in &local_device.screens {
            for remote_screen in &device.screens {
                if edge_screens_overlap(local_screen, remote_screen) {
                    continue;
                }
                let Some(side) = edge_touching_side(local_screen, remote_screen) else {
                    continue;
                };
                let Some((x, y, width, height)) =
                    edge_drop_rect_between(local_screen, remote_screen, side)
                else {
                    continue;
                };
                specs.push(EdgeDropWindowSpec {
                    label: edge_drop_label(&device.id, local_screen, Some(remote_screen), side),
                    target_device_id: device.id.clone(),
                    x,
                    y,
                    width,
                    height,
                });
            }
        }
    }
    specs
}

fn client_edge_drop_specs(
    layout: &LayoutState,
    local_device: &Device,
    peers: &[LanPeer],
) -> Vec<EdgeDropWindowSpec> {
    let Some(target_device_id) = online_paired_controller_id(layout, peers) else {
        return Vec::new();
    };

    let mut specs = Vec::new();
    for local_screen in &local_device.screens {
        for side in [
            EdgeDropSide::Left,
            EdgeDropSide::Right,
            EdgeDropSide::Top,
            EdgeDropSide::Bottom,
        ] {
            if local_edge_has_neighbor(&local_device.screens, local_screen, side) {
                continue;
            }
            let (x, y, width, height) = outer_edge_drop_rect(local_screen, side);
            specs.push(EdgeDropWindowSpec {
                label: edge_drop_label(&target_device_id, local_screen, None, side),
                target_device_id: target_device_id.clone(),
                x,
                y,
                width,
                height,
            });
        }
    }

    specs
}

fn online_paired_controller_id(layout: &LayoutState, peers: &[LanPeer]) -> Option<String> {
    layout
        .paired_controllers
        .iter()
        .find(|controller| {
            peers.iter().any(|peer| {
                (peer.id == controller.id
                    || (!controller.transport_public_key.trim().is_empty()
                        && peer.transport_public_key == controller.transport_public_key))
                    && peer.protocol_version == quic_transport::PROTOCOL_VERSION
                    && !peer.transport_public_key.trim().is_empty()
                    && peer.quic_port != 0
            })
        })
        .map(|controller| controller.id.clone())
}

fn edge_drop_label(
    target_device_id: &str,
    local_screen: &Screen,
    remote_screen: Option<&Screen>,
    side: EdgeDropSide,
) -> String {
    let remote = remote_screen
        .map(|screen| edge_label_component(&screen.id))
        .unwrap_or_else(|| "outer".into());
    format!(
        "{}{}-{}-{}-{}",
        EDGE_DROP_LABEL_PREFIX,
        edge_label_component(target_device_id),
        edge_label_component(&local_screen.id),
        remote,
        side.as_str()
    )
}

fn edge_label_component(value: &str) -> String {
    let component = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    let component = component
        .trim_matches('-')
        .chars()
        .take(48)
        .collect::<String>();
    if !component.is_empty() {
        return component;
    }

    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    format!("id-{:016x}", hasher.finish())
}

fn edge_drop_rect_between(
    local: &Screen,
    remote: &Screen,
    side: EdgeDropSide,
) -> Option<(f64, f64, f64, f64)> {
    let thickness = EDGE_DROP_THICKNESS;
    let (x, y, width, height) = match side {
        EdgeDropSide::Left | EdgeDropSide::Right => {
            let y = local.y.max(remote.y);
            let height = (local.y + local.height).min(remote.y + remote.height) - y;
            if height <= 0 {
                return None;
            }
            let x = if side == EdgeDropSide::Right {
                local.x + local.width - thickness
            } else {
                local.x
            };
            (x, y, thickness, height)
        }
        EdgeDropSide::Top | EdgeDropSide::Bottom => {
            let x = local.x.max(remote.x);
            let width = (local.x + local.width).min(remote.x + remote.width) - x;
            if width <= 0 {
                return None;
            }
            let y = if side == EdgeDropSide::Bottom {
                local.y + local.height - thickness
            } else {
                local.y
            };
            (x, y, width, thickness)
        }
    };

    Some((x as f64, y as f64, width as f64, height as f64))
}

fn outer_edge_drop_rect(screen: &Screen, side: EdgeDropSide) -> (f64, f64, f64, f64) {
    let thickness = EDGE_DROP_THICKNESS;
    let (x, y, width, height) = match side {
        EdgeDropSide::Left => (screen.x, screen.y, thickness, screen.height),
        EdgeDropSide::Right => (
            screen.x + screen.width - thickness,
            screen.y,
            thickness,
            screen.height,
        ),
        EdgeDropSide::Top => (screen.x, screen.y, screen.width, thickness),
        EdgeDropSide::Bottom => (
            screen.x,
            screen.y + screen.height - thickness,
            screen.width,
            thickness,
        ),
    };

    (x as f64, y as f64, width as f64, height as f64)
}

fn local_edge_has_neighbor(screens: &[Screen], screen: &Screen, side: EdgeDropSide) -> bool {
    screens
        .iter()
        .filter(|candidate| candidate.id != screen.id)
        .any(|candidate| edge_touching_side(screen, candidate) == Some(side))
}

fn edge_touching_side(local: &Screen, remote: &Screen) -> Option<EdgeDropSide> {
    if edge_near(local.x + local.width, remote.x)
        && edge_ranges_overlap(
            local.y,
            local.y + local.height,
            remote.y,
            remote.y + remote.height,
        )
    {
        return Some(EdgeDropSide::Right);
    }

    if edge_near(local.x, remote.x + remote.width)
        && edge_ranges_overlap(
            local.y,
            local.y + local.height,
            remote.y,
            remote.y + remote.height,
        )
    {
        return Some(EdgeDropSide::Left);
    }

    if edge_near(local.y + local.height, remote.y)
        && edge_ranges_overlap(
            local.x,
            local.x + local.width,
            remote.x,
            remote.x + remote.width,
        )
    {
        return Some(EdgeDropSide::Bottom);
    }

    if edge_near(local.y, remote.y + remote.height)
        && edge_ranges_overlap(
            local.x,
            local.x + local.width,
            remote.x,
            remote.x + remote.width,
        )
    {
        return Some(EdgeDropSide::Top);
    }

    None
}

fn edge_screens_overlap(local: &Screen, remote: &Screen) -> bool {
    local.x < remote.x + remote.width
        && local.x + local.width > remote.x
        && local.y < remote.y + remote.height
        && local.y + local.height > remote.y
}

fn edge_near(a: i32, b: i32) -> bool {
    (a - b).abs() <= 80
}

fn edge_ranges_overlap(a_start: i32, a_end: i32, b_start: i32, b_end: i32) -> bool {
    i32::min(a_end, b_end) - i32::max(a_start, b_start) > 80
}

#[tauri::command]
fn sync_window_chrome(window: tauri::WebviewWindow, theme: String) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        apply_windows_window_chrome(&window, &theme)?;
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = window;
        let _ = theme;
    }

    Ok(())
}

#[tauri::command]
fn minimize_main_window(app: AppHandle) -> Result<(), String> {
    let window = app
        .get_webview_window("main")
        .ok_or_else(|| "main window is not available".to_string())?;

    #[cfg(target_os = "macos")]
    let result = macos_miniaturize_window(&window);

    #[cfg(not(target_os = "macos"))]
    let result = window
        .minimize()
        .map_err(|error| format!("failed to minimize main window: {error}"));

    if result.is_ok() {
        set_main_window_visible(&app, false);
        set_main_window_focused(&app, false);
    }

    result
}

#[tauri::command]
fn hide_main_window(app: AppHandle) -> Result<(), String> {
    hide_main_window_handle(&app)
}

#[tauri::command]
fn toggle_maximize_main_window(app: AppHandle) -> Result<(), String> {
    let window = app
        .get_webview_window("main")
        .ok_or_else(|| "main window is not available".to_string())?;
    let maximized = window
        .is_maximized()
        .map_err(|error| format!("failed to read main window state: {error}"))?;

    if maximized {
        window
            .unmaximize()
            .map_err(|error| format!("failed to restore main window: {error}"))
    } else {
        window
            .maximize()
            .map_err(|error| format!("failed to maximize main window: {error}"))
    }
}

#[tauri::command]
fn start_window_drag(app: AppHandle) -> Result<(), String> {
    app.get_webview_window("main")
        .ok_or_else(|| "main window is not available".to_string())?
        .start_dragging()
        .map_err(|error| format!("failed to start dragging main window: {error}"))
}

#[tauri::command]
fn read_clipboard_text() -> Result<String, String> {
    clipboard::read_text()
}

#[tauri::command]
fn write_clipboard_text(text: String) -> Result<(), String> {
    clipboard::write_text(&text)
}

#[tauri::command]
fn read_performance_sample(state: tauri::State<'_, AppRuntime>) -> PerformanceSample {
    performance::read_process_sample(
        state.transport_packets.load(Ordering::Relaxed),
        state.input_events.load(Ordering::Relaxed),
        state.clipboard_packets.load(Ordering::Relaxed),
    )
}

#[tauri::command]
fn set_app_upgrading(state: tauri::State<'_, AppRuntime>, enabled: bool) {
    state.upgrading.store(enabled, Ordering::Relaxed);
}

#[tauri::command]
async fn scan_lan_peers(state: tauri::State<'_, AppRuntime>) -> Result<DiscoveryStatus, String> {
    state.start_discovery()?;
    let layout = state
        .layout
        .lock()
        .map_err(|_| "layout state lock poisoned".to_string())?
        .clone();
    let mut local_peer = local_peer_from_layout(&layout);
    if let Some(transport) = state.quic_transport_handle() {
        apply_transport_to_peer(&mut local_peer, &transport);
    }
    let base_port = discovery_base_port(&layout);

    // scan_for_peers blocks for ~1.4s on UDP recv; run it on a blocking thread
    // so the async command doesn't freeze the webview UI.
    let discovered =
        tauri::async_runtime::spawn_blocking(move || scan_for_peers(&local_peer, base_port))
            .await
            .map_err(|e| format!("scan task failed: {e}"))??;

    for peer in discovered {
        merge_peer(&state.peers, peer);
    }
    prune_stale_peers(&state.peers);
    sync_layout_peer_presence(&state.layout, &state.peers);

    Ok(state.discovery_status())
}

#[tauri::command]
fn probe_lan_peer(host: String, state: tauri::State<'_, AppRuntime>) -> Result<LanPeer, String> {
    state.start_discovery()?;
    let layout = state
        .layout
        .lock()
        .map_err(|_| "layout state lock poisoned".to_string())?
        .clone();
    let mut local_peer = local_peer_from_layout(&layout);
    if let Some(transport) = state.quic_transport_handle() {
        apply_transport_to_peer(&mut local_peer, &transport);
    }
    let peer = probe_for_peer(&local_peer, &host, discovery_base_port(&layout))?;
    merge_peer(&state.peers, peer.clone());
    sync_layout_peer_presence(&state.layout, &state.peers);
    Ok(peer)
}

#[tauri::command]
fn request_lan_pairing(
    host: String,
    state: tauri::State<'_, AppRuntime>,
) -> Result<LanPeer, String> {
    state.start_discovery()?;
    let layout = state
        .layout
        .lock()
        .map_err(|_| "layout state lock poisoned".to_string())?
        .clone();
    if layout.machine_role != "server" {
        return Err("只有服务端可以发起配对。".into());
    }

    let mut local_peer = local_peer_from_layout(&layout);
    if let Some(transport) = state.quic_transport_handle() {
        apply_transport_to_peer(&mut local_peer, &transport);
    }
    let peer = match request_pairing_for_peer(&local_peer, &host, discovery_base_port(&layout)) {
        Ok(peer) => peer,
        Err(error) => {
            log::warn!("LAN pairing request failed host={host}: {error}");
            return Err(error);
        }
    };
    merge_peer(&state.peers, peer.clone());
    Ok(peer)
}

#[tauri::command]
fn confirm_lan_pairing(
    host: String,
    code: String,
    state: tauri::State<'_, AppRuntime>,
) -> Result<LanPeer, String> {
    state.start_discovery()?;
    let layout = state
        .layout
        .lock()
        .map_err(|_| "layout state lock poisoned".to_string())?
        .clone();
    if layout.machine_role != "server" {
        return Err("只有服务端可以确认配对。".into());
    }

    let mut local_peer = local_peer_from_layout(&layout);
    let transport = state
        .quic_transport_handle()
        .ok_or_else(|| "QUIC 传输未启动，无法安全确认配对。".to_string())?;
    apply_transport_to_peer(&mut local_peer, &transport);
    let peer = match confirm_pairing_for_peer(
        &local_peer,
        &transport,
        &layout.pair_secret,
        &host,
        &code,
        discovery_base_port(&layout),
    ) {
        Ok(peer) => peer,
        Err(error) => {
            log::warn!("LAN pairing confirm failed host={host}: {error}");
            return Err(error);
        }
    };
    merge_peer(&state.peers, peer.clone());
    sync_layout_peer_presence(&state.layout, &state.peers);
    Ok(peer)
}

#[tauri::command]
fn dismiss_pairing_request(state: tauri::State<'_, AppRuntime>) -> Result<RuntimeStatus, String> {
    {
        let mut challenge = state
            .pairing_challenge
            .lock()
            .map_err(|_| "pairing challenge lock poisoned".to_string())?;
        *challenge = None;
    }

    Ok(state.runtime_status())
}

/// Drop this machine's stored pairing trust so it can be paired afresh.
///
/// A client only accepts a new pairing handshake while `pairing_required`
/// (i.e. `paired_controllers` is empty — see `begin_pairing_challenge`), so a
/// stale pairing leaves it "already paired" with credentials the controller no
/// longer matches, and there is otherwise no way back without hand-editing
/// `layout.json`. Clearing the controllers here flips the client back to
/// "needs pairing" and re-announces, letting the server re-initiate.
#[tauri::command]
fn reset_pairing(state: tauri::State<'_, AppRuntime>) -> Result<AppStateSnapshot, String> {
    let updated_layout = {
        let mut layout = state
            .layout
            .lock()
            .map_err(|_| "layout state lock poisoned".to_string())?;
        layout.paired_controllers.clear();
        layout.clone()
    };
    write_layout_to_disk(&state.config_path, &updated_layout)?;

    if let Ok(mut challenge) = state.pairing_challenge.lock() {
        *challenge = None;
    }

    restart_runtime_if_running(&state)?;

    Ok(state.snapshot())
}

#[tauri::command]
fn set_autostart(app: AppHandle, enabled: bool) -> Result<bool, String> {
    use tauri_plugin_autostart::ManagerExt;
    let manager = app.autolaunch();
    if enabled {
        manager
            .enable()
            .map_err(|error| format!("failed to enable launch at startup: {error}"))?;
    } else {
        manager
            .disable()
            .map_err(|error| format!("failed to disable launch at startup: {error}"))?;
    }
    manager
        .is_enabled()
        .map_err(|error| format!("failed to read launch-at-startup state: {error}"))
}

#[tauri::command]
fn is_autostart_enabled(app: AppHandle) -> Result<bool, String> {
    use tauri_plugin_autostart::ManagerExt;
    app.autolaunch()
        .is_enabled()
        .map_err(|error| format!("failed to read launch-at-startup state: {error}"))
}

#[tauri::command]
fn open_repository_url() -> Result<(), String> {
    open_external_url(REPOSITORY_URL)
}

#[tauri::command]
fn open_releases_url() -> Result<(), String> {
    open_external_url(RELEASES_URL)
}

#[tauri::command]
fn is_portable_mode() -> Result<bool, String> {
    let exe_path =
        env::current_exe().map_err(|error| format!("failed to read current exe path: {error}"))?;
    Ok(exe_path
        .parent()
        .map(|directory| directory.join("portable.ini").is_file())
        .unwrap_or(false))
}

pub fn handle_process_control_args() -> bool {
    let args = env::args().collect::<Vec<_>>();
    if args.iter().any(|arg| arg == QUIT_EXISTING_ARG) {
        request_existing_instance_quit();
        return true;
    }

    #[cfg(target_os = "windows")]
    {
        if args.iter().any(|arg| arg == INSTALL_INPUT_SERVICE_ARG) {
            let helper_path = arg_value(&args, HELPER_PATH_ARG)
                .map(PathBuf::from)
                .or_else(|| resolve_input_helper_path().ok());
            match helper_path {
                Some(path) => {
                    if let Err(error) = install_windows_input_service(&path)
                        .and_then(|_| start_windows_input_service())
                    {
                        eprintln!("{error}");
                    }
                }
                None => eprintln!("failed to resolve mykvm-input-helper path"),
            }
            return true;
        }

        if args.iter().any(|arg| arg == UNINSTALL_INPUT_SERVICE_ARG) {
            if let Err(error) = uninstall_windows_input_service() {
                eprintln!("{error}");
            }
            return true;
        }
    }

    false
}

fn arg_value(args: &[String], key: &str) -> Option<String> {
    args.windows(2)
        .find_map(|window| (window[0] == key).then(|| window[1].clone()))
}

#[cfg(target_os = "windows")]
pub fn acquire_single_instance() -> bool {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, ERROR_ALREADY_EXISTS},
        System::Threading::CreateMutexW,
    };

    let mutex_name = wide_null(SINGLE_INSTANCE_MUTEX_NAME);
    let mutex = unsafe { CreateMutexW(std::ptr::null_mut(), 0, mutex_name.as_ptr()) };
    if mutex.is_null() {
        return true;
    }

    let already_exists =
        unsafe { windows_sys::Win32::Foundation::GetLastError() } == ERROR_ALREADY_EXISTS;
    if already_exists {
        unsafe {
            CloseHandle(mutex);
        }
        return false;
    }

    let guard = SINGLE_INSTANCE_MUTEX.get_or_init(|| Mutex::new(None));
    if let Ok(mut guard) = guard.lock() {
        *guard = Some(SingleInstanceGuard { mutex });
    }
    true
}

#[cfg(not(target_os = "windows"))]
pub fn acquire_single_instance() -> bool {
    true
}

#[cfg(target_os = "windows")]
fn release_single_instance() {
    use windows_sys::Win32::Foundation::CloseHandle;

    let Some(guard) = SINGLE_INSTANCE_MUTEX.get() else {
        return;
    };
    let Ok(mut guard) = guard.lock() else {
        return;
    };
    if let Some(guard) = guard.take() {
        unsafe {
            CloseHandle(guard.mutex);
        }
    }
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
#[cfg(not(target_os = "windows"))]
fn release_single_instance() {}

pub fn activate_existing_instance() -> bool {
    #[cfg(target_os = "windows")]
    {
        return signal_named_instance_event(ACTIVATE_INSTANCE_EVENT_NAME);
    }

    #[cfg(not(target_os = "windows"))]
    {
        false
    }
}

pub fn request_existing_instance_quit() -> bool {
    #[cfg(target_os = "windows")]
    {
        return signal_named_instance_event(QUIT_INSTANCE_EVENT_NAME);
    }

    #[cfg(not(target_os = "windows"))]
    {
        false
    }
}

#[cfg(target_os = "windows")]
fn signal_named_instance_event(name: &str) -> bool {
    use windows_sys::Win32::System::Threading::{OpenEventW, SetEvent, EVENT_MODIFY_STATE};

    let event_name = wide_null(name);
    for _ in 0..20 {
        let event = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, event_name.as_ptr()) };
        if !event.is_null() {
            unsafe {
                SetEvent(event);
                windows_sys::Win32::Foundation::CloseHandle(event);
            }
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }

    false
}

#[cfg(target_os = "windows")]
fn setup_single_instance_events(app: AppHandle) {
    spawn_instance_event_listener(
        ACTIVATE_INSTANCE_EVENT_NAME,
        app.clone(),
        InstanceEvent::Activate,
    );
    spawn_instance_event_listener(QUIT_INSTANCE_EVENT_NAME, app, InstanceEvent::Quit);
}

#[cfg(not(target_os = "windows"))]
fn setup_single_instance_events(app: AppHandle) {
    let _ = app;
}

#[cfg(target_os = "windows")]
#[derive(Clone, Copy)]
enum InstanceEvent {
    Activate,
    Quit,
}

#[cfg(target_os = "windows")]
fn spawn_instance_event_listener(name: &str, app: AppHandle, event_kind: InstanceEvent) {
    use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject, INFINITE};

    let event_name = wide_null(name);
    let event = unsafe { CreateEventW(std::ptr::null_mut(), 0, 0, event_name.as_ptr()) };
    if event.is_null() {
        log::warn!("failed to create instance event {name}");
        return;
    }

    let event = SendHandle(event);
    thread::spawn(move || loop {
        let result = unsafe { WaitForSingleObject(event.raw(), INFINITE) };
        if result != 0 {
            break;
        }

        match event_kind {
            InstanceEvent::Activate => {
                let handle = app.clone();
                let _ = app.run_on_main_thread(move || {
                    let _ = show_main_window_handle(&handle);
                });
            }
            InstanceEvent::Quit => {
                request_app_quit(&app);
                break;
            }
        }
    });
}

fn request_app_quit(app: &AppHandle) {
    mark_explicit_quit(app);
    app.exit(0);
}

fn mark_explicit_quit(app: &AppHandle) {
    if let Some(state) = app.try_state::<AppRuntime>() {
        state.allow_explicit_quit.store(true, Ordering::Relaxed);
    }
}

fn should_allow_app_exit(app: &AppHandle, code: Option<i32>) -> bool {
    let explicit_quit = app
        .try_state::<AppRuntime>()
        .map(|state| state.allow_explicit_quit.swap(false, Ordering::Relaxed))
        .unwrap_or(false);
    should_allow_app_exit_request(code, explicit_quit)
}

fn should_allow_app_exit_request(code: Option<i32>, explicit_quit: bool) -> bool {
    code == Some(tauri::RESTART_EXIT_CODE) || explicit_quit
}

fn launched_from_autostart() -> bool {
    args_contain_autostart(env::args())
}

fn args_contain_autostart<I, S>(args: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    args.into_iter().any(|arg| arg.as_ref() == AUTOSTART_ARG)
}

#[cfg(target_os = "macos")]
fn macos_miniaturize_window(window: &tauri::WebviewWindow) -> Result<(), String> {
    use std::ffi::c_void;
    use std::os::raw::c_char;

    #[link(name = "objc")]
    extern "C" {
        fn sel_registerName(name: *const c_char) -> *mut c_void;
        fn objc_msgSend();
    }

    let ns_window = window
        .ns_window()
        .map_err(|error| format!("failed to resolve NSWindow: {error}"))?;
    if ns_window.is_null() {
        return Err("main NSWindow is null".into());
    }

    unsafe {
        let miniaturize_sel = sel_registerName(b"miniaturize:\0".as_ptr() as *const c_char);
        let msg_id_arg: extern "C" fn(*mut c_void, *mut c_void, *mut c_void) =
            std::mem::transmute(objc_msgSend as *const ());
        msg_id_arg(ns_window, miniaturize_sel, ns_window);
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn macos_order_front_window(window: &tauri::WebviewWindow) -> Result<(), String> {
    use std::ffi::c_void;
    use std::os::raw::c_char;

    #[link(name = "objc")]
    extern "C" {
        fn objc_getClass(name: *const c_char) -> *mut c_void;
        fn sel_registerName(name: *const c_char) -> *mut c_void;
        fn objc_msgSend();
    }

    let ns_window = window
        .ns_window()
        .map_err(|error| format!("failed to resolve NSWindow: {error}"))?;
    if ns_window.is_null() {
        return Err("main NSWindow is null".into());
    }

    unsafe {
        let app_class = objc_getClass(b"NSApplication\0".as_ptr() as *const c_char);
        if !app_class.is_null() {
            let shared_sel = sel_registerName(b"sharedApplication\0".as_ptr() as *const c_char);
            let activate_sel =
                sel_registerName(b"activateIgnoringOtherApps:\0".as_ptr() as *const c_char);
            let msg_id: extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void =
                std::mem::transmute(objc_msgSend as *const ());
            let ns_app = msg_id(app_class, shared_sel);
            if !ns_app.is_null() {
                let msg_bool: extern "C" fn(*mut c_void, *mut c_void, i8) =
                    std::mem::transmute(objc_msgSend as *const ());
                msg_bool(ns_app, activate_sel, 1);
            }
        }

        let make_key_sel = sel_registerName(b"makeKeyAndOrderFront:\0".as_ptr() as *const c_char);
        let msg_id_arg: extern "C" fn(*mut c_void, *mut c_void, *mut c_void) =
            std::mem::transmute(objc_msgSend as *const ());
        msg_id_arg(ns_window, make_key_sel, std::ptr::null_mut());

        let order_front_sel = sel_registerName(b"orderFrontRegardless\0".as_ptr() as *const c_char);
        let msg_void: extern "C" fn(*mut c_void, *mut c_void) =
            std::mem::transmute(objc_msgSend as *const ());
        msg_void(ns_window, order_front_sel);
    }

    Ok(())
}

#[cfg(target_os = "macos")]
fn macos_set_main_webview_cursor_hidden(app: &AppHandle, hidden: bool) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    let script = if hidden {
        "document.documentElement.dataset.remoteInputActive = 'true';"
    } else {
        "delete document.documentElement.dataset.remoteInputActive;"
    };
    let _ = window.eval(script);
}

#[cfg(target_os = "macos")]
fn setup_macos_cursor_hider(app: &tauri::App) {
    // Only mirror remote-input state onto the webview DOM (a CSS `cursor:none`
    // toggle that matters only while the window is visible). The actual pointer
    // hide/show is driven synchronously from the input-capture thread in
    // input.rs (CGDisplayHideCursor + NSCursor hide), so do NOT also call
    // NSCursor here: `run_on_main_thread` lands on the main run loop, which
    // macOS de-prioritizes once the window is hidden/minimized, so a hide/unhide
    // posted here can sit in the queue for ~1s and then race the capture thread's
    // synchronous calls — the "cursor hides a second late, sometimes instantly"
    // stutter. Leaving only the DOM mirror keeps that path free of cursor work.
    let remote_active = app.state::<AppRuntime>().remote_input_active.clone();
    let app_handle = app.handle().clone();
    thread::spawn(move || {
        let mut was_active = false;
        loop {
            thread::sleep(Duration::from_millis(8));
            let active = remote_active.load(Ordering::Relaxed);
            if active == was_active {
                continue;
            }
            was_active = active;
            let handle = app_handle.clone();
            let _ = app_handle.run_on_main_thread(move || {
                macos_set_main_webview_cursor_hidden(&handle, active);
            });
        }
    });
}

#[cfg(target_os = "macos")]
fn setup_macos_window_visibility_watcher(app: &tauri::App) {
    let app_handle = app.handle().clone();
    thread::spawn(move || {
        let mut last_visible = true;
        loop {
            thread::sleep(Duration::from_millis(100));
            let visible = app_handle
                .get_webview_window("main")
                .and_then(|window| {
                    let visible = window.is_visible().ok()?;
                    let minimized = window.is_minimized().ok()?;
                    Some(visible && !minimized)
                })
                .unwrap_or(false);

            if visible == last_visible {
                continue;
            }
            last_visible = visible;
            set_main_window_visible(&app_handle, visible);
        }
    });
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec![AUTOSTART_ARG]),
        ))
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, shortcut, event| {
                    if event.state == ShortcutState::Pressed {
                        if let Err(error) = route_global_shortcut(app, shortcut) {
                            log::warn!("global shortcut failed: {error}");
                        }
                    }
                })
                .build(),
        )
        .on_window_event(|window, event| {
            if handle_edge_drop_window_event(window, event) {
                return;
            }
            if handle_file_drop_landing_window_event(window, event) {
                return;
            }
            if window.label() == "main" {
                if let WindowEvent::Focused(focused) = event {
                    set_main_window_focused(window.app_handle(), *focused);
                }
                if let WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    let _ = hide_main_window_handle(window.app_handle());
                }
            }
        })
        .setup(|app| {
            let silent_launch = launched_from_autostart();
            if let Err(error) = app
                .handle()
                .plugin(tauri_plugin_updater::Builder::new().build())
            {
                eprintln!("failed to initialize updater plugin: {error}");
            }
            app.handle().plugin(
                tauri_plugin_log::Builder::default()
                    .level(log::LevelFilter::Info)
                    .max_file_size(LOG_MAX_FILE_SIZE_BYTES)
                    .rotation_strategy(tauri_plugin_log::RotationStrategy::KeepSome(5))
                    .build(),
            )?;
            if let Ok(log_dir) = app.path().app_log_dir() {
                log::info!("file logging enabled at {}", log_dir.display());
            }

            let config_dir = app
                .path()
                .app_config_dir()
                .map_err(|error| format!("failed to resolve app config dir: {error}"))?;
            fs::create_dir_all(&config_dir).map_err(|error| {
                format!(
                    "failed to create app config dir {}: {error}",
                    config_dir.display()
                )
            })?;

            let detected_layout = detect_local_layout(app.handle());
            let runtime = AppRuntime::new(
                app.handle().clone(),
                config_dir.join("layout.json"),
                detected_layout,
            );
            app.manage(runtime);

            // Eagerly start discovery + input BEFORE the WebView2/frontend is
            // ready. The old flow waited for the frontend to call
            // `start_runtime`, which only happens after WebView2 initializes
            // (3-5 s on Windows). That window is exactly the "admin-restart
            // dead time" where the peer can't see us. Starting discovery here
            // binds the UDP socket and begins announcing within ~1 s of process
            // launch, so the peer picks us back up in one announce cycle.
            {
                let state = app.state::<AppRuntime>();
                let runtime_ref = state.inner();
                let layout = runtime_ref.layout_snapshot();
                let _ = runtime_ref.start_discovery();
                let (capture, inject) = runtime_ref.start_input(layout.clone());
                let clipboard = runtime_ref.start_clipboard(layout.clone());
                let discovery = runtime_ref.discovery_status_for_layout(&layout);
                let pairing = runtime_ref.pairing_status_for_layout(&layout);
                let privilege = current_privilege_status();
                let input_service = current_input_service_status();
                let transport = ready_transport_status(&discovery);
                if let Ok(mut runtime) = runtime_ref.runtime.lock() {
                    *runtime = RuntimeStatus {
                        started: true,
                        transport,
                        capture,
                        inject,
                        clipboard,
                        discovery,
                        pairing,
                        privilege,
                        input_service,
                    };
                }
            }

            #[cfg(target_os = "macos")]
            setup_macos_cursor_hider(app);
            #[cfg(target_os = "macos")]
            setup_macos_window_visibility_watcher(app);
            if EDGE_DROP_WINDOWS_ENABLED {
                start_edge_drop_window_sync(app.handle().clone());
            }
            setup_tray(app)?;
            if let Err(error) = sync_runtime_toggle_shortcut(app.handle()) {
                log::warn!("failed to register quick start/stop shortcut: {error}");
            }
            if let Err(error) = sync_screen_switch_shortcuts(app.handle()) {
                log::warn!("failed to register screen switch shortcuts: {error}");
            }
            #[cfg(target_os = "windows")]
            apply_custom_chrome(app.handle())?;
            setup_single_instance_events(app.handle().clone());
            if silent_launch {
                hide_main_window_handle(app.handle())?;
            } else {
                show_main_window_handle(app.handle())?;
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            load_app_state,
            read_runtime_status,
            read_diagnostic_info,
            open_log_directory,
            save_layout,
            start_runtime,
            stop_runtime,
            read_clipboard_text,
            write_clipboard_text,
            read_performance_sample,
            set_app_upgrading,
            scan_lan_peers,
            probe_lan_peer,
            request_lan_pairing,
            confirm_lan_pairing,
            dismiss_pairing_request,
            reset_pairing,
            set_autostart,
            is_autostart_enabled,
            restart_as_admin,
            read_input_service_status,
            install_input_service,
            uninstall_input_service,
            send_secure_attention,
            send_files_to_device,
            sync_window_chrome,
            minimize_main_window,
            hide_main_window,
            toggle_maximize_main_window,
            start_window_drag,
            open_repository_url,
            open_releases_url,
            is_portable_mode
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| match event {
            tauri::RunEvent::ExitRequested { code, api, .. } => {
                if !should_allow_app_exit(app, code) {
                    api.prevent_exit();
                    let _ = hide_main_window_handle(app);
                }
            }
            #[cfg(target_os = "macos")]
            tauri::RunEvent::Reopen {
                has_visible_windows: false,
                ..
            } => {
                let _ = show_main_window_handle(app);
            }
            _ => {}
        });
}

fn setup_tray(app: &tauri::App) -> tauri::Result<()> {
    let runtime_started = app
        .try_state::<AppRuntime>()
        .map(|state| state.runtime_status().started)
        .unwrap_or(false);
    let show_item = MenuItem::with_id(app, "show", "Show mykvm", true, None::<&str>)?;
    let runtime_toggle_item = MenuItem::with_id(
        app,
        "runtime-toggle",
        runtime_toggle_menu_label(runtime_started),
        true,
        None::<&str>,
    )?;
    let hide_item = MenuItem::with_id(app, "hide", "Hide to tray", true, None::<&str>)?;
    let quit_item = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[&show_item, &runtime_toggle_item, &hide_item, &quit_item],
    )?;

    if let Some(state) = app.try_state::<AppRuntime>() {
        if let Ok(mut item) = state.runtime_toggle_menu_item.lock() {
            *item = Some(runtime_toggle_item);
        }
    }

    let mut tray = TrayIconBuilder::with_id("main")
        .menu(&menu)
        .tooltip(runtime_tray_tooltip(runtime_started))
        .show_menu_on_left_click(true)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "show" => {
                let _ = show_main_window_handle(app);
            }
            "runtime-toggle" => {
                if let Err(error) = toggle_runtime_from_app(app) {
                    log::warn!("quick start/stop tray action failed: {error}");
                }
            }
            "hide" => {
                let _ = hide_main_window_handle(app);
            }
            "quit" => request_app_quit(app),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            let should_show = matches!(
                event,
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } | TrayIconEvent::DoubleClick {
                    button: MouseButton::Left,
                    ..
                }
            );

            if should_show {
                let _ = show_main_window_handle(tray.app_handle());
            }
        });

    if let Some(icon) = app.default_window_icon().cloned() {
        tray = tray.icon(icon);
    }

    tray.build(app)?;
    Ok(())
}

fn show_main_window_handle(app: &AppHandle) -> Result<(), String> {
    let window = ensure_main_window(app)?;
    window
        .show()
        .map_err(|error| format!("failed to show main window: {error}"))?;
    window
        .unminimize()
        .map_err(|error| format!("failed to restore main window: {error}"))?;
    #[cfg(target_os = "macos")]
    macos_order_front_window(&window)?;
    set_main_window_visible(app, true);
    window
        .set_focus()
        .map_err(|error| format!("failed to focus main window: {error}"))?;
    Ok(())
}

fn hide_main_window_handle(app: &AppHandle) -> Result<(), String> {
    destroy_main_window_handle(app)
}

fn destroy_main_window_handle(app: &AppHandle) -> Result<(), String> {
    let Some(window) = app.get_webview_window("main") else {
        set_main_window_visible(app, false);
        set_main_window_focused(app, false);
        return Ok(());
    };
    let result = window
        .destroy()
        .map_err(|error| format!("failed to destroy main window: {error}"));

    if result.is_ok() {
        set_main_window_visible(app, false);
        set_main_window_focused(app, false);
    }

    result
}

fn ensure_main_window(app: &AppHandle) -> Result<tauri::WebviewWindow, String> {
    if let Some(window) = app.get_webview_window("main") {
        return Ok(window);
    }

    let window = WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
        .title("MyKVM")
        .inner_size(1480.0, 960.0)
        .min_inner_size(1200.0, 760.0)
        .resizable(true)
        .theme(Some(tauri::Theme::Dark))
        .visible(false)
        .build()
        .map_err(|error| format!("failed to create main window: {error}"))?;

    #[cfg(target_os = "windows")]
    {
        window
            .set_decorations(false)
            .map_err(|error| format!("failed to apply main window chrome: {error}"))?;
        apply_windows_window_chrome(&window, "dark")?;
    }

    Ok(window)
}

fn set_main_window_visible(app: &AppHandle, visible: bool) {
    if let Some(state) = app.try_state::<AppRuntime>() {
        state.main_window_visible.store(visible, Ordering::Relaxed);
    }
}

fn set_main_window_focused(app: &AppHandle, focused: bool) {
    if let Some(state) = app.try_state::<AppRuntime>() {
        state.main_window_focused.store(focused, Ordering::Relaxed);
    }
}

#[cfg(target_os = "windows")]
fn apply_custom_chrome(app: &AppHandle) -> tauri::Result<()> {
    if let Some(window) = app.get_webview_window("main") {
        window.set_decorations(false)?;
    }

    Ok(())
}

fn open_external_url(url: &str) -> Result<(), String> {
    let mut command = if cfg!(target_os = "macos") {
        let mut command = Command::new("open");
        command.arg(url);
        command
    } else if cfg!(target_os = "windows") {
        let mut command = Command::new("cmd");
        command.args(["/C", "start", "", url]);
        command
    } else {
        let mut command = Command::new("xdg-open");
        command.arg(url);
        command
    };

    command
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("failed to open URL: {error}"))
}

fn open_external_path(path: &PathBuf) -> Result<(), String> {
    let mut command = if cfg!(target_os = "macos") {
        let mut command = Command::new("open");
        command.arg(path);
        command
    } else if cfg!(target_os = "windows") {
        let mut command = Command::new("explorer");
        command.arg(path);
        command
    } else {
        let mut command = Command::new("xdg-open");
        command.arg(path);
        command
    };

    command
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("failed to open path {}: {error}", path.display()))
}

fn load_layout_from_disk(path: &PathBuf) -> Option<LayoutState> {
    let contents = fs::read_to_string(path).ok()?;
    serde_json::from_str::<LayoutState>(&contents).ok()
}

fn write_layout_to_disk(path: &PathBuf, layout: &LayoutState) -> Result<(), String> {
    let json = serde_json::to_string_pretty(layout)
        .map_err(|error| format!("failed to serialize layout: {error}"))?;

    fs::write(path, json)
        .map_err(|error| format!("failed to write layout file {}: {error}", path.display()))
}

fn default_runtime(layout: &LayoutState) -> RuntimeStatus {
    RuntimeStatus {
        started: false,
        transport: NativeStageStatus {
            state: "stubbed".into(),
            detail: "Runtime is stopped. Start it to enable LAN discovery and shared input.".into(),
        },
        capture: NativeStageStatus {
            state: "stubbed".into(),
            detail: input::stopped_capture_status().detail,
        },
        inject: NativeStageStatus {
            state: "stubbed".into(),
            detail: input::stopped_inject_status().detail,
        },
        clipboard: if layout.clipboard_sync {
            NativeStageStatus {
                state: "idle".into(),
                detail: "剪贴板同步已开启，启动共享服务后会开始同步。".into(),
            }
        } else {
            clipboard_disabled_status()
        },
        privilege: current_privilege_status(),
        input_service: current_input_service_status(),
        discovery: DiscoveryStatus {
            state: "idle".into(),
            detail: "LAN discovery is stopped. Start runtime or scan the LAN to find peers.".into(),
            port: layout.transport_port,
            local_peer: local_peer_from_layout(layout),
            peers: Vec::new(),
        },
        pairing: idle_pairing_status(),
    }
}

fn idle_pairing_status() -> PairingStatus {
    PairingStatus {
        state: "idle".into(),
        code: String::new(),
        requester_name: String::new(),
        requester_ip: String::new(),
        expires_at_ms: 0,
        detail: String::new(),
    }
}

#[cfg(target_os = "windows")]
fn current_privilege_status() -> PrivilegeStatus {
    let is_elevated = is_windows_process_elevated().unwrap_or(false);

    let detail = if is_elevated {
        "Running as administrator. MyKVM can inject input into elevated desktop windows."
    } else {
        "Standard user mode. Restart as administrator to control elevated desktop windows."
    };

    PrivilegeStatus {
        is_elevated,
        can_elevate: !is_elevated,
        detail: detail.into(),
    }
}

#[cfg(not(target_os = "windows"))]
fn current_privilege_status() -> PrivilegeStatus {
    PrivilegeStatus {
        is_elevated: false,
        can_elevate: false,
        detail: "Administrator elevation is only needed on Windows for elevated desktop windows."
            .into(),
    }
}

#[cfg(target_os = "windows")]
fn current_input_service_status() -> InputServiceStatus {
    match query_windows_input_service_status() {
        Ok(status) => status,
        Err(error) => InputServiceStatus {
            installed: false,
            running: false,
            worker_session_id: None,
            pipe_available: false,
            sas_available: false,
            detail: error,
        },
    }
}

#[cfg(not(target_os = "windows"))]
fn current_input_service_status() -> InputServiceStatus {
    InputServiceStatus {
        installed: false,
        running: false,
        worker_session_id: None,
        pipe_available: false,
        sas_available: false,
        detail: "Windows lock-screen input service is only available on Windows.".into(),
    }
}

#[cfg(target_os = "windows")]
fn query_windows_input_service_status() -> Result<InputServiceStatus, String> {
    use windows_sys::Win32::{
        Foundation::{GetLastError, ERROR_SERVICE_DOES_NOT_EXIST, ERROR_SERVICE_MARKED_FOR_DELETE},
        System::{
            RemoteDesktop::WTSGetActiveConsoleSessionId,
            Services::{
                OpenSCManagerW, OpenServiceW, QueryServiceStatusEx, SC_MANAGER_CONNECT,
                SC_STATUS_PROCESS_INFO, SERVICE_QUERY_STATUS, SERVICE_RUNNING,
                SERVICE_STATUS_PROCESS,
            },
        },
    };

    unsafe {
        let scm = OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_CONNECT);
        if scm.is_null() {
            return Err(windows_last_error("OpenSCManagerW"));
        }
        let _scm = ServiceHandleGuard(scm);

        let service_name = wide_null(shared_input::INPUT_SERVICE_NAME);
        let service = OpenServiceW(scm, service_name.as_ptr(), SERVICE_QUERY_STATUS);
        if service.is_null() {
            let code = GetLastError();
            if code == ERROR_SERVICE_DOES_NOT_EXIST || code == ERROR_SERVICE_MARKED_FOR_DELETE {
                return Ok(InputServiceStatus {
                    installed: false,
                    running: false,
                    worker_session_id: None,
                    pipe_available: false,
                    sas_available: false,
                    detail: "Lock-screen input service is not installed.".into(),
                });
            }
            return Err(windows_last_error("OpenServiceW"));
        }
        let _service = ServiceHandleGuard(service);

        let service_status = query_service_status_process(service)?;
        let running = service_status.dwCurrentState == SERVICE_RUNNING;
        let pipe_available = running && input::windows_input_pipe_available();
        let active_session = WTSGetActiveConsoleSessionId();
        let worker_session_id = (running && active_session != u32::MAX).then_some(active_session);
        let sas_available = running && sas_dll_available() && software_sas_allows_services();
        let detail = if running {
            if pipe_available {
                "Lock-screen input service is running and the worker pipe is available."
            } else {
                "Lock-screen input service is running; waiting for the session worker pipe."
            }
        } else {
            "Lock-screen input service is installed but not running."
        };

        return Ok(InputServiceStatus {
            installed: true,
            running,
            worker_session_id,
            pipe_available,
            sas_available,
            detail: detail.into(),
        });
    }

    unsafe fn query_service_status_process(
        service: windows_sys::Win32::System::Services::SC_HANDLE,
    ) -> Result<SERVICE_STATUS_PROCESS, String> {
        let mut status = SERVICE_STATUS_PROCESS::default();
        let mut needed = 0_u32;
        let ok = QueryServiceStatusEx(
            service,
            SC_STATUS_PROCESS_INFO,
            &mut status as *mut SERVICE_STATUS_PROCESS as *mut u8,
            std::mem::size_of::<SERVICE_STATUS_PROCESS>() as u32,
            &mut needed,
        ) != 0;
        if ok {
            Ok(status)
        } else {
            Err(windows_last_error("QueryServiceStatusEx"))
        }
    }
}

#[cfg(target_os = "windows")]
fn install_windows_input_service(helper_path: &PathBuf) -> Result<(), String> {
    use windows_sys::Win32::{
        Foundation::{GetLastError, ERROR_SERVICE_EXISTS},
        System::Services::{
            ChangeServiceConfigW, CreateServiceW, OpenSCManagerW, OpenServiceW, SC_MANAGER_CONNECT,
            SC_MANAGER_CREATE_SERVICE, SERVICE_ALL_ACCESS, SERVICE_AUTO_START,
            SERVICE_ERROR_NORMAL, SERVICE_WIN32_OWN_PROCESS,
        },
    };

    if !helper_path.is_file() {
        return Err(format!(
            "input helper binary does not exist: {}",
            helper_path.display()
        ));
    }

    unsafe {
        let scm = OpenSCManagerW(
            std::ptr::null(),
            std::ptr::null(),
            SC_MANAGER_CONNECT | SC_MANAGER_CREATE_SERVICE,
        );
        if scm.is_null() {
            return Err(windows_last_error("OpenSCManagerW"));
        }
        let _scm = ServiceHandleGuard(scm);

        let service_name = wide_null(shared_input::INPUT_SERVICE_NAME);
        let display_name = wide_null(shared_input::INPUT_SERVICE_DISPLAY_NAME);
        let binary = wide_null(&format!(
            "{} --service",
            quote_windows_arg_str(&helper_path.to_string_lossy())
        ));
        let mut service = CreateServiceW(
            scm,
            service_name.as_ptr(),
            display_name.as_ptr(),
            SERVICE_ALL_ACCESS,
            SERVICE_WIN32_OWN_PROCESS,
            SERVICE_AUTO_START,
            SERVICE_ERROR_NORMAL,
            binary.as_ptr(),
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
        );

        if service.is_null() {
            let code = GetLastError();
            if code != ERROR_SERVICE_EXISTS {
                return Err(windows_last_error("CreateServiceW"));
            }
            service = OpenServiceW(scm, service_name.as_ptr(), SERVICE_ALL_ACCESS);
            if service.is_null() {
                return Err(windows_last_error("OpenServiceW(existing)"));
            }
            if ChangeServiceConfigW(
                service,
                SERVICE_WIN32_OWN_PROCESS,
                SERVICE_AUTO_START,
                SERVICE_ERROR_NORMAL,
                binary.as_ptr(),
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                display_name.as_ptr(),
            ) == 0
            {
                let _service = ServiceHandleGuard(service);
                return Err(windows_last_error("ChangeServiceConfigW"));
            }
        }

        let _service = ServiceHandleGuard(service);

        // Let the logged-in (Authenticated) user stop/start this LocalSystem
        // service, so the per-user (non-elevated) updater can restart it during
        // upgrades without a UAC prompt — important for an unattended client.
        // SYSTEM and Administrators keep full control; AU only gains
        // start/stop/query. Best-effort: failure just leaves the default DACL.
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let sddl = "D:(A;;CCLCSWRPWPDTLOCRRC;;;SY)(A;;CCDCLCSWRPWPDTLOCRSDRCWDWO;;;BA)(A;;CCLCSWRPWPLORC;;;AU)S:(AU;FA;CCDCLCSWRPWPDTLOCRSDRCWDWO;;;WD)";
        let _ = std::process::Command::new("sc.exe")
            .args(["sdset", shared_input::INPUT_SERVICE_NAME, sddl])
            .creation_flags(CREATE_NO_WINDOW)
            .output();

        Ok(())
    }
}

#[cfg(target_os = "windows")]
fn start_windows_input_service() -> Result<(), String> {
    use windows_sys::Win32::{
        Foundation::{GetLastError, ERROR_SERVICE_ALREADY_RUNNING},
        System::Services::{
            OpenSCManagerW, OpenServiceW, StartServiceW, SC_MANAGER_CONNECT, SERVICE_QUERY_STATUS,
            SERVICE_START,
        },
    };

    unsafe {
        let scm = OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_CONNECT);
        if scm.is_null() {
            return Err(windows_last_error("OpenSCManagerW"));
        }
        let _scm = ServiceHandleGuard(scm);
        let service_name = wide_null(shared_input::INPUT_SERVICE_NAME);
        let service = OpenServiceW(
            scm,
            service_name.as_ptr(),
            SERVICE_START | SERVICE_QUERY_STATUS,
        );
        if service.is_null() {
            return Err(windows_last_error("OpenServiceW(start)"));
        }
        let _service = ServiceHandleGuard(service);
        if StartServiceW(service, 0, std::ptr::null()) == 0 {
            let code = GetLastError();
            if code != ERROR_SERVICE_ALREADY_RUNNING {
                return Err(windows_last_error("StartServiceW"));
            }
        }
        Ok(())
    }
}

#[cfg(target_os = "windows")]
fn uninstall_windows_input_service() -> Result<(), String> {
    use windows_sys::Win32::{
        Foundation::{GetLastError, ERROR_SERVICE_DOES_NOT_EXIST},
        Storage::FileSystem::DELETE,
        System::Services::{
            ControlService, DeleteService, OpenSCManagerW, OpenServiceW, SC_MANAGER_CONNECT,
            SERVICE_CONTROL_STOP, SERVICE_QUERY_STATUS, SERVICE_STATUS, SERVICE_STOP,
        },
    };

    unsafe {
        let scm = OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_CONNECT);
        if scm.is_null() {
            return Err(windows_last_error("OpenSCManagerW"));
        }
        let _scm = ServiceHandleGuard(scm);
        let service_name = wide_null(shared_input::INPUT_SERVICE_NAME);
        let service = OpenServiceW(
            scm,
            service_name.as_ptr(),
            SERVICE_STOP | SERVICE_QUERY_STATUS | DELETE,
        );
        if service.is_null() {
            let code = GetLastError();
            if code == ERROR_SERVICE_DOES_NOT_EXIST {
                return Ok(());
            }
            return Err(windows_last_error("OpenServiceW(uninstall)"));
        }
        let _service = ServiceHandleGuard(service);

        let mut stop_status = SERVICE_STATUS::default();
        let _ = ControlService(service, SERVICE_CONTROL_STOP, &mut stop_status);

        if DeleteService(service) == 0 {
            return Err(windows_last_error("DeleteService"));
        }
        return Ok(());
    }
}

#[cfg(target_os = "windows")]
fn resolve_input_helper_path() -> Result<PathBuf, String> {
    let exe =
        env::current_exe().map_err(|error| format!("failed to locate current exe: {error}"))?;
    let exe_dir = exe
        .parent()
        .map(PathBuf::from)
        .ok_or_else(|| "current exe has no parent directory".to_string())?;
    let candidates = [
        exe_dir.join("mykvm-input-helper.exe"),
        exe_dir.join("mykvm-input-helper-x86_64-pc-windows-msvc.exe"),
        exe_dir
            .join("resources")
            .join("mykvm-input-helper-x86_64-pc-windows-msvc.exe"),
        exe_dir.join("resources").join("mykvm-input-helper.exe"),
    ];

    candidates
        .iter()
        .find(|path| path.is_file())
        .cloned()
        .or_else(|| candidates.first().cloned())
        .ok_or_else(|| "failed to build input helper path candidates".into())
}

#[cfg(target_os = "windows")]
struct ServiceHandleGuard(windows_sys::Win32::System::Services::SC_HANDLE);

#[cfg(target_os = "windows")]
impl Drop for ServiceHandleGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = windows_sys::Win32::System::Services::CloseServiceHandle(self.0);
            }
        }
    }
}

#[cfg(target_os = "windows")]
fn software_sas_allows_services() -> bool {
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY_LOCAL_MACHINE, KEY_READ, REG_DWORD,
    };

    unsafe {
        let subkey = wide_null(r"SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System");
        let mut key = std::ptr::null_mut();
        if RegOpenKeyExW(HKEY_LOCAL_MACHINE, subkey.as_ptr(), 0, KEY_READ, &mut key) != 0 {
            return false;
        }
        let _key = RegistryKeyGuard(key);

        let value_name = wide_null("SoftwareSASGeneration");
        let mut value_type = 0_u32;
        let mut value = 0_u32;
        let mut value_len = std::mem::size_of::<u32>() as u32;
        let ok = RegQueryValueExW(
            key,
            value_name.as_ptr(),
            std::ptr::null(),
            &mut value_type,
            &mut value as *mut u32 as *mut u8,
            &mut value_len,
        ) == 0;
        return ok && value_type == REG_DWORD && matches!(value, 1 | 3);
    }

    struct RegistryKeyGuard(windows_sys::Win32::System::Registry::HKEY);
    impl Drop for RegistryKeyGuard {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe {
                    let _ = RegCloseKey(self.0);
                }
            }
        }
    }
}

#[cfg(target_os = "windows")]
fn sas_dll_available() -> bool {
    use windows_sys::Win32::{
        Foundation::FreeLibrary,
        System::LibraryLoader::{GetProcAddress, LoadLibraryW},
    };

    unsafe {
        let dll = LoadLibraryW(wide_null("sas.dll").as_ptr());
        if dll.is_null() {
            return false;
        }
        let available = GetProcAddress(dll, c"SendSAS".as_ptr() as *const u8).is_some();
        let _ = FreeLibrary(dll);
        available
    }
}

#[cfg(target_os = "windows")]
fn quote_windows_arg_str(value: &str) -> String {
    let mut quoted = String::from("\"");
    for ch in value.chars() {
        if ch == '"' {
            quoted.push('\\');
        }
        quoted.push(ch);
    }
    quoted.push('"');
    quoted
}

#[cfg(target_os = "windows")]
fn windows_last_error(context: &str) -> String {
    let code = unsafe { windows_sys::Win32::Foundation::GetLastError() };
    format!("{context} failed with Windows error {code}")
}

#[cfg(target_os = "windows")]
fn is_windows_process_elevated() -> Result<bool, String> {
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY},
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };

    unsafe {
        let mut token = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err("failed to open current process token".into());
        }

        let mut elevation = TOKEN_ELEVATION::default();
        let mut return_length = 0;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            &mut elevation as *mut TOKEN_ELEVATION as *mut _,
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut return_length,
        );
        let _ = CloseHandle(token);

        if ok == 0 {
            return Err("failed to read process elevation token".into());
        }

        Ok(elevation.TokenIsElevated != 0)
    }
}

#[cfg(target_os = "windows")]
fn restart_current_process_as_admin() -> Result<(), String> {
    launch_current_process_as_admin(&[])
}

#[cfg(target_os = "windows")]
fn launch_current_process_as_admin(args: &[String]) -> Result<(), String> {
    use windows_sys::Win32::{UI::Shell::ShellExecuteW, UI::WindowsAndMessaging::SW_SHOWNORMAL};

    let exe =
        env::current_exe().map_err(|error| format!("failed to locate current exe: {error}"))?;
    let operation = wide_null("runas");
    let file = wide_null(&exe.to_string_lossy());
    let params = args
        .iter()
        .map(|arg| quote_windows_arg_str(arg))
        .collect::<Vec<_>>()
        .join(" ");
    let params_w = wide_null(&params);
    let params_ptr = if params.is_empty() {
        std::ptr::null()
    } else {
        params_w.as_ptr()
    };
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            operation.as_ptr(),
            file.as_ptr(),
            params_ptr,
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };

    if (result as isize) <= 32 {
        return Err("administrator restart was cancelled or blocked by Windows".into());
    }

    Ok(())
}

#[cfg(target_os = "windows")]
fn apply_windows_window_chrome(window: &tauri::WebviewWindow, theme: &str) -> Result<(), String> {
    use std::ffi::c_void;
    use windows_sys::Win32::{
        Foundation::HWND,
        Graphics::Dwm::{
            DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_CAPTION_COLOR, DWMWA_TEXT_COLOR,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
        },
    };

    let hwnd = window
        .hwnd()
        .map_err(|error| format!("failed to resolve native window handle: {error}"))?
        .0 as HWND;
    let is_dark = theme.eq_ignore_ascii_case("dark");
    let dark_mode = u32::from(is_dark);
    let (caption_color, text_color, border_color) = if is_dark {
        (0x001b1818, 0x00f5f4f4, 0x00463f3f)
    } else {
        (0x00fcfbfb, 0x001f1718, 0x00d8d4d4)
    };

    unsafe {
        set_dwm_u32(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE as u32, dark_mode);
        set_dwm_u32(hwnd, DWMWA_CAPTION_COLOR as u32, caption_color);
        set_dwm_u32(hwnd, DWMWA_TEXT_COLOR as u32, text_color);
        set_dwm_u32(hwnd, DWMWA_BORDER_COLOR as u32, border_color);
    }

    unsafe fn set_dwm_u32(hwnd: HWND, attribute: u32, value: u32) {
        let _ = DwmSetWindowAttribute(
            hwnd,
            attribute,
            &value as *const u32 as *const c_void,
            std::mem::size_of::<u32>() as u32,
        );
    }

    Ok(())
}

#[cfg(target_os = "windows")]
pub(crate) fn wide_null(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn detect_local_layout(app: &AppHandle) -> LayoutState {
    let device_id = "local-device".to_string();
    let screens = detect_local_screens(app, &device_id);
    let transport_port = choose_available_transport_port(default_transport_port());
    let quic_port = preferred_quic_port(transport_port);
    let selected_screen_id = screens
        .iter()
        .find(|screen| screen.is_primary)
        .or_else(|| screens.first())
        .map(|screen| screen.id.clone())
        .unwrap_or_else(|| "local-display-1".into());

    LayoutState {
        active_device_id: device_id.clone(),
        selected_screen_id,
        input_mode: default_input_mode(),
        machine_role: default_machine_role(),
        cluster_id: default_cluster_id(),
        pair_secret: default_pair_secret(),
        paired_controllers: Vec::new(),
        clipboard_sync: default_clipboard_sync(),
        file_transfer_enabled: default_file_transfer_enabled(),
        language: default_language(),
        theme_mode: default_theme_mode(),
        performance_monitor: default_performance_monitor(),
        transport_port_mode: default_transport_port_mode(),
        transport_port,
        quic_port,
        modifier_remap: default_modifier_remap(),
        modifier_map: default_modifier_map(),
        edge_switch_hotkey: default_edge_switch_hotkey(),
        screen_switch_hotkeys: ScreenSwitchHotkeys::default(),
        devices: vec![Device {
            id: device_id,
            name: local_device_name(),
            platform: current_platform().into(),
            host: local_host_label(),
            transport_port,
            quic_port,
            transport_public_key: String::new(),
            protocol_version: quic_transport::PROTOCOL_VERSION,
            color: "#2f7af8".into(),
            online: true,
            input_ready: false,
            upgrading: false,
            upgrading_until_ms: 0,
            role: "local".into(),
            source: "detected".into(),
            screens,
        }],
    }
}

fn detect_fallback_layout() -> LayoutState {
    LayoutState {
        devices: Vec::new(),
        active_device_id: String::new(),
        selected_screen_id: String::new(),
        input_mode: default_input_mode(),
        machine_role: default_machine_role(),
        cluster_id: default_cluster_id(),
        pair_secret: default_pair_secret(),
        paired_controllers: Vec::new(),
        clipboard_sync: default_clipboard_sync(),
        file_transfer_enabled: default_file_transfer_enabled(),
        language: default_language(),
        theme_mode: default_theme_mode(),
        performance_monitor: default_performance_monitor(),
        transport_port_mode: default_transport_port_mode(),
        transport_port: default_transport_port(),
        quic_port: preferred_quic_port(default_transport_port()),
        modifier_remap: default_modifier_remap(),
        modifier_map: default_modifier_map(),
        edge_switch_hotkey: default_edge_switch_hotkey(),
        screen_switch_hotkeys: ScreenSwitchHotkeys::default(),
    }
}

fn detect_local_screens(app: &AppHandle, device_id: &str) -> Vec<Screen> {
    let monitors = app.available_monitors().unwrap_or_default();
    let primary = app.primary_monitor().ok().flatten();

    if monitors.is_empty() {
        return vec![Screen {
            id: "local-display-1".into(),
            device_id: device_id.into(),
            name: "Display unavailable".into(),
            x: 0,
            y: 0,
            width: 1,
            height: 1,
            scale: 1.0,
            is_primary: true,
        }];
    }

    monitors
        .iter()
        .enumerate()
        .map(|(index, monitor)| {
            let size = monitor.size();
            let position = monitor.position();
            let raw_scale = monitor.scale_factor();
            let scale = round_scale(raw_scale);
            let is_primary = primary
                .as_ref()
                .map(|primary_monitor| same_monitor(monitor, primary_monitor))
                .unwrap_or(index == 0);

            Screen {
                id: format!("local-display-{}", index + 1),
                device_id: device_id.into(),
                name: monitor
                    .name()
                    .cloned()
                    .filter(|name| !name.trim().is_empty())
                    .unwrap_or_else(|| format!("Display {}", index + 1)),
                x: logical_position(position.x, raw_scale),
                y: logical_position(position.y, raw_scale),
                width: logical_size(size.width, raw_scale),
                height: logical_size(size.height, raw_scale),
                scale,
                is_primary,
            }
        })
        .collect()
}

fn normalize_saved_layout(saved_layout: LayoutState, detected_layout: LayoutState) -> LayoutState {
    if is_old_demo_layout(&saved_layout) || saved_layout.devices.is_empty() {
        return detected_layout;
    }

    let local_device =
        merge_detected_local_device(&saved_layout, detected_layout.devices[0].clone());
    let local_device_id = local_device.id.clone();
    let mut devices = vec![local_device];

    devices.extend(
        saved_layout
            .devices
            .into_iter()
            .filter(|device| device.id != local_device_id && !is_old_demo_device(device)),
    );

    let active_device_id = if devices
        .iter()
        .any(|device| device.id == saved_layout.active_device_id)
    {
        saved_layout.active_device_id
    } else {
        local_device_id
    };

    let selected_screen_id = if devices.iter().any(|device| {
        device
            .screens
            .iter()
            .any(|screen| screen.id == saved_layout.selected_screen_id)
    }) {
        saved_layout.selected_screen_id
    } else {
        detected_layout.selected_screen_id
    };

    let transport_port = normalize_transport_port(saved_layout.transport_port);

    LayoutState {
        devices,
        active_device_id,
        selected_screen_id,
        input_mode: normalize_input_mode(&saved_layout.input_mode),
        machine_role: normalize_machine_role(&saved_layout.machine_role),
        cluster_id: normalize_cluster_id(&saved_layout.cluster_id),
        pair_secret: normalize_pair_secret(&saved_layout.pair_secret),
        paired_controllers: normalize_paired_controllers(saved_layout.paired_controllers),
        clipboard_sync: saved_layout.clipboard_sync,
        file_transfer_enabled: saved_layout.file_transfer_enabled,
        language: normalize_language(&saved_layout.language),
        theme_mode: normalize_theme_mode(&saved_layout.theme_mode),
        performance_monitor: saved_layout.performance_monitor,
        transport_port_mode: normalize_transport_port_mode(&saved_layout.transport_port_mode),
        transport_port,
        quic_port: normalize_quic_port(transport_port, saved_layout.quic_port),
        modifier_remap: saved_layout.modifier_remap,
        modifier_map: normalize_modifier_map(&saved_layout.modifier_map),
        edge_switch_hotkey: normalize_edge_switch_hotkey(&saved_layout.edge_switch_hotkey),
        screen_switch_hotkeys: saved_layout.screen_switch_hotkeys.clone(),
    }
}

fn merge_detected_local_device(saved_layout: &LayoutState, mut detected_device: Device) -> Device {
    if let Some(saved_device) = saved_layout
        .devices
        .iter()
        .find(|device| device.id == detected_device.id)
    {
        detected_device.screens = detected_device
            .screens
            .into_iter()
            .map(|screen| {
                saved_device
                    .screens
                    .iter()
                    .find(|saved_screen| saved_screen.id == screen.id)
                    .map(|saved_screen| Screen {
                        x: saved_screen.x,
                        y: saved_screen.y,
                        ..screen.clone()
                    })
                    .unwrap_or(screen)
            })
            .collect();
    }

    detected_device
}

fn is_old_demo_layout(layout: &LayoutState) -> bool {
    layout
        .devices
        .iter()
        .any(|device| is_old_demo_device(device))
}

fn is_old_demo_device(device: &Device) -> bool {
    matches!(device.id.as_str(), "studio-win" | "macbook-pro")
        || matches!(device.host.as_str(), "192.168.31.24" | "192.168.31.63")
}

fn same_monitor(a: &Monitor, b: &Monitor) -> bool {
    a.position().x == b.position().x
        && a.position().y == b.position().y
        && a.size().width == b.size().width
        && a.size().height == b.size().height
}

fn round_scale(scale: f64) -> f64 {
    (scale * 100.0).round() / 100.0
}

fn logical_size(value: u32, scale: f64) -> i32 {
    ((value as f64) / safe_scale(scale))
        .round()
        .clamp(1.0, i32::MAX as f64) as i32
}

fn logical_position(value: i32, scale: f64) -> i32 {
    ((value as f64) / safe_scale(scale))
        .round()
        .clamp(i32::MIN as f64, i32::MAX as f64) as i32
}

fn safe_scale(scale: f64) -> f64 {
    if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    }
}

pub(crate) fn current_platform() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "unknown"
    }
}

fn local_device_name() -> String {
    hostname()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| "This device".into())
}

fn hostname() -> Option<String> {
    HOSTNAME_CACHE.get_or_init(read_hostname).clone()
}

fn read_hostname() -> Option<String> {
    env::var("COMPUTERNAME")
        .or_else(|_| env::var("HOSTNAME"))
        .ok()
        .or_else(|| {
            Command::new("hostname")
                .output()
                .ok()
                .filter(|output| output.status.success())
                .and_then(|output| String::from_utf8(output.stdout).ok())
                .map(|name| name.trim().to_string())
        })
}

fn local_host_label() -> String {
    match (hostname(), local_ip_address()) {
        (Some(name), Some(ip)) => format!("{name} / {ip}"),
        (Some(name), None) => name,
        (None, Some(ip)) => ip,
        (None, None) => "localhost".into(),
    }
}

fn local_ip_address() -> Option<String> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    let address = socket.local_addr().ok()?;
    Some(address.ip().to_string())
}

fn local_ipv4_addresses() -> Vec<Ipv4Addr> {
    let mut addresses = Vec::new();

    if let Ok(interfaces) = if_addrs::get_if_addrs() {
        for interface in interfaces {
            if interface.is_loopback() {
                continue;
            }

            let if_addrs::IfAddr::V4(address) = interface.addr else {
                continue;
            };
            if usable_discovery_ipv4(address.ip) {
                addresses.push(address.ip);
            }
        }
    }

    if let Some(default_ip) = default_route_ipv4_address() {
        if usable_discovery_ipv4(default_ip) {
            addresses.insert(0, default_ip);
        }
    }

    addresses.sort_by_key(|address| address.octets());
    addresses.dedup();
    addresses
}

fn default_route_ipv4_address() -> Option<Ipv4Addr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    let address = socket.local_addr().ok()?;
    match address.ip() {
        std::net::IpAddr::V4(ip) => Some(ip),
        std::net::IpAddr::V6(_) => None,
    }
}

fn usable_discovery_ipv4(address: Ipv4Addr) -> bool {
    !address.is_loopback()
        && !address.is_unspecified()
        && !address.is_multicast()
        && !address.is_broadcast()
        && !address.is_link_local()
}

fn default_device_source() -> String {
    "manual".into()
}

fn default_input_mode() -> String {
    "control".into()
}

fn default_machine_role() -> String {
    "unset".into()
}

fn default_cluster_id() -> String {
    format!("cluster-{}", random_hex(16))
}

fn default_pair_secret() -> String {
    random_hex(32)
}

fn default_clipboard_sync() -> bool {
    false
}

fn default_file_transfer_enabled() -> bool {
    true
}

fn default_language() -> String {
    "cn".into()
}

fn default_theme_mode() -> String {
    "system".into()
}

fn default_performance_monitor() -> bool {
    false
}

fn default_transport_port_mode() -> String {
    "auto".into()
}

fn default_modifier_remap() -> bool {
    true
}

fn default_modifier_control() -> String {
    "meta".into()
}

fn default_modifier_alt() -> String {
    "same".into()
}

fn default_modifier_meta() -> String {
    "control".into()
}

fn default_modifier_map() -> ModifierMap {
    ModifierMap {
        control: default_modifier_control(),
        alt: default_modifier_alt(),
        meta: default_modifier_meta(),
    }
}

fn default_edge_switch_hotkey() -> String {
    "alt+shift+k".into()
}

fn normalize_edge_switch_hotkey(value: &str) -> String {
    let normalized = value.trim().to_ascii_lowercase().replace(' ', "");
    if normalized.is_empty() {
        return default_edge_switch_hotkey();
    }

    normalized
}

fn normalize_modifier_target(value: &str, fallback: fn() -> String) -> String {
    match value {
        "control" | "alt" | "meta" | "same" => value.into(),
        _ => fallback(),
    }
}

fn normalize_modifier_map(map: &ModifierMap) -> ModifierMap {
    ModifierMap {
        control: normalize_modifier_target(&map.control, default_modifier_control),
        alt: normalize_modifier_target(&map.alt, default_modifier_alt),
        meta: normalize_modifier_target(&map.meta, default_modifier_meta),
    }
}

fn default_transport_port() -> u16 {
    DISCOVERY_PORT
}

fn default_protocol_version() -> u16 {
    quic_transport::PROTOCOL_VERSION
}

fn preferred_quic_port(discovery_port: u16) -> u16 {
    discovery_port
        .saturating_add(1)
        .clamp(TRANSPORT_PORT_MIN, TRANSPORT_PORT_MAX)
}

fn normalize_input_mode(mode: &str) -> String {
    if mode == "receive" {
        "receive".into()
    } else {
        "control".into()
    }
}

fn normalize_machine_role(role: &str) -> String {
    match role {
        "server" | "client" => role.into(),
        _ => "unset".into(),
    }
}

fn normalize_cluster_id(cluster_id: &str) -> String {
    let cluster_id = cluster_id.trim();
    if cluster_id.is_empty() {
        default_cluster_id()
    } else {
        cluster_id.into()
    }
}

fn normalize_pair_secret(pair_secret: &str) -> String {
    let pair_secret = pair_secret.trim();
    if pair_secret.is_empty() {
        default_pair_secret()
    } else {
        pair_secret.into()
    }
}

fn normalize_paired_controllers(controllers: Vec<PairedController>) -> Vec<PairedController> {
    controllers
        .into_iter()
        .filter(|controller| {
            !controller.id.trim().is_empty()
                && !controller.transport_public_key.trim().is_empty()
                && !controller.cluster_id.trim().is_empty()
        })
        .collect()
}

fn normalize_language(language: &str) -> String {
    match language {
        "en" => "en".into(),
        _ => "cn".into(),
    }
}

fn normalize_theme_mode(theme_mode: &str) -> String {
    match theme_mode {
        "dark" | "light" | "system" => theme_mode.into(),
        _ => "system".into(),
    }
}

fn normalize_transport_port_mode(mode: &str) -> String {
    match mode {
        "fixed" => "fixed".into(),
        _ => "auto".into(),
    }
}

fn normalize_transport_port(port: u16) -> u16 {
    port.clamp(TRANSPORT_PORT_MIN, TRANSPORT_PORT_MAX)
}

fn normalize_quic_port(discovery_port: u16, quic_port: u16) -> u16 {
    if quic_port == 0 {
        preferred_quic_port(discovery_port)
    } else {
        normalize_transport_port(quic_port)
    }
}

fn choose_available_transport_port(preferred: u16) -> u16 {
    bind_available_udp_port(preferred)
        .map(|(socket, port)| {
            drop(socket);
            port
        })
        .unwrap_or_else(|_| default_transport_port())
}

fn bind_available_udp_port(preferred: u16) -> Result<(UdpSocket, u16), String> {
    let start = normalize_transport_port(preferred);
    for offset in 0..64_u16 {
        let candidate = start.saturating_add(offset);
        if candidate > TRANSPORT_PORT_MAX {
            break;
        }

        if let Ok(socket) = bind_reusable_udp_port(candidate) {
            return Ok((socket, candidate));
        }
    }

    let socket = bind_reusable_udp_port(0)
        .map_err(|error| format!("failed to bind any UDP transport port: {error}"))?;
    let port = socket
        .local_addr()
        .map_err(|error| format!("failed to read selected UDP transport port: {error}"))?
        .port();

    Ok((socket, port))
}

/// Bind a UDP socket on `0.0.0.0:port` with address/port reuse enabled. Reuse
/// lets a fresh discovery socket re-grab the same port while the previous one is
/// still tearing down on a runtime restart (the old socket can sit in `recv_from`
/// for up to its read timeout). Without it the rebind failed and the port
/// silently drifted upward (47833 -> 47834), stranding two peers on mismatched
/// discovery ports so they could never see each other again.
fn bind_reusable_udp_port(port: u16) -> std::io::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};

    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    #[cfg(unix)]
    socket.set_reuse_port(true)?;
    let address = std::net::SocketAddr::from((std::net::Ipv4Addr::UNSPECIFIED, port));
    socket.bind(&address.into())?;
    Ok(socket.into())
}

fn clipboard_disabled_status() -> NativeStageStatus {
    NativeStageStatus {
        state: "idle".into(),
        detail: "剪贴板同步已关闭。".into(),
    }
}

fn clipboard_ready_status() -> NativeStageStatus {
    NativeStageStatus {
        state: "ready".into(),
        detail: "剪贴板同步已开启，仅在鼠标切到远端设备后复用当前传输端口发送。".into(),
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClipboardPacket {
    protocol: String,
    origin_id: String,
    #[serde(default)]
    target_id: String,
    #[serde(default)]
    cluster_id: String,
    #[serde(default)]
    pair_secret: String,
    #[serde(default)]
    signature: String,
    #[serde(default)]
    formats: Vec<ClipboardFormat>,
    // Empty when the payload is an image. Defaulted so packets from older peers
    // (text-only) still decode.
    #[serde(default)]
    text: String,
    // Present only for image copies. Skipped on the wire for text packets, and
    // defaulted so text-only peers still decode image-capable packets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    image: Option<ClipboardImage>,
    sequence: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClipboardFormat {
    kind: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    image: Option<ClipboardImage>,
}

fn clipboard_packet_from_content(
    content: ClipboardContent,
    origin_id: String,
    target_id: String,
    cluster_id: String,
    pair_secret: String,
    sequence: u64,
) -> ClipboardPacket {
    let signature = content.signature();
    match content {
        ClipboardContent::Text(text) => ClipboardPacket {
            protocol: CLIPBOARD_PROTOCOL.into(),
            origin_id,
            target_id,
            cluster_id,
            pair_secret,
            signature,
            formats: vec![ClipboardFormat {
                kind: "plainText".into(),
                text: text.clone(),
                image: None,
            }],
            text,
            image: None,
            sequence,
        },
        ClipboardContent::Image(image) => ClipboardPacket {
            protocol: CLIPBOARD_PROTOCOL.into(),
            origin_id,
            target_id,
            cluster_id,
            pair_secret,
            signature,
            formats: vec![ClipboardFormat {
                kind: "imageRgba".into(),
                text: String::new(),
                image: Some(image.clone()),
            }],
            text: String::new(),
            image: Some(image),
            sequence,
        },
    }
}

#[allow(clippy::too_many_arguments)]
fn run_clipboard_sync(
    quic_transport: quic_transport::TransportHandle,
    local_peer_id: String,
    clipboard_seen_text: Arc<Mutex<Option<String>>>,
    clipboard_echo_until: Arc<Mutex<Option<Instant>>>,
    clipboard_target: Arc<Mutex<Option<input::ClipboardTarget>>>,
    transport_packets: Arc<AtomicU64>,
    clipboard_packets: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
) {
    let mut last_sent: Option<(String, String, String)> = None;
    let mut last_failed: Option<(String, String, String, Instant)> = None;
    let mut last_poll = Instant::now() - Duration::from_secs(1);
    let mut sequence = now_ms();

    while !stop.load(Ordering::Relaxed) {
        let Some(target) = input::current_clipboard_target(&clipboard_target) else {
            thread::sleep(Duration::from_millis(120));
            last_poll = Instant::now() - Duration::from_secs(1);
            continue;
        };

        if last_poll.elapsed() < Duration::from_millis(CLIPBOARD_POLL_INTERVAL_MS) {
            thread::sleep(Duration::from_millis(CLIPBOARD_IDLE_SLEEP_MS));
            continue;
        }
        last_poll = Instant::now();

        let Some(content) = clipboard::read_content() else {
            continue;
        };
        let signature = content.signature();

        // If this is the content we just wrote after receiving a peer packet,
        // suppress it. A different signature during the grace window is treated
        // as a fresh local copy so quick copy/screenshot + paste stays current.
        if clipboard_echo_active(&clipboard_echo_until) {
            let is_known_echo = clipboard_seen_text
                .lock()
                .map(|seen| seen.as_deref() == Some(signature.as_str()))
                .unwrap_or(false);
            if is_known_echo {
                continue;
            }
            if let Ok(mut seen) = clipboard_seen_text.lock() {
                *seen = None;
            }
        }

        if content.is_oversized() {
            continue;
        }

        if last_sent
            .as_ref()
            .map(|(device_id, addr, previous)| {
                device_id == &target.device_id && addr == &target.addr && previous == &signature
            })
            .unwrap_or(false)
        {
            continue;
        }
        if last_failed
            .as_ref()
            .map(|(device_id, addr, previous, failed_at)| {
                device_id == &target.device_id
                    && addr == &target.addr
                    && previous == &signature
                    && failed_at.elapsed() < Duration::from_millis(CLIPBOARD_RETRY_INTERVAL_MS)
            })
            .unwrap_or(false)
        {
            continue;
        }

        let should_send = clipboard_seen_text
            .lock()
            .map(|mut seen| {
                if seen.as_deref() == Some(signature.as_str()) {
                    *seen = None;
                    false
                } else {
                    true
                }
            })
            .unwrap_or(true);

        if !should_send {
            last_sent = Some((target.device_id.clone(), target.addr.clone(), signature));
            continue;
        }

        sequence = sequence.saturating_add(1);
        let packet = clipboard_packet_from_content(
            content,
            local_peer_id.clone(),
            target.device_id.clone(),
            target.cluster_id.clone(),
            target.pair_secret.clone(),
            sequence,
        );

        if let Ok(payload) = encode_wire_packet(&packet) {
            let peer = quic_transport.peer(
                target.addr.clone(),
                target.transport_public_key.clone(),
                target.protocol_version,
            );
            let send_result = quic_transport.send_stream_expect_ack(peer, payload);
            if send_result.is_ok() {
                transport_packets.fetch_add(1, Ordering::Relaxed);
                clipboard_packets.fetch_add(1, Ordering::Relaxed);
                last_failed = None;
                last_sent = Some((target.device_id, target.addr, signature));
            } else {
                if let Err(error) = send_result {
                    log::warn!("clipboard send failed: {error}");
                }
                last_failed = Some((
                    target.device_id.clone(),
                    target.addr.clone(),
                    signature,
                    Instant::now(),
                ));
            }
        }
    }
}

/// True while we are inside the post-write grace window (see
/// `CLIPBOARD_ECHO_GRACE_MS`).
fn clipboard_echo_active(clipboard_echo_until: &Arc<Mutex<Option<Instant>>>) -> bool {
    clipboard_echo_until
        .lock()
        .map(|until| {
            until
                .map(|deadline| Instant::now() < deadline)
                .unwrap_or(false)
        })
        .unwrap_or(false)
}

fn arm_clipboard_echo_guard(clipboard_echo_until: &Arc<Mutex<Option<Instant>>>) {
    if let Ok(mut until) = clipboard_echo_until.lock() {
        *until = Some(Instant::now() + Duration::from_millis(CLIPBOARD_ECHO_GRACE_MS));
    }
}

fn write_clipboard_content_with_retry(content: &ClipboardContent) -> Result<(), String> {
    retry_clipboard_content_write(
        content,
        CLIPBOARD_WRITE_ATTEMPTS,
        Duration::from_millis(CLIPBOARD_WRITE_RETRY_DELAY_MS),
        clipboard::write_content,
    )
}

fn retry_clipboard_content_write<F>(
    content: &ClipboardContent,
    attempts: usize,
    retry_delay: Duration,
    mut write_content: F,
) -> Result<(), String>
where
    F: FnMut(&ClipboardContent) -> Result<(), String>,
{
    let attempts = attempts.max(1);
    let mut last_error = None;
    for attempt in 0..attempts {
        match write_content(content) {
            Ok(()) => return Ok(()),
            Err(error) => last_error = Some(error),
        }
        if attempt + 1 < attempts && !retry_delay.is_zero() {
            thread::sleep(retry_delay);
        }
    }

    Err(last_error.unwrap_or_else(|| "failed to write clipboard content".into()))
}

fn handle_clipboard_packet(
    payload: &[u8],
    layout: &LayoutState,
    local_peer_id: &str,
    clipboard_seen_text: &Arc<Mutex<Option<String>>>,
    clipboard_echo_until: &Arc<Mutex<Option<Instant>>>,
    clipboard_last_sequences: &Arc<Mutex<HashMap<String, u64>>>,
) -> bool {
    handle_clipboard_packet_with_writer(
        payload,
        layout,
        local_peer_id,
        clipboard_seen_text,
        clipboard_echo_until,
        clipboard_last_sequences,
        write_clipboard_content_with_retry,
    )
}

fn handle_clipboard_packet_with_writer<F>(
    payload: &[u8],
    layout: &LayoutState,
    local_peer_id: &str,
    clipboard_seen_text: &Arc<Mutex<Option<String>>>,
    clipboard_echo_until: &Arc<Mutex<Option<Instant>>>,
    clipboard_last_sequences: &Arc<Mutex<HashMap<String, u64>>>,
    mut write_content: F,
) -> bool
where
    F: FnMut(&ClipboardContent) -> Result<(), String>,
{
    let Some(packet) = decode_wire_packet::<ClipboardPacket>(payload) else {
        return false;
    };

    if packet.protocol != CLIPBOARD_PROTOCOL {
        return false;
    }

    if !clipboard_packet_authorized(layout, &packet) {
        return false;
    }

    if !clipboard_packet_targets_local(layout, &packet, local_peer_id) {
        return false;
    }

    if packet.origin_id == local_peer_id {
        return true;
    }

    if !clipboard_packet_sequence_is_current(&packet, clipboard_last_sequences) {
        return false;
    }

    let accepted_sequence = clipboard_packet_sequence(&packet);
    let content = clipboard_content_from_packet(packet);

    let Some(content) = content else {
        return false;
    };
    if content.is_oversized() {
        return false;
    }

    let signature = content.signature();
    let written = match write_content(&content) {
        Ok(()) => true,
        Err(error) => {
            log::warn!("clipboard receive write failed: {error}");
            false
        }
    };

    if written {
        if let Some((origin_id, sequence)) = accepted_sequence {
            remember_clipboard_packet_sequence(clipboard_last_sequences, origin_id, sequence);
        }
        // Remember what we just wrote so our own poll loop recognizes it as an
        // echo (signature match) and arm the time-based guard as a backstop in
        // case the OS hands the bitmap back to us with slightly different bytes.
        if let Ok(mut seen) = clipboard_seen_text.lock() {
            *seen = Some(signature);
        }
        arm_clipboard_echo_guard(clipboard_echo_until);
    }

    written
}

fn clipboard_packet_sequence(packet: &ClipboardPacket) -> Option<(String, u64)> {
    if packet.formats.is_empty() || packet.origin_id.trim().is_empty() {
        None
    } else {
        Some((packet.origin_id.clone(), packet.sequence))
    }
}

fn clipboard_packet_sequence_is_current(
    packet: &ClipboardPacket,
    clipboard_last_sequences: &Arc<Mutex<HashMap<String, u64>>>,
) -> bool {
    let Some((origin_id, sequence)) = clipboard_packet_sequence(packet) else {
        return true;
    };

    clipboard_last_sequences
        .lock()
        .map(|last_sequences| {
            last_sequences
                .get(&origin_id)
                .map(|last_sequence| sequence > *last_sequence)
                .unwrap_or(true)
        })
        .unwrap_or(true)
}

fn remember_clipboard_packet_sequence(
    clipboard_last_sequences: &Arc<Mutex<HashMap<String, u64>>>,
    origin_id: String,
    sequence: u64,
) {
    if let Ok(mut last_sequences) = clipboard_last_sequences.lock() {
        last_sequences
            .entry(origin_id)
            .and_modify(|last_sequence| *last_sequence = (*last_sequence).max(sequence))
            .or_insert(sequence);
    }
}

fn clipboard_packet_targets_local(
    layout: &LayoutState,
    packet: &ClipboardPacket,
    local_peer_id: &str,
) -> bool {
    if packet.target_id.trim().is_empty() || packet.target_id == local_peer_id {
        return true;
    }

    layout
        .devices
        .iter()
        .any(|device| device.role == "local" && device.id == packet.target_id)
}

fn clipboard_content_from_packet(packet: ClipboardPacket) -> Option<ClipboardContent> {
    if let Some(content) = packet
        .formats
        .into_iter()
        .find_map(clipboard_content_from_format)
    {
        return Some(content);
    }

    if let Some(image) = packet.image {
        Some(ClipboardContent::Image(image))
    } else if !packet.text.is_empty() {
        Some(ClipboardContent::Text(packet.text))
    } else {
        None
    }
}

fn clipboard_content_from_format(format: ClipboardFormat) -> Option<ClipboardContent> {
    match format.kind.as_str() {
        "plainText" if !format.text.is_empty() => Some(ClipboardContent::Text(format.text)),
        "imageRgba" => format.image.map(ClipboardContent::Image),
        _ => None,
    }
}

fn clipboard_packet_authorized(layout: &LayoutState, packet: &ClipboardPacket) -> bool {
    if layout.cluster_id.trim().is_empty()
        || layout.pair_secret.trim().is_empty()
        || packet.cluster_id != layout.cluster_id
        || packet.pair_secret != layout.pair_secret
    {
        return false;
    }

    if layout.machine_role == "client" && !layout.paired_controllers.is_empty() {
        return layout
            .paired_controllers
            .iter()
            .any(|controller| controller.id == packet.origin_id);
    }

    true
}

fn file_transfer_target_for_device(
    layout: &LayoutState,
    peers: &[LanPeer],
    device_id: &str,
) -> Result<FileTransferTarget, String> {
    if layout.cluster_id.trim().is_empty() || layout.pair_secret.trim().is_empty() {
        return Err("当前设备尚未完成配对，无法传输文件。".into());
    }

    if let Some(device) = layout
        .devices
        .iter()
        .find(|device| device.id == device_id && device.role != "local")
    {
        if !device.online || !device.input_ready {
            return Err(format!("{} 当前不在线，无法传输文件。", device.name));
        }
        if device.protocol_version != quic_transport::PROTOCOL_VERSION
            || device.transport_public_key.trim().is_empty()
        {
            return Err(format!("{} 版本过旧，请先升级 MyKVM。", device.name));
        }
        let quic_port = normalize_quic_port(device.transport_port, device.quic_port);
        let host = file_transfer_host(&device.host)
            .ok_or_else(|| format!("{} 缺少可用地址。", device.name))?;

        return Ok(FileTransferTarget {
            device_id: device.id.clone(),
            name: device.name.clone(),
            addr: format!("{host}:{quic_port}"),
            transport_public_key: device.transport_public_key.clone(),
            protocol_version: device.protocol_version,
            cluster_id: layout.cluster_id.clone(),
            pair_secret: layout.pair_secret.clone(),
        });
    }

    if layout.machine_role == "client" {
        if let Some(controller) = layout
            .paired_controllers
            .iter()
            .find(|controller| controller.id == device_id)
        {
            let peer = peers
                .iter()
                .find(|peer| {
                    peer.id == controller.id
                        || (!controller.transport_public_key.trim().is_empty()
                            && peer.transport_public_key == controller.transport_public_key)
                })
                .ok_or_else(|| format!("{} 当前不在线，无法传输文件。", controller.name))?;
            if peer.protocol_version != quic_transport::PROTOCOL_VERSION
                || peer.transport_public_key.trim().is_empty()
                || peer.quic_port == 0
            {
                return Err(format!("{} 版本过旧，请先升级 MyKVM。", controller.name));
            }
            let host = if !peer.ip.trim().is_empty() {
                peer.ip.clone()
            } else {
                file_transfer_host(&peer.host)
                    .or_else(|| file_transfer_host(&controller.ip))
                    .or_else(|| file_transfer_host(&controller.host))
                    .ok_or_else(|| format!("{} 缺少可用地址。", controller.name))?
            };

            return Ok(FileTransferTarget {
                device_id: controller.id.clone(),
                name: controller.name.clone(),
                addr: format!("{}:{}", host, peer.quic_port),
                transport_public_key: peer.transport_public_key.clone(),
                protocol_version: peer.protocol_version,
                cluster_id: layout.cluster_id.clone(),
                pair_secret: layout.pair_secret.clone(),
            });
        }
    }

    Err("没有找到可传输的目标设备。".into())
}

fn file_transfer_host(host_value: &str) -> Option<String> {
    host_candidates(host_value)
        .into_iter()
        .find_map(|candidate| {
            let (host, _) = split_host_port(&candidate);
            (!host.trim().is_empty()).then_some(host)
        })
}

fn collect_transfer_files(paths: &[String]) -> Result<Vec<TransferFile>, String> {
    let mut files = Vec::new();
    for path_value in paths {
        let path_value = path_value.trim();
        if path_value.is_empty() {
            continue;
        }
        let path = PathBuf::from(path_value);
        let metadata = fs::metadata(&path)
            .map_err(|error| format!("无法读取文件 {}: {error}", path.display()))?;
        if !metadata.is_file() {
            return Err(format!("暂不支持传输文件夹或特殊文件：{}", path.display()));
        }
        if metadata.len() > FILE_TRANSFER_MAX_FILE_BYTES {
            return Err(format!(
                "{} 超过单文件上限 {}。",
                path.display(),
                format_bytes(FILE_TRANSFER_MAX_FILE_BYTES)
            ));
        }
        let name = transfer_file_name(&path)?;
        files.push(TransferFile {
            path,
            name,
            total_bytes: metadata.len(),
        });
    }

    if files.is_empty() {
        Err("请选择要传输的文件。".into())
    } else {
        Ok(files)
    }
}

fn transfer_file_name(path: &Path) -> Result<String, String> {
    let raw_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    sanitize_transfer_file_name(&raw_name).ok_or_else(|| {
        format!(
            "文件名不可用于传输：{}",
            if raw_name.is_empty() {
                path.display().to_string()
            } else {
                raw_name
            }
        )
    })
}

fn send_transfer_file(
    quic_transport: &quic_transport::TransportHandle,
    origin_id: &str,
    target: &FileTransferTarget,
    file: &TransferFile,
    destination_hint: Option<&str>,
) -> Result<u64, String> {
    let transfer_id = format!("file-{}-{}", now_ms(), random_hex(8));
    let mut packet_count = 0_u64;

    send_file_transfer_packet(
        quic_transport,
        target,
        file_transfer_packet(
            "start",
            &transfer_id,
            origin_id,
            target,
            &file.name,
            file.total_bytes,
            0,
            0,
            destination_hint,
            Vec::new(),
        ),
    )?;
    packet_count += 1;

    let mut file_handle = fs::File::open(&file.path)
        .map_err(|error| format!("无法打开文件 {}: {error}", file.path.display()))?;
    let mut buffer = vec![0_u8; FILE_TRANSFER_CHUNK_BYTES];
    let mut offset = 0_u64;
    let mut chunk_index = 0_u64;
    loop {
        let read = file_handle
            .read(&mut buffer)
            .map_err(|error| format!("读取文件 {} 失败: {error}", file.path.display()))?;
        if read == 0 {
            break;
        }
        let data = buffer[..read].to_vec();
        send_file_transfer_packet(
            quic_transport,
            target,
            file_transfer_packet(
                "chunk",
                &transfer_id,
                origin_id,
                target,
                &file.name,
                file.total_bytes,
                chunk_index,
                offset,
                destination_hint,
                data,
            ),
        )?;
        packet_count += 1;
        offset = offset.saturating_add(read as u64);
        chunk_index = chunk_index.saturating_add(1);
    }

    send_file_transfer_packet(
        quic_transport,
        target,
        file_transfer_packet(
            "finish",
            &transfer_id,
            origin_id,
            target,
            &file.name,
            file.total_bytes,
            chunk_index,
            offset,
            destination_hint,
            Vec::new(),
        ),
    )?;
    packet_count += 1;

    Ok(packet_count)
}

#[allow(clippy::too_many_arguments)]
fn file_transfer_packet(
    kind: &str,
    transfer_id: &str,
    origin_id: &str,
    target: &FileTransferTarget,
    file_name: &str,
    total_bytes: u64,
    chunk_index: u64,
    offset: u64,
    destination_hint: Option<&str>,
    data: Vec<u8>,
) -> FileTransferPacket {
    FileTransferPacket {
        protocol: FILE_TRANSFER_PROTOCOL.into(),
        kind: kind.into(),
        transfer_id: transfer_id.into(),
        origin_id: origin_id.into(),
        target_id: target.device_id.clone(),
        cluster_id: target.cluster_id.clone(),
        pair_secret: target.pair_secret.clone(),
        file_name: file_name.into(),
        destination_hint: destination_hint.map(str::to_string),
        total_bytes,
        chunk_index,
        offset,
        data,
    }
}

fn send_file_transfer_packet(
    quic_transport: &quic_transport::TransportHandle,
    target: &FileTransferTarget,
    packet: FileTransferPacket,
) -> Result<(), String> {
    let payload = encode_wire_packet(&packet)?;
    let peer = quic_transport.peer(
        target.addr.clone(),
        target.transport_public_key.clone(),
        target.protocol_version,
    );
    quic_transport
        .send_stream_expect_ack(peer, payload)
        .map_err(|error| format!("文件传输失败: {error}"))
}

fn handle_file_transfer_packet(
    payload: &[u8],
    layout: &LayoutState,
    local_peer_id: &str,
    transfers: &Arc<Mutex<HashMap<String, IncomingFileTransfer>>>,
    app: &AppHandle,
) -> bool {
    let Some(packet) = decode_wire_packet::<FileTransferPacket>(payload) else {
        return false;
    };
    let Ok(receive_root) = file_transfer_receive_root(app) else {
        log::warn!("file transfer receive failed: could not resolve receive directory");
        return false;
    };
    let pointer_receive_root = if packet.kind == "start"
        && packet.destination_hint.as_deref() == Some(FILE_TRANSFER_DESTINATION_POINTER)
    {
        file_transfer_pointer_receive_root(app)
    } else {
        None
    };

    handle_decoded_file_transfer_packet(
        packet,
        layout,
        local_peer_id,
        transfers,
        &receive_root,
        pointer_receive_root.as_deref(),
        Some(app),
    )
}

fn file_transfer_receive_root(app: &AppHandle) -> Result<PathBuf, String> {
    if let Ok(downloads) = app.path().download_dir() {
        return Ok(downloads.join("MyKVM Transfers"));
    }

    app.path()
        .app_data_dir()
        .map(|directory| directory.join("MyKVM Transfers"))
        .map_err(|error| format!("failed to resolve file transfer receive directory: {error}"))
}

fn file_transfer_pointer_receive_root(app: &AppHandle) -> Option<PathBuf> {
    platform_pointer_receive_root(app).and_then(usable_existing_directory)
}

#[cfg(target_os = "macos")]
fn platform_pointer_receive_root(app: &AppHandle) -> Option<PathBuf> {
    app.path()
        .desktop_dir()
        .ok()
        .or_else(macos_finder_insertion_directory)
}

#[cfg(not(target_os = "macos"))]
fn platform_pointer_receive_root(app: &AppHandle) -> Option<PathBuf> {
    app.path().desktop_dir().ok()
}

fn usable_existing_directory(path: PathBuf) -> Option<PathBuf> {
    if path.is_absolute() && path.is_dir() {
        Some(path)
    } else {
        None
    }
}

#[cfg(target_os = "macos")]
fn macos_finder_insertion_directory() -> Option<PathBuf> {
    let script = r#"
tell application "Finder"
  try
    set targetFolder to insertion location as alias
    return POSIX path of targetFolder
  on error
    return ""
  end try
end tell
"#;
    let output = Command::new("osascript")
        .arg("-e")
        .arg(script)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let path = String::from_utf8_lossy(&output.stdout)
        .trim()
        .trim_end_matches('/')
        .to_string();
    if path.is_empty() {
        return None;
    }

    Some(PathBuf::from(path))
}

#[cfg(test)]
fn handle_file_transfer_packet_with_root(
    payload: &[u8],
    layout: &LayoutState,
    local_peer_id: &str,
    transfers: &Arc<Mutex<HashMap<String, IncomingFileTransfer>>>,
    receive_root: &Path,
) -> bool {
    handle_file_transfer_packet_with_destination_root(
        payload,
        layout,
        local_peer_id,
        transfers,
        receive_root,
        None,
        None,
    )
}

#[cfg(test)]
fn handle_file_transfer_packet_with_destination_root(
    payload: &[u8],
    layout: &LayoutState,
    local_peer_id: &str,
    transfers: &Arc<Mutex<HashMap<String, IncomingFileTransfer>>>,
    receive_root: &Path,
    pointer_receive_root: Option<&Path>,
    landing_app: Option<&AppHandle>,
) -> bool {
    let Some(packet) = decode_wire_packet::<FileTransferPacket>(payload) else {
        return false;
    };

    handle_decoded_file_transfer_packet(
        packet,
        layout,
        local_peer_id,
        transfers,
        receive_root,
        pointer_receive_root,
        landing_app,
    )
}

fn handle_decoded_file_transfer_packet(
    packet: FileTransferPacket,
    layout: &LayoutState,
    local_peer_id: &str,
    transfers: &Arc<Mutex<HashMap<String, IncomingFileTransfer>>>,
    receive_root: &Path,
    pointer_receive_root: Option<&Path>,
    landing_app: Option<&AppHandle>,
) -> bool {
    if packet.protocol != FILE_TRANSFER_PROTOCOL {
        return false;
    }
    if !layout.file_transfer_enabled {
        return false;
    }
    if !file_transfer_packet_authorized(layout, &packet) {
        return false;
    }
    if !file_transfer_packet_targets_local(layout, &packet, local_peer_id) {
        return false;
    }
    if packet.origin_id == local_peer_id {
        return true;
    }

    let destination_root =
        file_transfer_destination_root(&packet, receive_root, pointer_receive_root);
    match packet.kind.as_str() {
        "start" => {
            let show_landing =
                packet.destination_hint.as_deref() == Some(FILE_TRANSFER_DESTINATION_POINTER);
            let file_name = packet.file_name.clone();
            let accepted = start_incoming_file_transfer(packet, transfers, destination_root);
            if accepted && show_landing {
                if let Some(app) = landing_app {
                    show_file_drop_landing_window(app, &file_name);
                }
            }
            accepted
        }
        "chunk" => append_incoming_file_transfer_chunk(packet, transfers),
        "finish" => finish_incoming_file_transfer(packet, transfers),
        _ => false,
    }
}

fn file_transfer_destination_root<'a>(
    packet: &FileTransferPacket,
    receive_root: &'a Path,
    pointer_receive_root: Option<&'a Path>,
) -> &'a Path {
    if packet.destination_hint.as_deref() == Some(FILE_TRANSFER_DESTINATION_POINTER) {
        return pointer_receive_root.unwrap_or(receive_root);
    }

    receive_root
}

fn show_file_drop_landing_window(app: &AppHandle, _file_name: &str) {
    let Some((x, y)) = current_pointer_position() else {
        return;
    };
    let x = x - FILE_DROP_LANDING_WIDTH / 2.0;
    let y = y - FILE_DROP_LANDING_HEIGHT / 2.0;

    if let Some(window) = app.get_webview_window(FILE_DROP_LANDING_LABEL) {
        let _ = window.set_position(tauri::Position::Logical(tauri::LogicalPosition::new(x, y)));
        let _ = window.set_always_on_top(true);
        let _ = window.set_focusable(false);
        let _ = window.show();
        schedule_file_drop_landing_hide(app.clone());
        return;
    }

    let build_result = WebviewWindowBuilder::new(
        app,
        FILE_DROP_LANDING_LABEL,
        WebviewUrl::App("file-drop-landing.html".into()),
    )
    .title("")
    .position(x, y)
    .inner_size(FILE_DROP_LANDING_WIDTH, FILE_DROP_LANDING_HEIGHT)
    .decorations(false)
    .resizable(false)
    .maximizable(false)
    .minimizable(false)
    .closable(false)
    .shadow(false)
    .transparent(true)
    .always_on_top(true)
    .visible_on_all_workspaces(true)
    .skip_taskbar(true)
    .focused(false)
    .focusable(false)
    .build();

    if let Err(error) = build_result {
        log::warn!("file drop landing window failed: {error}");
        return;
    }

    schedule_file_drop_landing_hide(app.clone());
}

fn schedule_file_drop_landing_hide(app: AppHandle) {
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(2200));
        let main_handle = app.clone();
        let _ = app.run_on_main_thread(move || {
            if let Some(window) = main_handle.get_webview_window(FILE_DROP_LANDING_LABEL) {
                let _ = window.hide();
            }
        });
    });
}

fn current_pointer_position() -> Option<(f64, f64)> {
    platform_current_pointer_position()
}

#[cfg(target_os = "macos")]
fn platform_current_pointer_position() -> Option<(f64, f64)> {
    use core_graphics::{
        event::CGEvent,
        event_source::{CGEventSource, CGEventSourceStateID},
    };

    let source = CGEventSource::new(CGEventSourceStateID::CombinedSessionState).ok()?;
    let event = CGEvent::new(source).ok()?;
    let point = event.location();
    Some((point.x, point.y))
}

#[cfg(target_os = "windows")]
fn platform_current_pointer_position() -> Option<(f64, f64)> {
    use windows_sys::Win32::{Foundation::POINT, UI::WindowsAndMessaging::GetCursorPos};

    let mut point = POINT { x: 0, y: 0 };
    let ok = unsafe { GetCursorPos(&mut point) };
    if ok == 0 {
        return None;
    }

    Some((point.x as f64, point.y as f64))
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn platform_current_pointer_position() -> Option<(f64, f64)> {
    None
}

fn start_incoming_file_transfer(
    packet: FileTransferPacket,
    transfers: &Arc<Mutex<HashMap<String, IncomingFileTransfer>>>,
    receive_root: &Path,
) -> bool {
    if packet.transfer_id.trim().is_empty()
        || packet.origin_id.trim().is_empty()
        || packet.total_bytes > FILE_TRANSFER_MAX_FILE_BYTES
        || !packet.data.is_empty()
    {
        return false;
    }

    let Some(file_name) = sanitize_transfer_file_name(&packet.file_name) else {
        return false;
    };

    if let Err(error) = fs::create_dir_all(receive_root) {
        log::warn!(
            "file transfer receive failed: could not create {}: {error}",
            receive_root.display()
        );
        return false;
    }

    let final_path = unique_transfer_destination(receive_root, &file_name);
    let temp_path = receive_root.join(format!(
        ".mykvm-{}-{}.part",
        sanitize_transfer_id(&packet.transfer_id),
        file_name
    ));

    if let Ok(mut transfers) = transfers.lock() {
        if let Some(previous) = transfers.remove(&packet.transfer_id) {
            let _ = fs::remove_file(previous.temp_path);
        }
    } else {
        return false;
    }

    if fs::File::create(&temp_path).is_err() {
        return false;
    }

    let transfer = IncomingFileTransfer {
        origin_id: packet.origin_id.clone(),
        target_id: packet.target_id.clone(),
        file_name,
        total_bytes: packet.total_bytes,
        received_bytes: 0,
        next_chunk_index: 0,
        temp_path,
        final_path,
    };

    transfers
        .lock()
        .map(|mut transfers| {
            transfers.insert(packet.transfer_id, transfer);
            true
        })
        .unwrap_or(false)
}

fn append_incoming_file_transfer_chunk(
    packet: FileTransferPacket,
    transfers: &Arc<Mutex<HashMap<String, IncomingFileTransfer>>>,
) -> bool {
    if packet.data.is_empty() || packet.data.len() > FILE_TRANSFER_CHUNK_BYTES {
        return false;
    }
    let Ok(mut transfers) = transfers.lock() else {
        return false;
    };
    let Some(transfer) = transfers.get_mut(&packet.transfer_id) else {
        return false;
    };
    if packet.origin_id != transfer.origin_id
        || packet.target_id != transfer.target_id
        || packet.file_name != transfer.file_name
        || packet.total_bytes != transfer.total_bytes
        || packet.chunk_index != transfer.next_chunk_index
        || packet.offset != transfer.received_bytes
        || transfer
            .received_bytes
            .saturating_add(packet.data.len() as u64)
            > transfer.total_bytes
    {
        return false;
    }

    let write_result = fs::OpenOptions::new()
        .append(true)
        .open(&transfer.temp_path)
        .and_then(|mut file| file.write_all(&packet.data));
    if let Err(error) = write_result {
        log::warn!("file transfer chunk write failed: {error}");
        return false;
    }

    transfer.received_bytes = transfer
        .received_bytes
        .saturating_add(packet.data.len() as u64);
    transfer.next_chunk_index = transfer.next_chunk_index.saturating_add(1);
    true
}

fn finish_incoming_file_transfer(
    packet: FileTransferPacket,
    transfers: &Arc<Mutex<HashMap<String, IncomingFileTransfer>>>,
) -> bool {
    if !packet.data.is_empty() {
        return false;
    }
    let (temp_path, final_path, file_name, total_bytes) = {
        let Ok(transfers) = transfers.lock() else {
            return false;
        };
        let Some(transfer) = transfers.get(&packet.transfer_id) else {
            return false;
        };
        if packet.origin_id != transfer.origin_id
            || packet.target_id != transfer.target_id
            || packet.file_name != transfer.file_name
            || packet.total_bytes != transfer.total_bytes
            || packet.offset != transfer.received_bytes
            || packet.chunk_index != transfer.next_chunk_index
            || transfer.received_bytes != transfer.total_bytes
        {
            return false;
        }
        (
            transfer.temp_path.clone(),
            transfer.final_path.clone(),
            transfer.file_name.clone(),
            transfer.total_bytes,
        )
    };

    match fs::rename(&temp_path, &final_path) {
        Ok(()) => {
            if let Ok(mut transfers) = transfers.lock() {
                transfers.remove(&packet.transfer_id);
            }
            log::info!(
                "received file transfer {} bytes={} path={}",
                file_name,
                total_bytes,
                final_path.display()
            );
            true
        }
        Err(error) => {
            log::warn!("file transfer finalize failed: {error}");
            false
        }
    }
}

fn file_transfer_packet_authorized(layout: &LayoutState, packet: &FileTransferPacket) -> bool {
    if layout.cluster_id.trim().is_empty()
        || layout.pair_secret.trim().is_empty()
        || packet.cluster_id != layout.cluster_id
        || packet.pair_secret != layout.pair_secret
    {
        return false;
    }

    if layout.machine_role == "client" && !layout.paired_controllers.is_empty() {
        return layout
            .paired_controllers
            .iter()
            .any(|controller| controller.id == packet.origin_id);
    }

    true
}

fn file_transfer_packet_targets_local(
    layout: &LayoutState,
    packet: &FileTransferPacket,
    local_peer_id: &str,
) -> bool {
    if packet.target_id.trim().is_empty() || packet.target_id == local_peer_id {
        return true;
    }

    layout
        .devices
        .iter()
        .any(|device| device.role == "local" && device.id == packet.target_id)
}

fn sanitize_transfer_file_name(name: &str) -> Option<String> {
    let mut output = String::with_capacity(name.len().min(180));
    for character in name.trim().chars() {
        let safe = match character {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            character if character.is_control() => '_',
            character => character,
        };
        output.push(safe);
        if output.len() >= 180 {
            break;
        }
    }
    let output = output.trim().trim_matches('.').trim().to_string();
    if output.is_empty() || output == "." || output == ".." {
        None
    } else {
        Some(output)
    }
}

fn sanitize_transfer_id(transfer_id: &str) -> String {
    let id = transfer_id
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || *character == '-')
        .take(80)
        .collect::<String>();
    if id.is_empty() {
        random_hex(8)
    } else {
        id
    }
}

fn unique_transfer_destination(directory: &Path, file_name: &str) -> PathBuf {
    let first = directory.join(file_name);
    if !first.exists() {
        return first;
    }

    let path = Path::new(file_name);
    let stem = path
        .file_stem()
        .map(|value| value.to_string_lossy().into_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "file".into());
    let extension = path
        .extension()
        .map(|value| format!(".{}", value.to_string_lossy()))
        .unwrap_or_default();

    for index in 1..10_000 {
        let candidate = directory.join(format!("{stem} ({index}){extension}"));
        if !candidate.exists() {
            return candidate;
        }
    }

    directory.join(format!("{stem}-{}{extension}", random_hex(4)))
}

fn format_bytes(bytes: u64) -> String {
    const GIB: u64 = 1024 * 1024 * 1024;
    const MIB: u64 = 1024 * 1024;
    if bytes >= GIB {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else {
        format!("{bytes} bytes")
    }
}

fn encode_wire_packet<T: Serialize>(packet: &T) -> Result<Vec<u8>, String> {
    rmp_serde::to_vec_named(packet).map_err(|error| error.to_string())
}

fn decode_wire_packet<T>(payload: &[u8]) -> Option<T>
where
    T: for<'de> Deserialize<'de>,
{
    rmp_serde::from_slice::<T>(payload).ok()
}

fn sync_layout_peer_presence(
    layout_state: &Arc<Mutex<LayoutState>>,
    peers: &Arc<Mutex<Vec<LanPeer>>>,
) {
    let peers = active_peer_snapshot(peers);
    if let Ok(mut layout) = layout_state.lock() {
        apply_peer_presence(&mut layout, &peers);
    }
}

fn active_peer_snapshot(peers: &Arc<Mutex<Vec<LanPeer>>>) -> Vec<LanPeer> {
    let now = now_ms();
    peers
        .lock()
        .map(|mut peers| {
            prune_stale_peer_entries(&mut peers, now);
            peers.clone()
        })
        .unwrap_or_default()
}

fn apply_peer_presence(layout: &mut LayoutState, peers: &[LanPeer]) {
    let local_transport_port = layout.transport_port;
    let local_quic_port = layout.quic_port;
    let cluster_id = layout.cluster_id.clone();
    for device in &mut layout.devices {
        if device.role == "local" {
            device.online = true;
            device.input_ready = false;
            device.transport_port = local_transport_port;
            device.quic_port = local_quic_port;
            device.protocol_version = quic_transport::PROTOCOL_VERSION;
            continue;
        }

        let peer = peers
            .iter()
            .find(|peer| device_matches_peer(device, peer, &cluster_id));
        if let Some(peer) = peer {
            update_device_from_peer(device, peer);
        } else {
            device.online = false;
            device.input_ready = false;
            if device.upgrading && now_ms() > device.upgrading_until_ms {
                device.upgrading = false;
            }
        }
    }

    refresh_paired_controller_keys(layout, peers);
}

/// Keeps each paired controller's transport_public_key (and id/host/ip) in sync
/// with the peer it was paired with. A peer's QUIC transport identity is
/// regenerated whenever its self-signed cert/key file is missing — app updates,
/// reinstalls, or the file being cleared all rotate the advertised
/// transport_public_key while the pairing credentials (cluster_id/pair_secret)
/// stay the same. Without this sync the controller's stored key goes stale, the
/// input path rejects every packet with "controller not in paired-controllers
/// list", and the user is forced to re-pair even though the pairing is still
/// valid. The security premise is unchanged: input packets still have to match
/// cluster_id/pair_secret, which only the two paired endpoints know.
fn refresh_paired_controller_keys(layout: &mut LayoutState, peers: &[LanPeer]) {
    if layout.paired_controllers.is_empty() {
        return;
    }

    for controller in &mut layout.paired_controllers {
        let Some(peer) = peers
            .iter()
            .find(|peer| paired_controller_can_repair_with_peer(controller, peer))
        else {
            continue;
        };

        let new_key = peer.transport_public_key.trim();
        if !new_key.is_empty() && controller.transport_public_key != new_key {
            log::info!(
                "paired controller {} rotated transport key; updating stored key",
                controller.id
            );
            controller.transport_public_key = new_key.to_string();
        }

        let new_id = peer_device_id(peer);
        if !new_id.is_empty() && controller.id != new_id {
            controller.id = new_id;
        }
        if !peer.host.trim().is_empty() {
            controller.host = peer.host.clone();
        }
        if !peer.ip.trim().is_empty() {
            controller.ip = peer.ip.clone();
        }
        if !peer.name.trim().is_empty() {
            controller.name = peer.name.clone();
        }
        controller.protocol_version = peer.protocol_version;
    }
}

fn device_matches_peer(device: &Device, peer: &LanPeer, layout_cluster_id: &str) -> bool {
    device.id == peer_device_id(peer)
        || (!device.transport_public_key.trim().is_empty()
            && device.transport_public_key == peer.transport_public_key)
        || same_cluster_host(device, peer, layout_cluster_id)
}

fn same_cluster_host(device: &Device, peer: &LanPeer, layout_cluster_id: &str) -> bool {
    let cluster_id = layout_cluster_id.trim();
    !cluster_id.is_empty()
        && !peer.pairing_required
        && peer.cluster_id == cluster_id
        && (same_host(&device.host, &peer.host) || same_host(&device.host, &peer.ip))
}

#[allow(dead_code)]
fn same_host(value: &str, host: &str) -> bool {
    let host = host.trim().to_ascii_lowercase();
    if host.is_empty() {
        return false;
    }

    value
        .split('/')
        .map(|part| part.trim().to_ascii_lowercase())
        .any(|part| part == host)
}

fn peer_device_id(peer: &LanPeer) -> String {
    let id = sanitize_id(if peer.id.trim().is_empty() {
        if peer.name.trim().is_empty() {
            &peer.ip
        } else {
            &peer.name
        }
    } else {
        &peer.id
    });

    if id.is_empty() {
        "peer-device".into()
    } else {
        id
    }
}

fn sanitize_id(value: &str) -> String {
    value
        .trim()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

fn update_device_from_peer(device: &mut Device, peer: &LanPeer) {
    device.online = true;
    device.input_ready = peer.input_ready;
    device.host = if peer.ip.trim().is_empty() {
        peer.host.clone()
    } else {
        peer.ip.clone()
    };
    device.transport_port = peer.transport_port;
    device.quic_port = normalize_quic_port(peer.transport_port, peer.quic_port);
    device.transport_public_key = peer.transport_public_key.clone();
    device.protocol_version = peer.protocol_version;
    if !peer.platform.trim().is_empty() {
        device.platform = normalize_peer_platform(&peer.platform).into();
    }
    if !peer.name.trim().is_empty() && device.source == "detected" {
        device.name = peer.name.clone();
    }
    if !peer.screens.is_empty() {
        device.screens = screens_from_peer(peer, &device.id, &device.screens);
    }
    if peer.upgrading && !device.upgrading {
        device.upgrading_until_ms = now_ms() + 120_000;
    }
    device.upgrading = peer.upgrading;
}

fn screens_from_peer(peer: &LanPeer, device_id: &str, existing_screens: &[Screen]) -> Vec<Screen> {
    if peer.screens.is_empty() {
        return existing_screens.to_vec();
    }

    let peer_min_x = peer
        .screens
        .iter()
        .map(|screen| screen.x)
        .min()
        .unwrap_or_default();
    let peer_min_y = peer
        .screens
        .iter()
        .map(|screen| screen.y)
        .min()
        .unwrap_or_default();
    peer.screens
        .iter()
        .enumerate()
        .map(|(index, peer_screen)| {
            let id = unique_peer_screen_id(device_id, peer_screen, index);
            let existing_screen = existing_screens.iter().find(|screen| screen.id == id);

            Screen {
                id,
                device_id: device_id.into(),
                name: if peer_screen.name.trim().is_empty() {
                    format!("Display {}", index + 1)
                } else {
                    peer_screen.name.clone()
                },
                x: existing_screen
                    .map(|screen| screen.x)
                    .unwrap_or(peer_screen.x - peer_min_x),
                y: existing_screen
                    .map(|screen| screen.y)
                    .unwrap_or(peer_screen.y - peer_min_y),
                width: peer_screen.width,
                height: peer_screen.height,
                scale: peer_screen.scale,
                is_primary: peer_screen.is_primary,
            }
        })
        .collect()
}

fn unique_peer_screen_id(device_id: &str, screen: &LanPeerScreen, index: usize) -> String {
    let seed = if !screen.id.trim().is_empty() {
        screen.id.as_str()
    } else if !screen.name.trim().is_empty() {
        screen.name.as_str()
    } else {
        return format!("{device_id}-display-{}", index + 1);
    };

    let suffix = sanitize_id(seed);
    if suffix.is_empty() {
        format!("{device_id}-display-{}", index + 1)
    } else {
        format!("{device_id}-{suffix}")
    }
}

fn normalize_peer_platform(platform: &str) -> &'static str {
    if platform.eq_ignore_ascii_case("windows") {
        "windows"
    } else if platform.eq_ignore_ascii_case("macos") {
        "macos"
    } else {
        "unknown"
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DiscoveryPacket {
    protocol: String,
    kind: String,
    peer: LanPeer,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pairing_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pair_cluster_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pair_secret: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pairing_error: Option<String>,
}

#[derive(Default)]
struct DiscoveryPairingFields {
    code: Option<String>,
    cluster_id: Option<String>,
    secret: Option<String>,
    error: Option<String>,
}

struct IncomingDiscovery {
    kind: String,
    peer: LanPeer,
    pairing_code: Option<String>,
    pair_cluster_id: Option<String>,
    pair_secret: Option<String>,
}

fn local_peer_from_layout(layout: &LayoutState) -> LanPeer {
    let local_device = layout
        .devices
        .iter()
        .find(|device| device.role == "local")
        .or_else(|| layout.devices.first());
    let fallback_name = local_device_name();
    let host = hostname().unwrap_or_else(|| "localhost".into());
    let ip = local_ip_address().unwrap_or_else(|| "127.0.0.1".into());

    LanPeer {
        id: local_peer_id(&host, &ip),
        name: local_device
            .map(|device| device.name.clone())
            .filter(|name| !name.trim().is_empty())
            .unwrap_or(fallback_name),
        platform: current_platform().into(),
        machine_role: layout.machine_role.clone(),
        cluster_id: advertised_cluster_id(layout),
        pairing_required: pairing_required(layout),
        host,
        ip,
        transport_port: layout.transport_port,
        quic_port: normalize_quic_port(layout.transport_port, layout.quic_port),
        transport_public_key: local_device
            .map(|device| device.transport_public_key.clone())
            .unwrap_or_default(),
        protocol_version: local_device
            .map(|device| device.protocol_version)
            .unwrap_or_else(default_protocol_version),
        screen_count: local_device.map(|device| device.screens.len()).unwrap_or(0),
        input_ready: false,
        upgrading: false,
        screens: local_device
            .map(|device| device.screens.iter().map(screen_to_peer_screen).collect())
            .unwrap_or_default(),
        app_version: env!("CARGO_PKG_VERSION").into(),
        last_seen_ms: now_ms(),
    }
}

fn apply_transport_to_peer(peer: &mut LanPeer, transport: &quic_transport::TransportHandle) {
    peer.quic_port = transport.port();
    peer.transport_public_key = transport.public_key().to_string();
    peer.protocol_version = quic_transport::PROTOCOL_VERSION;
}

fn warm_quic_peer(transport: &quic_transport::TransportHandle, peer: &LanPeer) {
    if !peer.input_ready || peer.transport_public_key.trim().is_empty() || peer.quic_port == 0 {
        return;
    }
    let endpoint = transport.peer(
        format!("{}:{}", peer.ip, peer.quic_port),
        peer.transport_public_key.clone(),
        peer.protocol_version,
    );
    let _ = transport.send_datagram(endpoint, Vec::new());
}

fn pairing_required(layout: &LayoutState) -> bool {
    layout.machine_role == "client" && layout.paired_controllers.is_empty()
}

fn advertised_cluster_id(layout: &LayoutState) -> String {
    if pairing_required(layout) {
        String::new()
    } else {
        layout.cluster_id.clone()
    }
}

fn advertised_input_ready(layout: &LayoutState, input_ready: bool) -> bool {
    input_ready && !pairing_required(layout) && !layout.cluster_id.trim().is_empty()
}

fn should_send_public_announce(layout: &LayoutState) -> bool {
    // Paired clients used to stay silent on public announces and only reply
    // to their paired server's probes. But if the reply path ever fails (the
    // server's announce arrives while the client is still starting up after an
    // admin-restart, or the cluster_id the server broadcasts momentarily
    // differs), the server never sees the client come back online and the
    // cursor can't cross — the "paired but shows online and nothing happens"
    // trap that forces a re-pair. Letting a paired client also announce means
    // the server's apply_peer_presence picks it up within one announce cycle
    // (3 s) without relying solely on the reply path. The announce only
    // carries public fields (cluster_id, transport_public_key, host, screens)
    // — never the pair_secret — and MyKVM is designed for trusted LANs, so
    // this does not lower the security posture.
    let _ = layout;
    true
}

fn screen_to_peer_screen(screen: &Screen) -> LanPeerScreen {
    LanPeerScreen {
        id: screen.id.clone(),
        name: screen.name.clone(),
        x: screen.x,
        y: screen.y,
        width: screen.width,
        height: screen.height,
        scale: screen.scale,
        is_primary: screen.is_primary,
    }
}

fn local_peer_id(host: &str, ip: &str) -> String {
    let seed = format!("{host}-{ip}");
    let normalized = seed
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string();

    if normalized.is_empty() {
        "peer-local".into()
    } else {
        format!("peer-{normalized}")
    }
}

fn scan_for_peers(local_peer: &LanPeer, base_port: u16) -> Result<Vec<LanPeer>, String> {
    let socket = UdpSocket::bind("0.0.0.0:0")
        .map_err(|error| format!("failed to open UDP scan socket: {error}"))?;
    socket
        .set_broadcast(true)
        .map_err(|error| format!("failed to enable UDP broadcast: {error}"))?;
    socket
        .set_read_timeout(Some(Duration::from_millis(250)))
        .map_err(|error| format!("failed to set UDP scan timeout: {error}"))?;

    for target in broadcast_addrs(base_port) {
        let _ = send_discovery_packet(&socket, "announce", local_peer, target);
    }
    // Fallback for networks that drop broadcast but forward unicast.
    for target in unicast_sweep_targets(base_port) {
        let _ = send_discovery_packet(&socket, "announce", local_peer, target.as_str());
    }

    let started = Instant::now();
    let mut buffer = [0_u8; 4096];
    let mut peers = Vec::new();

    while started.elapsed() < Duration::from_millis(1400) {
        if let Ok((length, source)) = socket.recv_from(&mut buffer) {
            if let Some(packet) = decode_discovery_packet(&buffer[..length]) {
                if let Some(incoming) =
                    peer_from_discovery_packet(packet, source.ip().to_string(), &local_peer.id)
                {
                    if peer_visible_to_local_peer(local_peer, &incoming.peer) {
                        merge_peer_entry(&mut peers, incoming.peer);
                    }
                }
            }
        }
    }

    Ok(peers)
}

fn probe_known_peer_targets(local_peer: &LanPeer, targets: &[String]) -> Vec<LanPeer> {
    if targets.is_empty() {
        return Vec::new();
    }

    let Ok(socket) = UdpSocket::bind("0.0.0.0:0") else {
        return Vec::new();
    };
    let _ = socket.set_read_timeout(Some(Duration::from_millis(120)));
    for target in targets {
        let _ = send_discovery_packet(&socket, "probe", local_peer, target.as_str());
    }

    let started = Instant::now();
    let mut buffer = [0_u8; 4096];
    let mut peers = Vec::new();
    while started.elapsed() < Duration::from_millis(700) {
        let Ok((length, source)) = socket.recv_from(&mut buffer) else {
            continue;
        };
        let Some(packet) = decode_discovery_packet(&buffer[..length]) else {
            continue;
        };
        let Some(incoming) =
            peer_from_discovery_packet(packet, source.ip().to_string(), &local_peer.id)
        else {
            continue;
        };
        if peer_visible_to_local_peer(local_peer, &incoming.peer) {
            merge_peer_entry(&mut peers, incoming.peer);
        }
    }
    peers
}

fn probe_for_peer(local_peer: &LanPeer, host: &str, base_port: u16) -> Result<LanPeer, String> {
    let (host, explicit_port) = split_host_port(host.trim());
    let socket = UdpSocket::bind("0.0.0.0:0")
        .map_err(|error| format!("failed to open UDP probe socket: {error}"))?;
    socket
        .set_read_timeout(Some(Duration::from_millis(250)))
        .map_err(|error| format!("failed to set UDP probe timeout: {error}"))?;

    // With an explicit `host:port` probe exactly that port (e.g. a forwarded
    // public endpoint reached across NAT); otherwise the peer may have drifted
    // off the base port onto a neighbour, so probe the whole discovery span.
    let ports = match explicit_port {
        Some(port) => vec![port],
        None => discovery_target_ports(base_port),
    };
    for port in &ports {
        let target = format!("{host}:{port}");
        let _ = send_discovery_packet(&socket, "probe", local_peer, target.as_str());
    }

    let started = Instant::now();
    let mut buffer = [0_u8; 4096];
    while started.elapsed() < Duration::from_millis(1800) {
        if let Ok((length, source)) = socket.recv_from(&mut buffer) {
            if let Some(packet) = decode_discovery_packet(&buffer[..length]) {
                if let Some(incoming) =
                    peer_from_discovery_packet(packet, source.ip().to_string(), &local_peer.id)
                {
                    if peer_visible_to_local_peer(local_peer, &incoming.peer) {
                        return Ok(incoming.peer);
                    }
                }
            }
        }
    }

    let port_hint = match (ports.first(), ports.last()) {
        (Some(first), Some(last)) if first != last => format!("UDP {first}-{last}"),
        (Some(only), _) => format!("UDP {only}"),
        _ => format!("UDP {base_port}"),
    };
    Err(format!(
        "no mykvm peer answered at {host} ({port_hint}); \
         make sure mykvm is running on that device and UDP is allowed"
    ))
}

fn request_pairing_for_peer(
    local_peer: &LanPeer,
    host: &str,
    base_port: u16,
) -> Result<LanPeer, String> {
    let (host, ports) = pairing_probe_targets(host, base_port);
    let socket = UdpSocket::bind("0.0.0.0:0")
        .map_err(|error| format!("failed to open UDP pairing socket: {error}"))?;
    socket
        .set_read_timeout(Some(Duration::from_millis(250)))
        .map_err(|error| format!("failed to set UDP pairing timeout: {error}"))?;

    for port in &ports {
        let target = format!("{host}:{port}");
        let _ = send_discovery_packet(&socket, "pair-request", local_peer, target.as_str());
    }

    let started = Instant::now();
    let mut buffer = [0_u8; 4096];
    while started.elapsed() < Duration::from_millis(1800) {
        if let Ok((length, source)) = socket.recv_from(&mut buffer) {
            if let Some(packet) = decode_discovery_packet(&buffer[..length]) {
                if let Some(incoming) =
                    peer_from_discovery_packet(packet, source.ip().to_string(), &local_peer.id)
                {
                    if incoming.kind == "pair-challenge"
                        && pair_challenge_usable_for_local_peer(local_peer, &incoming.peer)
                    {
                        return Ok(incoming.peer);
                    }
                }
            }
        }
    }

    Err(format!(
        "no pairing challenge received from {host}; make sure the client is running and reachable"
    ))
}

fn confirm_pairing_for_peer(
    local_peer: &LanPeer,
    quic_transport: &quic_transport::TransportHandle,
    pair_secret: &str,
    host: &str,
    code: &str,
    base_port: u16,
) -> Result<LanPeer, String> {
    let challenge_peer = request_pairing_for_peer(local_peer, host, base_port)?;
    if challenge_peer.transport_public_key.trim().is_empty()
        || challenge_peer.protocol_version != quic_transport::PROTOCOL_VERSION
        || challenge_peer.quic_port == 0
    {
        return Err("客户端暂不支持安全配对确认，请升级客户端后重试。".into());
    }

    let fields = DiscoveryPairingFields {
        code: Some(code.trim().into()),
        cluster_id: Some(local_peer.cluster_id.clone()),
        secret: Some(pair_secret.trim().into()),
        error: None,
    };
    let payload = encode_discovery_payload("pair-confirm", local_peer, fields)?;
    let target_addr = format!("{}:{}", challenge_peer.ip, challenge_peer.quic_port);
    let endpoint = quic_transport.peer(
        target_addr,
        challenge_peer.transport_public_key.clone(),
        challenge_peer.protocol_version,
    );
    quic_transport
        .send_stream_expect_ack(endpoint, payload)
        .map_err(|error| format!("failed to send encrypted pairing confirmation: {error}"))?;

    let paired_peer = probe_for_peer(local_peer, host, base_port)?;
    if paired_peer.pairing_required {
        return Err("配对未被客户端接受，请检查验证码后重试。".into());
    }

    Ok(paired_peer)
}

fn pairing_probe_targets(host: &str, base_port: u16) -> (String, Vec<u16>) {
    let (host, explicit_port) = split_host_port(host.trim());
    let ports = match explicit_port {
        Some(port) => vec![port],
        None => discovery_target_ports(base_port),
    };
    (host, ports)
}

/// Splits a manual `host` entry into a host and an optional explicit port. A
/// parseable trailing `:<port>` (e.g. `203.0.113.7:47833`) pins the probe to
/// that exact port — useful across NAT/port-forwarding where the peer is not on
/// the default discovery port. Bare hosts return `None`.
fn split_host_port(input: &str) -> (String, Option<u16>) {
    if let Some((host, port)) = input.rsplit_once(':') {
        let host = host.trim();
        if !host.is_empty() {
            if let Ok(port) = port.trim().parse::<u16>() {
                return (host.to_string(), Some(port));
            }
        }
    }
    (input.trim().to_string(), None)
}

fn send_discovery_packet(
    socket: &UdpSocket,
    kind: &str,
    local_peer: &LanPeer,
    target: impl std::net::ToSocketAddrs,
) -> Result<(), String> {
    send_discovery_packet_with_pairing(
        socket,
        kind,
        local_peer,
        target,
        DiscoveryPairingFields::default(),
    )
}

fn send_discovery_packet_with_pairing(
    socket: &UdpSocket,
    kind: &str,
    local_peer: &LanPeer,
    target: impl std::net::ToSocketAddrs,
    pairing: DiscoveryPairingFields,
) -> Result<(), String> {
    let payload = encode_discovery_payload(kind, local_peer, pairing)?;
    socket
        .send_to(&payload, target)
        .map(|_| ())
        .map_err(|error| format!("failed to send discovery packet: {error}"))
}

fn encode_discovery_payload(
    kind: &str,
    local_peer: &LanPeer,
    pairing: DiscoveryPairingFields,
) -> Result<Vec<u8>, String> {
    let mut peer = local_peer.clone();
    peer.last_seen_ms = now_ms();
    let packet = DiscoveryPacket {
        protocol: DISCOVERY_PROTOCOL.into(),
        kind: kind.into(),
        peer,
        pairing_code: pairing.code,
        pair_cluster_id: pairing.cluster_id,
        pair_secret: pairing.secret,
        pairing_error: pairing.error,
    };
    encode_wire_packet(&packet)
        .map_err(|error| format!("failed to encode discovery packet: {error}"))
}

fn decode_discovery_packet(payload: &[u8]) -> Option<DiscoveryPacket> {
    let packet = decode_wire_packet::<DiscoveryPacket>(payload)?;
    (packet.protocol == DISCOVERY_PROTOCOL).then_some(packet)
}

fn peer_from_discovery_packet(
    packet: DiscoveryPacket,
    source_ip: String,
    local_peer_id: &str,
) -> Option<IncomingDiscovery> {
    if packet.peer.id == local_peer_id {
        return None;
    }

    let mut peer = packet.peer;
    peer.ip = source_ip;
    if peer.quic_port == 0 {
        peer.quic_port = peer.transport_port;
    }
    if peer.protocol_version == 0 {
        peer.protocol_version = default_protocol_version();
    }
    if peer.transport_public_key.trim().is_empty()
        || peer.protocol_version != quic_transport::PROTOCOL_VERSION
    {
        peer.input_ready = false;
    }
    peer.last_seen_ms = now_ms();
    Some(IncomingDiscovery {
        kind: packet.kind,
        peer,
        pairing_code: packet.pairing_code,
        pair_cluster_id: packet.pair_cluster_id,
        pair_secret: packet.pair_secret,
    })
}

fn merge_peer(peers: &Arc<Mutex<Vec<LanPeer>>>, next_peer: LanPeer) {
    if let Ok(mut peers) = peers.lock() {
        merge_peer_entry(&mut peers, next_peer);
    }
}

fn merge_peer_entry(peers: &mut Vec<LanPeer>, next_peer: LanPeer) {
    let now = now_ms();
    prune_stale_peer_entries(peers, now);

    if let Some(existing) = peers.iter_mut().find(|peer| peer.id == next_peer.id) {
        if existing.input_ready != next_peer.input_ready
            || existing.pairing_required != next_peer.pairing_required
            || existing.ip != next_peer.ip
            || existing.transport_port != next_peer.transport_port
            || existing.quic_port != next_peer.quic_port
        {
            log::info!(
                "discovery peer updated id={} name={} ip={} discovery_port={} quic_port={} input_ready={} pairing_required={}",
                next_peer.id,
                next_peer.name,
                next_peer.ip,
                next_peer.transport_port,
                next_peer.quic_port,
                next_peer.input_ready,
                next_peer.pairing_required
            );
        }
        *existing = next_peer;
        return;
    }

    if peers.len() >= MAX_DISCOVERY_PEERS {
        if let Some((oldest_index, _)) = peers
            .iter()
            .enumerate()
            .min_by_key(|(_, peer)| peer.last_seen_ms)
        {
            peers.swap_remove(oldest_index);
        }
    }

    log::debug!(
        "discovery peer found id={} name={} ip={} discovery_port={} quic_port={} input_ready={} pairing_required={}",
        next_peer.id,
        next_peer.name,
        next_peer.ip,
        next_peer.transport_port,
        next_peer.quic_port,
        next_peer.input_ready,
        next_peer.pairing_required
    );
    peers.push(next_peer);
}

fn active_peers(peers: &Arc<Mutex<Vec<LanPeer>>>, local_peer_id: &str) -> Vec<LanPeer> {
    let now = now_ms();
    peers
        .lock()
        .map(|peers| {
            peers
                .iter()
                .filter(|peer| {
                    peer.id != local_peer_id && now.saturating_sub(peer.last_seen_ms) <= PEER_TTL_MS
                })
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

fn peer_visible_to_layout(layout: &LayoutState, peer: &LanPeer) -> bool {
    if peer.pairing_required {
        return layout.machine_role == "server";
    }

    let cluster_id = layout.cluster_id.trim();
    !cluster_id.is_empty() && peer.cluster_id == cluster_id
}

fn peer_visible_to_local_peer(local_peer: &LanPeer, peer: &LanPeer) -> bool {
    if peer.pairing_required {
        return local_peer.machine_role == "server";
    }

    let cluster_id = local_peer.cluster_id.trim();
    !cluster_id.is_empty() && peer.cluster_id == cluster_id
}

fn should_reply_to_discovery(layout: &LayoutState, peer: &LanPeer) -> bool {
    if peer_visible_to_layout(layout, peer) {
        return true;
    }

    if layout.machine_role == "client" && pairing_required(layout) {
        return peer.machine_role == "server";
    }

    layout.machine_role == "client" && is_paired_controller(layout, peer)
}

fn is_paired_controller(layout: &LayoutState, peer: &LanPeer) -> bool {
    layout
        .paired_controllers
        .iter()
        .any(|controller| paired_controller_identity_matches_peer(controller, peer))
}

fn paired_controller_identity_matches_peer(controller: &PairedController, peer: &LanPeer) -> bool {
    (!peer.id.trim().is_empty() && controller.id == peer.id)
        || controller.id == peer_device_id(peer)
        || (!peer.transport_public_key.trim().is_empty()
            && controller.transport_public_key == peer.transport_public_key)
}

fn paired_controller_can_repair_with_peer(controller: &PairedController, peer: &LanPeer) -> bool {
    if paired_controller_identity_matches_peer(controller, peer) {
        return true;
    }

    text_matches(&controller.name, &peer.name)
        || same_host(&controller.host, &peer.host)
        || same_host(&peer.host, &controller.host)
        || text_matches(&controller.ip, &peer.ip)
}

fn text_matches(left: &str, right: &str) -> bool {
    let left = left.trim();
    let right = right.trim();
    !left.is_empty() && !right.is_empty() && left.eq_ignore_ascii_case(right)
}

fn pair_challenge_usable_for_local_peer(local_peer: &LanPeer, peer: &LanPeer) -> bool {
    if !peer.machine_role.trim().is_empty() && peer.machine_role != "client" {
        return false;
    }
    if peer.pairing_required {
        return true;
    }

    peer_visible_to_local_peer(local_peer, peer) || !peer.transport_public_key.trim().is_empty()
}

fn handle_pairing_stream_packet(
    payload: &[u8],
    source: SocketAddr,
    layout_state: &Arc<Mutex<LayoutState>>,
    pairing_challenge: &Arc<Mutex<Option<PairingChallenge>>>,
    config_path: &PathBuf,
    peers: &Arc<Mutex<Vec<LanPeer>>>,
) -> bool {
    let Some(packet) = decode_discovery_packet(payload) else {
        return false;
    };
    if packet.kind != "pair-confirm" {
        return false;
    }

    let local_peer_id = layout_state
        .lock()
        .map(|layout| local_peer_from_layout(&layout).id)
        .unwrap_or_default();
    let Some(incoming) =
        peer_from_discovery_packet(packet, source.ip().to_string(), &local_peer_id)
    else {
        return false;
    };

    match complete_pairing_from_confirm(
        layout_state,
        pairing_challenge,
        config_path,
        &incoming.peer,
        incoming.pairing_code,
        incoming.pair_cluster_id,
        incoming.pair_secret,
    ) {
        Ok(()) => {
            merge_peer(peers, incoming.peer);
            sync_layout_peer_presence(layout_state, peers);
            true
        }
        Err(error) => {
            log::warn!("pairing confirmation rejected: {error}");
            false
        }
    }
}

fn begin_pairing_challenge(
    pairing_challenge: &Arc<Mutex<Option<PairingChallenge>>>,
    layout: &LayoutState,
    requester: &LanPeer,
    requester_ip: String,
) -> bool {
    if layout.machine_role != "client" {
        return false;
    }
    if requester.machine_role != "server" {
        return false;
    }
    // Accept a fresh handshake when we have no pairing yet, OR when the
    // requester looks like a controller we were already paired with. Repair
    // matching intentionally includes host/name/IP so a rotated transport
    // certificate does not trap a headless client behind its old controller key.
    let requester_already_known = layout
        .paired_controllers
        .iter()
        .any(|controller| paired_controller_can_repair_with_peer(controller, requester));
    if !pairing_required(layout) && !requester_already_known {
        return false;
    }

    let now = Instant::now();
    let expires_at = now + Duration::from_millis(PAIRING_CODE_TTL_MS);
    let expires_at_ms = now_ms().saturating_add(PAIRING_CODE_TTL_MS);

    if let Ok(mut challenge) = pairing_challenge.lock() {
        if let Some(existing) = challenge.as_mut() {
            if existing.expires_at > now {
                if existing.requester_id == requester.id {
                    if existing.attempts > 0 {
                        existing.code = random_pairing_code();
                        existing.expires_at = expires_at;
                        existing.expires_at_ms = expires_at_ms;
                        existing.attempts = 0;
                    }
                    existing.requester_ip = requester_ip;
                    existing.requester_host = requester.host.clone();
                    existing.requester_public_key = requester.transport_public_key.clone();
                    existing.requester_protocol_version = requester.protocol_version;
                    return true;
                }
                return false;
            }
        }

        *challenge = Some(PairingChallenge {
            code: random_pairing_code(),
            requester_id: requester.id.clone(),
            requester_name: requester.name.clone(),
            requester_ip,
            requester_host: requester.host.clone(),
            requester_public_key: requester.transport_public_key.clone(),
            requester_protocol_version: requester.protocol_version,
            expires_at,
            expires_at_ms,
            attempts: 0,
        });
        return true;
    }

    false
}

fn complete_pairing_from_confirm(
    layout_state: &Arc<Mutex<LayoutState>>,
    pairing_challenge: &Arc<Mutex<Option<PairingChallenge>>>,
    config_path: &PathBuf,
    requester: &LanPeer,
    code: Option<String>,
    cluster_id: Option<String>,
    pair_secret: Option<String>,
) -> Result<(), String> {
    let code = code.unwrap_or_default();
    let cluster_id = cluster_id.unwrap_or_default();
    let pair_secret = pair_secret.unwrap_or_default();
    if code.trim().is_empty() || cluster_id.trim().is_empty() || pair_secret.trim().is_empty() {
        return Err("配对请求缺少验证码或组信息。".into());
    }

    {
        let mut challenge = pairing_challenge
            .lock()
            .map_err(|_| "pairing challenge lock poisoned".to_string())?;
        let Some(existing) = challenge.as_mut() else {
            return Err("验证码已过期，请重新发起配对。".into());
        };
        if existing.expires_at <= Instant::now() {
            *challenge = None;
            return Err("验证码已过期，请重新发起配对。".into());
        }
        if existing.requester_id != requester.id
            || (!existing.requester_public_key.trim().is_empty()
                && existing.requester_public_key != requester.transport_public_key)
        {
            return Err("配对请求来源不一致，请重新发起配对。".into());
        }
        if existing.code != code.trim() {
            existing.attempts = existing.attempts.saturating_add(1);
            if existing.attempts >= PAIRING_MAX_ATTEMPTS {
                *challenge = None;
            }
            return Err("验证码不正确。".into());
        }
        *challenge = None;
    }

    let snapshot = {
        let mut layout = layout_state
            .lock()
            .map_err(|_| "layout state lock poisoned".to_string())?;
        if layout.machine_role != "client" {
            return Err("只有客户端可以接受服务端配对。".into());
        }

        layout.cluster_id = cluster_id.trim().into();
        layout.pair_secret = pair_secret.trim().into();
        layout.input_mode = "receive".into();
        layout.paired_controllers = vec![PairedController {
            id: requester.id.clone(),
            name: requester.name.clone(),
            host: requester.host.clone(),
            ip: requester.ip.clone(),
            transport_public_key: requester.transport_public_key.clone(),
            protocol_version: requester.protocol_version,
            cluster_id: layout.cluster_id.clone(),
            paired_at_ms: now_ms(),
        }];
        layout.clone()
    };

    write_layout_to_disk(config_path, &snapshot)
}

fn prune_stale_peers(peers: &Arc<Mutex<Vec<LanPeer>>>) {
    if let Ok(mut peers) = peers.lock() {
        prune_stale_peer_entries(&mut peers, now_ms());
    }
}

fn prune_stale_peer_entries(peers: &mut Vec<LanPeer>, now: u64) {
    for peer in peers
        .iter()
        .filter(|peer| now.saturating_sub(peer.last_seen_ms) > PEER_TTL_MS)
    {
        log::info!(
            "discovery peer stale id={} name={} ip={} last_seen_age_ms={} ttl_ms={}",
            peer.id,
            peer.name,
            peer.ip,
            now.saturating_sub(peer.last_seen_ms),
            PEER_TTL_MS
        );
    }
    peers.retain(|peer| now.saturating_sub(peer.last_seen_ms) <= PEER_TTL_MS);
}

fn discovery_detail(peer_count: usize, listening: bool, port: u16) -> String {
    let mode = if listening {
        "listening and broadcasting"
    } else {
        "ready to scan"
    };
    format!("UDP {port} is {mode}; {peer_count} LAN peer(s) detected.")
}

/// Broadcast destinations for discovery, fanned out across the discovery port
/// span (`base_port ..= base_port + DISCOVERY_PORT_SPAN - 1`). Sending to the
/// whole span — rather than a single port — lets us reach peers that drifted
/// onto a neighbouring port when their preferred port was momentarily taken.
pub(crate) fn broadcast_addrs(base_port: u16) -> Vec<String> {
    broadcast_addrs_for_ips(base_port, &local_ipv4_addresses())
}

fn broadcast_addrs_for_ips(base_port: u16, local_ips: &[Ipv4Addr]) -> Vec<String> {
    let mut addresses = Vec::new();
    for port in discovery_target_ports(base_port) {
        addresses.push(format!("255.255.255.255:{port}"));
        for ip in local_ips {
            let [a, b, c, _] = ip.octets();
            addresses.push(format!("{a}.{b}.{c}.255:{port}"));
        }
    }

    addresses.sort();
    addresses.dedup();
    addresses
}

/// Directed discovery destinations for peers we already know about. Pairing and
/// manual probing use unicast, but the long-running discovery loop used to rely
/// only on broadcast after that. On LANs where broadcast is flaky or filtered,
/// the peer would age out after `PEER_TTL_MS` even though direct UDP still
/// worked. Keep paired/configured machines warm with a small directed announce
/// fan-out.
fn known_peer_discovery_targets(layout: &LayoutState, base_port: u16) -> Vec<String> {
    let base_ports = discovery_target_ports(base_port);
    let mut targets = Vec::new();

    for device in layout
        .devices
        .iter()
        .filter(|device| device.role != "local")
    {
        let ports = known_peer_ports(base_port, device.transport_port);
        push_host_discovery_targets(&mut targets, &device.host, &ports);
    }

    for controller in &layout.paired_controllers {
        push_host_discovery_targets(&mut targets, &controller.ip, &base_ports);
        push_host_discovery_targets(&mut targets, &controller.host, &base_ports);
    }

    targets.sort();
    targets.dedup();
    targets
}

fn known_peer_ports(base_port: u16, stored_port: u16) -> Vec<u16> {
    let mut ports = discovery_target_ports(base_port);
    let stored_port = normalize_transport_port(stored_port);
    if !ports.contains(&stored_port) {
        ports.push(stored_port);
    }
    ports.sort();
    ports.dedup();
    ports
}

fn push_host_discovery_targets(targets: &mut Vec<String>, host_value: &str, ports: &[u16]) {
    for host in host_candidates(host_value) {
        let (host, explicit_port) = split_host_port(&host);
        if host.trim().is_empty() {
            continue;
        }

        if let Some(port) = explicit_port {
            targets.push(format!("{host}:{port}"));
            continue;
        }

        for port in ports {
            targets.push(format!("{host}:{port}"));
        }
    }
}

fn host_candidates(host_value: &str) -> Vec<String> {
    let mut candidates: Vec<String> = host_value
        .split('/')
        .map(|part| part.trim().to_string())
        .filter(|part| !part.is_empty())
        .collect();

    candidates.sort();
    candidates.dedup();
    candidates
}

/// The consecutive discovery ports we aim traffic at, starting from `base`.
fn discovery_target_ports(base: u16) -> Vec<u16> {
    let base = normalize_transport_port(base);
    let mut ports = Vec::new();
    for offset in 0..DISCOVERY_PORT_SPAN {
        let Some(port) = base.checked_add(offset) else {
            break;
        };
        if port > TRANSPORT_PORT_MAX {
            break;
        }
        ports.push(port);
    }
    ports
}

/// The base discovery port peers rendezvous on: the canonical port in auto mode,
/// or the user's configured port when pinned. Discovery traffic fans out from
/// here across `DISCOVERY_PORT_SPAN`, independent of whichever port we actually
/// managed to bind locally.
fn discovery_base_port(layout: &LayoutState) -> u16 {
    if layout.transport_port_mode == "auto" {
        default_transport_port()
    } else {
        normalize_transport_port(layout.transport_port)
    }
}

/// Every other host address in our local /24, used as a fallback when a network
/// drops broadcast traffic (common with Wi-Fi "AP/client isolation" and some
/// managed switches) but still forwards unicast between clients.
pub(crate) fn unicast_sweep_targets(port: u16) -> Vec<String> {
    unicast_sweep_targets_for_ips(port, &local_ipv4_addresses())
}

fn unicast_sweep_targets_for_ips(port: u16, local_ips: &[Ipv4Addr]) -> Vec<String> {
    let ports = discovery_target_ports(port);
    let mut targets = Vec::new();

    for ip in local_ips {
        let [a, b, c, self_host] = ip.octets();
        let subnet_prefix = format!("{a}.{b}.{c}");
        targets.extend(
            (1..=254u8)
                .filter(|host| *host != self_host)
                .flat_map(|host| {
                    let subnet_prefix = subnet_prefix.clone();
                    ports
                        .iter()
                        .map(move |port| format!("{subnet_prefix}.{host}:{port}"))
                }),
        );
    }

    targets.sort();
    targets.dedup();
    targets
}

/// Adds (once per process) an inbound UDP allow rule for this binary to Windows
/// Defender Firewall so LAN peers can reach our discovery and QUIC sockets.
/// Requires elevation; when we are not elevated, skip the `netsh` calls so
/// startup does not block on commands that cannot succeed.
#[cfg(target_os = "windows")]
fn ensure_windows_firewall_rule() {
    if WINDOWS_FIREWALL_ENSURED.swap(true, Ordering::Relaxed) {
        return;
    }

    if !is_windows_process_elevated().unwrap_or(false) {
        log::warn!(
            "skipping Windows Defender Firewall rule setup without administrator rights; \
             if LAN peers cannot find this device, allow MyKVM through the firewall for all \
             networks or relaunch MyKVM as administrator"
        );
        return;
    }

    let Ok(exe) = env::current_exe() else {
        return;
    };
    let exe = exe.to_string_lossy().to_string();
    let rule_name = "MyKVM (UDP-In)";

    // Drop any stale rule first so re-installs/path changes don't pile up.
    let _ = Command::new("netsh")
        .args([
            "advfirewall",
            "firewall",
            "delete",
            "rule",
            &format!("name={rule_name}"),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    let status = Command::new("netsh")
        .args([
            "advfirewall",
            "firewall",
            "add",
            "rule",
            &format!("name={rule_name}"),
            "dir=in",
            "action=allow",
            &format!("program={exe}"),
            "protocol=udp",
            "profile=any",
            "enable=yes",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    match status {
        Ok(status) if status.success() => {
            log::info!("ensured Windows Defender Firewall inbound UDP rule for MyKVM");
        }
        _ => {
            log::warn!(
                "could not add Windows Defender Firewall rule (administrator rights required); \
                 if LAN peers cannot find this device, allow MyKVM through the firewall for all \
                 networks or relaunch MyKVM as administrator"
            );
        }
    }
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

fn random_hex(byte_count: usize) -> String {
    let rng = SystemRandom::new();
    let mut bytes = vec![0_u8; byte_count];
    if rng.fill(&mut bytes).is_err() {
        let fallback = now_ms().to_le_bytes();
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = fallback[index % fallback.len()] ^ (index as u8).wrapping_mul(31);
        }
    }

    let mut output = String::with_capacity(byte_count * 2);
    for byte in bytes {
        output.push_str(&format!("{byte:02x}"));
    }
    output
}

fn random_pairing_code() -> String {
    let rng = SystemRandom::new();
    let mut bytes = [0_u8; 4];
    if rng.fill(&mut bytes).is_err() {
        bytes = now_ms().to_le_bytes()[..4].try_into().unwrap_or([0; 4]);
    }
    format!("{:06}", u32::from_le_bytes(bytes) % 1_000_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_screen(device_id: &str) -> Screen {
        Screen {
            id: format!("{device_id}-display-1"),
            device_id: device_id.into(),
            name: "Display".into(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            scale: 1.0,
            is_primary: true,
        }
    }

    fn test_layout() -> LayoutState {
        LayoutState {
            devices: vec![
                Device {
                    id: "local-device".into(),
                    name: "Local".into(),
                    platform: "macos".into(),
                    host: "local / 10.0.0.1".into(),
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
                    screens: vec![test_screen("local-device")],
                },
                Device {
                    id: "peer-client-10-0-0-2".into(),
                    name: "Client".into(),
                    platform: "windows".into(),
                    host: "client / 10.0.0.2".into(),
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
                    screens: vec![test_screen("peer-client-10-0-0-2")],
                },
            ],
            active_device_id: "local-device".into(),
            selected_screen_id: "local-device-display-1".into(),
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
            transport_port: 49152,
            quic_port: 49153,
            modifier_remap: true,
            modifier_map: default_modifier_map(),
            edge_switch_hotkey: default_edge_switch_hotkey(),
            screen_switch_hotkeys: ScreenSwitchHotkeys::default(),
        }
    }

    fn test_peer() -> LanPeer {
        LanPeer {
            id: "peer-client-10-0-0-2".into(),
            name: "Client".into(),
            platform: "windows".into(),
            machine_role: "client".into(),
            cluster_id: "cluster-test".into(),
            pairing_required: false,
            host: "client".into(),
            ip: "10.0.0.2".into(),
            transport_port: 52000,
            quic_port: 52001,
            transport_public_key: "peer-public-key".into(),
            protocol_version: quic_transport::PROTOCOL_VERSION,
            screen_count: 1,
            input_ready: true,
            upgrading: false,
            screens: vec![LanPeerScreen {
                id: "local-display-1".into(),
                name: "Display".into(),
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
                scale: 1.0,
                is_primary: true,
            }],
            app_version: "test".into(),
            last_seen_ms: now_ms(),
        }
    }

    #[test]
    fn app_exit_policy_blocks_implicit_last_window_exit() {
        assert!(!should_allow_app_exit_request(None, false));
        assert!(should_allow_app_exit_request(None, true));
        assert!(should_allow_app_exit_request(
            Some(tauri::RESTART_EXIT_CODE),
            false
        ));
    }

    #[test]
    fn runtime_toggle_shortcut_normalizes_command_aliases() {
        assert_eq!(
            canonical_runtime_toggle_shortcut("command+1").expect("shortcut"),
            Some("super+1".into())
        );
        assert_eq!(
            canonical_runtime_toggle_shortcut("meta+1").expect("legacy shortcut"),
            Some("super+1".into())
        );
    }

    #[test]
    fn runtime_toggle_shortcut_supports_function_key_and_disabled() {
        assert_eq!(
            canonical_runtime_toggle_shortcut("f12").expect("shortcut"),
            Some("F12".into())
        );
        assert_eq!(
            canonical_runtime_toggle_shortcut("disabled").expect("disabled shortcut"),
            None
        );
    }

    #[test]
    fn runtime_toggle_shortcut_rejects_single_letter_globals() {
        assert!(canonical_runtime_toggle_shortcut("k").is_err());
    }

    #[test]
    fn runtime_toggle_shortcut_disabled_for_client_role() {
        let mut layout = test_layout();
        layout.machine_role = "client".into();

        assert_eq!(
            runtime_toggle_shortcut_for_layout(&layout).expect("shortcut"),
            None
        );
    }

    #[test]
    fn screen_switch_shortcuts_disabled_for_client_role() {
        let mut layout = test_layout();
        layout.machine_role = "client".into();

        assert_eq!(
            screen_switch_shortcuts_for_layout(&layout),
            ScreenSwitchHotkeys {
                left: String::new(),
                right: String::new(),
                up: String::new(),
                down: String::new(),
            }
        );
    }

    #[test]
    fn peer_presence_marks_missing_remote_offline() {
        let mut layout = test_layout();

        apply_peer_presence(&mut layout, &[]);

        assert!(layout.devices[0].online);
        assert_eq!(layout.devices[0].transport_port, 49152);
        assert!(!layout.devices[1].online);
        assert!(!layout.devices[1].input_ready);
    }

    #[test]
    fn peer_presence_updates_live_address_and_port() {
        let mut layout = test_layout();
        let peer = test_peer();

        apply_peer_presence(&mut layout, &[peer]);

        assert!(layout.devices[1].online);
        assert!(layout.devices[1].input_ready);
        assert_eq!(layout.devices[1].host, "10.0.0.2");
        assert_eq!(layout.devices[1].transport_port, 52000);
    }

    #[test]
    fn peer_presence_matches_same_cluster_host_after_identity_rotation() {
        let mut layout = test_layout();
        let mut peer = test_peer();
        peer.id = "rotated-client-id".into();
        peer.transport_public_key = "rotated-client-key".into();

        apply_peer_presence(&mut layout, &[peer]);

        assert!(layout.devices[1].online);
        assert!(layout.devices[1].input_ready);
        assert_eq!(layout.devices[1].transport_public_key, "rotated-client-key");
    }

    #[test]
    fn discovery_keeps_peer_through_short_heartbeat_gap() {
        let mut peer = test_peer();
        peer.last_seen_ms = now_ms().saturating_sub(45_000);
        let peers = Arc::new(Mutex::new(vec![peer.clone()]));

        assert_eq!(active_peers(&peers, "local-device").len(), 1);

        let mut entries = vec![peer];
        prune_stale_peer_entries(&mut entries, now_ms());
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn peer_presence_keeps_discovered_peer_online_without_input_ready() {
        let mut layout = test_layout();
        let mut peer = test_peer();
        peer.input_ready = false;

        apply_peer_presence(&mut layout, &[peer]);

        assert!(layout.devices[1].online);
        assert!(!layout.devices[1].input_ready);
        assert_eq!(layout.devices[1].host, "10.0.0.2");
    }

    #[test]
    fn edge_drop_specs_follow_server_screen_adjacency() {
        let mut layout = test_layout();
        layout.devices[1].screens[0].x = 1920;

        let specs = edge_drop_specs_for_layout(&layout, &[]);

        assert_eq!(specs.len(), 1);
        let spec = &specs[0];
        assert_eq!(spec.target_device_id, "peer-client-10-0-0-2");
        assert_eq!(spec.x, 1912.0);
        assert_eq!(spec.y, 0.0);
        assert_eq!(spec.width, EDGE_DROP_THICKNESS as f64);
        assert_eq!(spec.height, 1080.0);
    }

    #[test]
    fn edge_drop_specs_respect_file_transfer_setting() {
        let mut layout = test_layout();
        layout.devices[1].screens[0].x = 1920;
        layout.file_transfer_enabled = false;

        let specs = edge_drop_specs_for_layout(&layout, &[]);

        assert!(specs.is_empty());
    }

    #[test]
    fn edge_drop_specs_pause_when_main_window_is_hidden() {
        let mut layout = test_layout();
        layout.devices[1].screens[0].x = 1920;

        assert_eq!(
            edge_drop_specs_for_window_visibility(&layout, &[], true).len(),
            1
        );
        assert!(edge_drop_specs_for_window_visibility(&layout, &[], false).is_empty());
    }

    #[test]
    fn edge_drop_specs_skip_internal_client_monitor_edges() {
        let mut layout = test_layout();
        layout.machine_role = "client".into();
        layout.input_mode = "receive".into();
        layout.devices.truncate(1);
        let mut second_screen = test_screen("local-device");
        second_screen.id = "local-device-display-2".into();
        second_screen.x = 1920;
        layout.devices[0].screens.push(second_screen);
        layout.paired_controllers = vec![PairedController {
            id: "peer-server-10-0-0-1".into(),
            name: "Server".into(),
            host: "server".into(),
            ip: "10.0.0.1".into(),
            transport_public_key: "server-public-key".into(),
            protocol_version: quic_transport::PROTOCOL_VERSION,
            cluster_id: layout.cluster_id.clone(),
            paired_at_ms: now_ms(),
        }];
        let mut peer = test_peer();
        peer.id = "peer-server-10-0-0-1".into();
        peer.name = "Server".into();
        peer.machine_role = "server".into();
        peer.transport_public_key = "server-public-key".into();

        let specs = edge_drop_specs_for_layout(&layout, &[peer]);

        assert_eq!(specs.len(), 6);
        assert!(specs.iter().all(|spec| {
            spec.target_device_id == "peer-server-10-0-0-1" && spec.width > 0.0 && spec.height > 0.0
        }));
        assert!(!specs.iter().any(|spec| {
            spec.x == (1920 - EDGE_DROP_THICKNESS) as f64
                && spec.y == 0.0
                && spec.width == EDGE_DROP_THICKNESS as f64
                && spec.height == 1080.0
        }));
        assert!(specs.iter().any(|spec| {
            spec.x == (3840 - EDGE_DROP_THICKNESS) as f64
                && spec.y == 0.0
                && spec.width == EDGE_DROP_THICKNESS as f64
                && spec.height == 1080.0
        }));
    }

    #[test]
    fn edge_drop_label_handles_non_ascii_ids() {
        let mut local = test_screen("local-device");
        local.id = "显示器".into();
        let mut remote = test_screen("remote-device");
        remote.id = "远程".into();

        let label = edge_drop_label("设备", &local, Some(&remote), EdgeDropSide::Right);

        assert!(label.starts_with(EDGE_DROP_LABEL_PREFIX));
        assert!(label.contains("id-"));
        assert!(label.ends_with("-right"));
    }

    #[test]
    fn peer_presence_does_not_add_unapproved_peer_screens() {
        let mut layout = test_layout();
        layout.devices.truncate(1);
        let peer = test_peer();

        apply_peer_presence(&mut layout, &[peer]);

        assert_eq!(layout.devices.len(), 1);
        assert_eq!(layout.devices[0].id, "local-device");
    }

    #[test]
    fn discovery_hides_other_clusters() {
        let layout = test_layout();
        let mut peer = test_peer();
        peer.cluster_id = "cluster-other".into();

        assert!(!peer_visible_to_layout(&layout, &peer));
    }

    #[test]
    fn discovery_shows_unpaired_clients_to_servers() {
        let layout = test_layout();
        let mut peer = test_peer();
        peer.cluster_id.clear();
        peer.pairing_required = true;

        assert!(peer_visible_to_layout(&layout, &peer));
    }

    #[test]
    fn pairing_challenge_rejects_second_requester_while_active() {
        let mut layout = test_layout();
        layout.machine_role = "client".into();
        layout.paired_controllers.clear();
        let challenge = Arc::new(Mutex::new(None));
        let mut first = test_peer();
        first.id = "server-one".into();
        first.machine_role = "server".into();
        let mut second = first.clone();
        second.id = "server-two".into();
        second.transport_public_key = "server-two-key".into();

        assert!(begin_pairing_challenge(
            &challenge,
            &layout,
            &first,
            "10.0.0.1".into(),
        ));
        assert!(!begin_pairing_challenge(
            &challenge,
            &layout,
            &second,
            "10.0.0.2".into(),
        ));

        let stored = challenge.lock().expect("challenge lock");
        assert_eq!(stored.as_ref().expect("challenge").requester_id, first.id);
    }

    #[test]
    fn pairing_challenge_accepts_known_requester_after_identity_rotation() {
        let mut layout = test_layout();
        layout.machine_role = "client".into();
        layout.paired_controllers = vec![PairedController {
            id: "server-old-id".into(),
            name: "Server".into(),
            host: "server.local".into(),
            ip: "10.0.0.1".into(),
            transport_public_key: "server-old-key".into(),
            protocol_version: quic_transport::PROTOCOL_VERSION,
            cluster_id: layout.cluster_id.clone(),
            paired_at_ms: now_ms(),
        }];
        let challenge = Arc::new(Mutex::new(None));
        let mut requester = test_peer();
        requester.id = "server-new-id".into();
        requester.name = "Server".into();
        requester.machine_role = "server".into();
        requester.host = "server.local".into();
        requester.ip = "10.0.0.1".into();
        requester.transport_public_key = "server-new-key".into();

        assert!(begin_pairing_challenge(
            &challenge,
            &layout,
            &requester,
            requester.ip.clone(),
        ));

        let stored = challenge.lock().expect("challenge lock");
        assert_eq!(
            stored.as_ref().expect("challenge").requester_id,
            requester.id
        );
    }

    #[test]
    fn pairing_challenge_refreshes_code_after_failed_attempt() {
        let mut layout = test_layout();
        layout.machine_role = "client".into();
        layout.paired_controllers.clear();
        let challenge = Arc::new(Mutex::new(None));
        let mut requester = test_peer();
        requester.id = "server-one".into();
        requester.machine_role = "server".into();

        assert!(begin_pairing_challenge(
            &challenge,
            &layout,
            &requester,
            "10.0.0.1".into(),
        ));

        {
            let mut stored = challenge.lock().expect("challenge lock");
            let stored = stored.as_mut().expect("challenge");
            stored.code = "000000".into();
            stored.expires_at_ms = 42;
            stored.attempts = 1;
        }

        assert!(begin_pairing_challenge(
            &challenge,
            &layout,
            &requester,
            "10.0.0.1".into(),
        ));

        let stored = challenge.lock().expect("challenge lock");
        let stored = stored.as_ref().expect("challenge");
        assert_eq!(stored.attempts, 0);
        assert_ne!(stored.expires_at_ms, 42);
    }

    #[test]
    fn pair_challenge_accepts_paired_client_for_repair() {
        let local_peer = local_peer_from_layout(&test_layout());
        let mut client = test_peer();
        client.machine_role = "client".into();
        client.pairing_required = false;
        client.cluster_id = "cluster-before-repair".into();
        client.transport_public_key = "client-public-key".into();

        assert!(pair_challenge_usable_for_local_peer(&local_peer, &client));

        client.machine_role = "server".into();
        assert!(!pair_challenge_usable_for_local_peer(&local_peer, &client));
    }

    #[test]
    fn paired_client_still_announces_publicly() {
        // A paired client keeps sending public announces so the server can pick
        // it back up within one announce cycle after the client restarts (e.g. an
        // admin-restart), instead of depending solely on the reply path. The
        // announce only carries public fields, never the pair_secret.
        let mut layout = test_layout();
        layout.machine_role = "client".into();
        layout.paired_controllers = vec![PairedController {
            id: "server".into(),
            name: "Server".into(),
            host: "server".into(),
            ip: "10.0.0.1".into(),
            transport_public_key: "server-key".into(),
            protocol_version: quic_transport::PROTOCOL_VERSION,
            cluster_id: layout.cluster_id.clone(),
            paired_at_ms: now_ms(),
        }];

        assert!(should_send_public_announce(&layout));
    }

    #[test]
    fn save_merge_preserves_backend_pairing_from_stale_settings_snapshot() {
        let mut current = test_layout();
        current.machine_role = "client".into();
        current.input_mode = "receive".into();
        current.cluster_id = "paired-cluster".into();
        current.pair_secret = "paired-secret".into();
        current.paired_controllers = vec![PairedController {
            id: "server".into(),
            name: "Server".into(),
            host: "server".into(),
            ip: "10.0.0.1".into(),
            transport_public_key: "server-key".into(),
            protocol_version: quic_transport::PROTOCOL_VERSION,
            cluster_id: current.cluster_id.clone(),
            paired_at_ms: now_ms(),
        }];

        let mut stale_settings = current.clone();
        stale_settings.cluster_id = "old-cluster".into();
        stale_settings.pair_secret = "old-secret".into();
        stale_settings.paired_controllers.clear();
        stale_settings.performance_monitor = true;

        let merged = merge_runtime_owned_layout_fields(stale_settings, &current);

        assert_eq!(merged.cluster_id, "paired-cluster");
        assert_eq!(merged.pair_secret, "paired-secret");
        assert_eq!(merged.paired_controllers, current.paired_controllers);
        assert!(merged.performance_monitor);
    }

    #[test]
    fn save_merge_preserves_local_transport_identity() {
        let mut current = test_layout();
        current.devices[0].transport_public_key = "runtime-key".into();
        current.devices[0].protocol_version = quic_transport::PROTOCOL_VERSION;

        let mut stale_settings = current.clone();
        stale_settings.devices[0].transport_public_key.clear();
        stale_settings.devices[0].protocol_version = 0;

        let merged = merge_runtime_owned_layout_fields(stale_settings, &current);

        assert_eq!(merged.devices[0].transport_public_key, "runtime-key");
        assert_eq!(
            merged.devices[0].protocol_version,
            quic_transport::PROTOCOL_VERSION
        );
    }

    #[test]
    fn disk_refresh_preserves_runtime_pairing_when_disk_snapshot_is_empty() {
        let mut current = test_layout();
        current.machine_role = "client".into();
        current.cluster_id = "runtime-cluster".into();
        current.pair_secret = "runtime-secret".into();
        current.paired_controllers = vec![PairedController {
            id: "server".into(),
            name: "Server".into(),
            host: "server".into(),
            ip: "10.0.0.1".into(),
            transport_public_key: "server-key".into(),
            protocol_version: quic_transport::PROTOCOL_VERSION,
            cluster_id: current.cluster_id.clone(),
            paired_at_ms: now_ms(),
        }];
        let mut disk = current.clone();
        disk.cluster_id = "empty-disk-cluster".into();
        disk.pair_secret = "empty-disk-secret".into();
        disk.paired_controllers.clear();

        let merged = merge_disk_layout_into_runtime(disk, &current);

        assert_eq!(merged.cluster_id, "runtime-cluster");
        assert_eq!(merged.pair_secret, "runtime-secret");
        assert_eq!(merged.paired_controllers, current.paired_controllers);
    }

    #[test]
    fn pairing_confirm_stream_saves_paired_controller() {
        let mut layout = test_layout();
        layout.machine_role = "client".into();
        layout.cluster_id = "client-old-cluster".into();
        layout.pair_secret = "client-old-secret".into();
        layout.paired_controllers.clear();

        let mut server = test_peer();
        server.id = "server-10-0-0-1".into();
        server.name = "Server".into();
        server.machine_role = "server".into();
        server.ip = "10.0.0.1".into();
        server.transport_public_key = "server-public-key".into();

        let layout_state = Arc::new(Mutex::new(layout));
        let pairing_challenge = Arc::new(Mutex::new(Some(PairingChallenge {
            code: "123456".into(),
            requester_id: server.id.clone(),
            requester_name: server.name.clone(),
            requester_ip: server.ip.clone(),
            requester_host: server.host.clone(),
            requester_public_key: server.transport_public_key.clone(),
            requester_protocol_version: server.protocol_version,
            expires_at: Instant::now() + Duration::from_secs(60),
            expires_at_ms: now_ms() + 60_000,
            attempts: 0,
        })));
        let config_path =
            std::env::temp_dir().join(format!("mykvm-pairing-stream-test-{}.json", now_ms()));
        let peers = Arc::new(Mutex::new(Vec::new()));
        let payload = encode_discovery_payload(
            "pair-confirm",
            &server,
            DiscoveryPairingFields {
                code: Some("123456".into()),
                cluster_id: Some("server-cluster".into()),
                secret: Some("server-secret".into()),
                error: None,
            },
        )
        .expect("pair-confirm should encode");

        assert!(handle_pairing_stream_packet(
            &payload,
            SocketAddr::from(([10, 0, 0, 1], 52001)),
            &layout_state,
            &pairing_challenge,
            &config_path,
            &peers,
        ));

        let saved = layout_state.lock().expect("layout lock").clone();
        assert_eq!(saved.cluster_id, "server-cluster");
        assert_eq!(saved.pair_secret, "server-secret");
        assert_eq!(saved.paired_controllers.len(), 1);
        assert_eq!(saved.paired_controllers[0].id, server.id);
        assert!(pairing_challenge.lock().expect("challenge lock").is_none());
        let _ = fs::remove_file(config_path);
    }

    #[test]
    fn pairing_confirm_stream_rejects_wrong_code() {
        let mut layout = test_layout();
        layout.machine_role = "client".into();
        layout.paired_controllers.clear();

        let mut server = test_peer();
        server.id = "server-10-0-0-1".into();
        server.name = "Server".into();
        server.machine_role = "server".into();
        server.ip = "10.0.0.1".into();
        server.transport_public_key = "server-public-key".into();

        let layout_state = Arc::new(Mutex::new(layout));
        let pairing_challenge = Arc::new(Mutex::new(Some(PairingChallenge {
            code: "123456".into(),
            requester_id: server.id.clone(),
            requester_name: server.name.clone(),
            requester_ip: server.ip.clone(),
            requester_host: server.host.clone(),
            requester_public_key: server.transport_public_key.clone(),
            requester_protocol_version: server.protocol_version,
            expires_at: Instant::now() + Duration::from_secs(60),
            expires_at_ms: now_ms() + 60_000,
            attempts: 0,
        })));
        let config_path =
            std::env::temp_dir().join(format!("mykvm-pairing-reject-test-{}.json", now_ms()));
        let peers = Arc::new(Mutex::new(Vec::new()));
        let payload = encode_discovery_payload(
            "pair-confirm",
            &server,
            DiscoveryPairingFields {
                code: Some("000000".into()),
                cluster_id: Some("server-cluster".into()),
                secret: Some("server-secret".into()),
                error: None,
            },
        )
        .expect("pair-confirm should encode");

        assert!(!handle_pairing_stream_packet(
            &payload,
            SocketAddr::from(([10, 0, 0, 1], 52001)),
            &layout_state,
            &pairing_challenge,
            &config_path,
            &peers,
        ));

        let saved = layout_state.lock().expect("layout lock").clone();
        assert!(saved.paired_controllers.is_empty());
        assert_eq!(
            pairing_challenge
                .lock()
                .expect("challenge lock")
                .as_ref()
                .expect("challenge still active")
                .attempts,
            1
        );
        let _ = fs::remove_file(config_path);
    }

    #[test]
    fn clipboard_packet_requires_paired_controller_on_client() {
        let mut layout = test_layout();
        layout.machine_role = "client".into();
        layout.paired_controllers = vec![PairedController {
            id: "server-10-0-0-1".into(),
            name: "Server".into(),
            host: "server".into(),
            ip: "10.0.0.1".into(),
            transport_public_key: "server-key".into(),
            protocol_version: quic_transport::PROTOCOL_VERSION,
            cluster_id: layout.cluster_id.clone(),
            paired_at_ms: now_ms(),
        }];
        let mut packet = ClipboardPacket {
            protocol: CLIPBOARD_PROTOCOL.into(),
            origin_id: "attacker".into(),
            target_id: "local-device".into(),
            cluster_id: layout.cluster_id.clone(),
            pair_secret: layout.pair_secret.clone(),
            signature: "text:hello".into(),
            formats: vec![ClipboardFormat {
                kind: "plainText".into(),
                text: "hello".into(),
                image: None,
            }],
            text: "hello".into(),
            image: None,
            sequence: 1,
        };

        assert!(!clipboard_packet_authorized(&layout, &packet));
        packet.origin_id = "server-10-0-0-1".into();
        assert!(clipboard_packet_authorized(&layout, &packet));
    }

    #[test]
    fn clipboard_image_signature_includes_content_hash() {
        let first = ClipboardContent::Image(ClipboardImage {
            width: 2,
            height: 1,
            rgba_base64: "AAAAAAAAAAA=".into(),
        });
        let second = ClipboardContent::Image(ClipboardImage {
            width: 2,
            height: 1,
            rgba_base64: "AQEBAQEBAQE=".into(),
        });

        assert_ne!(first.signature(), second.signature());
    }

    #[test]
    fn clipboard_image_packet_fits_transport_stream_budget() {
        let raw_rgba_bytes = 800 * 600 * 4;
        let encoded_len = raw_rgba_bytes / 3 * 4;
        let packet = clipboard_packet_from_content(
            ClipboardContent::Image(ClipboardImage {
                width: 800,
                height: 600,
                rgba_base64: "A".repeat(encoded_len),
            }),
            "local-device".into(),
            "peer-device".into(),
            "cluster-test".into(),
            "secret-test".into(),
            1,
        );
        let payload = encode_wire_packet(&packet).expect("clipboard packet should encode");

        assert!(
            payload.len() <= quic_transport::MAX_STREAM_BYTES,
            "clipboard image packet is {} bytes but stream budget is {} bytes",
            payload.len(),
            quic_transport::MAX_STREAM_BYTES
        );
    }

    #[test]
    fn clipboard_packet_uses_formats_envelope() {
        let packet = clipboard_packet_from_content(
            ClipboardContent::Text("hello".into()),
            "local-device".into(),
            "peer-device".into(),
            "cluster-test".into(),
            "secret-test".into(),
            42,
        );

        assert_eq!(packet.target_id, "peer-device");
        assert_eq!(packet.signature, "text:hello");
        assert_eq!(packet.formats.len(), 1);
        assert_eq!(packet.formats[0].kind, "plainText");
        assert_eq!(packet.formats[0].text, "hello");
        assert!(packet.formats[0].image.is_none());
        assert_eq!(packet.text, "hello");
    }

    #[test]
    fn clipboard_write_retries_transient_failures() {
        let content = ClipboardContent::Text("hello".into());
        let mut calls = 0;

        let result = retry_clipboard_content_write(&content, 3, Duration::ZERO, |_| {
            calls += 1;
            if calls < 3 {
                Err("clipboard busy".into())
            } else {
                Ok(())
            }
        });

        assert!(result.is_ok());
        assert_eq!(calls, 3);
    }

    #[test]
    fn clipboard_formats_only_text_packet_is_accepted() {
        let layout = test_layout();
        let packet = ClipboardPacket {
            protocol: CLIPBOARD_PROTOCOL.into(),
            origin_id: "peer-client-10-0-0-2".into(),
            target_id: "local-device".into(),
            cluster_id: layout.cluster_id.clone(),
            pair_secret: layout.pair_secret.clone(),
            signature: "text:hello".into(),
            formats: vec![ClipboardFormat {
                kind: "plainText".into(),
                text: "hello".into(),
                image: None,
            }],
            text: String::new(),
            image: None,
            sequence: 1,
        };
        let payload = encode_wire_packet(&packet).expect("clipboard packet should encode");
        let clipboard_seen_text = Arc::new(Mutex::new(None));
        let clipboard_echo_until = Arc::new(Mutex::new(None));
        let clipboard_last_sequences = Arc::new(Mutex::new(HashMap::new()));
        let mut written = None;

        let accepted = handle_clipboard_packet_with_writer(
            &payload,
            &layout,
            "local-device",
            &clipboard_seen_text,
            &clipboard_echo_until,
            &clipboard_last_sequences,
            |content| {
                if let ClipboardContent::Text(text) = content {
                    written = Some(text.clone());
                }
                Ok(())
            },
        );

        assert!(accepted);
        assert_eq!(written.as_deref(), Some("hello"));
        assert_eq!(
            clipboard_seen_text.lock().expect("seen lock").as_deref(),
            Some("text:hello")
        );
    }

    #[test]
    fn clipboard_formats_packet_preserves_non_ascii_text() {
        let layout = test_layout();
        let packet = clipboard_packet_from_content(
            ClipboardContent::Text("中文测试 abc 123".into()),
            "peer-client-10-0-0-2".into(),
            "local-device".into(),
            layout.cluster_id.clone(),
            layout.pair_secret.clone(),
            1,
        );
        let payload = encode_wire_packet(&packet).expect("clipboard packet should encode");
        let clipboard_seen_text = Arc::new(Mutex::new(None));
        let clipboard_echo_until = Arc::new(Mutex::new(None));
        let clipboard_last_sequences = Arc::new(Mutex::new(HashMap::new()));
        let mut written = None;

        let accepted = handle_clipboard_packet_with_writer(
            &payload,
            &layout,
            "local-device",
            &clipboard_seen_text,
            &clipboard_echo_until,
            &clipboard_last_sequences,
            |content| {
                if let ClipboardContent::Text(text) = content {
                    written = Some(text.clone());
                }
                Ok(())
            },
        );

        assert!(accepted);
        assert_eq!(written.as_deref(), Some("中文测试 abc 123"));
        assert_eq!(
            clipboard_seen_text.lock().expect("seen lock").as_deref(),
            Some("text:中文测试 abc 123")
        );
    }

    #[test]
    fn clipboard_legacy_text_packet_is_still_accepted() {
        let layout = test_layout();
        let packet = ClipboardPacket {
            protocol: CLIPBOARD_PROTOCOL.into(),
            origin_id: "peer-client-10-0-0-2".into(),
            target_id: String::new(),
            cluster_id: layout.cluster_id.clone(),
            pair_secret: layout.pair_secret.clone(),
            signature: String::new(),
            formats: Vec::new(),
            text: "legacy".into(),
            image: None,
            sequence: 1,
        };
        let payload = encode_wire_packet(&packet).expect("legacy clipboard packet should encode");
        let clipboard_seen_text = Arc::new(Mutex::new(None));
        let clipboard_echo_until = Arc::new(Mutex::new(None));
        let clipboard_last_sequences = Arc::new(Mutex::new(HashMap::new()));
        let mut written = None;

        let accepted = handle_clipboard_packet_with_writer(
            &payload,
            &layout,
            "local-device",
            &clipboard_seen_text,
            &clipboard_echo_until,
            &clipboard_last_sequences,
            |content| {
                if let ClipboardContent::Text(text) = content {
                    written = Some(text.clone());
                }
                Ok(())
            },
        );

        assert!(accepted);
        assert_eq!(written.as_deref(), Some("legacy"));
    }

    #[test]
    fn clipboard_formats_packet_rejects_stale_sequence() {
        let layout = test_layout();
        let clipboard_seen_text = Arc::new(Mutex::new(None));
        let clipboard_echo_until = Arc::new(Mutex::new(None));
        let clipboard_last_sequences = Arc::new(Mutex::new(HashMap::new()));
        let mut written = Vec::new();

        let first = clipboard_packet_from_content(
            ClipboardContent::Text("new".into()),
            "peer-client-10-0-0-2".into(),
            "local-device".into(),
            layout.cluster_id.clone(),
            layout.pair_secret.clone(),
            10,
        );
        let stale = clipboard_packet_from_content(
            ClipboardContent::Text("old".into()),
            "peer-client-10-0-0-2".into(),
            "local-device".into(),
            layout.cluster_id.clone(),
            layout.pair_secret.clone(),
            9,
        );

        for packet in [first, stale] {
            let payload = encode_wire_packet(&packet).expect("clipboard packet should encode");
            let _ = handle_clipboard_packet_with_writer(
                &payload,
                &layout,
                "local-device",
                &clipboard_seen_text,
                &clipboard_echo_until,
                &clipboard_last_sequences,
                |content| {
                    if let ClipboardContent::Text(text) = content {
                        written.push(text.clone());
                    }
                    Ok(())
                },
            );
        }

        assert_eq!(written, vec!["new"]);
    }

    #[test]
    fn clipboard_text_packet_rejects_when_system_write_fails() {
        let layout = test_layout();
        let packet = clipboard_packet_from_content(
            ClipboardContent::Text("hello".into()),
            "peer-client-10-0-0-2".into(),
            "local-device".into(),
            layout.cluster_id.clone(),
            layout.pair_secret.clone(),
            1,
        );
        let payload = encode_wire_packet(&packet).expect("clipboard packet should encode");
        let clipboard_seen_text = Arc::new(Mutex::new(None));
        let clipboard_echo_until = Arc::new(Mutex::new(None));
        let clipboard_last_sequences = Arc::new(Mutex::new(HashMap::new()));

        let accepted = handle_clipboard_packet_with_writer(
            &payload,
            &layout,
            "local-device",
            &clipboard_seen_text,
            &clipboard_echo_until,
            &clipboard_last_sequences,
            |_| Err("clipboard busy".into()),
        );

        assert!(!accepted);
        assert!(clipboard_seen_text.lock().expect("seen lock").is_none());
    }

    #[test]
    fn file_transfer_target_uses_peer_quic_port() {
        let layout = test_layout();
        let target = file_transfer_target_for_device(&layout, &[], "peer-client-10-0-0-2").unwrap();

        assert_eq!(target.addr, "10.0.0.2:47834");
        assert_eq!(target.transport_public_key, "peer-public-key");
    }

    #[test]
    fn file_transfer_client_targets_online_paired_controller() {
        let mut layout = test_layout();
        layout.machine_role = "client".into();
        layout.paired_controllers = vec![PairedController {
            id: "peer-server-10-0-0-1".into(),
            name: "Server".into(),
            host: "server.local".into(),
            ip: "10.0.0.1".into(),
            transport_public_key: "server-public-key".into(),
            protocol_version: quic_transport::PROTOCOL_VERSION,
            cluster_id: layout.cluster_id.clone(),
            paired_at_ms: now_ms(),
        }];
        let peers = vec![LanPeer {
            id: "peer-server-10-0-0-1".into(),
            name: "Server".into(),
            platform: "macos".into(),
            machine_role: "server".into(),
            cluster_id: layout.cluster_id.clone(),
            pairing_required: false,
            host: "server.local".into(),
            ip: "10.0.0.1".into(),
            transport_port: 52000,
            quic_port: 52001,
            transport_public_key: "server-public-key".into(),
            protocol_version: quic_transport::PROTOCOL_VERSION,
            screen_count: 1,
            input_ready: false,
            upgrading: false,
            screens: vec![],
            app_version: "0.1.0".into(),
            last_seen_ms: now_ms(),
        }];

        let target =
            file_transfer_target_for_device(&layout, &peers, "peer-server-10-0-0-1").unwrap();

        assert_eq!(target.addr, "10.0.0.1:52001");
        assert_eq!(target.transport_public_key, "server-public-key");
    }

    #[test]
    fn file_transfer_writes_chunked_file_to_receive_root() {
        let layout = test_layout();
        let root = temp_test_dir("file-transfer-ok");
        let transfers = Arc::new(Mutex::new(HashMap::new()));

        for packet in [
            test_file_transfer_packet("start", "transfer-1", "note.txt", 11, 0, 0, b""),
            test_file_transfer_packet("chunk", "transfer-1", "note.txt", 11, 0, 0, b"hello "),
            test_file_transfer_packet("chunk", "transfer-1", "note.txt", 11, 1, 6, b"world"),
            test_file_transfer_packet("finish", "transfer-1", "note.txt", 11, 2, 11, b""),
        ] {
            let payload = encode_wire_packet(&packet).expect("file packet should encode");
            assert!(handle_file_transfer_packet_with_root(
                &payload,
                &layout,
                "local-device",
                &transfers,
                &root
            ));
        }

        assert_eq!(
            fs::read_to_string(root.join("note.txt")).expect("received file"),
            "hello world"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn file_transfer_pointer_hint_writes_to_drop_destination() {
        let layout = test_layout();
        let default_root = temp_test_dir("file-transfer-default-root");
        let drop_root = temp_test_dir("file-transfer-pointer-root");
        let transfers = Arc::new(Mutex::new(HashMap::new()));

        for packet in [
            test_file_transfer_packet_with_destination_hint(
                "start",
                "transfer-pointer",
                "desktop.txt",
                7,
                0,
                0,
                b"",
                Some(FILE_TRANSFER_DESTINATION_POINTER),
            ),
            test_file_transfer_packet_with_destination_hint(
                "chunk",
                "transfer-pointer",
                "desktop.txt",
                7,
                0,
                0,
                b"desktop",
                Some(FILE_TRANSFER_DESTINATION_POINTER),
            ),
            test_file_transfer_packet_with_destination_hint(
                "finish",
                "transfer-pointer",
                "desktop.txt",
                7,
                1,
                7,
                b"",
                Some(FILE_TRANSFER_DESTINATION_POINTER),
            ),
        ] {
            let payload = encode_wire_packet(&packet).expect("file packet should encode");
            assert!(handle_file_transfer_packet_with_destination_root(
                &payload,
                &layout,
                "local-device",
                &transfers,
                &default_root,
                Some(&drop_root),
                None,
            ));
        }

        assert!(!default_root.join("desktop.txt").exists());
        assert_eq!(
            fs::read_to_string(drop_root.join("desktop.txt")).expect("received file"),
            "desktop"
        );
        let _ = fs::remove_dir_all(default_root);
        let _ = fs::remove_dir_all(drop_root);
    }

    #[test]
    fn file_transfer_rejects_out_of_order_chunks() {
        let layout = test_layout();
        let root = temp_test_dir("file-transfer-order");
        let transfers = Arc::new(Mutex::new(HashMap::new()));
        let start = test_file_transfer_packet("start", "transfer-2", "note.txt", 5, 0, 0, b"");
        let start_payload = encode_wire_packet(&start).expect("start should encode");
        assert!(handle_file_transfer_packet_with_root(
            &start_payload,
            &layout,
            "local-device",
            &transfers,
            &root
        ));

        let stale = test_file_transfer_packet("chunk", "transfer-2", "note.txt", 5, 1, 0, b"hello");
        let stale_payload = encode_wire_packet(&stale).expect("chunk should encode");
        assert!(!handle_file_transfer_packet_with_root(
            &stale_payload,
            &layout,
            "local-device",
            &transfers,
            &root
        ));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn file_transfer_sanitizes_received_file_names() {
        assert_eq!(
            sanitize_transfer_file_name("../bad:name?.txt").as_deref(),
            Some("_bad_name_.txt")
        );
        assert!(sanitize_transfer_file_name("..").is_none());
        assert!(sanitize_transfer_file_name("  ").is_none());
    }

    fn temp_test_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("{name}-{}", random_hex(4)));
        fs::create_dir_all(&path).expect("temp test dir");
        path
    }

    fn test_file_transfer_packet(
        kind: &str,
        transfer_id: &str,
        file_name: &str,
        total_bytes: u64,
        chunk_index: u64,
        offset: u64,
        data: &[u8],
    ) -> FileTransferPacket {
        test_file_transfer_packet_with_destination_hint(
            kind,
            transfer_id,
            file_name,
            total_bytes,
            chunk_index,
            offset,
            data,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn test_file_transfer_packet_with_destination_hint(
        kind: &str,
        transfer_id: &str,
        file_name: &str,
        total_bytes: u64,
        chunk_index: u64,
        offset: u64,
        data: &[u8],
        destination_hint: Option<&str>,
    ) -> FileTransferPacket {
        FileTransferPacket {
            protocol: FILE_TRANSFER_PROTOCOL.into(),
            kind: kind.into(),
            transfer_id: transfer_id.into(),
            origin_id: "peer-client-10-0-0-2".into(),
            target_id: "local-device".into(),
            cluster_id: "cluster-test".into(),
            pair_secret: "secret-test".into(),
            file_name: file_name.into(),
            destination_hint: destination_hint.map(str::to_string),
            total_bytes,
            chunk_index,
            offset,
            data: data.to_vec(),
        }
    }

    #[test]
    fn device_matching_rejects_same_host_from_other_cluster() {
        let layout = test_layout();
        let device = &layout.devices[1];
        let mut peer = test_peer();
        peer.id = "peer-other".into();
        peer.transport_public_key = "different-key".into();
        peer.cluster_id = "other-cluster".into();

        assert!(!device_matches_peer(device, &peer, &layout.cluster_id));
    }

    #[test]
    fn discovery_target_ports_spans_neighbouring_ports() {
        let ports = discovery_target_ports(DISCOVERY_PORT);
        assert_eq!(ports.len(), DISCOVERY_PORT_SPAN as usize);
        assert_eq!(ports[0], DISCOVERY_PORT);
        // A peer that drifted from 47833 to 47834 must still be a target.
        assert!(ports.contains(&(DISCOVERY_PORT + 1)));
        assert_eq!(
            *ports.last().unwrap(),
            DISCOVERY_PORT + DISCOVERY_PORT_SPAN - 1
        );
    }

    #[test]
    fn discovery_target_ports_clamp_near_max() {
        let ports = discovery_target_ports(TRANSPORT_PORT_MAX - 1);
        assert_eq!(ports, vec![TRANSPORT_PORT_MAX - 1, TRANSPORT_PORT_MAX]);
    }

    #[test]
    fn broadcast_addrs_reach_a_drifted_peer_port() {
        // The exact failure we are fixing: one peer on 47833 must still address a
        // peer that landed on 47834, via the global broadcast target.
        let addrs = broadcast_addrs(DISCOVERY_PORT);
        assert!(addrs.contains(&format!("255.255.255.255:{DISCOVERY_PORT}")));
        assert!(addrs.contains(&format!("255.255.255.255:{}", DISCOVERY_PORT + 1)));
    }

    #[test]
    fn broadcast_addrs_include_every_local_ipv4_subnet() {
        let addrs = broadcast_addrs_for_ips(
            DISCOVERY_PORT,
            &[Ipv4Addr::new(192, 168, 66, 106), Ipv4Addr::new(10, 0, 0, 4)],
        );

        assert!(addrs.contains(&format!("255.255.255.255:{DISCOVERY_PORT}")));
        assert!(addrs.contains(&format!("192.168.66.255:{DISCOVERY_PORT}")));
        assert!(addrs.contains(&format!("10.0.0.255:{DISCOVERY_PORT}")));
        assert!(addrs.contains(&format!("192.168.66.255:{}", DISCOVERY_PORT + 1)));
    }

    #[test]
    fn unicast_sweep_targets_cover_every_local_ipv4_subnet() {
        let targets = unicast_sweep_targets_for_ips(
            DISCOVERY_PORT,
            &[Ipv4Addr::new(192, 168, 66, 106), Ipv4Addr::new(10, 0, 0, 4)],
        );

        assert!(targets.contains(&format!("192.168.66.92:{DISCOVERY_PORT}")));
        assert!(targets.contains(&format!("10.0.0.1:{DISCOVERY_PORT}")));
        assert!(!targets.contains(&format!("192.168.66.106:{DISCOVERY_PORT}")));
        assert!(!targets.contains(&format!("10.0.0.4:{DISCOVERY_PORT}")));
    }

    #[test]
    fn known_peer_targets_include_saved_host_and_drifted_ports() {
        let mut layout = test_layout();
        layout.devices[1].host = "Client / 10.0.0.2".into();
        layout.devices[1].transport_port = DISCOVERY_PORT + DISCOVERY_PORT_SPAN + 2;

        let targets = known_peer_discovery_targets(&layout, DISCOVERY_PORT);

        assert!(targets.contains(&format!("10.0.0.2:{DISCOVERY_PORT}")));
        assert!(targets.contains(&format!("10.0.0.2:{}", DISCOVERY_PORT + 1)));
        assert!(targets.contains(&format!(
            "10.0.0.2:{}",
            DISCOVERY_PORT + DISCOVERY_PORT_SPAN + 2
        )));
        assert!(targets.contains(&format!("Client:{DISCOVERY_PORT}")));
    }

    #[test]
    fn known_peer_targets_include_paired_controller_on_clients() {
        let mut layout = test_layout();
        layout.machine_role = "client".into();
        layout.paired_controllers = vec![PairedController {
            id: "server".into(),
            name: "Server".into(),
            host: "server-host".into(),
            ip: "10.0.0.1".into(),
            transport_public_key: "server-key".into(),
            protocol_version: quic_transport::PROTOCOL_VERSION,
            cluster_id: layout.cluster_id.clone(),
            paired_at_ms: now_ms(),
        }];

        let targets = known_peer_discovery_targets(&layout, DISCOVERY_PORT);

        assert!(targets.contains(&format!("10.0.0.1:{DISCOVERY_PORT}")));
        assert!(targets.contains(&format!("server-host:{DISCOVERY_PORT}")));
    }

    #[test]
    fn split_host_port_parses_optional_port() {
        assert_eq!(
            split_host_port("192.168.1.5"),
            ("192.168.1.5".to_string(), None)
        );
        assert_eq!(
            split_host_port("192.168.1.5:47833"),
            ("192.168.1.5".to_string(), Some(47833))
        );
        assert_eq!(
            split_host_port("  host.local : 5000 "),
            ("host.local".to_string(), Some(5000))
        );
        // A non-numeric trailing segment stays part of a bare host.
        assert_eq!(split_host_port("myhost"), ("myhost".to_string(), None));
    }
}
