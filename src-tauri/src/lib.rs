use std::{
    env, fs,
    io::Write,
    net::UdpSocket,
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex, OnceLock,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Manager, Monitor, WindowEvent,
};

mod input;
mod quic_transport;

const DISCOVERY_PORT: u16 = 47833;
const TRANSPORT_PORT_MIN: u16 = 1024;
const TRANSPORT_PORT_MAX: u16 = 65_535;
const REPOSITORY_URL: &str = "https://github.com/XxMinor/mykvm";
const RELEASES_URL: &str = "https://github.com/XxMinor/mykvm/releases/latest";
const DISCOVERY_PROTOCOL: &str = "mykvm.discovery.v1";
const PEER_TTL_MS: u64 = 30_000;
const MAX_DISCOVERY_PEERS: usize = 128;
const CLIPBOARD_PROTOCOL: &str = "mykvm.clipboard.v1";
const CLIPBOARD_MAX_TEXT_BYTES: usize = 256 * 1024;
const REMOTE_SCREEN_GAP: i32 = 0;
const DEVICE_COLORS: [&str; 6] = [
    "#2f7af8", "#0f766e", "#b45309", "#7c3aed", "#be123c", "#0891b2",
];

static HOSTNAME_CACHE: OnceLock<Option<String>> = OnceLock::new();

#[cfg(target_os = "windows")]
static WINDOWS_PROCESS_SAMPLE: OnceLock<Mutex<Option<WindowsProcessSample>>> = OnceLock::new();

#[cfg(target_os = "windows")]
#[derive(Clone, Copy)]
struct WindowsProcessSample {
    instant: Instant,
    process_time_100ns: u64,
}

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
struct RuntimeStatus {
    started: bool,
    transport: NativeStageStatus,
    capture: NativeStageStatus,
    inject: NativeStageStatus,
    clipboard: NativeStageStatus,
    discovery: DiscoveryStatus,
    privilege: PrivilegeStatus,
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
struct PerformanceSample {
    timestamp_ms: u64,
    app_cpu_percent: f64,
    app_memory_mb: f64,
    transport_packets: u64,
    input_events: u64,
    clipboard_packets: u64,
}

struct AppRuntime {
    layout: Arc<Mutex<LayoutState>>,
    native_layout: Mutex<LayoutState>,
    runtime: Mutex<RuntimeStatus>,
    peers: Arc<Mutex<Vec<LanPeer>>>,
    quic_transport: Mutex<Option<quic_transport::TransportHandle>>,
    discovery_stop: Mutex<Option<Arc<AtomicBool>>>,
    input_stop: Mutex<Option<Arc<AtomicBool>>>,
    clipboard_stop: Mutex<Option<Arc<AtomicBool>>>,
    clipboard_seen_text: Arc<Mutex<Option<String>>>,
    remote_input_active: Arc<AtomicBool>,
    clipboard_target: Arc<Mutex<Option<input::ClipboardTarget>>>,
    input_receive_enabled: Arc<AtomicBool>,
    clipboard_receive_enabled: Arc<AtomicBool>,
    transport_packets: Arc<AtomicU64>,
    input_events: Arc<AtomicU64>,
    clipboard_packets: Arc<AtomicU64>,
    config_path: PathBuf,
}

impl AppRuntime {
    fn new(config_path: PathBuf, detected_layout: LayoutState) -> Self {
        let layout = load_layout_from_disk(&config_path)
            .map(|saved_layout| normalize_saved_layout(saved_layout, detected_layout.clone()))
            .unwrap_or_else(|| detected_layout.clone());
        Self {
            layout: Arc::new(Mutex::new(layout)),
            native_layout: Mutex::new(detected_layout.clone()),
            runtime: Mutex::new(default_runtime(&detected_layout)),
            peers: Arc::new(Mutex::new(Vec::new())),
            quic_transport: Mutex::new(None),
            discovery_stop: Mutex::new(None),
            input_stop: Mutex::new(None),
            clipboard_stop: Mutex::new(None),
            clipboard_seen_text: Arc::new(Mutex::new(None)),
            remote_input_active: Arc::new(AtomicBool::new(false)),
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

    fn runtime_status(&self) -> RuntimeStatus {
        let layout = self.layout_snapshot();

        self.runtime_status_for_layout(&layout)
    }

    fn runtime_status_for_layout(&self, layout: &LayoutState) -> RuntimeStatus {
        let mut runtime = self.runtime.lock().unwrap().clone();
        runtime.discovery = self.discovery_status_for_layout(layout);
        runtime.clipboard = self.clipboard_status(layout);
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
        local_peer.input_ready = self.input_receive_enabled.load(Ordering::Relaxed);
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
        let input_receive_enabled = Arc::clone(&self.input_receive_enabled);
        let clipboard_receive_enabled = Arc::clone(&self.clipboard_receive_enabled);
        let clipboard_seen_text = Arc::clone(&self.clipboard_seen_text);
        let clipboard_target = Arc::clone(&self.clipboard_target);
        let transport_packets_for_input = Arc::clone(&self.transport_packets);
        let transport_packets_for_clipboard = Arc::clone(&self.transport_packets);
        let input_events = Arc::clone(&self.input_events);
        let clipboard_packets = Arc::clone(&self.clipboard_packets);

        let on_datagram = Arc::new(move |payload: Vec<u8>, source| {
            if !input_receive_enabled.load(Ordering::Relaxed) {
                return;
            }
            let Ok(layout) = layout_for_input.lock() else {
                return;
            };
            let current_peer = local_peer_from_layout(&layout);
            if input::try_inject_packet_from_source(
                &layout,
                &payload,
                source,
                &input_events,
                &current_peer.id,
                &clipboard_target,
            ) {
                transport_packets_for_input.fetch_add(1, Ordering::Relaxed);
            }
        });

        let on_stream = Arc::new(move |payload: Vec<u8>, _source| {
            if !clipboard_receive_enabled.load(Ordering::Relaxed) {
                return;
            }
            let Ok(layout) = layout_for_clipboard.lock() else {
                return;
            };
            let current_peer = local_peer_from_layout(&layout);
            if handle_clipboard_packet(&payload, &current_peer.id, &clipboard_seen_text) {
                transport_packets_for_clipboard.fetch_add(1, Ordering::Relaxed);
                clipboard_packets.fetch_add(1, Ordering::Relaxed);
            }
        });

        let transport = quic_transport::start(preferred_port, on_datagram, on_stream)?;
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
        local_peer.input_ready = self.input_receive_enabled.load(Ordering::Relaxed);
        let peers = Arc::clone(&self.peers);
        let layout_state = Arc::clone(&self.layout);
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
        let broadcast_targets = broadcast_addrs(actual_port);
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
                            let mut peer = local_peer_from_layout(&layout);
                            apply_transport_to_peer(&mut peer, &quic_transport);
                            peer.input_ready = current_input_ready;
                            peer
                        })
                        .unwrap_or_else(|_| local_peer.clone());
                    for target in &broadcast_targets {
                        let _ = send_discovery_packet(
                            &socket,
                            "announce",
                            &local_peer,
                            target.as_str(),
                        );
                    }
                    last_announce = Instant::now();
                    last_input_ready = current_input_ready;
                }

                if let Ok((length, source)) = socket.recv_from(&mut buffer) {
                    transport_packets.fetch_add(1, Ordering::Relaxed);
                    let payload = &buffer[..length];

                    if let Some(packet) = decode_discovery_packet(payload) {
                        let current_peer = layout_state
                            .lock()
                            .map(|layout| {
                                let mut peer = local_peer_from_layout(&layout);
                                apply_transport_to_peer(&mut peer, &quic_transport);
                                peer.input_ready = input_receive_enabled.load(Ordering::Relaxed);
                                peer
                            })
                            .unwrap_or_else(|_| local_peer.clone());

                        if let Some((kind, peer)) = peer_from_discovery_packet(
                            packet,
                            source.ip().to_string(),
                            &current_peer.id,
                        ) {
                            merge_peer(&peers, peer);
                            sync_layout_peer_presence(&layout_state, &peers);

                            if matches!(kind.as_str(), "announce" | "probe") {
                                let _ =
                                    send_discovery_packet(&socket, "reply", &current_peer, source);
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
                detail: "剪贴板同步已开启，仅在鼠标切到远端设备后惰性发送文本剪贴板。".into(),
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
    let _ = state.start_discovery();
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
) -> Result<LayoutState, String> {
    write_layout_to_disk(&state.config_path, &layout)?;
    let previous_layout = {
        let mut stored_layout = state
            .layout
            .lock()
            .map_err(|_| "layout state lock poisoned".to_string())?;
        let previous_layout = stored_layout.clone();
        *stored_layout = layout.clone();
        previous_layout
    };

    if runtime_relevant_layout_changed(&previous_layout, &layout) {
        if previous_layout.transport_port_mode != layout.transport_port_mode
            || previous_layout.transport_port != layout.transport_port
        {
            state.stop_discovery();
            thread::sleep(Duration::from_millis(200));
        }
        restart_runtime_services_if_running(&state, layout.clone())?;
        if !state
            .runtime
            .lock()
            .map_err(|_| "runtime state lock poisoned".to_string())?
            .started
        {
            state.start_discovery()?;
        }
    }
    Ok(layout)
}

fn runtime_relevant_layout_changed(previous: &LayoutState, next: &LayoutState) -> bool {
    previous.devices != next.devices
        || previous.input_mode != next.input_mode
        || previous.machine_role != next.machine_role
        || previous.clipboard_sync != next.clipboard_sync
        || previous.transport_port_mode != next.transport_port_mode
        || previous.transport_port != next.transport_port
}

fn restart_runtime_services_if_running(
    state: &AppRuntime,
    layout: LayoutState,
) -> Result<(), String> {
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
    let (capture, inject) = state.start_input(layout.clone());
    let clipboard = state.start_clipboard(layout);
    let mut runtime = state
        .runtime
        .lock()
        .map_err(|_| "runtime state lock poisoned".to_string())?;

    runtime.capture = capture;
    runtime.inject = inject;
    runtime.clipboard = clipboard;
    Ok(())
}

#[tauri::command]
fn start_runtime(state: tauri::State<'_, AppRuntime>) -> Result<RuntimeStatus, String> {
    let discovery_error = state.start_discovery().err();
    let layout = state.layout_snapshot();
    let mut discovery = state.discovery_status();
    if let Some(error) = discovery_error {
        discovery.state = "error".into();
        discovery.detail = error;
    }
    let (capture, inject) = state.start_input(layout.clone());
    let clipboard = state.start_clipboard(layout);

    let mut runtime = state
        .runtime
        .lock()
        .map_err(|_| "runtime state lock poisoned".to_string())?;

    *runtime = RuntimeStatus {
        started: true,
        transport: NativeStageStatus {
            state: "ready".into(),
            detail: format!(
                "UDP discovery is ready on {}; QUIC is ready on {} for input datagrams and clipboard streams.",
                discovery.port, discovery.local_peer.quic_port
            ),
        },
        capture,
        inject,
        clipboard,
        discovery,
        privilege: current_privilege_status(),
    };

    Ok(runtime.clone())
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
    *runtime = stopped_runtime;
    Ok(runtime.clone())
}

#[tauri::command]
fn restart_as_admin(app: AppHandle) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        if is_windows_process_elevated().unwrap_or(false) {
            return Ok(());
        }

        restart_current_process_as_admin()?;
        app.exit(0);
        Ok(())
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = app;
        Err("Administrator restart is only available on Windows.".into())
    }
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
    app.get_webview_window("main")
        .ok_or_else(|| "main window is not available".to_string())?
        .minimize()
        .map_err(|error| format!("failed to minimize main window: {error}"))
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
    let discovered = scan_for_peers(&local_peer)?;

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
    let peer = probe_for_peer(&local_peer, &host)?;
    merge_peer(&state.peers, peer.clone());
    sync_layout_peer_presence(&state.layout, &state.peers);
    Ok(peer)
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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_process::init())
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
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
            app.manage(AppRuntime::new(
                config_dir.join("layout.json"),
                detected_layout,
            ));
            setup_tray(app)?;
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            apply_custom_chrome(app.handle())?;
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
            restart_as_admin,
            sync_window_chrome,
            minimize_main_window,
            hide_main_window,
            toggle_maximize_main_window,
            start_window_drag,
            open_repository_url,
            open_releases_url,
            is_portable_mode
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
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
            "quit" => app.exit(0),
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
    window
        .set_focus()
        .map_err(|error| format!("failed to focus main window: {error}"))?;
    Ok(())
}

fn hide_main_window_handle(app: &AppHandle) -> Result<(), String> {
    app.get_webview_window("main")
        .ok_or_else(|| "main window is not available".to_string())?
        .hide()
        .map_err(|error| format!("failed to hide main window: {error}"))
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
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
        discovery: DiscoveryStatus {
            state: "idle".into(),
            detail: "LAN discovery is stopped. Start runtime or scan the LAN to find peers.".into(),
            port: layout.transport_port,
            local_peer: local_peer_from_layout(layout),
            peers: Vec::new(),
        },
    }
}

#[cfg(target_os = "windows")]
fn current_privilege_status() -> PrivilegeStatus {
    let is_elevated = is_windows_process_elevated().unwrap_or(false);
    PrivilegeStatus {
        is_elevated,
        can_elevate: !is_elevated,
        detail: if is_elevated {
            "Running as administrator. MyKVM can inject into elevated desktop windows."
        } else {
            "Standard user mode. Restart as administrator to control Task Manager and elevated windows."
        }
        .into(),
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
    use windows_sys::Win32::{UI::Shell::ShellExecuteW, UI::WindowsAndMessaging::SW_SHOWNORMAL};

    let exe =
        env::current_exe().map_err(|error| format!("failed to locate current exe: {error}"))?;
    let operation = wide_null("runas");
    let file = wide_null(&exe.to_string_lossy());
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            operation.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
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
        clipboard_sync: default_clipboard_sync(),
        language: default_language(),
        theme_mode: default_theme_mode(),
        performance_monitor: default_performance_monitor(),
        transport_port_mode: default_transport_port_mode(),
        transport_port,
        quic_port,
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
        clipboard_sync: default_clipboard_sync(),
        language: default_language(),
        theme_mode: default_theme_mode(),
        performance_monitor: default_performance_monitor(),
        transport_port_mode: default_transport_port_mode(),
        transport_port: default_transport_port(),
        quic_port: preferred_quic_port(default_transport_port()),
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
        clipboard_sync: saved_layout.clipboard_sync,
        language: normalize_language(&saved_layout.language),
        theme_mode: normalize_theme_mode(&saved_layout.theme_mode),
        performance_monitor: saved_layout.performance_monitor,
        transport_port_mode: normalize_transport_port_mode(&saved_layout.transport_port_mode),
        transport_port,
        quic_port: normalize_quic_port(transport_port, saved_layout.quic_port),
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

fn current_platform() -> &'static str {
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

        if let Ok(socket) = UdpSocket::bind(("0.0.0.0", candidate)) {
            return Ok((socket, candidate));
        }
    }

    let socket = UdpSocket::bind("0.0.0.0:0")
        .map_err(|error| format!("failed to bind any UDP transport port: {error}"))?;
    let port = socket
        .local_addr()
        .map_err(|error| format!("failed to read selected UDP transport port: {error}"))?
        .port();

    Ok((socket, port))
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
    text: String,
    sequence: u64,
}

fn run_clipboard_sync(
    quic_transport: quic_transport::TransportHandle,
    local_peer_id: String,
    clipboard_seen_text: Arc<Mutex<Option<String>>>,
    clipboard_target: Arc<Mutex<Option<input::ClipboardTarget>>>,
    transport_packets: Arc<AtomicU64>,
    clipboard_packets: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
) {
    let mut last_sent: Option<(String, String, String)> = None;
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

        let Ok(text) = read_system_clipboard() else {
            continue;
        };

        if text.len() > CLIPBOARD_MAX_TEXT_BYTES
            || last_sent
                .as_ref()
                .map(|(device_id, addr, previous_text)| {
                    device_id == &target.device_id && addr == &target.addr && previous_text == &text
                })
                .unwrap_or(false)
        {
            continue;
        }

        let should_send = clipboard_seen_text
            .lock()
            .map(|mut seen_text| {
                if seen_text.as_deref() == Some(text.as_str()) {
                    *seen_text = None;
                    false
                } else {
                    true
                }
            })
            .unwrap_or(true);

        if !should_send {
            last_sent = Some((target.device_id.clone(), target.addr.clone(), text));
            continue;
        }

        sequence = sequence.saturating_add(1);
        let packet = ClipboardPacket {
            protocol: CLIPBOARD_PROTOCOL.into(),
            origin_id: local_peer_id.clone(),
            text,
            sequence,
        };

        if let Ok(payload) = encode_wire_packet(&packet) {
            let peer = quic_transport.peer(
                target.addr.clone(),
                target.transport_public_key.clone(),
                target.protocol_version,
            );
            if quic_transport.send_stream(peer, payload).is_ok() {
                transport_packets.fetch_add(1, Ordering::Relaxed);
                clipboard_packets.fetch_add(1, Ordering::Relaxed);
                last_sent = Some((target.device_id, target.addr, packet.text));
            }
        }
    }
}

fn handle_clipboard_packet(
    payload: &[u8],
    local_peer_id: &str,
    clipboard_seen_text: &Arc<Mutex<Option<String>>>,
) -> bool {
    let Some(packet) = decode_wire_packet::<ClipboardPacket>(payload) else {
        return false;
    };

    if packet.protocol != CLIPBOARD_PROTOCOL {
        return false;
    }

    if packet.origin_id == local_peer_id {
        return true;
    }

    if packet.text.len() <= CLIPBOARD_MAX_TEXT_BYTES && write_system_clipboard(&packet.text).is_ok()
    {
        if let Ok(mut seen_text) = clipboard_seen_text.lock() {
            *seen_text = Some(packet.text);
        }
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

    for peer in peers {
        if peer.screens.is_empty()
            || layout
                .devices
                .iter()
                .any(|device| device_matches_peer(device, peer))
        {
            continue;
        }

        layout.devices.push(create_device_from_peer(layout, peer));
    }
}

fn device_matches_peer(device: &Device, peer: &LanPeer) -> bool {
    device.id == peer_device_id(peer)
        || same_host(&device.host, &peer.ip)
        || same_host(&device.host, &peer.host)
}

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
        device.screens = screens_from_peer(peer, &device.id, &device.screens, None);
    }
}

fn create_device_from_peer(layout: &LayoutState, peer: &LanPeer) -> Device {
    let id = peer_device_id(peer);
    let color = DEVICE_COLORS[layout.devices.len() % DEVICE_COLORS.len()].to_string();

    Device {
        id: id.clone(),
        name: peer
            .name
            .trim()
            .is_empty()
            .then(|| "LAN device".to_string())
            .unwrap_or_else(|| peer.name.clone()),
        platform: normalize_peer_platform(&peer.platform).into(),
        host: if peer.ip.trim().is_empty() {
            peer.host.clone()
        } else {
            peer.ip.clone()
        },
        transport_port: peer.transport_port,
        quic_port: normalize_quic_port(peer.transport_port, peer.quic_port),
        transport_public_key: peer.transport_public_key.clone(),
        protocol_version: peer.protocol_version,
        color,
        online: peer.input_ready,
        input_ready: peer.input_ready,
        role: "client".into(),
        source: "detected".into(),
        screens: screens_from_peer(peer, &id, &[], Some(layout)),
    }
}

fn screens_from_peer(
    peer: &LanPeer,
    device_id: &str,
    existing_screens: &[Screen],
    layout: Option<&LayoutState>,
) -> Vec<Screen> {
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
    let local_bounds = layout.and_then(local_screen_bounds);
    let start_x = local_bounds.map(|bounds| bounds.max_x).unwrap_or_default() + REMOTE_SCREEN_GAP;
    let start_y = local_bounds.map(|bounds| bounds.min_y).unwrap_or_default();

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
                    .unwrap_or(start_x + (peer_screen.x - peer_min_x)),
                y: existing_screen
                    .map(|screen| screen.y)
                    .unwrap_or(start_y + (peer_screen.y - peer_min_y)),
                width: peer_screen.width,
                height: peer_screen.height,
                scale: peer_screen.scale,
                is_primary: peer_screen.is_primary,
            }
        })
        .collect()
}

#[derive(Clone, Copy)]
struct LayoutBounds {
    min_y: i32,
    max_x: i32,
}

fn local_screen_bounds(layout: &LayoutState) -> Option<LayoutBounds> {
    let screens = layout
        .devices
        .iter()
        .find(|device| device.role == "local")
        .or_else(|| layout.devices.first())
        .map(|device| device.screens.as_slice())?;

    Some(LayoutBounds {
        min_y: screens.iter().map(|screen| screen.y).min()?,
        max_x: screens.iter().map(|screen| screen.x + screen.width).max()?,
    })
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

fn read_system_clipboard() -> Result<String, String> {
    let output = if cfg!(target_os = "macos") {
        Command::new("pbpaste").output()
    } else if cfg!(target_os = "windows") {
        Command::new("powershell")
            .args(["-NoProfile", "-Command", "Get-Clipboard -Raw"])
            .output()
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

fn write_system_clipboard(text: &str) -> Result<(), String> {
    let mut child = if cfg!(target_os = "macos") {
        Command::new("pbcopy").stdin(Stdio::piped()).spawn()
    } else if cfg!(target_os = "windows") {
        Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                "$text = [Console]::In.ReadToEnd(); if ($null -eq $text) { $text = '' }; Set-Clipboard -Value $text",
            ])
            .stdin(Stdio::piped())
            .spawn()
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

fn scan_for_peers(local_peer: &LanPeer) -> Result<Vec<LanPeer>, String> {
    let socket = UdpSocket::bind("0.0.0.0:0")
        .map_err(|error| format!("failed to open UDP scan socket: {error}"))?;
    socket
        .set_broadcast(true)
        .map_err(|error| format!("failed to enable UDP broadcast: {error}"))?;
    socket
        .set_read_timeout(Some(Duration::from_millis(250)))
        .map_err(|error| format!("failed to set UDP scan timeout: {error}"))?;

    for target in broadcast_addrs(local_peer.transport_port) {
        let _ = send_discovery_packet(&socket, "announce", local_peer, target);
    }

    let started = Instant::now();
    let mut buffer = [0_u8; 4096];
    let mut peers = Vec::new();

    while started.elapsed() < Duration::from_millis(1400) {
        if let Ok((length, source)) = socket.recv_from(&mut buffer) {
            if let Some(packet) = decode_discovery_packet(&buffer[..length]) {
                if let Some((_kind, peer)) =
                    peer_from_discovery_packet(packet, source.ip().to_string(), &local_peer.id)
                {
                    merge_peer_entry(&mut peers, peer);
                }
            }
        }
    }

    Ok(peers)
}

fn probe_for_peer(local_peer: &LanPeer, host: &str) -> Result<LanPeer, String> {
    let target = format!("{}:{}", host.trim(), local_peer.transport_port);
    let socket = UdpSocket::bind("0.0.0.0:0")
        .map_err(|error| format!("failed to open UDP probe socket: {error}"))?;
    socket
        .set_read_timeout(Some(Duration::from_millis(250)))
        .map_err(|error| format!("failed to set UDP probe timeout: {error}"))?;

    send_discovery_packet(&socket, "probe", local_peer, target.as_str())?;

    let started = Instant::now();
    let mut buffer = [0_u8; 4096];
    while started.elapsed() < Duration::from_millis(1800) {
        if let Ok((length, source)) = socket.recv_from(&mut buffer) {
            if let Some(packet) = decode_discovery_packet(&buffer[..length]) {
                if let Some((_kind, peer)) =
                    peer_from_discovery_packet(packet, source.ip().to_string(), &local_peer.id)
                {
                    return Ok(peer);
                }
            }
        }
    }

    Err(format!(
    "no mykvm peer answered at {host}:{}; make sure mykvm is running on that device and UDP {} is allowed",
    local_peer.transport_port, local_peer.transport_port
  ))
}

fn send_discovery_packet(
    socket: &UdpSocket,
    kind: &str,
    local_peer: &LanPeer,
    target: impl std::net::ToSocketAddrs,
) -> Result<(), String> {
    let mut peer = local_peer.clone();
    peer.last_seen_ms = now_ms();
    let packet = DiscoveryPacket {
        protocol: DISCOVERY_PROTOCOL.into(),
        kind: kind.into(),
        peer,
    };
    let payload = encode_wire_packet(&packet)
        .map_err(|error| format!("failed to encode discovery packet: {error}"))?;
    socket
        .send_to(&payload, target)
        .map(|_| ())
        .map_err(|error| format!("failed to send discovery packet: {error}"))
}

fn decode_discovery_packet(payload: &[u8]) -> Option<DiscoveryPacket> {
    let packet = decode_wire_packet::<DiscoveryPacket>(payload)?;
    (packet.protocol == DISCOVERY_PROTOCOL).then_some(packet)
}

fn peer_from_discovery_packet(
    packet: DiscoveryPacket,
    source_ip: String,
    local_peer_id: &str,
) -> Option<(String, LanPeer)> {
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
    Some((packet.kind, peer))
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

pub(crate) fn broadcast_addrs(port: u16) -> Vec<String> {
    let mut addresses = vec![format!("255.255.255.255:{port}")];

    if let Some(ip) = local_ip_address() {
        let parts = ip.split('.').collect::<Vec<_>>();
        if parts.len() == 4 {
            addresses.push(format!("{}.{}.{}.255:{port}", parts[0], parts[1], parts[2]));
        }
    }

    addresses.sort();
    addresses.dedup();
    addresses
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
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
            clipboard_sync: false,
            language: "cn".into(),
            theme_mode: "system".into(),
            performance_monitor: false,
            transport_port_mode: "auto".into(),
            transport_port: 49152,
            quic_port: 49153,
        }
    }

    fn test_peer() -> LanPeer {
        LanPeer {
            id: "peer-client-10-0-0-2".into(),
            name: "Client".into(),
            platform: "windows".into(),
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
    fn peer_presence_adds_new_online_peer_screens() {
        let mut layout = test_layout();
        layout.devices.truncate(1);
        let peer = test_peer();

        apply_peer_presence(&mut layout, &[peer]);

        assert_eq!(layout.devices.len(), 2);
        assert_eq!(layout.devices[1].id, "peer-client-10-0-0-2");
        assert!(layout.devices[1].online);
        assert!(layout.devices[1].input_ready);
        assert_eq!(layout.devices[1].screens.len(), 1);
        assert_eq!(
            layout.devices[1].screens[0].id,
            "peer-client-10-0-0-2-local-display-1"
        );
        assert_eq!(layout.devices[1].screens[0].x, 1920);
    }
}
