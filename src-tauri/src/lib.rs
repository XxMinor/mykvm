use std::{
    env, fs,
    net::{SocketAddr, UdpSocket},
    path::PathBuf,
    process::Command,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex, OnceLock,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[cfg(not(target_os = "windows"))]
use std::{io::Write, process::Stdio};

use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Manager, Monitor, WindowEvent,
};

mod input;
mod quic_transport;
pub mod shared_input;
#[cfg(target_os = "windows")]
pub mod windows_input;

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
const PEER_TTL_MS: u64 = 30_000;
const MAX_DISCOVERY_PEERS: usize = 128;
const PAIRING_CODE_TTL_MS: u64 = 60_000;
const PAIRING_MAX_ATTEMPTS: u8 = 5;
const CLIPBOARD_PROTOCOL: &str = "mykvm.clipboard.v1";
const CLIPBOARD_MAX_TEXT_BYTES: usize = 256 * 1024;
// Raw RGBA can be large (a 2560x1440 frame is ~14 MB); cap it so a stray huge
// copy never floods the LAN transport. Images above this are skipped.
const CLIPBOARD_MAX_IMAGE_BYTES: usize = 32 * 1024 * 1024;
// After we write clipboard content received from a peer, ignore our own
// clipboard for a short grace window. Reading an image back through the OS
// pasteboard is not always byte-identical to what we wrote (macOS re-encodes
// it), so a pure content-signature check can ping-pong; this window guarantees
// we never echo received content straight back.
const CLIPBOARD_ECHO_GRACE_MS: u64 = 1200;
const CLIPBOARD_RETRY_INTERVAL_MS: u64 = 2000;
const QUIT_EXISTING_ARG: &str = "--mykvm-quit-existing";
const INSTALL_INPUT_SERVICE_ARG: &str = "--install-input-service";
const UNINSTALL_INPUT_SERVICE_ARG: &str = "--uninstall-input-service";
const HELPER_PATH_ARG: &str = "--helper-path";

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
static WINDOWS_PROCESS_SAMPLE: OnceLock<Mutex<Option<WindowsProcessSample>>> = OnceLock::new();

#[cfg(target_os = "windows")]
#[derive(Clone, Copy)]
struct WindowsProcessSample {
    instant: Instant,
    process_time_100ns: u64,
}

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
    role: String,
    #[serde(default = "default_device_source")]
    source: String,
    screens: Vec<Screen>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PerformanceSample {
    timestamp_ms: u64,
    app_cpu_percent: f64,
    app_memory_mb: f64,
    transport_packets: u64,
    input_events: u64,
    clipboard_packets: u64,
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

struct AppRuntime {
    app_handle: AppHandle,
    layout: Arc<Mutex<LayoutState>>,
    native_layout: Mutex<LayoutState>,
    runtime: Mutex<RuntimeStatus>,
    peers: Arc<Mutex<Vec<LanPeer>>>,
    pairing_challenge: Arc<Mutex<Option<PairingChallenge>>>,
    quic_transport: Mutex<Option<quic_transport::TransportHandle>>,
    discovery_stop: Mutex<Option<Arc<AtomicBool>>>,
    input_stop: Mutex<Option<Arc<AtomicBool>>>,
    clipboard_stop: Mutex<Option<Arc<AtomicBool>>>,
    clipboard_seen_text: Arc<Mutex<Option<String>>>,
    clipboard_echo_until: Arc<Mutex<Option<Instant>>>,
    remote_input_active: Arc<AtomicBool>,
    main_window_visible: Arc<AtomicBool>,
    allow_explicit_quit: Arc<AtomicBool>,
    clipboard_target: Arc<Mutex<Option<input::ClipboardTarget>>>,
    input_receive_enabled: Arc<AtomicBool>,
    clipboard_receive_enabled: Arc<AtomicBool>,
    transport_packets: Arc<AtomicU64>,
    input_events: Arc<AtomicU64>,
    clipboard_packets: Arc<AtomicU64>,
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
            quic_transport: Mutex::new(None),
            discovery_stop: Mutex::new(None),
            input_stop: Mutex::new(None),
            clipboard_stop: Mutex::new(None),
            clipboard_seen_text: Arc::new(Mutex::new(None)),
            clipboard_echo_until: Arc::new(Mutex::new(None)),
            remote_input_active: Arc::new(AtomicBool::new(false)),
            main_window_visible: Arc::new(AtomicBool::new(true)),
            allow_explicit_quit: Arc::new(AtomicBool::new(false)),
            clipboard_target: Arc::new(Mutex::new(None)),
            input_receive_enabled: Arc::new(AtomicBool::new(false)),
            clipboard_receive_enabled: Arc::new(AtomicBool::new(false)),
            transport_packets: Arc::new(AtomicU64::new(0)),
            input_events: Arc::new(AtomicU64::new(0)),
            clipboard_packets: Arc::new(AtomicU64::new(0)),
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
        if let Ok(mut current) = self.layout.lock() {
            *current = merge_disk_layout_into_runtime(disk_layout, &current);
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
        runtime.input_service = current_input_service_status();

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
        let layout_for_pairing = Arc::clone(&self.layout);
        let native_layout_for_input = self.native_layout();
        let input_receive_enabled = Arc::clone(&self.input_receive_enabled);
        let clipboard_receive_enabled = Arc::clone(&self.clipboard_receive_enabled);
        let clipboard_seen_text = Arc::clone(&self.clipboard_seen_text);
        let clipboard_echo_until = Arc::clone(&self.clipboard_echo_until);
        let clipboard_target = Arc::clone(&self.clipboard_target);
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
                return;
            }

            if !clipboard_receive_enabled.load(Ordering::Relaxed) {
                return;
            }
            let Ok(layout) = layout_for_clipboard.lock() else {
                return;
            };
            let current_peer = local_peer_from_layout(&layout);
            if handle_clipboard_packet(
                &payload,
                &layout,
                &current_peer.id,
                &clipboard_seen_text,
                &clipboard_echo_until,
            ) {
                transport_packets_for_stream.fetch_add(1, Ordering::Relaxed);
                clipboard_packets.fetch_add(1, Ordering::Relaxed);
            }
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
        sync_layout_peer_presence(&self.layout, &self.peers);

        thread::spawn(move || {
            let mut buffer = [0_u8; 4096];
            let mut last_announce = Instant::now() - Duration::from_secs(10);
            let mut last_input_ready = input_receive_enabled.load(Ordering::Relaxed);

            while !thread_stop.load(Ordering::Relaxed) {
                let current_input_ready = input_receive_enabled.load(Ordering::Relaxed);
                if last_announce.elapsed() >= Duration::from_secs(3)
                    || current_input_ready != last_input_ready
                {
                    let local_peer = layout_state
                        .lock()
                        .map(|layout| {
                            if !should_send_public_announce(&layout) {
                                return None;
                            }
                            let mut peer = local_peer_from_layout(&layout);
                            apply_transport_to_peer(&mut peer, &quic_transport);
                            peer.input_ready = advertised_input_ready(&layout, current_input_ready);
                            Some(peer)
                        })
                        .unwrap_or_else(|_| Some(local_peer.clone()));
                    if let Some(local_peer) = local_peer {
                        for target in &broadcast_targets {
                            let _ = send_discovery_packet(
                                &socket,
                                "announce",
                                &local_peer,
                                target.as_str(),
                            );
                        }
                    }
                    last_announce = Instant::now();
                    last_input_ready = current_input_ready;
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
                            }

                            if matches!(incoming.kind.as_str(), "announce" | "probe") {
                                let reply = should_reply_to_discovery(&current_layout, &incoming.peer);
                                log::info!(
                                    "discovery {} from {} id={} key={} cluster={} pairing_required={} -> reply={}",
                                    incoming.kind,
                                    source,
                                    incoming.peer.id,
                                    if incoming.peer.transport_public_key.is_empty() { "empty" } else { "set" },
                                    incoming.peer.cluster_id,
                                    incoming.peer.pairing_required,
                                    reply
                                );
                                if reply {
                                    let _ =
                                        send_discovery_packet(&socket, "reply", &current_peer, source);
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
            Arc::clone(&self.clipboard_target),
            Arc::clone(&self.input_events),
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
    incoming
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
    state.stop_discovery();
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

#[tauri::command]
fn start_runtime(state: tauri::State<'_, AppRuntime>) -> Result<RuntimeStatus, String> {
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

fn ready_transport_status(discovery: &DiscoveryStatus) -> NativeStageStatus {
    NativeStageStatus {
        state: "ready".into(),
        detail: format!(
            "UDP discovery is ready on {}; QUIC is ready on {} for input datagrams and clipboard streams.",
            discovery.port, discovery.local_peer.quic_port
        ),
    }
}

#[tauri::command]
fn stop_runtime(state: tauri::State<'_, AppRuntime>) -> Result<RuntimeStatus, String> {
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
        app.exit(0);
        Ok(())
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = (app, state);
        Err("Administrator restart is only available on Windows.".into())
    }
}

#[tauri::command]
fn read_input_service_status() -> InputServiceStatus {
    current_input_service_status()
}

#[tauri::command]
fn install_input_service() -> Result<InputServiceStatus, String> {
    #[cfg(target_os = "windows")]
    {
        let helper_path = resolve_input_helper_path()?;
        if is_windows_process_elevated().unwrap_or(false) {
            install_windows_input_service(&helper_path)?;
            start_windows_input_service()?;
            return Ok(current_input_service_status());
        }

        launch_current_process_as_admin(&[
            INSTALL_INPUT_SERVICE_ARG.into(),
            HELPER_PATH_ARG.into(),
            helper_path.to_string_lossy().into_owned(),
        ])?;
        Ok(InputServiceStatus {
            detail: "Administrator approval requested to install the input service.".into(),
            ..current_input_service_status()
        })
    }

    #[cfg(not(target_os = "windows"))]
    {
        Err("Windows input service is only available on Windows.".into())
    }
}

#[tauri::command]
fn uninstall_input_service() -> Result<InputServiceStatus, String> {
    #[cfg(target_os = "windows")]
    {
        if is_windows_process_elevated().unwrap_or(false) {
            uninstall_windows_input_service()?;
            return Ok(current_input_service_status());
        }

        launch_current_process_as_admin(&[UNINSTALL_INPUT_SERVICE_ARG.into()])?;
        Ok(InputServiceStatus {
            detail: "Administrator approval requested to uninstall the input service.".into(),
            ..current_input_service_status()
        })
    }

    #[cfg(not(target_os = "windows"))]
    {
        Err("Windows input service is only available on Windows.".into())
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
    read_system_clipboard()
}

#[tauri::command]
fn write_clipboard_text(text: String) -> Result<(), String> {
    write_system_clipboard(&text)
}

#[tauri::command]
fn read_performance_sample(state: tauri::State<'_, AppRuntime>) -> PerformanceSample {
    read_system_performance_sample(&state)
}

#[tauri::command]
fn scan_lan_peers(state: tauri::State<'_, AppRuntime>) -> Result<DiscoveryStatus, String> {
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
    let discovered = scan_for_peers(&local_peer, discovery_base_port(&layout))?;

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
    let peer = request_pairing_for_peer(&local_peer, &host, discovery_base_port(&layout))?;
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
    let peer = confirm_pairing_for_peer(
        &local_peer,
        &transport,
        &layout.pair_secret,
        &host,
        &code,
        discovery_base_port(&layout),
    )?;
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
                let _ = show_main_window_handle(&app);
            }
            InstanceEvent::Quit => {
                app.exit(0);
                break;
            }
        }
    });
}

fn request_app_quit(app: &AppHandle) {
    if let Some(state) = app.try_state::<AppRuntime>() {
        state.allow_explicit_quit.store(true, Ordering::Relaxed);
    }
    app.exit(0);
}

#[cfg(target_os = "macos")]
fn should_allow_macos_exit(app: &AppHandle, code: Option<i32>) -> bool {
    if code == Some(tauri::RESTART_EXIT_CODE) {
        return true;
    }

    app.try_state::<AppRuntime>()
        .map(|state| state.allow_explicit_quit.swap(false, Ordering::Relaxed))
        .unwrap_or(false)
}

#[cfg(target_os = "macos")]
static MACOS_APPKIT_CURSOR_HIDE_COUNT: AtomicU64 = AtomicU64::new(0);

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

/// Hide/unhide through AppKit without activating MyKVM. CoreGraphics cursor
/// hide/decouple APIs are foreground-sensitive; AppKit's cursor hide stack lets
/// us make the cursor invisible while the HID tap forwards movement to the
/// remote client, without raising the visible MyKVM window.
#[cfg(target_os = "macos")]
fn macos_set_cursor_hidden_with_appkit(hidden: bool) {
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
            if MACOS_APPKIT_CURSOR_HIDE_COUNT.load(Ordering::Relaxed) >= 128 {
                return;
            }
            MACOS_APPKIT_CURSOR_HIDE_COUNT.fetch_add(1, Ordering::Relaxed);
            sel_registerName(b"hide\0".as_ptr() as *const c_char)
        } else {
            let count = MACOS_APPKIT_CURSOR_HIDE_COUNT.swap(0, Ordering::Relaxed);
            let unhide_sel = sel_registerName(b"unhide\0".as_ptr() as *const c_char);
            let msg_void: extern "C" fn(*mut c_void, *mut c_void) =
                std::mem::transmute(objc_msgSend as *const ());
            for _ in 0..count {
                msg_void(class, unhide_sel);
            }
            return;
        };
        let msg_void: extern "C" fn(*mut c_void, *mut c_void) =
            std::mem::transmute(objc_msgSend as *const ());
        msg_void(class, selector);
    }
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
                macos_set_cursor_hidden_with_appkit(active);
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
            None::<Vec<&str>>,
        ))
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                set_main_window_visible(window.app_handle(), false);
                let _ = window.hide();
            }
        })
        .setup(|app| {
            if let Err(error) = app
                .handle()
                .plugin(tauri_plugin_updater::Builder::new().build())
            {
                eprintln!("failed to initialize updater plugin: {error}");
            }
            app.handle().plugin(
                tauri_plugin_log::Builder::default()
                    .level(log::LevelFilter::Info)
                    .build(),
            )?;

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
            setup_tray(app)?;
            #[cfg(target_os = "windows")]
            apply_custom_chrome(app.handle())?;
            setup_single_instance_events(app.handle().clone());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            load_app_state,
            read_runtime_status,
            save_layout,
            start_runtime,
            stop_runtime,
            read_clipboard_text,
            write_clipboard_text,
            read_performance_sample,
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
        .run(|app, event| {
            #[cfg(target_os = "macos")]
            {
                match event {
                    tauri::RunEvent::ExitRequested { code, api, .. } => {
                        if !should_allow_macos_exit(app, code) {
                            api.prevent_exit();
                            let _ = hide_main_window_handle(app);
                        }
                    }
                    tauri::RunEvent::Ready
                    | tauri::RunEvent::Reopen {
                        has_visible_windows: false,
                        ..
                    } => {
                        let _ = show_main_window_handle(app);
                    }
                    _ => {}
                }
            }

            #[cfg(not(target_os = "macos"))]
            let _ = (app, event);
        });
}

fn setup_tray(app: &tauri::App) -> tauri::Result<()> {
    let show_item = MenuItem::with_id(app, "show", "Show mykvm", true, None::<&str>)?;
    let hide_item = MenuItem::with_id(app, "hide", "Hide to tray", true, None::<&str>)?;
    let quit_item = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show_item, &hide_item, &quit_item])?;

    let mut tray = TrayIconBuilder::with_id("main")
        .menu(&menu)
        .tooltip("mykvm")
        .show_menu_on_left_click(true)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "show" => {
                let _ = show_main_window_handle(app);
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
    let window = app
        .get_webview_window("main")
        .ok_or_else(|| "main window is not available".to_string())?;
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
    let window = app
        .get_webview_window("main")
        .ok_or_else(|| "main window is not available".to_string())?;
    let result = window
        .hide()
        .map_err(|error| format!("failed to hide main window: {error}"));

    if result.is_ok() {
        set_main_window_visible(app, false);
    }

    result
}

fn set_main_window_visible(app: &AppHandle, visible: bool) {
    if let Some(state) = app.try_state::<AppRuntime>() {
        state.main_window_visible.store(visible, Ordering::Relaxed);
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
        Foundation::{GetLastError, ERROR_SERVICE_DOES_NOT_EXIST},
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
            if code == ERROR_SERVICE_DOES_NOT_EXIST {
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
        let binary = wide_null(&format!("{} --service", quote_windows_arg(helper_path)));
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
    use std::time::{Duration, Instant};
    use windows_sys::Win32::{
        Foundation::{GetLastError, ERROR_SERVICE_DOES_NOT_EXIST},
        Storage::FileSystem::DELETE,
        System::Services::{
            ControlService, DeleteService, OpenSCManagerW, OpenServiceW, SC_MANAGER_CONNECT,
            SERVICE_CONTROL_STOP, SERVICE_QUERY_STATUS, SERVICE_STATUS, SERVICE_STOP,
            SERVICE_STOPPED,
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

        if let Ok(status) = query_service_status_process_for_uninstall(service) {
            if status.dwCurrentState != SERVICE_STOPPED {
                let mut stop_status = SERVICE_STATUS::default();
                let _ = ControlService(service, SERVICE_CONTROL_STOP, &mut stop_status);
                let deadline = Instant::now() + Duration::from_secs(8);
                while Instant::now() < deadline {
                    if let Ok(status) = query_service_status_process_for_uninstall(service) {
                        if status.dwCurrentState == SERVICE_STOPPED {
                            break;
                        }
                    }
                    thread::sleep(Duration::from_millis(200));
                }
            }
        }

        if DeleteService(service) == 0 {
            return Err(windows_last_error("DeleteService"));
        }
        return Ok(());
    }

    unsafe fn query_service_status_process_for_uninstall(
        service: windows_sys::Win32::System::Services::SC_HANDLE,
    ) -> Result<windows_sys::Win32::System::Services::SERVICE_STATUS_PROCESS, String> {
        use windows_sys::Win32::System::Services::{
            QueryServiceStatusEx, SC_STATUS_PROCESS_INFO, SERVICE_STATUS_PROCESS,
        };
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
fn quote_windows_arg(value: &PathBuf) -> String {
    quote_windows_arg_str(&value.to_string_lossy())
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
fn wide_null(value: &str) -> Vec<u16> {
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
        language: default_language(),
        theme_mode: default_theme_mode(),
        performance_monitor: default_performance_monitor(),
        transport_port_mode: default_transport_port_mode(),
        transport_port,
        quic_port,
        modifier_remap: default_modifier_remap(),
        modifier_map: default_modifier_map(),
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
        language: default_language(),
        theme_mode: default_theme_mode(),
        performance_monitor: default_performance_monitor(),
        transport_port_mode: default_transport_port_mode(),
        transport_port: default_transport_port(),
        quic_port: preferred_quic_port(default_transport_port()),
        modifier_remap: default_modifier_remap(),
        modifier_map: default_modifier_map(),
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
        language: normalize_language(&saved_layout.language),
        theme_mode: normalize_theme_mode(&saved_layout.theme_mode),
        performance_monitor: saved_layout.performance_monitor,
        transport_port_mode: normalize_transport_port_mode(&saved_layout.transport_port_mode),
        transport_port,
        quic_port: normalize_quic_port(transport_port, saved_layout.quic_port),
        modifier_remap: saved_layout.modifier_remap,
        modifier_map: normalize_modifier_map(&saved_layout.modifier_map),
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
    cluster_id: String,
    #[serde(default)]
    pair_secret: String,
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

/// A bitmap copied to the clipboard, carried as base64-encoded RGBA8 plus its
/// dimensions. RGBA is what `arboard` hands us and expects back, so no image
/// codec is needed on either end.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClipboardImage {
    width: u32,
    height: u32,
    rgba_base64: String,
}

/// One unit of clipboard content read from (or written to) the local system.
enum ClipboardContent {
    Text(String),
    Image(ClipboardImage),
}

impl ClipboardContent {
    fn is_oversized(&self) -> bool {
        match self {
            ClipboardContent::Text(text) => text.len() > CLIPBOARD_MAX_TEXT_BYTES,
            ClipboardContent::Image(image) => {
                // base64 inflates ~4/3; compare against the decoded RGBA budget.
                image.rgba_base64.len() / 4 * 3 > CLIPBOARD_MAX_IMAGE_BYTES
            }
        }
    }

    /// A stable, cheap fingerprint used to detect "did the clipboard change"
    /// and to suppress echoing content we just received from a peer.
    fn signature(&self) -> String {
        match self {
            ClipboardContent::Text(text) => format!("text:{text}"),
            ClipboardContent::Image(image) => {
                format!(
                    "image:{}x{}:{}",
                    image.width,
                    image.height,
                    image.rgba_base64.len()
                )
            }
        }
    }

    fn into_packet(
        self,
        origin_id: String,
        cluster_id: String,
        pair_secret: String,
        sequence: u64,
    ) -> ClipboardPacket {
        match self {
            ClipboardContent::Text(text) => ClipboardPacket {
                protocol: CLIPBOARD_PROTOCOL.into(),
                origin_id,
                cluster_id,
                pair_secret,
                text,
                image: None,
                sequence,
            },
            ClipboardContent::Image(image) => ClipboardPacket {
                protocol: CLIPBOARD_PROTOCOL.into(),
                origin_id,
                cluster_id,
                pair_secret,
                text: String::new(),
                image: Some(image),
                sequence,
            },
        }
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

        if last_poll.elapsed() < Duration::from_millis(500) {
            thread::sleep(Duration::from_millis(40));
            continue;
        }
        last_poll = Instant::now();

        // Within the grace window after writing peer content, don't send. We do
        // read once and record the signature: the OS can hand a bitmap back with
        // slightly different bytes than we wrote, so learning the actual
        // read-back signature here lets the echo check below recognize it once
        // the window lifts instead of bouncing it back to the peer.
        if clipboard_echo_active(&clipboard_echo_until) {
            if let Some(content) = read_clipboard_content() {
                if let Ok(mut seen) = clipboard_seen_text.lock() {
                    *seen = Some(content.signature());
                }
            }
            continue;
        }

        let Some(content) = read_clipboard_content() else {
            continue;
        };
        if content.is_oversized() {
            continue;
        }
        let signature = content.signature();

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
        let packet = content.into_packet(
            local_peer_id.clone(),
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
            if quic_transport.send_stream(peer, payload).is_ok() {
                transport_packets.fetch_add(1, Ordering::Relaxed);
                clipboard_packets.fetch_add(1, Ordering::Relaxed);
                last_failed = None;
                last_sent = Some((target.device_id, target.addr, signature));
            } else {
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

fn handle_clipboard_packet(
    payload: &[u8],
    layout: &LayoutState,
    local_peer_id: &str,
    clipboard_seen_text: &Arc<Mutex<Option<String>>>,
    clipboard_echo_until: &Arc<Mutex<Option<Instant>>>,
) -> bool {
    let Some(packet) = decode_wire_packet::<ClipboardPacket>(payload) else {
        return false;
    };

    if packet.protocol != CLIPBOARD_PROTOCOL {
        return false;
    }

    if !clipboard_packet_authorized(layout, &packet) {
        return true;
    }

    if packet.origin_id == local_peer_id {
        return true;
    }

    let content = if let Some(image) = packet.image {
        Some(ClipboardContent::Image(image))
    } else if !packet.text.is_empty() {
        Some(ClipboardContent::Text(packet.text))
    } else {
        None
    };

    let Some(content) = content else {
        return true;
    };
    if content.is_oversized() {
        return true;
    }

    let signature = content.signature();
    let written = match &content {
        ClipboardContent::Text(text) => write_system_clipboard(text).is_ok(),
        ClipboardContent::Image(image) => write_clipboard_image(image).is_ok(),
    };

    if written {
        // Remember what we just wrote so our own poll loop recognizes it as an
        // echo (signature match) and arm the time-based guard as a backstop in
        // case the OS hands the bitmap back to us with slightly different bytes.
        if let Ok(mut seen) = clipboard_seen_text.lock() {
            *seen = Some(signature);
        }
        arm_clipboard_echo_guard(clipboard_echo_until);
    }

    true
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
    for device in &mut layout.devices {
        if device.role == "local" {
            device.online = true;
            device.input_ready = false;
            device.transport_port = local_transport_port;
            device.quic_port = local_quic_port;
            device.protocol_version = quic_transport::PROTOCOL_VERSION;
            continue;
        }

        let peer = peers.iter().find(|peer| device_matches_peer(device, peer));
        if let Some(peer) = peer {
            update_device_from_peer(device, peer);
        } else {
            device.online = false;
            device.input_ready = false;
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

fn device_matches_peer(device: &Device, peer: &LanPeer) -> bool {
    device.id == peer_device_id(peer)
        || (!device.transport_public_key.trim().is_empty()
            && device.transport_public_key == peer.transport_public_key)
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
    device.online = peer.input_ready;
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

#[cfg(target_os = "windows")]
fn read_system_clipboard() -> Result<String, String> {
    let mut clipboard =
        arboard::Clipboard::new().map_err(|error| format!("failed to open clipboard: {error}"))?;
    clipboard
        .get_text()
        .map_err(|error| format!("failed to read clipboard text: {error}"))
}

#[cfg(not(target_os = "windows"))]
fn read_system_clipboard() -> Result<String, String> {
    let output = if cfg!(target_os = "macos") {
        Command::new("pbpaste").output()
    } else {
        Command::new("sh")
            .args([
                "-c",
                "wl-paste -n 2>/dev/null || xclip -selection clipboard -out",
            ])
            .output()
    }
    .map_err(|error| format!("failed to read clipboard: {error}"))?;

    if output.status.success() {
        String::from_utf8(output.stdout)
            .map_err(|error| format!("clipboard text is not valid UTF-8: {error}"))
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

#[cfg(target_os = "windows")]
fn write_system_clipboard(text: &str) -> Result<(), String> {
    let mut clipboard =
        arboard::Clipboard::new().map_err(|error| format!("failed to open clipboard: {error}"))?;
    clipboard
        .set_text(text.to_string())
        .map_err(|error| format!("failed to write clipboard text: {error}"))
}

#[cfg(not(target_os = "windows"))]
fn write_system_clipboard(text: &str) -> Result<(), String> {
    let mut child = if cfg!(target_os = "macos") {
        Command::new("pbcopy").stdin(Stdio::piped()).spawn()
    } else {
        Command::new("sh")
            .args(["-c", "wl-copy 2>/dev/null || xclip -selection clipboard"])
            .stdin(Stdio::piped())
            .spawn()
    }
    .map_err(|error| format!("failed to write clipboard: {error}"))?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(text.as_bytes())
            .map_err(|error| format!("failed to send clipboard text: {error}"))?;
    }

    let status = child
        .wait()
        .map_err(|error| format!("failed to finish clipboard write: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("clipboard command exited with status {status}"))
    }
}

/// Reads whatever is currently on the clipboard, preferring an image when one
/// is present and otherwise falling back to text. Returns `None` when the
/// clipboard is empty or unreadable.
fn read_clipboard_content() -> Option<ClipboardContent> {
    if let Some(image) = read_clipboard_image() {
        return Some(ClipboardContent::Image(image));
    }
    match read_system_clipboard() {
        Ok(text) if !text.is_empty() => Some(ClipboardContent::Text(text)),
        _ => None,
    }
}

/// Reads a bitmap from the system clipboard via `arboard`. `get_image` returns
/// an error (not an image) when the clipboard holds text, so callers should try
/// this first and fall back to text.
fn read_clipboard_image() -> Option<ClipboardImage> {
    use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};

    let mut clipboard = arboard::Clipboard::new().ok()?;
    let image = clipboard.get_image().ok()?;
    if image.width == 0 || image.height == 0 || image.bytes.is_empty() {
        return None;
    }
    if image.bytes.len() > CLIPBOARD_MAX_IMAGE_BYTES {
        return None;
    }

    Some(ClipboardImage {
        width: image.width as u32,
        height: image.height as u32,
        rgba_base64: BASE64.encode(image.bytes.as_ref()),
    })
}

/// Writes a received bitmap to the system clipboard via `arboard`.
fn write_clipboard_image(image: &ClipboardImage) -> Result<(), String> {
    use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};

    let bytes = BASE64
        .decode(image.rgba_base64.as_bytes())
        .map_err(|error| format!("failed to decode clipboard image: {error}"))?;
    let width = image.width as usize;
    let height = image.height as usize;
    if width == 0 || height == 0 || bytes.len() != width.saturating_mul(height).saturating_mul(4) {
        return Err("clipboard image has invalid dimensions".into());
    }

    let mut clipboard =
        arboard::Clipboard::new().map_err(|error| format!("failed to open clipboard: {error}"))?;
    clipboard
        .set_image(arboard::ImageData {
            width,
            height,
            bytes: std::borrow::Cow::Owned(bytes),
        })
        .map_err(|error| format!("failed to write clipboard image: {error}"))
}

fn read_system_performance_sample(state: &AppRuntime) -> PerformanceSample {
    let (app_cpu_percent, app_memory_mb) = if cfg!(target_os = "windows") {
        read_windows_process_performance().unwrap_or((0.0, 0.0))
    } else {
        read_unix_process_performance().unwrap_or((0.0, 0.0))
    };

    PerformanceSample {
        timestamp_ms: now_ms(),
        app_cpu_percent: app_cpu_percent.clamp(0.0, 100.0),
        app_memory_mb: app_memory_mb.max(0.0),
        transport_packets: state.transport_packets.load(Ordering::Relaxed),
        input_events: state.input_events.load(Ordering::Relaxed),
        clipboard_packets: state.clipboard_packets.load(Ordering::Relaxed),
    }
}

fn read_unix_process_performance() -> Result<(f64, f64), String> {
    let pid = std::process::id().to_string();
    let output = command_stdout(Command::new("ps").args(["-p", &pid, "-o", "%cpu=,rss="]))?;
    parse_process_metrics(&output)
}

#[cfg(target_os = "windows")]
fn read_windows_process_performance() -> Result<(f64, f64), String> {
    use windows_sys::Win32::{
        Foundation::FILETIME,
        System::{
            ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS},
            Threading::{GetCurrentProcess, GetProcessTimes},
        },
    };

    let process = unsafe { GetCurrentProcess() };
    let mut counters = PROCESS_MEMORY_COUNTERS {
        cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    let memory_ok = unsafe { GetProcessMemoryInfo(process, &mut counters, counters.cb) };
    if memory_ok == 0 {
        return Err("failed to read process memory counters".into());
    }

    let mut creation_time = FILETIME::default();
    let mut exit_time = FILETIME::default();
    let mut kernel_time = FILETIME::default();
    let mut user_time = FILETIME::default();
    let time_ok = unsafe {
        GetProcessTimes(
            process,
            &mut creation_time,
            &mut exit_time,
            &mut kernel_time,
            &mut user_time,
        )
    };
    if time_ok == 0 {
        return Err("failed to read process cpu counters".into());
    }

    let now = Instant::now();
    let process_time_100ns = filetime_to_u64(&kernel_time) + filetime_to_u64(&user_time);
    let cpu_percent = {
        let sample = WINDOWS_PROCESS_SAMPLE.get_or_init(|| Mutex::new(None));
        let mut previous = sample
            .lock()
            .map_err(|_| "windows process sample lock poisoned".to_string())?;
        let cpu_percent = previous
            .map(|previous_sample| {
                let process_delta =
                    process_time_100ns.saturating_sub(previous_sample.process_time_100ns);
                let elapsed_100ns =
                    now.duration_since(previous_sample.instant).as_secs_f64() * 10_000_000.0;
                let cpu_count = std::thread::available_parallelism()
                    .map(|count| count.get())
                    .unwrap_or(1) as f64;

                if elapsed_100ns > 0.0 {
                    (process_delta as f64 / elapsed_100ns / cpu_count) * 100.0
                } else {
                    0.0
                }
            })
            .unwrap_or(0.0);
        *previous = Some(WindowsProcessSample {
            instant: now,
            process_time_100ns,
        });
        cpu_percent
    };

    Ok((
        cpu_percent,
        counters.WorkingSetSize as f64 / 1024.0 / 1024.0,
    ))
}

#[cfg(not(target_os = "windows"))]
fn read_windows_process_performance() -> Result<(f64, f64), String> {
    Err("windows process performance is unavailable on this platform".into())
}

fn parse_process_metrics(output: &str) -> Result<(f64, f64), String> {
    let values = output
        .trim()
        .split(|character: char| character == ',' || character.is_whitespace())
        .filter(|value| !value.trim().is_empty())
        .map(|value| value.trim().parse::<f64>().unwrap_or(0.0))
        .collect::<Vec<_>>();

    if values.len() >= 2 {
        Ok((
            values[0],
            values[1]
                / if cfg!(target_os = "windows") {
                    1.0
                } else {
                    1024.0
                },
        ))
    } else {
        Err("performance command did not return process cpu and memory".into())
    }
}

#[cfg(target_os = "windows")]
fn filetime_to_u64(filetime: &windows_sys::Win32::Foundation::FILETIME) -> u64 {
    ((filetime.dwHighDateTime as u64) << 32) | filetime.dwLowDateTime as u64
}

#[allow(dead_code)]
fn read_system_overview_performance() -> PerformanceSample {
    let (app_cpu_percent, app_memory_mb, _memory_total_mb) = if cfg!(target_os = "macos") {
        read_macos_performance().unwrap_or((0.0, 0.0, 0.0))
    } else if cfg!(target_os = "windows") {
        read_windows_performance().unwrap_or((0.0, 0.0, 0.0))
    } else {
        read_linux_performance().unwrap_or((0.0, 0.0, 0.0))
    };

    PerformanceSample {
        timestamp_ms: now_ms(),
        app_cpu_percent: app_cpu_percent.clamp(0.0, 100.0),
        app_memory_mb,
        transport_packets: 0,
        input_events: 0,
        clipboard_packets: 0,
    }
}

fn read_macos_performance() -> Result<(f64, f64, f64), String> {
    let cpu_total = command_stdout(
        Command::new("sh").args(["-c", "ps -A -o %cpu= | awk '{s+=$1} END{print s+0}'"]),
    )?
    .trim()
    .parse::<f64>()
    .unwrap_or(0.0);
    let cpu_count = command_stdout(Command::new("sysctl").args(["-n", "hw.logicalcpu"]))?
        .trim()
        .parse::<f64>()
        .unwrap_or(1.0)
        .max(1.0);
    let total_bytes = command_stdout(Command::new("sysctl").args(["-n", "hw.memsize"]))?
        .trim()
        .parse::<f64>()
        .unwrap_or(0.0);
    let vm_stat = command_stdout(&mut Command::new("vm_stat"))?;
    let page_size = vm_stat
        .lines()
        .next()
        .and_then(|line| line.split("page size of ").nth(1))
        .and_then(|value| value.split_whitespace().next())
        .and_then(|value| value.parse::<f64>().ok())
        .unwrap_or(4096.0);
    let free_pages = vm_stat_pages(&vm_stat, "Pages free")
        + vm_stat_pages(&vm_stat, "Pages inactive")
        + vm_stat_pages(&vm_stat, "Pages speculative");
    let total_mb = total_bytes / 1024.0 / 1024.0;
    let free_mb = free_pages * page_size / 1024.0 / 1024.0;
    let used_mb = (total_mb - free_mb).max(0.0);

    Ok((cpu_total / cpu_count, used_mb, total_mb))
}

fn read_windows_performance() -> Result<(f64, f64, f64), String> {
    let output = command_stdout(Command::new("powershell").args([
    "-NoProfile",
    "-Command",
    "$cpu=(Get-CimInstance Win32_Processor | Measure-Object -Property LoadPercentage -Average).Average; $os=Get-CimInstance Win32_OperatingSystem; $total=[math]::Round($os.TotalVisibleMemorySize/1024,2); $free=[math]::Round($os.FreePhysicalMemory/1024,2); Write-Output \"$cpu,$($total-$free),$total\"",
  ]))?;
    parse_metric_triplet(&output)
}

fn read_linux_performance() -> Result<(f64, f64, f64), String> {
    let output = command_stdout(Command::new("sh").args([
    "-c",
    "cpu=$(top -bn1 | awk '/Cpu\\(s\\)/ {print 100-$8; exit}'); mem=$(awk '/MemTotal/ {t=$2} /MemAvailable/ {a=$2} END {printf \"%.2f,%.2f\", (t-a)/1024, t/1024}' /proc/meminfo); echo \"$cpu,$mem\"",
  ]))?;
    parse_metric_triplet(&output)
}

fn command_stdout(command: &mut Command) -> Result<String, String> {
    let output = command
        .output()
        .map_err(|error| format!("failed to run performance command: {error}"))?;
    if output.status.success() {
        String::from_utf8(output.stdout)
            .map_err(|error| format!("performance command returned invalid UTF-8: {error}"))
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

fn parse_metric_triplet(output: &str) -> Result<(f64, f64, f64), String> {
    let values = output
        .trim()
        .split(',')
        .map(|value| value.trim().parse::<f64>().unwrap_or(0.0))
        .collect::<Vec<_>>();
    if values.len() >= 3 {
        Ok((values[0], values[1], values[2]))
    } else {
        Err("performance command did not return cpu, memory used, memory total".into())
    }
}

fn vm_stat_pages(vm_stat: &str, label: &str) -> f64 {
    vm_stat
        .lines()
        .find(|line| line.trim_start().starts_with(label))
        .and_then(|line| line.split(':').nth(1))
        .map(|value| value.trim().trim_end_matches('.').replace('.', ""))
        .and_then(|value| value.parse::<f64>().ok())
        .unwrap_or(0.0)
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
        .send_stream(endpoint, payload)
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
        return true;
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
        }
        Err(error) => {
            log::warn!("pairing confirmation rejected: {error}");
        }
    }

    true
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
    let subnet_prefix = local_ip_address().and_then(|ip| {
        let parts = ip.split('.').collect::<Vec<_>>();
        (parts.len() == 4).then(|| format!("{}.{}.{}", parts[0], parts[1], parts[2]))
    });

    let mut addresses = Vec::new();
    for port in discovery_target_ports(base_port) {
        addresses.push(format!("255.255.255.255:{port}"));
        if let Some(prefix) = &subnet_prefix {
            addresses.push(format!("{prefix}.255:{port}"));
        }
    }

    addresses.sort();
    addresses.dedup();
    addresses
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
    let Some(ip) = local_ip_address() else {
        return Vec::new();
    };
    let parts = ip.split('.').collect::<Vec<_>>();
    if parts.len() != 4 {
        return Vec::new();
    }
    let self_host = parts[3].parse::<u8>().unwrap_or(0);
    (1..=254u8)
        .filter(|host| *host != self_host)
        .map(|host| format!("{}.{}.{}.{}:{}", parts[0], parts[1], parts[2], host, port))
        .collect()
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

fn now_ms() -> u64 {
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
            language: "cn".into(),
            theme_mode: "system".into(),
            performance_monitor: false,
            transport_port_mode: "auto".into(),
            transport_port: 49152,
            quic_port: 49153,
            modifier_remap: true,
            modifier_map: default_modifier_map(),
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
    fn peer_presence_requires_input_ready_for_online() {
        let mut layout = test_layout();
        let mut peer = test_peer();
        peer.input_ready = false;

        apply_peer_presence(&mut layout, &[peer]);

        assert!(!layout.devices[1].online);
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
            cluster_id: layout.cluster_id.clone(),
            pair_secret: layout.pair_secret.clone(),
            text: "hello".into(),
            image: None,
            sequence: 1,
        };

        assert!(!clipboard_packet_authorized(&layout, &packet));
        packet.origin_id = "server-10-0-0-1".into();
        assert!(clipboard_packet_authorized(&layout, &packet));
    }

    #[test]
    fn device_matching_rejects_same_host_with_different_identity() {
        let layout = test_layout();
        let device = &layout.devices[1];
        let mut peer = test_peer();
        peer.id = "peer-other".into();
        peer.transport_public_key = "different-key".into();

        assert!(!device_matches_peer(device, &peer));
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
