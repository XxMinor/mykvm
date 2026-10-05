use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use crate::{
    advertised_input_ready, apply_transport_to_peer, bind_available_udp_port, broadcast_addrs,
    decode_discovery_packet, discovery_base_port, input, known_peer_discovery_targets,
    load_layout_from_disk, local_peer_from_layout, peer_from_discovery_packet, preferred_quic_port,
    quic_transport, send_discovery_packet, should_reply_to_discovery, LayoutState,
};

/// The receive-only network runtime used by the Windows service before the
/// interactive app takes over. Pairing, clipboard and file streams deliberately
/// remain in the user app.
pub struct HeadlessClientHandle {
    stop: Arc<AtomicBool>,
    discovery_thread: Option<thread::JoinHandle<()>>,
    transport: quic_transport::TransportHandle,
}

impl HeadlessClientHandle {
    pub fn stop_and_wait(&mut self) -> Result<(), String> {
        if self.stop.swap(true, Ordering::Relaxed) {
            return Ok(());
        }
        let transport_result = self.transport.stop_and_wait();
        input::close_windows_input_helper_pipe();
        if let Some(thread) = self.discovery_thread.take() {
            let _ = thread.join();
        }
        transport_result
    }
}

impl Drop for HeadlessClientHandle {
    fn drop(&mut self) {
        let _ = self.stop_and_wait();
    }
}

pub fn start(config_path: PathBuf) -> Result<Option<HeadlessClientHandle>, String> {
    let Some(mut layout) = load_layout_from_disk(&config_path) else {
        return Ok(None);
    };
    if !headless_receive_enabled(&layout) {
        return Ok(None);
    }

    let desired_port = discovery_base_port(&layout);
    let (socket, actual_port) = bind_available_udp_port(desired_port)?;
    socket
        .set_broadcast(true)
        .map_err(|error| format!("failed to enable headless UDP broadcast: {error}"))?;
    socket
        .set_read_timeout(Some(Duration::from_millis(250)))
        .map_err(|error| format!("failed to set headless discovery timeout: {error}"))?;

    let layout_state = Arc::new(Mutex::new(layout.clone()));
    let native_layout = Arc::new(Mutex::new(layout.clone()));
    let input_events = Arc::new(AtomicU64::new(0));
    let clipboard_target = Arc::new(Mutex::new(None));
    let layout_for_input = Arc::clone(&layout_state);
    let events_for_input = Arc::clone(&input_events);
    let clipboard_for_input = Arc::clone(&clipboard_target);
    input::start_held_input_watchdog(input::dispatch_input_command_to_windows_helper);
    let on_datagram = Arc::new(move |payload: Vec<u8>, source| {
        let sink = |command| input::dispatch_input_command_to_windows_helper(command);
        let _ = input::handle_input_datagram_with_sink(
            &layout_for_input,
            &native_layout,
            &payload,
            source,
            &events_for_input,
            &clipboard_for_input,
            &sink,
        );
    });
    let identity_dir = config_path
        .parent()
        .map(|parent| parent.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let transport = quic_transport::start_datagram_only(
        preferred_quic_port(actual_port),
        identity_dir,
        on_datagram,
    )?;

    layout.transport_port = actual_port;
    layout.quic_port = transport.port();
    for device in &mut layout.devices {
        if device.role == "local" {
            device.transport_port = actual_port;
            device.quic_port = transport.port();
            device.transport_public_key = transport.public_key().to_string();
            device.protocol_version = quic_transport::PROTOCOL_VERSION;
        }
    }
    if let Ok(mut stored) = layout_state.lock() {
        *stored = layout.clone();
    }

    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let thread_layout = Arc::clone(&layout_state);
    let thread_transport = transport.clone();
    let fallback_layout = layout;
    let discovery_thread = thread::Builder::new()
        .name("mykvm-headless-discovery".into())
        .spawn(move || {
            let mut buffer = [0_u8; 4096];
            let mut last_announce = Instant::now() - Duration::from_secs(10);

            while !thread_stop.load(Ordering::Relaxed) {
                if last_announce.elapsed() >= Duration::from_secs(3) {
                    let current = thread_layout
                        .lock()
                        .map(|layout| layout.clone())
                        .unwrap_or_else(|_| fallback_layout.clone());
                    let mut peer = local_peer_from_layout(&current);
                    apply_transport_to_peer(&mut peer, &thread_transport);
                    peer.input_ready = advertised_input_ready(&current, helper_ready());

                    for target in broadcast_addrs(desired_port)
                        .into_iter()
                        .chain(known_peer_discovery_targets(&current, desired_port))
                    {
                        let _ = send_discovery_packet(&socket, "announce", &peer, target.as_str());
                    }
                    last_announce = Instant::now();
                }

                let Ok((length, source)) = socket.recv_from(&mut buffer) else {
                    continue;
                };
                let Some(packet) = decode_discovery_packet(&buffer[..length]) else {
                    continue;
                };
                let current = thread_layout
                    .lock()
                    .map(|layout| layout.clone())
                    .unwrap_or_else(|_| fallback_layout.clone());
                let mut local_peer = local_peer_from_layout(&current);
                apply_transport_to_peer(&mut local_peer, &thread_transport);
                local_peer.input_ready = advertised_input_ready(&current, helper_ready());
                let Some(incoming) =
                    peer_from_discovery_packet(packet, source.ip().to_string(), &local_peer.id)
                else {
                    continue;
                };

                if matches!(incoming.kind.as_str(), "announce" | "probe")
                    && should_reply_to_discovery(&current, &incoming.peer)
                {
                    let _ = send_discovery_packet(&socket, "reply", &local_peer, source);
                }
            }
        });
    let discovery_thread = match discovery_thread {
        Ok(thread) => thread,
        Err(error) => {
            let _ = transport.stop_and_wait();
            return Err(format!(
                "failed to start headless discovery thread: {error}"
            ));
        }
    };

    Ok(Some(HeadlessClientHandle {
        stop,
        discovery_thread: Some(discovery_thread),
        transport,
    }))
}

fn helper_ready() -> bool {
    input::windows_input_pipe_available()
}

fn headless_receive_enabled(layout: &LayoutState) -> bool {
    layout.machine_role == "client"
        && layout.input_mode == "receive"
        && !layout.cluster_id.trim().is_empty()
        && !layout.pair_secret.trim().is_empty()
        && !layout.paired_controllers.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eligible_layout() -> LayoutState {
        serde_json::from_value(serde_json::json!({
            "devices": [],
            "activeDeviceId": "",
            "selectedScreenId": "",
            "inputMode": "receive",
            "machineRole": "client",
            "clusterId": "cluster-test",
            "pairSecret": "secret-test",
            "pairedControllers": [{
                "id": "controller",
                "name": "Controller",
                "host": "controller.local",
                "ip": "192.0.2.10",
                "transportPublicKey": "controller-key",
                "protocolVersion": 1,
                "clusterId": "cluster-test",
                "pairedAtMs": 1
            }]
        }))
        .expect("test layout")
    }

    #[test]
    fn headless_receiver_requires_a_paired_receive_client() {
        let mut layout = eligible_layout();
        assert!(headless_receive_enabled(&layout));

        layout.paired_controllers.clear();
        assert!(!headless_receive_enabled(&layout));

        layout = eligible_layout();
        layout.machine_role = "server".into();
        assert!(!headless_receive_enabled(&layout));
    }
}
