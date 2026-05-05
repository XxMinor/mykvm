use std::{
    collections::HashMap,
    net::{SocketAddr, ToSocketAddrs},
    sync::{mpsc, Arc},
    thread,
    time::Duration,
};

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use quinn::{
    rustls::{
        pki_types::{CertificateDer, PrivatePkcs8KeyDer},
        RootCertStore,
    },
    ClientConfig, Endpoint, ServerConfig,
};
use tokio::sync::mpsc as tokio_mpsc;

pub const PROTOCOL_VERSION: u16 = 1;

const SERVER_NAME: &str = "mykvm.local";
const MAX_DATAGRAM_BYTES: usize = 16 * 1024;
const MAX_STREAM_BYTES: usize = 512 * 1024;
const PORT_SCAN_COUNT: u16 = 64;

type PacketHandler = Arc<dyn Fn(Vec<u8>, SocketAddr) + Send + Sync + 'static>;

#[derive(Clone, Debug)]
pub struct PeerEndpoint {
    pub addr: String,
    pub public_key: String,
    pub protocol_version: u16,
}

#[derive(Clone)]
pub struct TransportHandle {
    commands: tokio_mpsc::UnboundedSender<TransportCommand>,
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
        if payload.len() > MAX_DATAGRAM_BYTES {
            return Err(format!(
                "QUIC datagram is too large: {} bytes",
                payload.len()
            ));
        }

        self.commands
            .send(TransportCommand::SendDatagram { peer, payload })
            .map_err(|_| "QUIC transport is stopped".to_string())
    }

    pub fn send_stream(&self, peer: PeerEndpoint, payload: Vec<u8>) -> Result<(), String> {
        if payload.len() > MAX_STREAM_BYTES {
            return Err(format!(
                "QUIC stream payload is too large: {} bytes",
                payload.len()
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
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| "QUIC stream send timed out".to_string())?
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
        result: mpsc::Sender<Result<(), String>>,
    },
    Shutdown,
}

#[derive(Hash, PartialEq, Eq)]
struct PeerKey {
    addr: SocketAddr,
    public_key: String,
}

pub fn start(
    preferred_port: u16,
    on_datagram: PacketHandler,
    on_stream: PacketHandler,
) -> Result<TransportHandle, String> {
    let (ready_tx, ready_rx) = mpsc::channel();
    let (command_tx, command_rx) = tokio_mpsc::unbounded_channel();

    thread::Builder::new()
        .name("mykvm-quic-transport".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .thread_name("mykvm-quic")
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
                command_rx,
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
        commands: command_tx,
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
    mut commands: tokio_mpsc::UnboundedReceiver<TransportCommand>,
    on_datagram: PacketHandler,
    on_stream: PacketHandler,
    ready_tx: mpsc::Sender<Result<ReadyTransport, String>>,
) {
    let (endpoint, public_key) = match bind_endpoint(preferred_port) {
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

    let mut connections: HashMap<PeerKey, quinn::Connection> = HashMap::new();
    while let Some(command) = commands.recv().await {
        match command {
            TransportCommand::SendDatagram { peer, payload } => {
                if let Err(error) = send_datagram(&endpoint, &mut connections, peer, payload).await
                {
                    log::warn!("QUIC datagram send failed: {error}");
                }
            }
            TransportCommand::SendStream {
                peer,
                payload,
                result,
            } => {
                let send_result = send_stream(&endpoint, &mut connections, peer, payload).await;
                if let Err(error) = &send_result {
                    log::warn!("QUIC stream send failed: {error}");
                }
                let _ = result.send(send_result);
            }
            TransportCommand::Shutdown => break,
        }
    }

    endpoint.close(0_u32.into(), b"shutdown");
    endpoint.wait_idle().await;
}

fn bind_endpoint(preferred_port: u16) -> Result<(Endpoint, String), String> {
    let mut last_error = None;

    for port in candidate_ports(preferred_port) {
        let (server_config, public_key) = server_config()?;
        let addr = SocketAddr::from(([0, 0, 0, 0], port));
        match Endpoint::server(server_config, addr) {
            Ok(endpoint) => return Ok((endpoint, public_key)),
            Err(error) => last_error = Some(error.to_string()),
        }
    }

    Err(format!(
        "failed to bind QUIC port: {}",
        last_error.unwrap_or_else(|| "no candidate ports available".into())
    ))
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

fn server_config() -> Result<(ServerConfig, String), String> {
    let cert = rcgen::generate_simple_self_signed(vec![SERVER_NAME.into(), "localhost".into()])
        .map_err(|error| format!("failed to generate QUIC certificate: {error}"))?;
    let cert_der = cert.cert.der().clone();
    let public_key = BASE64.encode(cert_der.as_ref());
    let key_der = PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der());
    let mut config = ServerConfig::with_single_cert(vec![cert_der], key_der.into())
        .map_err(|error| format!("failed to build QUIC server config: {error}"))?;
    let mut transport = quinn::TransportConfig::default();
    transport.max_concurrent_bidi_streams(64_u32.into());
    config.transport = Arc::new(transport);

    Ok((config, public_key))
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
    let mut roots = RootCertStore::empty();
    roots
        .add(CertificateDer::from(cert_der))
        .map_err(|error| format!("failed to trust peer transport public key: {error}"))?;

    let mut config = ClientConfig::with_root_certificates(Arc::new(roots))
        .map_err(|error| format!("failed to build QUIC client config: {error}"))?;
    let mut transport = quinn::TransportConfig::default();
    transport.max_concurrent_bidi_streams(64_u32.into());
    config.transport_config(Arc::new(transport));
    Ok(config)
}

fn spawn_accept_loop(endpoint: Endpoint, on_datagram: PacketHandler, on_stream: PacketHandler) {
    tokio::spawn(async move {
        while let Some(incoming) = endpoint.accept().await {
            let remote = incoming.remote_address();
            let on_datagram = Arc::clone(&on_datagram);
            let on_stream = Arc::clone(&on_stream);

            tokio::spawn(async move {
                match incoming.await {
                    Ok(connection) => {
                        spawn_datagram_reader(connection.clone(), remote, on_datagram);
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
    on_datagram: PacketHandler,
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
    on_stream: PacketHandler,
) {
    tokio::spawn(async move {
        loop {
            match connection.accept_bi().await {
                Ok((mut send, mut recv)) => {
                    let on_stream = Arc::clone(&on_stream);
                    tokio::spawn(async move {
                        match recv.read_to_end(MAX_STREAM_BYTES).await {
                            Ok(payload) => {
                                on_stream(payload, remote);
                                let _ = send.write_all(b"ok").await;
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
    let connection = connection_for(endpoint, connections, &peer).await?;
    connection
        .send_datagram(payload.into())
        .map_err(|error| error.to_string())
}

async fn send_stream(
    endpoint: &Endpoint,
    connections: &mut HashMap<PeerKey, quinn::Connection>,
    peer: PeerEndpoint,
    payload: Vec<u8>,
) -> Result<(), String> {
    let connection = connection_for(endpoint, connections, &peer).await?;
    let (mut send, mut recv) = connection
        .open_bi()
        .await
        .map_err(|error| format!("failed to open QUIC stream: {error}"))?;
    send.write_all(&payload)
        .await
        .map_err(|error| format!("failed to write QUIC stream: {error}"))?;
    send.finish()
        .map_err(|error| format!("failed to finish QUIC stream: {error}"))?;
    let _ = tokio::time::timeout(Duration::from_millis(500), recv.read_to_end(64)).await;
    Ok(())
}

async fn connection_for(
    endpoint: &Endpoint,
    connections: &mut HashMap<PeerKey, quinn::Connection>,
    peer: &PeerEndpoint,
) -> Result<quinn::Connection, String> {
    let addr = resolve_peer_addr(&peer.addr)?;
    let key = PeerKey {
        addr,
        public_key: peer.public_key.clone(),
    };

    if let Some(connection) = connections.get(&key) {
        if connection.close_reason().is_none() {
            return Ok(connection.clone());
        }
    }

    let config = client_config(peer)?;
    let connecting = endpoint
        .connect_with(config, addr, SERVER_NAME)
        .map_err(|error| format!("failed to start QUIC connection to {addr}: {error}"))?;
    let connection = tokio::time::timeout(Duration::from_secs(2), connecting)
        .await
        .map_err(|_| format!("QUIC connection to {addr} timed out"))?
        .map_err(|error| format!("failed to connect QUIC to {addr}: {error}"))?;
    connections.insert(key, connection.clone());
    Ok(connection)
}

fn resolve_peer_addr(addr: &str) -> Result<SocketAddr, String> {
    addr.to_socket_addrs()
        .map_err(|error| format!("invalid peer QUIC address {addr}: {error}"))?
        .next()
        .ok_or_else(|| format!("peer QUIC address {addr} did not resolve"))
}
