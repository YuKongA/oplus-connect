//! Bounded ODMF signaling for one authenticated Panta owner, at most two TCP peers.
//! Media negotiation is supplied by the controller; UDP time sync is peer-IP scoped.
use super::{Codec, auth::Identity, invalid, read_frame, write_plain};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{self, ErrorKind},
    net::{IpAddr, SocketAddr},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream, UdpSocket},
    sync::{mpsc, oneshot, watch},
    task::{JoinHandle, JoinSet},
    time::timeout,
};

pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(90);
// A multi-interface desktop can emit dozens of ICE candidates in one burst.
// Match the bounded media signaling queue rather than dropping valid startup.
const QUEUE_CAPACITY: usize = 64;
const MAX_CONNECTIONS: usize = 2;
static NEXT_CONNECTION_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub bind_ip: IpAddr,
    pub peer_ip: IpAddr,
    pub port: Option<u16>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ServerState {
    Listening,
    Authenticating,
    SignalingReady,
    Closed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionStatus {
    pub state: ServerState,
    /// IDs are unique per TCP connection and appear only after successful pairing.
    pub connection_ids: Vec<u64>,
    pub last_error: Option<String>,
}

/// Authenticated peer messages. Control envelopes are observations, not a
/// claim that this server implements their media or time-sync functionality.
#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
pub enum IncomingMessage {
    Signaling(Value),
    Control(Value),
}

struct Command {
    frame: Vec<u8>,
    close: bool,
    reply: oneshot::Sender<io::Result<()>>,
}

struct Peer {
    ready: bool,
    outgoing: mpsc::Sender<Command>,
    incoming: mpsc::Receiver<IncomingMessage>,
    cancel: watch::Sender<bool>,
}

#[derive(Default)]
struct Shared {
    peers: BTreeMap<u64, Peer>,
    closed: bool,
    last_error: Option<String>,
}

type SharedState = Arc<Mutex<Shared>>;

/// Aborting the accept task also drops its JoinSet, aborting every TCP worker.
/// Explicit shutdown additionally joins all workers before returning.
pub struct Server {
    task: Option<JoinHandle<()>>,
    time_sync: Option<JoinHandle<()>>,
    shutdown: watch::Sender<bool>,
    local_addr: SocketAddr,
    time_sync_addr: Option<SocketAddr>,
    identity: Arc<Identity>,
    codec: Codec,
    shared: SharedState,
}

impl Server {
    pub async fn bind(config: ServerConfig) -> io::Result<Self> {
        let identity = Arc::new(Identity::new());
        Self::bind_with_identity(config, identity).await
    }

    pub async fn bind_with_identity(
        config: ServerConfig,
        identity: Arc<Identity>,
    ) -> io::Result<Self> {
        if config.bind_ip.is_unspecified()
            || config.peer_ip.is_unspecified()
            || config.bind_ip.is_multicast()
            || config.peer_ip.is_multicast()
            || config.bind_ip.is_ipv4() != config.peer_ip.is_ipv4()
        {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "invalid cast listener configuration",
            ));
        }
        let port = config.port.unwrap_or(0);
        let listener = TcpListener::bind(SocketAddr::new(config.bind_ip, port)).await?;
        let local_addr = listener.local_addr()?;
        // Bind the advertised endpoint before publishing it to the owner.
        let time_sync_socket = UdpSocket::bind(SocketAddr::new(config.bind_ip, 0)).await?;
        let time_sync_addr = time_sync_socket.local_addr()?;
        let codec = identity.codec();
        let shared = Arc::new(Mutex::new(Shared::default()));
        let (shutdown, shutdown_rx) = watch::channel(false);
        let task = tokio::spawn(accept_loop(
            listener,
            config.peer_ip,
            Some(time_sync_addr),
            identity.clone(),
            shared.clone(),
            shutdown_rx,
        ));
        let time_sync = Some(tokio::spawn(time_sync_loop(
            time_sync_socket,
            config.peer_ip,
            shutdown.subscribe(),
        )));
        Ok(Self {
            task: Some(task),
            time_sync,
            shutdown,
            local_addr,
            time_sync_addr: Some(time_sync_addr),
            identity,
            codec,
            shared,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Advertised to the peer in `TimeSyncServer`; None only if unavailable.
    pub fn time_sync_addr(&self) -> Option<SocketAddr> {
        self.time_sync_addr
    }

    pub fn advertisement(&self) -> Value {
        self.identity.advertisement()
    }
    pub fn connection_ids(&self) -> Vec<u64> {
        self.status().connection_ids
    }

    pub fn status(&self) -> SessionStatus {
        let shared = self.shared.lock().unwrap();
        let connection_ids: Vec<_> = shared
            .peers
            .iter()
            .filter_map(|(&id, peer)| peer.ready.then_some(id))
            .collect();
        let state = if shared.closed {
            ServerState::Closed
        } else if !connection_ids.is_empty() {
            ServerState::SignalingReady
        } else if !shared.peers.is_empty() {
            ServerState::Authenticating
        } else {
            ServerState::Listening
        };
        SessionStatus {
            state,
            connection_ids,
            last_error: shared.last_error.clone(),
        }
    }

    /// Sends inner signaling JSON, not an ODMF envelope. Success means the frame
    /// was written, not that the remote media engine accepted the negotiation.
    pub async fn send(&self, connection_id: u64, payload: Value) -> io::Result<()> {
        if !payload.is_object() {
            return Err(invalid("cast signaling payload must be a JSON object"));
        }
        if payload.get("cmd").is_some() {
            return self.send_envelope(connection_id, payload, false).await;
        }
        let close = is_bye(&payload);
        let message =
            serde_json::to_string(&payload).map_err(|_| invalid("invalid cast signaling JSON"))?;
        self.send_envelope(connection_id, json!({"cmd":"send","msg":message}), close)
            .await
    }

    /// Requests the captured rotate-with-content video mode. The negotiated
    /// codec and the phone still determine the actual output dimensions/rate.
    pub async fn configure_video(
        &self,
        connection_id: u64,
        width: u32,
        fps: u32,
    ) -> io::Result<()> {
        if !(1..=4096).contains(&width) || !(1..=60).contains(&fps) {
            return Err(invalid("cast width must be 1..4096 and fps 1..60"));
        }
        self.send_envelope(
            connection_id,
            json!({"cmd":"customBroadcast","data":{
                "COMMAND":"SET_PARAMETER","foldRotatesWithContent":true,
                "fps":fps,"height":0,"rotatesWithContent":true,"width":width
            }}),
            false,
        )
        .await
    }

    async fn send_envelope(
        &self,
        connection_id: u64,
        envelope: Value,
        close: bool,
    ) -> io::Result<()> {
        let outgoing = {
            let shared = self.shared.lock().unwrap();
            shared
                .peers
                .get(&connection_id)
                .filter(|peer| peer.ready)
                .map(|peer| peer.outgoing.clone())
                .ok_or_else(disconnected)?
        };
        let body = serde_json::to_vec(&envelope)
            .map_err(|_| invalid("invalid cast signaling envelope"))?;
        let mut frame = Vec::new();
        self.codec.encode(&body, &mut frame)?;
        let frame_len = frame.len();
        let (reply, result) = oneshot::channel();
        if let Err(error) = outgoing.try_send(Command {
            frame,
            close,
            reply,
        }) {
            let message = match error {
                mpsc::error::TrySendError::Full(_) => {
                    "cast outgoing signaling queue full; connection closed"
                }
                mpsc::error::TrySendError::Closed(_) => "cast signaling connection closed",
            };
            close_peer(&self.shared, connection_id, message);
            return Err(io::Error::new(ErrorKind::BrokenPipe, message));
        }
        result.await.map_err(|_| disconnected())??;
        tracing::debug!("cast connection {connection_id} wrote {} bytes", frame_len);
        Ok(())
    }

    /// Nonblocking dequeue. Queues belong to a TCP token, never to a reconnect.
    pub fn receive(&self, connection_id: u64) -> io::Result<Option<IncomingMessage>> {
        Ok(self.receive_batch(connection_id, 1)?.pop())
    }

    /// Drain up to `max` messages in FIFO order, bounded by the queue capacity.
    pub fn receive_batch(
        &self,
        connection_id: u64,
        max: usize,
    ) -> io::Result<Vec<IncomingMessage>> {
        let mut shared = self.shared.lock().unwrap();
        let peer = shared
            .peers
            .get_mut(&connection_id)
            .filter(|peer| peer.ready)
            .ok_or_else(disconnected)?;
        let mut messages = Vec::new();
        while messages.len() < max.min(QUEUE_CAPACITY) {
            match peer.incoming.try_recv() {
                Ok(payload) => messages.push(payload),
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    if messages.is_empty() {
                        return Err(disconnected());
                    }
                    break;
                }
            }
        }
        Ok(messages)
    }

    pub async fn shutdown(&mut self) {
        self.shutdown.send_replace(true);
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
        if let Some(task) = self.time_sync.take() {
            let _ = task.await;
        }
        let mut shared = self.shared.lock().unwrap();
        shared.closed = true;
        shared.peers.clear();
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shutdown.send_replace(true);
        if let Some(task) = &self.task {
            task.abort();
        }
        if let Some(task) = &self.time_sync {
            task.abort();
        }
    }
}

fn disconnected() -> io::Error {
    io::Error::new(
        ErrorKind::NotConnected,
        "cast TCP connection token is no longer active",
    )
}

fn next_connection_id() -> u64 {
    loop {
        let id = NEXT_CONNECTION_ID.fetch_add(1, Ordering::Relaxed);
        if id != 0 {
            return id;
        }
    }
}

fn close_peer(shared: &SharedState, id: u64, message: &str) {
    let mut shared = shared.lock().unwrap();
    shared.last_error = Some(message.to_owned());
    if let Some(peer) = shared.peers.remove(&id) {
        peer.cancel.send_replace(true);
    }
}

struct PeerGuard {
    id: u64,
    shared: SharedState,
}
impl Drop for PeerGuard {
    fn drop(&mut self) {
        self.shared.lock().unwrap().peers.remove(&self.id);
    }
}

struct ListenerGuard(SharedState);
impl Drop for ListenerGuard {
    fn drop(&mut self) {
        let mut shared = self.0.lock().unwrap();
        shared.closed = true;
        shared.peers.clear();
    }
}

async fn accept_loop(
    listener: TcpListener,
    peer_ip: IpAddr,
    time_sync_addr: Option<SocketAddr>,
    identity: Arc<Identity>,
    shared: SharedState,
    mut shutdown: watch::Receiver<bool>,
) {
    let _guard = ListenerGuard(shared.clone());
    let mut tasks = JoinSet::new();
    loop {
        tokio::select! {
            biased;
            _ = shutdown.changed() => break,
            result = tasks.join_next(), if !tasks.is_empty() => {
                if let Some(Err(error)) = result {
                    shared.lock().unwrap().last_error = Some(format!("cast connection task failed: {error}"));
                }
            }
            accepted = listener.accept() => {
                let (stream, address) = match accepted {
                    Ok(peer) => peer,
                    Err(error) => {
                        shared.lock().unwrap().last_error = Some(format!("cast listener failed: {error}"));
                        break;
                    }
                };
                if address.ip() != peer_ip || tasks.len() >= MAX_CONNECTIONS {
                    tracing::warn!("cast rejected TCP peer {address} (owner={peer_ip}, active={})", tasks.len());
                    drop(stream);
                    continue;
                }
                let id = next_connection_id();
                tracing::info!("cast accepted TCP peer {address} as connection {id}");
                let (outgoing, outgoing_rx) = mpsc::channel(QUEUE_CAPACITY);
                let (incoming, incoming_rx) = mpsc::channel(QUEUE_CAPACITY);
                let (cancel, mut cancelled) = watch::channel(false);
                shared.lock().unwrap().peers.insert(id, Peer { ready: false, outgoing, incoming: incoming_rx, cancel });
                let shared = shared.clone();
                let identity = identity.clone();
                tasks.spawn(async move {
                    let _guard = PeerGuard { id, shared: shared.clone() };
                    let result = tokio::select! {
                        biased;
                        _ = cancelled.changed() => Ok(()),
                        result = serve_peer(stream, id, time_sync_addr, identity, &shared, outgoing_rx, incoming) => result,
                    };
                    if let Err(error) = result {
                        // Error strings describe protocol failures, never received payloads or keys.
                        tracing::warn!("cast connection {id} ended: {error}");
                        close_peer(&shared, id, &error.to_string());
                    } else {
                        tracing::info!("cast connection {id} closed");
                    }
                });
            }
        }
    }
    drop(listener);
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
}

async fn plain<W: AsyncWrite + Unpin>(writer: &mut W, body: &Value) -> io::Result<()> {
    timeout(WRITE_TIMEOUT, write_plain(writer, body))
        .await
        .map_err(|_| io::Error::new(ErrorKind::TimedOut, "cast write timed out"))?
}

async fn encrypted<W: AsyncWrite + Unpin>(
    writer: &mut W,
    codec: &Codec,
    body: &Value,
) -> io::Result<()> {
    let bytes = serde_json::to_vec(body).map_err(|_| invalid("invalid encrypted cast JSON"))?;
    let mut frame = Vec::new();
    codec.encode(&bytes, &mut frame)?;
    write_frame(writer, &frame).await
}

/// Captured official exchange: the peer sends `{"client_request_time":<ms>}`
/// and the PC replies with exactly `{"client_request_time":…,
/// "server_response_time":<ms>}`. The peer derives `TimeSyncOffset` from those
/// two values, so any other field name silently yields an epoch-sized offset.
async fn time_sync_loop(socket: UdpSocket, peer_ip: IpAddr, mut shutdown: watch::Receiver<bool>) {
    let mut buffer = [0u8; 512];
    let mut logged = false;
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() { return; }
            }
            result = socket.recv_from(&mut buffer) => {
                let (len, address) = match result {
                    Ok(value) => value,
                    Err(error) => {
                        tracing::warn!("cast time-sync socket failed: {error}");
                        return;
                    }
                };
                if address.ip() != peer_ip {
                    tracing::info!("cast time-sync ignored datagram from {address}");
                    continue;
                }
                let received = epoch_millis();
                let request: Option<Value> = serde_json::from_slice(&buffer[..len]).ok();
                let Some(client_request_time) = request
                    .as_ref()
                    .and_then(|value| value.get("client_request_time"))
                    .and_then(Value::as_u64)
                else {
                    tracing::info!("cast time-sync ignored unrecognized {len}-byte request");
                    continue;
                };
                if !logged {
                    logged = true;
                    tracing::info!("cast time-sync request from {address}: client={client_request_time}");
                }
                let reply = json!({
                    "client_request_time": client_request_time,
                    "server_response_time": received,
                });
                match serde_json::to_vec(&reply) {
                    Ok(bytes) => {
                        if let Err(error) = socket.send_to(&bytes, address).await {
                            tracing::warn!("cast time-sync reply to {address} failed: {error}");
                        }
                    }
                    Err(_) => tracing::warn!("cast time-sync reply encoding failed"),
                }
            }
        }
    }
}

fn epoch_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_millis() as u64)
        .unwrap_or_default()
}

async fn write_frame<W: AsyncWrite + Unpin>(writer: &mut W, frame: &[u8]) -> io::Result<()> {
    timeout(WRITE_TIMEOUT, writer.write_all(frame))
        .await
        .map_err(|_| io::Error::new(ErrorKind::TimedOut, "cast write timed out"))?
}

async fn handshake(
    stream: &mut TcpStream,
    id: u64,
    time_sync_addr: Option<SocketAddr>,
    identity: &Identity,
    codec: &Codec,
    buffer: &mut Vec<u8>,
) -> io::Result<()> {
    plain(
        stream,
        &json!({"auth_version":1,"cmd":"OdmfAuthRequest","msg":id}),
    )
    .await?;
    if !read_frame(stream, buffer).await? {
        return Err(disconnected());
    }
    let response: Value =
        serde_json::from_slice(buffer).map_err(|_| invalid("invalid cast authentication JSON"))?;
    let success = identity.authenticate(&response)?;
    plain(stream, &success).await?;
    tracing::info!("cast connection {id} authenticated peer identity");
    if !read_frame(stream, buffer).await? {
        return Err(disconnected());
    }
    let request: Value = serde_json::from_slice(codec.decode(buffer)?)
        .map_err(|_| invalid("invalid cast pairing JSON"))?;
    if request.get("cmd").and_then(Value::as_str) != Some("PairRequest") {
        return Err(invalid("expected encrypted cast PairRequest"));
    }
    // The successful official capture has plaintext PairSuccess, despite the
    // preceding encrypted PairRequest. This is not a downgrade for signaling.
    plain(
        stream,
        &json!({"cmd":"PairSuccess","high_performance":"","platform":"",
        "remote_transportbus_id":id.to_string(),"remote_user_id":"local_client",
        "remote_user_name":"local_client","x264":""}),
    )
    .await?;
    tracing::info!("cast connection {id} paired");
    // Observed official order: PairSuccess, TimeSyncServer, then the SDP offer.
    // The peer sent TimeSyncResult and its answer only after this control frame.
    let Some(addr) = time_sync_addr else {
        tracing::warn!("cast connection {id} has no time-sync endpoint to advertise");
        return Ok(());
    };
    encrypted(
        stream,
        codec,
        &json!({"cmd":"TimeSyncServer","time_sync_server_ip":addr.ip().to_string(),
            "time_sync_server_port":addr.port()}),
    )
    .await?;
    tracing::info!("cast connection {id} advertised TimeSyncServer {addr}");
    Ok(())
}

fn is_bye(payload: &Value) -> bool {
    matches!(
        payload.get("type").and_then(Value::as_str),
        Some("bye" | "bye_ack")
    )
}

fn decode_message(codec: &Codec, buffer: &mut [u8]) -> io::Result<Value> {
    if buffer.first() == Some(&1) {
        serde_json::from_slice(codec.decode(buffer)?)
            .map_err(|_| invalid("invalid encrypted cast JSON"))
    } else {
        let message: Value =
            serde_json::from_slice(buffer).map_err(|_| invalid("invalid plaintext cast JSON"))?;
        if !matches!(
            message.get("cmd").and_then(Value::as_str),
            Some("ping" | "pong")
        ) {
            return Err(invalid("plaintext cast signaling is forbidden"));
        }
        Ok(message)
    }
}

async fn serve_peer(
    mut stream: TcpStream,
    id: u64,
    time_sync_addr: Option<SocketAddr>,
    identity: Arc<Identity>,
    shared: &SharedState,
    mut outgoing: mpsc::Receiver<Command>,
    incoming: mpsc::Sender<IncomingMessage>,
) -> io::Result<()> {
    let mut buffer = Vec::new();
    let codec = identity.codec();
    timeout(
        HANDSHAKE_TIMEOUT,
        handshake(
            &mut stream,
            id,
            time_sync_addr,
            &identity,
            &codec,
            &mut buffer,
        ),
    )
    .await
    .map_err(|_| io::Error::new(ErrorKind::TimedOut, "cast authentication/pairing timed out"))??;
    {
        let mut shared = shared.lock().unwrap();
        shared.peers.get_mut(&id).ok_or_else(disconnected)?.ready = true;
    }
    let (mut reader, mut writer) = stream.split();
    loop {
        // Keep the same read future alive across outbound messages: cancelling
        // read_exact after a partial TCP frame would corrupt stream boundaries.
        let received = {
            let next_frame = timeout(IDLE_TIMEOUT, read_frame(&mut reader, &mut buffer));
            tokio::pin!(next_frame);
            loop {
                // Not biased: a peer that floods candidates must not starve the
                // writes queued by the controller, or our SDP/ICE would wait
                // behind the inbound burst.
                tokio::select! {
                    result = &mut next_frame => break result
                        .map_err(|_| io::Error::new(ErrorKind::TimedOut, "cast signaling read timed out"))??,
                    command = outgoing.recv() => {
                        let Some(command) = command else { return Ok(()); };
                        if command.reply.is_closed() { continue; }
                        let result = write_frame(&mut writer, &command.frame).await;
                        let failure = result.as_ref().err().map(|error| error.to_string());
                        let _ = command.reply.send(result);
                        if let Some(message) = failure {
                            return Err(io::Error::new(ErrorKind::BrokenPipe, message));
                        }
                        if command.close { return Ok(()); }
                    }
                }
            }
        };
        if !received {
            return Ok(());
        }
        let message = decode_message(&codec, &mut buffer)?;
        let command = message
            .get("cmd")
            .and_then(|value| value.as_str())
            .unwrap_or("?");
        match command {
            "customBroadcast" | "TimeSyncResult" => {
                let text = serde_json::to_string(&message).unwrap_or_default();
                tracing::info!(
                    "cast connection {id} control {command}: {}",
                    &text[..text.len().min(400)]
                );
            }
            _ => tracing::info!("cast connection {id} received {command}"),
        }
        let incoming_message = match message.get("cmd").and_then(Value::as_str) {
            Some("ping") => {
                plain(&mut writer, &json!({"cmd":"pong"})).await?;
                continue;
            }
            Some("pong") => continue,
            Some("bye") => return Ok(()),
            Some("send") => {
                let payload: Value = serde_json::from_str(
                    message
                        .get("msg")
                        .and_then(Value::as_str)
                        .ok_or_else(|| invalid("cast send message must contain JSON string"))?,
                )
                .map_err(|_| invalid("invalid cast signaling payload"))?;
                if !payload.is_object() {
                    return Err(invalid("cast signaling payload must be a JSON object"));
                }
                if is_bye(&payload) {
                    return Ok(());
                }
                IncomingMessage::Signaling(payload)
            }
            Some("customBroadcast" | "TimeSyncResult") => IncomingMessage::Control(message),
            _ => return Err(invalid("unsupported cast signaling command")),
        };
        incoming
            .try_send(incoming_message)
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => io::Error::new(
                    ErrorKind::OutOfMemory,
                    "cast incoming signaling queue full; connection closed",
                ),
                mpsc::error::TrySendError::Closed(_) => disconnected(),
            })?;
    }
}

#[cfg(test)]
#[path = "server_tests.rs"]
mod tests;
