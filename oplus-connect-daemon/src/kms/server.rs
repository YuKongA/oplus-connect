//! Explicit, single-peer KMS transport owned by an authenticated Panta session.
//!
//! Peer IP and plaintext device ID are consistency checks, not cryptographic
//! proof of peer identity. Input is sent only through an authenticated,
//! explicitly requested session; no local/global input capture exists.

use super::{Codec, HEADER_LEN, Metadata};
use crate::kms::input::InputEvent;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use getrandom::SysRng;
use rand_core::{Rng, UnwrapErr};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    io::{self, ErrorKind},
    net::{IpAddr, SocketAddr},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

static NEXT_CONNECTION_ID: AtomicU64 = AtomicU64::new(1);
use tokio::{
    io::{AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
    time::{Instant, MissedTickBehavior, timeout, timeout_at},
};

pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(90);
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_BODY_LEN: u32 = 64 * 1024;

const INPUT_QUEUE_CAPACITY: usize = 128;

fn next_connection_id() -> u64 {
    loop {
        let id = NEXT_CONNECTION_ID.fetch_add(1, Ordering::Relaxed);
        if id != 0 {
            return id;
        }
    }
}

enum InputCommand {
    Event {
        // The controller validates its authenticated TCP connection token
        // before enqueueing. Retaining it on the command also lets the worker
        // reject a command that races peer loss/reconnect.
        generation: u64,
        event: InputEvent,
        reply: oneshot::Sender<io::Result<()>>,
    },
    Release {
        generation: u64,
        reply: oneshot::Sender<io::Result<()>>,
    },
}

#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub device_id: String,
    pub peer_ip: IpAddr,
    pub bind_ip: IpAddr,
    pub width: u32,
    pub height: u32,
    pub direction: u8,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Listening,
    Handshaking,
    Connected,
    Closed,
}

/// Latest inbound pointer payload. Coordinates belong to the peer display;
/// this is observation only and never causes local pointer movement.
#[derive(Debug, Deserialize, Serialize, PartialEq)]
pub struct PeerPointerEvent {
    #[serde(rename = "type")]
    pub kind: u32,
    pub down_t: u64,
    pub event_t: u64,
    pub act: u32,
    pub act_b: u32,
    pub bt_s: u32,
    pub src: u32,
    pub cls: u32,
    pub flg: u32,
    pub left_key: u32,
    pub pt_c: u32,
    pub pts: Vec<PeerPointer>,
}

#[derive(Debug, Deserialize, Serialize, PartialEq)]
pub struct PeerPointer {
    pub pt_id: u32,
    pub tool_t: u32,
    pub x: f64,
    pub y: f64,
}

/// A supported peer-to-host cross-back event, returned once by `/kms/receive`.
#[derive(Clone, Copy, Debug, Serialize, PartialEq)]
pub struct CrossBack {
    pub pct_x: f64,
    pub pct_y: f64,
}

#[derive(Clone, Copy, Debug)]
struct CrossBackSnapshot {
    generation: u64,
    pct_x: f64,
    pct_y: f64,
}

fn parse_pointer_event(body: &[u8]) -> io::Result<PeerPointerEvent> {
    // The frame codec caps the incoming JSON at 64 KiB before allocation.
    // Bound retained points as well; keep only the latest event, never history.
    let event: PeerPointerEvent =
        serde_json::from_slice(body).map_err(|_| invalid("invalid KMS pointer event"))?;
    if event.kind != 1
        || event.pts.is_empty()
        || event.pts.len() > 32
        || event.pt_c as usize != event.pts.len()
        || event
            .pts
            .iter()
            .any(|point| !point.x.is_finite() || !point.y.is_finite())
    {
        return Err(invalid("invalid KMS pointer event"));
    }
    Ok(event)
}

fn cross_back_pct(
    event: &PeerPointerEvent,
    peer_width: u32,
    peer_height: u32,
) -> Option<(f64, f64)> {
    let point = event.pts.first()?;
    if event.pts.len() != 1
        || point.tool_t != 3
        || !point.x.is_finite()
        || !point.y.is_finite()
        || peer_width == 0
        || peer_height == 0
        || point.x < 0.0
        || point.x > f64::from(peer_width)
        || point.y < 0.0
        || point.y > f64::from(peer_height)
    {
        return None;
    }
    if !matches!(event.act, 2 | 7) {
        return None;
    }
    let pct_x = point.x / f64::from(peer_width);
    let pct_y = point.y / f64::from(peer_height);
    (pct_x.is_finite() && pct_y.is_finite()).then_some((pct_x, pct_y))
}

/// Owns the only worker; dropping it aborts that worker and its TCP streams.
/// Intentionally neither Debug nor Serialize: the password stays private.
pub struct Server {
    task: JoinHandle<()>,
    local_addr: SocketAddr,
    password: String,
    test_data: String,
    status: watch::Receiver<SessionStatus>,
    latest_pointer: watch::Sender<Option<Arc<PeerPointerEvent>>>,
    latest_cross_back: watch::Sender<Option<CrossBackSnapshot>>,
    input_tx: mpsc::Sender<InputCommand>,
    shutdown_tx: watch::Sender<bool>,
    connection_id: Arc<AtomicU64>,
    closed: bool,
}

#[derive(Clone, Copy)]
struct Timing {
    handshake: Duration,
    idle: Duration,
    heartbeat: Duration,
    write: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            handshake: HANDSHAKE_TIMEOUT,
            idle: IDLE_TIMEOUT,
            heartbeat: HEARTBEAT_INTERVAL,
            write: WRITE_TIMEOUT,
        }
    }
}

impl Server {
    pub async fn bind(config: ServerConfig) -> io::Result<Self> {
        Self::bind_with_timing(config, Timing::default()).await
    }

    async fn bind_with_timing(config: ServerConfig, timing: Timing) -> io::Result<Self> {
        if config.device_id.is_empty()
            || config.bind_ip.is_unspecified()
            || config.peer_ip.is_unspecified()
            || !(1..=16384).contains(&config.width)
            || !(1..=16384).contains(&config.height)
            || !(1..=4).contains(&config.direction)
        {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "invalid KMS listener configuration",
            ));
        }
        let listener = TcpListener::bind(SocketAddr::new(config.bind_ip, 0)).await?;
        let local_addr = listener.local_addr()?;
        // A power-of-two alphabet avoids modulo bias and provides 96 random bits.
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut random = [0u8; 16];
        UnwrapErr(SysRng).fill_bytes(&mut random);
        let password: String = random
            .iter()
            .map(|byte| ALPHABET[(byte & 63) as usize] as char)
            .collect();
        let codec = Codec::new(&password, MAX_BODY_LEN);
        let mut test_frame = Vec::new();
        codec.encode(
            Metadata {
                event_type: 0,
                sequence: 0,
                timestamp_ms: 0,
            },
            true,
            b"1234abcd",
            &mut test_frame,
        )?;
        let test_data = STANDARD.encode(&test_frame[HEADER_LEN..]);
        let (status_tx, status) = watch::channel(SessionStatus::Listening);
        let (latest_pointer, _) = watch::channel(None);
        let (latest_cross_back, _) = watch::channel(None);
        let (input_tx, input_rx) = mpsc::channel(INPUT_QUEUE_CAPACITY);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let connection_id = Arc::new(AtomicU64::new(0));
        let task = tokio::spawn(accept_loop(
            listener,
            config,
            codec,
            status_tx,
            latest_pointer.clone(),
            latest_cross_back.clone(),
            timing,
            input_rx,
            shutdown_rx,
            connection_id.clone(),
        ));
        Ok(Self {
            task,
            local_addr,
            password,
            test_data,
            status,
            latest_pointer,
            latest_cross_back,
            input_tx,
            shutdown_tx,
            connection_id,
            closed: false,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
    pub fn password(&self) -> &str {
        &self.password
    }
    pub fn test_data(&self) -> &str {
        &self.test_data
    }

    /// A cheap, immutable snapshot of the most recent inbound pointer/edge event
    /// from the current connection. No history is queued, and stale connections
    /// never expose a snapshot.
    pub fn latest_peer_pointer(&self) -> Option<Arc<PeerPointerEvent>> {
        if self.status() != SessionStatus::Connected {
            return None;
        }
        self.latest_pointer.borrow().clone()
    }

    pub fn status(&self) -> SessionStatus {
        if self.closed || self.task.is_finished() {
            SessionStatus::Closed
        } else {
            *self.status.borrow()
        }
    }

    pub fn connection_id(&self) -> Option<u64> {
        if self.status() != SessionStatus::Connected {
            return None;
        }
        match self.connection_id.load(Ordering::Acquire) {
            0 => None,
            id => Some(id),
        }
    }

    /// The worker owns all I/O futures directly. Cancellation therefore cannot
    /// leave a detached reader, writer, accepted stream, or listener behind.
    pub async fn send_input(&self, event: InputEvent) -> io::Result<()> {
        let generation = self.connection_id().ok_or_else(not_connected)?;
        self.send_input_for_generation(generation, event).await
    }

    /// The controller supplies the authenticated TCP connection token; the
    /// worker checks it again immediately before writing.
    pub async fn send_input_for_generation(
        &self,
        generation: u64,
        event: InputEvent,
    ) -> io::Result<()> {
        event.validate()?;
        if self.connection_id() != Some(generation) {
            return Err(not_connected());
        }
        let (reply, result) = oneshot::channel();
        self.input_tx
            .try_send(InputCommand::Event {
                generation,
                event,
                reply,
            })
            .map_err(|_| {
                io::Error::new(ErrorKind::WouldBlock, "KMS input queue is full or closed")
            })?;
        timeout(WRITE_TIMEOUT, result)
            .await
            .map_err(|_| timed_out("KMS input write timeout"))?
            .map_err(|_| closed())?
    }

    pub async fn release_input(&self) -> io::Result<()> {
        let generation = self.connection_id().ok_or_else(not_connected)?;
        self.release_input_for_generation(generation).await
    }

    pub async fn release_input_for_generation(&self, generation: u64) -> io::Result<()> {
        if self.connection_id() != Some(generation) {
            return Err(not_connected());
        }
        let (reply, result) = oneshot::channel();
        self.input_tx
            .try_send(InputCommand::Release { generation, reply })
            .map_err(|_| {
                io::Error::new(ErrorKind::WouldBlock, "KMS input queue is full or closed")
            })?;
        timeout(WRITE_TIMEOUT, result)
            .await
            .map_err(|_| timed_out("KMS input release timeout"))?
            .map_err(|_| closed())?
    }
    /// Nonblocking snapshot consumption, scoped to the authenticated TCP token.
    pub async fn receive_cross_back_for_generation(
        &self,
        generation: u64,
    ) -> io::Result<Option<CrossBack>> {
        if self.connection_id() != Some(generation) {
            return Err(not_connected());
        }
        let snapshot = self.latest_cross_back.send_replace(None);
        let result = snapshot
            .filter(|snapshot| snapshot.generation == generation)
            .map(|snapshot| CrossBack {
                pct_x: snapshot.pct_x,
                pct_y: snapshot.pct_y,
            });
        Ok(result)
    }
    pub async fn shutdown(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        let _ = self.shutdown_tx.send(true);
        if timeout(WRITE_TIMEOUT, &mut self.task).await.is_err() {
            self.task.abort();
            let _ = (&mut self.task).await;
        }
        self.latest_pointer.send_replace(None);
        self.latest_cross_back.send_replace(None);
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn accept_loop(
    listener: TcpListener,
    config: ServerConfig,
    codec: Codec,
    status: watch::Sender<SessionStatus>,
    latest_pointer: watch::Sender<Option<Arc<PeerPointerEvent>>>,
    latest_cross_back: watch::Sender<Option<CrossBackSnapshot>>,
    timing: Timing,
    mut input_rx: mpsc::Receiver<InputCommand>,
    mut shutdown_rx: watch::Receiver<bool>,
    connection_id: Arc<AtomicU64>,
) {
    loop {
        if *shutdown_rx.borrow() {
            break;
        }
        let (stream, peer) = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok(value) => value,
                Err(_) => break,
            },
            command = input_rx.recv() => {
                if let Some(command) = command {
                    reject_command(command, not_connected());
                } else {
                    break;
                }
                continue;
            }
            changed = shutdown_rx.changed() => {
                if changed.is_err() || *shutdown_rx.borrow() { break; }
                continue;
            }
        };
        if canonical_ip(peer.ip()) != canonical_ip(config.peer_ip) {
            drop(stream);
            continue;
        }
        status.send_replace(SessionStatus::Handshaking);
        {
            // Keep the entire session future alive while rejecting extra peers.
            // There are no spawned connection workers and no unbounded queues.
            let connection = handle_connection(
                stream,
                &config,
                &codec,
                &status,
                &latest_pointer,
                &latest_cross_back,
                timing,
                &mut input_rx,
                &mut shutdown_rx,
                &connection_id,
            );
            tokio::pin!(connection);
            let outcome = loop {
                tokio::select! {
                    result = &mut connection => break result,
                    accepted = listener.accept() => match accepted {
                        Ok((extra, _)) => drop(extra),
                        Err(_) => return,
                    },
                }
            };
            if let Err(error) = outcome {
                if error.kind() == io::ErrorKind::UnexpectedEof {
                    tracing::debug!(peer = %peer, "KMS peer disconnected");
                } else {
                    tracing::warn!(peer = %peer, "KMS peer session ended: {error}");
                }
            }
        }
        connection_id.store(0, Ordering::Release);
        drain_pending_commands(&mut input_rx);
        latest_pointer.send_replace(None);
        latest_cross_back.send_replace(None);
        if *shutdown_rx.borrow() {
            break;
        }
        status.send_replace(SessionStatus::Listening);
    }
    status.send_replace(SessionStatus::Closed);
}

fn canonical_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(ip) => ip
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(ip)),
        ip => ip,
    }
}

#[derive(Deserialize)]
struct Identity<'a> {
    #[serde(rename = "deviceid", borrow)]
    device_id: &'a str,
    #[serde(rename = "supportEncrypt")]
    support_encrypt: u8,
}

#[derive(Deserialize)]
struct Resolution<'a> {
    #[serde(borrow)]
    sync_t: &'a str,
    width: u32,
    height: u32,
}

#[derive(Deserialize, Serialize)]
struct Heartbeat {
    heartbeat_id: u64,
}

fn parse_resolution(body: &[u8]) -> io::Result<(u32, u32)> {
    let resolution: Resolution<'_> =
        serde_json::from_slice(body).map_err(|_| invalid("invalid KMS resolution"))?;
    if resolution.sync_t != "resolution" || resolution.width == 0 || resolution.height == 0 {
        return Err(invalid("invalid KMS resolution"));
    }
    Ok((resolution.width, resolution.height))
}

async fn handle_connection(
    mut stream: TcpStream,
    config: &ServerConfig,
    codec: &Codec,
    status: &watch::Sender<SessionStatus>,
    latest_pointer: &watch::Sender<Option<Arc<PeerPointerEvent>>>,
    latest_cross_back: &watch::Sender<Option<CrossBackSnapshot>>,
    timing: Timing,
    input_rx: &mut mpsc::Receiver<InputCommand>,
    shutdown_rx: &mut watch::Receiver<bool>,
    connection_id: &Arc<AtomicU64>,
) -> io::Result<()> {
    let mut buffer = Vec::new();
    let deadline = Instant::now() + timing.handshake;
    // Timeout cancellation always closes this stream; it is never reused after
    // a partially consumed handshake frame.
    // The peer resolution, not the host arrangement, defines normalized return coordinates.
    let (mut peer_width, mut peer_height);
    {
        let handshake = timeout_at(deadline, async {
            let first = codec
                .read(&mut stream, &mut buffer)
                .await?
                .ok_or_else(closed)?;
            if first.header.metadata.event_type != 6 {
                return Err(invalid("invalid KMS identity frame"));
            }
            let identity: Identity<'_> =
                serde_json::from_slice(first.body).map_err(|_| invalid("invalid KMS identity"))?;
            if !identity.device_id.eq_ignore_ascii_case(&config.device_id)
                || identity.support_encrypt != 1
            {
                return Err(invalid("KMS identity or encryption capability rejected"));
            }
            let second = codec
                .read(&mut stream, &mut buffer)
                .await?
                .ok_or_else(closed)?;
            if second.header.metadata.event_type != 1 {
                return Err(invalid("invalid KMS resolution frame"));
            }
            parse_resolution(second.body)
        });
        tokio::pin!(handshake);
        let handshake_result = tokio::select! {
            result = &mut handshake => result,
            changed = shutdown_rx.changed() => {
                if changed.is_err() || *shutdown_rx.borrow() { return Ok(()); }
                handshake.await
            }
        };
        (peer_width, peer_height) =
            handshake_result.map_err(|_| timed_out("KMS handshake timeout"))??;
    }

    let (mut reader, mut writer) = stream.split();
    let mut output = Outgoing {
        body: Vec::new(),
        wire: Vec::new(),
        sequence: 1,
    };
    send_json(
        codec,
        &mut writer,
        &mut output,
        1,
        &json!({"count": 1, "direction": config.direction, "sync_t": "arrange"}),
        timing.write,
    )
    .await?;
    send_json(
        codec,
        &mut writer,
        &mut output,
        1,
        &json!({"height": config.height, "sync_t": "resolution", "width": config.width}),
        timing.write,
    )
    .await?;
    let generation = next_connection_id();
    connection_id.store(generation, Ordering::Release);
    latest_cross_back.send_replace(None);
    status.send_replace(SessionStatus::Connected);

    let mut heartbeat =
        tokio::time::interval_at(Instant::now() + timing.heartbeat, timing.heartbeat);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut heartbeat_id = 1u64;
    let mut input_state = InputState::default();
    loop {
        let idle_deadline = Instant::now() + timing.idle;
        let frame = {
            // This read survives every heartbeat tick, including partial headers
            // and bodies. Separate stream halves let ticks write without ever
            // cancelling or recreating the pending read.
            let read = codec.read(&mut reader, &mut buffer);
            tokio::pin!(read);
            loop {
                tokio::select! {
                    result = &mut read => break result?.ok_or_else(closed)?,
                    _ = tokio::time::sleep_until(idle_deadline) => return Err(timed_out("KMS idle timeout")),
                    command = input_rx.recv() => {
                        let Some(command) = command else { return Err(closed()); };
                        match command {
                            InputCommand::Event { generation, event, reply } => {
                                if reply.is_closed() { continue; }
                                if connection_id.load(Ordering::Acquire) != generation {
                                    let _ = reply.send(Err(not_connected()));
                                    continue;
                                }
                                if matches!(&event, InputEvent::Enter { .. }) {
                                    latest_cross_back.send_replace(None);
                                }
                                match send_input_event(codec, &mut writer, &mut output, &event, timing.write).await {
                                    Ok(()) => {
                                        input_state.record(&event);
                                        let _ = reply.send(Ok(()));
                                    }
                                    Err(error) => {
                                        let kind = error.kind();
                                        let message = error.to_string();
                                        let _ = reply.send(Err(error));
                                        return Err(io::Error::new(kind, message));
                                    }
                                }
                            }
                            InputCommand::Release { generation, reply } => {
                                if reply.is_closed() { continue; }
                                if connection_id.load(Ordering::Acquire) != generation {
                                    let _ = reply.send(Err(not_connected()));
                                    continue;
                                }
                                match release_all(codec, &mut writer, &mut output, &mut input_state, timing.write).await {
                                    Ok(()) => { let _ = reply.send(Ok(())); }
                                    Err(error) => {
                                        let kind = error.kind();
                                        let message = error.to_string();
                                        let _ = reply.send(Err(error));
                                        return Err(io::Error::new(kind, message));
                                    }
                                }
                            }
                        }
                    }
                    changed = shutdown_rx.changed() => {
                        if changed.is_err() || *shutdown_rx.borrow() {
                            let _ = release_all(codec, &mut writer, &mut output, &mut input_state, timing.write).await;
                            return Ok(());
                        }
                    }
                    _ = heartbeat.tick() => {
                        send_json(codec, &mut writer, &mut output, 4,
                            &Heartbeat { heartbeat_id }, timing.write).await?;
                        heartbeat_id = heartbeat_id.checked_add(1).ok_or_else(|| invalid("KMS heartbeat id exhausted"))?;
                    },
                }
            }
        };
        // Official peers use plaintext controls even with supportEncrypt=1.
        // Heartbeats also leave this server plaintext, sequence zero, as captured;
        // arrangement and resolution always retain authenticated encryption.
        tracing::trace!(
            frame_type = frame.header.metadata.event_type,
            body_len = frame.body.len(),
            "KMS peer frame"
        );
        match frame.header.metadata.event_type {
            4 => {
                let echo: Heartbeat = serde_json::from_slice(frame.body)
                    .map_err(|_| invalid("invalid KMS heartbeat"))?;
                send_json(codec, &mut writer, &mut output, 5, &echo, timing.write).await?;
            }
            5 => {
                let _: Heartbeat = serde_json::from_slice(frame.body)
                    .map_err(|_| invalid("invalid KMS heartbeat echo"))?;
                tracing::debug!("KMS peer heartbeat echo");
            }
            100 if frame.body == b"alive" => {
                tracing::debug!("KMS peer alive");
            }
            1 => {
                let (width, height) = parse_resolution(frame.body)?;
                peer_width = width;
                peer_height = height;
            }
            2 => {
                let event = parse_pointer_event(frame.body)?;
                if let Some((pct_x, pct_y)) = cross_back_pct(&event, peer_width, peer_height) {
                    tracing::debug!(generation, pct_x, pct_y, "KMS peer cross-back stored");
                    latest_cross_back.send_replace(Some(CrossBackSnapshot {
                        generation,
                        pct_x,
                        pct_y,
                    }));
                }
                latest_pointer.send_replace(Some(Arc::new(event)));
            }
            other => {
                // Frames are length-delimited and authenticated, so an unknown
                // control type can be skipped without desynchronising the wire.
                // Ending the session here turned a benign extension into a drop.
                tracing::debug!(
                    frame_type = other,
                    body_len = frame.body.len(),
                    "ignoring unsupported KMS control frame"
                );
            }
        }
    }
}

struct InputState {
    keys: [bool; 256],
    key_modifiers: [u32; 256],
    buttons: [bool; 16],
}

impl Default for InputState {
    fn default() -> Self {
        Self {
            keys: [false; 256],
            key_modifiers: [0; 256],
            buttons: [false; 16],
        }
    }
}
impl InputState {
    fn record(&mut self, event: &InputEvent) {
        match event {
            InputEvent::Key {
                key_code,
                modifier,
                pressed,
            } => {
                self.keys[*key_code as usize] = *pressed;
                self.key_modifiers[*key_code as usize] = if *pressed { *modifier } else { 0 };
            }
            InputEvent::Button {
                button_type,
                pressed,
            } => {
                if let Some(slot) = self.buttons.get_mut(*button_type as usize) {
                    *slot = *pressed;
                }
            }
            _ => {}
        }
    }
}

async fn send_input_event<W: AsyncWrite + Unpin>(
    codec: &Codec,
    writer: &mut W,
    output: &mut Outgoing,
    event: &InputEvent,
    write_timeout: Duration,
) -> io::Result<()> {
    output.body.clear();
    event.encode_body(&mut output.body)?;
    let (body, wire, sequence) = (&output.body, &mut output.wire, &mut output.sequence);
    send_encoded(
        codec,
        writer,
        body,
        wire,
        sequence,
        event.event_type(),
        write_timeout,
    )
    .await
}

async fn release_all<W: AsyncWrite + Unpin>(
    codec: &Codec,
    writer: &mut W,
    output: &mut Outgoing,
    state: &mut InputState,
    write_timeout: Duration,
) -> io::Result<()> {
    for code in 1..=u8::MAX {
        if state.keys[code as usize] {
            let event = InputEvent::Key {
                key_code: code,
                modifier: state.key_modifiers[code as usize],
                pressed: false,
            };
            send_input_event(codec, writer, output, &event, write_timeout).await?;
            state.keys[code as usize] = false;
            state.key_modifiers[code as usize] = 0;
        }
    }
    const SUPPORTED_BUTTONS: [u8; 5] = [0, 1, 2, 6, 8];
    for &button in &SUPPORTED_BUTTONS {
        if state.buttons[button as usize] {
            let event = InputEvent::Button {
                button_type: button,
                pressed: false,
            };
            send_input_event(codec, writer, output, &event, write_timeout).await?;
            state.buttons[button as usize] = false;
        }
    }
    Ok(())
}

fn reject_command(command: InputCommand, error: io::Error) {
    match command {
        InputCommand::Event { reply, .. } | InputCommand::Release { reply, .. } => {
            let _ = reply.send(Err(error));
        }
    }
}

fn not_connected() -> io::Error {
    io::Error::new(ErrorKind::NotConnected, "KMS session is not connected")
}

fn drain_pending_commands(input_rx: &mut mpsc::Receiver<InputCommand>) {
    while let Ok(command) = input_rx.try_recv() {
        reject_command(command, not_connected());
    }
}

struct Outgoing {
    body: Vec<u8>,
    wire: Vec<u8>,
    sequence: u64,
}

async fn send_json<W: AsyncWrite + Unpin, T: Serialize>(
    codec: &Codec,
    writer: &mut W,
    output: &mut Outgoing,
    event_type: u8,
    body: &T,
    write_timeout: Duration,
) -> io::Result<()> {
    output.body.clear();
    serde_json::to_writer(&mut output.body, body)
        .map_err(|_| invalid("KMS JSON encoding failed"))?;
    let timestamp_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let heartbeat = matches!(event_type, 4 | 5);
    codec.encode(
        Metadata {
            event_type,
            sequence: if heartbeat { 0 } else { output.sequence },
            timestamp_ms,
        },
        !heartbeat,
        &output.body,
        &mut output.wire,
    )?;
    timeout(write_timeout, writer.write_all(&output.wire))
        .await
        .map_err(|_| timed_out("KMS write timeout"))??;
    if !heartbeat {
        output.sequence += 1;
    }
    Ok(())
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, message)
}

async fn send_encoded<W: AsyncWrite + Unpin>(
    codec: &Codec,
    writer: &mut W,
    body: &[u8],
    wire: &mut Vec<u8>,
    sequence: &mut u64,
    event_type: u8,
    write_timeout: Duration,
) -> io::Result<()> {
    let timestamp_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    codec.encode(
        Metadata {
            event_type,
            sequence: *sequence,
            timestamp_ms,
        },
        true,
        body,
        wire,
    )?;
    timeout(write_timeout, writer.write_all(wire))
        .await
        .map_err(|_| timed_out("KMS write timeout"))??;
    *sequence = (*sequence)
        .checked_add(1)
        .ok_or_else(|| invalid("KMS sequence exhausted"))?;
    Ok(())
}
fn timed_out(message: &'static str) -> io::Error {
    io::Error::new(ErrorKind::TimedOut, message)
}
fn closed() -> io::Error {
    io::Error::new(ErrorKind::UnexpectedEof, "KMS peer closed")
}

#[cfg(test)]
#[path = "server_tests.rs"]
mod tests;
