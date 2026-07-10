use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs,
    net::{SocketAddr, ToSocketAddrs},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
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
// A stream command has one bounded caller-side budget. The ACK gets a smaller
// slice so a slow-but-valid clipboard write can finish while connection setup,
// queueing, and payload upload still have time before the caller gives up.
const STREAM_COMMAND_TIMEOUT: Duration = Duration::from_secs(15);
const STREAM_ACK_TIMEOUT: Duration = Duration::from_secs(2);
const STREAM_TASK_TIMEOUT: Duration = Duration::from_millis(14_500);
const STREAM_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(16);
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(2);
const DATAGRAM_QUEUE_CAPACITY: usize = 64;
const DATAGRAM_MAX_PEERS: usize = 32;
const DATAGRAM_ENQUEUE_TIMEOUT: Duration = Duration::from_millis(100);
const DATAGRAM_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(750);
const RELIABLE_INPUT_QUEUE_CAPACITY: usize = 256;
const RELIABLE_INPUT_MAX_PEERS: usize = 32;
const RELIABLE_INPUT_ENQUEUE_TIMEOUT: Duration = Duration::from_millis(100);
const RELIABLE_INPUT_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(1);
const RELIABLE_INPUT_DELIVERY_ATTEMPTS: usize = 2;
const RELIABLE_INPUT_RETRY_MIN: Duration = Duration::from_millis(25);
const RELIABLE_INPUT_RECOVERY_TIMEOUT: Duration = Duration::from_secs(2);
const RELIABLE_INPUT_PROBE_ATTEMPT_TIMEOUT: Duration = Duration::from_millis(500);
const RELIABLE_INPUT_PROBE_RETRY: Duration = Duration::from_millis(250);
// Shutdown must outlive one blocked write plus the bounded Release/Reset
// recovery window. A shorter budget aborts the worker before its first timeout
// and defeats the drain that is supposed to prevent stuck input.
const RELIABLE_INPUT_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(3_250);
const MAX_INBOUND_CONNECTIONS: usize = 32;
const MAX_INBOUND_BI_STREAMS_GLOBAL: usize = 8;
const MAX_INBOUND_BI_STREAMS_PER_CONNECTION: usize = 8;
const MAX_INBOUND_UNI_STREAMS_PER_CONNECTION: usize = 16;
const INBOUND_STREAM_READ_TIMEOUT: Duration = Duration::from_secs(10);

type DatagramHandler = Arc<dyn Fn(Vec<u8>, SocketAddr) + Send + Sync + 'static>;
type StreamHandler = Arc<dyn Fn(Vec<u8>, SocketAddr) -> bool + Send + Sync + 'static>;

#[derive(Clone, Debug)]
pub struct PeerEndpoint {
    pub addr: String,
    pub public_key: String,
    pub protocol_version: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReliableInputClass {
    State,
    Transient,
    Release,
    ResetBoundary,
}

#[derive(Clone)]
struct ReliableInputFrame {
    payload: Vec<u8>,
    class: ReliableInputClass,
    reset_generation: u64,
}

impl ReliableInputFrame {
    fn is_probe(&self) -> bool {
        self.payload.is_empty()
    }
}

struct DatagramFrame {
    payload: Vec<u8>,
    mode: DatagramMode,
}

#[derive(Clone)]
pub struct TransportHandle {
    datagram_commands: tokio_mpsc::UnboundedSender<DatagramCommand>,
    stream_commands: tokio_mpsc::UnboundedSender<StreamCommand>,
    input_failures: Arc<Mutex<InputFailureState>>,
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

    pub fn peer_input_failed(&self, peer: &PeerEndpoint) -> bool {
        self.input_failures
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .contains(&TransportPeerKey::from(peer))
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
        self.send_reliable_input_with_class(peer, payload, ReliableInputClass::State)
    }

    pub fn send_reliable_input_with_class(
        &self,
        peer: PeerEndpoint,
        payload: Vec<u8>,
        class: ReliableInputClass,
    ) -> Result<(), String> {
        if payload.len() > MAX_DATAGRAM_BYTES {
            return Err(format!(
                "QUIC reliable input is too large: {} bytes",
                payload.len()
            ));
        }

        let (result_tx, result_rx) = mpsc::channel();
        self.datagram_commands
            .send(DatagramCommand::SendReliableInput {
                peer,
                payload,
                class,
                result: result_tx,
            })
            .map_err(|_| "QUIC transport is stopped".to_string())?;
        result_rx
            .recv_timeout(RELIABLE_INPUT_ENQUEUE_TIMEOUT)
            .map_err(|_| "QUIC reliable input enqueue timed out".to_string())?
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

        let peer_key = TransportPeerKey::from(&peer);
        let reliable_failed = self
            .input_failures
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .contains_path(&peer_key, InputFailurePath::Reliable);
        if reliable_failed {
            return Err("QUIC reliable input path is recovering".into());
        }

        let (result_tx, result_rx) = mpsc::channel();
        self.datagram_commands
            .send(DatagramCommand::SendDatagram {
                peer,
                payload,
                mode,
                result: result_tx,
            })
            .map_err(|_| "QUIC transport is stopped".to_string())?;
        result_rx
            .recv_timeout(DATAGRAM_ENQUEUE_TIMEOUT)
            .map_err(|_| "QUIC datagram enqueue timed out".to_string())?
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
            .recv_timeout(STREAM_COMMAND_TIMEOUT)
            .map_err(|_| "QUIC stream send timed out".to_string())?
    }

    pub fn shutdown(&self) {
        let (input_done_tx, input_done_rx) = mpsc::channel();
        let wait_for_input = self
            .datagram_commands
            .send(DatagramCommand::Shutdown {
                result: input_done_tx,
            })
            .is_ok();
        let (stream_done_tx, stream_done_rx) = mpsc::channel();
        let wait_for_streams = self
            .stream_commands
            .send(StreamCommand::Shutdown {
                result: stream_done_tx,
            })
            .is_ok();

        // App exit follows immediately after this call. Wait until the input
        // worker has finished and acknowledged its persistent streams so final
        // CursorPark/KeyUp/ButtonUp frames are not reset with the process.
        if wait_for_input && input_done_rx.recv_timeout(Duration::from_secs(4)).is_err() {
            log::warn!("QUIC reliable-input shutdown drain timed out");
        }
        if wait_for_streams
            && stream_done_rx
                .recv_timeout(STREAM_SHUTDOWN_TIMEOUT)
                .is_err()
        {
            log::warn!("QUIC stream shutdown drain timed out");
        }
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
        result: mpsc::Sender<Result<(), String>>,
    },
    SendReliableInput {
        peer: PeerEndpoint,
        payload: Vec<u8>,
        class: ReliableInputClass,
        result: mpsc::Sender<Result<(), String>>,
    },
    Shutdown {
        result: mpsc::Sender<()>,
    },
}

enum StreamCommand {
    SendStream {
        peer: PeerEndpoint,
        payload: Vec<u8>,
        ack_required: bool,
        result: mpsc::Sender<Result<(), String>>,
    },
    Shutdown {
        result: mpsc::Sender<()>,
    },
}

#[derive(Clone, Hash, PartialEq, Eq)]
struct PeerKey {
    addr: SocketAddr,
    public_key: String,
}

#[derive(Clone, Hash, PartialEq, Eq)]
struct TransportPeerKey {
    addr: String,
    public_key: String,
    protocol_version: u16,
}

impl From<&PeerEndpoint> for TransportPeerKey {
    fn from(peer: &PeerEndpoint) -> Self {
        Self {
            addr: peer.addr.clone(),
            public_key: peer.public_key.clone(),
            protocol_version: peer.protocol_version,
        }
    }
}

type ReliablePeerKey = TransportPeerKey;

#[derive(Clone, Copy)]
enum InputFailurePath {
    Datagram,
    Reliable,
}

#[derive(Default)]
struct InputFailureState {
    // Each delivery path clears only its own failures. Recreating the
    // TransportHandle is the explicit session reset for both sets.
    datagram: HashSet<TransportPeerKey>,
    reliable: HashSet<TransportPeerKey>,
}

impl InputFailureState {
    fn contains(&self, key: &TransportPeerKey) -> bool {
        self.datagram.contains(key) || self.reliable.contains(key)
    }

    fn path_mut(&mut self, path: InputFailurePath) -> &mut HashSet<TransportPeerKey> {
        match path {
            InputFailurePath::Datagram => &mut self.datagram,
            InputFailurePath::Reliable => &mut self.reliable,
        }
    }

    fn contains_path(&self, key: &TransportPeerKey, path: InputFailurePath) -> bool {
        match path {
            InputFailurePath::Datagram => self.datagram.contains(key),
            InputFailurePath::Reliable => self.reliable.contains(key),
        }
    }
}

fn clear_peer_input_failure(
    input_failures: &Arc<Mutex<InputFailureState>>,
    key: &TransportPeerKey,
    path: InputFailurePath,
) {
    input_failures
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .path_mut(path)
        .remove(key);
}

fn mark_peer_input_failed(
    input_failures: &Arc<Mutex<InputFailureState>>,
    key: TransportPeerKey,
    path: InputFailurePath,
) {
    input_failures
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .path_mut(path)
        .insert(key);
}

struct DatagramWorker {
    queue: Arc<DatagramQueue>,
    task: tokio::task::JoinHandle<()>,
}

#[derive(Default)]
struct DatagramQueueState {
    frames: VecDeque<DatagramFrame>,
    closed: bool,
}

#[derive(Default)]
struct DatagramQueue {
    state: Mutex<DatagramQueueState>,
    changed: tokio::sync::Notify,
}

impl DatagramQueue {
    fn is_closed(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .closed
    }

    fn enqueue(
        &self,
        payload: Vec<u8>,
        mode: DatagramMode,
        result: mpsc::Sender<Result<(), String>>,
    ) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if state.closed {
            let _ = result.send(Err("QUIC datagram worker is stopped".into()));
            return;
        }

        let replace_latest = mode == DatagramMode::Latest
            && state
                .frames
                .back()
                .is_some_and(|frame| frame.mode == DatagramMode::Latest);
        let evict_latest = (!replace_latest && state.frames.len() >= DATAGRAM_QUEUE_CAPACITY)
            .then(|| {
                state
                    .frames
                    .iter()
                    .position(|frame| frame.mode == DatagramMode::Latest)
            })
            .flatten();
        if !replace_latest
            && state.frames.len() >= DATAGRAM_QUEUE_CAPACITY
            && evict_latest.is_none()
        {
            let _ = result.send(Err("QUIC datagram queue is full".into()));
            return;
        }

        // Confirm admission while holding the queue lock. If the synchronous
        // caller already timed out, do not leave a ghost movement behind.
        if result.send(Ok(())).is_err() {
            return;
        }

        if replace_latest {
            let latest = state.frames.back_mut().expect("latest frame just checked");
            latest.payload = payload;
        } else {
            if let Some(index) = evict_latest {
                state.frames.remove(index);
            }
            state.frames.push_back(DatagramFrame { payload, mode });
        }
        drop(state);
        self.changed.notify_one();
    }

    async fn recv(&self) -> Option<DatagramFrame> {
        loop {
            let changed = self.changed.notified();
            {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                if let Some(frame) = state.frames.pop_front() {
                    return Some(frame);
                }
                if state.closed {
                    return None;
                }
            }
            changed.await;
        }
    }

    fn close(&self, discard: bool) -> usize {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        state.closed = true;
        let dropped = if discard {
            let dropped = state.frames.len();
            state.frames.clear();
            dropped
        } else {
            0
        };
        drop(state);
        self.changed.notify_waiters();
        dropped
    }
}

struct ReliableInputWorker {
    queue: Arc<ReliableInputQueue>,
    shutdown: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<()>,
}

#[derive(Default)]
struct ReliableInputQueueState {
    frames: VecDeque<ReliableInputFrame>,
    closed: bool,
    recovering: bool,
    reset_generation: u64,
}

#[derive(Default)]
struct ReliableInputQueue {
    state: Mutex<ReliableInputQueueState>,
    changed: tokio::sync::Notify,
}

impl ReliableInputQueue {
    fn is_closed(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .closed
    }

    fn enqueue(
        &self,
        payload: Vec<u8>,
        class: ReliableInputClass,
        result: mpsc::Sender<Result<(), String>>,
    ) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if state.closed {
            let _ = result.send(Err("QUIC reliable input worker is stopped".into()));
            return;
        }
        if state.recovering
            && !matches!(
                class,
                ReliableInputClass::Release | ReliableInputClass::ResetBoundary
            )
        {
            let _ = result.send(Err("QUIC reliable input path is recovering".into()));
            return;
        }

        let evict = if class == ReliableInputClass::ResetBoundary
            || state.frames.len() < RELIABLE_INPUT_QUEUE_CAPACITY
        {
            None
        } else if class == ReliableInputClass::Release {
            state.frames.iter().position(|frame| {
                matches!(
                    frame.class,
                    ReliableInputClass::State | ReliableInputClass::Transient
                )
            })
        } else {
            let _ = result.send(Err("QUIC reliable input queue is full".into()));
            return;
        };
        if class != ReliableInputClass::ResetBoundary
            && state.frames.len() >= RELIABLE_INPUT_QUEUE_CAPACITY
            && evict.is_none()
        {
            let _ = result.send(Err("QUIC reliable input release queue is full".into()));
            return;
        }

        // A timed-out caller drops its receiver. Acknowledge while holding the
        // queue lock, then mutate; a failed acknowledgement cannot queue a ghost.
        if result.send(Ok(())).is_err() {
            return;
        }

        if class == ReliableInputClass::ResetBoundary {
            state.reset_generation = state.reset_generation.wrapping_add(1);
            // CursorPark is authoritative for current receivers, but v2 peers
            // built before that reset behavior only park the pointer. Preserve
            // explicit Up frames so mixed v2 builds still converge safely.
            state
                .frames
                .retain(|frame| frame.class == ReliableInputClass::Release);
        } else if let Some(index) = evict {
            state.frames.remove(index);
        }
        let reset_generation = state.reset_generation;
        state.frames.push_back(ReliableInputFrame {
            payload,
            class,
            reset_generation,
        });
        drop(state);
        self.changed.notify_waiters();
    }

    async fn recv(&self) -> Option<ReliableInputFrame> {
        loop {
            let changed = self.changed.notified();
            {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                if let Some(frame) = state.frames.pop_front() {
                    return Some(frame);
                }
                if state.closed {
                    return None;
                }
            }
            changed.await;
        }
    }

    async fn wait_for_reset_after(&self, generation: u64) {
        loop {
            let changed = self.changed.notified();
            if self
                .state
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .reset_generation
                != generation
            {
                return;
            }
            changed.await;
        }
    }

    fn close(&self, discard: bool) -> usize {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        state.closed = true;
        let dropped = if discard {
            let dropped = state.frames.len();
            state.frames.clear();
            dropped
        } else {
            // Shutdown cancels queued State/Transient immediately so the
            // bounded drain budget is reserved for KeyUp/ButtonUp and the
            // final reset boundary. Bumping the generation also supersedes an
            // in-flight State/Transient through race_attempt_with_reset.
            state.reset_generation = state.reset_generation.wrapping_add(1);
            let before = state.frames.len();
            state.frames.retain(|frame| {
                matches!(
                    frame.class,
                    ReliableInputClass::Release | ReliableInputClass::ResetBoundary
                )
            });
            before.saturating_sub(state.frames.len())
        };
        drop(state);
        self.changed.notify_waiters();
        dropped
    }

    fn retain_recovery_frames(&self, current: &ReliableInputFrame) -> (usize, bool) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let before = state.frames.len() + 1;
        let last_reset = state
            .frames
            .iter()
            .rposition(|frame| frame.class == ReliableInputClass::ResetBoundary);
        let reset = last_reset
            .and_then(|index| state.frames.get(index).cloned())
            .or_else(|| {
                (current.class == ReliableInputClass::ResetBoundary).then(|| current.clone())
            });
        let mut recovery = VecDeque::new();
        if current.class == ReliableInputClass::Release {
            recovery.push_back(current.clone());
        }
        recovery.extend(
            state
                .frames
                .drain(..)
                .filter(|frame| frame.class == ReliableInputClass::Release),
        );
        if let Some(reset) = reset {
            recovery.push_back(reset);
        }
        state.frames = recovery;
        (
            before.saturating_sub(state.frames.len()),
            !state.frames.is_empty(),
        )
    }

    fn is_empty(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .frames
            .is_empty()
    }

    fn begin_probe_recovery(&self) -> usize {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        state.recovering = true;
        let before = state.frames.len();
        state.frames.retain(|frame| {
            matches!(
                frame.class,
                ReliableInputClass::Release | ReliableInputClass::ResetBoundary
            )
        });
        before.saturating_sub(state.frames.len())
    }

    fn pop_probe_recovery_frame(&self) -> Option<ReliableInputFrame> {
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .frames
            .pop_front()
    }

    fn restore_probe_recovery_frame(&self, frame: ReliableInputFrame) {
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .frames
            .push_front(frame);
    }

    fn finish_probe_recovery_if_empty(&self) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if !state.frames.is_empty() {
            return false;
        }
        state.recovering = false;
        drop(state);
        self.changed.notify_waiters();
        true
    }

    fn reset_generation(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .reset_generation
    }
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
    let input_failures = Arc::new(Mutex::new(InputFailureState::default()));
    let input_failures_for_runtime = Arc::clone(&input_failures);

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
                input_failures_for_runtime,
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
        input_failures,
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
    input_failures: Arc<Mutex<InputFailureState>>,
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

    let datagram_task = tokio::spawn(run_datagram_commands(
        endpoint.clone(),
        datagram_commands,
        input_failures,
    ));
    let stream_task = tokio::spawn(run_stream_commands(endpoint.clone(), stream_commands));
    let _ = datagram_task.await;
    let _ = stream_task.await;

    endpoint.close(0_u32.into(), b"shutdown");
    endpoint.wait_idle().await;
}

async fn run_datagram_commands(
    endpoint: Endpoint,
    mut commands: tokio_mpsc::UnboundedReceiver<DatagramCommand>,
    input_failures: Arc<Mutex<InputFailureState>>,
) {
    let mut datagram_workers: HashMap<TransportPeerKey, DatagramWorker> = HashMap::new();
    let mut reliable_workers: HashMap<ReliablePeerKey, ReliableInputWorker> = HashMap::new();
    while let Some(command) = commands.recv().await {
        match command {
            DatagramCommand::SendDatagram {
                peer,
                payload,
                mode,
                result,
            } => {
                enqueue_datagram(
                    &endpoint,
                    &mut datagram_workers,
                    peer,
                    payload,
                    mode,
                    result,
                    &input_failures,
                );
            }
            DatagramCommand::SendReliableInput {
                peer,
                payload,
                class,
                result,
            } => {
                enqueue_reliable_input(
                    &endpoint,
                    &mut reliable_workers,
                    peer,
                    payload,
                    class,
                    result,
                    &input_failures,
                );
            }
            DatagramCommand::Shutdown { result } => {
                shutdown_datagram_workers(&mut datagram_workers).await;
                shutdown_reliable_input_workers(&mut reliable_workers).await;
                let _ = result.send(());
                return;
            }
        }
    }

    shutdown_datagram_workers(&mut datagram_workers).await;
    shutdown_reliable_input_workers(&mut reliable_workers).await;
}

fn enqueue_datagram(
    endpoint: &Endpoint,
    workers: &mut HashMap<TransportPeerKey, DatagramWorker>,
    peer: PeerEndpoint,
    payload: Vec<u8>,
    mode: DatagramMode,
    result: mpsc::Sender<Result<(), String>>,
    input_failures: &Arc<Mutex<InputFailureState>>,
) {
    prune_datagram_workers(workers);
    let worker_key = TransportPeerKey::from(&peer);
    if !workers.contains_key(&worker_key) {
        if workers.len() >= DATAGRAM_MAX_PEERS {
            let _ = result.send(Err("QUIC datagram peer limit reached".into()));
            return;
        }
        workers.insert(
            worker_key.clone(),
            spawn_datagram_worker(
                endpoint.clone(),
                peer,
                Arc::clone(input_failures),
                worker_key.clone(),
            ),
        );
    }
    workers
        .get(&worker_key)
        .expect("datagram worker just inserted")
        .queue
        .enqueue(payload, mode, result);
}

fn prune_datagram_workers(workers: &mut HashMap<TransportPeerKey, DatagramWorker>) {
    let stale = workers
        .iter()
        .filter_map(|(key, worker)| {
            (worker.queue.is_closed() || worker.task.is_finished()).then_some(key.clone())
        })
        .collect::<Vec<_>>();
    for key in stale {
        if let Some(worker) = workers.remove(&key) {
            tokio::spawn(async move {
                if let Err(error) = worker.task.await {
                    log::warn!("QUIC datagram worker failed: {error}");
                }
            });
        }
    }
}

fn spawn_datagram_worker(
    endpoint: Endpoint,
    peer: PeerEndpoint,
    input_failures: Arc<Mutex<InputFailureState>>,
    worker_key: TransportPeerKey,
) -> DatagramWorker {
    let queue = Arc::new(DatagramQueue::default());
    let task = tokio::spawn(run_datagram_worker(
        endpoint,
        peer,
        Arc::clone(&queue),
        input_failures,
        worker_key,
    ));
    DatagramWorker { queue, task }
}

async fn run_datagram_worker(
    endpoint: Endpoint,
    peer: PeerEndpoint,
    queue: Arc<DatagramQueue>,
    input_failures: Arc<Mutex<InputFailureState>>,
    worker_key: TransportPeerKey,
) {
    let mut connections = HashMap::new();
    while let Some(frame) = queue.recv().await {
        match send_datagram(&endpoint, &mut connections, peer.clone(), frame.payload).await {
            Ok(()) => {
                clear_peer_input_failure(&input_failures, &worker_key, InputFailurePath::Datagram)
            }
            Err(error) => {
                mark_peer_input_failed(
                    &input_failures,
                    worker_key.clone(),
                    InputFailurePath::Datagram,
                );
                log::warn!("QUIC datagram send to {} failed: {error}", peer.addr);
            }
        }
    }
}

async fn shutdown_datagram_workers(workers: &mut HashMap<TransportPeerKey, DatagramWorker>) {
    let mut tasks = Vec::with_capacity(workers.len());
    for (_, worker) in workers.drain() {
        worker.queue.close(true);
        tasks.push(worker.task);
    }

    let deadline = tokio::time::Instant::now() + DATAGRAM_SHUTDOWN_TIMEOUT;
    for mut task in tasks {
        if tokio::time::timeout_at(deadline, &mut task).await.is_err() {
            task.abort();
            log::debug!("QUIC datagram worker shutdown timed out");
        }
    }
}

fn enqueue_reliable_input(
    endpoint: &Endpoint,
    workers: &mut HashMap<ReliablePeerKey, ReliableInputWorker>,
    peer: PeerEndpoint,
    payload: Vec<u8>,
    class: ReliableInputClass,
    result: mpsc::Sender<Result<(), String>>,
    input_failures: &Arc<Mutex<InputFailureState>>,
) {
    prune_reliable_input_workers(workers);
    let worker_key = ReliablePeerKey::from(&peer);
    if !workers.contains_key(&worker_key) {
        if workers.len() >= RELIABLE_INPUT_MAX_PEERS {
            let _ = result.send(Err("QUIC reliable input peer limit reached".into()));
            return;
        }
        workers.insert(
            worker_key.clone(),
            spawn_reliable_input_worker(
                endpoint.clone(),
                peer,
                Arc::clone(input_failures),
                worker_key.clone(),
            ),
        );
    }

    let worker = workers
        .get(&worker_key)
        .expect("reliable input worker just inserted");
    worker.queue.enqueue(payload, class, result);
}

fn prune_reliable_input_workers(workers: &mut HashMap<ReliablePeerKey, ReliableInputWorker>) {
    let stale = workers
        .iter()
        .filter_map(|(key, worker)| {
            (worker.queue.is_closed() || worker.task.is_finished()).then_some(key.clone())
        })
        .collect::<Vec<_>>();
    for key in stale {
        if let Some(worker) = workers.remove(&key) {
            tokio::spawn(async move {
                if let Err(error) = worker.task.await {
                    log::warn!("QUIC reliable input worker failed: {error}");
                }
            });
        }
    }
}

fn spawn_reliable_input_worker(
    endpoint: Endpoint,
    peer: PeerEndpoint,
    input_failures: Arc<Mutex<InputFailureState>>,
    worker_key: TransportPeerKey,
) -> ReliableInputWorker {
    let queue = Arc::new(ReliableInputQueue::default());
    let shutdown = Arc::new(AtomicBool::new(false));
    let task = tokio::spawn(run_reliable_input_worker(
        endpoint,
        peer,
        Arc::clone(&queue),
        Arc::clone(&shutdown),
        input_failures,
        worker_key,
    ));
    ReliableInputWorker {
        queue,
        shutdown,
        task,
    }
}

async fn run_reliable_input_worker(
    endpoint: Endpoint,
    peer: PeerEndpoint,
    queue: Arc<ReliableInputQueue>,
    shutdown: Arc<AtomicBool>,
    input_failures: Arc<Mutex<InputFailureState>>,
    worker_key: TransportPeerKey,
) {
    let mut connections = HashMap::new();
    let mut key_streams = HashMap::new();
    let mut recovery_deadline = None;
    while let Some(frame) = queue.recv().await {
        match deliver_reliable_input(
            &endpoint,
            &mut connections,
            &mut key_streams,
            &peer,
            &frame,
            &queue,
            &shutdown,
            recovery_deadline,
        )
        .await
        {
            ReliableDelivery::Delivered => {
                if recovery_deadline.is_some() && queue.is_empty() {
                    recovery_deadline = None;
                    clear_peer_input_failure(
                        &input_failures,
                        &worker_key,
                        InputFailurePath::Reliable,
                    );
                } else if recovery_deadline.is_none() {
                    clear_peer_input_failure(
                        &input_failures,
                        &worker_key,
                        InputFailurePath::Reliable,
                    );
                }
            }
            ReliableDelivery::Superseded => {
                log::debug!(
                    "QUIC reliable input frame for {} superseded by reset boundary",
                    peer.addr
                );
            }
            ReliableDelivery::Failed(error) => {
                mark_peer_input_failed(
                    &input_failures,
                    worker_key.clone(),
                    InputFailurePath::Reliable,
                );
                log::warn!(
                    "QUIC reliable input frame not delivered to {}: {error}",
                    peer.addr
                );
                if recovery_deadline.is_none() {
                    let (dropped, has_recovery) = queue.retain_recovery_frames(&frame);
                    if dropped > 0 {
                        log::warn!(
                            "discarded {dropped} stale reliable input frame(s) for {} after recovery failed",
                            peer.addr
                        );
                    }
                    if has_recovery {
                        recovery_deadline =
                            Some(tokio::time::Instant::now() + RELIABLE_INPUT_RECOVERY_TIMEOUT);
                        continue;
                    }
                }
                // On the second bounded failure the current Release/Reset has
                // already been popped. Put it back before switching to probe
                // mode; concurrent Release/Reset frames are retained too.
                let retained_current_drop = if recovery_deadline.is_some() {
                    queue.retain_recovery_frames(&frame).0
                } else {
                    0
                };
                let dropped = retained_current_drop + queue.begin_probe_recovery();
                if dropped > 0 {
                    log::warn!(
                        "discarded {dropped} reliable recovery frame(s) for {} after bounded recovery expired",
                        peer.addr
                    );
                }
                recovery_deadline = None;
                if recover_reliable_input_path(
                    &endpoint,
                    &mut connections,
                    &mut key_streams,
                    &peer,
                    &queue,
                    &shutdown,
                )
                .await
                {
                    // The recovery helper opens the queue only after every
                    // Release/Reset accepted during the outage is delivered.
                    // Clearing the externally visible bit last makes the next
                    // entry safe on both transport channels.
                    clear_peer_input_failure(
                        &input_failures,
                        &worker_key,
                        InputFailurePath::Reliable,
                    );
                    continue;
                }
                break;
            }
        }
    }
    finish_reliable_input_streams(&mut key_streams).await;
}

async fn recover_reliable_input_path(
    endpoint: &Endpoint,
    connections: &mut HashMap<PeerKey, quinn::Connection>,
    key_streams: &mut HashMap<PeerKey, quinn::SendStream>,
    peer: &PeerEndpoint,
    queue: &ReliableInputQueue,
    shutdown: &AtomicBool,
) -> bool {
    loop {
        tokio::time::sleep(RELIABLE_INPUT_PROBE_RETRY).await;
        let probe = ReliableInputFrame {
            payload: Vec::new(),
            class: ReliableInputClass::Transient,
            reset_generation: queue.reset_generation(),
        };
        match deliver_reliable_input(
            endpoint,
            connections,
            key_streams,
            peer,
            &probe,
            queue,
            shutdown,
            None,
        )
        .await
        {
            ReliableDelivery::Delivered => loop {
                if let Some(frame) = queue.pop_probe_recovery_frame() {
                    match deliver_reliable_input(
                        endpoint,
                        connections,
                        key_streams,
                        peer,
                        &frame,
                        queue,
                        shutdown,
                        None,
                    )
                    .await
                    {
                        ReliableDelivery::Delivered | ReliableDelivery::Superseded => continue,
                        ReliableDelivery::Failed(error) => {
                            log::debug!(
                                "QUIC reliable recovery frame to {} failed: {error}",
                                peer.addr
                            );
                            queue.restore_probe_recovery_frame(frame);
                            break;
                        }
                    }
                } else if queue.finish_probe_recovery_if_empty() {
                    return true;
                }
            },
            ReliableDelivery::Failed(error) => {
                log::debug!(
                    "QUIC reliable input recovery probe to {} failed: {error}",
                    peer.addr
                );
            }
            ReliableDelivery::Superseded => {}
        }
    }
}

async fn deliver_reliable_input(
    endpoint: &Endpoint,
    connections: &mut HashMap<PeerKey, quinn::Connection>,
    key_streams: &mut HashMap<PeerKey, quinn::SendStream>,
    peer: &PeerEndpoint,
    frame: &ReliableInputFrame,
    queue: &ReliableInputQueue,
    shutdown: &AtomicBool,
    recovery_deadline: Option<tokio::time::Instant>,
) -> ReliableDelivery {
    if shutdown.load(Ordering::Relaxed)
        && !frame.is_probe()
        && matches!(
            frame.class,
            ReliableInputClass::State | ReliableInputClass::Transient
        )
    {
        return ReliableDelivery::Superseded;
    }
    let encoded = encode_reliable_input_frame(&frame.payload);
    let delivery_attempts = if frame.is_probe() {
        1
    } else {
        RELIABLE_INPUT_DELIVERY_ATTEMPTS
    };
    for attempt_index in 0..delivery_attempts {
        let max_attempt_timeout = if frame.is_probe() {
            RELIABLE_INPUT_PROBE_ATTEMPT_TIMEOUT
        } else {
            RELIABLE_INPUT_ATTEMPT_TIMEOUT
        };
        let attempt_timeout = recovery_deadline
            .map(|deadline| {
                deadline
                    .saturating_duration_since(tokio::time::Instant::now())
                    .min(max_attempt_timeout)
            })
            .unwrap_or(max_attempt_timeout);
        if attempt_timeout.is_zero() {
            return ReliableDelivery::Failed("reliable input recovery window expired".into());
        }
        let attempt = if matches!(
            frame.class,
            ReliableInputClass::State | ReliableInputClass::Transient
        ) {
            race_attempt_with_reset(
                tokio::time::timeout(
                    attempt_timeout,
                    write_reliable_input_frame(endpoint, connections, key_streams, peer, &encoded),
                ),
                queue.wait_for_reset_after(frame.reset_generation),
            )
            .await
        } else {
            // A later reset may discard obsolete Down/transient state, but it
            // must never cancel an in-flight Up. Deliver releases first so old
            // v2 receivers that do not reset keys on CursorPark cannot latch.
            Some(
                tokio::time::timeout(
                    attempt_timeout,
                    write_reliable_input_frame(endpoint, connections, key_streams, peer, &encoded),
                )
                .await,
            )
        };
        let attempt = match attempt {
            Some(attempt) => {
                attempt.unwrap_or_else(|_| Err("reliable input attempt timed out".into()))
            }
            None => {
                key_streams.clear();
                connections.clear();
                return ReliableDelivery::Superseded;
            }
        };
        match attempt {
            Ok(()) => return ReliableDelivery::Delivered,
            Err(error) => {
                // A failed/partial length-prefixed frame is discarded with its
                // stream. Retry the same complete frame on a fresh connection;
                // the receiver only injects after read_exact gets the full body.
                key_streams.clear();
                connections.clear();
                if attempt_index + 1 == delivery_attempts {
                    return ReliableDelivery::Failed(error);
                }
                log::warn!(
                    "QUIC reliable input retrying current frame for {}: {error}",
                    peer.addr
                );
                tokio::time::sleep(RELIABLE_INPUT_RETRY_MIN).await;
            }
        }
    }
    unreachable!("reliable input delivery loop always returns")
}

async fn race_attempt_with_reset<T>(
    attempt: impl std::future::Future<Output = T>,
    reset: impl std::future::Future<Output = ()>,
) -> Option<T> {
    use std::{future::poll_fn, task::Poll};

    let mut attempt = Box::pin(attempt);
    let mut reset = Box::pin(reset);
    poll_fn(move |context| {
        if let Poll::Ready(result) = attempt.as_mut().poll(context) {
            return Poll::Ready(Some(result));
        }
        if reset.as_mut().poll(context).is_ready() {
            return Poll::Ready(None);
        }
        Poll::Pending
    })
    .await
}

enum ReliableDelivery {
    Delivered,
    Superseded,
    Failed(String),
}

async fn shutdown_reliable_input_workers(
    workers: &mut HashMap<ReliablePeerKey, ReliableInputWorker>,
) {
    let mut tasks = Vec::with_capacity(workers.len());
    for (_, worker) in workers.drain() {
        worker.shutdown.store(true, Ordering::Relaxed);
        worker.queue.close(false);
        tasks.push(worker.task);
    }

    let deadline = tokio::time::Instant::now() + RELIABLE_INPUT_SHUTDOWN_TIMEOUT;
    for mut task in tasks {
        if tokio::time::timeout_at(deadline, &mut task).await.is_err() {
            task.abort();
            log::warn!("QUIC reliable input worker shutdown drain timed out");
        }
    }
}

async fn run_stream_commands(
    endpoint: Endpoint,
    mut commands: tokio_mpsc::UnboundedReceiver<StreamCommand>,
) {
    let connections = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    let mut in_flight = tokio::task::JoinSet::new();
    while let Some(command) = commands.recv().await {
        while let Some(result) = in_flight.try_join_next() {
            if let Err(error) = result {
                log::warn!("QUIC stream task failed: {error}");
            }
        }

        match command {
            StreamCommand::SendStream {
                peer,
                payload,
                ack_required,
                result,
            } => {
                let endpoint = endpoint.clone();
                let connections = Arc::clone(&connections);
                in_flight.spawn(async move {
                    let send_result = tokio::time::timeout(
                        STREAM_TASK_TIMEOUT,
                        send_stream(&endpoint, &connections, peer, payload, ack_required),
                    )
                    .await
                    .unwrap_or_else(|_| Err("QUIC stream task timed out".into()));
                    if let Err(error) = &send_result {
                        log::warn!("QUIC stream send failed: {error}");
                    }
                    let _ = result.send(send_result);
                });
            }
            StreamCommand::Shutdown { result } => {
                commands.close();
                while let Some(task_result) = in_flight.join_next().await {
                    if let Err(error) = task_result {
                        log::warn!("QUIC stream task failed during shutdown: {error}");
                    }
                }
                let _ = result.send(());
                return;
            }
        }
    }

    while let Some(result) = in_flight.join_next().await {
        if let Err(error) = result {
            log::warn!("QUIC stream task failed during channel close: {error}");
        }
    }
}

/// Finish every persistent reliable-input stream and wait briefly for the peer
/// to acknowledge all buffered bytes. Dropping an unfinished Quinn SendStream
/// resets it, which can discard the final release boundary during app exit.
async fn finish_reliable_input_streams(key_streams: &mut HashMap<PeerKey, quinn::SendStream>) {
    let mut finished = Vec::with_capacity(key_streams.len());
    for (_, mut stream) in key_streams.drain() {
        if stream.finish().is_ok() {
            finished.push(stream);
        }
    }

    let deadline = tokio::time::Instant::now() + Duration::from_millis(750);
    for stream in finished {
        match tokio::time::timeout_at(deadline, stream.stopped()).await {
            Ok(Ok(None)) => {}
            Ok(Ok(Some(code))) => {
                log::warn!("peer stopped reliable-input stream during shutdown: {code}")
            }
            Ok(Err(error)) => {
                log::warn!("reliable-input stream shutdown acknowledgement failed: {error}")
            }
            Err(_) => {
                log::warn!("reliable-input stream shutdown acknowledgement timed out");
                break;
            }
        }
    }
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
    transport.max_concurrent_bidi_streams((MAX_INBOUND_BI_STREAMS_PER_CONNECTION as u32).into());
    transport.max_concurrent_uni_streams((MAX_INBOUND_UNI_STREAMS_PER_CONNECTION as u32).into());
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
    let connection_slots = Arc::new(tokio::sync::Semaphore::new(MAX_INBOUND_CONNECTIONS));
    let stream_slots = Arc::new(tokio::sync::Semaphore::new(MAX_INBOUND_BI_STREAMS_GLOBAL));
    tokio::spawn(async move {
        while let Some(incoming) = endpoint.accept().await {
            let Ok(connection_slot) = Arc::clone(&connection_slots).try_acquire_owned() else {
                log::warn!(
                    "refusing QUIC connection from {}: inbound connection limit reached",
                    incoming.remote_address()
                );
                incoming.refuse();
                continue;
            };
            let remote = incoming.remote_address();
            let on_datagram = Arc::clone(&on_datagram);
            let on_stream = Arc::clone(&on_stream);
            let stream_slots = Arc::clone(&stream_slots);

            tokio::spawn(async move {
                match incoming.await {
                    Ok(connection) => {
                        spawn_datagram_reader(connection.clone(), remote, Arc::clone(&on_datagram));
                        spawn_uni_input_reader(connection.clone(), remote, on_datagram);
                        spawn_stream_reader(
                            connection,
                            remote,
                            on_stream,
                            stream_slots,
                            connection_slot,
                        );
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
                            if len == 0 {
                                // Transport-only liveness probe. It deliberately
                                // bypasses input decoding and has no device-side
                                // effect, while keeping this persistent stream open.
                                continue;
                            }
                            if len > MAX_DATAGRAM_BYTES {
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
    global_slots: Arc<tokio::sync::Semaphore>,
    connection_slot: tokio::sync::OwnedSemaphorePermit,
) {
    tokio::spawn(async move {
        let _connection_slot = connection_slot;
        let connection_slots = Arc::new(tokio::sync::Semaphore::new(
            MAX_INBOUND_BI_STREAMS_PER_CONNECTION,
        ));
        loop {
            match connection.accept_bi().await {
                Ok((mut send, mut recv)) => {
                    let Ok(connection_stream_slot) =
                        Arc::clone(&connection_slots).try_acquire_owned()
                    else {
                        let _ = recv.stop(1_u32.into());
                        let _ = send.reset(1_u32.into());
                        continue;
                    };
                    let Ok(global_stream_slot) = Arc::clone(&global_slots).try_acquire_owned()
                    else {
                        let _ = recv.stop(1_u32.into());
                        let _ = send.reset(1_u32.into());
                        continue;
                    };
                    let on_stream = Arc::clone(&on_stream);
                    tokio::spawn(async move {
                        let _slots = (connection_stream_slot, global_stream_slot);
                        match tokio::time::timeout(
                            INBOUND_STREAM_READ_TIMEOUT,
                            recv.read_to_end(MAX_STREAM_BYTES),
                        )
                        .await
                        {
                            Ok(Ok(payload)) => {
                                // Clipboard writes and file-transfer disk I/O
                                // are synchronous and can take hundreds of
                                // milliseconds. Running that work directly on
                                // this runtime's two async workers starves the
                                // datagram and reliable-input readers. Keep the
                                // socket task async and isolate the handler on
                                // Tokio's blocking pool.
                                let accepted =
                                    tokio::task::spawn_blocking(move || on_stream(payload, remote))
                                        .await
                                        .unwrap_or(false);
                                let ack: &[u8] = if accepted { b"ok" } else { b"reject" };
                                let _ = send.write_all(ack).await;
                                let _ = send.finish();
                            }
                            Ok(Err(error)) => {
                                log::warn!("QUIC stream read failed from {remote}: {error}");
                            }
                            Err(_) => {
                                let _ = recv.stop(2_u32.into());
                                let _ = send.reset(2_u32.into());
                                log::warn!("QUIC stream read timed out from {remote}");
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

async fn write_reliable_input_frame(
    endpoint: &Endpoint,
    connections: &mut HashMap<PeerKey, quinn::Connection>,
    key_streams: &mut HashMap<PeerKey, quinn::SendStream>,
    peer: &PeerEndpoint,
    frame: &[u8],
) -> Result<(), String> {
    let key = peer_key(&peer)?;

    // Reuse (or lazily open) the peer's persistent uni stream so KeyDown/KeyUp
    // stay ordered — separate streams would not guarantee delivery order.
    if !key_streams.contains_key(&key) {
        let (_key, connection) = connection_for(endpoint, connections, peer).await?;
        let stream = connection
            .open_uni()
            .await
            .map_err(|error| format!("failed to open reliable input stream: {error}"))?;
        key_streams.insert(key.clone(), stream);
    }
    let stream = key_streams
        .get_mut(&key)
        .expect("reliable input stream just inserted");

    // Bound one slow peer's back-pressure so it cannot freeze every peer's
    // input. A dropped stream is rebuilt and the current complete frame retried.
    match tokio::time::timeout(Duration::from_secs(1), stream.write_all(frame)).await {
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

async fn send_stream(
    endpoint: &Endpoint,
    connections: &tokio::sync::Mutex<HashMap<PeerKey, quinn::Connection>>,
    peer: PeerEndpoint,
    payload: Vec<u8>,
    ack_required: bool,
) -> Result<(), String> {
    let (key, connection) = {
        let mut connections = connections.lock().await;
        connection_for(endpoint, &mut connections, &peer).await?
    };
    let connection_id = connection.stable_id();
    let result = send_stream_on_connection(connection, payload, ack_required).await;
    if result.is_err() {
        let mut connections = connections.lock().await;
        let still_current = connections
            .get(&key)
            .map(|cached| cached.stable_id() == connection_id)
            .unwrap_or(false);
        if still_current {
            connections.remove(&key);
        }
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
    let ack = tokio::time::timeout(STREAM_ACK_TIMEOUT, recv.read_to_end(64)).await;
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
    let connection = tokio::time::timeout(CONNECTION_TIMEOUT, connecting)
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
    use std::{
        sync::{Barrier, Condvar, Mutex},
        time::Instant,
    };

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

    async fn connect_test_client(transport: &TransportHandle) -> (Endpoint, quinn::Connection) {
        let mut endpoint =
            Endpoint::client("127.0.0.1:0".parse().unwrap()).expect("create test QUIC client");
        let target = transport.peer(
            format!("127.0.0.1:{}", transport.port()),
            transport.public_key().to_string(),
            PROTOCOL_VERSION,
        );
        endpoint.set_default_client_config(client_config(&target).expect("client config"));
        let connection = endpoint
            .connect(
                format!("127.0.0.1:{}", transport.port()).parse().unwrap(),
                SERVER_NAME,
            )
            .expect("start test QUIC connection")
            .await
            .expect("establish test QUIC connection");
        (endpoint, connection)
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

    fn recv_datagram_command(
        rx: &mut tokio_mpsc::UnboundedReceiver<DatagramCommand>,
    ) -> DatagramCommand {
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        loop {
            match rx.try_recv() {
                Ok(command) => return command,
                Err(tokio_mpsc::error::TryRecvError::Empty)
                    if std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("datagram command not received: {error:?}"),
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
    fn transport_exposes_and_clears_background_input_failure() {
        let (datagram_tx, _datagram_rx) = tokio_mpsc::unbounded_channel();
        let (stream_tx, _stream_rx) = tokio_mpsc::unbounded_channel();
        let input_failures = Arc::new(Mutex::new(InputFailureState::default()));
        let handle = TransportHandle {
            datagram_commands: datagram_tx,
            stream_commands: stream_tx,
            input_failures: Arc::clone(&input_failures),
            port: 47834,
            public_key: "local-cert".into(),
        };
        let peer = peer("127.0.0.1:47834");
        let key = TransportPeerKey::from(&peer);

        assert!(!handle.peer_input_failed(&peer));
        mark_peer_input_failed(&input_failures, key.clone(), InputFailurePath::Datagram);
        assert!(handle.peer_input_failed(&peer));
        clear_peer_input_failure(&input_failures, &key, InputFailurePath::Datagram);
        assert!(!handle.peer_input_failed(&peer));
    }

    #[test]
    fn datagram_success_does_not_clear_reliable_input_failure() {
        let (datagram_tx, _datagram_rx) = tokio_mpsc::unbounded_channel();
        let (stream_tx, _stream_rx) = tokio_mpsc::unbounded_channel();
        let input_failures = Arc::new(Mutex::new(InputFailureState::default()));
        let handle = TransportHandle {
            datagram_commands: datagram_tx,
            stream_commands: stream_tx,
            input_failures: Arc::clone(&input_failures),
            port: 47834,
            public_key: "local-cert".into(),
        };
        let peer = peer("127.0.0.1:47834");
        let key = TransportPeerKey::from(&peer);

        // A reliable write fails, then an unrelated mouse-move datagram lands.
        mark_peer_input_failed(&input_failures, key.clone(), InputFailurePath::Reliable);
        clear_peer_input_failure(&input_failures, &key, InputFailurePath::Datagram);

        assert!(handle.peer_input_failed(&peer));
        clear_peer_input_failure(&input_failures, &key, InputFailurePath::Reliable);
        assert!(!handle.peer_input_failed(&peer));
    }

    #[test]
    fn reliable_success_does_not_clear_datagram_input_failure() {
        let input_failures = Arc::new(Mutex::new(InputFailureState::default()));
        let peer = peer("127.0.0.1:47834");
        let key = TransportPeerKey::from(&peer);

        mark_peer_input_failed(&input_failures, key.clone(), InputFailurePath::Datagram);
        clear_peer_input_failure(&input_failures, &key, InputFailurePath::Reliable);

        assert!(input_failures.lock().unwrap().contains(&key));
        clear_peer_input_failure(&input_failures, &key, InputFailurePath::Datagram);
        assert!(!input_failures.lock().unwrap().contains(&key));
    }

    #[test]
    fn failed_reliable_path_rejects_entry_without_blocking_the_input_hook() {
        let (datagram_tx, mut datagram_rx) = tokio_mpsc::unbounded_channel();
        let (stream_tx, _stream_rx) = tokio_mpsc::unbounded_channel();
        let input_failures = Arc::new(Mutex::new(InputFailureState::default()));
        let handle = TransportHandle {
            datagram_commands: datagram_tx,
            stream_commands: stream_tx,
            input_failures: Arc::clone(&input_failures),
            port: 47834,
            public_key: "local-cert".into(),
        };
        let target = peer("127.0.0.1:47834");
        mark_peer_input_failed(
            &input_failures,
            TransportPeerKey::from(&target),
            InputFailurePath::Reliable,
        );

        let started = Instant::now();
        let result = handle.send_latest_datagram(target, b"enter".to_vec());
        let elapsed = started.elapsed();

        assert!(result
            .as_ref()
            .is_err_and(|error| error.contains("reliable input path is recovering")));
        assert!(
            elapsed < Duration::from_millis(50),
            "the mouse hook must never wait for a network probe: {elapsed:?}"
        );
        assert!(
            datagram_rx.try_recv().is_err(),
            "entry motion must wait until background reliable recovery succeeds"
        );
    }

    #[test]
    fn latest_datagram_drops_stale_moves_until_ordered_boundary() {
        let queue = DatagramQueue::default();
        for (payload, mode) in [
            (b"oldest".as_slice(), DatagramMode::Latest),
            (b"middle".as_slice(), DatagramMode::Latest),
            (b"latest".as_slice(), DatagramMode::Latest),
            (b"button".as_slice(), DatagramMode::Ordered),
        ] {
            let (result_tx, result_rx) = mpsc::channel();
            queue.enqueue(payload.to_vec(), mode, result_tx);
            assert!(result_rx.recv().unwrap().is_ok());
        }

        let state = queue.state.lock().unwrap();
        assert_eq!(state.frames.len(), 2);
        assert_eq!(state.frames[0].payload, b"latest");
        assert_eq!(state.frames[1].payload, b"button");
    }

    #[test]
    fn latest_datagram_does_not_cross_ordered_boundary() {
        let queue = DatagramQueue::default();
        for (payload, mode) in [
            (b"before".as_slice(), DatagramMode::Latest),
            (b"boundary".as_slice(), DatagramMode::Ordered),
            (b"after".as_slice(), DatagramMode::Latest),
        ] {
            let (result_tx, result_rx) = mpsc::channel();
            queue.enqueue(payload.to_vec(), mode, result_tx);
            assert!(result_rx.recv().unwrap().is_ok());
        }

        let state = queue.state.lock().unwrap();
        assert_eq!(state.frames.len(), 3);
        assert_eq!(state.frames[0].payload, b"before");
        assert_eq!(state.frames[1].payload, b"boundary");
        assert_eq!(state.frames[2].payload, b"after");
    }

    #[test]
    fn failed_in_flight_release_is_retained_for_bounded_recovery() {
        let queue = ReliableInputQueue::default();
        queue
            .state
            .lock()
            .unwrap()
            .frames
            .push_back(ReliableInputFrame {
                payload: b"stale-state".to_vec(),
                class: ReliableInputClass::State,
                reset_generation: 0,
            });
        let current = ReliableInputFrame {
            payload: b"key-up".to_vec(),
            class: ReliableInputClass::Release,
            reset_generation: 0,
        };

        let (dropped, has_recovery) = queue.retain_recovery_frames(&current);
        assert_eq!(dropped, 1);
        assert!(has_recovery);
        assert_eq!(queue.begin_probe_recovery(), 0);
        let state = queue.state.lock().unwrap();
        assert_eq!(state.frames.len(), 1);
        assert_eq!(state.frames[0].payload, b"key-up");
        assert_eq!(state.frames[0].class, ReliableInputClass::Release);
    }

    #[test]
    fn failed_in_flight_reset_supersedes_queued_state_during_recovery() {
        let queue = ReliableInputQueue::default();
        queue
            .state
            .lock()
            .unwrap()
            .frames
            .push_back(ReliableInputFrame {
                payload: b"newer-state".to_vec(),
                class: ReliableInputClass::State,
                reset_generation: 1,
            });
        let current = ReliableInputFrame {
            payload: b"cursor-park".to_vec(),
            class: ReliableInputClass::ResetBoundary,
            reset_generation: 1,
        };

        let (dropped, has_recovery) = queue.retain_recovery_frames(&current);
        assert_eq!(dropped, 1);
        assert!(has_recovery);
        assert_eq!(queue.begin_probe_recovery(), 0);
        let state = queue.state.lock().unwrap();
        assert_eq!(state.frames.len(), 1);
        assert_eq!(state.frames[0].payload, b"cursor-park");
        assert_eq!(state.frames[0].class, ReliableInputClass::ResetBoundary);
    }

    #[test]
    fn reset_boundary_preserves_release_frames_for_legacy_receivers() {
        let queue = ReliableInputQueue::default();
        for (payload, class) in [
            (b"key-down".as_slice(), ReliableInputClass::State),
            (b"key-up".as_slice(), ReliableInputClass::Release),
            (b"scroll".as_slice(), ReliableInputClass::Transient),
            (b"cursor-park".as_slice(), ReliableInputClass::ResetBoundary),
        ] {
            let (result_tx, result_rx) = mpsc::channel();
            queue.enqueue(payload.to_vec(), class, result_tx);
            assert!(result_rx.recv().unwrap().is_ok());
        }

        let state = queue.state.lock().unwrap();
        let frames = state
            .frames
            .iter()
            .map(|frame| (frame.payload.as_slice(), frame.class))
            .collect::<Vec<_>>();
        assert_eq!(
            frames,
            vec![
                (b"key-up".as_slice(), ReliableInputClass::Release),
                (b"cursor-park".as_slice(), ReliableInputClass::ResetBoundary),
            ]
        );
    }

    #[test]
    fn reliable_queue_reopens_only_after_background_probe_recovery() {
        let queue = ReliableInputQueue::default();
        for (payload, class) in [
            (b"stale-key-down".as_slice(), ReliableInputClass::State),
            (b"queued-key-up".as_slice(), ReliableInputClass::Release),
        ] {
            let (result_tx, result_rx) = mpsc::channel();
            queue.enqueue(payload.to_vec(), class, result_tx);
            assert!(result_rx.recv().unwrap().is_ok());
        }
        assert_eq!(queue.begin_probe_recovery(), 1);

        let (blocked_tx, blocked_rx) = mpsc::channel();
        queue.enqueue(b"key-down".to_vec(), ReliableInputClass::State, blocked_tx);
        assert!(blocked_rx
            .recv()
            .unwrap()
            .is_err_and(|error| error.contains("path is recovering")));

        let (release_tx, release_rx) = mpsc::channel();
        queue.enqueue(b"key-up".to_vec(), ReliableInputClass::Release, release_tx);
        assert!(release_rx.recv().unwrap().is_ok());
        assert!(!queue.finish_probe_recovery_if_empty());
        assert_eq!(
            queue
                .pop_probe_recovery_frame()
                .expect("queued release")
                .payload,
            b"queued-key-up"
        );
        assert_eq!(
            queue
                .pop_probe_recovery_frame()
                .expect("release accepted during recovery")
                .payload,
            b"key-up"
        );
        assert!(queue.finish_probe_recovery_if_empty());

        let (ready_tx, ready_rx) = mpsc::channel();
        queue.enqueue(b"key-down".to_vec(), ReliableInputClass::State, ready_tx);
        assert!(ready_rx.recv().unwrap().is_ok());
    }

    #[test]
    fn shutdown_drops_state_but_keeps_release_and_reset() {
        let queue = ReliableInputQueue::default();
        for (payload, class) in [
            (b"cursor-park".as_slice(), ReliableInputClass::ResetBoundary),
            (b"stale-key-down".as_slice(), ReliableInputClass::State),
            (b"key-up".as_slice(), ReliableInputClass::Release),
        ] {
            let (result_tx, result_rx) = mpsc::channel();
            queue.enqueue(payload.to_vec(), class, result_tx);
            assert!(result_rx.recv().unwrap().is_ok());
        }

        queue.close(false);

        let state = queue.state.lock().unwrap();
        let frames = state
            .frames
            .iter()
            .map(|frame| frame.payload.as_slice())
            .collect::<Vec<_>>();
        assert_eq!(frames, vec![b"cursor-park".as_slice(), b"key-up"]);
    }

    #[test]
    fn recovery_keeps_releases_before_the_last_reset_boundary() {
        let queue = ReliableInputQueue::default();
        {
            let mut state = queue.state.lock().unwrap();
            state.frames.extend([
                ReliableInputFrame {
                    payload: b"queued-up".to_vec(),
                    class: ReliableInputClass::Release,
                    reset_generation: 1,
                },
                ReliableInputFrame {
                    payload: b"stale-state".to_vec(),
                    class: ReliableInputClass::State,
                    reset_generation: 1,
                },
                ReliableInputFrame {
                    payload: b"cursor-park".to_vec(),
                    class: ReliableInputClass::ResetBoundary,
                    reset_generation: 2,
                },
            ]);
        }
        let current = ReliableInputFrame {
            payload: b"in-flight-up".to_vec(),
            class: ReliableInputClass::Release,
            reset_generation: 1,
        };

        let (_, has_recovery) = queue.retain_recovery_frames(&current);
        assert!(has_recovery);
        let state = queue.state.lock().unwrap();
        let frames = state
            .frames
            .iter()
            .map(|frame| frame.payload.as_slice())
            .collect::<Vec<_>>();
        assert_eq!(
            frames,
            vec![
                b"in-flight-up".as_slice(),
                b"queued-up".as_slice(),
                b"cursor-park".as_slice(),
            ]
        );
    }

    #[test]
    fn transport_handle_routes_datagrams_and_streams_to_separate_queues() {
        let (datagram_tx, mut datagram_rx) = tokio_mpsc::unbounded_channel();
        let (stream_tx, mut stream_rx) = tokio_mpsc::unbounded_channel();
        let handle = TransportHandle {
            datagram_commands: datagram_tx,
            stream_commands: stream_tx,
            input_failures: Arc::new(Mutex::new(InputFailureState::default())),
            port: 47834,
            public_key: "local-cert".into(),
        };
        let target = peer("127.0.0.1:47834");

        let datagram_handle = handle.clone();
        let datagram_peer = target.clone();
        let datagram_join = std::thread::spawn(move || {
            datagram_handle.send_datagram(datagram_peer, b"move".to_vec())
        });
        match recv_datagram_command(&mut datagram_rx) {
            DatagramCommand::SendDatagram {
                payload,
                mode,
                result,
                ..
            } => {
                assert_eq!(payload, b"move");
                assert_eq!(mode, DatagramMode::Ordered);
                result.send(Ok(())).expect("admit datagram");
            }
            DatagramCommand::SendReliableInput { .. } => panic!("unexpected reliable input"),
            DatagramCommand::Shutdown { .. } => panic!("unexpected shutdown"),
        }
        assert!(datagram_join.join().expect("datagram sender").is_ok());
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
            StreamCommand::Shutdown { .. } => panic!("unexpected shutdown"),
        };

        assert!(result.is_ok());
        assert!(datagram_rx.try_recv().is_err());

        // A reliable input event lands on the datagram/input queue as a distinct
        // command — never on the block-transfer stream queue. Its synchronous
        // result confirms admission to the bounded per-peer queue.
        let reliable_handle = handle.clone();
        let reliable_peer = target.clone();
        let reliable_join = std::thread::spawn(move || {
            reliable_handle.send_reliable_input(reliable_peer, b"key".to_vec())
        });
        match recv_datagram_command(&mut datagram_rx) {
            DatagramCommand::SendReliableInput {
                payload, result, ..
            } => {
                assert_eq!(payload, b"key");
                result.send(Ok(())).expect("admit reliable input");
            }
            _ => panic!("expected a reliable input command"),
        }
        assert!(reliable_join.join().expect("reliable sender").is_ok());
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
    fn stream_timeouts_cover_slow_lan_upload_handler_and_ack() {
        assert!(
            CONNECTION_TIMEOUT + INBOUND_STREAM_READ_TIMEOUT + STREAM_ACK_TIMEOUT
                < STREAM_TASK_TIMEOUT,
            "one stream task must cover connect + slow upload + ACK"
        );
        assert!(STREAM_TASK_TIMEOUT < STREAM_COMMAND_TIMEOUT);
        assert!(STREAM_COMMAND_TIMEOUT < STREAM_SHUTDOWN_TIMEOUT);
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
    fn one_connection_cannot_run_more_than_eight_blocking_streams() {
        let suffix = format!("{}-{}", std::process::id(), crate::now_ms());
        let dir = std::env::temp_dir().join(format!("mykvm-bi-cap-{suffix}"));
        let _ = fs::remove_dir_all(&dir);

        let entered = Arc::new((Mutex::new(0_usize), Condvar::new()));
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let entered_for_handler = Arc::clone(&entered);
        let release_for_handler = Arc::clone(&release);
        let blocking_stream: StreamHandler = Arc::new(move |_, _| {
            let (entered_lock, entered_cv) = &*entered_for_handler;
            *entered_lock.lock().expect("entered lock") += 1;
            entered_cv.notify_all();

            let (release_lock, release_cv) = &*release_for_handler;
            let released = release_lock.lock().expect("release lock");
            let _released = release_cv
                .wait_while(released, |released| !*released)
                .expect("release wait");
            true
        });
        let noop_datagram: DatagramHandler = Arc::new(|_, _| {});
        let transport = start(54200, dir.clone(), noop_datagram, blocking_stream)
            .expect("start capped receiver");

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let entered_count = runtime.block_on(async {
            let (endpoint, connection) = connect_test_client(&transport).await;
            let mut responses = Vec::new();
            for index in 0..9_u8 {
                let opened =
                    tokio::time::timeout(Duration::from_millis(500), connection.open_bi()).await;
                let Ok(Ok((mut send, recv))) = opened else {
                    break;
                };
                send.write_all(&[index]).await.expect("write stream body");
                send.finish().expect("finish stream body");
                responses.push(recv);
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
            let count = *entered.0.lock().expect("entered count");

            *release.0.lock().expect("release lock") = true;
            release.1.notify_all();
            for mut response in responses {
                let _ =
                    tokio::time::timeout(Duration::from_secs(1), response.read_to_end(64)).await;
            }
            connection.close(0_u32.into(), b"done");
            endpoint.close(0_u32.into(), b"done");
            count
        });

        transport.shutdown();
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(
            entered_count, 8,
            "one connection admitted {entered_count} blocking streams"
        );
    }

    #[test]
    fn global_stream_limit_is_shared_across_connections() {
        let suffix = format!("{}-{}", std::process::id(), crate::now_ms());
        let dir = std::env::temp_dir().join(format!("mykvm-global-bi-cap-{suffix}"));
        let _ = fs::remove_dir_all(&dir);

        let entered = Arc::new((Mutex::new(0_usize), Condvar::new()));
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let entered_for_handler = Arc::clone(&entered);
        let release_for_handler = Arc::clone(&release);
        let blocking_stream: StreamHandler = Arc::new(move |_, _| {
            *entered_for_handler.0.lock().expect("entered lock") += 1;
            entered_for_handler.1.notify_all();
            let released = release_for_handler.0.lock().expect("release lock");
            let _released = release_for_handler
                .1
                .wait_while(released, |released| !*released)
                .expect("release wait");
            true
        });
        let noop_datagram: DatagramHandler = Arc::new(|_, _| {});
        let transport = start(54400, dir.clone(), noop_datagram, blocking_stream)
            .expect("start globally capped receiver");

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let entered_count = runtime.block_on(async {
            let (endpoint_a, connection_a) = connect_test_client(&transport).await;
            let (endpoint_b, connection_b) = connect_test_client(&transport).await;
            let (endpoint_c, connection_c) = connect_test_client(&transport).await;
            let mut responses = Vec::new();
            for connection in [&connection_a, &connection_b] {
                for index in 0..4_u8 {
                    let (mut send, recv) = connection.open_bi().await.expect("open capped stream");
                    send.write_all(&[index]).await.expect("write capped stream");
                    send.finish().expect("finish capped stream");
                    responses.push(recv);
                }
            }
            let (mut overflow_send, overflow_recv) =
                connection_c.open_bi().await.expect("open overflow stream");
            overflow_send
                .write_all(b"overflow")
                .await
                .expect("write overflow stream");
            overflow_send.finish().expect("finish overflow stream");
            responses.push(overflow_recv);

            tokio::time::sleep(Duration::from_millis(250)).await;
            let count = *entered.0.lock().expect("entered count");
            *release.0.lock().expect("release lock") = true;
            release.1.notify_all();
            for mut response in responses {
                let _ =
                    tokio::time::timeout(Duration::from_secs(1), response.read_to_end(64)).await;
            }
            for connection in [&connection_a, &connection_b, &connection_c] {
                connection.close(0_u32.into(), b"done");
            }
            for endpoint in [endpoint_a, endpoint_b, endpoint_c] {
                endpoint.close(0_u32.into(), b"done");
            }
            count
        });

        transport.shutdown();
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(entered_count, MAX_INBOUND_BI_STREAMS_GLOBAL);
    }

    #[test]
    fn inbound_connection_count_is_bounded() {
        let suffix = format!("{}-{}", std::process::id(), crate::now_ms());
        let dir = std::env::temp_dir().join(format!("mykvm-connection-cap-{suffix}"));
        let _ = fs::remove_dir_all(&dir);
        let noop_datagram: DatagramHandler = Arc::new(|_, _| {});
        let noop_stream: StreamHandler = Arc::new(|_, _| false);
        let transport = start(54600, dir.clone(), noop_datagram, noop_stream)
            .expect("start connection-capped receiver");

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let accepted = runtime.block_on(async {
            let mut endpoint = Endpoint::client("127.0.0.1:0".parse().unwrap())
                .expect("create connection-cap client");
            let target = transport.peer(
                format!("127.0.0.1:{}", transport.port()),
                transport.public_key().to_string(),
                PROTOCOL_VERSION,
            );
            endpoint.set_default_client_config(client_config(&target).expect("client config"));
            let addr = format!("127.0.0.1:{}", transport.port()).parse().unwrap();
            let mut connections = Vec::new();
            for _ in 0..=MAX_INBOUND_CONNECTIONS {
                let connection = endpoint
                    .connect(addr, SERVER_NAME)
                    .expect("start connection");
                match tokio::time::timeout(Duration::from_secs(1), connection).await {
                    Ok(Ok(connection)) => connections.push(connection),
                    _ => break,
                }
            }
            let count = connections.len();
            for connection in connections {
                connection.close(0_u32.into(), b"done");
            }
            endpoint.close(0_u32.into(), b"done");
            count
        });

        transport.shutdown();
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(accepted, MAX_INBOUND_CONNECTIONS);
    }

    #[test]
    fn incomplete_stream_body_is_reset_after_read_timeout() {
        let suffix = format!("{}-{}", std::process::id(), crate::now_ms());
        let dir = std::env::temp_dir().join(format!("mykvm-stream-read-timeout-{suffix}"));
        let _ = fs::remove_dir_all(&dir);
        let handler_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls_for_handler = Arc::clone(&handler_calls);
        let stream: StreamHandler = Arc::new(move |_, _| {
            calls_for_handler.fetch_add(1, Ordering::Relaxed);
            true
        });
        let datagram: DatagramHandler = Arc::new(|_, _| {});
        let transport =
            start(54800, dir.clone(), datagram, stream).expect("start timeout receiver");

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let reset = runtime.block_on(async {
            let (endpoint, connection) = connect_test_client(&transport).await;
            let (mut send, mut recv) = connection.open_bi().await.expect("open incomplete stream");
            send.write_all(b"partial")
                .await
                .expect("write partial body");
            // Intentionally keep the request side open: read_to_end must not
            // hold a global stream permit forever waiting for a malicious peer.
            let response = tokio::time::timeout(
                INBOUND_STREAM_READ_TIMEOUT + Duration::from_secs(2),
                recv.read_to_end(64),
            )
            .await;
            drop(send);
            connection.close(0_u32.into(), b"done");
            endpoint.close(0_u32.into(), b"done");
            matches!(response, Ok(Err(_)))
        });

        transport.shutdown();
        let _ = fs::remove_dir_all(&dir);
        assert!(reset, "incomplete stream was not reset by the receiver");
        assert_eq!(handler_calls.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn dead_reliable_peer_does_not_block_other_peer_or_datagrams() {
        let suffix = format!("{}-{}", std::process::id(), crate::now_ms());
        let dir_a = std::env::temp_dir().join(format!("mykvm-peer-isolation-a-{suffix}"));
        let dir_b = std::env::temp_dir().join(format!("mykvm-peer-isolation-b-{suffix}"));
        let _ = fs::remove_dir_all(&dir_a);
        let _ = fs::remove_dir_all(&dir_b);

        let noop_datagram: DatagramHandler = Arc::new(|_, _| {});
        let noop_stream: StreamHandler = Arc::new(|_, _| false);
        let transport_a = start(53200, dir_a.clone(), noop_datagram, noop_stream)
            .expect("start isolation sender");

        let (input_tx, input_rx) = mpsc::channel();
        let received_input: DatagramHandler = Arc::new(move |payload, _| {
            let _ = input_tx.send(payload);
        });
        let noop_stream_b: StreamHandler = Arc::new(|_, _| false);
        let transport_b = start(53400, dir_b.clone(), received_input, noop_stream_b)
            .expect("start isolation receiver");
        let live_peer = transport_a.peer(
            format!("127.0.0.1:{}", transport_b.port()),
            transport_b.public_key().to_string(),
            PROTOCOL_VERSION,
        );

        // Warm both paths so the timed section measures peer isolation rather
        // than the healthy peer's first TLS handshake.
        transport_a
            .send_datagram(live_peer.clone(), b"warm-datagram".to_vec())
            .expect("warm datagram");
        transport_a
            .send_reliable_input(live_peer.clone(), b"warm-reliable".to_vec())
            .expect("warm reliable input");
        for _ in 0..2 {
            input_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("warm healthy peer");
        }

        // A bound socket that never speaks QUIC behaves like a reachable host
        // whose input service is wedged: the handshake consumes its full timeout.
        let blackhole = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind blackhole");
        let dead_peer = transport_a.peer(
            blackhole.local_addr().expect("blackhole addr").to_string(),
            transport_b.public_key().to_string(),
            PROTOCOL_VERSION,
        );
        transport_a
            .send_reliable_input(dead_peer, b"blocked-peer".to_vec())
            .expect("queue blocked peer input");
        std::thread::sleep(Duration::from_millis(50));

        transport_a
            .send_datagram(live_peer.clone(), b"live-datagram".to_vec())
            .expect("send live datagram");
        transport_a
            .send_reliable_input(live_peer, b"live-reliable".to_vec())
            .expect("send live reliable input");

        let deadline = Instant::now() + Duration::from_millis(750);
        let mut live = Vec::new();
        while live.len() < 2 {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            match input_rx.recv_timeout(deadline - now) {
                Ok(payload) => live.push(payload),
                Err(_) => break,
            }
        }
        let isolated = live.iter().any(|payload| payload == b"live-datagram")
            && live.iter().any(|payload| payload == b"live-reliable");

        drop(blackhole);
        transport_a.shutdown();
        transport_b.shutdown();
        let _ = fs::remove_dir_all(&dir_a);
        let _ = fs::remove_dir_all(&dir_b);

        assert!(
            isolated,
            "a dead peer delayed healthy datagram/reliable input: {live:?}"
        );
    }

    #[test]
    fn dead_datagram_peer_does_not_block_other_peer_or_release_admission() {
        let suffix = format!("{}-{}", std::process::id(), crate::now_ms());
        let dir_a = std::env::temp_dir().join(format!("mykvm-datagram-isolation-a-{suffix}"));
        let dir_b = std::env::temp_dir().join(format!("mykvm-datagram-isolation-b-{suffix}"));
        let _ = fs::remove_dir_all(&dir_a);
        let _ = fs::remove_dir_all(&dir_b);
        let noop_datagram: DatagramHandler = Arc::new(|_, _| {});
        let noop_stream: StreamHandler = Arc::new(|_, _| false);
        let transport_a = start(55000, dir_a.clone(), noop_datagram, noop_stream)
            .expect("start datagram isolation sender");
        let (input_tx, input_rx) = mpsc::channel();
        let received_input: DatagramHandler = Arc::new(move |payload, _| {
            let _ = input_tx.send(payload);
        });
        let noop_stream_b: StreamHandler = Arc::new(|_, _| false);
        let transport_b = start(55200, dir_b.clone(), received_input, noop_stream_b)
            .expect("start datagram isolation receiver");
        let live_peer = transport_a.peer(
            format!("127.0.0.1:{}", transport_b.port()),
            transport_b.public_key().to_string(),
            PROTOCOL_VERSION,
        );

        transport_a
            .send_datagram(live_peer.clone(), b"warm-datagram".to_vec())
            .expect("warm datagram");
        input_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("warm healthy datagram path");

        let blackhole = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind blackhole");
        let dead_peer = transport_a.peer(
            blackhole.local_addr().expect("blackhole addr").to_string(),
            transport_b.public_key().to_string(),
            PROTOCOL_VERSION,
        );
        transport_a
            .send_datagram(dead_peer, b"blocked-datagram".to_vec())
            .expect("queue blocked datagram");
        std::thread::sleep(Duration::from_millis(50));

        transport_a
            .send_latest_datagram(live_peer.clone(), b"live-datagram".to_vec())
            .expect("send live datagram");
        let release_admitted = transport_a.send_reliable_input_with_class(
            live_peer,
            b"live-release".to_vec(),
            ReliableInputClass::Release,
        );
        let deadline = Instant::now() + Duration::from_millis(750);
        let mut live = Vec::new();
        while live.len() < 2 {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            match input_rx.recv_timeout(deadline - now) {
                Ok(payload) => live.push(payload),
                Err(_) => break,
            }
        }
        let isolated = release_admitted.is_ok()
            && live.iter().any(|payload| payload == b"live-datagram")
            && live.iter().any(|payload| payload == b"live-release");

        drop(blackhole);
        transport_a.shutdown();
        transport_b.shutdown();
        let _ = fs::remove_dir_all(&dir_a);
        let _ = fs::remove_dir_all(&dir_b);
        assert!(
            isolated,
            "dead datagram peer blocked release/datagram: release={release_admitted:?} live={live:?}"
        );
    }

    #[test]
    fn failed_reliable_frame_does_not_replay_stale_backlog_after_recovery() {
        let suffix = format!("{}-{}", std::process::id(), crate::now_ms());
        let dir_a = std::env::temp_dir().join(format!("mykvm-recovery-a-{suffix}"));
        let dir_b = std::env::temp_dir().join(format!("mykvm-recovery-b-{suffix}"));
        let _ = fs::remove_dir_all(&dir_a);
        let _ = fs::remove_dir_all(&dir_b);

        let noop_datagram: DatagramHandler = Arc::new(|_, _| {});
        let noop_stream: StreamHandler = Arc::new(|_, _| false);
        let transport_a =
            start(53600, dir_a.clone(), noop_datagram, noop_stream).expect("start recovery sender");

        // Pre-create B's identity, but leave only a silent UDP socket on its
        // future port so A exhausts the bounded delivery budget first.
        let identity_b = load_or_create_identity(&dir_b).expect("create receiver identity");
        let blackhole = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind blackhole");
        let receiver_port = blackhole.local_addr().expect("blackhole addr").port();
        let peer_b = transport_a.peer(
            format!("127.0.0.1:{receiver_port}"),
            identity_b.public_key,
            PROTOCOL_VERSION,
        );
        transport_a
            .send_reliable_input(peer_b.clone(), b"stale-key-down".to_vec())
            .expect("queue stale frame");
        std::thread::sleep(Duration::from_millis(2_600));

        drop(blackhole);
        let (input_tx, input_rx) = mpsc::channel();
        let received_input: DatagramHandler = Arc::new(move |payload, _| {
            let _ = input_tx.send(payload);
        });
        let noop_stream_b: StreamHandler = Arc::new(|_, _| false);
        let transport_b = start(receiver_port, dir_b.clone(), received_input, noop_stream_b)
            .expect("start recovered receiver");
        assert_eq!(transport_b.port(), receiver_port);

        transport_a
            .send_reliable_input_with_class(
                peer_b,
                b"fresh-release".to_vec(),
                ReliableInputClass::Release,
            )
            .expect("queue fresh release");
        let first = input_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("receive post-recovery frame");

        transport_a.shutdown();
        transport_b.shutdown();
        let _ = fs::remove_dir_all(&dir_a);
        let _ = fs::remove_dir_all(&dir_b);

        assert_eq!(
            first, b"fresh-release",
            "a failed peer replayed stale state after it recovered"
        );
    }

    #[test]
    fn reset_boundary_supersedes_in_flight_and_queued_stale_state() {
        let suffix = format!("{}-{}", std::process::id(), crate::now_ms());
        let dir_a = std::env::temp_dir().join(format!("mykvm-reset-a-{suffix}"));
        let dir_b = std::env::temp_dir().join(format!("mykvm-reset-b-{suffix}"));
        let _ = fs::remove_dir_all(&dir_a);
        let _ = fs::remove_dir_all(&dir_b);

        let noop_datagram: DatagramHandler = Arc::new(|_, _| {});
        let noop_stream: StreamHandler = Arc::new(|_, _| false);
        let transport_a =
            start(53800, dir_a.clone(), noop_datagram, noop_stream).expect("start reset sender");
        let identity_b = load_or_create_identity(&dir_b).expect("create reset receiver identity");
        let blackhole = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind reset blackhole");
        let receiver_port = blackhole.local_addr().expect("reset blackhole addr").port();
        let peer_b = transport_a.peer(
            format!("127.0.0.1:{receiver_port}"),
            identity_b.public_key,
            PROTOCOL_VERSION,
        );

        transport_a
            .send_reliable_input_with_class(
                peer_b.clone(),
                b"stale-current".to_vec(),
                ReliableInputClass::State,
            )
            .expect("queue current state");
        std::thread::sleep(Duration::from_millis(50));
        transport_a
            .send_reliable_input_with_class(
                peer_b.clone(),
                b"stale-queued".to_vec(),
                ReliableInputClass::State,
            )
            .expect("queue stale state");
        transport_a
            .send_reliable_input_with_class(
                peer_b.clone(),
                b"stale-scroll".to_vec(),
                ReliableInputClass::Transient,
            )
            .expect("queue stale transient");
        transport_a
            .send_reliable_input_with_class(
                peer_b,
                b"reset-boundary".to_vec(),
                ReliableInputClass::ResetBoundary,
            )
            .expect("queue reset boundary");

        drop(blackhole);
        let (input_tx, input_rx) = mpsc::channel();
        let received_input: DatagramHandler = Arc::new(move |payload, _| {
            let _ = input_tx.send(payload);
        });
        let noop_stream_b: StreamHandler = Arc::new(|_, _| false);
        let transport_b = start(receiver_port, dir_b.clone(), received_input, noop_stream_b)
            .expect("start reset receiver");
        assert_eq!(transport_b.port(), receiver_port);

        let first = input_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("receive reset boundary");
        let extra = input_rx.recv_timeout(Duration::from_millis(300)).ok();

        transport_a.shutdown();
        transport_b.shutdown();
        let _ = fs::remove_dir_all(&dir_a);
        let _ = fs::remove_dir_all(&dir_b);

        assert_eq!(first, b"reset-boundary");
        assert_eq!(extra, None, "stale state followed the reset boundary");
    }

    #[test]
    fn queued_release_survives_failed_state_without_replaying_state() {
        let suffix = format!("{}-{}", std::process::id(), crate::now_ms());
        let dir_a = std::env::temp_dir().join(format!("mykvm-release-a-{suffix}"));
        let dir_b = std::env::temp_dir().join(format!("mykvm-release-b-{suffix}"));
        let _ = fs::remove_dir_all(&dir_a);
        let _ = fs::remove_dir_all(&dir_b);

        let noop_datagram: DatagramHandler = Arc::new(|_, _| {});
        let noop_stream: StreamHandler = Arc::new(|_, _| false);
        let transport_a =
            start(54000, dir_a.clone(), noop_datagram, noop_stream).expect("start release sender");
        let identity_b = load_or_create_identity(&dir_b).expect("create release receiver identity");
        let blackhole = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind release blackhole");
        let receiver_port = blackhole
            .local_addr()
            .expect("release blackhole addr")
            .port();
        let peer_b = transport_a.peer(
            format!("127.0.0.1:{receiver_port}"),
            identity_b.public_key,
            PROTOCOL_VERSION,
        );

        transport_a
            .send_reliable_input_with_class(
                peer_b.clone(),
                b"stale-state".to_vec(),
                ReliableInputClass::State,
            )
            .expect("queue stale state");
        std::thread::sleep(Duration::from_millis(50));
        transport_a
            .send_reliable_input_with_class(
                peer_b,
                b"release".to_vec(),
                ReliableInputClass::Release,
            )
            .expect("queue release");

        // The state frame has exhausted its two attempts; the release is now in
        // the separate bounded recovery window when B becomes available.
        std::thread::sleep(Duration::from_millis(2_600));
        drop(blackhole);
        let (input_tx, input_rx) = mpsc::channel();
        let received_input: DatagramHandler = Arc::new(move |payload, _| {
            let _ = input_tx.send(payload);
        });
        let noop_stream_b: StreamHandler = Arc::new(|_, _| false);
        let transport_b = start(receiver_port, dir_b.clone(), received_input, noop_stream_b)
            .expect("start release receiver");
        assert_eq!(transport_b.port(), receiver_port);

        let first = input_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("receive preserved release");
        let extra = input_rx.recv_timeout(Duration::from_millis(300)).ok();

        transport_a.shutdown();
        transport_b.shutdown();
        let _ = fs::remove_dir_all(&dir_a);
        let _ = fs::remove_dir_all(&dir_b);

        assert_eq!(first, b"release");
        assert_eq!(extra, None, "stale state replayed before/after release");
    }

    #[test]
    fn blocking_stream_handlers_do_not_starve_input_readers() {
        let suffix = format!("{}-{}", std::process::id(), crate::now_ms());
        let dir_a = std::env::temp_dir().join(format!("mykvm-stream-isolation-a-{suffix}"));
        let dir_b = std::env::temp_dir().join(format!("mykvm-stream-isolation-b-{suffix}"));
        let dir_c = std::env::temp_dir().join(format!("mykvm-stream-isolation-c-{suffix}"));
        for dir in [&dir_a, &dir_b, &dir_c] {
            let _ = fs::remove_dir_all(dir);
        }

        let noop_datagram: DatagramHandler = Arc::new(|_, _| {});
        let noop_stream: StreamHandler = Arc::new(|_, _| false);
        let transport_a = start(
            51000,
            dir_a.clone(),
            Arc::clone(&noop_datagram),
            Arc::clone(&noop_stream),
        )
        .expect("start sender A");
        let transport_c =
            start(51400, dir_c.clone(), noop_datagram, noop_stream).expect("start sender C");

        let (input_tx, input_rx) = mpsc::channel();
        let received_input: DatagramHandler = Arc::new(move |payload, _| {
            let _ = input_tx.send(payload);
        });
        let entered = Arc::new((Mutex::new(0_usize), Condvar::new()));
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let entered_for_handler = Arc::clone(&entered);
        let release_for_handler = Arc::clone(&release);
        let blocking_stream: StreamHandler = Arc::new(move |_, _| {
            let (entered_lock, entered_cv) = &*entered_for_handler;
            let mut count = entered_lock.lock().expect("entered lock");
            *count += 1;
            entered_cv.notify_all();
            drop(count);

            let (release_lock, release_cv) = &*release_for_handler;
            let released = release_lock.lock().expect("release lock");
            let _released = release_cv
                .wait_while(released, |released| !*released)
                .expect("release wait");
            true
        });
        let transport_b =
            start(51200, dir_b.clone(), received_input, blocking_stream).expect("start receiver B");

        let peer_b_from_a = transport_a.peer(
            format!("127.0.0.1:{}", transport_b.port()),
            transport_b.public_key().to_string(),
            PROTOCOL_VERSION,
        );
        let peer_b_from_c = transport_c.peer(
            format!("127.0.0.1:{}", transport_b.port()),
            transport_b.public_key().to_string(),
            PROTOCOL_VERSION,
        );

        // Establish both input paths before occupying B's worker threads, so
        // this assertion measures receiver starvation rather than handshakes.
        transport_a
            .send_datagram(peer_b_from_a.clone(), b"warm-datagram".to_vec())
            .expect("warm datagram");
        transport_a
            .send_reliable_input(peer_b_from_a.clone(), b"warm-reliable".to_vec())
            .expect("warm reliable input");
        let mut warmed = Vec::new();
        while warmed.len() < 2 {
            warmed.push(
                input_rx
                    .recv_timeout(Duration::from_secs(5))
                    .expect("warm input path"),
            );
        }
        assert!(warmed.iter().any(|payload| payload == b"warm-datagram"));
        assert!(warmed.iter().any(|payload| payload == b"warm-reliable"));

        let stream_a = transport_a.clone();
        let stream_peer_a = peer_b_from_a.clone();
        let send_a = std::thread::spawn(move || {
            stream_a.send_stream_expect_ack(stream_peer_a, b"block-a".to_vec())
        });
        let stream_c = transport_c.clone();
        let send_c = std::thread::spawn(move || {
            stream_c.send_stream_expect_ack(peer_b_from_c, b"block-c".to_vec())
        });

        let (entered_lock, entered_cv) = &*entered;
        let count = entered_lock.lock().expect("entered lock");
        let (count, timeout) = entered_cv
            .wait_timeout_while(count, Duration::from_secs(5), |count| *count < 2)
            .expect("entered wait");
        assert!(!timeout.timed_out(), "both blocking handlers must start");
        assert_eq!(*count, 2);
        drop(count);

        transport_a
            .send_datagram(peer_b_from_a.clone(), b"live-datagram".to_vec())
            .expect("send live datagram");
        transport_a
            .send_reliable_input(peer_b_from_a, b"live-reliable".to_vec())
            .expect("send live reliable input");

        let deadline = Instant::now() + Duration::from_millis(750);
        let mut live = Vec::new();
        while live.len() < 2 {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            match input_rx.recv_timeout(deadline - now) {
                Ok(payload) => live.push(payload),
                Err(_) => break,
            }
        }
        let input_arrived_while_handlers_blocked = live.len() == 2
            && live.iter().any(|payload| payload == b"live-datagram")
            && live.iter().any(|payload| payload == b"live-reliable");

        let (release_lock, release_cv) = &*release;
        *release_lock.lock().expect("release lock") = true;
        release_cv.notify_all();
        let _ = send_a.join().expect("stream sender A");
        let _ = send_c.join().expect("stream sender C");

        transport_a.shutdown();
        transport_c.shutdown();
        transport_b.shutdown();
        for dir in [&dir_a, &dir_b, &dir_c] {
            let _ = fs::remove_dir_all(dir);
        }

        assert!(
            input_arrived_while_handlers_blocked,
            "datagram and reliable-input readers were starved by blocking stream handlers"
        );
    }

    #[test]
    fn stream_ack_waits_for_a_bounded_slow_handler() {
        let suffix = format!("{}-{}", std::process::id(), crate::now_ms());
        let dir_a = std::env::temp_dir().join(format!("mykvm-slow-ack-a-{suffix}"));
        let dir_b = std::env::temp_dir().join(format!("mykvm-slow-ack-b-{suffix}"));
        let _ = fs::remove_dir_all(&dir_a);
        let _ = fs::remove_dir_all(&dir_b);

        let noop_datagram: DatagramHandler = Arc::new(|_, _| {});
        let noop_stream: StreamHandler = Arc::new(|_, _| false);
        let transport_a =
            start(51600, dir_a.clone(), noop_datagram, noop_stream).expect("start slow-ack sender");

        let slow_stream: StreamHandler = Arc::new(|payload, _| {
            assert_eq!(payload, b"clipboard-image");
            std::thread::sleep(Duration::from_millis(750));
            true
        });
        let noop_datagram_b: DatagramHandler = Arc::new(|_, _| {});
        let transport_b = start(51800, dir_b.clone(), noop_datagram_b, slow_stream)
            .expect("start slow-ack receiver");

        let peer_b = transport_a.peer(
            format!("127.0.0.1:{}", transport_b.port()),
            transport_b.public_key().to_string(),
            PROTOCOL_VERSION,
        );
        let started = Instant::now();
        let result = transport_a.send_stream_expect_ack(peer_b, b"clipboard-image".to_vec());
        let elapsed = started.elapsed();

        transport_a.shutdown();
        transport_b.shutdown();
        let _ = fs::remove_dir_all(&dir_a);
        let _ = fs::remove_dir_all(&dir_b);

        assert!(
            result.is_ok(),
            "a bounded 750ms receiver write must fit the stream command budget: {result:?}"
        );
        assert!(
            elapsed < STREAM_COMMAND_TIMEOUT,
            "the bounded handler must not turn into an unbounded wait: {elapsed:?}"
        );
    }

    #[test]
    fn independent_bi_streams_run_concurrently_on_one_connection() {
        let suffix = format!("{}-{}", std::process::id(), crate::now_ms());
        let dir_a = std::env::temp_dir().join(format!("mykvm-stream-concurrency-a-{suffix}"));
        let dir_b = std::env::temp_dir().join(format!("mykvm-stream-concurrency-b-{suffix}"));
        let _ = fs::remove_dir_all(&dir_a);
        let _ = fs::remove_dir_all(&dir_b);

        let noop_datagram: DatagramHandler = Arc::new(|_, _| {});
        let noop_stream: StreamHandler = Arc::new(|_, _| false);
        let transport_a = start(52000, dir_a.clone(), noop_datagram, noop_stream)
            .expect("start concurrent sender");
        let delayed_stream: StreamHandler = Arc::new(|payload, _| {
            if payload != b"warm" {
                std::thread::sleep(Duration::from_millis(100));
            }
            true
        });
        let noop_datagram_b: DatagramHandler = Arc::new(|_, _| {});
        let transport_b = start(52200, dir_b.clone(), noop_datagram_b, delayed_stream)
            .expect("start concurrent receiver");
        let peer_b = transport_a.peer(
            format!("127.0.0.1:{}", transport_b.port()),
            transport_b.public_key().to_string(),
            PROTOCOL_VERSION,
        );

        // Establish the cached connection first. The timed section then
        // measures four independent streams, not a TLS handshake.
        transport_a
            .send_stream_expect_ack(peer_b.clone(), b"warm".to_vec())
            .expect("warm stream connection");

        let barrier = Arc::new(Barrier::new(5));
        let mut sends: Vec<std::thread::JoinHandle<Result<(), String>>> = Vec::new();
        for index in 0..4_u8 {
            let transport = transport_a.clone();
            let peer = peer_b.clone();
            let barrier = Arc::clone(&barrier);
            sends.push(std::thread::spawn(move || {
                barrier.wait();
                transport.send_stream_expect_ack(peer, vec![index])
            }));
        }

        let started = Instant::now();
        barrier.wait();
        let results: Vec<Result<(), String>> = sends
            .into_iter()
            .map(|send| send.join().expect("stream sender thread"))
            .collect::<Vec<_>>();
        let elapsed = started.elapsed();

        transport_a.shutdown();
        transport_b.shutdown();
        let _ = fs::remove_dir_all(&dir_a);
        let _ = fs::remove_dir_all(&dir_b);

        assert!(
            results.iter().all(|result| result.is_ok()),
            "results: {results:?}"
        );
        assert!(
            elapsed < Duration::from_millis(300),
            "four 100ms handlers ran serially instead of concurrently: {elapsed:?}"
        );
    }

    #[test]
    fn stream_shutdown_waits_for_in_flight_ack() {
        let suffix = format!("{}-{}", std::process::id(), crate::now_ms());
        let dir_a = std::env::temp_dir().join(format!("mykvm-stream-shutdown-a-{suffix}"));
        let dir_b = std::env::temp_dir().join(format!("mykvm-stream-shutdown-b-{suffix}"));
        let _ = fs::remove_dir_all(&dir_a);
        let _ = fs::remove_dir_all(&dir_b);

        let noop_datagram: DatagramHandler = Arc::new(|_, _| {});
        let noop_stream: StreamHandler = Arc::new(|_, _| false);
        let transport_a =
            start(52400, dir_a.clone(), noop_datagram, noop_stream).expect("start shutdown sender");
        let (entered_tx, entered_rx) = mpsc::channel();
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let release_for_handler = Arc::clone(&release);
        let delayed_stream: StreamHandler = Arc::new(move |_, _| {
            let _ = entered_tx.send(());
            let (release_lock, release_cv) = &*release_for_handler;
            let released = release_lock.lock().expect("release lock");
            let _released = release_cv
                .wait_while(released, |released| !*released)
                .expect("release wait");
            true
        });
        let noop_datagram_b: DatagramHandler = Arc::new(|_, _| {});
        let transport_b = start(52600, dir_b.clone(), noop_datagram_b, delayed_stream)
            .expect("start shutdown receiver");
        let peer_b = transport_a.peer(
            format!("127.0.0.1:{}", transport_b.port()),
            transport_b.public_key().to_string(),
            PROTOCOL_VERSION,
        );

        let sender = transport_a.clone();
        let send = std::thread::spawn(move || {
            sender.send_stream_expect_ack(peer_b, b"in-flight".to_vec())
        });
        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("receiver handler entered");

        let release_after_delay = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            let (release_lock, release_cv) = &*release;
            *release_lock.lock().expect("release lock") = true;
            release_cv.notify_all();
        });
        let started = Instant::now();
        transport_a.shutdown();
        let elapsed = started.elapsed();
        release_after_delay.join().expect("release handler");
        let send_result = send.join().expect("in-flight sender");

        transport_b.shutdown();
        let _ = fs::remove_dir_all(&dir_a);
        let _ = fs::remove_dir_all(&dir_b);

        assert!(send_result.is_ok(), "in-flight result: {send_result:?}");
        assert!(
            elapsed >= Duration::from_millis(150),
            "shutdown returned before the in-flight handler ACK: {elapsed:?}"
        );
        assert!(elapsed < STREAM_SHUTDOWN_TIMEOUT);
    }

    #[test]
    fn failed_stream_connection_is_rebuilt_for_next_send() {
        let suffix = format!("{}-{}", std::process::id(), crate::now_ms());
        let dir_a = std::env::temp_dir().join(format!("mykvm-stream-rebuild-a-{suffix}"));
        let dir_b = std::env::temp_dir().join(format!("mykvm-stream-rebuild-b-{suffix}"));
        let _ = fs::remove_dir_all(&dir_a);
        let _ = fs::remove_dir_all(&dir_b);

        let noop_datagram: DatagramHandler = Arc::new(|_, _| {});
        let noop_stream: StreamHandler = Arc::new(|_, _| false);
        let transport_a =
            start(52800, dir_a.clone(), noop_datagram, noop_stream).expect("start rebuild sender");
        let selective_stream: StreamHandler = Arc::new(|payload, _| payload != b"reject");
        let noop_datagram_b: DatagramHandler = Arc::new(|_, _| {});
        let transport_b = start(53000, dir_b.clone(), noop_datagram_b, selective_stream)
            .expect("start rebuild receiver");
        let peer_b = transport_a.peer(
            format!("127.0.0.1:{}", transport_b.port()),
            transport_b.public_key().to_string(),
            PROTOCOL_VERSION,
        );

        transport_a
            .send_stream_expect_ack(peer_b.clone(), b"warm".to_vec())
            .expect("warm cached connection");
        let rejected = transport_a.send_stream_expect_ack(peer_b.clone(), b"reject".to_vec());
        let recovered = transport_a.send_stream_expect_ack(peer_b, b"recover".to_vec());

        transport_a.shutdown();
        transport_b.shutdown();
        let _ = fs::remove_dir_all(&dir_a);
        let _ = fs::remove_dir_all(&dir_b);

        assert!(rejected.is_err(), "receiver rejection must reach caller");
        assert!(
            recovered.is_ok(),
            "next send must rebuild the evicted connection: {recovered:?}"
        );
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

        // A zero-length reliable frame is a transport-only recovery probe.
        // The receiver must ignore it without closing the persistent stream;
        // the three real input frames below must still arrive in order.
        transport_a
            .send_reliable_input(peer_b.clone(), Vec::new())
            .expect("enqueue no-op recovery probe");
        transport_a
            .send_reliable_input(peer_b.clone(), b"key-down".to_vec())
            .expect("enqueue key-down");
        let first = rx.recv_timeout(Duration::from_secs(5)).expect("key-down");
        assert_eq!(first, b"key-down");
        transport_a
            .send_reliable_input_with_class(
                peer_b.clone(),
                b"key-up".to_vec(),
                ReliableInputClass::Release,
            )
            .expect("enqueue key-up");
        transport_a
            .send_reliable_input_with_class(
                peer_b,
                b"cursor-park".to_vec(),
                ReliableInputClass::ResetBoundary,
            )
            .expect("enqueue final boundary");

        // Shutdown must finish the persistent input stream and wait for its
        // bytes to be acknowledged. Receive only after shutdown returns so this
        // fails if the final frames were merely queued and then reset.
        transport_a.shutdown();

        let second = rx.recv_timeout(Duration::from_secs(5)).expect("key-up");
        let third = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("cursor-park");
        assert_eq!(second, b"key-up");
        assert_eq!(third, b"cursor-park");

        transport_b.shutdown();
        let _ = fs::remove_dir_all(&dir_a);
        let _ = fs::remove_dir_all(&dir_b);
    }

    #[test]
    #[ignore = "spins up and restarts live QUIC endpoints on loopback"]
    fn background_reliable_probe_recovers_before_next_datagram() {
        let suffix = format!("{}-{}", std::process::id(), crate::now_ms());
        let dir_a = std::env::temp_dir().join(format!("mykvm-probe-recovery-a-{suffix}"));
        let dir_b = std::env::temp_dir().join(format!("mykvm-probe-recovery-b-{suffix}"));
        let _ = fs::remove_dir_all(&dir_a);
        let _ = fs::remove_dir_all(&dir_b);

        let noop_datagram: DatagramHandler = Arc::new(|_, _| {});
        let noop_stream: StreamHandler = Arc::new(|_, _| false);
        let transport_a = start(
            56000,
            dir_a.clone(),
            noop_datagram,
            Arc::clone(&noop_stream),
        )
        .expect("start recovery sender");
        let transport_b = start(
            56200,
            dir_b.clone(),
            Arc::new(|_, _| {}),
            Arc::clone(&noop_stream),
        )
        .expect("start recovery receiver");
        let receiver_port = transport_b.port();
        let peer_b = transport_a.peer(
            format!("127.0.0.1:{receiver_port}"),
            transport_b.public_key().to_string(),
            PROTOCOL_VERSION,
        );
        transport_b.shutdown();
        drop(transport_b);

        transport_a
            .send_reliable_input(peer_b.clone(), b"key-down".to_vec())
            .expect("admit input that will expose the stopped peer");
        let failed_deadline = Instant::now() + Duration::from_secs(5);
        while !transport_a.peer_input_failed(&peer_b) && Instant::now() < failed_deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            transport_a.peer_input_failed(&peer_b),
            "the stopped reliable path must become externally failed"
        );

        let (received_tx, received_rx) = mpsc::channel();
        let received: DatagramHandler = Arc::new(move |payload, _| {
            let _ = received_tx.send(payload);
        });
        let restarted_b = start(
            receiver_port,
            dir_b.clone(),
            received,
            Arc::clone(&noop_stream),
        )
        .expect("restart recovery receiver");
        assert_eq!(restarted_b.port(), receiver_port);

        let recovery_deadline = Instant::now() + Duration::from_secs(5);
        while transport_a.peer_input_failed(&peer_b) && Instant::now() < recovery_deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !transport_a.peer_input_failed(&peer_b),
            "the background no-op reliable probe must recover before a new entry"
        );
        transport_a
            .send_latest_datagram(peer_b, b"enter".to_vec())
            .expect("entry datagram after reliable recovery");
        assert_eq!(
            received_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("receive post-recovery datagram"),
            b"enter"
        );

        transport_a.shutdown();
        restarted_b.shutdown();
        let _ = fs::remove_dir_all(&dir_a);
        let _ = fs::remove_dir_all(&dir_b);
    }

    #[test]
    #[ignore = "uses a live QUIC peer that appears during shutdown drain"]
    fn shutdown_drain_retries_release_after_peer_recovers() {
        let suffix = format!("{}-{}", std::process::id(), crate::now_ms());
        let dir_a = std::env::temp_dir().join(format!("mykvm-shutdown-drain-a-{suffix}"));
        let dir_b = std::env::temp_dir().join(format!("mykvm-shutdown-drain-b-{suffix}"));
        let _ = fs::remove_dir_all(&dir_a);
        let _ = fs::remove_dir_all(&dir_b);

        let noop_datagram: DatagramHandler = Arc::new(|_, _| {});
        let noop_stream: StreamHandler = Arc::new(|_, _| false);
        let transport_a = start(
            56400,
            dir_a.clone(),
            noop_datagram,
            Arc::clone(&noop_stream),
        )
        .expect("start shutdown-drain sender");
        let identity_b = load_or_create_identity(&dir_b).expect("create receiver identity");
        let blackhole = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind blackhole");
        let receiver_port = blackhole.local_addr().expect("blackhole addr").port();
        let peer_b = transport_a.peer(
            format!("127.0.0.1:{receiver_port}"),
            identity_b.public_key,
            PROTOCOL_VERSION,
        );
        transport_a
            .send_reliable_input_with_class(
                peer_b,
                b"final-key-up".to_vec(),
                ReliableInputClass::Release,
            )
            .expect("queue final release");

        let shutdown_handle = transport_a.clone();
        let shutdown_started = Instant::now();
        let shutdown = std::thread::spawn(move || shutdown_handle.shutdown());

        // Let two 1s attempts expire while shutdown is set. The drain must
        // still retain the Release and use its remaining bounded recovery
        // budget once the peer returns.
        std::thread::sleep(Duration::from_millis(2_100));
        drop(blackhole);
        let (received_tx, received_rx) = mpsc::channel();
        let received: DatagramHandler = Arc::new(move |payload, _| {
            let _ = received_tx.send(payload);
        });
        let transport_b = start(receiver_port, dir_b.clone(), received, noop_stream)
            .expect("start peer during shutdown drain");
        assert_eq!(transport_b.port(), receiver_port);

        assert_eq!(
            received_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("receive release during shutdown drain"),
            b"final-key-up"
        );
        shutdown.join().expect("shutdown thread");
        assert!(
            shutdown_started.elapsed() < Duration::from_secs(4),
            "shutdown drain exceeded its caller-side bound"
        );

        transport_b.shutdown();
        let _ = fs::remove_dir_all(&dir_a);
        let _ = fs::remove_dir_all(&dir_b);
    }
}
