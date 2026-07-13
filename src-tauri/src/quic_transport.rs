use std::{
    collections::HashMap,
    fs,
    net::{SocketAddr, ToSocketAddrs},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc, Arc, Mutex,
    },
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
use tokio::sync::{mpsc as tokio_mpsc, Mutex as TokioMutex};

pub const PROTOCOL_VERSION: u16 = 1;

const SERVER_NAME: &str = "mykvm.local";
const MAX_DATAGRAM_BYTES: usize = 16 * 1024;
// Clipboard images are sent as RGBA base64 over streams. The clipboard module
// caps decoded images at 32 MiB, which becomes roughly 43 MiB on the wire.
pub(crate) const MAX_STREAM_BYTES: usize = 48 * 1024 * 1024;
const PORT_SCAN_COUNT: u16 = 64;
const QUIC_WORKER_THREADS: usize = 2;

type DatagramHandler = Arc<dyn Fn(Vec<u8>, SocketAddr) + Send + Sync + 'static>;
type StreamHandler = Arc<dyn Fn(Vec<u8>, SocketAddr) -> bool + Send + Sync + 'static>;

#[derive(Clone, Debug)]
pub struct PeerEndpoint {
    pub addr: String,
    pub public_key: String,
    pub protocol_version: u16,
}

/// Maximum consecutive datagram failures before `send_datagram` short-circuits
/// with an error so the input layer can release the cursor immediately.
const DATAGRAM_FAIL_THRESHOLD: u64 = 3;
const DATAGRAM_QUEUE_CAP: u64 = 32;
const DATAGRAM_HARD_QUEUE_CAP: u64 = 64;
const STREAM_QUEUE_CAP: u64 = 8;
const BULK_STREAM_QUEUE_CAP: u64 = 6;
const MAX_DATAGRAM_HEALTH_PEERS: usize = 64;

fn try_reserve_pending(pending: &AtomicU64, cap: u64) -> bool {
    pending
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            (current < cap).then_some(current + 1)
        })
        .is_ok()
}

fn release_pending(pending: &AtomicU64) {
    let _ = pending.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(current.saturating_sub(1))
    });
}

#[derive(Default)]
struct DatagramHealth {
    failures: HashMap<String, u64>,
}

impl DatagramHealth {
    fn should_fast_fail(&self, peer: &str) -> bool {
        self.failure_count(peer) >= DATAGRAM_FAIL_THRESHOLD
    }

    fn failure_count(&self, peer: &str) -> u64 {
        self.failures.get(peer).copied().unwrap_or(0)
    }

    fn record_failure(&mut self, peer: &str) -> u64 {
        if !self.failures.contains_key(peer) && self.failures.len() >= MAX_DATAGRAM_HEALTH_PEERS {
            if let Some(stale) = self.failures.keys().next().cloned() {
                self.failures.remove(&stale);
            }
        }
        let failures = self.failures.entry(peer.to_string()).or_default();
        *failures = failures.saturating_add(1);
        *failures
    }

    fn record_success(&mut self, peer: &str) {
        self.failures.remove(peer);
    }

    fn reset(&mut self) {
        self.failures.clear();
    }
}

fn datagram_health_key(peer: &PeerEndpoint) -> String {
    format!(
        "{}\0{}\0{}",
        peer.addr, peer.public_key, peer.protocol_version
    )
}

#[derive(Clone)]
pub struct TransportHandle {
    commands: tokio_mpsc::UnboundedSender<TransportCommand>,
    port: u16,
    public_key: String,
    /// Consecutive datagram send failures observed by the transport loop.
    /// Shared with the input layer so it can detect a dead peer without
    /// waiting for the async transport to propagate the error.
    datagram_health: Arc<Mutex<DatagramHealth>>,
    datagram_pending: Arc<AtomicU64>,
    stream_pending: Arc<AtomicU64>,
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

    pub fn send_datagram(&self, peer: PeerEndpoint, payload: Vec<u8>) -> Result<(), String> {
        self.send_datagram_with_policy(peer, payload, false)
    }

    pub fn send_datagram_lossy(&self, peer: PeerEndpoint, payload: Vec<u8>) -> Result<(), String> {
        self.send_datagram_with_policy(peer, payload, true)
    }

    fn send_datagram_with_policy(
        &self,
        peer: PeerEndpoint,
        payload: Vec<u8>,
        lossy: bool,
    ) -> Result<(), String> {
        if payload.len() > MAX_DATAGRAM_BYTES {
            return Err(format!(
                "QUIC datagram is too large: {} bytes",
                payload.len()
            ));
        }

        // Fast-fail: if the transport loop has observed multiple consecutive
        // datagram failures for the current peer, report the error immediately
        // so the input layer can release the cursor.
        let health_key = datagram_health_key(&peer);
        if self
            .datagram_health
            .lock()
            .map(|health| health.should_fast_fail(&health_key))
            .unwrap_or(false)
        {
            return Err(
                "QUIC datagram send failed: peer unreachable (consecutive failures)".to_string(),
            );
        }

        let cap = if lossy {
            DATAGRAM_QUEUE_CAP
        } else {
            DATAGRAM_HARD_QUEUE_CAP
        };
        if !try_reserve_pending(&self.datagram_pending, cap) {
            return if lossy {
                Ok(())
            } else {
                Err("QUIC input queue is full".into())
            };
        }

        if self
            .commands
            .send(TransportCommand::SendDatagram { peer, payload })
            .is_err()
        {
            release_pending(&self.datagram_pending);
            return Err("QUIC transport is stopped".into());
        }
        Ok(())
    }

    pub fn send_stream_expect_ack(
        &self,
        peer: PeerEndpoint,
        payload: Vec<u8>,
    ) -> Result<(), String> {
        self.send_stream_inner(peer, payload, true, false)
    }

    pub fn send_bulk_stream_expect_ack(
        &self,
        peer: PeerEndpoint,
        payload: Vec<u8>,
    ) -> Result<(), String> {
        self.send_stream_inner(peer, payload, true, true)
    }

    fn send_stream_inner(
        &self,
        peer: PeerEndpoint,
        payload: Vec<u8>,
        ack_required: bool,
        bulk: bool,
    ) -> Result<(), String> {
        if payload.len() > MAX_STREAM_BYTES {
            return Err(format!(
                "QUIC stream payload is too large: {} bytes",
                payload.len()
            ));
        }

        let queue_cap = if bulk {
            BULK_STREAM_QUEUE_CAP
        } else {
            STREAM_QUEUE_CAP
        };
        if !try_reserve_pending(&self.stream_pending, queue_cap) {
            return Err("QUIC reliable stream queue is full".into());
        }

        let (result_tx, result_rx) = mpsc::channel();
        if self
            .commands
            .send(TransportCommand::SendStream {
                peer,
                payload,
                ack_required,
                result: result_tx,
            })
            .is_err()
        {
            release_pending(&self.stream_pending);
            return Err("QUIC transport is stopped".into());
        }
        result_rx
            .recv_timeout(Duration::from_secs(3))
            .map_err(|_| "QUIC stream send timed out".to_string())?
    }

    /// Reset the datagram failure counter. Call this when a peer is
    /// re-discovered so subsequent sends are attempted immediately instead
    /// of being short-circuited.
    pub fn reset_datagram_failures(&self) {
        if let Ok(mut health) = self.datagram_health.lock() {
            health.reset();
        }
    }

    pub fn shutdown(&self) {
        let _ = self.commands.send(TransportCommand::Shutdown);
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
        ack_required: bool,
        result: mpsc::Sender<Result<(), String>>,
    },
    Shutdown,
}

#[derive(Clone, Hash, PartialEq, Eq)]
struct PeerKey {
    addr: SocketAddr,
    public_key: String,
}

type SharedConnections = Arc<TokioMutex<HashMap<PeerKey, quinn::Connection>>>;

pub fn start(
    preferred_port: u16,
    identity_dir: PathBuf,
    on_datagram: DatagramHandler,
    on_stream: StreamHandler,
) -> Result<TransportHandle, String> {
    // Load (or create-and-persist) this machine's transport identity *before*
    // spawning the runtime thread so a stable public key is reused across
    // restarts/updates. A churning key breaks the peer's certificate pinning
    // and its paired-controllers authorization until both sides re-pair.
    let identity = load_or_create_identity(&identity_dir)?;
    let (ready_tx, ready_rx) = mpsc::channel();
    let (command_tx, command_rx) = tokio_mpsc::unbounded_channel();
    let datagram_health = Arc::new(Mutex::new(DatagramHealth::default()));
    let datagram_health_inner = Arc::clone(&datagram_health);
    let datagram_pending = Arc::new(AtomicU64::new(0));
    let datagram_pending_inner = Arc::clone(&datagram_pending);
    let stream_pending = Arc::new(AtomicU64::new(0));
    let stream_pending_inner = Arc::clone(&stream_pending);

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

            runtime.block_on(run_transport(
                preferred_port,
                identity,
                command_rx,
                on_datagram,
                on_stream,
                ready_tx,
                datagram_health_inner,
                datagram_pending_inner,
                stream_pending_inner,
            ));
        })
        .map_err(|error| format!("failed to spawn QUIC transport thread: {error}"))?;

    let ready = ready_rx
        .recv_timeout(Duration::from_secs(3))
        .map_err(|_| "QUIC transport did not become ready".to_string())??;

    Ok(TransportHandle {
        commands: command_tx,
        port: ready.port,
        public_key: ready.public_key,
        datagram_health,
        datagram_pending,
        stream_pending,
    })
}

struct ReadyTransport {
    port: u16,
    public_key: String,
}

async fn run_transport(
    preferred_port: u16,
    identity: TransportIdentity,
    mut commands: tokio_mpsc::UnboundedReceiver<TransportCommand>,
    on_datagram: DatagramHandler,
    on_stream: StreamHandler,
    ready_tx: mpsc::Sender<Result<ReadyTransport, String>>,
    datagram_health: Arc<Mutex<DatagramHealth>>,
    datagram_pending: Arc<AtomicU64>,
    stream_pending: Arc<AtomicU64>,
) {
    let (endpoint, public_key) = match bind_endpoint(preferred_port, &identity) {
        Ok(bound) => bound,
        Err(error) => {
            let _ = ready_tx.send(Err(error));
            return;
        }
    };

    let port = match endpoint.local_addr() {
        Ok(addr) => addr.port(),
        Err(error) => {
            let _ = ready_tx.send(Err(format!("failed to read QUIC port: {error}")));
            return;
        }
    };

    let _ = ready_tx.send(Ok(ReadyTransport { port, public_key }));
    spawn_accept_loop(endpoint.clone(), on_datagram, on_stream);

    let connections: SharedConnections = Arc::new(TokioMutex::new(HashMap::new()));
    let mut last_datagram_fail_log: Option<Instant> = None;
    // Cache of peers whose connection recently failed, to avoid repeated 2s
    // timeout attempts on every mouse-move datagram.
    let mut datagram_fail_cache: HashMap<SocketAddr, Instant> = HashMap::new();
    while let Some(command) = commands.recv().await {
        match command {
            TransportCommand::SendDatagram { peer, payload } => {
                datagram_pending.fetch_sub(1, Ordering::Relaxed);
                let health_key = datagram_health_key(&peer);
                // Skip send attempt if this peer failed recently (within 5s).
                let skip = resolve_peer_addr(&peer.addr)
                    .ok()
                    .and_then(|addr| datagram_fail_cache.get(&addr))
                    .map(|t| t.elapsed() < Duration::from_secs(5))
                    .unwrap_or(false);
                let result = if skip {
                    Err("peer recently unreachable".to_string())
                } else {
                    send_datagram(&endpoint, &connections, peer.clone(), payload).await
                };
                if let Err(error) = result {
                    let count = datagram_health
                        .lock()
                        .map(|mut health| health.record_failure(&health_key))
                        .unwrap_or(1);
                    // Evict the dead connection so next attempt reconnects.
                    if let Ok(addr) = resolve_peer_addr(&peer.addr) {
                        let key = PeerKey {
                            addr,
                            public_key: peer.public_key.clone(),
                        };
                        connections.lock().await.remove(&key);
                        datagram_fail_cache.insert(addr, Instant::now());
                    }
                    // Rate-limit the warning log.
                    let should_log = last_datagram_fail_log
                        .map(|t| t.elapsed() > Duration::from_secs(10))
                        .unwrap_or(true);
                    if should_log {
                        log::warn!("QUIC datagram send failed (x{count}): {error}");
                        last_datagram_fail_log = Some(Instant::now());
                    }
                } else {
                    if let Ok(mut health) = datagram_health.lock() {
                        health.record_success(&health_key);
                    }
                    // Clear fail cache on success so we try normally next time.
                    if let Ok(addr) = resolve_peer_addr(&peer.addr) {
                        datagram_fail_cache.remove(&addr);
                    }
                }
            }
            TransportCommand::SendStream {
                peer,
                payload,
                ack_required,
                result,
            } => {
                let endpoint = endpoint.clone();
                let connections = Arc::clone(&connections);
                let stream_pending = Arc::clone(&stream_pending);
                tokio::spawn(async move {
                    let send_result = send_stream(
                        &endpoint,
                        &connections,
                        peer.clone(),
                        payload.clone(),
                        ack_required,
                    )
                    .await;
                    let send_result = if send_result.is_err() {
                        if let Ok(key) = peer_key(&peer) {
                            connections.lock().await.remove(&key);
                        }
                        log::info!("QUIC stream send retry after connection eviction");
                        send_stream(&endpoint, &connections, peer, payload, ack_required).await
                    } else {
                        send_result
                    };
                    if let Err(error) = &send_result {
                        log::warn!("QUIC stream send failed: {error}");
                    }
                    release_pending(&stream_pending);
                    let _ = result.send(send_result);
                });
            }
            TransportCommand::Shutdown => break,
        }
    }

    endpoint.close(0_u32.into(), b"shutdown");
    endpoint.wait_idle().await;
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
    if let Ok(timeout) = quinn::IdleTimeout::try_from(Duration::from_secs(10)) {
        transport.max_idle_timeout(Some(timeout));
    }
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

fn spawn_accept_loop(endpoint: Endpoint, on_datagram: DatagramHandler, on_stream: StreamHandler) {
    tokio::spawn(async move {
        let mut last_fail_log: HashMap<SocketAddr, Instant> = HashMap::new();
        while let Some(incoming) = endpoint.accept().await {
            let remote = incoming.remote_address();
            let on_datagram = Arc::clone(&on_datagram);
            let on_stream = Arc::clone(&on_stream);

            let should_log = last_fail_log
                .get(&remote)
                .map(|t| t.elapsed() > Duration::from_secs(10))
                .unwrap_or(true);

            tokio::spawn(async move {
                match incoming.await {
                    Ok(connection) => {
                        spawn_datagram_reader(connection.clone(), remote, on_datagram);
                        spawn_stream_reader(connection, remote, on_stream);
                    }
                    Err(error) => {
                        if should_log {
                            log::warn!("QUIC incoming connection failed from {remote}: {error}");
                        }
                    }
                }
            });

            // Rate-limit failure logs per remote address.
            if should_log {
                last_fail_log.insert(remote, Instant::now());
            }
            // Prevent unbounded growth of the log-rate map.
            if last_fail_log.len() > 64 {
                let cutoff = Instant::now() - Duration::from_secs(60);
                last_fail_log.retain(|_, t| *t > cutoff);
            }
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
                Ok(payload) => on_datagram(payload.to_vec(), remote),
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
) {
    tokio::spawn(async move {
        loop {
            match connection.accept_bi().await {
                Ok((mut send, mut recv)) => {
                    let on_stream = Arc::clone(&on_stream);
                    tokio::spawn(async move {
                        match recv.read_to_end(MAX_STREAM_BYTES).await {
                            Ok(payload) => {
                                let accepted = on_stream(payload, remote);
                                let ack: &[u8] = if accepted { b"ok" } else { b"reject" };
                                let _ = send.write_all(ack).await;
                                let _ = send.finish();
                            }
                            Err(error) => {
                                log::warn!("QUIC stream read failed from {remote}: {error}");
                            }
                        }
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

async fn send_datagram(
    endpoint: &Endpoint,
    connections: &SharedConnections,
    peer: PeerEndpoint,
    payload: Vec<u8>,
) -> Result<(), String> {
    let (key, connection) = connection_for(endpoint, connections, &peer).await?;
    match connection.send_datagram(payload.into()) {
        Ok(()) => Ok(()),
        Err(error) => {
            connections.lock().await.remove(&key);
            Err(error.to_string())
        }
    }
}

async fn send_stream(
    endpoint: &Endpoint,
    connections: &SharedConnections,
    peer: PeerEndpoint,
    payload: Vec<u8>,
    ack_required: bool,
) -> Result<(), String> {
    let (key, connection) = connection_for(endpoint, connections, &peer).await?;
    let result = send_stream_on_connection(connection, payload, ack_required).await;
    if result.is_err() {
        connections.lock().await.remove(&key);
    }
    result
}

async fn send_stream_on_connection(
    connection: quinn::Connection,
    payload: Vec<u8>,
    ack_required: bool,
) -> Result<(), String> {
    let (mut send, mut recv) = connection
        .open_bi()
        .await
        .map_err(|error| format!("failed to open QUIC stream: {error}"))?;
    send.write_all(&payload)
        .await
        .map_err(|error| format!("failed to write QUIC stream: {error}"))?;
    send.finish()
        .map_err(|error| format!("failed to finish QUIC stream: {error}"))?;
    let ack = tokio::time::timeout(Duration::from_millis(500), recv.read_to_end(64)).await;
    if ack_required {
        match ack {
            Ok(Ok(bytes)) => verify_stream_ack(&bytes)?,
            Ok(Err(error)) => {
                return Err(format!("failed to read QUIC stream ack: {error}"));
            }
            Err(_) => {
                return Err("QUIC stream ack timed out".into());
            }
        }
    }
    Ok(())
}

fn verify_stream_ack(bytes: &[u8]) -> Result<(), String> {
    if bytes == b"ok" {
        Ok(())
    } else {
        Err(format!(
            "QUIC stream receiver rejected payload: {}",
            String::from_utf8_lossy(bytes)
        ))
    }
}

async fn connection_for(
    endpoint: &Endpoint,
    connections: &SharedConnections,
    peer: &PeerEndpoint,
) -> Result<(PeerKey, quinn::Connection), String> {
    let key = peer_key(peer)?;

    {
        let mut connections = connections.lock().await;
        if let Some(connection) = connections.get(&key) {
            if connection.close_reason().is_none() {
                return Ok((key, connection.clone()));
            }
        }
        connections.remove(&key);
    }

    let config = client_config(peer)?;
    let connecting = endpoint
        .connect_with(config, key.addr, SERVER_NAME)
        .map_err(|error| format!("failed to start QUIC connection to {}: {error}", key.addr))?;
    let connection = tokio::time::timeout(Duration::from_secs(2), connecting)
        .await
        .map_err(|_| format!("QUIC connection to {} timed out", key.addr))?
        .map_err(|error| format!("failed to connect QUIC to {}: {error}", key.addr))?;
    connections
        .lock()
        .await
        .insert(key.clone(), connection.clone());
    Ok((key, connection))
}

fn peer_key(peer: &PeerEndpoint) -> Result<PeerKey, String> {
    Ok(PeerKey {
        addr: resolve_peer_addr(&peer.addr)?,
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
    fn datagram_health_is_isolated_per_peer() {
        let mut health = DatagramHealth::default();
        for _ in 0..DATAGRAM_FAIL_THRESHOLD {
            health.record_failure("peer-a");
        }

        assert!(health.should_fast_fail("peer-a"));
        assert!(!health.should_fast_fail("peer-b"));

        health.record_failure("peer-b");
        health.record_success("peer-a");

        assert!(!health.should_fast_fail("peer-a"));
        assert_eq!(health.failure_count("peer-b"), 1);
    }

    #[test]
    fn pending_budget_is_bounded_and_reusable() {
        let pending = AtomicU64::new(0);

        assert!(try_reserve_pending(&pending, 2));
        assert!(try_reserve_pending(&pending, 2));
        assert!(!try_reserve_pending(&pending, 2));

        release_pending(&pending);
        assert!(try_reserve_pending(&pending, 2));
        assert_eq!(pending.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn bulk_streams_leave_capacity_for_priority_streams() {
        let pending = AtomicU64::new(0);
        for _ in 0..BULK_STREAM_QUEUE_CAP {
            assert!(try_reserve_pending(&pending, BULK_STREAM_QUEUE_CAP));
        }
        assert!(!try_reserve_pending(&pending, BULK_STREAM_QUEUE_CAP));

        assert!(try_reserve_pending(&pending, STREAM_QUEUE_CAP));
        assert!(try_reserve_pending(&pending, STREAM_QUEUE_CAP));
        assert!(!try_reserve_pending(&pending, STREAM_QUEUE_CAP));
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
}
