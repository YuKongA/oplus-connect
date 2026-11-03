//! Passive/active SCP listener on the official 10xxx control ports.
//!
//! Reconstructed from the macOS daemon (`O+Connect Service`, full symbol table):
//!
//! ```text
//! ScpStage: SocketAccept -> ReadHello -> WriteHello -> ReadSync -> WriteSync
//!           -> Authenticate -> ReadSyncSecure -> WriteSyncSecure
//!           -> ReadInteract -> WriteInteract -> Disconnect
//! ```
//!
//! `ScpProtocol::handle_hello_packet` compares the peer Hello's
//! `service_name` / `namespace` / `channel` with the local ones and requires
//! `local.sequence_no == received.sequence_no + 1`, so the reply to a phone's
//! Hello is a Hello, *not* a SyncSecure. Verified against the tablet: after our
//! Hello the phone keeps the connection open instead of dropping it, and the
//! SCP layer then waits for the Sync step.

use crate::scp::codec::{CMD_ACTIVE_CONNECTION, ScpFrame};
use crate::scp::proto::{MSG_HELLO, ScpHelloData, ScpSyncData, ScpSyncSecureData, ScpWrapData};
use std::collections::HashMap;
use std::io::Write;
use std::sync::LazyLock;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tracing::{info, warn};

enum PeerCommand {
    Accept,
    Close,
    DataConnection(TcpStream, std::net::SocketAddr),
}

struct PeerSession {
    connection_id: String,
    tx: mpsc::Sender<PeerCommand>,
    approval_deadline: Option<tokio::time::Instant>,
    decision_made: bool,
}

struct PeerLease(Option<(String, String)>);

impl Drop for PeerLease {
    fn drop(&mut self) {
        if let Some((peer_ip, connection_id)) = self.0.take() {
            tokio::spawn(async move {
                let mut active = peers().lock().await;
                if active.get(&peer_ip).is_some_and(|peer| peer.connection_id == connection_id) {
                    active.remove(&peer_ip);
                }
            });
        }
    }
}

const APPROVAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
const FRAME_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
const MAX_FRAME_LEN: usize = crate::scp::codec::SCP_HEADER_LEN + u16::MAX as usize;
const READ_CHUNK_LEN: usize = 8192;
const MAX_BUFFER_LEN: usize = MAX_FRAME_LEN + READ_CHUNK_LEN;
const MAX_QUERY_LEN: usize = 8192;

static PEERS: LazyLock<tokio::sync::Mutex<HashMap<String, PeerSession>>> =
    LazyLock::new(|| tokio::sync::Mutex::new(HashMap::new()));
#[cfg(test)]
pub(crate) static PEER_TEST_LOCK: LazyLock<tokio::sync::Mutex<()>> =
    LazyLock::new(|| tokio::sync::Mutex::new(()));

fn peers() -> &'static tokio::sync::Mutex<HashMap<String, PeerSession>> {
    &PEERS
}

#[cfg(test)]
pub async fn has_peer(peer_ip: &str) -> bool {
    peers().lock().await.contains_key(peer_ip)
}

#[cfg(test)]
pub async fn peer_connection_id(peer_ip: &str) -> Option<String> {
    peers()
        .lock()
        .await
        .get(peer_ip)
        .map(|peer| peer.connection_id.clone())
}

/// Mark only this connection as pending; repeated Hello packets cannot renew
/// the deadline or create a second approval after a decision.
async fn note_awaiting_approval(peer_ip: &str, connection_id: &str) {
    let mut active = peers().lock().await;
    if let Some(peer) = active.get_mut(peer_ip) {
        if peer.connection_id == connection_id && !peer.decision_made {
            peer.approval_deadline
                .get_or_insert_with(|| tokio::time::Instant::now() + APPROVAL_TIMEOUT);
        }
    }
}

async fn clear_awaiting_approval(peer_ip: &str, connection_id: &str) {
    let mut active = peers().lock().await;
    if let Some(peer) = active.get_mut(peer_ip) {
        if peer.connection_id == connection_id {
            peer.approval_deadline = None;
            peer.decision_made = true;
        }
    }
}

async fn decide_peer(peer_ip: &str, connection_id: &str, command: PeerCommand) -> bool {
    let mut active = peers().lock().await;
    let Some(peer) = active.get_mut(peer_ip) else {
        return false;
    };
    if connection_id.is_empty() || peer.connection_id != connection_id || peer.decision_made {
        return false;
    }
    let Some(deadline) = peer.approval_deadline else {
        return false;
    };
    if tokio::time::Instant::now() >= deadline {
        peer.approval_deadline = None;
        peer.decision_made = true;
        return false;
    }
    // Enqueue while holding the registry lock so a replacement connection
    // cannot redirect or race the decision. Never block on a full queue.
    if peer.tx.try_send(command).is_err() {
        return false;
    }
    peer.approval_deadline = None;
    peer.decision_made = true;
    true
}

pub async fn accept_peer(peer_ip: &str, connection_id: &str) -> bool {
    decide_peer(peer_ip, connection_id, PeerCommand::Accept).await
}

pub async fn reject_peer(peer_ip: &str, connection_id: &str) -> bool {
    decide_peer(peer_ip, connection_id, PeerCommand::Close).await
}

/// KSC established by the SCP `SyncSecure` of the current connection, keyed by
/// the phone's device id. The panta data channel authenticates with this key.
static SESSION_KSC: LazyLock<tokio::sync::Mutex<HashMap<String, Vec<u8>>>> =
    LazyLock::new(|| tokio::sync::Mutex::new(HashMap::new()));

/// Records the KSC exchanged during this SCP connection.
pub async fn remember_session_ksc(device_id: &str, ksc: &[u8]) {
    if device_id.is_empty() || ksc.len() != 16 {
        return;
    }
    SESSION_KSC
        .lock()
        .await
        .insert(device_id.to_ascii_uppercase(), ksc.to_vec());
}

/// Looks up the KSC for a phone device id (`c77c702ec021` or `C77C702EC021`).
pub async fn session_ksc_for_device(device_id: &str) -> Option<Vec<u8>> {
    let wanted = device_id.to_ascii_uppercase();
    SESSION_KSC.lock().await.get(&wanted).cloned()
}

pub static KSC: LazyLock<Result<crate::scp::ksc::Ksc, std::io::Error>> =
    LazyLock::new(crate::scp::ksc::Ksc::load_or_create);

pub fn initialize_ksc() -> std::io::Result<()> {
    KSC.as_ref()
        .map(|_| ())
        .map_err(|error| std::io::Error::new(error.kind(), error.to_string()))
}

/// Historical EPI profile-count value used by the official Windows client.
/// The later `0x0008` service-list frame contains four profiles, including
/// `app:oshare.win.agent`.
pub const SERVICE_PROFILE_COUNT: u8 = crate::panta::EPI_SERVICE_PROFILE_COUNT;

/// Serve an already-bound SCP listener so startup can report bind failures.
pub async fn serve_port_probe(listener: TcpListener, state: crate::state::AppState) {
    let addr = listener.local_addr().expect("bound SCP listener address");
    let port = addr.port();
    info!("probe: listening on {} (SCP control port)", addr);

    let mut connections = JoinSet::new();
    loop {
        let accepted = tokio::select! {
            accepted = listener.accept() => accepted,
            Some(_) = connections.join_next(), if !connections.is_empty() => continue,
        };
        let (sock, peer) = match accepted {
            Ok(v) => v,
            Err(e) => {
                warn!("probe: accept error on {}: {}", addr, e);
                continue;
            }
        };
        let state = state.clone();
        connections.spawn(serve_peer(sock, port, peer, state));
    }
}
/// Serve an already-bound data listener alongside its active SCP sessions.
pub async fn serve_data_listener(listener: TcpListener) {
    serve_data_listener_with_state(listener, crate::state::AppState::new()).await;
}

pub async fn serve_data_listener_with_state(listener: TcpListener, state: crate::state::AppState) {
    let addr = listener.local_addr().expect("bound data listener address");
    let port = addr.port();
    info!("data: listening on {} (data transport)", addr);
    let mut connections = JoinSet::new();
    loop {
        let accepted = tokio::select! {
            accepted = listener.accept() => accepted,
            Some(_) = connections.join_next(), if !connections.is_empty() => continue,
        };
        let (socket, peer) = match accepted {
            Ok(connection) => connection,
            Err(error) => {
                warn!("data[{}]: accept error: {}", port, error);
                continue;
            }
        };
        info!("data[{}]: connection from {}", port, peer);
        let tx = peers()
            .lock()
            .await
            .get(&peer.ip().to_string())
            .filter(|peer| !peer.tx.is_closed())
            .map(|peer| peer.tx.clone());
        if let Some(tx) = tx {
            if tx
                .send(PeerCommand::DataConnection(socket, peer))
                .await
                .is_err()
            {
                warn!("data[{}]: SCP session for {} has closed", port, peer);
            }
        } else {
            // The official daemon serves the data port from its own process, so
            // do not require a live SCP session (reconnects and races land here).
            info!(
                "data[{}]: no SCP session for {}; serving the data channel directly",
                port, peer
            );
            let state_clone = state.clone();
            let lifecycle = state.clone();
            connections.spawn(async move {
                let broadcaster = std::sync::Arc::new(move |evt: &str| {
                    state_clone.broadcast_event(evt);
                });
                crate::panta::serve_with_events(
                    socket,
                    peer,
                    &crate::net_utils::hostname(),
                    SERVICE_PROFILE_COUNT,
                    |device_id| async move { session_ksc_for_device(&device_id).await },
                    Some(broadcaster),
                    Some(lifecycle),
                )
                .await;
            });
        }
    }
}

pub(crate) async fn serve_peer(
    sock: tokio::net::TcpStream,
    port: u16,
    peer: std::net::SocketAddr,
    state: crate::state::AppState,
) {
    // Frontends require explicit, connection-bound approval unless OPLUS_AUTO_ACCEPT=1.
    let auto_accept = std::env::var("OPLUS_AUTO_ACCEPT")
        .map(|value| value == "1")
        .unwrap_or(false);
    serve_peer_with_policy(sock, port, peer, state, auto_accept).await;
}

async fn serve_peer_with_policy(
    mut sock: tokio::net::TcpStream,
    port: u16,
    peer: std::net::SocketAddr,
    state: crate::state::AppState,
    auto_accept: bool,
) {
    info!("probe[{}]: connection from {}", port, peer);
    let (tx, mut rx) = mpsc::channel::<PeerCommand>(8);
    let peer_ip = peer.ip().to_string();
    let mut random_id = [0u8; 16];
    if let Err(error) = getrandom::fill(&mut random_id) {
        warn!(
            "probe[{}]: cannot generate connection identity: {}",
            port, error
        );
        return;
    }
    let connection_id = hex(&random_id);
    peers().lock().await.insert(
        peer_ip.clone(),
        PeerSession {
            connection_id: connection_id.clone(),
            tx,
            approval_deadline: None,
            decision_made: false,
        },
    );
    let mut lease = PeerLease(Some((peer_ip.clone(), connection_id.clone())));

    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut chunk = [0u8; READ_CHUNK_LEN];
    let mut server_handshake: Option<crate::scp::ukey2::ServerHandshake> = None;
    let mut pending_server_init: Option<Vec<u8>> = None;
    let mut cloud_probe: Option<TcpStream> = None;
    let mut accepted = false;
    let mut sync_identity: Option<crate::scp::proto::ScpSyncData> = None;
    let mut session_device_id: Option<String> = None;
    let mut approval_requested = false;
    let handshake_deadline = tokio::time::Instant::now() + APPROVAL_TIMEOUT;
    let mut read_deadline = tokio::time::Instant::now();
    let mut data_connections = JoinSet::new();
    'connection: loop {
        tokio::select! {
            Some(_) = data_connections.join_next(), if !data_connections.is_empty() => {}
            _ = tokio::time::sleep_until(handshake_deadline), if server_handshake.as_ref().and_then(|server| server.secure_channel()).is_none() => {
                warn!("probe[{}]: handshake or approval timed out for {}", port, peer);
                break;
            }
            _ = tokio::time::sleep_until(read_deadline), if !buf.is_empty() => {
                warn!("probe[{}]: incomplete frame timed out for {}", port, peer);
                break;
            }
            Some(command) = rx.recv() => match command {
                PeerCommand::Accept => {
                    accepted = true;
                    clear_awaiting_approval(&peer_ip, &connection_id).await;
                    info!("probe[{}]: accepted connection request from {}", port, peer);
                    if let Some(server_init) = pending_server_init.take() {
                        if let Err(error) = send_ukey2_server_init(port, &mut sock, server_init).await {
                            warn!("probe[{}]: cannot send UKEY2 ServerInit after acceptance: {}", port, error);
                            break;
                        }
                    }
                }
                PeerCommand::Close => {
                    clear_awaiting_approval(&peer_ip, &connection_id).await;
                    info!("probe[{}]: rejected connection from {}", port, peer);
                    break;
                }
                PeerCommand::DataConnection(socket, data_peer) => {
                    info!("probe[{}]: handing data connection from {} to the panta channel", port, data_peer);
                    let state_clone = state.clone();
                    let lifecycle = state.clone();
                    data_connections.spawn(async move {
                        let broadcaster = std::sync::Arc::new(move |evt: &str| {
                            state_clone.broadcast_event(evt);
                        });
                        crate::panta::serve_with_events(
                            socket,
                            data_peer,
                            &crate::net_utils::hostname(),
                            SERVICE_PROFILE_COUNT,
                            |device_id| async move {
                                session_ksc_for_device(&device_id).await
                            },
                            Some(broadcaster),
                            Some(lifecycle),
                        )
                        .await;
                    });
                }
            },
            read_res = sock.read(&mut chunk) => match read_res {
                Ok(0) => {
                    info!("probe[{}]: peer {} closed", port, peer);
                    break;
                }
                Ok(n) => {
                    if buf.len() + n > MAX_BUFFER_LEN {
                        warn!("probe[{}]: receive buffer limit exceeded", port);
                        break;
                    }
                    if buf.is_empty() {
                        read_deadline = tokio::time::Instant::now() + FRAME_TIMEOUT;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    info!(
                        "probe[{}]: read {} bytes hex={}",
                        port,
                        n,
                        hex(&chunk[..n.min(128)])
                    );
                    log_capture(port, &peer, &chunk[..n]);
                    loop {
                        let total = match incoming_len(&buf) {
                            Ok(Some(total)) => total,
                            Ok(None) => break,
                            Err(error) => {
                                warn!("probe[{}]: invalid frame from {}: {}", port, peer, error);
                                break 'connection;
                            }
                        };
                        read_deadline = tokio::time::Instant::now() + FRAME_TIMEOUT;
                        if buf.starts_with(b"QUERY ") {
                            let response = crate::discovery::announce::announce_payload(port);
                            if let Err(error) = sock.write_all(response.as_bytes()).await {
                                warn!("probe[{}]: cannot answer TCP SDP query: {}", port, error);
                                break 'connection;
                            }
                            buf.drain(..total);
                            continue;
                        }
                        let raw = buf[..total].to_vec();
                        buf.drain(..total);
                        if let Err(e) = handle_frame(
                            port,
                            &peer,
                            &connection_id,
                            &raw,
                            &state,
                            &mut sock,
                            &mut server_handshake,
                            &mut pending_server_init,
                            &mut cloud_probe,
                            &mut sync_identity,
                            &mut session_device_id,
                            &mut approval_requested,
                            accepted,
                        )
                        .await
                        {
                            warn!("probe[{}]: frame handling failed: {}", port, e);
                            break 'connection;
                        }
                        if !accepted && auto_accept {
                            if let Some(server_init) = pending_server_init.take() {
                                accepted = true;
                                clear_awaiting_approval(&peer_ip, &connection_id).await;
                                match send_ukey2_server_init(port, &mut sock, server_init).await {
                                    Ok(()) => info!(
                                        "probe[{}]: auto-accepted the connection (automatic approval enabled)",
                                        port
                                    ),
                                    Err(error) => {
                                        warn!("probe[{}]: auto-accept failed: {}", port, error);
                                        break 'connection;
                                    }
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!("probe[{}]: read error from {}: {}", port, peer, e);
                    break;
                }
            }
        }
    }

    let mut active = peers().lock().await;
    let approval_expired = active.get(&peer_ip).is_some_and(|current| {
        current.connection_id == connection_id && current.approval_deadline.is_some()
    });
    if active
        .get(&peer_ip)
        .is_some_and(|current| current.connection_id == connection_id)
    {
        active.remove(&peer_ip);
    }
    drop(active);
    lease.0 = None;
    if approval_expired {
        let event = serde_json::json!({
            "event": "connect_request_expired",
            "data": {
                "device_address": peer.ip().to_string(),
                "connection_id": connection_id,
            }
        });
        state.broadcast_event(&event.to_string());
    }

    if let Some(device_id) = session_device_id {
        // The phone closes this handshake socket right after SyncSecure while
        // the data channel keeps running, so only arm a delayed cleanup. An
        // authenticated data channel cancels it.
        state
            .schedule_device_removal(&device_id, tokio::time::Duration::from_secs(3))
            .await;
    }
    // SCP normally closes immediately after pairing. Its data channels must
    // outlive that handshake, but remain owned by this listener's worker.
    drop(sock);
    drop(rx);
    while data_connections.join_next().await.is_some() {}
}

async fn send_ukey2_server_init(
    port: u16,
    sock: &mut tokio::net::TcpStream,
    server_init: Vec<u8>,
) -> std::io::Result<()> {
    let body = ScpWrapData::new(crate::scp::proto::MSG_UKEY2_INIT, server_init).encode();
    let mut reply = ScpFrame::new(CMD_ACTIVE_CONNECTION, body);
    reply.is_response = true;
    reply.flags = 0;
    sock.write_all(&reply.encode()).await?;
    info!(
        "probe[{}]: sent UKEY2 ServerInit after user acceptance",
        port
    );
    Ok(())
}
async fn send_sync_secure(
    port: u16,
    sock: &mut tokio::net::TcpStream,
    command: u64,
    channel: &crate::scp::ukey2::SecureChannel,
    peer_secure: &ScpSyncSecureData,
) -> std::io::Result<()> {
    let secure = ScpSyncSecureData {
        device_id: crate::net_utils::local_device_id()
            .trim_start_matches("pc_")
            .to_string(),
        ksc_alias: peer_secure.ksc_alias.clone(),
        ksc: peer_secure.ksc.clone(),
    };
    let body = ScpWrapData::new(crate::scp::proto::MSG_SYNC_SECURE, secure.encode()).encode();
    let mut reply = ScpFrame::new(command, channel.encrypt(&body)?);
    reply.is_response = true;
    reply.is_encrypted = true;
    reply.flags = 0;
    sock.write_all(&reply.encode()).await?;
    info!("probe[{}]: sent SCP SyncSecure response", port);
    Ok(())
}

async fn send_ukey2_server_finished(
    port: u16,
    sock: &mut tokio::net::TcpStream,
) -> std::io::Result<()> {
    let body = ScpWrapData::new(crate::scp::proto::MSG_UKEY2_FINISHED, Vec::new()).encode();
    let mut reply = ScpFrame::new(CMD_ACTIVE_CONNECTION, body);
    reply.is_response = true;
    reply.flags = 0;
    sock.write_all(&reply.encode()).await?;
    info!("probe[{}]: sent empty UKEY2 Finished response", port);
    Ok(())
}

async fn send_sync_identity(
    port: u16,
    sock: &mut tokio::net::TcpStream,
    command: u64,
    is_response: bool,
) -> std::io::Result<()> {
    // Android decodes Sync.model_id as exactly three hexadecimal bytes.
    // The QR value "1" is not a valid wire representation; preserve its value as 000001.
    let model_id = "000001".to_string();
    let data_port = std::env::var("OPLUS_DATA_PORT")
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(48999);
    let mut custom_data = Vec::new();
    crate::scp::codec::write_varint_field(&mut custom_data, 1, data_port as u64);
    let ours = ScpSyncData::new(
        crate::net_utils::hostname(),
        custom_data,
        6, // device type: PC
        model_id,
    );
    let body = ScpWrapData::new(crate::scp::proto::MSG_SYNC, ours.encode()).encode();
    let mut reply = ScpFrame::new(command, body);
    reply.is_response = is_response;
    reply.flags = 0;
    sock.write_all(&reply.encode()).await?;
    info!("probe[{}]: sent SCP Sync identity", port);
    Ok(())
}

/// Distinguish incomplete frames from invalid magic before buffering more data.
/// TCP discovery shares this listener and has a separate, much smaller bound.
fn incoming_len(buf: &[u8]) -> std::io::Result<Option<usize>> {
    if buf.is_empty() {
        return Ok(None);
    }
    if buf[0] == b'Q' {
        let prefix = b"QUERY ";
        let prefix_len = buf.len().min(prefix.len());
        if buf[..prefix_len] != prefix[..prefix_len] {
            return Err(std::io::Error::other("invalid TCP discovery prefix"));
        }
        if let Some(end) = buf.windows(4).position(|window| window == b"\r\n\r\n") {
            if end + 4 <= MAX_QUERY_LEN {
                return Ok(Some(end + 4));
            }
        } else if buf.len() < MAX_QUERY_LEN {
            return Ok(None);
        }
        return Err(std::io::Error::other("TCP discovery query too large"));
    }
    if buf[0] != 0x40 || (buf.len() >= 2 && buf[1] & !0x0d != 0x30) {
        return Err(std::io::Error::other("invalid SCP magic"));
    }
    if buf.len() < 4 {
        return Ok(None);
    }
    let declared = u16::from_be_bytes([buf[2], buf[3]]) as usize;
    let total = crate::scp::codec::SCP_HEADER_LEN + declared;
    Ok((buf.len() >= total).then_some(total))
}
fn decode_scp_error(data: &[u8]) -> Option<String> {
    let mut input = data;
    let mut reason = None;
    while !input.is_empty() {
        let key = crate::scp::codec::read_varint(&mut input)?;
        let field = (key >> 3) as u32;
        match (key & 7) as u8 {
            0 => {
                crate::scp::codec::read_varint(&mut input)?;
            }
            2 => {
                let len = crate::scp::codec::read_varint(&mut input)? as usize;
                if len > input.len() {
                    return None;
                }
                let (value, rest) = input.split_at(len);
                input = rest;
                if field == 3 {
                    reason = Some(String::from_utf8_lossy(value).into_owned());
                }
            }
            _ => return None,
        }
    }
    reason
}

async fn handle_frame(
    port: u16,
    peer: &std::net::SocketAddr,
    connection_id: &str,
    raw: &[u8],
    state: &crate::state::AppState,
    sock: &mut tokio::net::TcpStream,
    server_handshake: &mut Option<crate::scp::ukey2::ServerHandshake>,
    pending_server_init: &mut Option<Vec<u8>>,
    cloud_probe: &mut Option<TcpStream>,
    sync_identity: &mut Option<crate::scp::proto::ScpSyncData>,
    session_device_id: &mut Option<String>,
    approval_requested: &mut bool,
    accepted: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let frame = ScpFrame::decode(raw)?;
    let decrypted;
    let payload = if frame.is_encrypted {
        let channel = server_handshake
            .as_ref()
            .and_then(|server| server.secure_channel())
            .ok_or_else(|| std::io::Error::other("encrypted SCP frame before UKEY2 completion"))?;
        decrypted = channel.decrypt(&frame.payload)?;
        decrypted.as_slice()
    } else {
        frame.payload.as_slice()
    };
    if frame.is_business {
        if !frame.is_encrypted {
            return Err(std::io::Error::other("plaintext SCP business frame rejected").into());
        }
        if payload == b"interact hello" || payload == b"reconnect interact hello" {
            let channel = server_handshake
                .as_ref()
                .and_then(|server| server.secure_channel())
                .ok_or_else(|| {
                    std::io::Error::other("business interaction before UKEY2 completion")
                })?;
            let mut reply = ScpFrame::new(
                frame.command().unwrap_or(CMD_ACTIVE_CONNECTION),
                channel.encrypt(b"accept")?,
            );
            reply.is_response = true;
            reply.is_encrypted = true;
            reply.is_business = true;
            reply.flags = 0;
            sock.write_all(&reply.encode()).await?;
            match tokio::time::timeout(
                tokio::time::Duration::from_millis(100),
                TcpStream::connect(std::net::SocketAddr::new(peer.ip(), 10153)),
            )
            .await
            {
                Ok(Ok(probe)) => {
                    *cloud_probe = Some(probe);
                    info!(
                        "probe[{}]: connected reverse WLAN probe to {}:10153",
                        port,
                        peer.ip()
                    );
                }
                Ok(Err(error)) => warn!(
                    "probe[{}]: reverse WLAN probe to {}:10153 failed: {}",
                    port,
                    peer.ip(),
                    error
                ),
                Err(_) => warn!(
                    "probe[{}]: reverse WLAN probe to {}:10153 timed out",
                    port,
                    peer.ip()
                ),
            }
            info!("probe[{}]: accepted SCP business interaction", port);
        } else {
            warn!("probe[{}]: unrecognized SCP business payload", port);
        }
        return Ok(());
    }
    let wrap = ScpWrapData::decode(payload);

    info!(
        "probe[{}]: cmd={:?} resp={} flags={:#06x} payload={} bytes",
        port,
        frame.command(),
        frame.is_response,
        frame.flags,
        frame.payload.len()
    );

    if let Some(wrap) = wrap.as_ref() {
        if wrap.msg_type == MSG_HELLO {
            if let Some(hello) = ScpHelloData::decode(&wrap.data) {
                info!(
                    "probe[{}]: phone Hello seq={} service={} namespace={} channel={}",
                    port, hello.sequence_no, hello.service_name, hello.namespace, hello.channel
                );

                // `handle_hello_packet` requires local == received + 1 and an
                // identical service/namespace/channel, so echo them back.
                let reply_hello = ScpHelloData {
                    sequence_no: hello.sequence_no.wrapping_add(1),
                    service_name: hello.service_name.clone(),
                    namespace: hello.namespace.clone(),
                    channel: hello.channel.clone(),
                };
                let body = ScpWrapData::new(MSG_HELLO, encode_hello(&reply_hello)).encode();
                let mut reply =
                    ScpFrame::new(frame.command().unwrap_or(CMD_ACTIVE_CONNECTION), body);
                reply.is_response = true;
                let bytes = reply.encode();
                sock.write_all(&bytes).await?;
                info!(
                    "probe[{}]: replied Hello seq={}",
                    port, reply_hello.sequence_no
                );

                // The phone names itself in the Sync frame that follows Hello.
                // Mark the peer pending here, but wait for Sync before asking the
                // user so the dialog can show the real name and device type.
                note_awaiting_approval(&peer.ip().to_string(), connection_id).await;
            }
        } else if wrap.msg_type == crate::scp::proto::MSG_SYNC {
            if let Some(sync) = ScpSyncData::decode(&wrap.data) {
                info!(
                    "probe[{}]: phone Sync name={} type={} model_id={}",
                    port, sync.device_name, sync.device_type, sync.model_id
                );
                // The phone names itself here, before SyncSecure. Keep these
                // values for this connection rather than reporting placeholders.
                let device_name = sync.device_name.clone();
                let device_type = if sync.device_type == 10 { "tablet" } else { "phone" };
                *sync_identity = Some(sync);
                send_sync_identity(
                    port,
                    sock,
                    frame.command().unwrap_or(CMD_ACTIVE_CONNECTION),
                    true,
                )
                .await?;

                if !accepted && !*approval_requested {
                    note_awaiting_approval(&peer.ip().to_string(), connection_id).await;
                    let dev = state.get_active_device().await;
                    let payload = serde_json::json!({
                        "event": "onConnectRequested",
                        "data": {
                            "device_id": dev.as_ref().map(|d| d.device_id.clone()).unwrap_or_default(),
                            "device_name": device_name,
                            "device_type": device_type,
                            "connect_type": "wlan",
                            "device_address": peer.ip().to_string(),
                            "connection_id": connection_id,
                        }
                    });
                    let evt = serde_json::json!({
                        "bridge": "connect_callback",
                        "payload": payload,
                    });
                    state.broadcast_event(&evt.to_string());
                    *approval_requested = true;
                }
            }
        } else if wrap.msg_type == crate::scp::proto::MSG_SYNC_SECURE {
            let secure = ScpSyncSecureData::decode(&wrap.data)
                .ok_or_else(|| std::io::Error::other("invalid phone SyncSecure payload"))?;
            info!(
                "probe[{}]: phone SyncSecure device_id={} alias={} key_bytes={}",
                port,
                secure.device_id,
                String::from_utf8_lossy(&secure.ksc_alias),
                secure.ksc.len()
            );
            let channel = server_handshake
                .as_ref()
                .and_then(|server| server.secure_channel())
                .ok_or_else(|| std::io::Error::other("SyncSecure before UKEY2 completion"))?;
            if !frame.is_encrypted {
                return Err(std::io::Error::other("plaintext SyncSecure rejected").into());
            }
            if secure.device_id.is_empty() || secure.ksc.len() != 16 {
                return Err(
                    std::io::Error::other("invalid SyncSecure identity or KSC length").into(),
                );
            }
            remember_session_ksc(&secure.device_id, &secure.ksc).await;
            if session_device_id.is_none() {
                *session_device_id = Some(secure.device_id.clone());
            }
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            // Only the address and session identity are known here. The name,
            // model and firmware come from the phone's Sync frame, and the brand
            // is not carried by this protocol, so absent values stay empty.
            let identity = sync_identity.as_ref();
            let dev_info = crate::state::DeviceInfo {
                device_id: secure.device_id.clone(),
                device_name: identity.map(|s| s.device_name.clone()).unwrap_or_default(),
                device_model: identity.map(|s| s.model_id.clone()).unwrap_or_default(),
                brand: String::new(),
                os_version: String::new(),
                battery: 100,
                is_charging: false,
                connect_type: "wifi".to_string(),
                ip_address: Some(peer.ip().to_string()),
                is_connected: true,
                last_seen: now,
            };
            state.upsert_device(dev_info.clone()).await;

            let connect_event = serde_json::json!({
                "event": "connect_callback",
                "status": 200,
                "data": {
                    "device_id": dev_info.device_id,
                    "device_name": dev_info.device_name,
                    "device_model": dev_info.device_model,
                    "brand": dev_info.brand,
                    "connect_type": dev_info.connect_type,
                    "battery": dev_info.battery,
                    "is_charging": dev_info.is_charging,
                }
            });
            state.broadcast_event(&connect_event.to_string());

            send_sync_secure(
                port,
                sock,
                frame.command().unwrap_or(CMD_ACTIVE_CONNECTION),
                channel,
                &secure,
            )
            .await?;
        } else if wrap.msg_type == crate::scp::proto::MSG_UKEY2_INIT {
            let message_type = crate::scp::ukey2::message_type(&wrap.data)?;
            if message_type == 2 {
                let server =
                    server_handshake.get_or_insert_with(crate::scp::ukey2::ServerHandshake::new);
                let server_init = server.handle_client_init(&wrap.data)?;
                if accepted {
                    send_ukey2_server_init(port, sock, server_init).await?;
                } else {
                    *pending_server_init = Some(server_init);
                    note_awaiting_approval(&peer.ip().to_string(), connection_id).await;
                    if !*approval_requested {
                        let dev = state.get_active_device().await;
                        let device_name = sync_identity
                            .as_ref()
                            .map(|sync| sync.device_name.clone())
                            .filter(|name| !name.trim().is_empty())
                            .or_else(|| dev.as_ref().map(|d| d.device_name.clone()))
                            .unwrap_or_default();
                        let device_type = sync_identity
                            .as_ref()
                            .map(|sync| if sync.device_type == 10 { "tablet" } else { "phone" })
                            .unwrap_or("phone");
                        let payload = serde_json::json!({
                            "event": "onConnectRequested",
                            "data": {
                                "device_id": dev.as_ref().map(|d| d.device_id.clone()).unwrap_or_default(),
                                "device_name": device_name,
                                "device_type": device_type,
                                "connect_type": "wlan",
                                "device_address": peer.ip().to_string(),
                                "connection_id": connection_id,
                            }
                        });
                        let evt = serde_json::json!({
                            "bridge": "connect_callback",
                            "payload": payload,
                        });
                        state.broadcast_event(&evt.to_string());
                        *approval_requested = true;
                    }
                    info!(
                        "probe[{}]: received phone UKEY2 ClientInit; awaiting approval",
                        port
                    );
                }
            } else if message_type == 4 {
                if !accepted {
                    return Err(
                        std::io::Error::other("UKEY2 ClientFinished before approval").into(),
                    );
                }
                let server = server_handshake.as_mut().ok_or_else(|| {
                    std::io::Error::other("UKEY2 ClientFinished without ClientInit")
                })?;
                let shared_secret = server.handle_client_finished(&wrap.data)?;
                info!(
                    "probe[{}]: verified phone UKEY2 ClientFinished; derived {}B ECDH secret",
                    port,
                    shared_secret.len()
                );
            } else {
                return Err(std::io::Error::other("unexpected UKEY2 message type").into());
            }
        } else if wrap.msg_type == crate::scp::proto::MSG_UKEY2_FINISHED {
            if !accepted {
                return Err(std::io::Error::other("UKEY2 ClientFinished before approval").into());
            }
            if crate::scp::ukey2::message_type(&wrap.data)? != 4 {
                return Err(std::io::Error::other("unexpected UKEY2 Finished payload").into());
            }
            let server = server_handshake
                .as_mut()
                .ok_or_else(|| std::io::Error::other("UKEY2 ClientFinished without ClientInit"))?;
            let shared_secret = server.handle_client_finished(&wrap.data)?;
            info!(
                "probe[{}]: verified phone UKEY2 ClientFinished; derived {}B ECDH secret",
                port,
                shared_secret.len()
            );
            send_ukey2_server_finished(port, sock).await?;
        } else if wrap.msg_type == crate::scp::proto::MSG_ERROR {
            let reason = decode_scp_error(&wrap.data)
                .unwrap_or_else(|| "<unparsed error payload>".to_string());
            warn!(
                "probe[{}]: phone SCP error from {}: {} (payload={})",
                port,
                peer,
                reason,
                hex(&wrap.data)
            );
        } else {
            info!(
                "probe[{}]: phone sent msg_type={} ({} bytes)",
                port,
                wrap.msg_type,
                wrap.data.len()
            );
        }
    }
    Ok(())
}

fn encode_hello(hello: &ScpHelloData) -> Vec<u8> {
    use crate::scp::codec::{write_len_delimited, write_varint_field};
    let mut out = Vec::new();
    write_varint_field(&mut out, 1, hello.sequence_no as u64);
    write_len_delimited(&mut out, 2, hello.service_name.as_bytes());
    write_len_delimited(&mut out, 3, hello.namespace.as_bytes());
    if !hello.channel.is_empty() {
        write_len_delimited(&mut out, 4, hello.channel.as_bytes());
    }
    out
}

fn log_capture(port: u16, peer: &std::net::SocketAddr, chunk: &[u8]) {
    let path = if let Ok(dir) = std::env::var("OPLUS_PROTOCOL_LOG_DIR") {
        std::path::PathBuf::from(dir).join("oppo-scp-capture.log")
    } else if cfg!(windows) {
        std::env::temp_dir().join("oppo-scp-capture.log")
    } else {
        std::path::PathBuf::from("/tmp/oppo-scp-capture.log")
    };
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "[{}] {} <- {}", port, peer, hex(chunk));
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect::<Vec<_>>()
        .join("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;
    use tokio::time::{Duration, timeout};

    async fn start_control() -> (TcpStream, tokio::task::JoinHandle<()>, String, String) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (server, peer) = listener.accept().await.unwrap();
        let peer_ip = peer.ip().to_string();
        let previous_id = peer_connection_id(&peer_ip).await;
        let task = tokio::spawn(serve_peer_with_policy(
            server,
            10152,
            peer,
            crate::state::AppState::new(),
            false,
        ));
        let connection_id = timeout(Duration::from_secs(1), async {
            loop {
                if let Some(id) = peer_connection_id(&peer_ip).await {
                    if Some(&id) != previous_id.as_ref() {
                        break id;
                    }
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        (client, task, peer_ip, connection_id)
    }

    fn hello_frame() -> Vec<u8> {
        let hello = ScpHelloData {
            sequence_no: 41,
            service_name: "active_connection".into(),
            namespace: "com.heytap.accessory".into(),
            channel: String::new(),
        };
        ScpFrame::new(
            CMD_ACTIVE_CONNECTION,
            ScpWrapData::new(MSG_HELLO, encode_hello(&hello)).encode(),
        )
        .encode()
    }

    async fn request_approval(client: &mut TcpStream, peer_ip: &str) {
        client.write_all(&hello_frame()).await.unwrap();
        assert!(
            timeout(Duration::from_secs(1), read_frame(client))
                .await
                .unwrap()
                .is_response
        );
        timeout(Duration::from_secs(1), async {
            loop {
                if peers()
                    .lock()
                    .await
                    .get(peer_ip)
                    .is_some_and(|peer| peer.approval_deadline.is_some())
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    async fn expect_closed(client: &mut TcpStream) {
        let mut byte = [0];
        assert_eq!(
            timeout(Duration::from_secs(1), client.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0
        );
    }

    #[test]
    fn frame_parser_preserves_partial_frames_and_bounds_discovery() {
        let bytes = hello_frame();
        for end in 0..bytes.len() {
            assert_eq!(incoming_len(&bytes[..end]).unwrap(), None, "prefix {end}");
        }
        assert_eq!(incoming_len(&bytes).unwrap(), Some(bytes.len()));
        assert!(incoming_len(&[0x41]).is_err());
        assert!(incoming_len(&[0x40, 0x32]).is_err());
        let query = b"QUERY / HTTP/1.1\r\n\r\n";
        for end in 0..query.len() {
            assert_eq!(incoming_len(&query[..end]).unwrap(), None);
        }
        assert_eq!(incoming_len(query).unwrap(), Some(query.len()));
        let mut oversized = b"QUERY ".to_vec();
        oversized.resize(MAX_QUERY_LEN, b'x');
        assert!(incoming_len(&oversized).is_err());
        let maximum = ScpFrame::new(CMD_ACTIVE_CONNECTION, vec![0; u16::MAX as usize]).encode();
        assert_eq!(incoming_len(&maximum[..maximum.len() - 1]).unwrap(), None);
        assert_eq!(incoming_len(&maximum).unwrap(), Some(MAX_FRAME_LEN));
    }

    #[tokio::test]
    async fn bad_magic_closes_socket_but_fragmented_hello_succeeds() {
        let _guard = PEER_TEST_LOCK.lock().await;
        for bytes in [vec![0x41], vec![0x40, 0x32]] {
            let (mut client, task, _, _) = start_control().await;
            client.write_all(&bytes).await.unwrap();
            expect_closed(&mut client).await;
            timeout(Duration::from_secs(1), task)
                .await
                .unwrap()
                .unwrap();
        }
        let (mut client, task, _, _) = start_control().await;
        let frame = hello_frame();
        for fragment in frame.chunks(3) {
            client.write_all(fragment).await.unwrap();
            tokio::task::yield_now().await;
        }
        assert!(
            timeout(Duration::from_secs(1), read_frame(&mut client))
                .await
                .unwrap()
                .is_response
        );
        drop(client);
        timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn plaintext_sync_secure_cannot_inject_or_replace_ksc() {
        let _guard = PEER_TEST_LOCK.lock().await;
        for existing in [None, Some([0x12; 16])] {
            let device_id = "plaintext-injection-test";
            SESSION_KSC
                .lock()
                .await
                .remove(&device_id.to_ascii_uppercase());
            if let Some(key) = existing {
                remember_session_ksc(device_id, &key).await;
            }
            let (mut client, task, _, _) = start_control().await;
            let secure = ScpSyncSecureData {
                device_id: device_id.into(),
                ksc_alias: b"alias".to_vec(),
                ksc: vec![0x99; 16],
            };
            let frame = ScpFrame::new(
                CMD_ACTIVE_CONNECTION,
                ScpWrapData::new(crate::scp::proto::MSG_SYNC_SECURE, secure.encode()).encode(),
            );
            client.write_all(&frame.encode()).await.unwrap();
            expect_closed(&mut client).await;
            timeout(Duration::from_secs(1), task)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                session_ksc_for_device(device_id).await,
                existing.map(|key| key.to_vec())
            );
            SESSION_KSC
                .lock()
                .await
                .remove(&device_id.to_ascii_uppercase());
        }
    }

    #[tokio::test]
    async fn sync_secure_uses_phone_reported_identity() {
        let _guard = PEER_TEST_LOCK.lock().await;
        let (mut phone, init) = crate::scp::ukey2::ClientHandshake::start().unwrap();
        let mut server = crate::scp::ukey2::ServerHandshake::new();
        let server_init = server.handle_client_init(&init).unwrap();
        let finished = phone.handle_server_init(&server_init).unwrap();
        server.handle_client_finished(&finished).unwrap();

        let device_id = "sync-identity-test";
        let sync = ScpSyncData::new("OnePlus 15T", vec![0x01], 10, "PLZ110");
        let sync_body = ScpWrapData::new(crate::scp::proto::MSG_SYNC, sync.encode()).encode();
        let secure = ScpSyncSecureData {
            device_id: device_id.into(),
            ksc_alias: b"alias".to_vec(),
            ksc: vec![0x42; 16],
        };
        let secure_body =
            ScpWrapData::new(crate::scp::proto::MSG_SYNC_SECURE, secure.encode()).encode();
        let secure_ciphertext = server.secure_channel().unwrap().encrypt(&secure_body).unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let _client = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (mut socket, peer) = listener.accept().await.unwrap();
        let mut handshake = Some(server);
        let mut sync_identity = None;
        let mut approval_requested = false;
        let state = crate::state::AppState::new();
        let mut send = async |payload: Vec<u8>, encrypted: bool| {
            let mut frame = ScpFrame::new(CMD_ACTIVE_CONNECTION, payload);
            frame.is_encrypted = encrypted;
            handle_frame(
                10152,
                &peer,
                "identity-test",
                &frame.encode(),
                &state,
                &mut socket,
                &mut handshake,
                &mut None,
                &mut None,
                &mut sync_identity,
                &mut None,
                &mut approval_requested,
                true,
            )
            .await
        };

        send(sync_body, false).await.unwrap();
        send(secure_ciphertext, true).await.unwrap();

        let device = state.get_active_device().await.expect("device registered");
        assert_eq!(device.device_name, "OnePlus 15T");
        assert_eq!(device.device_model, "PLZ110");
        assert!(device.brand.is_empty(), "brand is not part of Sync");
        assert!(device.os_version.is_empty(), "firmware is not part of Sync");
        SESSION_KSC
            .lock()
            .await
            .remove(&device_id.to_ascii_uppercase());
    }

    #[tokio::test]
    async fn sync_secure_requires_authenticated_ciphertext_after_ukey2() {
        let _guard = PEER_TEST_LOCK.lock().await;
        let (mut phone, init) = crate::scp::ukey2::ClientHandshake::start().unwrap();
        let mut server = crate::scp::ukey2::ServerHandshake::new();
        let server_init = server.handle_client_init(&init).unwrap();
        let finished = phone.handle_server_init(&server_init).unwrap();
        server.handle_client_finished(&finished).unwrap();
        let device_id = "authenticated-sync-test";
        remember_session_ksc(device_id, &[0x12; 16]).await;
        let secure = ScpSyncSecureData {
            device_id: device_id.into(),
            ksc_alias: b"alias".to_vec(),
            ksc: vec![0x99; 16],
        };
        let body = ScpWrapData::new(crate::scp::proto::MSG_SYNC_SECURE, secure.encode()).encode();
        let mut ciphertext = server.secure_channel().unwrap().encrypt(&body).unwrap();
        ciphertext[0] ^= 1;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let _client = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (mut socket, peer) = listener.accept().await.unwrap();
        let mut handshake = Some(server);
        let mut approval_requested = false;
        let state = crate::state::AppState::new();
        for (payload, encrypted) in [(body, false), (ciphertext, true)] {
            let mut frame = ScpFrame::new(CMD_ACTIVE_CONNECTION, payload);
            frame.is_encrypted = encrypted;
            assert!(
                handle_frame(
                    10152,
                    &peer,
                    "auth-test",
                    &frame.encode(),
                    &state,
                    &mut socket,
                    &mut handshake,
                    &mut None,
                    &mut None,
                    &mut None,
                    &mut None,
                    &mut approval_requested,
                    true
                )
                .await
                .is_err()
            );
            assert_eq!(
                session_ksc_for_device(device_id).await,
                Some(vec![0x12; 16])
            );
        }
        SESSION_KSC
            .lock()
            .await
            .remove(&device_id.to_ascii_uppercase());
    }

    #[tokio::test]
    async fn old_connection_cleanup_and_decisions_do_not_touch_replacement() {
        let _guard = PEER_TEST_LOCK.lock().await;
        let (mut old_client, old_task, peer_ip, old_id) = start_control().await;
        request_approval(&mut old_client, &peer_ip).await;
        let (mut client, task, _, connection_id) = start_control().await;
        request_approval(&mut client, &peer_ip).await;
        assert_ne!(old_id, connection_id);
        assert!(!accept_peer(&peer_ip, &old_id).await);
        assert!(!reject_peer(&peer_ip, &old_id).await);
        assert!(!accept_peer(&peer_ip, "").await);
        drop(old_client);
        timeout(Duration::from_secs(1), old_task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            peer_connection_id(&peer_ip).await.as_deref(),
            Some(connection_id.as_str())
        );
        assert!(accept_peer(&peer_ip, &connection_id).await);
        assert!(!accept_peer(&peer_ip, &connection_id).await);
        assert!(!reject_peer(&peer_ip, &connection_id).await);
        drop(client);
        timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn expired_approval_cannot_be_accepted_rejected_or_renewed() {
        let _guard = PEER_TEST_LOCK.lock().await;
        let (mut client, task, peer_ip, connection_id) = start_control().await;
        request_approval(&mut client, &peer_ip).await;
        peers()
            .lock()
            .await
            .get_mut(&peer_ip)
            .unwrap()
            .approval_deadline = Some(tokio::time::Instant::now() - Duration::from_secs(1));
        assert!(!accept_peer(&peer_ip, &connection_id).await);
        assert!(!reject_peer(&peer_ip, &connection_id).await);
        note_awaiting_approval(&peer_ip, &connection_id).await;
        assert!(!accept_peer(&peer_ip, &connection_id).await);
        drop(client);
        timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
    }

    async fn read_frame(stream: &mut tokio::net::TcpStream) -> ScpFrame {
        let mut header = [0u8; crate::scp::codec::SCP_HEADER_LEN];
        stream.read_exact(&mut header).await.unwrap();
        let payload_len = u16::from_be_bytes([header[2], header[3]]) as usize;
        let mut bytes = Vec::from(header);
        bytes.resize(crate::scp::codec::SCP_HEADER_LEN + payload_len, 0);
        stream
            .read_exact(&mut bytes[crate::scp::codec::SCP_HEADER_LEN..])
            .await
            .unwrap();
        ScpFrame::decode(&bytes).unwrap()
    }

    #[test]
    fn decodes_phone_scp_error_reason() {
        let reason = b"SYNC vs UKEY2_INIT";
        let mut payload = vec![0x08, 0x2a, 0x10, 0x03, 0x1a, reason.len() as u8];
        payload.extend_from_slice(reason);
        assert_eq!(
            decode_scp_error(&payload).as_deref(),
            Some("SYNC vs UKEY2_INIT")
        );
        assert_eq!(decode_scp_error(&payload[..payload.len() - 1]), None);
    }
    #[tokio::test]
    async fn user_approval_releases_server_init_for_phone_client() {
        let _test_guard = PEER_TEST_LOCK.lock().await;
        use tokio::io::AsyncWriteExt;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
        let (server, peer) = listener.accept().await.unwrap();
        let peer_ip = peer.ip().to_string();
        let session = tokio::spawn(serve_peer_with_policy(
            server,
            10152,
            peer,
            crate::state::AppState::new(),
            false,
        ));
        timeout(Duration::from_secs(1), async {
            while !has_peer(&peer_ip).await {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        let hello = ScpHelloData {
            sequence_no: 41,
            service_name: "active_connection".into(),
            namespace: "com.heytap.accessory".into(),
            channel: String::new(),
        };
        let mut hello_frame = ScpFrame::new(
            CMD_ACTIVE_CONNECTION,
            ScpWrapData::new(MSG_HELLO, encode_hello(&hello)).encode(),
        );
        hello_frame.flags = 0x0002;
        client.write_all(&hello_frame.encode()).await.unwrap();
        assert_eq!(
            ScpWrapData::decode(&read_frame(&mut client).await.payload)
                .unwrap()
                .msg_type,
            MSG_HELLO
        );

        let phone_sync = ScpSyncData::new("OPPO Pad Mini", Vec::new(), 10, "614AB1");
        let sync = ScpFrame::new(
            CMD_ACTIVE_CONNECTION,
            ScpWrapData::new(crate::scp::proto::MSG_SYNC, phone_sync.encode()).encode(),
        );
        client.write_all(&sync.encode()).await.unwrap();
        assert!(read_frame(&mut client).await.is_response);

        let (mut phone_handshake, client_init) =
            crate::scp::ukey2::ClientHandshake::start().unwrap();
        let phone_init = ScpWrapData::new(crate::scp::proto::MSG_UKEY2_INIT, client_init.clone());
        client
            .write_all(&ScpFrame::new(CMD_ACTIVE_CONNECTION, phone_init.encode()).encode())
            .await
            .unwrap();
        assert!(
            timeout(Duration::from_millis(50), read_frame(&mut client))
                .await
                .is_err()
        );

        let connection_id = peer_connection_id(&peer_ip).await.unwrap();
        assert!(accept_peer(&peer_ip, &connection_id).await);
        let server_frame = timeout(Duration::from_secs(1), read_frame(&mut client))
            .await
            .unwrap();
        assert!(server_frame.is_response);
        let server_wrap = ScpWrapData::decode(&server_frame.payload).unwrap();
        assert_eq!(server_wrap.msg_type, crate::scp::proto::MSG_UKEY2_INIT);
        assert_eq!(
            crate::scp::ukey2::message_type(&server_wrap.data).unwrap(),
            3
        );

        let client_finished = phone_handshake
            .handle_server_init(&server_wrap.data)
            .unwrap();
        let finish = ScpWrapData::new(crate::scp::proto::MSG_UKEY2_FINISHED, client_finished);
        client
            .write_all(&ScpFrame::new(CMD_ACTIVE_CONNECTION, finish.encode()).encode())
            .await
            .unwrap();
        let ack = timeout(Duration::from_secs(1), read_frame(&mut client))
            .await
            .unwrap();
        assert!(ack.is_response);
        let ack_wrap = ScpWrapData::decode(&ack.payload).unwrap();
        assert_eq!(ack_wrap.msg_type, crate::scp::proto::MSG_UKEY2_FINISHED);
        assert!(ack_wrap.data.is_empty());

        let channel = crate::scp::ukey2::SecureChannel::derive(
            phone_handshake.shared_secret().unwrap(),
            &client_init,
            &server_wrap.data,
            &[0x42; 16],
        )
        .unwrap();
        let phone_secure = ScpSyncSecureData {
            device_id: "A069C3ECE487".into(),
            ksc_alias: b"614AB1".to_vec(),
            ksc: vec![0x5a; 16],
        };
        let body =
            ScpWrapData::new(crate::scp::proto::MSG_SYNC_SECURE, phone_secure.encode()).encode();
        let mut secure_frame =
            ScpFrame::new(CMD_ACTIVE_CONNECTION, channel.encrypt(&body).unwrap());
        secure_frame.is_encrypted = true;
        assert_eq!(secure_frame.encode()[1], 0x38);
        client.write_all(&secure_frame.encode()).await.unwrap();
        let secure_reply = timeout(Duration::from_secs(1), read_frame(&mut client))
            .await
            .unwrap();
        assert!(secure_reply.is_response && secure_reply.is_encrypted);
        assert_eq!(secure_reply.encode()[1], 0x3c);
        let plaintext = channel.decrypt(&secure_reply.payload).unwrap();
        let secure_wrap = ScpWrapData::decode(&plaintext).unwrap();
        assert_eq!(secure_wrap.msg_type, crate::scp::proto::MSG_SYNC_SECURE);
        let response = ScpSyncSecureData::decode(&secure_wrap.data).unwrap();
        assert_eq!(response.ksc, phone_secure.ksc);
        assert_eq!(response.ksc_alias, phone_secure.ksc_alias);

        for hello in [
            b"interact hello".as_slice(),
            b"reconnect interact hello".as_slice(),
        ] {
            let mut interaction =
                ScpFrame::new(CMD_ACTIVE_CONNECTION, channel.encrypt(hello).unwrap());
            interaction.is_encrypted = true;
            interaction.is_business = true;
            assert_eq!(interaction.encode()[1], 0x39);
            client.write_all(&interaction.encode()).await.unwrap();
            let response = timeout(Duration::from_secs(1), read_frame(&mut client))
                .await
                .unwrap();
            assert!(response.is_response && response.is_encrypted && response.is_business);
            assert_eq!(response.encode()[1], 0x3d);
            assert_eq!(channel.decrypt(&response.payload).unwrap(), b"accept");
        }

        drop(client);
        timeout(Duration::from_secs(1), session)
            .await
            .unwrap()
            .unwrap();
        assert!(!has_peer(&peer_ip).await);
    }

    #[tokio::test]
    async fn data_listener_routes_live_socket_to_scp_session() {
        let _test_guard = PEER_TEST_LOCK.lock().await;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (tx, mut rx) = mpsc::channel(1);
        peers().lock().await.insert(
            address.ip().to_string(),
            PeerSession {
                connection_id: "data-routing-test".to_string(),
                tx,
                approval_deadline: None,
                decision_made: true,
            },
        );
        let task = tokio::spawn(serve_data_listener(listener));

        let mut client = TcpStream::connect(address).await.unwrap();
        client.write_all(b"data").await.unwrap();
        let command = timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        let PeerCommand::DataConnection(mut socket, peer) = command else {
            panic!("expected data connection");
        };
        assert_eq!(peer.ip(), address.ip());
        let mut payload = [0; 4];
        socket.read_exact(&mut payload).await.unwrap();
        assert_eq!(&payload, b"data");
        peers().lock().await.remove(&address.ip().to_string());
        task.abort();
    }

    async fn read_panta_offer(client: &mut TcpStream) {
        let mut length = [0u8; 2];
        client.read_exact(&mut length).await.unwrap();
        let mut offer = vec![0u8; u16::from_be_bytes(length) as usize];
        client.read_exact(&mut offer).await.unwrap();
    }

    #[tokio::test]
    async fn cancelling_data_listener_closes_direct_panta_connection() {
        let _test_guard = PEER_TEST_LOCK.lock().await;
        peers().lock().await.clear();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(serve_data_listener(listener));
        let mut client = TcpStream::connect(address).await.unwrap();
        timeout(Duration::from_secs(1), read_panta_offer(&mut client)).await.unwrap();
        task.abort();
        let _ = task.await;
        let mut byte = [0u8; 1];
        assert_eq!(timeout(Duration::from_secs(1), client.read(&mut byte)).await.unwrap().unwrap(), 0);
    }

    #[tokio::test]
    async fn routed_data_survives_scp_close_but_not_listener_cancellation() {
        let _test_guard = PEER_TEST_LOCK.lock().await;
        peers().lock().await.clear();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(serve_port_probe(listener, crate::state::AppState::new()));
        let control = TcpStream::connect(address).await.unwrap();
        let peer_ip = address.ip().to_string();
        timeout(Duration::from_secs(1), async {
            while !has_peer(&peer_ip).await { tokio::task::yield_now().await; }
        }).await.unwrap();
        let data_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut data = TcpStream::connect(data_listener.local_addr().unwrap()).await.unwrap();
        let (socket, peer) = data_listener.accept().await.unwrap();
        let tx = peers().lock().await.get(&peer_ip).unwrap().tx.clone();
        tx.send(PeerCommand::DataConnection(socket, peer)).await.unwrap();
        timeout(Duration::from_secs(1), read_panta_offer(&mut data)).await.unwrap();
        drop(control);
        timeout(Duration::from_secs(1), async {
            while has_peer(&peer_ip).await { tokio::task::yield_now().await; }
        }).await.unwrap();
        let mut byte = [0u8; 1];
        assert!(timeout(Duration::from_millis(20), data.read(&mut byte)).await.is_err());
        task.abort();
        let _ = task.await;
        assert_eq!(timeout(Duration::from_secs(1), data.read(&mut byte)).await.unwrap().unwrap(), 0);
    }

    #[tokio::test]
    async fn rejecting_live_peer_closes_socket_and_unregisters_session() {
        let _test_guard = PEER_TEST_LOCK.lock().await;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let client = tokio::net::TcpStream::connect(address).await.unwrap();
        let (server, peer) = listener.accept().await.unwrap();
        let peer_ip = peer.ip().to_string();
        let session = tokio::spawn(serve_peer(
            server,
            10152,
            peer,
            crate::state::AppState::new(),
        ));

        timeout(Duration::from_secs(1), async {
            while !has_peer(&peer_ip).await {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        let connection_id = peer_connection_id(&peer_ip).await.unwrap();
        note_awaiting_approval(&peer_ip, &connection_id).await;
        assert!(reject_peer(&peer_ip, &connection_id).await);
        let mut client = client;
        let mut byte = [0u8; 1];
        let read = timeout(Duration::from_secs(1), client.read(&mut byte))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(read, 0, "reject must close the active TCP connection");
        timeout(Duration::from_secs(1), session)
            .await
            .unwrap()
            .unwrap();
        assert!(!has_peer(&peer_ip).await);
    }
}
