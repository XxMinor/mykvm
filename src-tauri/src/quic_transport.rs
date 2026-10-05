use std::{
    collections::HashMap,
    fs,
    net::{SocketAddr, ToSocketAddrs},
    path::{Path, PathBuf},
    sync::{mpsc, Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use quinn::{
    rustls::{
        self,
        client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
        crypto::{
            ring::default_provider, verify_tls12_signature, verify_tls13_signature,
            WebPkiSupportedAlgorithms,
        },
        pki_types::{CertificateDer, PrivatePkcs8KeyDer, ServerName, UnixTime},
        DigitallySignedStruct, SignatureScheme,
    },
    ClientConfig, Endpoint, ServerConfig,
};
use tokio::sync::mpsc as tokio_mpsc;

pub const PROTOCOL_VERSION: u16 = 1;

const SERVER_NAME: &str = "mykvm.local";
const MAX_DATAGRAM_BYTES: usize = 16 * 1024;
// Clipboard images are sent as RGBA base64 over streams. The clipboard module
// caps decoded images at 32 MiB, which becomes roughly 43 MiB on the wire.
pub(crate) const MAX_STREAM_BYTES: usize = 48 * 1024 * 1024;
/// Prefix of the error for a stream the receiver answered with `reject`.
pub(crate) const STREAM_REJECTED: &str = "QUIC stream receiver rejected payload";
const PORT_SCAN_COUNT: u16 = 64;
const QUIC_WORKER_THREADS: usize = 2;
// Datagram-health fast-fail (concept adopted from PR #22): after this many
// consecutive send/connect failures to a peer, sends short-circuit with an
// error until the retry window elapses, so the input layer releases the
// cursor immediately instead of freezing behind connect timeouts.
const DATAGRAM_FAIL_THRESHOLD: u32 = 3;
const DATAGRAM_RETRY_WINDOW: Duration = Duration::from_secs(3);
const MAX_HEALTH_PEERS: usize = 64;
// Streams (clipboard, files) are handled in spawned tasks; cap the concurrent
// in-flight count so a burst cannot spawn unbounded copies of a 48MB write.
const MAX_CONCURRENT_STREAMS: usize = 8;

type DatagramHandler = Arc<dyn Fn(Vec<u8>, SocketAddr) + Send + Sync + 'static>;
type StreamHandler = Arc<dyn Fn(Vec<u8>, SocketAddr) -> bool + Send + Sync + 'static>;

#[derive(Clone, Debug)]
pub struct PeerEndpoint {
    pub addr: String,
    pub public_key: String,
    pub protocol_version: u16,
}

/// Consecutive-failure record for one peer address. Shared between the
/// caller-facing handle (fast-fail check) and the transport loop (updates).
#[derive(Debug, Clone, Copy)]
struct PeerHealth {
    consecutive_failures: u32,
    last_failure: Instant,
}

type HealthMap = Arc<Mutex<HashMap<String, PeerHealth>>>;

fn peer_fast_fail_active(health: &HealthMap, addr: &str) -> bool {
    health
        .lock()
        .map(|health| {
            health.get(addr).is_some_and(|entry| {
                entry.consecutive_failures >= DATAGRAM_FAIL_THRESHOLD
                    && entry.last_failure.elapsed() < DATAGRAM_RETRY_WINDOW
            })
        })
        .unwrap_or(false)
}

fn record_peer_failure(health: &HealthMap, addr: &str, error: &str) {
    let Ok(mut health) = health.lock() else {
        return;
    };
    if health.len() >= MAX_HEALTH_PEERS && !health.contains_key(addr) {
        if let Some(stale) = health
            .iter()
            .min_by_key(|(_, entry)| entry.last_failure)
            .map(|(key, _)| key.clone())
        {
            health.remove(&stale);
        }
    }
    let now = Instant::now();
    let entry = health.entry(addr.to_string()).or_insert(PeerHealth {
        consecutive_failures: 0,
        last_failure: now,
    });
    entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
    entry.last_failure = now;
    // Log the first failure and the transition into fast-fail; everything in
    // between and every muted retry is debug. The old unconditional warn wrote
    // a disk line every few seconds for as long as a peer stayed unreachable.
    match entry.consecutive_failures {
        1 => log::warn!("QUIC send to {addr} failed: {error}"),
        DATAGRAM_FAIL_THRESHOLD => log::warn!(
            "QUIC sends to {addr} keep failing; muting attempts to one probe per {}s: {error}",
            DATAGRAM_RETRY_WINDOW.as_secs()
        ),
        _ => log::debug!("QUIC send to {addr} still failing: {error}"),
    }
}

fn record_peer_success(health: &HealthMap, addr: &str) {
    let Ok(mut health) = health.lock() else {
        return;
    };
    if let Some(entry) = health.remove(addr) {
        if entry.consecutive_failures >= DATAGRAM_FAIL_THRESHOLD {
            log::info!("QUIC sends to {addr} recovered");
        }
    }
}

#[derive(Clone)]
pub struct TransportHandle {
    commands: tokio_mpsc::UnboundedSender<TransportCommand>,
    port: u16,
    public_key: String,
    peer_health: HealthMap,
    connections: ConnectionMap,
}

impl TransportHandle {
    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn public_key(&self) -> &str {
        &self.public_key
    }

    pub fn peer(&self, addr: String, public_key: String, protocol_version: u16) -> PeerEndpoint {
        PeerEndpoint {
            addr,
            public_key,
            protocol_version,
        }
    }

    /// Returns false while warming a connection, so capture keeps input local
    /// instead of treating connection startup as an input send.
    pub fn send_datagram(&self, peer: PeerEndpoint, payload: Vec<u8>) -> Result<bool, String> {
        if payload.len() > MAX_DATAGRAM_BYTES {
            return Err(format!(
                "QUIC datagram is too large: {} bytes",
                payload.len()
            ));
        }
        // Fail fast while the peer is known-dead so the input layer releases
        // the cursor instead of streaming moves into a black hole.
        if peer_fast_fail_active(&self.peer_health, &peer.addr) {
            return Err(format!(
                "QUIC peer {} unreachable ({DATAGRAM_FAIL_THRESHOLD}+ consecutive failures)",
                peer.addr
            ));
        }

        // Do not resolve DNS from the native input callback. The connection
        // loop resolves endpoints while warming them.
        let ready = self.connections.lock()
            .map_err(|_| "QUIC connection map is poisoned".to_string())?
            .iter()
            .any(|(key, slot)| key.requested_addr == peer.addr && key.public_key == peer.public_key
                && matches!(slot, ConnectionSlot::Ready(connection) if connection.close_reason().is_none()));
        self.commands
            .send(TransportCommand::SendDatagram {
                peer,
                payload: if ready { payload } else { Vec::new() },
            })
            .map_err(|_| "QUIC transport is stopped".to_string())?;
        Ok(ready)
    }

    pub fn send_stream_expect_ack(
        &self,
        peer: PeerEndpoint,
        payload: Vec<u8>,
    ) -> Result<(), String> {
        if payload.len() > MAX_STREAM_BYTES {
            return Err(format!(
                "QUIC stream payload is too large: {} bytes",
                payload.len()
            ));
        }
        if peer_fast_fail_active(&self.peer_health, &peer.addr) {
            return Err(format!(
                "QUIC peer {} unreachable ({DATAGRAM_FAIL_THRESHOLD}+ consecutive failures)",
                peer.addr
            ));
        }

        let (result_tx, result_rx) = mpsc::channel();
        self.commands
            .send(TransportCommand::SendStream {
                peer,
                payload,
                result: result_tx,
            })
            .map_err(|_| "QUIC transport is stopped".to_string())?;
        result_rx
            .recv_timeout(Duration::from_secs(6))
            .map_err(|_| "QUIC stream send timed out".to_string())?
    }

    pub fn shutdown(&self) {
        let _ = self.stop_and_wait();
    }

    pub fn stop_and_wait(&self) -> Result<(), String> {
        let (ack_tx, ack_rx) = mpsc::channel();
        self.commands
            .send(TransportCommand::Shutdown { ack: ack_tx })
            .map_err(|_| "QUIC transport is stopped".to_string())?;
        ack_rx
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| "QUIC transport shutdown timed out".to_string())
    }
}

enum TransportCommand {
    SendDatagram {
        peer: PeerEndpoint,
        payload: Vec<u8>,
    },
    SendStream {
        peer: PeerEndpoint,
        payload: Vec<u8>,
        result: mpsc::Sender<Result<(), String>>,
    },
    Shutdown {
        ack: mpsc::Sender<()>,
    },
}

#[derive(Clone, Hash, PartialEq, Eq)]
struct PeerKey {
    addr: SocketAddr,
    requested_addr: String,
    public_key: String,
}

pub fn start(
    preferred_port: u16,
    identity_dir: PathBuf,
    on_datagram: DatagramHandler,
    on_stream: StreamHandler,
) -> Result<TransportHandle, String> {
    start_inner(preferred_port, identity_dir, on_datagram, Some(on_stream))
}

pub fn start_datagram_only(
    preferred_port: u16,
    identity_dir: PathBuf,
    on_datagram: DatagramHandler,
) -> Result<TransportHandle, String> {
    start_inner(preferred_port, identity_dir, on_datagram, None)
}

fn start_inner(
    preferred_port: u16,
    identity_dir: PathBuf,
    on_datagram: DatagramHandler,
    on_stream: Option<StreamHandler>,
) -> Result<TransportHandle, String> {
    // Load (or create-and-persist) this machine's transport identity *before*
    // spawning the runtime thread so a stable public key is reused across
    // restarts/updates. A churning key breaks the peer's certificate pinning
    // and its paired-controllers authorization until both sides re-pair.
    let identity = load_or_create_identity(&identity_dir)?;
    let (ready_tx, ready_rx) = mpsc::channel();
    let (command_tx, command_rx) = tokio_mpsc::unbounded_channel();
    let peer_health: HealthMap = Arc::new(Mutex::new(HashMap::new()));
    let loop_health = Arc::clone(&peer_health);
    let connections: ConnectionMap = Arc::new(Mutex::new(HashMap::new()));
    let loop_connections = Arc::clone(&connections);

    thread::Builder::new()
        .name("mykvm-quic-transport".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .thread_name("mykvm-quic")
                .worker_threads(QUIC_WORKER_THREADS)
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ = ready_tx.send(Err(format!("failed to start QUIC runtime: {error}")));
                    return;
                }
            };

            let shutdown_ack = runtime.block_on(run_transport(
                preferred_port,
                identity,
                command_rx,
                on_datagram,
                on_stream,
                loop_health,
                loop_connections,
                ready_tx,
            ));
            drop(runtime);
            if let Some(ack) = shutdown_ack {
                let _ = ack.send(());
            }
        })
        .map_err(|error| format!("failed to spawn QUIC transport thread: {error}"))?;

    let ready = ready_rx
        .recv_timeout(Duration::from_secs(3))
        .map_err(|_| "QUIC transport did not become ready".to_string())??;

    Ok(TransportHandle {
        commands: command_tx,
        port: ready.port,
        public_key: ready.public_key,
        peer_health,
        connections,
    })
}

struct ReadyTransport {
    port: u16,
    public_key: String,
}

/// A cached peer connection, or a marker that a background task is already
/// establishing one (so a burst of mouse moves cannot spawn a connect storm).
enum ConnectionSlot {
    Connecting,
    Ready(quinn::Connection),
}

type ConnectionMap = Arc<Mutex<HashMap<PeerKey, ConnectionSlot>>>;

async fn run_transport(
    preferred_port: u16,
    identity: TransportIdentity,
    mut commands: tokio_mpsc::UnboundedReceiver<TransportCommand>,
    on_datagram: DatagramHandler,
    on_stream: Option<StreamHandler>,
    health: HealthMap,
    connections: ConnectionMap,
    ready_tx: mpsc::Sender<Result<ReadyTransport, String>>,
) -> Option<mpsc::Sender<()>> {
    let (endpoint, public_key) = match bind_endpoint(preferred_port, &identity) {
        Ok(bound) => bound,
        Err(error) => {
            let _ = ready_tx.send(Err(error));
            return None;
        }
    };

    let port = match endpoint.local_addr() {
        Ok(addr) => addr.port(),
        Err(error) => {
            let _ = ready_tx.send(Err(format!("failed to read QUIC port: {error}")));
            return None;
        }
    };

    let _ = ready_tx.send(Ok(ReadyTransport { port, public_key }));
    spawn_accept_loop(endpoint.clone(), on_datagram, on_stream);

    // The command loop must never await network progress: one dead peer's 2s
    // connect timeout or one 48MB stream write would stall every queued input
    // datagram behind it (the "periodic input freeze + warn every 4s" bug).
    // Datagrams go out synchronously on established connections; connection
    // establishment and stream sends run in spawned tasks.
    let stream_slots = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_STREAMS));
    let mut shutdown_ack = None;
    while let Some(command) = commands.recv().await {
        match command {
            TransportCommand::SendDatagram { peer, payload } => {
                send_datagram_nonblocking(&endpoint, &connections, &health, peer, payload);
            }
            TransportCommand::SendStream {
                peer,
                payload,
                result,
            } => {
                let Ok(permit) = Arc::clone(&stream_slots).try_acquire_owned() else {
                    let _ = result.send(Err(format!(
                        "QUIC stream queue is full ({MAX_CONCURRENT_STREAMS} in flight)"
                    )));
                    continue;
                };
                let endpoint = endpoint.clone();
                let connections = Arc::clone(&connections);
                let health = Arc::clone(&health);
                tokio::spawn(async move {
                    // Cancel stalled writes too, not just the caller's wait for
                    // an answer, so timed-out transfers release their buffers.
                    let outcome = tokio::time::timeout(
                        Duration::from_secs(5),
                        send_stream_task(&endpoint, &connections, &health, peer, payload),
                    ).await.unwrap_or_else(|_| Err("QUIC stream send timed out".into()));
                    if let Err(error) = &outcome {
                        log::warn!("QUIC stream send failed: {error}");
                    }
                    let _ = result.send(outcome);
                    drop(permit);
                });
            }
            TransportCommand::Shutdown { ack } => {
                shutdown_ack = Some(ack);
                break;
            }
        }
    }

    endpoint.close(0_u32.into(), b"shutdown");
    endpoint.wait_idle().await;
    shutdown_ack
}

fn bind_endpoint(
    preferred_port: u16,
    identity: &TransportIdentity,
) -> Result<(Endpoint, String), String> {
    let runtime = quinn::default_runtime()
        .ok_or_else(|| "no async runtime available for QUIC endpoint".to_string())?;
    let mut last_error = None;

    for port in candidate_ports(preferred_port) {
        let server_config = server_config(identity)?;
        let socket = match bind_reusable_quic_socket(port) {
            Ok(socket) => socket,
            Err(error) => {
                last_error = Some(error.to_string());
                continue;
            }
        };
        // Build the endpoint from our own reuse-enabled socket instead of
        // Endpoint::server (which binds a plain socket without SO_REUSEADDR).
        match Endpoint::new(
            quinn::EndpointConfig::default(),
            Some(server_config),
            socket,
            runtime.clone(),
        ) {
            Ok(endpoint) => return Ok((endpoint, identity.public_key.clone())),
            Err(error) => last_error = Some(error.to_string()),
        }
    }

    Err(format!(
        "failed to bind QUIC port: {}",
        last_error.unwrap_or_else(|| "no candidate ports available".into())
    ))
}

/// Bind the QUIC endpoint's UDP socket with address reuse enabled, mirroring the
/// discovery socket. Without `SO_REUSEADDR` a fresh endpoint cannot re-grab the
/// same QUIC port while the previous process's socket is still tearing down — on
/// an admin-restart, app relaunch, or runtime restart the port silently drifts
/// upward (47834 -> 47835 ...) and the controller keeps targeting the stale port
/// until re-discovery propagates the new one, which is the intermittent "shows
/// online but the cursor won't cross" symptom.
fn bind_reusable_quic_socket(port: u16) -> std::io::Result<std::net::UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};

    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    #[cfg(unix)]
    socket.set_reuse_port(true)?;
    let address = SocketAddr::from(([0, 0, 0, 0], port));
    socket.bind(&address.into())?;
    Ok(socket.into())
}

/// This machine's persisted QUIC transport identity. The advertised
/// `public_key` is the base64 of the certificate DER — peers pin it during
/// discovery, so it MUST stay stable across restarts.
#[derive(Clone)]
struct TransportIdentity {
    cert_der: Vec<u8>,
    key_der: Vec<u8>,
    public_key: String,
}

const QUIC_CERT_FILE: &str = "quic-transport-cert.der";
const QUIC_KEY_FILE: &str = "quic-transport-key.der";

/// Load the persisted self-signed cert/key, or generate one and persist it on
/// first run (or when the stored files are missing/corrupt). Without this the
/// identity was regenerated on every launch, rotating the advertised public
/// key and breaking the peer's pinned-cert handshake / pairing authorization.
fn load_or_create_identity(dir: &Path) -> Result<TransportIdentity, String> {
    let cert_path = dir.join(QUIC_CERT_FILE);
    let key_path = dir.join(QUIC_KEY_FILE);

    if let (Ok(cert_der), Ok(key_der)) = (fs::read(&cert_path), fs::read(&key_path)) {
        if !cert_der.is_empty() && !key_der.is_empty() {
            return Ok(TransportIdentity {
                public_key: BASE64.encode(&cert_der),
                cert_der,
                key_der,
            });
        }
    }

    let generated =
        rcgen::generate_simple_self_signed(vec![SERVER_NAME.into(), "localhost".into()])
            .map_err(|error| format!("failed to generate QUIC certificate: {error}"))?;
    let cert_der = generated.cert.der().to_vec();
    let key_der = generated.key_pair.serialize_der();

    if let Err(error) = fs::create_dir_all(dir) {
        log::warn!(
            "failed to create QUIC identity dir {}: {error}",
            dir.display()
        );
    }
    if let Err(error) = fs::write(&cert_path, &cert_der) {
        log::warn!("failed to persist QUIC certificate: {error}");
    }
    if let Err(error) = fs::write(&key_path, &key_der) {
        log::warn!("failed to persist QUIC key: {error}");
    }

    Ok(TransportIdentity {
        public_key: BASE64.encode(&cert_der),
        cert_der,
        key_der,
    })
}

fn candidate_ports(preferred_port: u16) -> Vec<u16> {
    let start = preferred_port.max(1024);
    let mut ports = Vec::new();
    for offset in 0..PORT_SCAN_COUNT {
        let Some(port) = start.checked_add(offset) else {
            break;
        };
        if port == 0 {
            continue;
        }
        ports.push(port);
    }
    ports.push(0);
    ports
}

fn server_config(identity: &TransportIdentity) -> Result<ServerConfig, String> {
    let cert_der = CertificateDer::from(identity.cert_der.clone());
    let key_der = PrivatePkcs8KeyDer::from(identity.key_der.clone());
    let mut config = ServerConfig::with_single_cert(vec![cert_der], key_der.into())
        .map_err(|error| format!("failed to build QUIC server config: {error}"))?;
    config.transport = Arc::new(tuned_transport_config());

    Ok(config)
}

/// Shared QUIC transport tuning. The keep-alive interval holds connections open
/// through idle periods so the first input event after the machine has been
/// sitting unused does not pay a fresh handshake (the "laggy after idle" feel),
/// while the idle timeout still reaps connections to peers that truly vanished.
fn tuned_transport_config() -> quinn::TransportConfig {
    let mut transport = quinn::TransportConfig::default();
    transport.max_concurrent_bidi_streams(64_u32.into());
    // Keep-alive well under the idle timeout so a healthy link never drops, but
    // keep the idle timeout short: when a client vanishes (e.g. it is killed and
    // reinstalled during an app upgrade) the controller's cached connection must
    // close on its own within a few seconds. Otherwise the controller keeps
    // reusing the now-dead connection after the client comes back, so input
    // silently goes nowhere until the user toggles the runtime to force a
    // reconnect. 10 s tolerates brief LAN/Wi-Fi hiccups while auto-recovering
    // across the typical upgrade downtime without any manual toggle.
    transport.keep_alive_interval(Some(Duration::from_secs(3)));
    // A LAN round trip is a few ms; quinn's 333 ms default made the first
    // handshake retransmit wait about a second. When a peer process hands its
    // ports over (an update, the input service taking over), the reconnect's
    // first Initial can hit the port before it is bound again, so retry fast.
    transport.initial_rtt(Duration::from_millis(50));
    if let Ok(timeout) = quinn::IdleTimeout::try_from(Duration::from_secs(10)) {
        transport.max_idle_timeout(Some(timeout));
    }
    // Input datagrams are stale within a second. quinn's 1 MiB default queued
    // thousands of old moves (and clicks) during a Wi-Fi stall and replayed
    // them afterwards; a small queue drops the oldest instead.
    transport.datagram_send_buffer_size(16 * 1024);
    transport.datagram_receive_buffer_size(Some(64 * 1024));
    transport
}

/// Certificate-pinning verifier for the QUIC transport.
///
/// Each peer generates a fresh self-signed certificate at startup and
/// advertises it during discovery. We pin *exactly* that certificate instead
/// of running a WebPKI chain/CA validation over a self-signed leaf — the latter
/// is brittle across platforms and was rejecting otherwise valid peers with
/// `invalid peer certificate: BadSignature` (Mac↔Windows handshakes failed, so
/// input/clipboard never connected). The handshake signature is still verified
/// against the pinned certificate's key via the ring provider, so a peer must
/// prove it actually holds the advertised key — pinning by bytes alone is not
/// enough on its own.
#[derive(Debug)]
struct PinnedCertVerifier {
    pinned: CertificateDer<'static>,
    supported: WebPkiSupportedAlgorithms,
}

impl PinnedCertVerifier {
    fn new(pinned: CertificateDer<'static>) -> Self {
        Self {
            pinned,
            supported: default_provider().signature_verification_algorithms,
        }
    }
}

impl ServerCertVerifier for PinnedCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if end_entity.as_ref() == self.pinned.as_ref() {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(
                "peer certificate does not match the pinned transport certificate".to_string(),
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.supported)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.supported)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.supported.supported_schemes()
    }
}

fn client_config(peer: &PeerEndpoint) -> Result<ClientConfig, String> {
    if peer.protocol_version != PROTOCOL_VERSION {
        return Err(format!(
            "unsupported peer transport protocol version {}",
            peer.protocol_version
        ));
    }

    let cert_der = BASE64
        .decode(peer.public_key.as_bytes())
        .map_err(|error| format!("invalid peer transport public key: {error}"))?;
    let pinned = CertificateDer::from(cert_der);

    // QUIC is TLS 1.3 only; pin the advertised certificate with our own verifier
    // rather than WebPKI root validation.
    let crypto = rustls::ClientConfig::builder_with_provider(Arc::new(default_provider()))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|error| format!("failed to build QUIC client crypto: {error}"))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedCertVerifier::new(pinned)))
        .with_no_client_auth();

    let quic_crypto = quinn::crypto::rustls::QuicClientConfig::try_from(crypto)
        .map_err(|error| format!("failed to build QUIC client config: {error}"))?;
    let mut config = ClientConfig::new(Arc::new(quic_crypto));
    config.transport_config(Arc::new(tuned_transport_config()));
    Ok(config)
}

fn spawn_accept_loop(
    endpoint: Endpoint,
    on_datagram: DatagramHandler,
    on_stream: Option<StreamHandler>,
) {
    let stream_slots = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_STREAMS));
    tokio::spawn(async move {
        while let Some(incoming) = endpoint.accept().await {
            let remote = incoming.remote_address();
            let on_datagram = Arc::clone(&on_datagram);
            let on_stream = on_stream.clone();
            let stream_slots = Arc::clone(&stream_slots);

            tokio::spawn(async move {
                match incoming.await {
                    Ok(connection) => {
                        spawn_datagram_reader(connection.clone(), remote, on_datagram);
                        if let Some(on_stream) = on_stream {
                            spawn_stream_reader(connection, remote, on_stream, stream_slots);
                        }
                    }
                    Err(error) => {
                        log::warn!("QUIC incoming connection failed from {remote}: {error}");
                    }
                }
            });
        }
    });
}

fn spawn_datagram_reader(
    connection: quinn::Connection,
    remote: SocketAddr,
    on_datagram: DatagramHandler,
) {
    tokio::spawn(async move {
        loop {
            match connection.read_datagram().await {
                // Native input injection can block on the desktop or main queue.
                // Keep per-connection ordering without occupying a QUIC worker.
                Ok(payload) => {
                    tokio::task::block_in_place(|| on_datagram(payload.to_vec(), remote))
                }
                Err(error) => {
                    log::debug!("QUIC datagram reader stopped for {remote}: {error}");
                    break;
                }
            }
        }
    });
}

fn spawn_stream_reader(
    connection: quinn::Connection,
    remote: SocketAddr,
    on_stream: StreamHandler,
    stream_slots: Arc<tokio::sync::Semaphore>,
) {
    tokio::spawn(async move {
        loop {
            match connection.accept_bi().await {
                Ok((mut send, mut recv)) => {
                    let Ok(permit) = Arc::clone(&stream_slots).try_acquire_owned() else {
                        let _ = recv.stop(0_u32.into());
                        let _ = send.reset(0_u32.into());
                        continue;
                    };
                    let on_stream = Arc::clone(&on_stream);
                    tokio::spawn(async move {
                        let payload = tokio::time::timeout(Duration::from_secs(10), recv.read_to_end(MAX_STREAM_BYTES))
                            .await.map_err(|error| error.to_string())
                            .and_then(|result| result.map_err(|error| error.to_string()));
                        match payload {
                            Ok(payload) => {
                                // Clipboard tools, image decoding and file writes are blocking.
                                let accepted =
                                    tokio::task::block_in_place(|| on_stream(payload, remote));
                                let ack: &[u8] = if accepted { b"ok" } else { b"reject" };
                                let _ = send.write_all(ack).await;
                                let _ = send.finish();
                            }
                            Err(error) => {
                                log::warn!("QUIC stream read failed from {remote}: {error}");
                            }
                        }
                        drop(permit);
                    });
                }
                Err(error) => {
                    log::debug!("QUIC stream reader stopped for {remote}: {error}");
                    break;
                }
            }
        }
    });
}

/// Datagram send that never awaits: an established connection queues the
/// payload synchronously (quinn's `send_datagram` is not async); a missing or
/// dead connection kicks off a background connect. The caller receives false
/// during warmup and does not enter remote control until the peer is ready.
fn send_datagram_nonblocking(
    endpoint: &Endpoint,
    connections: &ConnectionMap,
    health: &HealthMap,
    peer: PeerEndpoint,
    payload: Vec<u8>,
) {
    let key = match peer_key(&peer) {
        Ok(key) => key,
        Err(error) => {
            record_peer_failure(health, &peer.addr, &error);
            return;
        }
    };

    let ready = {
        let Ok(mut map) = connections.lock() else {
            return;
        };
        match map.get(&key) {
            Some(ConnectionSlot::Ready(connection)) if connection.close_reason().is_none() => {
                Some(connection.clone())
            }
            // A background task is already dialing this peer; drop the payload.
            Some(ConnectionSlot::Connecting) => return,
            _ => {
                map.remove(&key);
                None
            }
        }
    };

    if let Some(connection) = ready {
        match connection.send_datagram(payload.into()) {
            Ok(()) => record_peer_success(health, &peer.addr),
            // TooLarge/Disabled say nothing about the peer; only a closed
            // connection is re-dialed and counted against its health.
            Err(error) if connection.close_reason().is_some() => {
                forget_connection(connections, &key, &connection);
                record_peer_failure(health, &peer.addr, &error.to_string());
            }
            Err(error) => log::debug!("QUIC datagram to {} not sent: {error}", peer.addr),
        }
        return;
    }

    // Known-dead peer inside its retry window: skip even the background dial
    // so an unreachable box costs nothing between probes.
    if peer_fast_fail_active(health, &peer.addr) {
        return;
    }
    if let Ok(mut map) = connections.lock() {
        map.insert(key.clone(), ConnectionSlot::Connecting);
    }
    let endpoint = endpoint.clone();
    let connections = Arc::clone(connections);
    let health = Arc::clone(health);
    tokio::spawn(async move {
        match establish_connection(&endpoint, &peer, &key).await {
            Ok(connection) => {
                if let Ok(mut map) = connections.lock() {
                    map.insert(key, ConnectionSlot::Ready(connection));
                }
                record_peer_success(&health, &peer.addr);
            }
            Err(error) => {
                if let Ok(mut map) = connections.lock() {
                    map.remove(&key);
                }
                record_peer_failure(&health, &peer.addr, &error);
            }
        }
    });
}

/// Stream send running inside its own task: reuses a ready connection or
/// dials one inline (a racing datagram dial at worst produces one redundant
/// connection that is dropped on replacement — streams are rare).
async fn send_stream_task(
    endpoint: &Endpoint,
    connections: &ConnectionMap,
    health: &HealthMap,
    peer: PeerEndpoint,
    payload: Vec<u8>,
) -> Result<(), String> {
    let key = peer_key(&peer)?;
    let existing = {
        let Ok(map) = connections.lock() else {
            return Err("QUIC connection map is poisoned".into());
        };
        match map.get(&key) {
            Some(ConnectionSlot::Ready(connection)) if connection.close_reason().is_none() => {
                Some(connection.clone())
            }
            _ => None,
        }
    };
    let connection = match existing {
        Some(connection) => connection,
        None => match establish_connection(endpoint, &peer, &key).await {
            Ok(connection) => {
                if let Ok(mut map) = connections.lock() {
                    map.insert(key.clone(), ConnectionSlot::Ready(connection.clone()));
                }
                record_peer_success(health, &peer.addr);
                connection
            }
            Err(error) => {
                record_peer_failure(health, &peer.addr, &error);
                return Err(error);
            }
        },
    };

    let result = send_stream_on_connection(connection.clone(), payload).await;
    // A peer's `reject` or a slow ack says nothing about the link that input
    // datagrams share with this stream; evicting it here bounced the user back
    // to local control mid-session. Forget the connection only once it is dead.
    if result.is_err() && connection.close_reason().is_some() {
        forget_connection(connections, &key, &connection);
    }
    result
}

/// Drop `connection` from the map unless a newer dial already replaced it.
fn forget_connection(connections: &ConnectionMap, key: &PeerKey, connection: &quinn::Connection) {
    if let Ok(mut map) = connections.lock() {
        if matches!(map.get(key), Some(ConnectionSlot::Ready(current)) if current.stable_id() == connection.stable_id())
        {
            map.remove(key);
        }
    }
}

async fn send_stream_on_connection(
    connection: quinn::Connection,
    payload: Vec<u8>,
) -> Result<(), String> {
    // The ack also waits for the payload tail to land and for the receiver to
    // apply it (image decode, pasteboard write): budget ~2 MB/s on top of 500 ms.
    let ack_timeout = Duration::from_millis(500 + (payload.len() / 2048) as u64);
    let (mut send, mut recv) = connection
        .open_bi()
        .await
        .map_err(|error| format!("failed to open QUIC stream: {error}"))?;
    send.write_all(&payload)
        .await
        .map_err(|error| format!("failed to write QUIC stream: {error}"))?;
    send.finish()
        .map_err(|error| format!("failed to finish QUIC stream: {error}"))?;
    match tokio::time::timeout(ack_timeout, recv.read_to_end(64)).await {
        Ok(Ok(bytes)) => verify_stream_ack(&bytes),
        Ok(Err(error)) => Err(format!("failed to read QUIC stream ack: {error}")),
        Err(_) => Err("QUIC stream ack timed out".into()),
    }
}

fn verify_stream_ack(bytes: &[u8]) -> Result<(), String> {
    if bytes == b"ok" {
        Ok(())
    } else {
        Err(format!(
            "{STREAM_REJECTED}: {}",
            String::from_utf8_lossy(bytes)
        ))
    }
}

async fn establish_connection(
    endpoint: &Endpoint,
    peer: &PeerEndpoint,
    key: &PeerKey,
) -> Result<quinn::Connection, String> {
    let config = client_config(peer)?;
    let connecting = endpoint
        .connect_with(config, key.addr, SERVER_NAME)
        .map_err(|error| format!("failed to start QUIC connection to {}: {error}", key.addr))?;
    tokio::time::timeout(Duration::from_secs(2), connecting)
        .await
        .map_err(|_| format!("QUIC connection to {} timed out", key.addr))?
        .map_err(|error| format!("failed to connect QUIC to {}: {error}", key.addr))
}

fn peer_key(peer: &PeerEndpoint) -> Result<PeerKey, String> {
    Ok(PeerKey {
        addr: resolve_peer_addr(&peer.addr)?,
        requested_addr: peer.addr.clone(),
        public_key: peer.public_key.clone(),
    })
}

fn resolve_peer_addr(addr: &str) -> Result<SocketAddr, String> {
    addr.to_socket_addrs()
        .map_err(|error| format!("invalid peer QUIC address {addr}: {error}"))?
        .next()
        .ok_or_else(|| format!("peer QUIC address {addr} did not resolve"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_health_fast_fails_after_threshold_and_recovers_on_success() {
        let health: HealthMap = Arc::new(Mutex::new(HashMap::new()));
        let addr = "10.0.0.9:47834";

        for strikes in 1..DATAGRAM_FAIL_THRESHOLD {
            record_peer_failure(&health, addr, "timeout");
            assert!(
                !peer_fast_fail_active(&health, addr),
                "{strikes} failures must not fast-fail yet"
            );
        }
        record_peer_failure(&health, addr, "timeout");
        assert!(
            peer_fast_fail_active(&health, addr),
            "reaching the threshold enters fast-fail"
        );
        assert!(
            !peer_fast_fail_active(&health, "10.0.0.8:47834"),
            "health is tracked per peer address"
        );

        record_peer_success(&health, addr);
        assert!(
            !peer_fast_fail_active(&health, addr),
            "one successful send clears the fast-fail state"
        );
    }

    fn make_cert() -> CertificateDer<'static> {
        rcgen::generate_simple_self_signed(vec!["mykvm.local".to_string()])
            .unwrap()
            .cert
            .der()
            .clone()
    }

    #[test]
    fn pinned_verifier_accepts_matching_cert_and_rejects_others() {
        let pinned = make_cert();
        let other = make_cert();
        let verifier = PinnedCertVerifier::new(pinned.clone());
        let name = ServerName::try_from("mykvm.local").unwrap();
        let now = UnixTime::now();

        assert!(
            verifier
                .verify_server_cert(&pinned, &[], &name, &[], now)
                .is_ok(),
            "the advertised certificate must be accepted"
        );
        assert!(
            verifier
                .verify_server_cert(&other, &[], &name, &[], now)
                .is_err(),
            "a different certificate must be rejected"
        );
    }

    #[test]
    fn client_config_builds_from_advertised_public_key() {
        let peer = PeerEndpoint {
            addr: "127.0.0.1:47834".to_string(),
            public_key: BASE64.encode(make_cert().as_ref()),
            protocol_version: PROTOCOL_VERSION,
        };
        assert!(client_config(&peer).is_ok());
    }

    #[test]
    fn client_config_rejects_protocol_version_mismatch() {
        let peer = PeerEndpoint {
            addr: "127.0.0.1:47834".to_string(),
            public_key: BASE64.encode(make_cert().as_ref()),
            protocol_version: PROTOCOL_VERSION + 1,
        };
        assert!(client_config(&peer).is_err());
    }

    #[test]
    fn stream_ack_rejects_non_ok_payloads() {
        assert!(verify_stream_ack(b"ok").is_ok());
        assert!(verify_stream_ack(b"reject").is_err());
    }

    #[test]
    fn peer_key_uses_resolved_addr_and_public_key() {
        let key = peer_key(&PeerEndpoint {
            addr: "127.0.0.1:47834".into(),
            public_key: "pinned-cert".into(),
            protocol_version: PROTOCOL_VERSION,
        })
        .expect("peer key");

        assert_eq!(key.addr, "127.0.0.1:47834".parse::<SocketAddr>().unwrap());
        assert_eq!(key.public_key, "pinned-cert");
    }

    #[test]
    fn quic_runtime_uses_small_worker_pool() {
        assert_eq!(QUIC_WORKER_THREADS, 2);
    }

    #[test]
    fn identity_is_stable_across_reloads() {
        let dir = std::env::temp_dir().join("mykvm-quic-identity-stability-test");
        let _ = fs::remove_dir_all(&dir);

        let first = load_or_create_identity(&dir).expect("first identity load");
        let second = load_or_create_identity(&dir).expect("second identity load");

        assert_eq!(
            first.public_key, second.public_key,
            "the advertised public key must survive a reload"
        );
        assert_eq!(first.cert_der, second.cert_der);
        assert_eq!(first.key_der, second.key_der);
        assert!(!first.public_key.is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn input_survives_slow_streams_and_idle_periods() {
        let free_port = || {
            std::net::UdpSocket::bind(("127.0.0.1", 0))
                .unwrap()
                .local_addr()
                .unwrap()
                .port()
        };
        let port = free_port();
        let dir =
            std::env::temp_dir().join(format!("mykvm-stream-input-{}-{port}", std::process::id()));
        let (entered_tx, entered_rx) = mpsc::channel();
        let (input_tx, input_rx) = mpsc::channel();
        let release = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let gate = Arc::clone(&release);
        let receiver = start(
            port,
            dir.join("receiver"),
            Arc::new(move |payload, _| {
                let _ = input_tx.send(payload);
            }),
            Arc::new(move |_, _| {
                let _ = entered_tx.send(());
                let _ = gate.1.wait_timeout_while(
                    gate.0.lock().unwrap(),
                    Duration::from_secs(5),
                    |released| !*released,
                );
                true
            }),
        )
        .unwrap();
        let sender = start(
            free_port(),
            dir.join("sender"),
            Arc::new(|_, _| {}),
            Arc::new(|_, _| true),
        )
        .unwrap();
        let peer = sender.peer(
            format!("127.0.0.1:{}", receiver.port()),
            receiver.public_key().into(),
            PROTOCOL_VERSION,
        );
        assert!(!sender
            .send_datagram(peer.clone(), b"warm".to_vec())
            .unwrap());
        let sends: Vec<_> = (0..QUIC_WORKER_THREADS)
            .map(|_| {
                let sender = sender.clone();
                let peer = peer.clone();
                thread::spawn(move || sender.send_stream_expect_ack(peer, b"clipboard".to_vec()))
            })
            .collect();
        let all_entered = (0..QUIC_WORKER_THREADS)
            .all(|_| entered_rx.recv_timeout(Duration::from_secs(2)).is_ok());
        assert!(sender
            .send_datagram(peer.clone(), b"input".to_vec())
            .unwrap());
        let input = input_rx.recv_timeout(Duration::from_millis(300));
        *release.0.lock().unwrap() = true;
        release.1.notify_all();
        for send in sends {
            let _ = send.join().unwrap();
        }
        // Longer than the configured 10-second idle timeout: keepalive must
        // preserve a ready connection without a fresh handshake on input.
        thread::sleep(Duration::from_secs(11));
        let ready_after_idle = sender.send_datagram(peer.clone(), b"after-idle".to_vec());
        let after_idle = input_rx.recv_timeout(Duration::from_millis(300));
        sender.stop_and_wait().unwrap();
        receiver.stop_and_wait().unwrap();
        let _ = fs::remove_dir_all(dir);
        assert!(
            all_entered,
            "both blocking clipboard handlers must be running"
        );
        assert_eq!(
            input.unwrap(),
            b"input",
            "input must arrive while stream handlers are blocked"
        );
        assert!(ready_after_idle.unwrap());
        assert_eq!(after_idle.unwrap(), b"after-idle");
    }

    #[test]
    fn stream_reject_keeps_input_connection() {
        let free_port = || {
            std::net::UdpSocket::bind(("127.0.0.1", 0))
                .unwrap()
                .local_addr()
                .unwrap()
                .port()
        };
        let dir = std::env::temp_dir().join(format!(
            "mykvm-stream-reject-{}-{}",
            std::process::id(),
            free_port()
        ));
        let (input_tx, input_rx) = mpsc::channel();
        // A receiver with clipboard sync off answers every stream `reject`.
        let receiver = start(
            free_port(),
            dir.join("receiver"),
            Arc::new(move |payload, _| {
                let _ = input_tx.send(payload);
            }),
            Arc::new(|_, _| false),
        )
        .unwrap();
        let sender = start(
            free_port(),
            dir.join("sender"),
            Arc::new(|_, _| {}),
            Arc::new(|_, _| true),
        )
        .unwrap();
        let peer = sender.peer(
            format!("127.0.0.1:{}", receiver.port()),
            receiver.public_key().into(),
            PROTOCOL_VERSION,
        );
        let deadline = Instant::now() + Duration::from_secs(3);
        while !sender.send_datagram(peer.clone(), b"warm".to_vec()).unwrap() {
            assert!(Instant::now() < deadline, "connection never became ready");
            thread::sleep(Duration::from_millis(20));
        }

        let rejected = sender.send_stream_expect_ack(peer.clone(), b"clipboard".to_vec());
        let ready = sender.send_datagram(peer.clone(), b"input".to_vec());
        let input = (0..50)
            .map_while(|_| input_rx.recv_timeout(Duration::from_millis(300)).ok())
            .find(|payload| payload == b"input");
        sender.stop_and_wait().unwrap();
        receiver.stop_and_wait().unwrap();
        let _ = fs::remove_dir_all(dir);

        assert!(rejected.unwrap_err().starts_with(STREAM_REJECTED));
        assert!(
            ready.unwrap(),
            "a rejected stream must not tear down the input connection"
        );
        assert_eq!(input.as_deref(), Some(&b"input"[..]));
    }

    #[test]
    fn stop_and_wait_releases_udp_port_before_returning() {
        let probe = std::net::UdpSocket::bind(("0.0.0.0", 0)).expect("reserve test port");
        let port = probe.local_addr().expect("test port address").port();
        drop(probe);
        let dir = std::env::temp_dir().join(format!(
            "mykvm-quic-shutdown-test-{}-{port}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);

        let handle = start(
            port,
            dir.clone(),
            Arc::new(|_, _| {}),
            Arc::new(|_, _| true),
        )
        .expect("start transport");
        let bound_port = handle.port();
        handle.stop_and_wait().expect("stop transport");

        let rebound = std::net::UdpSocket::bind(("0.0.0.0", bound_port))
            .expect("shutdown ack must wait for the UDP port to be released");
        drop(rebound);
        let _ = fs::remove_dir_all(&dir);
    }
}
