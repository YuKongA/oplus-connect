//! GUI-owned cast lifecycle. Only signaling crosses IPC; decoded frames are
//! handed to the GPUI cast window, which sizes itself from the phone's own
//! resolution.
use crate::cast_media::{
    probe_config, spawn_receiver, MediaEvent, MediaReceiver, ReceiverConfig, SignalingMessage,
    VideoFrame,
};
use crate::client::CastRequestRejected;
use crate::client::DaemonClient;
use crate::state::{CastEvent, CastIncoming, CastPhase};
use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::{collections::HashMap, time::Duration};
use tokio::sync::{mpsc, watch};

type ShutdownSignal = (watch::Sender<bool>, watch::Receiver<bool>);
static ACTIVE_SESSION: std::sync::Mutex<Option<ShutdownSignal>> = std::sync::Mutex::new(None);

pub async fn shutdown() {
    let active = ACTIVE_SESSION
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take();
    if let Some((cancel, mut done)) = active {
        let _ = cancel.send(true);
        loop {
            if *done.borrow() {
                break;
            }
            if done.changed().await.is_err() {
                break;
            }
        }
    }
}

pub struct CastSessionHandle {
    cancel: watch::Sender<bool>,
    done: watch::Receiver<bool>,
}

impl CastSessionHandle {
    pub fn stop(&self) {
        let _ = self.cancel.send(true);
    }
    pub fn completion(&self) -> watch::Receiver<bool> {
        self.done.clone()
    }
}

const INPUT_CAPACITY: usize = 128;

/// Bounded UI input transport. Overflow stops the session instead of silently
/// losing a release and leaving a key or touch held on the phone.
#[derive(Clone)]
pub struct InputSender {
    sender: mpsc::Sender<Vec<u8>>,
    cancel: watch::Sender<bool>,
}

impl InputSender {
    pub fn send(
        &self,
        frame: Vec<u8>,
    ) -> std::result::Result<(), mpsc::error::TrySendError<Vec<u8>>> {
        if self.is_closed() {
            return Err(mpsc::error::TrySendError::Closed(frame));
        }
        let result = self.sender.try_send(frame);
        if result.is_err() {
            let _ = self.cancel.send(true);
        }
        result
    }

    pub fn is_closed(&self) -> bool {
        self.sender.is_closed() || *self.cancel.borrow()
    }
}

impl Drop for CastSessionHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

pub fn start(
    device_id: String,
    events: mpsc::Sender<CastEvent>,
    frames: mpsc::Sender<VideoFrame>,
) -> (CastSessionHandle, InputSender) {
    let (cancel, cancel_rx) = watch::channel(false);
    let (done_tx, done) = watch::channel(false);
    let (input_tx, inputs) = mpsc::channel(INPUT_CAPACITY);
    let input = InputSender {
        sender: input_tx,
        cancel: cancel.clone(),
    };
    let config = ReceiverConfig::default();
    *ACTIVE_SESSION
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = Some((cancel.clone(), done.clone()));
    crate::TOKIO_RT.spawn(async move {
        let result = run_session(&device_id, &events, &frames, inputs, config, cancel_rx).await;
        let event = match result {
            Ok(()) => CastEvent::Closed,
            Err(error) => {
                tracing::warn!("cast session for {device_id} failed: {error:#}");
                CastEvent::Error(format!("投屏已停止：{error:#}"))
            }
        };
        let _ = done_tx.send(true);
        let _ = events.send(event).await;
    });
    (CastSessionHandle { cancel, done }, input)
}

async fn cancelled(cancel: &mut watch::Receiver<bool>) {
    if *cancel.borrow() {
        return;
    }
    while cancel.changed().await.is_ok() {
        if *cancel.borrow() {
            return;
        }
    }
}

async fn run_session(
    device_id: &str,
    events: &mpsc::Sender<CastEvent>,
    frames: &mpsc::Sender<VideoFrame>,
    inputs: mpsc::Receiver<Vec<u8>>,
    config: ReceiverConfig,
    mut cancel: watch::Receiver<bool>,
) -> Result<()> {
    events.send(CastEvent::Phase(CastPhase::Probing)).await.ok();
    let probe_settings = config.clone();
    tokio::task::spawn_blocking(move || probe_config(&probe_settings))
        .await
        .context("媒体检查线程失败")??;
    if *cancel.borrow() {
        return Ok(());
    }
    events
        .send(CastEvent::Phase(CastPhase::Starting))
        .await
        .ok();
    let client = DaemonClient::new();
    // Do not cancel a start that may already have reached the daemon. An
    // explicit HTTP rejection proves this worker does not own the session;
    // transport/timeouts remain ambiguous and receive best-effort stop.
    let start_result = client.cast_start(device_id).await;
    let result = match start_result {
        Ok(_) if !*cancel.borrow() => {
            pump_session(
                device_id,
                events,
                frames,
                inputs,
                config,
                &client,
                &mut cancel,
            )
            .await
        }
        Ok(_) => Ok(()),
        Err(error) if error.downcast_ref::<CastRequestRejected>().is_some() => {
            return Err(error.context("投屏启动被拒绝"));
        }
        Err(error) => Err(error.context("启动投屏监听失败")),
    };
    let stop_result = client.cast_stop(device_id).await;
    match (result, stop_result) {
        (Err(error), _) => Err(error),
        (Ok(()), Err(error)) => Err(error.context("本地媒体已关闭，守护进程停止请求失败")),
        (Ok(()), Ok(())) => Ok(()),
    }
}

struct Peer {
    cancel: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
    dimensions: Option<(u32, u32)>,
    input_ready: bool,
    inputs: mpsc::Sender<Vec<u8>>,
}

enum PeerEvent {
    Displaying(u64, u32, u32),
    Frame(u64, VideoFrame),
    InputReady(u64, bool),
    Ended(u64, Result<()>),
}

async fn stop_peer(peer: Peer) {
    let _ = peer.cancel.send(true);
    let _ = peer.task.await;
}

async fn pump_session(
    device_id: &str,
    events: &mpsc::Sender<CastEvent>,
    frames: &mpsc::Sender<VideoFrame>,
    mut inputs: mpsc::Receiver<Vec<u8>>,
    config: ReceiverConfig,
    client: &DaemonClient,
    cancel: &mut watch::Receiver<bool>,
) -> Result<()> {
    let (peer_tx, mut peer_rx) = mpsc::channel(16);
    let mut peers: HashMap<u64, Peer> = HashMap::new();
    let mut input_peer = None;
    let mut video_peer = None;
    let startup_deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut has_frame = false;
    let mut poll = tokio::time::interval(Duration::from_millis(250));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_event = None;
    let result = loop {
        tokio::select! {
            _ = cancelled(cancel) => break Ok(()),
            _ = tokio::time::sleep_until(startup_deadline), if !has_frame => {
                break Err(anyhow!("等待手机连接或画面超时，请在手机确认投屏授权后重试"));
            },
            input = inputs.recv() => {
                let Some(input) = input else { break Ok(()); };
                let target = input_peer.or_else(|| video_peer.filter(|id|
                    peers.get(id).is_some_and(|peer| peer.input_ready)));
                if let Some(id) = target {
                    let Some(peer) = peers.get(&id).filter(|peer| peer.input_ready) else {
                        break Err(anyhow!("键鼠反控通道已关闭"));
                    };
                    if peer.inputs.try_send(input).is_err() {
                        break Err(anyhow!("键鼠反控队列已满或通道已关闭"));
                    }
                    input_peer = Some(id);
                }
            }
            event = peer_rx.recv() => {
                match event {
                    Some(PeerEvent::Displaying(id, width, height)) => {
                        if let Some(peer) = peers.get_mut(&id) { peer.dimensions = Some((width, height)); }
                    }
                    Some(PeerEvent::InputReady(id, ready)) => {
                        if let Some(peer) = peers.get_mut(&id) { peer.input_ready = ready; }
                        if input_peer == Some(id) && !ready {
                            break Err(anyhow!("键鼠反控通道已关闭"));
                        }
                    }
                    Some(PeerEvent::Frame(id, frame)) if peers.contains_key(&id) => {
                        has_frame = true;
                        // A bounded mailbox drops new frames when repaint falls
                        // behind instead of accumulating latency.
                        let displayed = *video_peer.get_or_insert(id);
                        if displayed == id { let _ = frames.try_send(frame); }
                    }
                    Some(PeerEvent::Ended(id, result)) if peers.contains_key(&id) => {
                        // A token can disappear between status and receive. In
                        // that case discard its worker, never replay on a new ID.
                        if result.is_err() {
                            let status = tokio::select! {
                                _ = cancelled(cancel) => break Ok(()),
                                status = client.cast_status(device_id) => status,
                            };
                            if let Ok(status) = status {
                                if status.status.as_ref().is_some_and(|s| !s.connection_ids.contains(&id)) {
                                    if input_peer == Some(id) { break result; }
                                    if video_peer == Some(id) { video_peer = None; }
                                    if let Some(peer) = peers.remove(&id) { stop_peer(peer).await; }
                                    continue;
                                }
                            }
                        }
                        break result;
                    }
                    _ => {}
                }
            }
            _ = poll.tick() => {
                let status = tokio::select! {
                    _ = cancelled(cancel) => break Ok(()),
                    status = client.cast_status(device_id) => status,
                };
                let status = match status { Ok(status) => status, Err(error) => break Err(error) };
                let Some(status) = status.status else { break Err(anyhow!("手机投屏监听已关闭")); };
                if status.state == "closed" { break Err(anyhow!("{}", status.last_error.as_deref().unwrap_or("投屏连接已关闭"))); }
                let stale: Vec<_> = peers.keys().copied().filter(|id| !status.connection_ids.contains(id)).collect();
                if input_peer.is_some_and(|id| stale.contains(&id)) {
                    break Err(anyhow!("键鼠反控连接已关闭"));
                }
                for id in stale { if let Some(peer) = peers.remove(&id) { stop_peer(peer).await; } }
                if video_peer.is_some_and(|id| !peers.contains_key(&id)) { video_peer = None; }
                for id in status.connection_ids {
                    if id == 0 || peers.contains_key(&id) { continue; }
                    let (peer_cancel, peer_cancel_rx) = watch::channel(false);
                    let peer_events = peer_tx.clone();
                    let device_id = device_id.to_owned();
                    let (peer_input_tx, peer_inputs) = mpsc::channel(INPUT_CAPACITY);
                    let peer_config = config.clone();
                    let task = tokio::spawn(async move {
                        let mut terminal_cancel = peer_cancel_rx.clone();
                        let result =
                            run_peer(&device_id, id, &peer_events, peer_inputs, peer_config, peer_cancel_rx)
                                .await;
                        tokio::select! {
                            _ = cancelled(&mut terminal_cancel) => {},
                            _ = peer_events.send(PeerEvent::Ended(id, result)) => {},
                        }
                    });
                    peers.insert(id, Peer { cancel: peer_cancel, task, dimensions: None, input_ready: false, inputs: peer_input_tx });
                }
            }
        }
        let event = if let Some((width, height)) = video_peer
            .and_then(|id| peers.get(&id))
            .and_then(|peer| peer.dimensions)
        {
            CastEvent::Displaying { width, height }
        } else if peers.is_empty() {
            CastEvent::Phase(CastPhase::Waiting)
        } else {
            CastEvent::Phase(CastPhase::Negotiating)
        };
        if last_event.as_ref() != Some(&event) {
            last_event = Some(event.clone());
            tokio::select! {
                _ = cancelled(cancel) => break Ok(()),
                sent = events.send(event) => if sent.is_err() { break Ok(()); },
            }
        }
    };
    // Signal every peer first, then join. No detached media workers may outlive
    // stop, device disconnection, or destruction of the controlling GUI.
    for peer in peers.values() {
        let _ = peer.cancel.send(true);
    }
    for (_, peer) in peers {
        stop_peer(peer).await;
    }
    result
}

async fn run_peer(
    device_id: &str,
    id: u64,
    events: &mpsc::Sender<PeerEvent>,
    inputs: mpsc::Receiver<Vec<u8>>,
    config: ReceiverConfig,
    mut cancel: watch::Receiver<bool>,
) -> Result<()> {
    let h265 = config.h265_enabled;
    let mut receiver = tokio::task::spawn_blocking(move || spawn_receiver(config)).await??;
    let result = tokio::select! {
        biased;
        _ = cancelled(&mut cancel) => Ok(()),
        result = pump_peer(&mut receiver, device_id, id, events, inputs, h265) => result,
    };
    let stopped = tokio::task::spawn_blocking(move || receiver.stop()).await?;
    result.and(stopped)
}

async fn pump_peer(
    receiver: &mut MediaReceiver,
    device_id: &str,
    id: u64,
    events: &mpsc::Sender<PeerEvent>,
    mut inputs: mpsc::Receiver<Vec<u8>>,
    h265: bool,
) -> Result<()> {
    let client = DaemonClient::new();
    let codec = if h265 { "H265" } else { "H264" };
    client
        .cast_send(
            device_id,
            id,
            &json!({"cmd":"send","codec":codec,"type":"pc_codec"}),
        )
        .await?;
    client.cast_configure(device_id, id, 1080, 60).await?;
    let mut last_dimensions = None;
    let mut last_input_ready = false;
    let mut last_stats = std::time::Instant::now();
    let mut last_received = 0u64;
    let mut last_decoded = 0u64;
    let mut last_presented = 0u64;
    loop {
        if let Some(event) = receiver.try_event() {
            match event {
                MediaEvent::Error(error) => return Err(anyhow!(error)),
                MediaEvent::Ended => return Ok(()),
            }
        }
        let status = receiver.status();
        if status.input_ready != last_input_ready {
            events
                .send(PeerEvent::InputReady(id, status.input_ready))
                .await?;
            last_input_ready = status.input_ready;
        }
        if let Some(error) = status.last_error {
            return Err(anyhow!(error));
        }
        if last_stats.elapsed() >= std::time::Duration::from_secs(2) {
            let elapsed = last_stats.elapsed().as_secs_f64();
            tracing::debug!(
                connection_id = id,
                width = status.width,
                height = status.height,
                received = status.frames_received,
                decoded = status.frames_decoded,
                presented = status.frames_presented,
                in_fps = (status.frames_received - last_received) as f64 / elapsed,
                out_fps = (status.frames_decoded - last_decoded) as f64 / elapsed,
                show_fps = (status.frames_presented - last_presented) as f64 / elapsed,
                "cast media frame rate"
            );
            last_stats = std::time::Instant::now();
            last_received = status.frames_received;
            last_decoded = status.frames_decoded;
            last_presented = status.frames_presented;
        }
        if let Some(frame) = receiver.take_frame() {
            let _ = events.try_send(PeerEvent::Frame(id, frame));
        }
        if status.frames_decoded > 0 && status.width > 0 && status.height > 0 {
            let dimensions = (status.width, status.height);
            if last_dimensions != Some(dimensions) {
                events
                    .send(PeerEvent::Displaying(id, status.width, status.height))
                    .await?;
                last_dimensions = Some(dimensions);
            }
        }
        // At most one send and one receive in flight per peer; no unbounded
        // spawn loop or serial multi-peer polling on the UI thread.
        // Inbound first: the peer's SDP answer shares the queue with dozens of
        // ICE candidates, and awaiting a burst of outbound writes before reading
        // kept the answer unapplied for seconds.
        for _ in 0..INPUT_CAPACITY {
            let Ok(frame) = inputs.try_recv() else { break };
            // Once routed, input must not be silently lost (especially releases).
            receiver.send_input(&frame).context("发送键鼠反控失败")?;
        }
        let received = client.cast_receive_batch(device_id, id, 16).await?;
        let empty = received.is_empty();
        for item in received {
            match item {
                CastIncoming::Signaling(payload) => {
                    if let Some(message) = signaling_from_json(&payload)? {
                        receiver.send(message)?;
                    }
                }
                CastIncoming::Control(payload) => {
                    if is_remote_disconnect(&payload) {
                        return Ok(());
                    }
                    tracing::debug!(
                        connection_id = id,
                        command = ?payload.get("cmd"),
                        "observed cast control (no synthesized response)"
                    );
                }
            }
        }
        let mut outgoing = Vec::new();
        for _ in 0..8 {
            let Some(message) = receiver.try_outgoing() else {
                break;
            };
            if let SignalingMessage::Sdp { type_, sdp } = &message {
                tracing::info!(
                    connection_id = id,
                    "cast session sending {type_} ({} bytes)",
                    sdp.len()
                );
            }
            outgoing.push(signaling_to_json(message));
        }
        for payload in outgoing {
            if let Err(error) = client.cast_send(device_id, id, &payload).await {
                tracing::warn!(connection_id = id, "cast signal send failed: {error:#}");
                return Err(error);
            }
        }
        if empty {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

fn signaling_to_json(message: SignalingMessage) -> Value {
    match message {
        SignalingMessage::Sdp { type_, sdp } => json!({"type":type_,"sdp":sdp}),
        // The phone's OdmfSignalingClient throws "No value for id" without this
        // field. `id` is the SDP mid and `label` the m-line index, matching the
        // captured official candidate; the mid falls back to the index only when
        // the local offer has not been parsed yet.
        SignalingMessage::Ice {
            candidate,
            sdp_mline_index,
            mid,
        } => json!({"type":"candidate","candidate":candidate,
            "id":mid.unwrap_or_else(|| sdp_mline_index.to_string()),
            "label":sdp_mline_index}),
    }
}

fn signaling_from_json(payload: &Value) -> Result<Option<SignalingMessage>> {
    match payload.get("type").and_then(Value::as_str) {
        Some(type_ @ ("offer" | "answer")) => Ok(Some(SignalingMessage::Sdp {
            type_: type_.to_owned(),
            sdp: payload["sdp"]
                .as_str()
                .context("SDP 消息缺少 sdp")?
                .to_owned(),
        })),
        Some("candidate") => {
            let label = payload
                .get("label")
                .or_else(|| payload.get("sdpMLineIndex"))
                .context("ICE 消息缺少 m-line")?;
            let index = label
                .as_u64()
                .or_else(|| label.as_str().and_then(|s| s.parse().ok()))
                .context("ICE m-line 无效")?;
            Ok(Some(SignalingMessage::Ice {
                candidate: payload["candidate"]
                    .as_str()
                    .context("ICE 消息缺少 candidate")?
                    .to_owned(),
                sdp_mline_index: u32::try_from(index)?,
                mid: payload.get("id").and_then(Value::as_str).map(str::to_owned),
            }))
        }
        _ => {
            tracing::debug!(message_type = ?payload.get("type"), "observed non-SDP cast signaling");
            Ok(None)
        }
    }
}

fn is_remote_disconnect(payload: &Value) -> bool {
    if payload["COMMAND"].as_str() == Some("REMOTE_DISCONNECT") {
        return true;
    }
    if let Some(msg) = payload.get("msg") {
        if let Some(encoded) = msg.as_str() {
            if let Ok(decoded) = serde_json::from_str::<Value>(encoded) {
                return is_remote_disconnect(&decoded);
            }
        } else if msg["COMMAND"].as_str() == Some("REMOTE_DISCONNECT") {
            return true;
        }
    }
    payload.get("data").is_some_and(is_remote_disconnect)
}

#[cfg(test)]
mod tests {
    use super::{signaling_to_json, SignalingMessage};
    use serde_json::json;

    #[test]
    fn input_overflow_stops_instead_of_losing_a_release() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        let (cancel, cancelled) = tokio::sync::watch::channel(false);
        let input = super::InputSender { sender, cancel };
        input.send(vec![1]).unwrap();
        assert!(input.send(vec![2]).is_err());
        assert!(*cancelled.borrow());
        assert!(input.is_closed());
        assert_eq!(receiver.try_recv().unwrap(), vec![1]);
    }

    #[test]
    fn candidate_signaling_carries_the_mid_the_phone_requires_as_id() {
        let value = signaling_to_json(SignalingMessage::Ice {
            candidate: "candidate:1 1 udp 1 127.0.0.1 1 typ host".into(),
            sdp_mline_index: 1,
            mid: Some("video1".into()),
        });
        assert_eq!(
            value,
            json!({"type":"candidate","candidate":"candidate:1 1 udp 1 127.0.0.1 1 typ host",
                "id":"video1","label":1})
        );
    }

    #[test]
    fn candidate_signaling_falls_back_to_the_index_without_a_mid() {
        let value = signaling_to_json(SignalingMessage::Ice {
            candidate: "candidate:2 1 udp 1 127.0.0.1 2 typ host".into(),
            sdp_mline_index: 0,
            mid: None,
        });
        assert_eq!(value["id"], "0");
        assert_eq!(value["label"], 0);
    }

    #[test]
    fn offer_signaling_uses_the_captured_inner_shape() {
        let value = signaling_to_json(SignalingMessage::Sdp {
            type_: "offer".into(),
            sdp: "v=0".into(),
        });
        assert_eq!(value, json!({"type":"offer","sdp":"v=0"}));
    }
}
