//! Lifecycle management for tablet secondary display session.
use crate::client::DaemonClient;
use crate::expand_cast_media::{self, SenderConfig, SignalingMessage};
use anyhow::{anyhow, Context, Result};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::sync::{mpsc, watch};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ExpandCastPhase {
    #[default]
    Idle,
    Preparing,
    WaitingForTablet,
    Streaming,
    Stopping,
    Failed,
}

#[derive(Clone, Debug, Default)]
pub struct ExpandCastViewState {
    pub device_id: String,
    pub generation: u64,
    pub phase: ExpandCastPhase,
    pub detail: String,
    pub width: u32,
    pub height: u32,
}

impl ExpandCastViewState {
    pub fn active(&self) -> bool {
        !matches!(self.phase, ExpandCastPhase::Idle | ExpandCastPhase::Failed)
    }
}

pub enum ExpandCastEvent {
    Phase(ExpandCastPhase),
    Resolution(u32, u32),
    Closed,
    Error(String),
}

pub struct ExpandCastSessionHandle {
    cancel_tx: watch::Sender<bool>,
    done_rx: watch::Receiver<bool>,
    stopped: Arc<AtomicBool>,
}

impl ExpandCastSessionHandle {
    pub fn stop(&self) {
        if !self.stopped.swap(true, Ordering::SeqCst) {
            let _ = self.cancel_tx.send(true);
        }
    }
}

impl Drop for ExpandCastSessionHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

static ACTIVE_SESSION: Mutex<Option<(watch::Sender<bool>, watch::Receiver<bool>)>> =
    Mutex::new(None);

pub async fn shutdown() {
    let handle = {
        let mut guard = ACTIVE_SESSION.lock().unwrap_or_else(|e| e.into_inner());
        guard.take()
    };
    if let Some((cancel, mut done)) = handle {
        let _ = cancel.send(true);
        let _ = tokio::time::timeout(Duration::from_millis(1500), async {
            while !*done.borrow_and_update() {
                if done.changed().await.is_err() {
                    break;
                }
            }
        })
        .await;
    }
}

pub fn start(device_id: String, events: mpsc::Sender<ExpandCastEvent>) -> ExpandCastSessionHandle {
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let (done_tx, done_rx) = watch::channel(false);
    let stopped = Arc::new(AtomicBool::new(false));

    {
        let mut guard = ACTIVE_SESSION.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((prev_cancel, _)) = guard.take() {
            let _ = prev_cancel.send(true);
        }
        *guard = Some((cancel_tx.clone(), done_rx.clone()));
    }

    let dev_id = device_id.clone();
    let events_clone = events.clone();
    let cancel_worker = cancel_rx.clone();

    crate::TOKIO_RT.spawn(async move {
        let res = run_session(&dev_id, &events_clone, cancel_worker).await;
        if let Err(err) = res {
            tracing::error!("ExpandCast session failed: {err:#}");
            let _ = events_clone.try_send(ExpandCastEvent::Error(format!("{err:#}")));
        } else {
            let _ = events_clone.try_send(ExpandCastEvent::Closed);
        }
        let _ = done_tx.send(true);
        let mut guard = ACTIVE_SESSION.lock().unwrap_or_else(|e| e.into_inner());
        *guard = None;
    });

    ExpandCastSessionHandle {
        cancel_tx,
        done_rx,
        stopped,
    }
}

async fn run_session(
    device_id: &str,
    events: &mpsc::Sender<ExpandCastEvent>,
    mut cancel: watch::Receiver<bool>,
) -> Result<()> {
    let client = DaemonClient::new();
    let _ = events.try_send(ExpandCastEvent::Phase(ExpandCastPhase::Preparing));

    // 1. Tell daemon to initiate ExpandCast
    let _start_status = client
        .expand_cast_start(device_id)
        .await
        .context("启动平板屏幕共享服务失败")?;

    let _ = events.try_send(ExpandCastEvent::Phase(ExpandCastPhase::WaitingForTablet));

    // 2. Poll status until tablet connects and reports dimensions
    let mut poll_interval = tokio::time::interval(Duration::from_millis(250));
    let mut connection_id = None;
    let mut tablet_width = 2016;
    let mut tablet_height = 1344;

    let wait_res = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if *cancel.borrow() {
                return Ok::<(), anyhow::Error>(());
            }
            poll_interval.tick().await;
            // The daemon answers 409 until the tablet's data bus is ready, which
            // is expected during handshake: keep polling instead of aborting.
            match client.expand_cast_status(device_id).await {
                Ok(status) => {
                    if let (Some(w), Some(h)) = (status.tablet_width, status.tablet_height) {
                        tablet_width = w;
                        tablet_height = h;
                    }
                    if let Some(id) = status.connection_id {
                        connection_id = Some(id);
                        break;
                    }
                }
                Err(error) => {
                    tracing::warn!("ExpandCast status not ready yet: {error:#}");
                }
            }
        }
        Ok(())
    })
    .await;

    match wait_res {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            let _ = client.expand_cast_stop(device_id).await;
            return Err(error).context("等待平板连接屏幕共享失败");
        }
        Err(_) => {
            let _ = client.expand_cast_stop(device_id).await;
            return Err(anyhow!("等待平板连接屏幕共享超时"));
        }
    }

    let conn_id = match connection_id {
        Some(id) => id,
        None => {
            let _ = client.expand_cast_stop(device_id).await;
            return Ok(());
        }
    };

    let _ = events.try_send(ExpandCastEvent::Resolution(tablet_width, tablet_height));
    let _ = events.try_send(ExpandCastEvent::Phase(ExpandCastPhase::Streaming));

    // 3. Acquire the portal screencast session (KDE prompts for approval).
    let screencast = match tokio::time::timeout(Duration::from_secs(30), try_acquire_screencast()).await {
        Ok(Ok(session)) => {
            tracing::info!(
                "Acquired portal screencast: node={} fd={}",
                session.node(),
                session.fd()
            );
            Some(session)
        }
        Ok(Err(err)) => {
            tracing::warn!("Could not acquire portal screencast: {err:#}");
            None
        }
        Err(_) => {
            tracing::warn!("Portal screencast prompt timed out");
            None
        }
    };
    // The guard keeps the compositor-side session (and thus the PipeWire node)
    // alive for as long as this function streams.
    let (pipewire_fd, pipewire_node) = match &screencast {
        Some(session) => (Some(session.fd()), Some(session.node())),
        None => (None, None),
    };

    // Keep the stream at the capture's native pixel count: upscaling to the panel
    // costs GPU time and adds encoder latency for no extra detail. The output
    // takes the panel's aspect ratio, so the 16:9 desktop is letterboxed inside
    // it - the compositor chooses the virtual output size, not us.
    let (render_width, render_height, out_width, out_height) =
        match screencast.as_ref().and_then(|s| s.size()) {
            Some((src_w, src_h)) if src_w > 0 && src_h > 0 => {
                let render_w = src_w & !1;
                let render_h = src_h & !1;
                let out_h = ((render_w as u64 * tablet_height as u64 / tablet_width as u64) as u32) & !1;
                (render_w, render_h, render_w, out_h.max(2))
            }
            _ => (tablet_width, tablet_height, tablet_width, tablet_height),
        };
    tracing::info!(
        "ExpandCast: encoding {out_width}x{out_height} from {render_width}x{render_height} content"
    );

    // Only a compositor-created virtual output can be asked for a different size;
    // a mirrored monitor is fixed to its own resolution. The request is opt-in
    // (see `OPLUS_EXPAND_CAST_PIN_CAPTURE`) because current KWin rejects it.
    let (capture_width, capture_height) = match screencast.as_ref() {
        Some(session) if session.is_virtual() => (Some(tablet_width), Some(tablet_height)),
        _ => (None, None),
    };

    // 4. Start GStreamer WebRTC sender. The tablet advertises a 30 fps cap and a
    // 2500-6500 kbps video band, so encode inside that envelope instead of
    // overshooting and letting Wi-Fi queue frames.
    let sender_config = SenderConfig {
        width: out_width,
        height: out_height,
        render_width,
        render_height,
        capture_width,
        capture_height,
        fps: 30,
        bitrate_kbps: 6000,
        pipewire_fd,
        pipewire_node,
    };

    let mut media = expand_cast_media::spawn_sender(sender_config)
        .context("启动屏幕共享推流管道失败")?;

    // Send SUB_MODE and DISPLAY_RESOLUTION to tablet
    let sub_mode_cmd = serde_json::json!({
        "cmd": "customBroadcast",
        "data": {
            "COMMAND": "SUB_MODE",
            "subMode": 1
        }
    });
    let _ = client.expand_cast_send(device_id, conn_id, &sub_mode_cmd).await;

    let res_cmd = serde_json::json!({
        "cmd": "customBroadcast",
        "data": {
            "COMMAND": "DISPLAY_RESOLUTION",
            "width": tablet_width,
            "height": tablet_height,
            "rotation": 0
        }
    });
    let _ = client.expand_cast_send(device_id, conn_id, &res_cmd).await;
    // 5. Signaling loop
    let mut poll_receive = tokio::time::interval(Duration::from_millis(100));
    let mut poll_status = tokio::time::interval(Duration::from_millis(1000));

    while !*cancel.borrow() {
        tokio::select! {
            _ = cancel.changed() => break,
            Some(msg) = media.outgoing.recv() => {
                match msg {
                    SignalingMessage::Sdp { sdp, .. } => {
                        let payload = serde_json::json!({ "type": "offer", "sdp": sdp });
                        let _ = client.expand_cast_send(device_id, conn_id, &payload).await;
                    }
                    SignalingMessage::Ice { candidate, sdp_mline_index, .. } => {
                        let payload = serde_json::json!({
                            "type": "candidate",
                            "label": sdp_mline_index,
                            "id": format!("{sdp_mline_index}"),
                            "candidate": candidate,
                        });
                        let _ = client.expand_cast_send(device_id, conn_id, &payload).await;
                    }
                }
            }
            _ = poll_receive.tick() => {
                if let Ok(msgs) = client.expand_cast_receive(device_id, conn_id, 8).await {
                    for val in msgs {
                        let inner_obj = if let Some(payload) = val.get("payload") {
                            payload.clone()
                        } else if let Some(msg_str) = val.get("msg").and_then(|v| v.as_str()) {
                            serde_json::from_str::<serde_json::Value>(msg_str).unwrap_or(val)
                        } else {
                            val
                        };
                        if inner_obj.get("type").and_then(|v| v.as_str()) == Some("answer") {
                            if let Some(sdp) = inner_obj.get("sdp").and_then(|v| v.as_str()) {
                                let _ = media.incoming.try_send(SignalingMessage::Sdp {
                                    type_: "answer".into(),
                                    sdp: sdp.to_string(),
                                });
                            }
                        } else if inner_obj.get("type").and_then(|v| v.as_str()) == Some("candidate") {
                            if let Some(candidate) = inner_obj.get("candidate").and_then(|v| v.as_str()) {
                                let label = inner_obj.get("label").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                                let _ = media.incoming.try_send(SignalingMessage::Ice {
                                    candidate: candidate.to_string(),
                                    sdp_mline_index: label,
                                    mid: Some("0".into()),
                                });
                            }
                        }
                    }
                }
            }
            _ = poll_status.tick() => {
                let status = client.expand_cast_status(device_id).await?;
                if status.connection_id != Some(conn_id) || status.status != "connected" {
                    tracing::info!("ExpandCast connection disconnected by peer");
                    break;
                }
            }
        }
    }

    // 6. Cleanup
    let _ = events.try_send(ExpandCastEvent::Phase(ExpandCastPhase::Stopping));
    media.stop.store(true, Ordering::Release);
    let _ = client.expand_cast_stop(device_id).await;
    Ok(())
}

/// Portal-granted desktop capture. Dropping the portal proxies ends the
/// screencast session in the compositor, so the guard must outlive the pipeline.
pub struct ScreencastSession {
    fd: i32,
    node: u32,
    size: Option<(u32, u32)>,
    virtual_output: bool,
    _screencast: ashpd::desktop::screencast::Screencast,
    _session: ashpd::desktop::Session<ashpd::desktop::screencast::Screencast>,
}

impl ScreencastSession {
    fn fd(&self) -> i32 {
        self.fd
    }

    fn node(&self) -> u32 {
        self.node
    }

    /// Compositor-reported capture size, when the portal provides one.
    fn size(&self) -> Option<(u32, u32)> {
        self.size
    }

    /// True when the user picked a compositor-created virtual output. Only such
    /// a source can be asked to take the tablet's size, and current KWin rejects
    /// that request, so treat it as a hint rather than a guarantee.
    fn is_virtual(&self) -> bool {
        self.virtual_output
    }
}

async fn try_acquire_screencast() -> Result<ScreencastSession> {
    use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
    use ashpd::enumflags2::BitFlags;
    use std::os::fd::AsRawFd;

    let screencast = Screencast::new().await.context("connecting to screencast portal")?;
    let session = screencast
        .create_session(Default::default())
        .await
        .context("creating screencast session")?;

    // The KDE dialog already offers both mirroring a monitor and creating a
    // virtual output for this request; the app just accepts whichever is chosen.
    screencast
        .select_sources(
            &session,
            SelectSourcesOptions::default()
                .set_cursor_mode(CursorMode::Embedded)
                .set_sources(BitFlags::from_flag(SourceType::Monitor))
                .set_multiple(false),
        )
        .await
        .context("selecting screencast sources")?;

    let response = screencast
        .start(&session, None, Default::default())
        .await
        .context("starting screencast session")?
        .response()
        .context("getting screencast response")?;

    let stream = response
        .streams()
        .first()
        .context("no screencast streams in portal response")?;
    let node_id = stream.pipe_wire_node_id();
    let virtual_output = matches!(
        stream.source_type(),
        Some(ashpd::desktop::screencast::SourceType::Virtual)
    );
    let size = stream
        .size()
        .and_then(|(w, h)| u32::try_from(w).ok().zip(u32::try_from(h).ok()))
        .filter(|(w, h)| *w > 0 && *h > 0);
    tracing::info!(
        "portal screencast stream: node={node_id} size={size:?} source_type={:?}",
        stream.source_type()
    );
    let remote_fd = screencast
        .open_pipe_wire_remote(&session, Default::default())
        .await
        .context("opening pipewire remote fd")?;

    let raw_fd = remote_fd.as_raw_fd();
    // `pipewiresrc` hands this fd to pipewire, which takes ownership of it and
    // closes it on disconnect; leaking our handle avoids a double close.
    std::mem::forget(remote_fd);
    Ok(ScreencastSession {
        fd: raw_fd,
        node: node_id,
        size,
        virtual_output,
        _screencast: screencast,
        _session: session,
    })
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_cast_view_state_transitions_correctly() {
        let mut state = ExpandCastViewState::default();
        assert!(!state.active());
        assert_eq!(state.phase, ExpandCastPhase::Idle);

        state.phase = ExpandCastPhase::Preparing;
        assert!(state.active());

        state.phase = ExpandCastPhase::Streaming;
        assert!(state.active());

        state.phase = ExpandCastPhase::Stopping;
        assert!(state.active());

        state.phase = ExpandCastPhase::Failed;
        assert!(!state.active());
    }

    #[tokio::test]
    async fn handle_cancels_cleanly_on_stop() {
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let (_done_tx, done_rx) = watch::channel(false);
        let stopped = Arc::new(AtomicBool::new(false));

        let handle = ExpandCastSessionHandle {
            cancel_tx,
            done_rx,
            stopped: stopped.clone(),
        };

        assert!(!*cancel_rx.borrow());
        assert!(!stopped.load(Ordering::SeqCst));

        handle.stop();

        assert!(*cancel_rx.borrow());
        assert!(stopped.load(Ordering::SeqCst));
    }
}
