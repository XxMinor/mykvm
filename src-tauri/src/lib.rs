use std::{
    collections::HashMap,
    env, fs,
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
    AppHandle, Emitter, Manager, Monitor, WebviewUrl, WebviewWindowBuilder, WindowEvent, Wry,
};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};

mod clipboard;
mod rich_clipboard;
mod clipboard_exchange;
mod folder_transfer;
mod transfer_jobs;
#[cfg(any(target_os = "windows", test))]
mod drag_buffer;
#[cfg(target_os = "windows")]
pub mod headless_client;
mod input;
mod input_guard;
#[cfg(any(target_os = "windows", test))]
mod native_drag;
mod performance;
mod quic_transport;
pub mod shared_input;
#[cfg(target_os = "windows")]
pub mod windows_drag;
mod windows_drag_overlay;
#[cfg(target_os = "windows")]
pub mod windows_drop_catcher;
#[cfg(target_os = "windows")]
pub mod windows_input;
#[cfg(any(target_os = "windows", test))]
mod windows_keyboard_monitor;
#[cfg(target_os = "windows")]
mod windows_keyboard_capture;
#[cfg(target_os = "windows")]
mod windows_cursor_guard;
#[cfg(any(target_os = "windows", test))]
mod local_capture_session;

use clipboard::{ClipboardContent, ClipboardImage};
use performance::PerformanceSample;
use input_guard::{InputProtection, SharedGuard};

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
// A peer that stays down doubles the wait each time, up to a minute.
const CLIPBOARD_RETRY_MAX_MS: u64 = 60_000;
// Progressive 50, 100, ... 450 ms (~2.25 s in all) outlasts another process
// holding the clipboard open (PR #22).
const CLIPBOARD_WRITE_ATTEMPTS: usize = 10;
const CLIPBOARD_WRITE_RETRY_DELAY_MS: u64 = 50;
const FILE_TRANSFER_PROTOCOL: &str = "mykvm.file-transfer.v1";
const DRAG_TREE_PROTOCOL: &str = "mykvm.drag-tree.v1";
const CLIPBOARD_FILES_PROTOCOL: &str = "mykvm.clipboard-files.v1";
const CLIPBOARD_PULL_PROTOCOL: &str = "mykvm.clipboard-pull.v1";
const DRAG_CONTROL_PROTOCOL: &str = "mykvm.drag-control.v1";
const LOG_REQUEST_PROTOCOL: &str = "mykvm.log-request.v1";
// A paired controller asks a client to update itself (it runs its own updater,
// so the update is still verified against this app's updater key).
const REMOTE_UPDATE_PROTOCOL: &str = "mykvm.remote-update.v1";
// Lock sync: the controller locked its screen and asks its clients to lock too.
const LOCK_SCREEN_PROTOCOL: &str = "mykvm.lock-screen.v1";
// A paired peer asks for this device's recent log; the reply is the tail of the
// device's newest log file, streamed back over the file-transfer path.
const CLIENT_LOG_TAIL_BYTES: u64 = 512 * 1024;
/// Prefix of a drag-control send error: the peer never opened a drag session.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const DRAG_CONTROL_FAILED: &str = "拖放控制失败";
const FILE_TRANSFER_CHUNK_BYTES: usize = 256 * 1024;
const FILE_TRANSFER_MAX_FILE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const LOG_MAX_FILE_SIZE_BYTES: u128 = 1024 * 1024;
const AUTOSTART_ARG: &str = "--mykvm-autostart";
const QUIT_EXISTING_ARG: &str = "--mykvm-quit-existing";
const INSTALL_INPUT_SERVICE_ARG: &str = "--install-input-service";
const UNINSTALL_INPUT_SERVICE_ARG: &str = "--uninstall-input-service";
const HELPER_PATH_ARG: &str = "--helper-path";
const RUNTIME_STATE_EVENT: &str = "runtime-state-changed";
const FILE_TRANSFER_PROGRESS_EVENT: &str = "file-transfer-progress";
// Progress is emitted at most this often per file while sending (plus always on
// completion) so a multi-GB transfer doesn't flood the webview with events.
const FILE_TRANSFER_PROGRESS_INTERVAL_MS: u64 = 100;

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

// Matches the `identifier` in tauri.conf.json.
#[cfg(target_os = "macos")]
const MACOS_BUNDLE_ID: &str = "com.xzhpl.mykvm";

// Holds the flock'd lock file for the process lifetime; the kernel releases
// the lock when the process exits, however it exits.
#[cfg(target_os = "macos")]
static MACOS_INSTANCE_LOCK: OnceLock<std::fs::File> = OnceLock::new();

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
    /// Pointer and scroll speed this controller applies when driving the
    /// device (a per-device multiplier; 1 = as the local mouse moves).
    #[serde(default = "default_input_speed")]
    pointer_speed: f64,
    #[serde(default = "default_input_speed")]
    scroll_speed: f64,
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
    #[serde(default)]
    clipboard_on_demand: bool,
    #[serde(default = "default_clipboard_paste_hotkey")]
    clipboard_paste_hotkey: String,
    #[serde(default = "default_file_transfer_enabled")]
    file_transfer_enabled: bool,
    /// Controller: locking this machine locks its online clients too.
    #[serde(default)]
    lock_sync: bool,
    #[serde(default)]
    input_protection: InputProtection,
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
    input_protection: input_guard::GuardStatus,
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
    directory: bool,
    bundle: Option<Arc<folder_transfer::FolderBundle>>,
}

#[derive(Debug)]
struct IncomingFileTransfer {
    origin_id: String,
    target_id: String,
    file_name: String,
    total_bytes: u64,
    received_bytes: u64,
    next_chunk_index: u64,
    directory_bundle: bool,
    clipboard_transfer: bool,
    touched_ms: u64,
    temp_path: PathBuf,
    final_path: PathBuf,
    // ShareMouse-style drag: on finish, hand the completed file to the drag
    // placer (which drops it into the folder under the cursor on release)
    // instead of leaving it at final_path.
    staged: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileTransferSummary {
    target_name: String,
    file_count: usize,
    byte_count: u64,
}

// One progress update for the sending-side toast. `file_index` is 1-based.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct FileTransferProgress {
    transfer_id: String,
    file_name: String,
    target_name: String,
    sent_bytes: u64,
    total_bytes: u64,
    file_index: usize,
    file_count: usize,
    done: bool,
    error: Option<String>,
}

// Carries what a per-file send needs to emit progress. Optional throughout the
// send path so tests and any UI-less caller simply pass `None`.
struct FileTransferProgressReporter<'a> {
    app: &'a AppHandle,
    target_name: &'a str,
    file_index: usize,
    file_count: usize,
}

impl FileTransferProgressReporter<'_> {
    fn emit(
        &self,
        transfer_id: &str,
        file: &TransferFile,
        sent_bytes: u64,
        done: bool,
        error: Option<String>,
    ) {
        let _ = self.app.emit(
            FILE_TRANSFER_PROGRESS_EVENT,
            FileTransferProgress {
                transfer_id: transfer_id.into(),
                file_name: file.name.clone(),
                target_name: self.target_name.into(),
                sent_bytes,
                total_bytes: file.total_bytes,
                file_index: self.file_index,
                file_count: self.file_count,
                done,
                error,
            },
        );
    }
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
    total_bytes: u64,
    chunk_index: u64,
    offset: u64,
    #[serde(default)]
    data: Vec<u8>,
    // Edge drag-drop: land the file on the receiver's Desktop instead of the
    // transfers folder. Named-field msgpack, so old peers just ignore it (and
    // an old sender leaves it false here).
    #[serde(default)]
    drop_to_desktop: bool,
    // ShareMouse-style drag: the receiver stages the file and, when the drag is
    // released over it, drops it into the folder under the cursor (else Desktop)
    // instead of placing it immediately.
    #[serde(default)]
    drag_drop: bool,
    // A device's log fetched by its peer: land it in the "MyKVM Remote Logs"
    // folder instead of the transfers folder.
    #[serde(default)]
    client_log: bool,
    #[serde(default)]
    directory_bundle: bool,
    #[serde(default)]
    clipboard_transfer: bool,
}

struct AppRuntime {
    app_handle: AppHandle,
    layout: Arc<Mutex<LayoutState>>,
    native_layout: Arc<Mutex<LayoutState>>,
    runtime: Mutex<RuntimeStatus>,
    peers: Arc<Mutex<Vec<LanPeer>>>,
    pairing_challenge: Arc<Mutex<Option<PairingChallenge>>>,
    file_transfers: Arc<Mutex<HashMap<String, IncomingFileTransfer>>>,
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
    input_guard: SharedGuard,
    clipboard_packets: Arc<AtomicU64>,
    runtime_toggle_shortcut: Mutex<Option<String>>,
    clipboard_paste_shortcut: Mutex<Option<String>>,
    runtime_toggle_menu_item: Mutex<Option<MenuItem<Wry>>>,
    screen_switch_request: Arc<Mutex<Option<input::SwitchDirection>>>,
    screen_switch_shortcuts: Mutex<ScreenSwitchHotkeys>,
    config_path: PathBuf,
    #[cfg(target_os = "windows")]
    input_service_network_lease: Mutex<Option<std::fs::File>>,
}

impl AppRuntime {
    fn new(app_handle: AppHandle, config_path: PathBuf, mut detected_layout: LayoutState) -> Self {
        let layout = load_layout_from_disk(&config_path)
            .map(|saved_layout| normalize_saved_layout(saved_layout, detected_layout.clone()))
            .unwrap_or_else(|| detected_layout.clone());
        align_native_screen_ids(&mut detected_layout, &layout);
        let input_guard = SharedGuard::default();
        let notify_app = app_handle.clone();
        input_guard::set_notifier(&input_guard, Arc::new(move || {
            let handle = notify_app.clone();
            let _ = notify_app.run_on_main_thread(move || {
                let _ = sync_screen_switch_shortcuts(&handle);
                if let Some(state) = handle.try_state::<AppRuntime>() {
                    notify_runtime_state_changed(&handle, &state.runtime_status());
                }
            });
        }));
        Self {
            app_handle,
            layout: Arc::new(Mutex::new(layout)),
            native_layout: Arc::new(Mutex::new(detected_layout.clone())),
            runtime: Mutex::new(default_runtime(&detected_layout)),
            peers: Arc::new(Mutex::new(Vec::new())),
            pairing_challenge: Arc::new(Mutex::new(None)),
            file_transfers: Arc::new(Mutex::new(HashMap::new())),
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
            input_guard,
            clipboard_packets: Arc::new(AtomicU64::new(0)),
            runtime_toggle_shortcut: Mutex::new(None),
            clipboard_paste_shortcut: Mutex::new(None),
            runtime_toggle_menu_item: Mutex::new(None),
            screen_switch_request: Arc::new(Mutex::new(None)),
            screen_switch_shortcuts: Mutex::new(empty_screen_switch_hotkeys()),
            config_path,
            #[cfg(target_os = "windows")]
            input_service_network_lease: Mutex::new(None),
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
        // Status reads are polled by the UI; a panic elsewhere that poisoned
        // the lock must not turn every poll into another panic.
        let mut runtime = self
            .runtime
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        runtime.discovery = self.discovery_status_for_layout(layout);
        runtime.clipboard = self.clipboard_status(layout);
        runtime.pairing = self.pairing_status_for_layout(layout);
        runtime.privilege = current_privilege_status();
        if runtime.started && layout.machine_role == "server" {
            let protection = input_guard::snapshot(&self.input_guard);
            if !protection.capture_available {
                runtime.capture = NativeStageStatus { state: "error".into(), detail: "输入捕获未就绪，请检查系统权限或重新启动共享。".into() };
            } else if protection.reason.is_some() {
                runtime.capture = NativeStageStatus { state: "paused".into(),
                    detail: format!("输入保护已暂停跨屏：{:?}", protection.reason) };
            }
        }
        runtime.input_protection = input_guard::snapshot(&self.input_guard);

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
        let running = self
            .discovery_stop
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some();
        let state = if running {
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
        pairing_status(layout, &self.pairing_challenge)
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
        let layout_for_pairing = Arc::clone(&self.layout);
        let native_layout_for_input = Arc::clone(&self.native_layout);
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
            if input::handle_input_datagram(
                &layout_for_input,
                &native_layout_for_input,
                &payload,
                source,
                &input_events,
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

            // Snapshot the layout instead of holding the lock through the
            // handlers: clipboard writes retry with sleeps, spawn pbcopy and
            // decode up to 32MB of base64, and file transfers write chunks to
            // disk — holding the layout lock through any of that stalls the
            // input hot paths (which take the same lock) for tens to hundreds
            // of ms per sync. Stream packets are rare; one clone is nothing.
            let layout = {
                let Ok(layout) = layout_for_clipboard.lock() else {
                    return false;
                };
                layout.clone()
            };
            let current_peer = local_peer_from_layout(&layout);

            if handle_drag_control_packet(&payload, &layout, &current_peer.id) {
                transport_packets_for_stream.fetch_add(1, Ordering::Relaxed);
                return true;
            }

            if handle_peer_request_packet(
                &payload,
                &layout,
                &current_peer.id,
                &app_handle_for_file_transfer,
            ) {
                transport_packets_for_stream.fetch_add(1, Ordering::Relaxed);
                return true;
            }

            if let Some(accepted) = handle_clipboard_files_commit(
                &payload, &layout, &current_peer.id, &clipboard_seen_text, &clipboard_echo_until,
            ) {
                if accepted {
                    transport_packets_for_stream.fetch_add(1, Ordering::Relaxed);
                    clipboard_packets.fetch_add(1, Ordering::Relaxed);
                }
                return accepted;
            }

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

            if !clipboard_receive_enabled.load(Ordering::Relaxed) {
                return false;
            }
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
        #[cfg(target_os = "windows")]
        let _ = self.acquire_input_service_network_lease()?;

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
                                let fixed = current_layout.devices.iter().find(|device| {
                                    device.source == "manual"
                                        && device_matches_peer(
                                            device,
                                            &incoming.peer,
                                            &current_layout.cluster_id,
                                        )
                                });
                                if fixed
                                    .is_none_or(|device| same_host(&device.host, &incoming.peer.ip))
                                {
                                    warm_quic_peer(&quic_transport, &incoming.peer);
                                }
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

    #[cfg(target_os = "windows")]
    fn acquire_input_service_network_lease(&self) -> Result<bool, String> {
        let mut lease = self
            .input_service_network_lease
            .lock()
            .map_err(|_| "input service network lease lock poisoned".to_string())?;
        if lease
            .as_ref()
            .is_some_and(windows_input_service_network_lease_alive)
        {
            return Ok(true);
        }
        *lease = None;

        if let Some(file) = acquire_windows_input_service_network_lease()? {
            *lease = Some(file);
            return Ok(true);
        }

        let service = query_windows_input_service_status()?;
        let owns_network = windows_input_service_owns_network_ports()?;
        if service.installed && !service.running {
            // Revive a stopped lock-screen service even when it predates the
            // network takeover: without it the lock screen gets no input (#27).
            match start_windows_input_service() {
                Ok(()) => {
                    if let Some(file) = acquire_windows_input_service_network_lease()? {
                        *lease = Some(file);
                        return Ok(true);
                    }
                }
                Err(error) if owns_network => return Err(error),
                // An older install may only be startable by an administrator;
                // that must not block discovery.
                Err(error) => {
                    static WARNED: AtomicBool = AtomicBool::new(false);
                    if !WARNED.swap(true, Ordering::Relaxed) {
                        log::warn!("could not start the stopped input service: {error}");
                    }
                }
            }
        }
        if owns_network {
            return Err(
                "MyKVM input service is running but its network takeover pipe is unavailable."
                    .into(),
            );
        }
        Ok(false)
    }

    #[cfg(target_os = "windows")]
    fn release_input_service_network_lease(&self) -> Result<(), String> {
        self.input_service_network_lease
            .lock()
            .map_err(|_| "input service network lease lock poisoned".to_string())?
            .take();
        Ok(())
    }

    fn start_input(&self, layout: LayoutState) -> (NativeStageStatus, NativeStageStatus) {
        sync_layout_peer_presence(&self.layout, &self.peers);
        self.input_receive_enabled.store(
            layout.input_mode == "receive" && input::NATIVE_INPUT_SUPPORTED,
            Ordering::Relaxed,
        );
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
        input_guard::set_capture_available(&self.input_guard, true);
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
            Arc::clone(&self.input_guard),
        );
        *input_stop = Some(stop);
        #[cfg(target_os = "macos")]
        input::set_macos_app_nap_suppressed(
            statuses.0.state == "ready" || statuses.1.state == "ready",
        );
        // These were only shown in the window, so a startup failure left no
        // trace in the log once the window was closed.
        for (stage, status) in [("capture", &statuses.0), ("inject", &statuses.1)] {
            if status.state == "error" {
                log::warn!("input {stage} unavailable: {}", status.detail);
            }
        }
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
        let clipboard_layout = Arc::clone(&self.layout);
        let clipboard_app = self.app_handle.clone();
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
                clipboard_layout,
                clipboard_app,
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

    /// This process is about to end (quit, or an update about to install).
    /// Release remote control, then close QUIC so the peer gets a
    /// CONNECTION_CLOSE and reconnects at once — on a Windows client to the
    /// input service, which takes over the same ports and identity when this
    /// process is gone — instead of sending into a dead connection until its
    /// 10 s idle timeout.
    fn hand_off_network(&self) {
        self.stop_input();
        self.stop_clipboard();
        self.stop_discovery();
    }

    fn stop_input(&self) {
        #[cfg(target_os = "macos")]
        input::set_macos_app_nap_suppressed(false);
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
        input_guard::update_status(&self.input_guard, None, &Default::default());
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
        let mut saved_layout = merge_runtime_owned_layout_fields(layout, &previous_layout);
        saved_layout.input_protection = InputProtection::default();
        saved_layout.clipboard_on_demand = false;
        saved_layout.clipboard_paste_hotkey = default_clipboard_paste_hotkey();
        validate_shared_hotkeys(&saved_layout)?;
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
    sync_clipboard_paste_shortcut(&state.app_handle)?;
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
        input_protection: Default::default(),
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
        format!(
            "local: {} / {} (all IPv4: {})",
            local_peer.name,
            local_peer.ip,
            local_ip_list().unwrap_or_default()
        ),
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
        app_version: APP_VERSION
            .get()
            .cloned()
            .unwrap_or_else(|| env!("CARGO_PKG_VERSION").into()),
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

fn validate_shared_hotkeys(layout: &LayoutState) -> Result<(), String> {
    let mut seen = std::collections::HashSet::new();
    for value in [&layout.clipboard_paste_hotkey, &layout.edge_switch_hotkey,
        &layout.screen_switch_hotkeys.left, &layout.screen_switch_hotkeys.right, &layout.screen_switch_hotkeys.up, &layout.screen_switch_hotkeys.down] {
        if let Some(shortcut) = canonical_runtime_toggle_shortcut(value)? {
            if !seen.insert(shortcut) { return Err("启停、粘贴和切屏快捷键不能重复。".into()); }
        }
    }
    Ok(())
}

/// Register/unregister the four direction hotkeys so they stay in sync with the
/// saved layout. Mirrors `sync_runtime_toggle_shortcut`: compares against the
/// stored values and only touches the ones that changed.
fn sync_screen_switch_shortcuts(app: &AppHandle) -> Result<(), String> {
    let Some(state) = app.try_state::<AppRuntime>() else {
        return Ok(());
    };
    let layout = state.layout_snapshot();
    let next = if input_guard::is_paused(&state.input_guard) || layout.input_protection.local_only {
        empty_screen_switch_hotkeys()
    } else { screen_switch_shortcuts_for_layout(&layout) };

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

    let paste = state.clipboard_paste_shortcut.lock().map_err(|_| "clipboard shortcut is poisoned".to_string())?.clone();
    if paste.as_ref().and_then(|s| s.parse::<tauri_plugin_global_shortcut::Shortcut>().ok()).as_ref() == Some(shortcut) {
        paste_remote_clipboard(state)?;
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

    if input_guard::is_paused(&state.input_guard) || state.layout_snapshot().input_protection.local_only {
        return Ok(());
    }
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
    app: AppHandle,
    state: tauri::State<'_, AppRuntime>,
) -> Result<InputServiceStatus, String> {
    #[cfg(target_os = "windows")]
    {
        let previous_service_pid = windows_input_service_process_id()?;
        let helper_path = resolve_input_helper_path()?;
        let owner_sid = current_windows_user_sid()?;
        let status = if is_windows_process_elevated().unwrap_or(false) {
            state.release_input_service_network_lease()?;
            let result: Result<InputServiceStatus, String> = (|| {
                install_windows_input_service(&helper_path, &state.config_path, &owner_sid)?;
                start_windows_input_service()?;
                let _ = state.acquire_input_service_network_lease()?;
                Ok(current_input_service_status())
            })();
            if result.is_err() {
                let _ = state.acquire_input_service_network_lease();
            }
            result?
        } else {
            launch_current_process_as_admin(&[
                INSTALL_INPUT_SERVICE_ARG.into(),
                HELPER_PATH_ARG.into(),
                helper_path.to_string_lossy().into_owned(),
                shared_input::SERVICE_CONFIG_PATH_ARG.into(),
                state.config_path.to_string_lossy().into_owned(),
                shared_input::SERVICE_OWNER_SID_ARG.into(),
                owner_sid,
            ])?;
            wait_for_input_service_network_lease_after_restart(app, previous_service_pid);
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
        let _ = (app, state);
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
            state.release_input_service_network_lease()?;
            let result = uninstall_windows_input_service();
            if let Err(error) = result {
                let _ = state.acquire_input_service_network_lease();
                return Err(error);
            }
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

#[cfg(target_os = "windows")]
fn wait_for_input_service_network_lease_after_restart(
    app: AppHandle,
    previous_service_pid: Option<u32>,
) {
    thread::spawn(move || {
        for _ in 0..240 {
            let current_service_pid = windows_input_service_process_id().ok().flatten();
            if current_service_pid.is_none() || current_service_pid == previous_service_pid {
                thread::sleep(Duration::from_millis(250));
                continue;
            }
            let state = app.state::<AppRuntime>();
            match state.acquire_input_service_network_lease() {
                Ok(true) => {
                    let status = current_input_service_status();
                    update_runtime_input_service_status(state.inner(), &status);
                    let runtime = state.runtime_status();
                    notify_runtime_state_changed(&app, &runtime);
                    return;
                }
                Ok(false) | Err(_) => thread::sleep(Duration::from_millis(250)),
            }
        }
    });
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
    send_files_to_device_inner(state.inner(), &device_id, &paths, DropMode::TransfersFolder)
}

fn send_files_to_device_inner(
    state: &AppRuntime,
    device_id: &str,
    paths: &[String],
    drop_mode: DropMode,
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
    let files = collect_transfer_files(paths)?;
    let total_files = files.len();
    let mut file_count = 0_usize;
    let mut byte_count = 0_u64;

    for (index, file) in files.iter().enumerate() {
        let reporter = FileTransferProgressReporter {
            app: &state.app_handle,
            target_name: &target.name,
            file_index: index + 1,
            file_count: total_files,
        };
        let packet_count = send_transfer_file(
            &quic_transport,
            &local_peer.id,
            &target,
            file,
            &new_transfer_id("file"),
            drop_mode,
            Some(&reporter),
            None,
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

/// Ask an online peer (a client, or the paired controller) for its recent log.
/// The log arrives asynchronously in the "MyKVM Remote Logs" folder (which this
/// opens); returns its path.
/// Lock sync (controller side): when this machine locks, ask its online
/// clients to lock as well. Polled, since neither platform hands a lock to a
/// tray app without a window to receive it.
fn spawn_lock_sync_watcher(app: AppHandle) {
    let _ = thread::Builder::new()
        .name("mykvm-lock-sync".into())
        .spawn(move || {
            let mut was_locked = local_screen_locked();
            loop {
                thread::sleep(Duration::from_secs(1));
                let locked = local_screen_locked();
                if locked && !was_locked {
                    if let Some(state) = app.try_state::<AppRuntime>() {
                        lock_online_clients(state.inner());
                    }
                }
                was_locked = locked;
            }
        });
}

fn lock_online_clients(state: &AppRuntime) {
    let layout = state.layout_snapshot();
    if layout.machine_role != "server" || !layout.lock_sync {
        return;
    }
    for device in layout
        .devices
        .iter()
        .filter(|device| device.role != "local" && device.online)
    {
        match send_peer_request(state, &device.id, LOCK_SCREEN_PROTOCOL) {
            Ok(()) => log::info!("lock sync: locked {}", device.name),
            Err(error) => log::warn!("lock sync: could not lock {}: {error}", device.name),
        }
    }
}

/// Whether this machine's session is locked. Windows: the session's lock flag
/// (not the input desktop, which a UAC prompt switches too).
#[cfg(target_os = "windows")]
fn local_screen_locked() -> bool {
    use windows_sys::Win32::System::RemoteDesktop::{
        WTSFreeMemory, WTSQuerySessionInformationW, WTSSessionInfoEx, WTSINFOEXW,
        WTS_CURRENT_SERVER_HANDLE, WTS_CURRENT_SESSION, WTS_SESSIONSTATE_LOCK,
    };
    unsafe {
        let mut buffer: windows_sys::core::PWSTR = std::ptr::null_mut();
        let mut bytes = 0_u32;
        if WTSQuerySessionInformationW(
            WTS_CURRENT_SERVER_HANDLE,
            WTS_CURRENT_SESSION,
            WTSSessionInfoEx,
            &mut buffer,
            &mut bytes,
        ) == 0
            || buffer.is_null()
        {
            return false;
        }
        let info = &*(buffer as *const WTSINFOEXW);
        let locked = info.Level == 1
            && info.Data.WTSInfoExLevel1.SessionFlags == WTS_SESSIONSTATE_LOCK as i32;
        WTSFreeMemory(buffer as *mut std::ffi::c_void);
        locked
    }
}

#[cfg(target_os = "macos")]
fn local_screen_locked() -> bool {
    use core_foundation::base::{CFType, TCFType};
    use core_foundation::boolean::CFBoolean;
    use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
    use core_foundation::string::CFString;
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGSessionCopyCurrentDictionary() -> CFDictionaryRef;
    }
    let raw = unsafe { CGSessionCopyCurrentDictionary() };
    if raw.is_null() {
        return false;
    }
    let session: CFDictionary<CFString, CFType> = unsafe { TCFType::wrap_under_create_rule(raw) };
    session
        .find(CFString::from_static_string("CGSSessionScreenIsLocked"))
        .and_then(|value| value.downcast::<CFBoolean>())
        .map(bool::from)
        .unwrap_or(false)
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn local_screen_locked() -> bool {
    false
}

/// Lock this machine's screen (lock sync, client side).
#[cfg(target_os = "windows")]
fn lock_local_screen() {
    if unsafe { windows_sys::Win32::System::Shutdown::LockWorkStation() } == 0 {
        log::warn!("lock sync: LockWorkStation failed");
    }
}

/// The login framework's SACLockScreenImmediate is what the menu bar's Lock
/// Screen calls; if it is ever gone, press its shortcut (Ctrl+Cmd+Q) instead.
#[cfg(target_os = "macos")]
fn lock_local_screen() {
    use std::os::raw::{c_char, c_int, c_void};
    extern "C" {
        fn dlopen(path: *const c_char, mode: c_int) -> *mut c_void;
        fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    }
    const RTLD_LAZY: c_int = 1;
    unsafe {
        let framework = dlopen(
            b"/System/Library/PrivateFrameworks/login.framework/Versions/Current/login\0".as_ptr()
                as *const c_char,
            RTLD_LAZY,
        );
        if !framework.is_null() {
            let lock = dlsym(framework, b"SACLockScreenImmediate\0".as_ptr() as *const c_char);
            if !lock.is_null() {
                let lock: extern "C" fn() -> c_int = std::mem::transmute(lock);
                lock();
                return;
            }
        }
    }
    input::post_lock_screen_shortcut();
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn lock_local_screen() {}

/// Send a paired peer a request (`protocol`) over the stream channel.
fn send_peer_request(state: &AppRuntime, device_id: &str, protocol: &str) -> Result<(), String> {
    state.start_discovery()?;
    let layout = state.layout_snapshot();
    let mut local_peer = local_peer_from_layout(&layout);
    let quic_transport = state
        .quic_transport_handle()
        .ok_or_else(|| "QUIC transport is not ready; start the runtime first.".to_string())?;
    apply_transport_to_peer(&mut local_peer, &quic_transport);
    let peers = active_peer_snapshot(&state.peers);
    let target = file_transfer_target_for_device(&layout, &peers, device_id)?;

    let packet = PeerRequestPacket {
        protocol: protocol.into(),
            paste_token: String::new(),        origin_id: local_peer.id.clone(),
        target_id: target.device_id.clone(),
        cluster_id: target.cluster_id.clone(),
        pair_secret: target.pair_secret.clone(),
    };
    let payload = encode_wire_packet(&packet)?;
    let peer = quic_transport.peer(
        target.addr.clone(),
        target.transport_public_key.clone(),
        target.protocol_version,
    );
    quic_transport.send_stream_expect_ack(peer, payload)
}

/// Ask an online peer (a client, or the paired controller) for its recent log.
/// The log arrives asynchronously in the "MyKVM Remote Logs" folder (which this
/// opens); returns its path.
#[tauri::command]
fn fetch_client_log(
    device_id: String,
    state: tauri::State<'_, AppRuntime>,
) -> Result<String, String> {
    let state = state.inner();
    if !state.layout_snapshot().file_transfer_enabled {
        return Err("文件传输未开启，无法拉取日志。".into());
    }
    send_peer_request(state, &device_id, LOG_REQUEST_PROTOCOL)
        .map_err(|error| format!("拉取日志失败: {error}"))?;

    let dir = client_log_dir(&state.app_handle)?;
    let _ = fs::create_dir_all(&dir);
    let _ = open_external_path(&dir);
    Ok(dir.to_string_lossy().into_owned())
}

/// Ask a client to update itself; it installs the release its own updater
/// finds and restarts, while the controller keeps control through the hand-off.
#[tauri::command]
fn request_client_update(
    device_id: String,
    state: tauri::State<'_, AppRuntime>,
) -> Result<(), String> {
    send_peer_request(state.inner(), &device_id, REMOTE_UPDATE_PROTOCOL)
        .map_err(|error| format!("请求客户端更新失败: {error}"))
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

/// The update is downloaded and installs next. On Windows the updater ends
/// this process itself, without the exit hooks, so hand the network over now.
#[tauri::command]
fn prepare_update_install(state: tauri::State<'_, AppRuntime>) {
    state.hand_off_network();
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
    let enabled = manager
        .is_enabled()
        .map_err(|error| format!("failed to read launch-at-startup state: {error}"))?;
    #[cfg(target_os = "macos")]
    macos_set_relaunch_on_login(!enabled);
    Ok(enabled)
}

/// macOS "Reopen windows when logging back in" relaunches a running app at
/// login. With launch at startup on, our LaunchAgent starts it as well, and
/// the two race for the instance lock: the restored copy won (window shown,
/// not silent) and the autostart copy bounced off it. While the LaunchAgent
/// owns login launches, opt out of the restore relaunch as AppKit recommends
/// for launchd-launched apps.
#[cfg(target_os = "macos")]
fn macos_set_relaunch_on_login(relaunch: bool) {
    use std::ffi::c_void;
    use std::os::raw::c_char;

    #[link(name = "objc")]
    extern "C" {
        fn objc_getClass(name: *const c_char) -> *mut c_void;
        fn sel_registerName(name: *const c_char) -> *mut c_void;
        fn objc_msgSend();
    }

    // The two AppKit calls nest like a counter; keep at most one disable.
    static DISABLED: AtomicBool = AtomicBool::new(false);
    if DISABLED.swap(!relaunch, Ordering::Relaxed) == !relaunch {
        return;
    }
    let selector: &[u8] = if relaunch {
        b"enableRelaunchOnLogin\0"
    } else {
        b"disableRelaunchOnLogin\0"
    };
    unsafe {
        let app_class = objc_getClass(b"NSApplication\0".as_ptr() as *const c_char);
        if app_class.is_null() {
            return;
        }
        let msg: extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void =
            std::mem::transmute(objc_msgSend as *const ());
        let ns_app = msg(
            app_class,
            sel_registerName(b"sharedApplication\0".as_ptr() as *const c_char),
        );
        if !ns_app.is_null() {
            msg(ns_app, sel_registerName(selector.as_ptr() as *const c_char));
        }
    }
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
            let config_path =
                arg_value(&args, shared_input::SERVICE_CONFIG_PATH_ARG).map(PathBuf::from);
            let owner_sid = arg_value(&args, shared_input::SERVICE_OWNER_SID_ARG);
            match (helper_path, config_path, owner_sid) {
                (Some(path), Some(config_path), Some(owner_sid)) => {
                    if let Err(error) =
                        install_windows_input_service(&path, &config_path, &owner_sid)
                            .and_then(|_| start_windows_input_service())
                    {
                        eprintln!("{error}");
                    }
                }
                _ => eprintln!("missing input service path, config path, or owner SID"),
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
        System::Threading::{CreateMutexExW, SYNCHRONIZATION_SYNCHRONIZE},
    };

    let mutex_name = wide_null(SINGLE_INSTANCE_MUTEX_NAME);
    // We only need a lifetime handle and existence check, not mutex ownership.
    // Minimal access also lets a normal process detect an elevated instance.
    let mutex = unsafe { CreateMutexExW(std::ptr::null_mut(), mutex_name.as_ptr(), 0, SYNCHRONIZATION_SYNCHRONIZE) };
    if mutex.is_null() {
        log::error!("cannot acquire the single-instance lock (error {}); refusing a competing input runtime", unsafe { windows_sys::Win32::Foundation::GetLastError() });
        return false;
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

#[cfg(target_os = "macos")]
pub fn acquire_single_instance() -> bool {
    // Launch Services only dedupes Finder/Dock launches of the same bundle:
    // `open -n`, a second .app copy (e.g. one still on a mounted DMG), or a
    // bare binary all start another process, and two instances then fight
    // over the discovery/QUIC ports and the config dir.
    let Some(home) = std::env::var_os("HOME") else {
        return true;
    };
    let lock_dir = std::path::Path::new(&home)
        .join("Library/Application Support")
        .join(MACOS_BUNDLE_ID);
    if std::fs::create_dir_all(&lock_dir).is_err() {
        return true;
    }
    // Truncating a file another process holds a flock on is harmless: the
    // lock lives on the open file description, not the contents.
    let Ok(file) = std::fs::File::create(lock_dir.join("instance.lock")) else {
        // Never brick startup over lock-file plumbing.
        return true;
    };

    // An updater relaunch briefly overlaps the old and new processes, so retry
    // before concluding that a live instance owns the lock.
    for _ in 0..20 {
        match file.try_lock() {
            Ok(()) => {
                let _ = MACOS_INSTANCE_LOCK.set(file);
                return true;
            }
            Err(std::fs::TryLockError::WouldBlock) => thread::sleep(Duration::from_millis(100)),
            Err(std::fs::TryLockError::Error(_)) => return true,
        }
    }
    false
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
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

    #[cfg(target_os = "macos")]
    {
        // Launch Services delivers this to the running instance as a Reopen
        // event, which the app already answers by showing the main window.
        return std::process::Command::new("open")
            .args(["-b", MACOS_BUNDLE_ID])
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
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

pub fn launched_from_autostart() -> bool {
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

// Poll for display reconfiguration (lid open/close, monitor plug/unplug) and
// refresh the announced screen list. macOS enumerates displays via NSScreen,
// which must run on the main thread, so the change is applied there.
#[cfg(target_os = "macos")]
fn setup_macos_display_watcher(app: &tauri::App) {
    let app_handle = app.handle().clone();
    thread::spawn(move || {
        // Empty so the first stable reading re-checks what startup detected.
        let mut applied = Vec::new();
        let mut pending = None;
        loop {
            thread::sleep(Duration::from_millis(1500));
            let now = macos_display_fingerprint();
            if now == applied {
                pending = None;
                continue;
            }
            // Wake and lid changes arrive as bursts (4 flips in 5 s were seen
            // on wake); act once the set has held still for a tick.
            if pending.as_ref() != Some(&now) {
                pending = Some(now);
                continue;
            }
            applied = now;
            pending = None;
            let handle = app_handle.clone();
            let _ = app_handle.run_on_main_thread(move || {
                refresh_local_screens(&handle);
            });
        }
    });
}

/// CoreGraphics display ids and bounds: thread-safe and cheap, unlike the
/// NSScreen enumeration behind `detect_local_screens`, which needs the main
/// thread. Bounds catch resolution and arrangement changes as well.
#[cfg(target_os = "macos")]
fn macos_display_fingerprint() -> Vec<(u32, i64, i64, i64, i64)> {
    use core_graphics::display::CGDisplay;

    let mut displays: Vec<_> = CGDisplay::active_displays()
        .unwrap_or_default()
        .into_iter()
        .map(|id| {
            let bounds = CGDisplay::new(id).bounds();
            (
                id,
                bounds.origin.x as i64,
                bounds.origin.y as i64,
                bounds.size.width as i64,
                bounds.size.height as i64,
            )
        })
        .collect();
    displays.sort_unstable();
    displays
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // The capture thread owns the keyboard raw-input sink used for hook
        // health. Tao's unused DeviceEvents must not compete for registration.
        .device_event_filter(tauri::DeviceEventFilter::Always)
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
            let _ = APP_VERSION.set(app.config().version.clone().unwrap_or_else(|| app.package_info().version.to_string()));
            spawn_lock_sync_watcher(app.handle().clone());
            let silent_launch = launched_from_autostart();
            #[cfg(target_os = "macos")]
            {
                use tauri_plugin_autostart::ManagerExt;
                macos_set_relaunch_on_login(!app.autolaunch().is_enabled().unwrap_or(false));
            }
            if let Err(error) = app
                .handle()
                .plugin(tauri_plugin_updater::Builder::new().default_version_comparator(|current, release| {
                    let installed = APP_VERSION.get().and_then(|version| version.parse().ok()).unwrap_or(current);
                    release.version > installed
                }).build())
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
            let pending_files = Arc::downgrade(&app.state::<AppRuntime>().file_transfers);
            thread::spawn(move || loop {
                thread::sleep(Duration::from_secs(30));
                let Some(files) = pending_files.upgrade() else { break; };
                if let Ok(mut transfers) = files.lock() {
                    let now = now_ms();
                    transfers.retain(|_, transfer| {
                        if now.saturating_sub(transfer.touched_ms) <= 5 * 60 * 1000 { return true; }
                        let _ = fs::remove_file(&transfer.temp_path);
                        false
                    });
                };
            });

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
                        input_protection: Default::default(),
                    };
                }
            }

            #[cfg(target_os = "macos")]
            setup_macos_cursor_hider(app);
            #[cfg(target_os = "macos")]
            setup_macos_window_visibility_watcher(app);
            #[cfg(target_os = "macos")]
            setup_macos_display_watcher(app);
            setup_tray(app)?;
            if let Err(error) = sync_runtime_toggle_shortcut(app.handle()) {
                log::warn!("failed to register quick start/stop shortcut: {error}");
            }
            if let Err(error) = sync_screen_switch_shortcuts(app.handle()) {
                log::warn!("failed to register screen switch shortcuts: {error}");
            }
            #[cfg(target_os = "windows")]
            apply_custom_chrome(app.handle())?;
            let _ = sync_clipboard_paste_shortcut(app.handle());
            let clipboard_app = app.handle().clone();
            input::set_clipboard_paste_sender(Box::new(move |target| {
                let app = clipboard_app.clone();
                thread::spawn(move || { if let Err(error) = request_clipboard_paste(app.state::<AppRuntime>().inner(), target, true) {
                    let _ = app.emit("clipboard-paste-error", error);
                }});
            }));
            setup_single_instance_events(app.handle().clone());

            // Edge drag-drop (ShareMouse-style): the capture tap hands us drag
            // events as a local file drag crosses onto a controlled machine. A
            // Windows target gets a native OLE drag (icon + drop into any folder
            // or app); other targets transfer the files to land at the drop.
            #[cfg(target_os = "macos")]
            {
                let handle = app.handle().clone();
                // One worker, so control messages reach the peer in order: with a
                // thread per event a cancel could overtake its start, leaving the
                // Windows drag session open with a synthetic button held down.
                // File bytes stream on their own threads, so a drop is not held
                // back behind a large file.
                let (edge_drag_tx, edge_drag_rx) = std::sync::mpsc::channel::<input::EdgeDragEvent>();
                input::set_edge_drag_sender(Box::new(move |event| {
                    let _ = edge_drag_tx.send(event);
                }));
                thread::spawn(move || {
                    let to_paths = |files: Vec<std::path::PathBuf>| -> Vec<String> {
                        files
                            .iter()
                            .map(|path| path.to_string_lossy().into_owned())
                            .collect()
                    };
                    let deliver_to_desktop = |device_id: String, paths: Vec<String>, what: &'static str| {
                        let handle = handle.clone();
                        thread::spawn(move || {
                            let state = handle.state::<AppRuntime>();
                            match send_files_to_device_inner(
                                state.inner(),
                                &device_id,
                                &paths,
                                DropMode::Desktop,
                            ) {
                                Ok(summary) => log::info!(
                                    "{what}: delivered {} file(s) ({}) to {}'s Desktop",
                                    summary.file_count,
                                    format_bytes(summary.byte_count),
                                    summary.target_name
                                ),
                                Err(error) => log::warn!("{what} failed: {error}"),
                            }
                        });
                    };
                    // A drag whose native start the peer refused (e.g. a client too
                    // old for drag control) is delivered as a plain transfer when the
                    // button is released over it, instead of being lost (#33).
                    let mut refused: Option<(String, Vec<String>)> = None;
                    for event in edge_drag_rx {
                        let state = handle.state::<AppRuntime>();
                        let state = state.inner();
                        match event {
                            input::EdgeDragEvent::StartOle { device_id, files } => {
                                let paths = to_paths(files);
                                match send_ole_drag_start(state, &device_id, &paths) {
                                    Ok(stream) => {
                                        let handle = handle.clone();
                                        thread::spawn(move || {
                                            let state = handle.state::<AppRuntime>();
                                            match stream_ole_drag_files(state.inner(), stream) {
                                                Ok(count) => log::info!(
                                                    "native drag streamed {count} file(s) to {device_id}"
                                                ),
                                                Err(error) => {
                                                    log::warn!("native drag stream failed: {error}")
                                                }
                                            }
                                        });
                                    }
                                    Err(error) => {
                                        log::warn!("native drag start failed: {error}");
                                        if error.starts_with(DRAG_CONTROL_FAILED) {
                                            if state.layout_snapshot().machine_role == "client" { deliver_to_desktop(device_id, paths, "refused controller drag"); }
                                            else { refused = Some((device_id, paths)); }
                                        }
                                    }
                                }
                            }
                            input::EdgeDragEvent::DropOle { device_id } => {
                                if let Some((device_id, paths)) =
                                    refused.take_if(|(id, _)| *id == device_id)
                                {
                                    deliver_to_desktop(device_id, paths, "refused native drag");
                                    continue;
                                }
                                match send_ole_drag_signal(state, &device_id, "drop") {
                                    Ok(()) => log::info!("native drag drop sent to {device_id}"),
                                    Err(error) => log::warn!("native drag drop failed: {error}"),
                                }
                            }
                            input::EdgeDragEvent::CancelOle { device_id } => {
                                // The peer never opened a session for a refused start.
                                if refused.take_if(|(id, _)| *id == device_id).is_some() {
                                    continue;
                                }
                                if let Err(error) = send_ole_drag_signal(state, &device_id, "cancel")
                                {
                                    log::warn!("native drag cancel failed: {error}");
                                }
                            }
                            input::EdgeDragEvent::Transfer { device_id, files } => {
                                deliver_to_desktop(device_id, to_paths(files), "edge drag-drop");
                            }
                        }
                    }
                });
            }

            // The other direction: this Windows machine is the controller and
            // the user drags files onto a controlled machine. The edge catcher
            // grabs the OLE drop and we transfer the files to land on the
            // controlled machine's Desktop.
            #[cfg(target_os = "windows")]
            {
                let handle = app.handle().clone();
                windows_drop_catcher::init(Box::new(move |device_id, files| {
                    let handle = handle.clone();
                    thread::spawn(move || {
                        let paths: Vec<String> = files
                            .iter()
                            .map(|path| path.to_string_lossy().into_owned())
                            .collect();
                        let state = handle.state::<AppRuntime>();
                        match send_files_to_device_inner(
                            state.inner(),
                            &device_id,
                            &paths,
                            DropMode::DragDrop,
                        ) {
                            Ok(summary) => log::info!(
                                "edge drop delivered {} file(s) ({}) to {}",
                                summary.file_count,
                                format_bytes(summary.byte_count),
                                summary.target_name
                            ),
                            Err(error) => log::warn!("edge drop transfer failed: {error}"),
                        }
                    });
                }));
            }
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
            cancel_file_transfer,
            paste_remote_clipboard,
            start_runtime,
            stop_runtime,
            read_clipboard_text,
            write_clipboard_text,
            read_performance_sample,
            set_app_upgrading,
            prepare_update_install,
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
            fetch_client_log,
            request_client_update,
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
            tauri::RunEvent::Exit => {
                if let Some(state) = app.try_state::<AppRuntime>() {
                    state.hand_off_network();
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
    // With no window on screen (closed, or minimized — the visibility watcher
    // catches the native yellow button / Cmd+M) MyKVM lives in the tray: drop
    // out of the Dock and Cmd+Tab, and come back as a regular app when shown.
    #[cfg(target_os = "macos")]
    let _ = app.set_activation_policy(if visible {
        tauri::ActivationPolicy::Regular
    } else {
        tauri::ActivationPolicy::Accessory
    });
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
        input_protection: Default::default(),
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
                OpenSCManagerW, OpenServiceW, SC_MANAGER_CONNECT, SERVICE_QUERY_STATUS,
                SERVICE_RUNNING,
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

        let service_status = query_windows_service_status_process(service)?;
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
}

#[cfg(target_os = "windows")]
unsafe fn query_windows_service_status_process(
    service: windows_sys::Win32::System::Services::SC_HANDLE,
) -> Result<windows_sys::Win32::System::Services::SERVICE_STATUS_PROCESS, String> {
    use windows_sys::Win32::System::Services::{
        QueryServiceStatusEx, SC_STATUS_PROCESS_INFO, SERVICE_STATUS_PROCESS,
    };

    let mut status = SERVICE_STATUS_PROCESS::default();
    let mut needed = 0_u32;
    if QueryServiceStatusEx(
        service,
        SC_STATUS_PROCESS_INFO,
        &mut status as *mut SERVICE_STATUS_PROCESS as *mut u8,
        std::mem::size_of::<SERVICE_STATUS_PROCESS>() as u32,
        &mut needed,
    ) == 0
    {
        return Err(windows_last_error("QueryServiceStatusEx"));
    }
    Ok(status)
}

#[cfg(target_os = "windows")]
fn windows_input_service_process_id() -> Result<Option<u32>, String> {
    use windows_sys::Win32::{
        Foundation::{GetLastError, ERROR_SERVICE_DOES_NOT_EXIST, ERROR_SERVICE_MARKED_FOR_DELETE},
        System::Services::{
            OpenSCManagerW, OpenServiceW, SC_MANAGER_CONNECT, SERVICE_QUERY_STATUS,
        },
    };

    unsafe {
        let scm = OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_CONNECT);
        if scm.is_null() {
            return Err(windows_last_error("OpenSCManagerW(process id)"));
        }
        let _scm = ServiceHandleGuard(scm);
        let service_name = wide_null(shared_input::INPUT_SERVICE_NAME);
        let service = OpenServiceW(scm, service_name.as_ptr(), SERVICE_QUERY_STATUS);
        if service.is_null() {
            let code = GetLastError();
            if code == ERROR_SERVICE_DOES_NOT_EXIST || code == ERROR_SERVICE_MARKED_FOR_DELETE {
                return Ok(None);
            }
            return Err(windows_last_error("OpenServiceW(process id)"));
        }
        let _service = ServiceHandleGuard(service);
        let status = query_windows_service_status_process(service)?;
        Ok((status.dwProcessId != 0).then_some(status.dwProcessId))
    }
}

#[cfg(target_os = "windows")]
fn acquire_windows_input_service_network_lease() -> Result<Option<std::fs::File>, String> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use windows_sys::Win32::{
        Foundation::{
            GetLastError, ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY, ERROR_SEM_TIMEOUT, GENERIC_READ,
            GENERIC_WRITE, INVALID_HANDLE_VALUE,
        },
        Storage::FileSystem::{
            CreateFileW, ReadFile, WriteFile, FILE_ATTRIBUTE_NORMAL, OPEN_EXISTING,
        },
        System::Pipes::WaitNamedPipeW,
    };

    let pipe_name = wide_null(shared_input::SERVICE_CONTROL_PIPE);
    if unsafe { WaitNamedPipeW(pipe_name.as_ptr(), 2_000) } == 0 {
        let error = unsafe { GetLastError() };
        if matches!(
            error,
            ERROR_FILE_NOT_FOUND | ERROR_PIPE_BUSY | ERROR_SEM_TIMEOUT
        ) {
            return Ok(None);
        }
        return Err(format!(
            "WaitNamedPipeW(MyKVM service control) failed with Windows error {error}"
        ));
    }

    let handle = unsafe {
        CreateFileW(
            pipe_name.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            0,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(windows_last_error("CreateFileW(MyKVM service control)"));
    }

    let file = unsafe { std::fs::File::from_raw_handle(handle) };
    let raw = file.as_raw_handle();
    let request = [1_u8];
    let mut written = 0_u32;
    if unsafe {
        WriteFile(
            raw,
            request.as_ptr(),
            request.len() as u32,
            &mut written,
            std::ptr::null_mut(),
        )
    } == 0
        || written != request.len() as u32
    {
        return Err(windows_last_error("write MyKVM service takeover request"));
    }

    let mut ack = [0_u8; 1];
    let mut read = 0_u32;
    if unsafe {
        ReadFile(
            raw,
            ack.as_mut_ptr(),
            ack.len() as u32,
            &mut read,
            std::ptr::null_mut(),
        )
    } == 0
        || read != 1
        || ack[0] != 1
    {
        return Err("MyKVM input service did not acknowledge network takeover.".into());
    }

    Ok(Some(file))
}

#[cfg(target_os = "windows")]
fn windows_input_service_network_lease_alive(file: &std::fs::File) -> bool {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::Pipes::PeekNamedPipe;

    unsafe {
        PeekNamedPipe(
            file.as_raw_handle(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        ) != 0
    }
}

#[cfg(target_os = "windows")]
fn windows_input_service_owns_network_ports() -> Result<bool, String> {
    use windows_sys::Win32::System::Services::{
        OpenSCManagerW, OpenServiceW, QueryServiceConfigW, QUERY_SERVICE_CONFIGW,
        SC_MANAGER_CONNECT, SERVICE_QUERY_CONFIG,
    };

    unsafe {
        let scm = OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_CONNECT);
        if scm.is_null() {
            return Err(windows_last_error("OpenSCManagerW(query config)"));
        }
        let _scm = ServiceHandleGuard(scm);
        let service_name = wide_null(shared_input::INPUT_SERVICE_NAME);
        let service = OpenServiceW(scm, service_name.as_ptr(), SERVICE_QUERY_CONFIG);
        if service.is_null() {
            return Ok(false);
        }
        let _service = ServiceHandleGuard(service);

        let mut needed = 0_u32;
        let _ = QueryServiceConfigW(service, std::ptr::null_mut(), 0, &mut needed);
        if needed == 0 {
            return Err(windows_last_error("QueryServiceConfigW(size)"));
        }
        let words =
            (needed as usize + std::mem::size_of::<usize>() - 1) / std::mem::size_of::<usize>();
        let mut buffer = vec![0_usize; words];
        let config = buffer.as_mut_ptr() as *mut QUERY_SERVICE_CONFIGW;
        if QueryServiceConfigW(service, config, needed, &mut needed) == 0 {
            return Err(windows_last_error("QueryServiceConfigW"));
        }
        let path = (*config).lpBinaryPathName;
        if path.is_null() {
            return Ok(false);
        }
        let len = (0..)
            .find(|index| *path.add(*index) == 0)
            .unwrap_or_default();
        let command = String::from_utf16_lossy(std::slice::from_raw_parts(path, len));
        Ok(command.contains(shared_input::SERVICE_CONFIG_PATH_ARG))
    }
}

#[cfg(target_os = "windows")]
fn install_windows_input_service(
    helper_path: &PathBuf,
    config_path: &PathBuf,
    owner_sid: &str,
) -> Result<(), String> {
    use windows_sys::Win32::{
        Foundation::{GetLastError, ERROR_SERVICE_EXISTS},
        System::Services::{
            ChangeServiceConfig2W, ChangeServiceConfigW, CreateServiceW, OpenSCManagerW,
            OpenServiceW, SC_ACTION, SC_ACTION_RESTART, SC_MANAGER_CONNECT,
            SC_MANAGER_CREATE_SERVICE, SERVICE_ALL_ACCESS, SERVICE_AUTO_START,
            SERVICE_CONFIG_FAILURE_ACTIONS, SERVICE_CONFIG_FAILURE_ACTIONS_FLAG,
            SERVICE_ERROR_NORMAL, SERVICE_FAILURE_ACTIONSW, SERVICE_FAILURE_ACTIONS_FLAG,
            SERVICE_WIN32_OWN_PROCESS,
        },
    };

    if !helper_path.is_file() {
        return Err(format!(
            "input helper binary does not exist: {}",
            helper_path.display()
        ));
    }

    stop_windows_input_service_and_wait()?;
    let protected_helper_path = install_protected_input_helper(helper_path)?;

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
            "{} --service {} {} {} {}",
            quote_windows_arg_str(&protected_helper_path.to_string_lossy()),
            shared_input::SERVICE_CONFIG_PATH_ARG,
            quote_windows_arg_str(&config_path.to_string_lossy()),
            shared_input::SERVICE_OWNER_SID_ARG,
            quote_windows_arg_str(owner_sid),
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

        let mut actions = [1000, 5000, 10_000].map(|delay| SC_ACTION {
            Type: SC_ACTION_RESTART,
            Delay: delay,
        });
        let recovery = SERVICE_FAILURE_ACTIONSW {
            dwResetPeriod: 86400,
            cActions: actions.len() as u32,
            lpsaActions: actions.as_mut_ptr(),
            ..Default::default()
        };
        let failure_flags = SERVICE_FAILURE_ACTIONS_FLAG {
            fFailureActionsOnNonCrashFailures: 1,
        };
        if ChangeServiceConfig2W(
            service,
            SERVICE_CONFIG_FAILURE_ACTIONS,
            (&recovery as *const SERVICE_FAILURE_ACTIONSW).cast(),
        ) == 0
            || ChangeServiceConfig2W(
                service,
                SERVICE_CONFIG_FAILURE_ACTIONS_FLAG,
                (&failure_flags as *const SERVICE_FAILURE_ACTIONS_FLAG).cast(),
            ) == 0
        {
            return Err(windows_last_error("configure input service recovery"));
        }

        // Authenticated users may only inspect service state/config. The owner
        // may start, stop, and remove it, but cannot reconfigure it or its DACL.
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let sddl = format!(
            "D:P(A;;CCDCLCSWRPWPDTLOCRSDRCWDWO;;;SY)(A;;CCDCLCSWRPWPDTLOCRSDRCWDWO;;;BA)(A;;CCLCRPWPLOSDRC;;;{owner_sid})(A;;CCLC;;;AU)"
        );
        let result = std::process::Command::new("sc.exe")
            .args(["sdset", shared_input::INPUT_SERVICE_NAME, &sddl])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map_err(|error| format!("failed to secure input service permissions: {error}"))?;
        if !result.status.success() {
            let detail = String::from_utf8_lossy(if result.stderr.is_empty() {
                &result.stdout
            } else {
                &result.stderr
            });
            return Err(format!(
                "failed to secure input service permissions: {}",
                detail.trim()
            ));
        }

        ensure_windows_input_service_firewall_rule(&protected_helper_path)
    }
}

#[cfg(target_os = "windows")]
fn protected_input_helper_path() -> Result<PathBuf, String> {
    use windows::Win32::{
        System::Com::CoTaskMemFree,
        UI::Shell::{FOLDERID_ProgramFiles, SHGetKnownFolderPath, KNOWN_FOLDER_FLAG},
    };

    unsafe {
        let raw = SHGetKnownFolderPath(&FOLDERID_ProgramFiles, KNOWN_FOLDER_FLAG(0), None)
            .map_err(|error| format!("failed to locate Program Files: {error}"))?;
        let program_files = raw
            .to_string()
            .map_err(|error| format!("invalid Program Files path: {error}"));
        CoTaskMemFree(Some(raw.as_ptr().cast()));
        Ok(PathBuf::from(program_files?)
            .join("MyKVM")
            .join("mykvm-input-helper.exe"))
    }
}

#[cfg(target_os = "windows")]
fn install_protected_input_helper(source: &PathBuf) -> Result<PathBuf, String> {
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let target = protected_input_helper_path()?;
    let directory = target
        .parent()
        .ok_or_else(|| "protected input helper path has no parent directory".to_string())?;
    fs::create_dir_all(directory).map_err(|error| {
        format!(
            "failed to create protected input helper directory {}: {error}",
            directory.display()
        )
    })?;
    secure_protected_input_helper_path(directory)?;

    let staging = target.with_extension("exe.installing");
    let _ = fs::remove_file(&staging);
    let mut staging_file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&staging)
        .map_err(|error| {
            format!(
                "failed to create protected input helper staging file {}: {error}",
                staging.display()
            )
        })?;
    secure_protected_input_helper_path(&staging)?;
    let mut source_file = fs::File::open(source)
        .map_err(|error| format!("failed to open input helper {}: {error}", source.display()))?;
    std::io::copy(&mut source_file, &mut staging_file).map_err(|error| {
        format!(
            "failed to copy input helper to {}: {error}",
            staging.display()
        )
    })?;
    staging_file.sync_all().map_err(|error| {
        format!(
            "failed to flush protected input helper {}: {error}",
            staging.display()
        )
    })?;
    drop(staging_file);

    let staging_wide = wide_null(&staging.to_string_lossy());
    let target_wide = wide_null(&target.to_string_lossy());
    if unsafe {
        MoveFileExW(
            staging_wide.as_ptr(),
            target_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        let error = windows_last_error("MoveFileExW(protected input helper)");
        let _ = fs::remove_file(&staging);
        return Err(error);
    }
    if let Err(error) = secure_protected_input_helper_path(&target) {
        let _ = fs::remove_file(&target);
        return Err(error);
    }

    Ok(target)
}

#[cfg(target_os = "windows")]
fn secure_protected_input_helper_path(path: &std::path::Path) -> Result<(), String> {
    use windows_sys::Win32::{
        Foundation::{LocalFree, HLOCAL},
        Security::{
            Authorization::{
                ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
            },
            SetFileSecurityW, DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
        },
    };

    let descriptor = wide_null("D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)");
    let mut security_descriptor = std::ptr::null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            descriptor.as_ptr(),
            SDDL_REVISION_1,
            &mut security_descriptor,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(windows_last_error(
            "ConvertStringSecurityDescriptorToSecurityDescriptorW(protected helper)",
        ));
    }

    let path = wide_null(&path.to_string_lossy());
    let secured = unsafe {
        SetFileSecurityW(
            path.as_ptr(),
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            security_descriptor,
        )
    } != 0;
    let error = (!secured)
        .then(|| windows_last_error("SetFileSecurityW(protected input helper)"));
    unsafe {
        let _ = LocalFree(security_descriptor as HLOCAL);
    }
    if let Some(error) = error {
        return Err(error);
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn remove_protected_input_helper() {
    let Ok(target) = protected_input_helper_path() else {
        return;
    };
    let _ = fs::remove_file(&target);
    let _ = fs::remove_file(target.with_extension("exe.installing"));
    if let Some(directory) = target.parent() {
        let _ = fs::remove_dir(directory);
    }
}

#[cfg(target_os = "windows")]
fn stop_windows_input_service_and_wait() -> Result<(), String> {
    use windows_sys::Win32::{
        Foundation::{
            GetLastError, ERROR_SERVICE_CANNOT_ACCEPT_CTRL, ERROR_SERVICE_DOES_NOT_EXIST,
            ERROR_SERVICE_NOT_ACTIVE,
        },
        System::Services::{
            ControlService, OpenSCManagerW, OpenServiceW, QueryServiceStatus, SC_MANAGER_CONNECT,
            SERVICE_CONTROL_STOP, SERVICE_QUERY_STATUS, SERVICE_STATUS, SERVICE_STOP,
            SERVICE_STOPPED, SERVICE_STOP_PENDING,
        },
    };

    unsafe {
        let scm = OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_CONNECT);
        if scm.is_null() {
            return Err(windows_last_error("OpenSCManagerW(stop)"));
        }
        let _scm = ServiceHandleGuard(scm);
        let service_name = wide_null(shared_input::INPUT_SERVICE_NAME);
        let service = OpenServiceW(
            scm,
            service_name.as_ptr(),
            SERVICE_STOP | SERVICE_QUERY_STATUS,
        );
        if service.is_null() {
            if GetLastError() == ERROR_SERVICE_DOES_NOT_EXIST {
                return Ok(());
            }
            return Err(windows_last_error("OpenServiceW(stop)"));
        }
        let _service = ServiceHandleGuard(service);
        let deadline = Instant::now() + Duration::from_secs(15);

        loop {
            let mut status = SERVICE_STATUS::default();
            if QueryServiceStatus(service, &mut status) == 0 {
                return Err(windows_last_error("QueryServiceStatus(stop)"));
            }
            if status.dwCurrentState == SERVICE_STOPPED {
                return Ok(());
            }

            if status.dwCurrentState != SERVICE_STOP_PENDING {
                let mut stop_status = SERVICE_STATUS::default();
                if ControlService(service, SERVICE_CONTROL_STOP, &mut stop_status) == 0 {
                    let code = GetLastError();
                    if code == ERROR_SERVICE_NOT_ACTIVE {
                        return Ok(());
                    }
                    if code != ERROR_SERVICE_CANNOT_ACCEPT_CTRL {
                        return Err(windows_last_error("ControlService(stop)"));
                    }
                }
            }

            if Instant::now() >= deadline {
                return Err("timed out waiting for MyKVM input service to stop".into());
            }
            thread::sleep(Duration::from_millis(200));
        }
    }
}

#[cfg(target_os = "windows")]
fn current_windows_user_sid() -> Result<String, String> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, LocalFree, HLOCAL},
        Security::{
            Authorization::ConvertSidToStringSidW, GetTokenInformation, TokenUser, TOKEN_QUERY,
            TOKEN_USER,
        },
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };

    unsafe {
        let mut token = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(windows_last_error("OpenProcessToken(current user SID)"));
        }

        let mut needed = 0_u32;
        let _ = GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed);
        if needed == 0 {
            let _ = CloseHandle(token);
            return Err(windows_last_error(
                "GetTokenInformation(current user SID size)",
            ));
        }
        let mut buffer = vec![0_u8; needed as usize];
        if GetTokenInformation(
            token,
            TokenUser,
            buffer.as_mut_ptr() as *mut _,
            needed,
            &mut needed,
        ) == 0
        {
            let _ = CloseHandle(token);
            return Err(windows_last_error("GetTokenInformation(current user SID)"));
        }
        let _ = CloseHandle(token);

        let token_user = &*(buffer.as_ptr() as *const TOKEN_USER);
        let mut sid = std::ptr::null_mut();
        if ConvertSidToStringSidW(token_user.User.Sid, &mut sid) == 0 {
            return Err(windows_last_error("ConvertSidToStringSidW"));
        }
        let len = (0..)
            .find(|index| *sid.add(*index) == 0)
            .unwrap_or_default();
        let value = String::from_utf16_lossy(std::slice::from_raw_parts(sid, len));
        let _ = LocalFree(sid as HLOCAL);
        Ok(value)
    }
}

#[cfg(target_os = "windows")]
fn ensure_windows_input_service_firewall_rule(helper_path: &PathBuf) -> Result<(), String> {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const RULE_NAME: &str = "MyKVM Headless Input (UDP-In)";
    let _ = Command::new("netsh")
        .args([
            "advfirewall",
            "firewall",
            "delete",
            "rule",
            &format!("name={RULE_NAME}"),
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    let result = Command::new("netsh")
        .args([
            "advfirewall",
            "firewall",
            "add",
            "rule",
            &format!("name={RULE_NAME}"),
            "dir=in",
            "action=allow",
            &format!("program={}", helper_path.display()),
            "protocol=UDP",
            "profile=any",
            "enable=yes",
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|error| format!("failed to configure helper firewall rule: {error}"))?;
    if !result.status.success() {
        return Err(format!(
            "failed to configure helper firewall rule: {}",
            String::from_utf8_lossy(&result.stderr).trim()
        ));
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn remove_windows_input_service_firewall_rule() {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let _ = Command::new("netsh")
        .args([
            "advfirewall",
            "firewall",
            "delete",
            "rule",
            "name=MyKVM Headless Input (UDP-In)",
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
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
        System::Services::{DeleteService, OpenSCManagerW, OpenServiceW, SC_MANAGER_CONNECT},
    };

    remove_windows_input_service_firewall_rule();
    stop_windows_input_service_and_wait()?;

    unsafe {
        let scm = OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_CONNECT);
        if scm.is_null() {
            return Err(windows_last_error("OpenSCManagerW"));
        }
        let _scm = ServiceHandleGuard(scm);
        let service_name = wide_null(shared_input::INPUT_SERVICE_NAME);
        let service = OpenServiceW(scm, service_name.as_ptr(), DELETE);
        if service.is_null() {
            let code = GetLastError();
            if code == ERROR_SERVICE_DOES_NOT_EXIST {
                remove_protected_input_helper();
                return Ok(());
            }
            return Err(windows_last_error("OpenServiceW(uninstall)"));
        }
        let _service = ServiceHandleGuard(service);

        if DeleteService(service) == 0 {
            return Err(windows_last_error("DeleteService"));
        }
    }

    remove_protected_input_helper();
    Ok(())
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
        clipboard_on_demand: false,
        clipboard_paste_hotkey: default_clipboard_paste_hotkey(),
        file_transfer_enabled: default_file_transfer_enabled(),
        lock_sync: false,
        input_protection: Default::default(),
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
            pointer_speed: 1.0,
            scroll_speed: 1.0,
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
        clipboard_on_demand: false,
        clipboard_paste_hotkey: default_clipboard_paste_hotkey(),
        file_transfer_enabled: default_file_transfer_enabled(),
        lock_sync: false,
        input_protection: Default::default(),
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

/// Re-detect this machine's displays and update the local device's screens in
/// the runtime layout, carrying over the user's saved on-canvas positions.
/// Called when the display configuration changes (e.g. a MacBook lid closes)
/// so the announce loop advertises the current screens instead of the stale
/// list captured at startup.
#[cfg(target_os = "macos")]
fn refresh_local_screens(app: &AppHandle) {
    let Some(state) = app.try_state::<AppRuntime>() else {
        return;
    };
    let state = state.inner();

    let local_id = {
        let Ok(layout) = state.layout.lock() else {
            return;
        };
        match layout.devices.iter().find(|device| device.role == "local") {
            Some(local) => local.id.clone(),
            None => return,
        }
    };

    let detected = detect_local_screens(app, &local_id);

    let updated = {
        let Ok(mut layout) = state.layout.lock() else {
            return;
        };
        let Ok(mut native) = state.native_layout.lock() else {
            return;
        };
        let Ok(mut memory) = screen_layout_memory().lock() else {
            return;
        };
        let changed = apply_detected_local_screens(&mut layout, &mut native, detected, &mut memory);
        drop(memory);
        drop(native);
        if !changed {
            None
        } else {
            if let Err(error) = write_layout_to_disk(&state.config_path, &layout) {
                log::warn!("failed to save refreshed display layout: {error}");
            }
            Some(layout.clone())
        }
    };

    if let Some(snapshot) = updated {
        if let Some(local) = snapshot.devices.iter().find(|d| d.role == "local") {
            let desc: Vec<String> = local
                .screens
                .iter()
                .map(|s| format!("{}[{}x{}]@({},{})", s.id, s.width, s.height, s.x, s.y))
                .collect();
            log::info!(
                "local displays changed: now advertising {} screen(s): {}",
                local.screens.len(),
                desc.join(", ")
            );
        }
        // Capture also caches native cursor geometry. Rebuild it on a real
        // display change; the live QUIC receiver reads the same native layout.
        // Off the main thread: the restart sleeps and waits for the event tap.
        let app = app.clone();
        thread::spawn(move || {
            if let Err(error) = restart_runtime_if_running(app.state::<AppRuntime>().inner()) {
                log::warn!("failed to refresh input after display change: {error}");
            }
        });
    }
}

// Keep disconnected displays for the next lid-open/reconnect event.
#[cfg(target_os = "macos")]
fn screen_layout_memory() -> &'static Mutex<Vec<Screen>> {
    static MEMORY: Mutex<Vec<Screen>> = Mutex::new(Vec::new());
    &MEMORY
}

/// Match names and dimensions before volatile enumeration ids.
// ponytail: identical names and dimensions retain enumeration order; use OS display UUIDs if those monitors must be distinguished across reboots.
fn restore_local_screen_layout(
    detected: Vec<Screen>,
    current: &[Screen],
    memory: &mut Vec<Screen>,
) -> Vec<Screen> {
    // NSScreen can temporarily be empty during sleep or display reconfiguration.
    // Never persist its 1x1 fallback over the last usable arrangement.
    if !local_screens_available(&detected) {
        return current.to_vec();
    }
    for screen in current.iter().filter(|s| s.width > 1 && s.height > 1) {
        if let Some(saved) = memory.iter_mut().find(|s| s.id == screen.id) {
            *saved = screen.clone();
        } else {
            memory.push(screen.clone());
        }
    }
    let mut used_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut merged: Vec<Screen> = Vec::with_capacity(detected.len());
    for mut screen in detected {
        let saved = memory
            .iter()
            .filter(|s| !used_ids.contains(&s.id))
            .filter(|s| {
                s.name == screen.name
                    || (s.width, s.height) == (screen.width, screen.height)
                    || s.id == screen.id
            })
            .min_by_key(|s| {
                (
                    s.name != screen.name,
                    (s.width, s.height) != (screen.width, screen.height),
                    s.id != screen.id,
                )
            });
        if let Some(saved) = saved {
            screen.id = saved.id.clone();
            screen.x = saved.x;
            screen.y = saved.y;
        }
        while used_ids.contains(&screen.id) {
            screen.id = format!("{}-b", screen.id);
        }
        used_ids.insert(screen.id.clone());
        merged.push(screen);
    }
    merged
}

fn local_screens_available(screens: &[Screen]) -> bool {
    screens
        .iter()
        .any(|screen| screen.width > 1 && screen.height > 1)
}

fn align_native_screen_ids(native: &mut LayoutState, arranged: &LayoutState) {
    let Some(local) = native.devices.iter_mut().find(|d| d.role == "local") else {
        return;
    };
    if !local_screens_available(&local.screens) {
        return;
    }
    let Some(arranged) = arranged.devices.iter().find(|d| d.role == "local") else {
        return;
    };
    for (screen, arranged) in local.screens.iter_mut().zip(&arranged.screens) {
        screen.id = arranged.id.clone();
    }
}

#[cfg(any(target_os = "macos", test))]
fn apply_detected_local_screens(
    layout: &mut LayoutState,
    native: &mut LayoutState,
    detected: Vec<Screen>,
    memory: &mut Vec<Screen>,
) -> bool {
    if !local_screens_available(&detected) {
        return false;
    }
    let Some(local) = layout.devices.iter_mut().find(|d| d.role == "local") else {
        return false;
    };
    let Some(native_local) = native.devices.iter_mut().find(|d| d.role == "local") else {
        return false;
    };
    let merged = restore_local_screen_layout(detected.clone(), &local.screens, memory);
    let mut native_screens = detected;
    for (screen, arranged) in native_screens.iter_mut().zip(&merged) {
        screen.id = arranged.id.clone();
    }
    if local.screens == merged && native_local.screens == native_screens {
        return false;
    }
    local.screens = merged;
    native_local.screens = native_screens;
    let fallback = local
        .screens
        .iter()
        .find(|s| s.is_primary)
        .or_else(|| local.screens.first())
        .map(|s| s.id.clone());
    if !layout
        .devices
        .iter()
        .flat_map(|d| &d.screens)
        .any(|s| s.id == layout.selected_screen_id)
    {
        if let Some(id) = fallback {
            layout.selected_screen_id = id;
        }
    }
    true
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
        clipboard_on_demand: false,
        clipboard_paste_hotkey: default_clipboard_paste_hotkey(),
        file_transfer_enabled: saved_layout.file_transfer_enabled,
        lock_sync: saved_layout.lock_sync,
        input_protection: InputProtection::default(),
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
        detected_device.screens = restore_local_screen_layout(
            detected_device.screens,
            &saved_device.screens,
            &mut Vec::new(),
        );
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
    match (hostname(), local_ip_list()) {
        (Some(name), Some(ips)) => format!("{name} / {ips}"),
        (Some(name), None) => name,
        (None, Some(ips)) => ips,
        (None, None) => "localhost".into(),
    }
}

/// The default-route address first, then every other usable IPv4, so a
/// direct-cable or Thunderbolt-bridge address is visible for manual pairing
/// instead of only the Wi-Fi one (#33).
fn local_ip_list() -> Option<String> {
    let primary = local_ip_address();
    let mut ips: Vec<String> = primary.iter().cloned().collect();
    ips.extend(
        local_ipv4_addresses()
            .into_iter()
            .map(|ip| ip.to_string())
            .filter(|ip| Some(ip) != primary.as_ref()),
    );
    (!ips.is_empty()).then(|| ips.join(", "))
}

fn local_ip_address() -> Option<String> {
    // Callers include per-event hot paths (input target building, packet
    // origin resolution) and the discovery loop; the UDP-socket probe is four
    // syscalls. The LAN address changes on network reconfigs, not per event —
    // cache it briefly.
    const LOCAL_IP_CACHE_TTL: Duration = Duration::from_secs(5);
    static CACHE: Mutex<Option<(Instant, Option<String>)>> = Mutex::new(None);

    if let Ok(mut cached) = CACHE.lock() {
        if let Some((probed_at, address)) = cached.as_ref() {
            if probed_at.elapsed() < LOCAL_IP_CACHE_TTL {
                return address.clone();
            }
        }
        let fresh = probe_local_ip_address();
        *cached = Some((Instant::now(), fresh.clone()));
        return fresh;
    }
    probe_local_ip_address()
}

fn probe_local_ip_address() -> Option<String> {
    // Prefer the interface that routes to the internet, but reject a loopback /
    // otherwise-unusable result: right after wake the network stack can hand
    // back 127.0.0.1, and announcing that address makes peers unable to connect
    // (the "worked yesterday, dead this morning" symptom). Fall back to any real
    // LAN interface address so we never advertise loopback.
    //
    // A proxy in TUN mode owns the default route; its tunnel address is not the
    // LAN, and since the device id is host + this address, the id flipped every
    // time the proxy toggled. Skip a point-to-point tunnel and pick the LAN.
    if let Some(ip) = default_route_ipv4_address() {
        if usable_discovery_ipv4(ip) && !point_to_point_ipv4(ip) {
            return Some(ip.to_string());
        }
    }
    preferred_lan_ipv4(&local_ipv4_addresses()).map(|ip| ip.to_string())
}

/// The likeliest physical-LAN address: host-side virtual adapters (VirtualBox,
/// VMware, Hyper-V/WSL) usually end in .1, then home > corporate > container
/// ranges.
// ponytail: heuristic, only used while a tunnel holds the default route; read
// the OS routing table's non-tunnel default route if this ever picks wrong.
fn preferred_lan_ipv4(addresses: &[Ipv4Addr]) -> Option<Ipv4Addr> {
    addresses.iter().copied().min_by_key(|ip| {
        let octets = ip.octets();
        let class = match octets {
            [192, 168, ..] => 0,
            [10, ..] => 1,
            [172, b, ..] if (16..32).contains(&b) => 2,
            _ => 3,
        };
        (octets[3] == 1, class)
    })
}

/// The default route's interface is a /30-or-narrower point-to-point link: a
/// TUN adapter (sing-box style), never a LAN.
fn point_to_point_ipv4(ip: Ipv4Addr) -> bool {
    if_addrs::get_if_addrs()
        .map(|interfaces| {
            interfaces.iter().any(|interface| match &interface.addr {
                if_addrs::IfAddr::V4(address) => {
                    address.ip == ip && u32::from(address.netmask).count_ones() >= 30
                }
                _ => false,
            })
        })
        .unwrap_or(false)
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
    let [a, b, ..] = address.octets();
    !address.is_loopback()
        && !address.is_unspecified()
        && !address.is_multicast()
        && !address.is_broadcast()
        && !address.is_link_local()
        // 198.18.0.0/15 (RFC 2544 benchmarking) is the TUN range of Clash,
        // Mihomo and Surge — never a LAN a peer could reach.
        && !(a == 198 && b & 0xfe == 18)
}

fn default_input_speed() -> f64 {
    1.0
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

fn sync_clipboard_paste_shortcut(app: &AppHandle) -> Result<(), String> {
    let Some(state) = app.try_state::<AppRuntime>() else { return Ok(()); };
    let layout = state.layout_snapshot();
    let next = if layout.machine_role == "server" && layout.clipboard_sync {
        canonical_runtime_toggle_shortcut(&layout.clipboard_paste_hotkey)?
    } else { None };
    let mut current = state.clipboard_paste_shortcut.lock().map_err(|_| "clipboard shortcut is poisoned".to_string())?;
    if *current == next { return Ok(()); }
    if let Some(previous) = current.take() { let _ = app.global_shortcut().unregister(previous.as_str()); }
    if let Some(next) = next { app.global_shortcut().register(next.as_str()).map_err(|e| e.to_string())?; *current = Some(next); }
    Ok(())
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClipboardFilesCommit {
    protocol: String, origin_id: String, origin_transport_public_key: String, target_id: String,
    cluster_id: String, pair_secret: String, ids: Vec<String>, paste: bool, paste_token: String,
}

fn paste_permitted(layout: &LayoutState, paste: bool, token: &str, source_key: &str) -> bool {
    paste && (layout.machine_role == "client" || clipboard_exchange::consume_paste(token, source_key))
}

const CLIPBOARD_FILES_SUPERSEDED: &str = "文件剪贴板已改变或同步已关闭。";

fn ensure_file_transfer_current(keep_going: Option<&dyn Fn() -> bool>) -> Result<(), String> {
    if keep_going.is_some_and(|check| !check()) {
        Err(CLIPBOARD_FILES_SUPERSEDED.into())
    } else {
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn send_clipboard_files(
    transport: &quic_transport::TransportHandle,
    local_id: &str,
    target: &FileTransferTarget,
    paths: &[String],
    paste: bool,
    token: String,
    app: Option<&AppHandle>,
    keep_going: Option<&dyn Fn() -> bool>,
) -> Result<u64, String> {
    if paths.len() > 64 {
        return Err("一次最多可粘贴 64 个文件或文件夹。".into());
    }
    ensure_file_transfer_current(keep_going)?;
    let probe = file_transfer_packet(
        "clipboard-probe",
        "",
        local_id,
        target,
        "",
        0,
        0,
        0,
        Vec::new(),
        DropMode::Clipboard,
        false,
    );
    send_file_transfer_packet(transport, target, probe)?;
    let mut ids = Vec::new();
    let mut packet_count = 1;
    for (index, path) in paths.iter().enumerate() {
        ensure_file_transfer_current(keep_going)?;
        // Prepare one entry at a time rather than spooling every selected folder.
        let files = collect_transfer_files(std::slice::from_ref(path))?;
        for file in files {
            let id = new_transfer_id("clipboard");
            let reporter = app.map(|app| FileTransferProgressReporter {
                app,
                target_name: &target.name,
                file_index: index + 1,
                file_count: paths.len(),
            });
            packet_count += send_transfer_file(
                transport,
                local_id,
                target,
                &file,
                &id,
                DropMode::Clipboard,
                reporter.as_ref(),
                keep_going,
            )?;
            ids.push(id);
        }
    }
    // A large file may finish after another copy or after the switch is disabled.
    // Leave the new clipboard alone instead of committing that old selection.
    ensure_file_transfer_current(keep_going)?;
    let commit = ClipboardFilesCommit {
        protocol: CLIPBOARD_FILES_PROTOCOL.into(),
        origin_id: local_id.into(),
        origin_transport_public_key: transport.public_key().into(),
        target_id: target.device_id.clone(),
        cluster_id: target.cluster_id.clone(),
        pair_secret: target.pair_secret.clone(),
        ids,
        paste,
        paste_token: token,
    };
    transport.send_stream_expect_ack(
        transport.peer(
            target.addr.clone(),
            target.transport_public_key.clone(),
            target.protocol_version,
        ),
        encode_wire_packet(&commit)?,
    )?;
    Ok(packet_count + 1)
}

fn send_current_clipboard(state: &AppRuntime, target: &FileTransferTarget, paste: bool, token: String) -> Result<(), String> {
    let layout = state.layout_snapshot();
    if !layout.clipboard_sync { return Err("剪贴板同步未开启。".into()); }
    let transport = state.quic_transport_handle().ok_or("传输尚未就绪。")?;
    let mut local = local_peer_from_layout(&layout); apply_transport_to_peer(&mut local, &transport);
    let peer = transport.peer(target.addr.clone(), target.transport_public_key.clone(), target.protocol_version);
    let paths = rich_clipboard::read_files();
    if !paths.is_empty() {
        if !layout.file_transfer_enabled { return Err("文件传输未开启。".into()); }
        return send_clipboard_files(&transport, &local.id, target, &paths, paste, token,
            Some(&state.app_handle), None).map(|_| ());
    }
    let content = clipboard::read_content().ok_or("剪贴板中没有可传输的内容。")?;
    if content.is_oversized() { return Err("剪贴板内容超过大小上限。".into()); }
    let mut packet = clipboard_packet_from_content(content, local.id, transport.public_key().into(), target.device_id.clone(),
        target.cluster_id.clone(), target.pair_secret.clone(), next_clipboard_sequence());
    packet.paste = paste; packet.paste_token = token;
    transport.send_stream_expect_ack(peer, encode_wire_packet(&packet)?)
}

fn request_clipboard_paste(state: &AppRuntime, target: input::ClipboardTarget, remote: bool) -> Result<(), String> {
    let destination = FileTransferTarget { device_id: target.device_id, name: String::new(), addr: target.addr,
        transport_public_key: target.transport_public_key, protocol_version: target.protocol_version,
        cluster_id: target.cluster_id, pair_secret: target.pair_secret };
    if remote { return send_current_clipboard(state, &destination, true, String::new()); }
    let layout = state.layout_snapshot();
    let transport = state.quic_transport_handle().ok_or("传输尚未就绪。")?;
    let local = local_peer_from_layout(&layout);
    let token = clipboard_exchange::expect_paste(&destination.transport_public_key);
    let packet = PeerRequestPacket { protocol: CLIPBOARD_PULL_PROTOCOL.into(), paste_token: token,
        origin_id: local.id, target_id: destination.device_id, cluster_id: destination.cluster_id, pair_secret: destination.pair_secret };
    transport.send_stream_expect_ack(transport.peer(destination.addr, destination.transport_public_key, destination.protocol_version), encode_wire_packet(&packet)?)
}

#[tauri::command]
fn paste_remote_clipboard(state: tauri::State<'_, AppRuntime>) -> Result<(), String> {
    let target = input::current_clipboard_target(&state.clipboard_target).ok_or("请先切换到目标电脑一次。")?;
    let remote = state.remote_input_active.load(Ordering::Relaxed);
    let app = state.app_handle.clone();
    thread::spawn(move || { if let Err(error) = request_clipboard_paste(app.state::<AppRuntime>().inner(), target, remote) {
        let _ = app.emit("clipboard-paste-error", error);
    }});
    Ok(())
}

fn handle_clipboard_files_commit(
    payload: &[u8],
    layout: &LayoutState,
    local_id: &str,
    clipboard_seen: &Arc<Mutex<Option<String>>>,
    clipboard_echo_until: &Arc<Mutex<Option<Instant>>>,
) -> Option<bool> {
    let accepted = handle_clipboard_files_commit_with_writer(
        payload,
        layout,
        local_id,
        clipboard_seen,
        clipboard_echo_until,
        |paths| {
            retry_clipboard_write(
                CLIPBOARD_WRITE_ATTEMPTS,
                Duration::from_millis(CLIPBOARD_WRITE_RETRY_DELAY_MS),
                || rich_clipboard::write_files(paths),
            )
        },
    );
    if accepted == Some(true) {
        let packet = decode_wire_packet::<ClipboardFilesCommit>(payload)?;
        if paste_permitted(
            layout,
            packet.paste,
            &packet.paste_token,
            &packet.origin_transport_public_key,
        ) {
            input::paste_received_clipboard();
        }
    }
    accepted
}

fn handle_clipboard_files_commit_with_writer<F>(
    payload: &[u8],
    layout: &LayoutState,
    local_id: &str,
    clipboard_seen: &Arc<Mutex<Option<String>>>,
    clipboard_echo_until: &Arc<Mutex<Option<Instant>>>,
    mut write_files: F,
) -> Option<bool>
where
    F: FnMut(&[String]) -> Result<(), String>,
{
    let packet = decode_wire_packet::<ClipboardFilesCommit>(payload)?;
    if packet.protocol != CLIPBOARD_FILES_PROTOCOL {
        return None;
    }
    if !layout.clipboard_sync || !layout.file_transfer_enabled {
        return Some(false);
    }
    if packet.paste
        && layout.machine_role != "client"
        && !clipboard_exchange::paste_expected(
            &packet.paste_token,
            &packet.origin_transport_public_key,
        )
    {
        return Some(false);
    }
    let auth = clipboard_packet_from_content(
        ClipboardContent::Text(String::new()),
        packet.origin_id.clone(),
        packet.origin_transport_public_key.clone(),
        packet.target_id.clone(),
        packet.cluster_id,
        packet.pair_secret,
        0,
    );
    if !clipboard_packet_authorized(layout, &auth)
        || !clipboard_packet_targets_local(layout, &auth, local_id)
    {
        return Some(false);
    }
    let Some(paths) = clipboard_exchange::take_files(&packet.origin_id, &packet.ids) else {
        return Some(false);
    };
    // Serialize the native write and its echo marker with the polling thread.
    let Ok(mut seen) = clipboard_seen.lock() else {
        return Some(false);
    };
    if let Err(error) = write_files(&paths) {
        log::warn!("file clipboard receive write failed: {error}");
        return Some(false);
    }
    *seen = Some(clipboard::files_signature(&paths));
    arm_clipboard_echo_guard(clipboard_echo_until);
    Some(true)
}

fn default_clipboard_paste_hotkey() -> String { "disabled".into() }

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
    #[serde(default)]
    paste: bool,
    #[serde(default)]
    paste_token: String,
    origin_id: String,
    // The sender's QUIC transport public key. Defaulted so packets from older
    // peers (which never sent it) still decode as an empty string. Authorization
    // matches on this stable key first, exactly like input packets, so a copy
    // still syncs after the origin's derived peer id drifts (e.g. its LAN IP
    // changed since pairing) — the bug where input kept working but clipboard
    // silently stopped in one direction.
    #[serde(default)]
    origin_transport_public_key: String,
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

fn next_clipboard_sequence() -> u64 {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    loop {
        let previous = SEQUENCE.load(Ordering::Relaxed);
        let next = now_ms().max(previous.saturating_add(1));
        if SEQUENCE.compare_exchange(previous, next, Ordering::Relaxed, Ordering::Relaxed).is_ok() { return next; }
    }
}

fn clipboard_packet_from_content(
    content: ClipboardContent,
    origin_id: String,
    origin_transport_public_key: String,
    target_id: String,
    cluster_id: String,
    pair_secret: String,
    sequence: u64,
) -> ClipboardPacket {
    let signature = content.signature();
    let (text, formats) = match content {
        ClipboardContent::Text(text) => (text.clone(), vec![ClipboardFormat { kind: "plainText".into(), text, image: None }]),
        ClipboardContent::Image(image) => (String::new(), vec![ClipboardFormat { kind: "imageRgba".into(), text: String::new(), image: Some(image) }]),
        ClipboardContent::Rich(rich) => {
            let mut formats = Vec::new();
            if let Some(html) = rich.html { formats.push(ClipboardFormat { kind: "htmlFragment".into(), text: html, image: None }); }
            if let Some(rtf) = rich.rtf { formats.push(ClipboardFormat { kind: "rtfBytes".into(), text: { use base64::Engine as _; base64::engine::general_purpose::STANDARD.encode(rtf) }, image: None }); }
            formats.push(ClipboardFormat { kind: "plainText".into(), text: rich.text.clone(), image: None });
            (rich.text, formats)
        }
    };
    ClipboardPacket { protocol: CLIPBOARD_PROTOCOL.into(),
            paste: false, paste_token: String::new(), origin_id, origin_transport_public_key,
        target_id, cluster_id, pair_secret, signature, formats, text, image: None, sequence }
}

fn clipboard_retry_delay(failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(5);
    Duration::from_millis((CLIPBOARD_RETRY_INTERVAL_MS << doublings).min(CLIPBOARD_RETRY_MAX_MS))
}

fn run_clipboard_sync(
    quic_transport: quic_transport::TransportHandle,
    local_peer_id: String,
    clipboard_seen_text: Arc<Mutex<Option<String>>>,
    clipboard_echo_until: Arc<Mutex<Option<Instant>>>,
    clipboard_target: Arc<Mutex<Option<input::ClipboardTarget>>>,
    transport_packets: Arc<AtomicU64>,
    clipboard_packets: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    layout_state: Arc<Mutex<LayoutState>>,
    app: AppHandle,
) {
    let mut last_sent: Option<(String, String, String)> = None;
    // (device, addr, signature, failed_at, consecutive failures)
    let mut last_failed: Option<(String, String, String, Instant, u32)> = None;
    let mut last_read: Option<(u64, String, String)> = None;
    let mut last_poll = Instant::now() - Duration::from_secs(1);
    let mut last_file_sync_enabled = false;

    while !stop.load(Ordering::Relaxed) {
        let Some((sync, on_demand, file_transfer_enabled)) =
            layout_state.lock().ok().map(|layout| {
                (
                    layout.clipboard_sync,
                    layout.clipboard_on_demand,
                    layout.file_transfer_enabled,
                )
            })
        else {
            break;
        };
        if !sync {
            break;
        }
        if on_demand {
            thread::sleep(Duration::from_millis(150));
            continue;
        }
        if file_transfer_enabled != last_file_sync_enabled {
            last_read = None;
            last_sent = None;
            last_failed = None;
            last_file_sync_enabled = file_transfer_enabled;
        }

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

        let version = clipboard::change_count();
        let retry_due = last_failed
            .as_ref()
            .is_some_and(|(_, _, _, failed_at, failures)| {
                failed_at.elapsed() >= clipboard_retry_delay(*failures)
            });
        if clipboard_poll_unchanged(version, &last_read, &target, retry_due) {
            continue;
        }
        let Some(content) = clipboard::read_snapshot() else {
            continue;
        };
        // Only cache a stable read. A copy racing the read must be polled again.
        last_read = version
            .filter(|version| Some(*version) == clipboard::change_count())
            .map(|version| (version, target.device_id.clone(), target.addr.clone()));
        let signature = content.signature();
        let send_key = content.send_key(version);
        let is_files = matches!(&content, clipboard::ClipboardSnapshot::Files(_));

        // If this is the content we just wrote after receiving a peer packet,
        // suppress it. A different signature during the grace window is treated
        // as a fresh local copy so quick copy/screenshot + paste stays current.
        if clipboard_echo_active(&clipboard_echo_until) {
            let is_known_echo = clipboard_seen_text
                .lock()
                .map(|seen| seen.as_deref() == Some(signature.as_str()))
                .unwrap_or(false);
            if is_known_echo {
                last_failed = None;
                continue;
            }
            if let Ok(mut seen) = clipboard_seen_text.lock() {
                *seen = None;
            }
        }

        if matches!(&content, clipboard::ClipboardSnapshot::Content(content) if content.is_oversized())
            || (is_files && !file_transfer_enabled)
        {
            last_failed = None;
            continue;
        }

        if last_sent
            .as_ref()
            .map(|(device_id, addr, previous)| {
                device_id == &target.device_id && addr == &target.addr && previous == &send_key
            })
            .unwrap_or(false)
        {
            last_failed = None;
            continue;
        }
        if last_failed
            .as_ref()
            .map(|(device_id, addr, previous, failed_at, failures)| {
                device_id == &target.device_id
                    && addr == &target.addr
                    && previous == &send_key
                    && failed_at.elapsed() < clipboard_retry_delay(*failures)
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
            last_failed = None;
            last_sent = Some((target.device_id.clone(), target.addr.clone(), send_key));
            continue;
        }

        let send_result = match content {
            clipboard::ClipboardSnapshot::Files(paths) => {
                let Some(layout) = layout_state.lock().ok().map(|layout| layout.clone()) else {
                    break;
                };
                let name = layout
                    .devices
                    .iter()
                    .find(|device| device.id == target.device_id)
                    .map(|device| device.name.clone())
                    .or_else(|| {
                        layout
                            .paired_controllers
                            .iter()
                            .find(|peer| peer.id == target.device_id)
                            .map(|peer| peer.name.clone())
                    })
                    .unwrap_or_else(|| target.device_id.clone());
                let destination = FileTransferTarget {
                    device_id: target.device_id.clone(),
                    name,
                    addr: target.addr.clone(),
                    transport_public_key: target.transport_public_key.clone(),
                    protocol_version: target.protocol_version,
                    cluster_id: target.cluster_id.clone(),
                    pair_secret: target.pair_secret.clone(),
                };
                let keep_going = || {
                    file_clipboard_sync_current(
                        &layout_state,
                        &stop,
                        version,
                        clipboard::change_count(),
                    )
                };
                send_clipboard_files(
                    &quic_transport,
                    &local_peer_id,
                    &destination,
                    &paths,
                    false,
                    String::new(),
                    Some(&app),
                    Some(&keep_going),
                )
            }
            clipboard::ClipboardSnapshot::Content(content) => {
                let packet = clipboard_packet_from_content(
                    content,
                    local_peer_id.clone(),
                    quic_transport.public_key().into(),
                    target.device_id.clone(),
                    target.cluster_id.clone(),
                    target.pair_secret.clone(),
                    next_clipboard_sequence(),
                );
                encode_wire_packet(&packet).and_then(|payload| {
                    let peer = quic_transport.peer(
                        target.addr.clone(),
                        target.transport_public_key.clone(),
                        target.protocol_version,
                    );
                    quic_transport
                        .send_stream_expect_ack(peer, payload)
                        .map(|_| 1)
                })
            }
        };
        // Retries of this same content (the peer is still down).
        let failures = last_failed
            .as_ref()
            .filter(|(device_id, addr, previous, _, _)| {
                device_id == &target.device_id && addr == &target.addr && previous == &send_key
            })
            .map_or(0, |(_, _, _, _, failures)| *failures);
        if let Ok(packet_count) = send_result {
            transport_packets.fetch_add(packet_count, Ordering::Relaxed);
            clipboard_packets.fetch_add(1, Ordering::Relaxed);
            if failures > 0 {
                log::info!("clipboard send recovered after {failures} failed attempt(s)");
            }
            last_failed = None;
            last_sent = Some((target.device_id, target.addr, send_key));
        } else {
            let error = send_result.err().unwrap_or_default();
            if error == CLIPBOARD_FILES_SUPERSEDED || error == "传输已取消。" {
                last_failed = None;
                last_sent = Some((target.device_id, target.addr, send_key));
                continue;
            }
            // One warning per content; a peer that stays down filled the
            // log every 2 s (1.7k lines in an afternoon, rotating out
            // everything useful).
            if failures == 0 {
                log::warn!("clipboard send failed: {error}");
            } else {
                log::debug!("clipboard send failed again ({failures}): {error}");
            }
            if error.contains(quic_transport::STREAM_REJECTED) {
                // The receiver refused it (clipboard sync off, unpaired, too
                // old) and will refuse the same content again; wait for the
                // next copy instead of resending every retry interval.
                last_failed = None;
                last_sent = Some((target.device_id, target.addr, send_key));
            } else {
                last_failed = Some((
                    target.device_id.clone(),
                    target.addr.clone(),
                    send_key,
                    Instant::now(),
                    failures.saturating_add(1),
                ));
            }
        }
    }
}

fn file_clipboard_sync_current(
    layout: &Arc<Mutex<LayoutState>>,
    stop: &AtomicBool,
    expected_version: Option<u64>,
    current_version: Option<u64>,
) -> bool {
    !stop.load(Ordering::Relaxed)
        && expected_version == current_version
        && layout
            .lock()
            .map(|layout| layout.clipboard_sync && layout.file_transfer_enabled)
            .unwrap_or(false)
}

fn clipboard_poll_unchanged(
    version: Option<u64>,
    previous: &Option<(u64, String, String)>,
    target: &input::ClipboardTarget,
    retry_due: bool,
) -> bool {
    !retry_due
        && version.is_some_and(|version| {
            previous
                .as_ref()
                .is_some_and(|(last_version, device_id, addr)| {
                    version == *last_version
                        && device_id == &target.device_id
                        && addr == &target.addr
                })
        })
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
    retry_clipboard_write(attempts, retry_delay, || write_content(content))
}

fn retry_clipboard_write<F>(
    attempts: usize,
    retry_delay: Duration,
    mut write: F,
) -> Result<(), String>
where
    F: FnMut() -> Result<(), String>,
{
    let attempts = attempts.max(1);
    let mut last_error = None;
    for attempt in 0..attempts {
        match write() {
            Ok(()) => return Ok(()),
            Err(error) => last_error = Some(error),
        }
        if attempt + 1 < attempts && !retry_delay.is_zero() {
            thread::sleep(retry_delay * (attempt as u32 + 1));
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
    let accepted = handle_clipboard_packet_with_writer(
        payload,
        layout,
        local_peer_id,
        clipboard_seen_text,
        clipboard_echo_until,
        clipboard_last_sequences,
        write_clipboard_content_with_retry,
    );
    if accepted {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Intent { #[serde(default)] paste: bool, #[serde(default)] paste_token: String,
            #[serde(default)] origin_transport_public_key: String, origin_id: String }
        if let Some(intent) = decode_wire_packet::<Intent>(payload) {
            if intent.origin_id != local_peer_id && paste_permitted(layout, intent.paste, &intent.paste_token, &intent.origin_transport_public_key) {
                input::paste_received_clipboard();
            }
        }
    }
    accepted
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

    if layout.clipboard_on_demand && !packet.paste { return false; }
    if packet.paste && layout.machine_role != "client"
        && !clipboard_exchange::paste_expected(&packet.paste_token, &packet.origin_transport_public_key) { return false; }

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
    let html = packet.formats.iter().find(|f| f.kind == "htmlFragment").map(|f| f.text.clone());
    let rtf = packet.formats.iter().find(|f| f.kind == "rtfBytes").and_then(|f| {
        use base64::Engine as _; base64::engine::general_purpose::STANDARD.decode(&f.text).ok()
    });
    if html.is_some() || rtf.is_some() {
        let text = packet.formats.iter().find(|f| f.kind == "plainText").map(|f| f.text.clone()).unwrap_or_else(|| packet.text.clone());
        return Some(ClipboardContent::Rich(rich_clipboard::RichText { text, html, rtf }));
    }
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
        // Mirror input-packet authorization (packet_authorized_fields): match on
        // the STABLE transport public key first, then the id, then the legacy
        // "local-device" fallback. Matching by id alone silently rejected a
        // controller whose derived peer id had drifted (LAN IP change) even
        // though its key was unchanged — which is why input kept working while
        // clipboard from that controller stopped.
        let key = packet.origin_transport_public_key.trim();
        return layout.paired_controllers.iter().any(|controller| {
            (!key.is_empty() && controller.transport_public_key == key)
                || controller.id == packet.origin_id
        }) || (layout.paired_controllers.len() == 1
            && packet.origin_id == "local-device"
            && !key.is_empty());
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

    // A client's only target is its controller: look there first. A device
    // entry with the same id can be left over from when this machine was the
    // server; it says "not input-ready" (a controller does not take input),
    // and answering from it failed every transfer — the drag hand-off and the
    // log — with "offline".
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
                // The controller's id as it announces itself now, not as stored
                // at pairing: it is host + IP, so it moves with the IP (a TUN
                // proxy flipped it), and the receiver matches target_id exactly.
                device_id: peer.id.clone(),
                name: controller.name.clone(),
                addr: format!("{}:{}", host, peer.quic_port),
                transport_public_key: peer.transport_public_key.clone(),
                protocol_version: peer.protocol_version,
                cluster_id: layout.cluster_id.clone(),
                pair_secret: layout.pair_secret.clone(),
            });
        }
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
        if metadata.is_dir() {
            let bundle = Arc::new(folder_transfer::bundle(&path)?);
            files.push(TransferFile { path: bundle.path().to_path_buf(), name: transfer_file_name(&path)?,
                total_bytes: bundle.size().map_err(|e| e.to_string())?, directory: false, bundle: Some(bundle) });
            continue;
        }
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
            directory: false,
            bundle: None,
        });
    }

    if files.is_empty() {
        Err("请选择要传输的文件。".into())
    } else {
    Ok(files)
    }
}

#[cfg(target_os = "macos")]
fn collect_native_drag_files(paths: &[String]) -> Result<Vec<TransferFile>, String> {
    let mut files = Vec::new();
    for raw in paths {
        let root = Path::new(raw);
        let name = transfer_file_name(root)?;
        if root.is_dir() {
            for (relative, directory, size) in folder_transfer::walk(root)? {
                let virtual_name = Path::new(&name).join(&relative).to_string_lossy().replace('\\', "/");
                if virtual_name.encode_utf16().count() >= 260 { return Err(format!("{DRAG_CONTROL_FAILED}: 文件夹路径过长，请使用普通文件传输。")); }
                files.push(TransferFile { path: root.join(relative), name: virtual_name, total_bytes: size, directory, bundle: None });
            }
        } else { files.extend(collect_transfer_files(&[raw.clone()])?); }
        if files.len() > folder_transfer::MAX_ENTRIES { return Err("拖放条目过多。".into()); }
    }
    if files.is_empty() { return Err("请选择要传输的文件。".into()); }
    Ok(files)
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

#[allow(clippy::too_many_arguments)]
fn send_transfer_file(
    quic_transport: &quic_transport::TransportHandle,
    origin_id: &str,
    target: &FileTransferTarget,
    file: &TransferFile,
    transfer_id: &str,
    drop_mode: DropMode,
    reporter: Option<&FileTransferProgressReporter>,
    keep_going: Option<&dyn Fn() -> bool>,
) -> Result<u64, String> {
    ensure_file_transfer_current(keep_going)?;
    if file.directory {
        return Ok(0);
    }
    if file.bundle.is_some() {
        let probe = file_transfer_packet(
            "directory-probe",
            "",
            origin_id,
            target,
            &file.name,
            0,
            0,
            0,
            Vec::new(),
            drop_mode,
            true,
        );
        send_file_transfer_packet(quic_transport, target, probe)
            .map_err(|_| "对端版本不支持文件夹，请先更新两端 MyKVM。".to_string())?;
    }
    if let Some(reporter) = reporter {
        reporter.emit(transfer_id, file, 0, false, None);
    }

    let job = transfer_jobs::TransferJob::new(transfer_id);
    let outcome = send_transfer_file_bytes(
        quic_transport,
        origin_id,
        target,
        file,
        drop_mode,
        transfer_id,
        reporter,
        &job,
        keep_going,
    );
    if outcome.is_err() {
        let abort = file_transfer_packet(
            "abort",
            transfer_id,
            origin_id,
            target,
            &file.name,
            file.total_bytes,
            0,
            0,
            Vec::new(),
            drop_mode,
            file.bundle.is_some(),
        );
        let _ = send_file_transfer_packet(quic_transport, target, abort);
    }

    if let Some(reporter) = reporter {
        match &outcome {
            Ok(_) => reporter.emit(transfer_id, file, file.total_bytes, true, None),
            Err(error) => reporter.emit(transfer_id, file, 0, true, Some(error.clone())),
        }
    }

    outcome
}

fn new_transfer_id(prefix: &str) -> String {
    format!("{prefix}-{}-{}", now_ms(), random_hex(8))
}

#[allow(clippy::too_many_arguments)]
fn send_transfer_file_bytes(
    quic_transport: &quic_transport::TransportHandle,
    origin_id: &str,
    target: &FileTransferTarget,
    file: &TransferFile,
    drop_mode: DropMode,
    transfer_id: &str,
    reporter: Option<&FileTransferProgressReporter>,
    job: &transfer_jobs::TransferJob,
    keep_going: Option<&dyn Fn() -> bool>,
) -> Result<u64, String> {
    ensure_file_transfer_current(keep_going)?;
    let mut packet_count = 0_u64;

    send_file_transfer_packet(
        quic_transport,
        target,
        file_transfer_packet(
            "start",
            transfer_id,
            origin_id,
            target,
            &file.name,
            file.total_bytes,
            0,
            0,
            Vec::new(),
            drop_mode,
            file.bundle.is_some(),
        ),
    )?;
    packet_count += 1;

    let mut file_handle = fs::File::open(&file.path)
        .map_err(|error| format!("无法打开文件 {}: {error}", file.path.display()))?;
    let mut buffer = vec![0_u8; FILE_TRANSFER_CHUNK_BYTES];
    let mut offset = 0_u64;
    let mut chunk_index = 0_u64;
    let mut last_progress = Instant::now();
    loop {
        if job.cancelled() {
            return Err("传输已取消。".into());
        }
        ensure_file_transfer_current(keep_going)?;
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
                transfer_id,
                origin_id,
                target,
                &file.name,
                file.total_bytes,
                chunk_index,
                offset,
                data,
                drop_mode,
                file.bundle.is_some(),
            ),
        )?;
        packet_count += 1;
        offset = offset.saturating_add(read as u64);
        chunk_index = chunk_index.saturating_add(1);
        if let Some(reporter) = reporter {
            if last_progress.elapsed() >= Duration::from_millis(FILE_TRANSFER_PROGRESS_INTERVAL_MS)
            {
                reporter.emit(transfer_id, file, offset, false, None);
                last_progress = Instant::now();
            }
        }
    }

    ensure_file_transfer_current(keep_going)?;
    send_file_transfer_packet(
        quic_transport,
        target,
        file_transfer_packet(
            "finish",
            transfer_id,
            origin_id,
            target,
            &file.name,
            file.total_bytes,
            chunk_index,
            offset,
            Vec::new(),
            drop_mode,
            file.bundle.is_some(),
        ),
    )?;
    packet_count += 1;

    Ok(packet_count)
}

#[tauri::command]
fn cancel_file_transfer(transfer_id: String) -> bool { transfer_jobs::cancel(&transfer_id) }

fn abort_incoming_file_transfer(packet: FileTransferPacket, transfers: &Arc<Mutex<HashMap<String, IncomingFileTransfer>>>) -> bool {
    let Ok(mut transfers) = transfers.lock() else { return false; };
    let Some(transfer) = transfers.get(&packet.transfer_id) else { return true; };
    if transfer.origin_id != packet.origin_id || transfer.target_id != packet.target_id { return false; }
    if let Some(transfer) = transfers.remove(&packet.transfer_id) { let _ = fs::remove_file(transfer.temp_path); }
    true
}

fn finalize_received_transfer(temp: &Path, final_path: &mut PathBuf, directory: bool) -> std::io::Result<()> {
    let parent = final_path.parent().ok_or_else(|| std::io::Error::other("missing destination directory"))?.to_path_buf();
    let name = final_path.file_name().ok_or_else(|| std::io::Error::other("missing file name"))?.to_string_lossy().into_owned();
    loop {
        if directory {
            match fs::create_dir(&*final_path) {
                Ok(()) => {
                    if let Err(error) = folder_transfer::extract(temp, final_path) {
                        let _ = fs::remove_dir_all(&*final_path);
                        return Err(std::io::Error::other(error));
                    }
                    fs::remove_file(temp)?;
                    return Ok(());
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        } else {
            match fs::hard_link(temp, &*final_path) {
                Ok(()) => { fs::remove_file(temp)?; return Ok(()); }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    *final_path = unique_transfer_destination(&parent, &name);
                    continue;
                }
                Err(_) => {}
            }
            match fs::OpenOptions::new().write(true).create_new(true).open(&*final_path) {
                Ok(mut output) => {
                    let copy = fs::File::open(temp).and_then(|mut input| std::io::copy(&mut input, &mut output));
                    drop(output);
                    if let Err(error) = copy { let _ = fs::remove_file(&*final_path); return Err(error); }
                    fs::remove_file(temp)?;
                    return Ok(());
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        *final_path = unique_transfer_destination(&parent, &name);
    }
}

// Where a transfer should land on the receiver.
#[derive(Clone, Copy, PartialEq)]
enum DropMode {
    /// The MyKVM Transfers folder (the manual "send files" button).
    TransfersFolder,
    /// Straight onto the Desktop (edge drag-drop onto a non-Windows machine).
    Desktop,
    /// ShareMouse-style: stage, then drop into the folder under the cursor when
    /// the drag is released (else Desktop).
    DragDrop,
    /// A device's log fetched on request; lands in the "MyKVM Remote Logs" folder.
    ClientLog,
    Clipboard,
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
    data: Vec<u8>,
    drop_mode: DropMode,
    directory_bundle: bool,
) -> FileTransferPacket {
    FileTransferPacket {
        protocol: if drop_mode == DropMode::Clipboard { CLIPBOARD_FILES_PROTOCOL } else { FILE_TRANSFER_PROTOCOL }.into(),
        kind: kind.into(),
        transfer_id: transfer_id.into(),
        origin_id: origin_id.into(),
        target_id: target.device_id.clone(),
        cluster_id: target.cluster_id.clone(),
        pair_secret: target.pair_secret.clone(),
        file_name: file_name.into(),
        total_bytes,
        chunk_index,
        offset,
        data,
        drop_to_desktop: drop_mode == DropMode::Desktop,
        drag_drop: drop_mode == DropMode::DragDrop,
        client_log: drop_mode == DropMode::ClientLog,
        directory_bundle,
        clipboard_transfer: drop_mode == DropMode::Clipboard,
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

// Control channel for ShareMouse-style native drag-drop onto a Windows client.
// The file bytes still travel as ordinary FileTransferPackets (matched to the
// drag by transfer_id); these messages open/close the client's OLE session.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DragControlPacket {
    protocol: String,
    kind: String, // "start" | "drop" | "cancel"
    origin_id: String,
    target_id: String,
    cluster_id: String,
    pair_secret: String,
    #[serde(default)]
    files: Vec<DragControlFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DragControlFile {
    transfer_id: String,
    name: String,
    size: u64,
    #[serde(default)]
    directory: bool,
}

/// Windows target: what a sent drag start still has to stream (edge drag-drop
/// path, macOS controller only): each file under the transfer
/// id announced in the start, so the peer feeds it to its drag session.
#[cfg(target_os = "macos")]
struct OleDragStream {
    quic_transport: quic_transport::TransportHandle,
    origin_id: String,
    target: FileTransferTarget,
    files: Vec<(String, TransferFile)>,
}

/// Announce a native drag to `device_id` (acknowledged before returning), so
/// that a drop or cancel sent afterwards can never overtake it.
#[cfg(target_os = "macos")]
fn send_ole_drag_start(
    state: &AppRuntime,
    device_id: &str,
    paths: &[String],
) -> Result<OleDragStream, String> {
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
    let files = collect_native_drag_files(paths)?;
    let tree = files.iter().any(|file| file.directory);

    // Pre-assign a transfer id per file so the client can match the streamed
    // bytes to the drag session it is about to open.
    let with_ids: Vec<(String, TransferFile)> = files
        .into_iter()
        .map(|file| (new_transfer_id("drag"), file))
        .collect();

    let manifest = with_ids
        .iter()
        .map(|(id, file)| DragControlFile {
            transfer_id: id.clone(),
            name: file.name.clone(),
            size: file.total_bytes,
            directory: file.directory,
        })
        .collect();
    let start = DragControlPacket {
        protocol: if tree { DRAG_TREE_PROTOCOL } else { DRAG_CONTROL_PROTOCOL }.into(),
        kind: "start".into(),
        origin_id: local_peer.id.clone(),
        target_id: target.device_id.clone(),
        cluster_id: target.cluster_id.clone(),
        pair_secret: target.pair_secret.clone(),
        files: manifest,
    };
    let payload = encode_wire_packet(&start)?;
    let peer = quic_transport.peer(
        target.addr.clone(),
        target.transport_public_key.clone(),
        target.protocol_version,
    );
    quic_transport
        .send_stream_expect_ack(peer, payload)
        .map_err(|error| format!("{DRAG_CONTROL_FAILED}: {error}"))?;

    Ok(OleDragStream {
        quic_transport,
        origin_id: local_peer.id,
        target,
        files: with_ids,
    })
}

#[cfg(target_os = "macos")]
fn stream_ole_drag_files(state: &AppRuntime, stream: OleDragStream) -> Result<usize, String> {
    let OleDragStream {
        quic_transport,
        origin_id,
        target,
        files: with_ids,
    } = stream;
    let total = with_ids.len();
    for (index, (transfer_id, file)) in with_ids.iter().enumerate() {
        let reporter = FileTransferProgressReporter {
            app: &state.app_handle,
            target_name: &target.name,
            file_index: index + 1,
            file_count: total,
        };
        let packet_count = send_transfer_file(
            &quic_transport,
            &origin_id,
            &target,
            file,
            transfer_id,
            DropMode::TransfersFolder,
            Some(&reporter),
            None,
        )?;
        state
            .transport_packets
            .fetch_add(packet_count, Ordering::Relaxed);
    }
    Ok(total)
}

/// Windows target: signal drop or cancel for an in-flight OLE drag session.
#[cfg(target_os = "macos")]
fn send_ole_drag_signal(state: &AppRuntime, device_id: &str, kind: &str) -> Result<(), String> {
    let layout = state.layout_snapshot();
    let mut local_peer = local_peer_from_layout(&layout);
    let quic_transport = state
        .quic_transport_handle()
        .ok_or_else(|| "QUIC transport is not ready.".to_string())?;
    apply_transport_to_peer(&mut local_peer, &quic_transport);
    let peers = active_peer_snapshot(&state.peers);
    let target = file_transfer_target_for_device(&layout, &peers, device_id)?;
    let packet = DragControlPacket {
        protocol: DRAG_CONTROL_PROTOCOL.into(),
        kind: kind.into(),
        origin_id: local_peer.id.clone(),
        target_id: target.device_id.clone(),
        cluster_id: target.cluster_id.clone(),
        pair_secret: target.pair_secret.clone(),
        files: Vec::new(),
    };
    let payload = encode_wire_packet(&packet)?;
    let peer = quic_transport.peer(
        target.addr.clone(),
        target.transport_public_key.clone(),
        target.protocol_version,
    );
    quic_transport
        .send_stream_expect_ack(peer, payload)
        .map_err(|error| format!("{DRAG_CONTROL_FAILED}: {error}"))
}

/// Controller (either platform): ask the controlled machine to hand its
/// in-flight file drag back to us. Sent when the cursor crosses back to the
/// controller mid-drag. Fire-and-forget on a thread so the input hot path never
/// blocks on the round-trip.
#[cfg(any(target_os = "windows", target_os = "macos"))]
pub(crate) fn send_drag_pull(
    quic_transport: &quic_transport::TransportHandle,
    origin_id: String,
    target_device_id: String,
    target_addr: String,
    target_pubkey: String,
    target_version: u16,
    cluster_id: String,
    pair_secret: String,
) {
    if target_pubkey.trim().is_empty()
        || target_addr.trim().is_empty()
        || cluster_id.trim().is_empty()
        || pair_secret.trim().is_empty()
    {
        return;
    }
    let quic_transport = quic_transport.clone();
    thread::spawn(move || {
        let label = target_device_id.clone();
        let packet = DragControlPacket {
            protocol: DRAG_CONTROL_PROTOCOL.into(),
            kind: "pull".into(),
            origin_id,
            target_id: target_device_id,
            cluster_id,
            pair_secret,
            files: Vec::new(),
        };
        let Ok(payload) = encode_wire_packet(&packet) else {
            return;
        };
        let peer = quic_transport.peer(target_addr, target_pubkey, target_version);
        match quic_transport.send_stream_expect_ack(peer, payload) {
            Ok(_) => log::info!("drag pull sent to {label}"),
            Err(error) => log::warn!("drag pull send failed: {error}"),
        }
    });
}

/// Client side: apply an incoming drag-control message. Returns true if the
/// payload was a drag-control packet addressed to us (whether or not this
/// platform can act on it).
fn handle_drag_control_packet(
    payload: &[u8],
    layout: &LayoutState,
    local_peer_id: &str,
) -> bool {
    let Some(packet) = decode_wire_packet::<DragControlPacket>(payload) else {
        return false;
    };
    if packet.protocol != DRAG_CONTROL_PROTOCOL && packet.protocol != DRAG_TREE_PROTOCOL {
        return false;
    }
    if !layout.file_transfer_enabled {
        return false;
    }
    // Reuse the file-transfer trust checks: same cluster, same pair secret, and
    // (on a client) an origin we are paired to.
    if layout.cluster_id.trim().is_empty()
        || layout.pair_secret.trim().is_empty()
        || packet.cluster_id != layout.cluster_id
        || packet.pair_secret != layout.pair_secret
    {
        return false;
    }
    if layout.machine_role == "client"
        && !layout.paired_controllers.is_empty()
        && !layout
            .paired_controllers
            .iter()
            .any(|controller| controller.id == packet.origin_id)
    {
        return false;
    }
    if packet.target_id != local_peer_id || packet.origin_id == local_peer_id {
        return false;
    }

    #[cfg(target_os = "windows")]
    {
        match packet.kind.as_str() {
            "start" => {
                let files = packet
                    .files
                    .into_iter()
                    .map(|file| windows_drag::DragFileMeta {
                        transfer_id: file.transfer_id,
                        name: file.name,
                        size: file.size,
                        directory: file.directory,
                    })
                    .collect();
                let mode = native_drag::NativeDragMode::for_role(&layout.machine_role);
                if windows_drag::start_drag_session(files, mode) {
                    log::info!("native drag session started from {}", packet.origin_id);
                } else { return false; }
            }
            "drop" => windows_drag::signal_drop(),
            "cancel" => windows_drag::cancel_session(),
            // The controller crossed back to itself mid-drag and wants the file
            // drag we are holding; the catcher reads it and transfers it up.
            "pull" => windows_drop_catcher::handoff_to_controller(&packet.origin_id),
            _ => {}
        }
    }
    #[cfg(target_os = "macos")]
    {
        // A Windows controller crossed back to itself mid-drag and asked us to
        // hand our local file drag over as a native OLE drag on its side.
        if packet.kind == "pull" {
            crate::input::receiver_handoff_drag(packet.origin_id.clone());
        }
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        // Only Windows runs OLE drag sessions and only macOS hands one off;
        // elsewhere the controller uses the transfer fallback.
        let _ = packet;
    }
    true
}

/// ShareMouse-style drag placement (macOS receiver). Files transferred during a
/// Windows→Mac drag are staged here; when the drag is released the input layer
/// reports the folder under the cursor and the staged files move into it.
#[cfg(target_os = "macos")]
mod drag_place {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};

    struct State {
        active: bool,
        staged: Vec<PathBuf>,
        target: Option<PathBuf>,
        target_at: Option<Instant>,
    }

    fn state() -> &'static Mutex<State> {
        static STATE: OnceLock<Mutex<State>> = OnceLock::new();
        STATE.get_or_init(|| {
            Mutex::new(State {
                active: false,
                staged: Vec::new(),
                target: None,
                target_at: None,
            })
        })
    }

    fn fresh(at: Option<Instant>) -> bool {
        at.map(|t| t.elapsed() < Duration::from_secs(3)).unwrap_or(false)
    }

    fn desktop_dir() -> PathBuf {
        std::env::var_os("HOME")
            .map(|home| Path::new(&home).join("Desktop"))
            .unwrap_or_else(|| PathBuf::from("."))
    }

    /// A Windows→Mac drag started delivering files. `file_name` seeds the drag
    /// overlay icon with that file type.
    pub fn begin(file_name: &str) {
        let first = {
            let Ok(mut state) = state().lock() else {
                return;
            };
            let first = !state.active;
            if first {
                state.staged.clear();
                state.target = None;
                state.target_at = None;
            }
            state.active = true;
            first
        };
        if first {
            let extension = Path::new(file_name)
                .extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| ext.to_string());
            crate::input::drag_overlay_show(extension);
        }
    }

    /// A file finished transferring into the staging dir.
    pub fn stage(path: PathBuf) {
        let ready_target = {
            let Ok(mut state) = state().lock() else {
                return;
            };
            match state.target.clone() {
                // Released already: drop it straight in.
                Some(target) if fresh(state.target_at) => Some(target),
                _ => {
                    state.staged.push(path.clone());
                    None
                }
            }
        };
        if let Some(target) = ready_target {
            place(&path, &target);
        }
    }

    /// The drag was released over the Mac; `folder` is the folder under the
    /// cursor (None = Desktop). Places everything staged so far.
    pub fn release(folder: Option<PathBuf>) {
        let target = folder.unwrap_or_else(desktop_dir);
        let staged = {
            let Ok(mut state) = state().lock() else {
                return;
            };
            state.target = Some(target.clone());
            state.target_at = Some(Instant::now());
            state.active = false;
            std::mem::take(&mut state.staged)
        };
        crate::input::drag_overlay_hide();
        log::info!("drag drop released over {}", target.display());
        for path in staged {
            place(&path, &target);
        }
    }

    /// Whether a pulled drag is genuinely still in the user's hand, i.e. it can
    /// be relayed onward to another machine. Stricter than `is_active`, whose
    /// grace window stays true for seconds AFTER a release so late-arriving
    /// files still get placed — crossing within that window must not be
    /// mistaken for a relay.
    pub fn is_relaying() -> bool {
        state().lock().map(|state| state.active).unwrap_or(false)
    }

    /// Client→client relay: take the files that finished streaming in and end
    /// the local placement — they belong to the next machine now.
    ///
    /// ponytail: files still in flight when the drag is released on the far
    /// machine are left in the staging dir and cleared by the next `begin`.
    /// Carrying them over would mean tracking the inbound transfer's file count,
    /// which the drag-control packet does not carry for this direction.
    pub fn take_staged_for_relay() -> Vec<PathBuf> {
        let staged = {
            let Ok(mut state) = state().lock() else {
                return Vec::new();
            };
            state.active = false;
            state.target = None;
            state.target_at = None;
            std::mem::take(&mut state.staged)
        };
        crate::input::drag_overlay_hide();
        staged
    }

    /// Whether a drag is in flight (so the release hook bothers to resolve the
    /// folder under the cursor). Cheap — checked on every injected button-up.
    pub fn is_active() -> bool {
        state()
            .lock()
            .map(|state| state.active || fresh(state.target_at))
            .unwrap_or(false)
    }

    fn place(staged: &Path, folder: &Path) {
        let Some(name) = staged.file_name() else {
            return;
        };
        let _ = fs::create_dir_all(folder);
        let mut dest = folder.join(name);
        let mut index = 1;
        while dest.exists() {
            let stem = staged.file_stem().and_then(|s| s.to_str()).unwrap_or("file");
            dest = folder.join(match staged.extension().and_then(|e| e.to_str()) {
                Some(ext) => format!("{stem} ({index}).{ext}"),
                None => format!("{stem} ({index})"),
            });
            index += 1;
        }
        let moved = if staged.is_dir() {
            crate::folder_transfer::copy_directory(staged, &dest).and_then(|_| fs::remove_dir_all(staged))
        } else { super::finalize_received_transfer(staged, &mut dest, false) };
        match moved {
            Ok(()) => log::info!("drag drop placed {} -> {}", name.to_string_lossy(), dest.display()),
            Err(error) => log::warn!("drag drop placement failed: {error}"),
        }
    }
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
    // Where the file finalizes:
    //  - drag_drop → a hidden staging dir; the real placement (into the folder
    //    under the cursor, or Desktop) happens when the drag is released.
    //  - drop_to_desktop → straight onto the Desktop.
    //  - otherwise → the transfers folder (None here).
    let drop_root = if packet.client_log {
        client_log_dir(app).ok()
    } else if packet.drag_drop {
        Some(receive_root.join(".mykvm-drag-staging"))
    } else if packet.drop_to_desktop {
        app.path().desktop_dir().ok()
    } else {
        None
    };
    handle_decoded_file_transfer_packet(
        packet,
        layout,
        local_peer_id,
        transfers,
        &receive_root,
        drop_root.as_deref(),
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

/// Where logs fetched from other devices land.
fn client_log_dir(app: &AppHandle) -> Result<PathBuf, String> {
    if let Ok(downloads) = app.path().download_dir() {
        return Ok(downloads.join("MyKVM Remote Logs"));
    }
    app.path()
        .app_data_dir()
        .map(|directory| directory.join("MyKVM Remote Logs"))
        .map_err(|error| format!("failed to resolve client log directory: {error}"))
}

/// Read at most the last `max_bytes` bytes of `path`.
fn tail_of_file(path: &Path, max_bytes: u64) -> std::io::Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = fs::File::open(path)?;
    let len = file.metadata()?.len();
    if len > max_bytes {
        file.seek(SeekFrom::Start(len - max_bytes))?;
    }
    let mut tail = Vec::new();
    file.read_to_end(&mut tail)?;
    Ok(tail)
}

/// Copy the tail of this machine's newest log file into a temp file named after
/// this device, ready to stream back to whoever asked. Returns the temp path.
fn write_own_log_tail(app: &AppHandle, device_name: &str) -> Result<PathBuf, String> {
    let log_dir = app
        .path()
        .app_log_dir()
        .map_err(|error| format!("failed to resolve log dir: {error}"))?;
    // The plugin keeps the live log plus rotated copies; the newest by mtime is
    // the one currently being written.
    let newest = fs::read_dir(&log_dir)
        .map_err(|error| format!("failed to read log dir: {error}"))?
        .flatten()
        .filter(|entry| {
            entry
                .path()
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("log"))
        })
        .max_by_key(|entry| {
            entry
                .metadata()
                .and_then(|meta| meta.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
        })
        .map(|entry| entry.path())
        .ok_or_else(|| "no log file found".to_string())?;

    let tail = tail_of_file(&newest, CLIENT_LOG_TAIL_BYTES)
        .map_err(|error| format!("failed to read log: {error}"))?;
    let header = format!("MyKVM diagnostics: pid={} version={} elevated={} {}\n", std::process::id(),
        app.config().version.as_deref().unwrap_or(env!("CARGO_PKG_VERSION")),
        current_privilege_status().is_elevated, input::diagnostic_summary());
    let tail = [header.as_bytes(), tail.as_slice()].concat();

    let safe_name = device_name
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>();
    let file_name = format!("{}-{}.log", safe_name.trim_matches('-'), now_ms());
    let temp_path = std::env::temp_dir().join(file_name);
    fs::write(&temp_path, &tail).map_err(|error| format!("failed to stage log: {error}"))?;
    Ok(temp_path)
}

// Control channel: a paired peer asks this device for something — its recent
// log (sent back as an ordinary file transfer tagged `client_log`) or, from
// its controller, to update itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PeerRequestPacket {
    protocol: String,
    #[serde(default)]
    paste_token: String,
    origin_id: String,
    target_id: String,
    cluster_id: String,
    pair_secret: String,
}

/// What a paired peer may ask of this device.
#[derive(Debug, PartialEq)]
enum PeerRequest {
    Log,
    Update,
    Lock,
    Clipboard,
}

/// Whether `packet` may ask this device for anything, and what (`None` =
/// refuse). File-transfer trust (same cluster and pair secret) plus: a client
/// answers only a controller it is paired with — an unpaired client that kept
/// its old secret answers no one — only a client can be asked to update, and a
/// log needs file transfer, since it travels back as one.
fn authorize_peer_request(
    packet: &PeerRequestPacket,
    layout: &LayoutState,
    local_peer_id: &str,
) -> Option<PeerRequest> {
    let request = match packet.protocol.as_str() {
        LOG_REQUEST_PROTOCOL if layout.file_transfer_enabled => PeerRequest::Log,
        CLIPBOARD_PULL_PROTOCOL if layout.clipboard_sync => PeerRequest::Clipboard,
        REMOTE_UPDATE_PROTOCOL if layout.machine_role == "client" => PeerRequest::Update,
        LOCK_SCREEN_PROTOCOL if layout.machine_role == "client" => PeerRequest::Lock,
        _ => return None,
    };
    if layout.cluster_id.trim().is_empty()
        || layout.pair_secret.trim().is_empty()
        || packet.cluster_id != layout.cluster_id
        || packet.pair_secret != layout.pair_secret
    {
        return None;
    }
    if layout.machine_role == "client"
        && !layout
            .paired_controllers
            .iter()
            .any(|controller| controller.id == packet.origin_id)
    {
        return None;
    }
    if packet.target_id != local_peer_id || packet.origin_id == local_peer_id {
        return None;
    }
    Some(request)
}

/// Device side of a peer request: authorize, then do the work off the stream
/// handler (it must not block on a round-trip). Returns true if the packet was
/// a peer request.
fn handle_peer_request_packet(
    payload: &[u8],
    layout: &LayoutState,
    local_peer_id: &str,
    app: &AppHandle,
) -> bool {
    let Some(packet) = decode_wire_packet::<PeerRequestPacket>(payload) else {
        return false;
    };
    if !matches!(
        packet.protocol.as_str(),
        LOG_REQUEST_PROTOCOL | REMOTE_UPDATE_PROTOCOL | LOCK_SCREEN_PROTOCOL | CLIPBOARD_PULL_PROTOCOL
    ) {
        return false;
    }
    match authorize_peer_request(&packet, layout, local_peer_id) {
        None => return packet.protocol != CLIPBOARD_PULL_PROTOCOL,
        Some(PeerRequest::Update) => {
            log::info!("update requested by controller {}", packet.origin_id);
            run_requested_update(app.clone());
            return true;
        }
        Some(PeerRequest::Lock) => {
            log::info!("lock sync: controller {} locked its screen", packet.origin_id);
            lock_local_screen();
            return true;
        }
        Some(PeerRequest::Clipboard) => {
            let origin = packet.origin_id;
            let token = packet.paste_token;
            let app = app.clone();
            thread::spawn(move || {
                let state = app.state::<AppRuntime>();
                let layout = state.layout_snapshot();
                let peers = active_peer_snapshot(&state.peers);
                let result = file_transfer_target_for_device(&layout, &peers, &origin)
                    .and_then(|target| send_current_clipboard(state.inner(), &target, true, token));
                if let Err(error) = result { log::warn!("requested clipboard send failed: {error}"); }
            });
            return true;
        }
        Some(PeerRequest::Log) => {}
    }

    let device_name = local_peer_from_layout(layout).name;
    let app = app.clone();
    let origin_id = packet.origin_id;
    thread::spawn(move || {
        let temp_path = match write_own_log_tail(&app, &device_name) {
            Ok(path) => path,
            Err(error) => {
                log::warn!("log request: could not stage log: {error}");
                return;
            }
        };
        let state = app.state::<AppRuntime>();
        let path = temp_path.to_string_lossy().into_owned();
        match send_files_to_device_inner(state.inner(), &origin_id, &[path], DropMode::ClientLog) {
            Ok(summary) => log::info!(
                "log request: sent {} to {}",
                format_bytes(summary.byte_count),
                summary.target_name
            ),
            Err(error) => log::warn!("log request: send failed: {error}"),
        }
        let _ = fs::remove_file(&temp_path);
    });
    true
}

/// Check, download and install the release this app's own updater finds (so
/// it is verified against our updater key), handing the network over first.
/// On Windows `install` ends this process and the installer restarts it.
fn run_requested_update(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        use tauri_plugin_updater::UpdaterExt;
        let state = app.state::<AppRuntime>();
        let update = match app
            .updater_builder()
            .timeout(Duration::from_secs(20))
            .build()
            .map_err(|error| error.to_string())
        {
            Ok(updater) => updater.check().await.map_err(|error| error.to_string()),
            Err(error) => Err(error),
        };
        let update = match update {
            Ok(Some(update)) => update,
            Ok(None) => {
                log::info!("requested update: already on the latest release");
                return;
            }
            Err(error) => {
                log::warn!("requested update: check failed: {error}");
                return;
            }
        };
        log::info!("requested update: installing {}", update.version);
        state.upgrading.store(true, Ordering::Relaxed);
        let bytes = match update.download(|_, _| {}, || {}).await {
            Ok(bytes) => bytes,
            Err(error) => {
                log::warn!("requested update: download failed: {error}");
                state.upgrading.store(false, Ordering::Relaxed);
                return;
            }
        };
        state.hand_off_network();
        if let Err(error) = update.install(bytes) {
            log::warn!("requested update: install failed: {error}");
            state.upgrading.store(false, Ordering::Relaxed);
            let _ = start_runtime_inner(state.inner());
            return;
        }
        app.restart();
    });
}

#[cfg(test)]
fn handle_file_transfer_packet_with_root(
    payload: &[u8],
    layout: &LayoutState,
    local_peer_id: &str,
    transfers: &Arc<Mutex<HashMap<String, IncomingFileTransfer>>>,
    receive_root: &Path,
) -> bool {
    let Some(packet) = decode_wire_packet::<FileTransferPacket>(payload) else {
        return false;
    };
    handle_decoded_file_transfer_packet(packet, layout, local_peer_id, transfers, receive_root, None)
}

fn handle_decoded_file_transfer_packet(
    packet: FileTransferPacket,
    layout: &LayoutState,
    local_peer_id: &str,
    transfers: &Arc<Mutex<HashMap<String, IncomingFileTransfer>>>,
    receive_root: &Path,
    drop_root: Option<&Path>,
) -> bool {
    if packet.protocol != FILE_TRANSFER_PROTOCOL && packet.protocol != CLIPBOARD_FILES_PROTOCOL {
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
    if packet.protocol == CLIPBOARD_FILES_PROTOCOL && !layout.clipboard_sync { return false; }
    if packet.kind == "directory-probe" || packet.kind == "clipboard-probe" { return true; }

    // A file that belongs to an active native drag session feeds that session's
    // in-memory stream (read by the OLE drop target) instead of landing on disk.
    #[cfg(target_os = "windows")]
    if windows_drag::session_wants(&packet.transfer_id) {
        return match packet.kind.as_str() {
            "start" => true,
            "chunk" => windows_drag::feed_chunk(&packet.transfer_id, &packet.data),
            "finish" => windows_drag::finish_file(&packet.transfer_id),
            "abort" => windows_drag::abort_file(&packet.transfer_id),
            _ => false,
        };
    }

    match packet.kind.as_str() {
        "start" => start_incoming_file_transfer(packet, transfers, receive_root, drop_root),
        "chunk" => append_incoming_file_transfer_chunk(packet, transfers),
        "finish" => finish_incoming_file_transfer(packet, transfers),
        "abort" => abort_incoming_file_transfer(packet, transfers),
        _ => false,
    }
}

fn start_incoming_file_transfer(
    packet: FileTransferPacket,
    transfers: &Arc<Mutex<HashMap<String, IncomingFileTransfer>>>,
    receive_root: &Path,
    drop_root: Option<&Path>,
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

    // The finalize root overrides the transfers folder (Desktop, or the hidden
    // drag-staging dir). The .part staging file always stays in the transfers
    // folder so it never flashes at the final location.
    let final_root = drop_root.unwrap_or(receive_root);
    if drop_root.is_some() {
        let _ = fs::create_dir_all(final_root);
    }
    let final_path = unique_transfer_destination(final_root, &file_name);
    let temp_path = receive_root.join(format!(
        ".mykvm-{}-{}.part",
        sanitize_transfer_id(&packet.transfer_id),
        file_name
    ));

    if let Ok(mut transfers) = transfers.lock() {
        if transfers.len() >= 32 || transfers.values().filter(|transfer| transfer.origin_id == packet.origin_id).count() >= 8 { return false; }
        if let Some(previous) = transfers.remove(&packet.transfer_id) {
            let _ = fs::remove_file(previous.temp_path);
        }
    } else {
        return false;
    }

    if fs::File::create(&temp_path).is_err() {
        return false;
    }

    // A ShareMouse-style drag: hold the file for release-time placement.
    #[cfg(target_os = "macos")]
    if packet.drag_drop {
        drag_place::begin(&packet.file_name);
    }

    let transfer = IncomingFileTransfer {
        origin_id: packet.origin_id.clone(),
        target_id: packet.target_id.clone(),
        file_name,
        total_bytes: packet.total_bytes,
        received_bytes: 0,
        next_chunk_index: 0,
        directory_bundle: packet.directory_bundle,
        clipboard_transfer: packet.clipboard_transfer,
        touched_ms: now_ms(),
        temp_path,
        final_path,
        staged: packet.drag_drop,
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
        || packet.directory_bundle != transfer.directory_bundle
        || packet.clipboard_transfer != transfer.clipboard_transfer
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
    transfer.touched_ms = now_ms();
    true
}

fn finish_incoming_file_transfer(
    packet: FileTransferPacket,
    transfers: &Arc<Mutex<HashMap<String, IncomingFileTransfer>>>,
) -> bool {
    if !packet.data.is_empty() {
        return false;
    }
    let (temp_path, mut final_path, file_name, total_bytes, staged, directory_bundle) = {
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
            || packet.directory_bundle != transfer.directory_bundle
            || packet.clipboard_transfer != transfer.clipboard_transfer
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
            transfer.staged,
            transfer.directory_bundle,
        )
    };

    // The staging dir and the final dir can sit on different volumes (e.g. a
    // redirected Desktop), where rename fails — fall back to copy + delete.
    let finalize = finalize_received_transfer(&temp_path, &mut final_path, directory_bundle);
    match finalize {
        Ok(()) => {
            if packet.clipboard_transfer { clipboard_exchange::remember_file(&packet.origin_id, &packet.transfer_id, final_path.clone()); }
            if let Ok(mut transfers) = transfers.lock() {
                transfers.remove(&packet.transfer_id);
            }
            // ShareMouse-style drag: the file is complete in the staging dir;
            // hand it to the placer, which drops it into the folder under the
            // cursor when the drag is released (or right away if already).
            #[cfg(target_os = "macos")]
            if staged {
                drag_place::stage(final_path.clone());
                return true;
            }
            let _ = staged;
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

    refresh_paired_controller_addresses(layout, peers);
}

/// Discovery may refresh an address, but cannot replace a paired certificate.
/// A replacement identity must pass the existing pairing-code flow.
fn refresh_paired_controller_addresses(layout: &mut LayoutState, peers: &[LanPeer]) {
    if layout.paired_controllers.is_empty() {
        return;
    }

    for controller in &mut layout.paired_controllers {
        let Some(peer) = peers
            .iter()
            .find(|peer| paired_controller_identity_matches_peer(controller, peer))
        else {
            continue;
        };

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
    if !device.transport_public_key.trim().is_empty() {
        return device.transport_public_key == peer.transport_public_key;
    }
    device.id == peer_device_id(peer) || same_cluster_host(device, peer, layout_cluster_id)
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
    if device.source != "manual" {
        device.host = if peer.ip.trim().is_empty() {
            peer.host.clone()
        } else {
            peer.ip.clone()
        };
    }
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
        app_version: APP_VERSION
            .get()
            .cloned()
            .unwrap_or_else(|| env!("CARGO_PKG_VERSION").into()),
        last_seen_ms: now_ms(),
    }
}

/// The version peers see (and a controller compares to offer an update): the
/// app's own version, which a build's config can set without touching
/// Cargo.toml. Set once at startup.
static APP_VERSION: OnceLock<String> = OnceLock::new();

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
    if !controller.transport_public_key.trim().is_empty() {
        return controller.transport_public_key == peer.transport_public_key;
    }
    (!peer.id.trim().is_empty() && controller.id == peer.id)
        || controller.id == peer_device_id(peer)
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

/// What the client's pairing panel shows. A live challenge wins over "paired":
/// `begin_pairing_challenge` lets a known controller re-pair (rotated key, or
/// the two machines swapped roles), and answering "paired" with an empty code
/// popped the window up with no code to type on the server.
fn pairing_status(
    layout: &LayoutState,
    pairing_challenge: &Mutex<Option<PairingChallenge>>,
) -> PairingStatus {
    if layout.machine_role != "client" {
        return idle_pairing_status();
    }

    let now = Instant::now();
    if let Ok(mut challenge) = pairing_challenge.lock() {
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

    PairingStatus {
        state: "available".into(),
        code: String::new(),
        requester_name: String::new(),
        requester_ip: String::new(),
        expires_at_ms: 0,
        detail: "客户端等待服务端发起配对。".into(),
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

    /// A drag pulled off one controlled machine can be relayed onward to
    /// another only while it is still in the user's hand. `is_active` stays
    /// true for seconds after the release so files still in flight get placed,
    /// and reading THAT as "a relay is in progress" made the next crossing
    /// forward a drag that had already been dropped here.
    #[cfg(target_os = "macos")]
    #[test]
    fn drag_place_relay_closes_before_the_placement_grace_window() {
        drag_place::begin("photo.png");
        assert!(drag_place::is_relaying());
        assert!(drag_place::is_active());

        drag_place::release(None);
        assert!(
            drag_place::is_active(),
            "late files must still find their target"
        );
        assert!(
            !drag_place::is_relaying(),
            "a released drag must not be forwarded to the next machine"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn unavailable_local_displays_preserve_layout_across_restart() {
        let mut saved = test_layout();
        let mut arranged = test_screen("local-device");
        arranged.id = "local-display-1".into();
        arranged.width = 2560;
        arranged.height = 1440;
        arranged.x = 7500;
        arranged.y = -3960;
        saved.devices[0].screens = vec![arranged.clone()];
        saved.selected_screen_id = arranged.id.clone();

        let mut unavailable = arranged.clone();
        unavailable.name = "Display unavailable".into();
        unavailable.width = 1;
        unavailable.height = 1;
        unavailable.x = 0;
        unavailable.y = 0;

        for detected in [Vec::new(), vec![unavailable]] {
            let sleeping = restore_local_screen_layout(
                detected.clone(),
                &saved.devices[0].screens,
                &mut Vec::new(),
            );
            assert_eq!(sleeping, saved.devices[0].screens);

            // Restart with no monitors available and no in-process memory.
            let mut disk = saved.clone();
            disk.devices[0].screens = sleeping;
            let disk = serde_json::from_slice(&serde_json::to_vec(&disk).unwrap()).unwrap();
            let mut native = test_layout();
            native.devices[0].screens = detected;
            let restarted = normalize_saved_layout(disk, native);
            assert_eq!(restarted.devices[0].screens, saved.devices[0].screens);
            assert_eq!(restarted.devices[1], saved.devices[1]);

            let mut awake = arranged.clone();
            awake.x = 0;
            awake.y = 0;
            let restored = restore_local_screen_layout(
                vec![awake],
                &restarted.devices[0].screens,
                &mut Vec::new(),
            );
            assert_eq!(restored, saved.devices[0].screens);
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn restore_local_screen_layout_survives_lid_close_and_reopen() {
        let mk = |id: &str, w: i32, h: i32, x: i32, y: i32| Screen {
            id: id.into(),
            device_id: "local-device".into(),
            name: format!("Monitor {w}x{h}"),
            x,
            y,
            width: w,
            height: h,
            scale: 1.0,
            is_primary: w == 2560,
        };
        let mut memory = Vec::new();

        // External on top, built-in arranged below (the user's real layout).
        let arranged = vec![
            mk("local-display-1", 2560, 1440, 0, 0),
            mk("local-display-2", 1512, 982, 518, 1440),
        ];

        // Lid closes: only the external is detected — built-in drops out.
        let closed = restore_local_screen_layout(
            vec![mk("local-display-1", 2560, 1440, 0, 0)],
            &arranged,
            &mut memory,
        );
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].id, "local-display-1");

        // Lid re-opens with the enumeration SHUFFLED (built-in now index 0, so
        // its index-based id collides with the external's). Each physical display
        // must still be restored to its own id and its user-arranged position.
        let reopened = restore_local_screen_layout(
            vec![
                mk("local-display-1", 1512, 982, 99, 99), // built-in, default (wrong) pos
                mk("local-display-2", 2560, 1440, 99, 99), // external, default (wrong) pos
            ],
            &closed,
            &mut memory,
        );
        let built_in = reopened
            .iter()
            .find(|s| (s.width, s.height) == (1512, 982))
            .unwrap();
        let external = reopened
            .iter()
            .find(|s| (s.width, s.height) == (2560, 1440))
            .unwrap();
        assert_eq!(built_in.id, "local-display-2");
        assert_eq!((built_in.x, built_in.y), (518, 1440), "built-in restored below");
        assert_eq!(external.id, "local-display-1");
        assert_eq!((external.x, external.y), (0, 0), "external restored to origin");
    }

    #[test]
    fn display_refresh_updates_native_coordinates_and_preserves_arrangement() {
        let mut layout = test_layout();
        let mut first = test_screen("local-device");
        first.id = "local-display-1".into();
        first.name = "Display A".into();
        let mut second = first.clone();
        second.id = "local-display-2".into();
        second.name = "Display B".into();
        second.x = first.width;
        second.is_primary = false;
        layout.devices[0].screens = vec![first.clone(), second.clone()];
        let mut native = layout.clone();
        for screen in &mut layout.devices[0].screens {
            screen.x += 7500;
            screen.y = -3960;
        }
        layout.selected_screen_id = layout.devices[1].screens[0].id.clone();
        let selected = layout.selected_screen_id.clone();

        second.id = "local-display-1".into();
        second.x = 0;
        second.y = 1080;
        second.scale = 1.5;
        first.id = "local-display-2".into();
        let detected = vec![second, first];
        let mut memory = Vec::new();
        assert!(apply_detected_local_screens(
            &mut layout,
            &mut native,
            detected.clone(),
            &mut memory
        ));
        let arranged = &layout.devices[0].screens[0];
        assert_eq!(arranged.id, "local-display-2");
        assert_eq!((arranged.x, arranged.y), (9420, -3960));
        let physical = &native.devices[0].screens[0];
        assert_eq!(physical.id, arranged.id);
        assert_eq!((physical.x, physical.y, physical.scale), (0, 1080, 1.5));
        assert_eq!(layout.selected_screen_id, selected);
        assert!(!apply_detected_local_screens(
            &mut layout,
            &mut native,
            detected.clone(),
            &mut memory
        ));
        assert!(!apply_detected_local_screens(
            &mut layout,
            &mut native,
            Vec::new(),
            &mut memory
        ));

        // Reboot with the OS enumerating the same two displays in reverse order.
        let mut detected_layout = test_layout();
        detected_layout.devices[0].screens = detected;
        let restored = normalize_saved_layout(layout.clone(), detected_layout.clone());
        align_native_screen_ids(&mut detected_layout, &restored);
        assert_eq!(restored.devices[0].screens, layout.devices[0].screens);
        assert_eq!(
            detected_layout.devices[0].screens,
            native.devices[0].screens
        );

        // Identical display models must not exchange their positions on every poll.
        for screen in &mut layout.devices[0].screens {
            screen.name = "Same model".into();
        }
        for screen in &mut native.devices[0].screens {
            screen.name = "Same model".into();
        }
        let unchanged = native.devices[0].screens.clone();
        assert!(!apply_detected_local_screens(
            &mut layout,
            &mut native,
            unchanged,
            &mut Vec::new()
        ));
    }

    #[test]
    fn legacy_extra_controls_are_disabled_without_changing_existing_switches() {
        let mut saved = test_layout();
        saved.edge_switch_hotkey = "ctrl+alt+k".into();
        saved.clipboard_sync = false;
        saved.clipboard_on_demand = true;
        saved.clipboard_paste_hotkey = "alt+shift+v".into();
        saved.input_protection = InputProtection {
            local_only: true, protect_fullscreen: true,
            blocked_applications: vec!["snipaste.exe".into()], lock_hotkey: "alt+shift+l".into(),
        };
        let restored = normalize_saved_layout(saved, test_layout());
        assert_eq!(restored.edge_switch_hotkey, "ctrl+alt+k");
        assert!(!restored.clipboard_sync);
        assert!(!restored.clipboard_on_demand);
        assert_eq!(restored.clipboard_paste_hotkey, "disabled");
        assert_eq!(restored.input_protection, InputProtection::default());
    }

    fn test_layout() -> LayoutState {
        LayoutState {
            lock_sync: false,
            input_protection: Default::default(),
            devices: vec![
                Device {
                    pointer_speed: 1.0,
                    scroll_speed: 1.0,
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
                    pointer_speed: 1.0,
                    scroll_speed: 1.0,
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
            clipboard_on_demand: false,
            clipboard_paste_hotkey: default_clipboard_paste_hotkey(),
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
    fn manually_selected_peer_address_survives_discovery_and_file_transfer() {
        let mut layout = test_layout();
        layout.devices[1].source = "manual".into();
        layout.devices[1].host = "169.254.10.2".into();
        let peer = test_peer();
        apply_peer_presence(&mut layout, &[peer.clone()]);
        assert_eq!(layout.devices[1].host, "169.254.10.2");
        assert!(layout.devices[1].online);
        assert_eq!(layout.devices[1].quic_port, peer.quic_port);
        let target =
            file_transfer_target_for_device(&layout, &[peer], &layout.devices[1].id).unwrap();
        assert_eq!(target.addr, "169.254.10.2:52001");
        let restored: LayoutState =
            serde_json::from_slice(&serde_json::to_vec(&layout).unwrap()).unwrap();
        assert_eq!(restored.devices[1].source, "manual");
        assert_eq!(restored.devices[1].host, "169.254.10.2");
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
    fn discovery_cannot_replace_paired_device_certificates() {
        let mut layout = test_layout();
        let peer = test_peer();
        layout.paired_controllers = vec![PairedController {
            id: peer.id.clone(),
            name: peer.name.clone(),
            host: peer.host.clone(),
            ip: peer.ip.clone(),
            transport_public_key: peer.transport_public_key.clone(),
            protocol_version: peer.protocol_version,
            cluster_id: layout.cluster_id.clone(),
            paired_at_ms: 1,
        }];
        let trusted = layout.paired_controllers[0].clone();
        let mut impostor = peer.clone();
        impostor.transport_public_key = "untrusted-replacement".into();
        apply_peer_presence(&mut layout, &[impostor]);
        assert!(!layout.devices[1].online);
        assert_eq!(layout.devices[1].transport_public_key, "peer-public-key");
        assert_eq!(layout.paired_controllers[0], trusted);

        let mut moved = peer;
        moved.id = "changed-peer-id".into();
        moved.ip = "10.0.0.99".into();
        apply_peer_presence(&mut layout, &[moved]);
        assert!(layout.devices[1].online);
        assert_eq!(layout.devices[1].host, "10.0.0.99");
        assert_eq!(layout.paired_controllers[0].ip, "10.0.0.99");
        assert_eq!(
            layout.paired_controllers[0].transport_public_key,
            trusted.transport_public_key
        );
    }

    #[test]
    fn peer_presence_keeps_trusted_key_when_peer_id_changes() {
        let mut layout = test_layout();
        let mut peer = test_peer();
        peer.id = "rotated-client-id".into();

        apply_peer_presence(&mut layout, &[peer]);

        assert!(layout.devices[1].online);
        assert!(layout.devices[1].input_ready);
        assert_eq!(layout.devices[1].transport_public_key, "peer-public-key");
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
        let code = stored.as_ref().expect("challenge").code.clone();
        drop(stored);

        // The already-paired client must show this code, not "paired": the
        // window popped up with nothing to type on the server.
        let status = pairing_status(&layout, &challenge);
        assert_eq!(status.state, "requested");
        assert_eq!(status.code, code);
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
            paste: false, paste_token: String::new(),
            origin_id: "attacker".into(),
            origin_transport_public_key: String::new(),
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
    fn clipboard_packet_authorized_by_transport_key_after_id_drift() {
        // Regression for the macOS->Windows one-way clipboard bug: input kept
        // working (it matches the stable transport key) but clipboard was
        // rejected because it matched the origin id alone, which drifts when the
        // controller's LAN IP changes. Clipboard must accept the same key.
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
            paste: false, paste_token: String::new(),
            // id no longer matches the recorded controller (IP moved),
            origin_id: "server-10-0-0-77".into(),
            origin_transport_public_key: "server-key".into(),
            target_id: "local-device".into(),
            cluster_id: layout.cluster_id.clone(),
            pair_secret: layout.pair_secret.clone(),
            signature: "text:hi".into(),
            formats: vec![ClipboardFormat {
                kind: "plainText".into(),
                text: "hi".into(),
                image: None,
            }],
            text: "hi".into(),
            image: None,
            sequence: 1,
        };

        assert!(
            clipboard_packet_authorized(&layout, &packet),
            "a drifted id with the paired transport key must still be authorized"
        );
        packet.origin_transport_public_key = "attacker-key".into();
        assert!(
            !clipboard_packet_authorized(&layout, &packet),
            "neither the id nor the key matches — must be rejected"
        );
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
    fn file_clipboard_stops_when_copy_changes_or_either_switch_is_disabled() {
        let mut enabled = test_layout();
        enabled.clipboard_sync = true;
        let layout = Arc::new(Mutex::new(enabled));
        let stop = AtomicBool::new(false);
        assert!(file_clipboard_sync_current(
            &layout,
            &stop,
            Some(7),
            Some(7)
        ));
        assert!(!file_clipboard_sync_current(
            &layout,
            &stop,
            Some(7),
            Some(8)
        ));
        layout.lock().unwrap().file_transfer_enabled = false;
        assert!(!file_clipboard_sync_current(
            &layout,
            &stop,
            Some(7),
            Some(7)
        ));
        layout.lock().unwrap().file_transfer_enabled = true;
        layout.lock().unwrap().clipboard_sync = false;
        assert!(!file_clipboard_sync_current(
            &layout,
            &stop,
            Some(7),
            Some(7)
        ));
        layout.lock().unwrap().clipboard_sync = true;
        stop.store(true, Ordering::Relaxed);
        assert!(!file_clipboard_sync_current(
            &layout,
            &stop,
            Some(7),
            Some(7)
        ));
    }

    #[test]
    fn file_clipboard_commit_respects_switches_and_pairing() {
        let mut layout = test_layout();
        layout.clipboard_sync = true;
        let id = new_transfer_id("test-clipboard-receipt");
        let origin = layout.devices[1].id.clone();
        clipboard_exchange::remember_file(&origin, &id, PathBuf::from("/received.txt"));
        let mut packet = ClipboardFilesCommit {
            protocol: CLIPBOARD_FILES_PROTOCOL.into(),
            origin_id: origin,
            origin_transport_public_key: layout.devices[1].transport_public_key.clone(),
            target_id: "local-device".into(),
            cluster_id: layout.cluster_id.clone(),
            pair_secret: layout.pair_secret.clone(),
            ids: vec![id],
            paste: false,
            paste_token: String::new(),
        };
        let seen = Arc::new(Mutex::new(None));
        let echo = Arc::new(Mutex::new(None));
        for disabled in ["clipboard", "files", "identity"] {
            let mut blocked = layout.clone();
            match disabled {
                "clipboard" => blocked.clipboard_sync = false,
                "files" => blocked.file_transfer_enabled = false,
                _ => packet.pair_secret = "unpaired-secret".into(),
            }
            let result = handle_clipboard_files_commit_with_writer(
                &encode_wire_packet(&packet).unwrap(),
                &blocked,
                "local-device",
                &seen,
                &echo,
                |_| panic!("a rejected file clipboard must not be written"),
            );
            assert_eq!(result, Some(false));
        }
        packet.pair_secret = layout.pair_secret.clone();
        assert_eq!(
            handle_clipboard_files_commit_with_writer(
                &encode_wire_packet(&packet).unwrap(),
                &layout,
                "local-device",
                &seen,
                &echo,
                |paths| {
                    assert_eq!(paths, ["/received.txt"]);
                    Ok(())
                },
            ),
            Some(true)
        );
        assert_eq!(
            seen.lock().unwrap().as_deref(),
            Some(clipboard::files_signature(&["/received.txt".into()]).as_str())
        );
        assert!(clipboard_echo_active(&echo));
    }

    #[test]
    fn file_clipboard_streams_files_and_folders_then_commits_without_a_paste() {
        let free_port = || UdpSocket::bind(("127.0.0.1", 0)).unwrap().local_addr().unwrap().port();
        let sandbox = tempfile::tempdir().unwrap();
        let source = sandbox.path().join("source");
        let destination = sandbox.path().join("received");
        fs::create_dir_all(source.join("中文文件夹/empty")).unwrap();
        fs::create_dir_all(&destination).unwrap();
        fs::write(source.join("note.txt"), b"hello file").unwrap();
        fs::write(source.join("中文文件夹/nested.txt"), b"hello folder").unwrap();
        let sender = quic_transport::start(
            free_port(),
            sandbox.path().join("sender-id"),
            Arc::new(|_, _| {}),
            Arc::new(|_, _| true),
        )
        .unwrap();
        let mut layout = test_layout();
        layout.clipboard_sync = true;
        layout.devices[1].transport_public_key = sender.public_key().into();
        let origin = layout.devices[1].id.clone();
        let seen = Arc::new(Mutex::new(None));
        let echo = Arc::new(Mutex::new(None));
        let written = Arc::new(Mutex::new(Vec::<String>::new()));
        let transfers = Arc::new(Mutex::new(HashMap::new()));
        let keep_running = Arc::new(AtomicBool::new(true));
        let stop_on_chunk = Arc::new(AtomicBool::new(false));
        let (
            receive_layout,
            receive_root,
            receive_seen,
            receive_echo,
            receive_written,
            receive_transfers,
            receive_running,
            receive_stop,
        ) = (
            layout.clone(),
            destination.clone(),
            Arc::clone(&seen),
            Arc::clone(&echo),
            Arc::clone(&written),
            Arc::clone(&transfers),
            Arc::clone(&keep_running),
            Arc::clone(&stop_on_chunk),
        );
        let receiver = quic_transport::start(
            free_port(),
            sandbox.path().join("receiver-id"),
            Arc::new(|_, _| {}),
            Arc::new(move |payload, _| {
                if let Some(commit) = decode_wire_packet::<ClipboardFilesCommit>(&payload) {
                    assert!(
                        !commit.paste,
                        "automatic sync must wait for the user's normal paste"
                    );
                }
                if let Some(accepted) = handle_clipboard_files_commit_with_writer(
                    &payload,
                    &receive_layout,
                    "local-device",
                    &receive_seen,
                    &receive_echo,
                    |paths| {
                        *receive_written.lock().unwrap() = paths.to_vec();
                        Ok(())
                    },
                ) {
                    return accepted;
                }
                if receive_stop.load(Ordering::Relaxed)
                    && decode_wire_packet::<FileTransferPacket>(&payload)
                        .is_some_and(|p| p.kind == "chunk")
                {
                    receive_running.store(false, Ordering::Relaxed);
                }
                handle_file_transfer_packet_with_root(
                    &payload,
                    &receive_layout,
                    "local-device",
                    &receive_transfers,
                    &receive_root,
                )
            }),
        )
        .unwrap();
        let target = FileTransferTarget {
            device_id: "local-device".into(),
            name: "Test receiver".into(),
            addr: format!("127.0.0.1:{}", receiver.port()),
            transport_public_key: receiver.public_key().into(),
            protocol_version: quic_transport::PROTOCOL_VERSION,
            cluster_id: layout.cluster_id,
            pair_secret: layout.pair_secret,
        };
        let paths = [source.join("note.txt"), source.join("中文文件夹")]
            .map(|p| p.to_string_lossy().into_owned());
        let guard = || keep_running.load(Ordering::Relaxed);
        send_clipboard_files(
            &sender,
            &origin,
            &target,
            &paths,
            false,
            String::new(),
            None,
            Some(&guard),
        )
        .unwrap();
        let received = written.lock().unwrap().clone();
        assert_eq!(received.len(), 2);
        assert_eq!(fs::read(&received[0]).unwrap(), b"hello file");
        assert_eq!(
            fs::read(Path::new(&received[1]).join("nested.txt")).unwrap(),
            b"hello folder"
        );
        assert!(Path::new(&received[1]).join("empty").is_dir());
        assert_eq!(
            *seen.lock().unwrap(),
            Some(clipboard::files_signature(&received))
        );
        assert!(clipboard_echo_active(&echo));

        // A later copy cancels the old upload before it can replace the clipboard.
        stop_on_chunk.store(true, Ordering::Relaxed);
        let error = send_clipboard_files(
            &sender,
            &origin,
            &target,
            &paths,
            false,
            String::new(),
            None,
            Some(&guard),
        )
        .unwrap_err();
        assert_eq!(error, CLIPBOARD_FILES_SUPERSEDED);
        assert_eq!(*written.lock().unwrap(), received);
        assert!(
            transfers.lock().unwrap().is_empty(),
            "cancelled partial uploads must be removed"
        );
        sender.stop_and_wait().unwrap();
        receiver.stop_and_wait().unwrap();
    }

    #[test]
    fn clipboard_poll_skips_unchanged_content_but_keeps_retries_and_peer_changes() {
        let mut target = input::ClipboardTarget {
            device_id: "client".into(),
            addr: "192.0.2.1:47834".into(),
            transport_public_key: "key".into(),
            protocol_version: 1,
            cluster_id: "cluster".into(),
            pair_secret: "secret".into(),
            expires_at: None,
        };
        let previous = Some((7, target.device_id.clone(), target.addr.clone()));
        assert!(clipboard_poll_unchanged(Some(7), &previous, &target, false));
        assert!(!clipboard_poll_unchanged(
            Some(8),
            &previous,
            &target,
            false
        ));
        assert!(!clipboard_poll_unchanged(Some(7), &previous, &target, true));
        assert!(!clipboard_poll_unchanged(None, &previous, &target, false));
        target.device_id = "other-client".into();
        assert!(!clipboard_poll_unchanged(
            Some(7),
            &previous,
            &target,
            false
        ));
        target.device_id = "client".into();
        target.addr = "192.0.2.2:47834".into();
        assert!(!clipboard_poll_unchanged(
            Some(7),
            &previous,
            &target,
            false
        ));
    }

    #[test]
    fn clipboard_image_packet_fits_transport_stream_budget() {
        let raw_rgba_bytes = 3840 * 2160 * 4;
        let encoded_len = raw_rgba_bytes / 3 * 4;
        let packet = clipboard_packet_from_content(
            ClipboardContent::Image(ClipboardImage {
                width: 3840,
                height: 2160,
                rgba_base64: "A".repeat(encoded_len),
            }),
            "local-device".into(),
            String::new(),
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
            String::new(),
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
            paste: false, paste_token: String::new(),
            origin_id: "peer-client-10-0-0-2".into(),
            origin_transport_public_key: String::new(),
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
            String::new(),
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
            paste: false, paste_token: String::new(),
            origin_id: "peer-client-10-0-0-2".into(),
            origin_transport_public_key: String::new(),
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
            String::new(),
            "local-device".into(),
            layout.cluster_id.clone(),
            layout.pair_secret.clone(),
            10,
        );
        let stale = clipboard_packet_from_content(
            ClipboardContent::Text("old".into()),
            "peer-client-10-0-0-2".into(),
            String::new(),
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
            String::new(),
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

        // The two machines swapped roles: this client still lists the
        // controller as a device from its server days, online but not
        // input-ready. The transfer must still go to the controller.
        let mut stale = layout.devices[1].clone();
        stale.id = "peer-server-10-0-0-1".into();
        stale.role = "client".into();
        stale.online = true;
        stale.input_ready = false;
        layout.devices.push(stale);
        let target =
            file_transfer_target_for_device(&layout, &peers, "peer-server-10-0-0-1").unwrap();
        assert_eq!(target.addr, "10.0.0.1:52001");
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
    fn edge_drop_transfer_finalizes_on_desktop_root_with_part_file_staged_elsewhere() {
        let layout = test_layout();
        let root = temp_test_dir("file-transfer-staging");
        let desktop = temp_test_dir("file-transfer-desktop");
        let transfers = Arc::new(Mutex::new(HashMap::new()));

        for packet in [
            test_file_transfer_packet("start", "transfer-3", "note.txt", 5, 0, 0, b""),
            test_file_transfer_packet("chunk", "transfer-3", "note.txt", 5, 0, 0, b"hello"),
            test_file_transfer_packet("finish", "transfer-3", "note.txt", 5, 1, 5, b""),
        ] {
            let mut packet = packet;
            packet.drop_to_desktop = true;
            assert!(handle_decoded_file_transfer_packet(
                packet,
                &layout,
                "local-device",
                &transfers,
                &root,
                Some(&desktop)
            ));
        }

        assert_eq!(
            fs::read_to_string(desktop.join("note.txt")).expect("file lands on the desktop root"),
            "hello"
        );
        assert!(
            !root.join("note.txt").exists(),
            "final file must not appear in the staging root"
        );
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(desktop);
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

    #[test]
    fn directory_packets_restore_the_tree_and_empty_folders() {
        let source = tempfile::tempdir().unwrap();
        fs::create_dir_all(source.path().join("empty")).unwrap();
        fs::create_dir_all(source.path().join("nested")).unwrap();
        fs::write(source.path().join("nested/example.txt"), "hello").unwrap();
        let bundle = folder_transfer::bundle(source.path()).unwrap();
        let bytes = fs::read(bundle.path()).unwrap();
        let root = tempfile::tempdir().unwrap();
        let mut layout = test_layout();
        layout.file_transfer_enabled = true;
        layout.cluster_id = "cluster-test".into();
        layout.pair_secret = "secret-test".into();
        let transfers = Arc::new(Mutex::new(HashMap::new()));
        for (kind, index, offset, data) in [
            ("start", 0, 0, &[][..]), ("chunk", 0, 0, bytes.as_slice()),
            ("finish", 1, bytes.len() as u64, &[][..]),
        ] {
            let mut packet = test_file_transfer_packet(kind, "directory-test", "photos", bytes.len() as u64, index, offset, data);
            packet.directory_bundle = true;
            assert!(handle_decoded_file_transfer_packet(packet, &layout, "local-device", &transfers, root.path(), None));
        }
        assert!(root.path().join("photos/empty").is_dir());
        assert_eq!(fs::read_to_string(root.path().join("photos/nested/example.txt")).unwrap(), "hello");
        assert!(transfers.lock().unwrap().is_empty());
    }

    #[test]
    fn finalizing_a_transfer_never_overwrites_an_existing_file() {
        let root = tempfile::tempdir().unwrap();
        let temp = root.path().join("incoming.part");
        let mut destination = root.path().join("existing.txt");
        fs::write(&destination, "original").unwrap();
        fs::write(&temp, "new").unwrap();
        finalize_received_transfer(&temp, &mut destination, false).unwrap();
        assert_eq!(fs::read_to_string(root.path().join("existing.txt")).unwrap(), "original");
        assert_eq!(fs::read_to_string(destination).unwrap(), "new");
        assert!(!temp.exists());
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
        FileTransferPacket {
            protocol: FILE_TRANSFER_PROTOCOL.into(),
            kind: kind.into(),
            transfer_id: transfer_id.into(),
            origin_id: "peer-client-10-0-0-2".into(),
            target_id: "local-device".into(),
            cluster_id: "cluster-test".into(),
            pair_secret: "secret-test".into(),
            file_name: file_name.into(),
            total_bytes,
            chunk_index,
            offset,
            data: data.to_vec(),
            drop_to_desktop: false,
            drag_drop: false,
            client_log: false,
            directory_bundle: false,
            clipboard_transfer: false,
        }
    }

    fn peer_request(protocol: &str, origin: &str) -> PeerRequestPacket {
        PeerRequestPacket {
            protocol: protocol.into(),
            paste_token: String::new(),            origin_id: origin.into(),
            target_id: "local-device".into(),
            cluster_id: "cluster-test".into(),
            pair_secret: "secret-test".into(),
        }
    }

    fn paired_client_layout() -> LayoutState {
        let mut layout = test_layout();
        layout.machine_role = "client".into();
        layout.cluster_id = "cluster-test".into();
        layout.pair_secret = "secret-test".into();
        layout.file_transfer_enabled = true;
        layout.paired_controllers = vec![PairedController {
            id: "controller".into(),
            name: "Controller".into(),
            host: "controller.local".into(),
            ip: "10.0.0.1".into(),
            transport_public_key: "key".into(),
            protocol_version: quic_transport::PROTOCOL_VERSION,
            cluster_id: "cluster-test".into(),
            paired_at_ms: 0,
        }];
        layout
    }

    #[test]
    fn only_the_paired_controller_can_update_a_client() {
        let layout = paired_client_layout();
        let update = |packet: &PeerRequestPacket, layout: &LayoutState| {
            authorize_peer_request(packet, layout, "local-device")
        };
        assert_eq!(
            update(&peer_request(REMOTE_UPDATE_PROTOCOL, "controller"), &layout),
            Some(PeerRequest::Update)
        );
        assert_eq!(
            update(&peer_request(LOG_REQUEST_PROTOCOL, "controller"), &layout),
            Some(PeerRequest::Log)
        );
        // Another device that knows the secret is not the paired controller.
        assert_eq!(update(&peer_request(REMOTE_UPDATE_PROTOCOL, "other"), &layout), None);
        // A wrong secret, or a request meant for another device.
        let mut wrong_secret = peer_request(REMOTE_UPDATE_PROTOCOL, "controller");
        wrong_secret.pair_secret = "guess".into();
        assert_eq!(update(&wrong_secret, &layout), None);
        let mut elsewhere = peer_request(REMOTE_UPDATE_PROTOCOL, "controller");
        elsewhere.target_id = "someone-else".into();
        assert_eq!(update(&elsewhere, &layout), None);
        // Unpaired (paired list cleared, secret kept): answers no one.
        let mut unpaired = layout.clone();
        unpaired.paired_controllers.clear();
        assert_eq!(update(&peer_request(REMOTE_UPDATE_PROTOCOL, "controller"), &unpaired), None);
        // Lock sync: the paired controller may lock a client, nobody else may.
        assert_eq!(
            update(&peer_request(LOCK_SCREEN_PROTOCOL, "controller"), &layout),
            Some(PeerRequest::Lock)
        );
        assert_eq!(update(&peer_request(LOCK_SCREEN_PROTOCOL, "other"), &layout), None);
        // A server is never updated or locked by a peer; no log without file
        // transfer.
        let mut server = layout.clone();
        server.machine_role = "server".into();
        assert_eq!(update(&peer_request(REMOTE_UPDATE_PROTOCOL, "controller"), &server), None);
        assert_eq!(update(&peer_request(LOCK_SCREEN_PROTOCOL, "controller"), &server), None);
        let mut no_transfer = layout.clone();
        no_transfer.file_transfer_enabled = false;
        assert_eq!(update(&peer_request(LOG_REQUEST_PROTOCOL, "controller"), &no_transfer), None);
    }

    #[test]
    fn clipboard_retry_backs_off_to_a_minute() {
        assert_eq!(clipboard_retry_delay(1), Duration::from_secs(2));
        assert_eq!(clipboard_retry_delay(2), Duration::from_secs(4));
        assert_eq!(clipboard_retry_delay(5), Duration::from_secs(32));
        assert_eq!(clipboard_retry_delay(6), Duration::from_secs(60));
        assert_eq!(clipboard_retry_delay(40), Duration::from_secs(60));
    }

    #[test]
    fn rich_clipboard_formats_round_trip_and_keep_a_plain_text_fallback() {
        let packet = clipboard_packet_from_content(ClipboardContent::Rich(rich_clipboard::RichText {
            text: "中文".into(), html: Some("<b>中文</b>".into()), rtf: Some(b"{\\rtf1 sample}".to_vec()),
        }), "peer".into(), "key".into(), "local-device".into(), "cluster-test".into(), "secret-test".into(), 1);
        let bytes = encode_wire_packet(&packet).unwrap();
        let decoded = decode_wire_packet::<ClipboardPacket>(&bytes).unwrap();
        assert_eq!(decoded.text, "中文");
        let content = clipboard_content_from_packet(decoded).unwrap();
        let ClipboardContent::Rich(rich) = content else { panic!("rich formats lost"); };
        assert_eq!(rich.html.as_deref(), Some("<b>中文</b>"));
        assert_eq!(rich.rtf.as_deref(), Some(&b"{\\rtf1 sample}"[..]));
        let mut legacy = decode_wire_packet::<ClipboardPacket>(&bytes).unwrap();
        legacy.formats.retain(|format| format.kind == "plainText");
        assert!(matches!(clipboard_content_from_packet(legacy), Some(ClipboardContent::Text(text)) if text == "中文"));
    }

    #[test]
    fn on_demand_clipboard_does_not_overwrite_local_content_without_a_requested_paste() {
        let mut layout = test_layout();
        layout.cluster_id = "cluster-test".into(); layout.pair_secret = "secret-test".into();
        layout.clipboard_on_demand = true;
        let mut packet = clipboard_packet_from_content(ClipboardContent::Text("remote".into()), "peer".into(), "key".into(),
            "local-device".into(), layout.cluster_id.clone(), layout.pair_secret.clone(), 1);
        let seen = Arc::new(Mutex::new(None)); let echo = Arc::new(Mutex::new(None)); let sequences = Arc::new(Mutex::new(HashMap::new()));
        let mut writes = 0;
        assert!(!handle_clipboard_packet_with_writer(&encode_wire_packet(&packet).unwrap(), &layout, "local-device", &seen, &echo, &sequences, |_| { writes += 1; Ok(()) }));
        packet.paste = true;
        assert!(!handle_clipboard_packet_with_writer(&encode_wire_packet(&packet).unwrap(), &layout, "local-device", &seen, &echo, &sequences, |_| { writes += 1; Ok(()) }));
        assert_eq!(writes, 0);
        packet.paste_token = clipboard_exchange::expect_paste("key");
        assert!(handle_clipboard_packet_with_writer(&encode_wire_packet(&packet).unwrap(), &layout, "local-device", &seen, &echo, &sequences, |_| { writes += 1; Ok(()) }));
        assert_eq!(writes, 1);
    }

    #[test]
    fn lan_ip_choice_skips_proxy_tunnels_and_host_side_adapters() {
        // Clash/Mihomo/Surge TUN range is never a LAN.
        assert!(!usable_discovery_ipv4(Ipv4Addr::new(198, 18, 0, 1)));
        assert!(!usable_discovery_ipv4(Ipv4Addr::new(198, 19, 255, 254)));
        assert!(usable_discovery_ipv4(Ipv4Addr::new(198, 20, 0, 1)));
        // sing-box tunnel, VirtualBox host-only, the real Wi-Fi address.
        let addresses = [
            Ipv4Addr::new(172, 19, 0, 1),
            Ipv4Addr::new(192, 168, 56, 1),
            Ipv4Addr::new(192, 168, 66, 106),
        ];
        assert_eq!(
            preferred_lan_ipv4(&addresses),
            Some(Ipv4Addr::new(192, 168, 66, 106))
        );
    }

    #[test]
    fn tail_of_file_returns_only_the_last_bytes() {
        let path = std::env::temp_dir().join(format!("mykvm-tail-{}.log", random_hex(6)));
        fs::write(&path, b"0123456789").expect("write");
        assert_eq!(tail_of_file(&path, 4).expect("tail"), b"6789");
        // Smaller than the cap: the whole file.
        assert_eq!(tail_of_file(&path, 100).expect("tail"), b"0123456789");
        let _ = fs::remove_file(&path);
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
