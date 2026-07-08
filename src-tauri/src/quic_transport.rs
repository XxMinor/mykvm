use std::{
    collections::HashMap,
    fs,
    net::{SocketAddr, ToSocketAddrs},
    path::{Path, PathBuf},
    sync::{mpsc, Arc},
    thread,
    time::Duration,
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

// v2: key and mouse-button events moved from unreliable datagrams onto a
// persistent reliable ordered stream (a dropped KeyUp no longer sticks a key).
// The version check is strict on both ends, so v1 and v2 peers do not connect
// or control each other — both sides must run a v2 build.
pub const PROTOCOL_VERSION: u16 = 2;

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

#[derive(Clone)]
pub struct TransportHandle {
    datagram_commands: tokio_mpsc::UnboundedSender<DatagramCommand>,
    stream_commands: tokio_mpsc::UnboundedSender<StreamCommand>,
    port: u16,
    public_key: String,
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
        self.send_datagram_inner(peer, payload, DatagramMode::Ordered)
    }

    pub fn send_latest_datagram(&self, peer: PeerEndpoint, payload: Vec<u8>) -> Result<(), String> {
        self.send_datagram_inner(peer, payload, DatagramMode::Latest)
    }

    /// Send an input event that must arrive intact and in order (key / mouse
    /// button up/down). Rides the peer's persistent reliable stream. Enqueue is
    /// fire-and-forget; a genuinely dead peer still surfaces via the datagram
    /// path's offline marking, so callers treat `Ok` as "queued", like datagrams.
    pub fn send_reliable_input(&self, peer: PeerEndpoint, payload: Vec<u8>) -> Result<(), String> {
        if payload.len() > MAX_DATAGRAM_BYTES {
            return Err(format!(
                "QUIC reliable input is too large: {} bytes",
                payload.len()
            ));
        }

        self.datagram_commands
            .send(DatagramCommand::SendReliableInput { peer, payload })
            .map_err(|_| "QUIC transport is stopped".to_string())
    }

    fn send_datagram_inner(
        &self,
        peer: PeerEndpoint,
        payload: Vec<u8>,
        mode: DatagramMode,
    ) -> Result<(), String> {
        if payload.len() > MAX_DATAGRAM_BYTES {
            return Err(format!(
                "QUIC datagram is too large: {} bytes",
                payload.len()
            ));
        }

        self.datagram_commands
            .send(DatagramCommand::SendDatagram {
                peer,
                payload,
                mode,
            })
            .map_err(|_| "QUIC transport is stopped".to_string())
    }

    pub fn send_stream_expect_ack(
        &self,
        peer: PeerEndpoint,
        payload: Vec<u8>,
    ) -> Result<(), String> {
        self.send_stream_inner(peer, payload, true)
    }

    fn send_stream_inner(
        &self,
        peer: PeerEndpoint,
        payload: Vec<u8>,
        ack_required: bool,
    ) -> Result<(), String> {
        if payload.len() > MAX_STREAM_BYTES {
            return Err(format!(
                "QUIC stream payload is too large: {} bytes",
                payload.len()
            ));
        }

        let (result_tx, result_rx) = mpsc::channel();
        self.stream_commands
            .send(StreamCommand::SendStream {
                peer,
                payload,
                ack_required,
                result: result_tx,
            })
            .map_err(|_| "QUIC transport is stopped".to_string())?;
        result_rx
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| "QUIC stream send timed out".to_string())?
    }

    pub fn shutdown(&self) {
        let _ = self.datagram_commands.send(DatagramCommand::Shutdown);
        let _ = self.stream_commands.send(StreamCommand::Shutdown);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DatagramMode {
    Ordered,
    Latest,
}

enum DatagramCommand {
    SendDatagram {
        peer: PeerEndpoint,
        payload: Vec<u8>,
        mode: DatagramMode,
    },
    SendReliableInput {
        peer: PeerEndpoint,
        payload: Vec<u8>,
    },
    Shutdown,
}

enum StreamCommand {
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
    let (datagram_tx, datagram_rx) = tokio_mpsc::unbounded_channel();
    let (stream_tx, stream_rx) = tokio_mpsc::unbounded_channel();

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
                datagram_rx,
                stream_rx,
                on_datagram,
                on_stream,
                ready_tx,
            ));
        })
        .map_err(|error| format!("failed to spawn QUIC transport thread: {error}"))?;

    let ready = ready_rx
        .recv_timeout(Duration::from_secs(3))
        .map_err(|_| "QUIC transport did not become ready".to_string())??;

    Ok(TransportHandle {
        datagram_commands: datagram_tx,
        stream_commands: stream_tx,
        port: ready.port,
        public_key: ready.public_key,
    })
}

struct ReadyTransport {
    port: u16,
    public_key: String,
}

async fn run_transport(
    preferred_port: u16,
    identity: TransportIdentity,
    datagram_commands: tokio_mpsc::UnboundedReceiver<DatagramCommand>,
    stream_commands: tokio_mpsc::UnboundedReceiver<StreamCommand>,
    on_datagram: DatagramHandler,
    on_stream: StreamHandler,
    ready_tx: mpsc::Sender<Result<ReadyTransport, String>>,
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

    let datagram_task = tokio::spawn(run_datagram_commands(endpoint.clone(), datagram_commands));
    let stream_task = tokio::spawn(run_stream_commands(endpoint.clone(), stream_commands));
    let _ = datagram_task.await;
    let _ = stream_task.await;

    endpoint.close(0_u32.into(), b"shutdown");
    endpoint.wait_idle().await;
}

async fn run_datagram_commands(
    endpoint: Endpoint,
    mut commands: tokio_mpsc::UnboundedReceiver<DatagramCommand>,
) {
    let mut connections: HashMap<PeerKey, quinn::Connection> = HashMap::new();
    // One persistent reliable send stream per peer for key / mouse-button
    // events. ponytail: shared with the datagram loop, so a peer whose stream
    // back-pressures (receiver not draining) can stall other peers' input for
    // up to the write timeout — split into a per-peer task if that ever bites.
    let mut key_streams: HashMap<PeerKey, quinn::SendStream> = HashMap::new();
    let mut deferred_command = None;
    loop {
        let command = match deferred_command.take() {
            Some(command) => command,
            None => match commands.recv().await {
                Some(command) => command,
                None => break,
            },
        };
        match command {
            DatagramCommand::SendDatagram {
                peer,
                payload,
                mode,
            } => {
                let (send_result, deferred) = if mode == DatagramMode::Latest {
                    send_latest_datagram(&endpoint, &mut connections, &mut commands, peer, payload)
                        .await
                } else {
                    (
                        send_datagram(&endpoint, &mut connections, peer, payload).await,
                        None,
                    )
                };
                deferred_command = deferred;
                if let Err(error) = send_result {
                    log::warn!("QUIC datagram send failed: {error}");
                }
            }
            DatagramCommand::SendReliableInput { peer, payload } => {
                if let Err(error) = send_reliable_input(
                    &endpoint,
                    &mut connections,
                    &mut key_streams,
                    peer,
                    payload,
                )
                .await
                {
                    log::warn!("QUIC reliable input send failed: {error}");
                }
            }
            DatagramCommand::Shutdown => break,
        }
    }
}

async fn run_stream_commands(
    endpoint: Endpoint,
    mut commands: tokio_mpsc::UnboundedReceiver<StreamCommand>,
) {
    let mut connections: HashMap<PeerKey, quinn::Connection> = HashMap::new();
    while let Some(command) = commands.recv().await {
        match command {
            StreamCommand::SendStream {
                peer,
                payload,
                ack_required,
                result,
            } => {
                let send_result =
                    send_stream(&endpoint, &mut connections, peer, payload, ack_required).await;
                if let Err(error) = &send_result {
                    log::warn!("QUIC stream send failed: {error}");
                }
                let _ = result.send(send_result);
            }
            StreamCommand::Shutdown => break,
        }
    }
}

fn drain_latest_datagram(
    commands: &mut tokio_mpsc::UnboundedReceiver<DatagramCommand>,
    mut peer: PeerEndpoint,
    mut payload: Vec<u8>,
) -> (PeerEndpoint, Vec<u8>, Option<DatagramCommand>) {
    loop {
        match commands.try_recv() {
            Ok(DatagramCommand::SendDatagram {
                peer: next_peer,
                payload: next_payload,
                mode: DatagramMode::Latest,
            }) if same_peer_endpoint(&peer, &next_peer) => {
                peer = next_peer;
                payload = next_payload;
            }
            Ok(other) => return (peer, payload, Some(other)),
            Err(tokio_mpsc::error::TryRecvError::Empty)
            | Err(tokio_mpsc::error::TryRecvError::Disconnected) => {
                return (peer, payload, None);
            }
        }
    }
}

fn same_peer_endpoint(left: &PeerEndpoint, right: &PeerEndpoint) -> bool {
    left.addr == right.addr
        && left.public_key == right.public_key
        && left.protocol_version == right.protocol_version
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
    // Input rides datagrams; when the congestion window collapses (Wi-Fi
    // hiccup) quinn buffers outgoing datagrams instead of dropping them. The
    // default 1 MiB buffer holds seconds of stale mouse motion that then
    // replays as a burst once the link recovers — the "freezes, then the
    // cursor flies on its own" feel. 64 KiB bounds that staleness to a couple
    // hundred events; older ones are dropped, which is correct for motion.
    // ponytail: a lost KeyUp under heavy congestion can still stick a key —
    // routing keys over a reliable stream (backlog F4) is the real fix.
    transport.datagram_send_buffer_size(64 * 1024);
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
        while let Some(incoming) = endpoint.accept().await {
            let remote = incoming.remote_address();
            let on_datagram = Arc::clone(&on_datagram);
            let on_stream = Arc::clone(&on_stream);

            tokio::spawn(async move {
                match incoming.await {
                    Ok(connection) => {
                        spawn_datagram_reader(connection.clone(), remote, Arc::clone(&on_datagram));
                        spawn_uni_input_reader(connection.clone(), remote, on_datagram);
                        spawn_stream_reader(connection, remote, on_stream);
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
                Ok(payload) => on_datagram(payload.to_vec(), remote),
                Err(error) => {
                    log::debug!("QUIC datagram reader stopped for {remote}: {error}");
                    break;
                }
            }
        }
    });
}

/// Reads reliable input frames off the peer's persistent uni streams and hands
/// each event to the same handler as datagrams (identical InputPacket payload),
/// so authorization and injection are unchanged — only the delivery guarantee
/// differs.
fn spawn_uni_input_reader(
    connection: quinn::Connection,
    remote: SocketAddr,
    on_datagram: DatagramHandler,
) {
    tokio::spawn(async move {
        loop {
            match connection.accept_uni().await {
                Ok(mut recv) => {
                    let on_datagram = Arc::clone(&on_datagram);
                    tokio::spawn(async move {
                        loop {
                            let mut len_bytes = [0_u8; 4];
                            if recv.read_exact(&mut len_bytes).await.is_err() {
                                break;
                            }
                            let len = u32::from_le_bytes(len_bytes) as usize;
                            if len == 0 || len > MAX_DATAGRAM_BYTES {
                                log::warn!(
                                    "reliable input frame from {remote} has bad length {len}"
                                );
                                break;
                            }
                            let mut payload = vec![0_u8; len];
                            if recv.read_exact(&mut payload).await.is_err() {
                                break;
                            }
                            on_datagram(payload, remote);
                        }
                    });
                }
                Err(error) => {
                    log::debug!("QUIC uni input reader stopped for {remote}: {error}");
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
    connections: &mut HashMap<PeerKey, quinn::Connection>,
    peer: PeerEndpoint,
    payload: Vec<u8>,
) -> Result<(), String> {
    let (key, connection) = connection_for(endpoint, connections, &peer).await?;
    match connection.send_datagram(payload.into()) {
        Ok(()) => Ok(()),
        Err(error) => {
            connections.remove(&key);
            Err(error.to_string())
        }
    }
}

/// Length-prefixed frame for one reliable input event (u32-le length + body).
/// Multiple frames share one persistent stream, so the receiver needs the
/// prefix to know where each event ends.
fn encode_reliable_input_frame(payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(payload);
    frame
}

async fn send_reliable_input(
    endpoint: &Endpoint,
    connections: &mut HashMap<PeerKey, quinn::Connection>,
    key_streams: &mut HashMap<PeerKey, quinn::SendStream>,
    peer: PeerEndpoint,
    payload: Vec<u8>,
) -> Result<(), String> {
    let key = peer_key(&peer)?;

    // Reuse (or lazily open) the peer's persistent uni stream so KeyDown/KeyUp
    // stay ordered — separate streams would not guarantee delivery order.
    if !key_streams.contains_key(&key) {
        let (_key, connection) = connection_for(endpoint, connections, &peer).await?;
        let stream = connection
            .open_uni()
            .await
            .map_err(|error| format!("failed to open reliable input stream: {error}"))?;
        key_streams.insert(key.clone(), stream);
    }
    let stream = key_streams
        .get_mut(&key)
        .expect("reliable input stream just inserted");

    let frame = encode_reliable_input_frame(&payload);
    // Bound one slow peer's back-pressure so it cannot freeze every peer's
    // input on the shared loop. A dropped stream is reopened on the next event.
    match tokio::time::timeout(Duration::from_secs(1), stream.write_all(&frame)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => {
            key_streams.remove(&key);
            Err(format!("failed to write reliable input stream: {error}"))
        }
        Err(_) => {
            key_streams.remove(&key);
            Err("reliable input stream write timed out".into())
        }
    }
}

async fn send_latest_datagram(
    endpoint: &Endpoint,
    connections: &mut HashMap<PeerKey, quinn::Connection>,
    commands: &mut tokio_mpsc::UnboundedReceiver<DatagramCommand>,
    peer: PeerEndpoint,
    payload: Vec<u8>,
) -> (Result<(), String>, Option<DatagramCommand>) {
    let (peer, mut payload, mut deferred) = drain_latest_datagram(commands, peer, payload);
    let (key, connection) = match connection_for(endpoint, connections, &peer).await {
        Ok(connection) => connection,
        Err(error) => return (Err(error), deferred),
    };

    if deferred.is_none() {
        let (_peer, latest_payload, latest_deferred) =
            drain_latest_datagram(commands, peer, payload);
        payload = latest_payload;
        deferred = latest_deferred;
    }

    let result = match connection.send_datagram(payload.into()) {
        Ok(()) => Ok(()),
        Err(error) => {
            connections.remove(&key);
            Err(error.to_string())
        }
    };
    (result, deferred)
}

async fn send_stream(
    endpoint: &Endpoint,
    connections: &mut HashMap<PeerKey, quinn::Connection>,
    peer: PeerEndpoint,
    payload: Vec<u8>,
    ack_required: bool,
) -> Result<(), String> {
    let (key, connection) = connection_for(endpoint, connections, &peer).await?;
    let result = send_stream_on_connection(connection, payload, ack_required).await;
    if result.is_err() {
        connections.remove(&key);
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
    connections: &mut HashMap<PeerKey, quinn::Connection>,
    peer: &PeerEndpoint,
) -> Result<(PeerKey, quinn::Connection), String> {
    let key = peer_key(peer)?;

    if let Some(connection) = connections.get(&key) {
        if connection.close_reason().is_none() {
            return Ok((key, connection.clone()));
        }
    }
    connections.remove(&key);

    let config = client_config(peer)?;
    let connecting = endpoint
        .connect_with(config, key.addr, SERVER_NAME)
        .map_err(|error| format!("failed to start QUIC connection to {}: {error}", key.addr))?;
    let connection = tokio::time::timeout(Duration::from_secs(2), connecting)
        .await
        .map_err(|_| format!("QUIC connection to {} timed out", key.addr))?
        .map_err(|error| format!("failed to connect QUIC to {}: {error}", key.addr))?;
    connections.insert(key.clone(), connection.clone());
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

    fn peer(addr: &str) -> PeerEndpoint {
        PeerEndpoint {
            addr: addr.to_string(),
            public_key: "pinned-cert".into(),
            protocol_version: PROTOCOL_VERSION,
        }
    }

    fn recv_stream_command(rx: &mut tokio_mpsc::UnboundedReceiver<StreamCommand>) -> StreamCommand {
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        loop {
            match rx.try_recv() {
                Ok(command) => return command,
                Err(tokio_mpsc::error::TryRecvError::Empty)
                    if std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("stream command not received: {error:?}"),
            }
        }
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
    fn latest_datagram_drops_stale_moves_until_ordered_boundary() {
        let (tx, mut rx) = tokio_mpsc::unbounded_channel();
        let target = peer("127.0.0.1:47834");

        tx.send(DatagramCommand::SendDatagram {
            peer: target.clone(),
            payload: b"middle".to_vec(),
            mode: DatagramMode::Latest,
        })
        .unwrap();
        tx.send(DatagramCommand::SendDatagram {
            peer: target.clone(),
            payload: b"latest".to_vec(),
            mode: DatagramMode::Latest,
        })
        .unwrap();
        tx.send(DatagramCommand::SendDatagram {
            peer: target.clone(),
            payload: b"button".to_vec(),
            mode: DatagramMode::Ordered,
        })
        .unwrap();

        let (_, payload, deferred) = drain_latest_datagram(&mut rx, target, b"oldest".to_vec());

        assert_eq!(payload, b"latest");
        match deferred.expect("ordered command should be preserved") {
            DatagramCommand::SendDatagram { payload, mode, .. } => {
                assert_eq!(payload, b"button");
                assert_eq!(mode, DatagramMode::Ordered);
            }
            _ => panic!("expected deferred datagram"),
        }
    }

    #[test]
    fn latest_datagram_keeps_other_peer_order() {
        let (tx, mut rx) = tokio_mpsc::unbounded_channel();
        let first_peer = peer("127.0.0.1:47834");
        let second_peer = peer("127.0.0.1:47835");

        tx.send(DatagramCommand::SendDatagram {
            peer: second_peer.clone(),
            payload: b"other".to_vec(),
            mode: DatagramMode::Latest,
        })
        .unwrap();

        let (_, payload, deferred) = drain_latest_datagram(&mut rx, first_peer, b"first".to_vec());

        assert_eq!(payload, b"first");
        match deferred.expect("other peer command should be preserved") {
            DatagramCommand::SendDatagram {
                peer,
                payload,
                mode,
            } => {
                assert!(same_peer_endpoint(&peer, &second_peer));
                assert_eq!(payload, b"other");
                assert_eq!(mode, DatagramMode::Latest);
            }
            _ => panic!("expected deferred datagram"),
        }
    }

    #[test]
    fn transport_handle_routes_datagrams_and_streams_to_separate_queues() {
        let (datagram_tx, mut datagram_rx) = tokio_mpsc::unbounded_channel();
        let (stream_tx, mut stream_rx) = tokio_mpsc::unbounded_channel();
        let handle = TransportHandle {
            datagram_commands: datagram_tx,
            stream_commands: stream_tx,
            port: 47834,
            public_key: "local-cert".into(),
        };
        let target = peer("127.0.0.1:47834");

        handle
            .send_datagram(target.clone(), b"move".to_vec())
            .expect("datagram enqueue");
        match datagram_rx.try_recv().expect("datagram command") {
            DatagramCommand::SendDatagram { payload, mode, .. } => {
                assert_eq!(payload, b"move");
                assert_eq!(mode, DatagramMode::Ordered);
            }
            DatagramCommand::SendReliableInput { .. } => panic!("unexpected reliable input"),
            DatagramCommand::Shutdown => panic!("unexpected shutdown"),
        }
        assert!(stream_rx.try_recv().is_err());

        let stream_handle = handle.clone();
        let stream_peer = target.clone();
        let join = std::thread::spawn(move || {
            stream_handle.send_stream_expect_ack(stream_peer, b"clipboard".to_vec())
        });
        let result = match recv_stream_command(&mut stream_rx) {
            StreamCommand::SendStream {
                payload, result, ..
            } => {
                assert_eq!(payload, b"clipboard");
                result.send(Ok(())).expect("return stream result");
                join.join().expect("stream sender thread")
            }
            StreamCommand::Shutdown => panic!("unexpected shutdown"),
        };

        assert!(result.is_ok());
        assert!(datagram_rx.try_recv().is_err());

        // A reliable input event lands on the datagram/input queue as a distinct
        // command — never on the block-transfer stream queue.
        handle
            .send_reliable_input(target.clone(), b"key".to_vec())
            .expect("reliable input enqueue");
        match datagram_rx.try_recv().expect("reliable input command") {
            DatagramCommand::SendReliableInput { payload, .. } => assert_eq!(payload, b"key"),
            _ => panic!("expected a reliable input command"),
        }
        assert!(stream_rx.try_recv().is_err());
    }

    #[test]
    fn reliable_input_frames_split_back_out_of_a_shared_stream() {
        let first = b"key-down".to_vec();
        let second = b"key-up".to_vec();
        let mut stream = encode_reliable_input_frame(&first);
        stream.extend(encode_reliable_input_frame(&second));

        // Mirror the receiver: u32-le length prefix, then that many body bytes,
        // repeated. Proves the prefix framing recovers exact event boundaries
        // even when several events are coalesced on one persistent stream.
        let mut frames = Vec::new();
        let mut cursor = 0;
        while cursor + 4 <= stream.len() {
            let len = u32::from_le_bytes(stream[cursor..cursor + 4].try_into().unwrap()) as usize;
            cursor += 4;
            frames.push(stream[cursor..cursor + len].to_vec());
            cursor += len;
        }

        assert_eq!(frames, vec![first, second]);
        assert_eq!(cursor, stream.len());
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

    // Real end-to-end check for the F4 reliable-input path: two live QUIC
    // endpoints on loopback, one sends key frames over the persistent uni
    // stream, the other must receive them intact and in order. #[ignore] so CI
    // stays deterministic (binds real UDP ports); run with
    // `cargo test -- --ignored reliable_input_delivers`.
    #[test]
    #[ignore = "spins up two real QUIC endpoints on loopback"]
    fn reliable_input_delivers_end_to_end_in_order() {
        let dir_a = std::env::temp_dir().join("mykvm-reliable-input-test-a");
        let dir_b = std::env::temp_dir().join("mykvm-reliable-input-test-b");
        let _ = fs::remove_dir_all(&dir_a);
        let _ = fs::remove_dir_all(&dir_b);

        // Distinct preferred ports spaced beyond PORT_SCAN_COUNT so the two
        // endpoints never land on the same reuse-port socket (port 0 would make
        // both scan from 1024 and collide).
        let noop_datagram: DatagramHandler = Arc::new(|_, _| {});
        let noop_stream: StreamHandler = Arc::new(|_, _| false);
        let transport_a = start(48200, dir_a.clone(), noop_datagram, noop_stream).expect("start A");

        let (tx, rx) = mpsc::channel();
        let received: DatagramHandler = Arc::new(move |payload, _| {
            let _ = tx.send(payload);
        });
        let noop_stream_b: StreamHandler = Arc::new(|_, _| false);
        let transport_b = start(48400, dir_b.clone(), received, noop_stream_b).expect("start B");

        let peer_b = transport_a.peer(
            format!("127.0.0.1:{}", transport_b.port()),
            transport_b.public_key().to_string(),
            PROTOCOL_VERSION,
        );

        transport_a
            .send_reliable_input(peer_b.clone(), b"key-down".to_vec())
            .expect("enqueue key-down");
        transport_a
            .send_reliable_input(peer_b, b"key-up".to_vec())
            .expect("enqueue key-up");

        let first = rx.recv_timeout(Duration::from_secs(5)).expect("key-down");
        let second = rx.recv_timeout(Duration::from_secs(5)).expect("key-up");
        assert_eq!(first, b"key-down");
        assert_eq!(second, b"key-up");

        transport_a.shutdown();
        transport_b.shutdown();
        let _ = fs::remove_dir_all(&dir_a);
        let _ = fs::remove_dir_all(&dir_b);
    }
}
