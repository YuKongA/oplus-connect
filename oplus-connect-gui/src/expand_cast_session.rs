//! Lifecycle management for tablet secondary display session.
use crate::client::DaemonClient;
use crate::expand_cast_media::{self, SenderConfig, SignalingMessage, TouchAction, TouchEvent};
use crate::persisted::ExpandCastSettings;
use anyhow::{Context, Result, anyhow};
use std::os::fd::{AsRawFd, OwnedFd};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
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
    pub mode: Option<u8>,
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

fn clear_current_session(
    active: &Mutex<Option<(watch::Sender<bool>, watch::Receiver<bool>)>>,
    identity: &watch::Sender<bool>,
) {
    let mut guard = active.lock().unwrap_or_else(|e| e.into_inner());
    if guard
        .as_ref()
        .is_some_and(|(cancel, _)| cancel.same_channel(identity))
    {
        *guard = None;
    }
}

async fn wait_for_session(mut done: watch::Receiver<bool>) {
    while !*done.borrow_and_update() {
        if done.changed().await.is_err() {
            break;
        }
    }
}

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

pub fn start(
    device_id: String,
    events: mpsc::Sender<ExpandCastEvent>,
    settings: ExpandCastSettings,
    mode: Option<u8>,
) -> ExpandCastSessionHandle {
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let (done_tx, done_rx) = watch::channel(false);
    let stopped = Arc::new(AtomicBool::new(false));

    let previous = ACTIVE_SESSION
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .replace((cancel_tx.clone(), done_rx.clone()));
    if let Some((cancel, _)) = &previous {
        let _ = cancel.send(true);
    }

    let dev_id = device_id.clone();
    let events_clone = events.clone();
    let cancel_worker = cancel_rx.clone();
    let worker_identity = cancel_tx.clone();

    crate::TOKIO_RT.spawn(async move {
        if let Some((_, done)) = previous {
            wait_for_session(done).await;
        }
        let res = if *cancel_worker.borrow() {
            Ok(())
        } else {
            run_session(&dev_id, &events_clone, cancel_worker, settings, mode).await
        };
        if let Err(err) = res {
            tracing::error!("ExpandCast session failed: {err:#}");
            let _ = events_clone.try_send(ExpandCastEvent::Error(format!("{err:#}")));
        } else {
            let _ = events_clone.try_send(ExpandCastEvent::Closed);
        }
        let _ = done_tx.send(true);
        clear_current_session(&ACTIVE_SESSION, &worker_identity);
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
    settings: ExpandCastSettings,
    mode: Option<u8>,
) -> Result<()> {
    let client = DaemonClient::new();
    let _ = events.try_send(ExpandCastEvent::Phase(ExpandCastPhase::Preparing));

    let _start_status = client
        .expand_cast_start(device_id, mode)
        .await
        .context("启动平板屏幕共享服务失败")?;

    let _ = events.try_send(ExpandCastEvent::Phase(ExpandCastPhase::WaitingForTablet));

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
    if tablet_width == 0 || tablet_height == 0 {
        let _ = client.expand_cast_stop(device_id).await;
        return Err(anyhow!("平板屏幕尺寸无效"));
    }

    let _ = events.try_send(ExpandCastEvent::Resolution(tablet_width, tablet_height));

    let mut screencast = match try_acquire_screencast(&mut cancel, mode).await {
        Ok(Some(session)) => {
            tracing::info!(
                "Acquired portal screencast: node={} fd={}",
                session.node(),
                session.fd()
            );
            Some(session)
        }
        Ok(None) => {
            let _ = client.expand_cast_stop(device_id).await;
            return Ok(());
        }
        Err(err) => {
            let _ = client.expand_cast_stop(device_id).await;
            return Err(err).context("无法获得屏幕源授权");
        }
    };
    // The guard keeps the compositor-side session (and thus the PipeWire node)
    // alive for as long as this function streams.
    let (pipewire_fd, pipewire_node) = match &screencast {
        Some(session) => (Some(Arc::clone(&session.fd)), Some(session.node())),
        None => (None, None),
    };

    // Avoid upscaling when the compositor cannot resize the virtual output.
    let (render_width, render_height, out_width, out_height) =
        match screencast.as_ref().and_then(|s| s.size()) {
            Some((src_w, src_h)) if src_w > 0 && src_h > 0 => {
                let render_w = src_w & !1;
                let render_h = src_h & !1;
                let out_h =
                    ((render_w as u64 * tablet_height as u64 / tablet_width as u64) as u32) & !1;
                (render_w, render_h, render_w, out_h.max(2))
            }
            _ => (tablet_width, tablet_height, tablet_width, tablet_height),
        };

    // Only virtual outputs can be resized through stream negotiation.
    let (capture_width, capture_height) = match screencast.as_ref() {
        Some(session) if session.is_virtual() => (Some(tablet_width), Some(tablet_height)),
        _ => (None, None),
    };
    match (capture_width, capture_height) {
        (Some(w), Some(h)) => tracing::info!(
            "ExpandCast: requesting a {w}x{h} virtual output (letterboxed fallback would be \
             {out_width}x{out_height} from {render_width}x{render_height})"
        ),
        _ => tracing::info!(
            "ExpandCast: encoding {out_width}x{out_height} from {render_width}x{render_height} content"
        ),
    }

    let sender_config = SenderConfig {
        width: out_width,
        height: out_height,
        render_width,
        render_height,
        capture_width,
        capture_height,
        fps: settings.fps,
        bitrate_kbps: settings.bitrate_kbps,
        adaptive_bitrate: settings.adaptive_bitrate,
        pipewire_fd,
        pipewire_node,
    };

    let mut media = match expand_cast_media::spawn_sender(sender_config) {
        Ok(media) => media,
        Err(error) => {
            let _ = client.expand_cast_stop(device_id).await;
            if let Some(session) = &mut screencast {
                session.session.close().await;
            }
            return Err(error).context("启动屏幕共享推流管道失败");
        }
    };
    let _ = events.try_send(ExpandCastEvent::Phase(ExpandCastPhase::Streaming));

    // Send SUB_MODE and DISPLAY_RESOLUTION to tablet
    let sub_mode_cmd = serde_json::json!({
        "cmd": "customBroadcast",
        "data": {
            "COMMAND": "SUB_MODE",
            "subMode": 1
        }
    });
    let _ = client
        .expand_cast_send(device_id, conn_id, &sub_mode_cmd)
        .await;

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
    let mut poll_receive = tokio::time::interval(Duration::from_millis(100));
    let mut poll_status = tokio::time::interval(Duration::from_millis(1000));
    let mut result = Ok(());
    let mut sender_finished = false;
    let mut pressed = false;

    while !*cancel.borrow() {
        tokio::select! {
            biased;
            _ = cancel.changed() => break,
            finished = &mut media.finished => {
                sender_finished = true;
                result = match finished {
                    Ok(Ok(())) => Err(anyhow!("屏幕共享推流已停止")),
                    Ok(Err(error)) => Err(anyhow!("屏幕共享推流失败: {error}")),
                    Err(_) => Err(anyhow!("屏幕共享推流线程异常退出")),
                };
                break;
            }
            touch = media.touch.recv() => {
                match touch {
                    Some(touch) => {
                        let geometry = *media.geometry.borrow();
                        tokio::select! {
                            biased;
                            _ = cancel.changed() => break,
                            finished = &mut media.finished => {
                                sender_finished = true;
                                result = match finished {
                                    Ok(Ok(())) => Err(anyhow!("屏幕共享推流已停止")),
                                    Ok(Err(error)) => Err(anyhow!("屏幕共享推流失败: {error}")),
                                    Err(_) => Err(anyhow!("屏幕共享推流线程异常退出")),
                                };
                                break;
                            }
                            injected = screencast.as_ref().expect("portal session exists")
                                .inject_touch(touch, &mut pressed, tablet_width, tablet_height, geometry) => {
                                if let Err(error) = injected {
                                    result = Err(error.context("注入远程触控失败"));
                                    break;
                                }
                            }
                        }
                    }
                    None => {
                        result = Err(anyhow!("平板触控通道已关闭"));
                        break;
                    }
                }
            }
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
                        } else if inner_obj.get("type").and_then(|v| v.as_str()) == Some("candidate")
                            && let Some(candidate) = inner_obj.get("candidate").and_then(|v| v.as_str()) {
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
            _ = poll_status.tick() => {
                let status = match client.expand_cast_status(device_id).await {
                    Ok(status) => status,
                    Err(error) => {
                        result = Err(error);
                        break;
                    }
                };
                if status.connection_id != Some(conn_id) || status.status != "connected" {
                    tracing::info!("ExpandCast connection disconnected by peer");
                    break;
                }
            }
        }
    }

    let _ = events.try_send(ExpandCastEvent::Phase(ExpandCastPhase::Stopping));
    if pressed
        && let Some(session) = &screencast
        && let Err(error) = session.release_touch().await
    {
        tracing::warn!("释放远程触控失败: {error:#}");
        if result.is_ok() {
            result = Err(error.context("释放远程触控失败"));
        }
    }
    media.stop.store(true, Ordering::Release);
    let _ = client.expand_cast_stop(device_id).await;
    if !sender_finished
        && tokio::time::timeout(Duration::from_secs(5), &mut media.finished)
            .await
            .is_err()
    {
        tracing::warn!("ExpandCast sender did not stop within 5 seconds");
    }
    if let Some(session) = &mut screencast {
        session.session.close().await;
    }
    result
}

/// Portal-granted capture and its explicitly closed portal session.
pub struct ScreencastSession {
    fd: Arc<OwnedFd>,
    node: u32,
    size: Option<(u32, u32)>,
    logical_size: Option<watch::Receiver<Option<(u32, u32)>>>,
    virtual_output: bool,
    remote: ashpd::desktop::remote_desktop::RemoteDesktop,
    session: PortalSession,
}

struct PortalSession(
    Option<ashpd::desktop::Session<ashpd::desktop::remote_desktop::RemoteDesktop>>,
);

impl PortalSession {
    async fn close(&mut self) {
        if let Some(session) = self.0.as_ref() {
            let result = session.close().await;
            self.0.take();
            if let Err(error) = result {
                tracing::warn!("Failed to close screencast portal session: {error}");
            }
        }
    }
}

impl Drop for PortalSession {
    fn drop(&mut self) {
        if let Some(session) = self.0.take() {
            crate::TOKIO_RT.spawn(async move {
                if let Err(error) = session.close().await {
                    tracing::warn!("Failed to close screencast portal session: {error}");
                }
            });
        }
    }
}

impl ScreencastSession {
    fn fd(&self) -> i32 {
        self.fd.as_raw_fd()
    }

    fn node(&self) -> u32 {
        self.node
    }

    /// Compositor-reported capture size, when the portal provides one.
    fn size(&self) -> Option<(u32, u32)> {
        self.size
    }

    /// Whether the selected source is a virtual output.
    fn is_virtual(&self) -> bool {
        self.virtual_output
    }

    async fn release_touch(&self) -> Result<()> {
        use ashpd::desktop::remote_desktop::KeyState;
        let session = self.session.0.as_ref().expect("portal session exists");
        self.remote
            .notify_pointer_button(session, 0x110, KeyState::Released, Default::default())
            .await?;
        Ok(())
    }

    async fn inject_touch(
        &self,
        touch: TouchEvent,
        pressed: &mut bool,
        tablet_width: u32,
        tablet_height: u32,
        geometry: Option<expand_cast_media::MediaGeometry>,
    ) -> Result<()> {
        use ashpd::desktop::remote_desktop::KeyState;
        if touch.action == TouchAction::Up {
            if *pressed {
                self.release_touch().await?;
                *pressed = false;
            }
            return Ok(());
        }
        if !accept_contact_event(*pressed, touch.action) {
            return Ok(());
        }
        let Some(geometry) = geometry else {
            return Ok(());
        };
        let logical_size = if self.virtual_output {
            let tracker = self
                .logical_size
                .as_ref()
                .context("系统未提供虚拟屏映射标识")?;
            if tracker.has_changed().is_err() {
                return Err(anyhow!("无法跟踪虚拟屏逻辑尺寸"));
            }
            *tracker.borrow()
        } else {
            self.size
        };
        let Some((logical_width, logical_height)) = logical_size else {
            return Ok(());
        };
        let Some((x, y)) = map_touch(
            touch.x,
            touch.y,
            (tablet_width, tablet_height),
            geometry,
            (logical_width, logical_height),
        ) else {
            return Ok(());
        };
        let session = self.session.0.as_ref().expect("portal session exists");
        self.remote
            .notify_pointer_motion_absolute(session, self.node, x, y, Default::default())
            .await?;
        if matches!(touch.action, TouchAction::Down) {
            *pressed = true;
            self.remote
                .notify_pointer_button(session, 0x110, KeyState::Pressed, Default::default())
                .await?;
        }
        Ok(())
    }
}

// Tablet coordinates address the displayed frame; portal coordinates are
// relative to the selected stream in compositor logical units.
fn map_touch(
    x: u32,
    y: u32,
    tablet: (u32, u32),
    geometry: expand_cast_media::MediaGeometry,
    logical: (u32, u32),
) -> Option<(f64, f64)> {
    let (tablet_width, tablet_height) = tablet;
    let (logical_width, logical_height) = logical;
    let expand_cast_media::MediaGeometry {
        out_width: output_width,
        out_height: output_height,
        render_width,
        render_height,
        ..
    } = geometry;
    if tablet_width == 0
        || tablet_height == 0
        || output_width == 0
        || output_height == 0
        || render_width == 0
        || render_height == 0
        || logical_width == 0
        || logical_height == 0
        || render_width > output_width
        || render_height > output_height
        || x >= tablet_width
        || y >= tablet_height
    {
        return None;
    }
    let px = x as f64 * output_width as f64 / tablet_width as f64;
    let py = y as f64 * output_height as f64 / tablet_height as f64;
    let left = ((output_width - render_width) / 2) as f64;
    let top = ((output_height - render_height) / 2) as f64;
    if px < left || px >= left + render_width as f64 || py < top || py >= top + render_height as f64
    {
        return None;
    }
    Some((
        (px - left) * logical_width as f64 / render_width as f64,
        (py - top) * logical_height as f64 / render_height as f64,
    ))
}

fn accept_contact_event(pressed: bool, action: TouchAction) -> bool {
    match action {
        TouchAction::Down => !pressed,
        TouchAction::Move | TouchAction::Up => pressed,
    }
}

fn virtual_source_for_request(
    mode: Option<u8>,
    source: Option<ashpd::desktop::screencast::SourceType>,
    portal_version: u32,
) -> Result<bool> {
    use ashpd::desktop::screencast::SourceType;
    match (mode, source) {
        (Some(1), Some(SourceType::VIRTUAL)) => Ok(true),
        (Some(2), Some(SourceType::MONITOR)) => Ok(false),
        // Before v3 the portal cannot report source_type. SelectSources above
        // requests exactly one type, so that request identifies the source.
        (Some(1), None) if portal_version < 3 => Ok(true),
        (Some(2), None) if portal_version < 3 => Ok(false),
        (None, source) => Ok(source == Some(SourceType::VIRTUAL)),
        (Some(1), _) => Err(anyhow!("系统未确认选择扩展屏，无法满足平板屏幕扩展请求")),
        (Some(2), _) => Err(anyhow!("系统未确认选择显示器，无法满足平板屏幕镜像请求")),
        _ => Err(anyhow!("不支持的屏幕共享模式")),
    }
}

async fn try_acquire_screencast(
    cancel: &mut watch::Receiver<bool>,
    mode: Option<u8>,
) -> Result<Option<ScreencastSession>> {
    use ashpd::desktop::remote_desktop::{DeviceType, RemoteDesktop, SelectDevicesOptions};
    use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let created = tokio::select! {
        _ = cancel.changed() => return Ok(None),
        result = tokio::time::timeout_at(deadline, async {
            let screencast = Screencast::new().await.context("connecting to screencast portal")?;
            let remote = RemoteDesktop::new().await.context("连接远程桌面授权服务失败")?;
            let session = remote
                .create_session(Default::default())
                .await
                .context("创建远程桌面会话失败")?;
            Ok::<_, anyhow::Error>((screencast, remote, session))
        }) => result.context("portal screencast prompt timed out")??,
    };
    let (screencast, remote, session) = created;
    let mut session = PortalSession(Some(session));
    let portal_session = session.0.as_ref().expect("portal session exists");
    let selected = tokio::select! {
        _ = cancel.changed() => Ok(None),
        result = tokio::time::timeout_at(deadline, async {
            // A single tablet contact maps to a pointer and left button.
            // Some portals ignore the stream when injecting touch events.
            remote.select_devices(
                portal_session,
                SelectDevicesOptions::default().set_devices(DeviceType::POINTER),
            ).await.context("申请远程输入设备失败")?.response().context("远程输入设备申请被拒绝")?;
            // Request exactly one source type: KDE's RemoteDesktop backend
            // creates a virtual output only for a Virtual-only request.
            screencast
                .select_sources(
                    portal_session,
                    SelectSourcesOptions::default()
                        .set_cursor_mode(CursorMode::EMBEDDED)
                        .set_sources(match mode {
                            Some(1) => SourceType::VIRTUAL,
                            _ => SourceType::MONITOR,
                        })
                        .set_multiple(false),
                )
                .await
                .context("选择屏幕源失败")?.response().context("屏幕源选择被拒绝")?;
            let response = remote
                .start(portal_session, None, Default::default())
                .await
                .context("启动远程桌面授权失败")?
                .response()
                .context("远程桌面授权被拒绝或失败")?;
            if !response.devices().contains(DeviceType::POINTER) {
                return Err(anyhow!("未获准使用指针输入设备"));
            }
            let stream = response
                .streams()
                .first()
                .context("no screencast streams in portal response")?;
            let node = stream.pipe_wire_node_id();
            let virtual_output = virtual_source_for_request(mode, stream.source_type(), screencast.version())?;
            let size = stream
                .size()
                .and_then(|(w, h)| u32::try_from(w).ok().zip(u32::try_from(h).ok()))
                .filter(|(w, h)| *w > 0 && *h > 0);
            tracing::info!(
                "portal screencast stream: node={node} size={size:?} source_type={:?}",
                stream.source_type()
            );
            let fd = screencast
                .open_pipe_wire_remote(portal_session, Default::default())
                .await
                .context("opening pipewire remote fd")?;
            Ok::<_, anyhow::Error>((fd, node, size, virtual_output, stream.mapping_id().map(str::to_owned)))
        }) => result.context("portal screencast prompt timed out").and_then(|result| result).map(Some),
    };
    match selected {
        Ok(Some((fd, node, size, virtual_output, mapping_id))) => Ok(Some(ScreencastSession {
            fd: Arc::new(fd),
            node,
            size,
            logical_size: if virtual_output {
                mapping_id.map(crate::output_geometry::track)
            } else {
                None
            },
            virtual_output,
            remote,
            session,
        })),
        other => {
            session.close().await;
            other.map(|_| None)
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn geometry(out: (u32, u32), render: (u32, u32)) -> expand_cast_media::MediaGeometry {
        expand_cast_media::MediaGeometry {
            out_width: out.0,
            out_height: out.1,
            render_width: render.0,
            render_height: render.1,
            pad_x: 0,
            pad_y: 0,
        }
    }

    #[test]
    fn maps_content_and_rejects_black_bars() {
        // A 1920x1080 image centered in a 1920x1280 frame, displayed on
        // a 1200x800 tablet, maps to a HiDPI 960x540 logical stream.
        assert_eq!(
            map_touch(
                600,
                400,
                (1200, 800),
                geometry((1920, 1280), (1920, 1080)),
                (960, 540)
            ),
            Some((480.0, 270.0))
        );
        assert_eq!(
            map_touch(
                0,
                0,
                (1200, 800),
                geometry((1920, 1280), (1920, 1080)),
                (960, 540)
            ),
            None
        );
        assert_eq!(
            map_touch(
                600,
                799,
                (1200, 800),
                geometry((1920, 1280), (1920, 1080)),
                (960, 540)
            ),
            None
        );
        assert_eq!(
            map_touch(
                1200,
                400,
                (1200, 800),
                geometry((1920, 1280), (1920, 1080)),
                (960, 540)
            ),
            None
        );
        assert_eq!(
            map_touch(
                0,
                0,
                (0, 800),
                geometry((1920, 1280), (1920, 1080)),
                (960, 540)
            ),
            None
        );
        assert_eq!(
            map_touch(
                600,
                400,
                (1200, 800),
                geometry((1200, 800), (1200, 800)),
                (1200, 800)
            ),
            Some((600.0, 400.0))
        );
    }

    #[test]
    fn legacy_portal_uses_the_single_requested_source_type() {
        use ashpd::desktop::screencast::SourceType;
        assert!(virtual_source_for_request(Some(1), None, 2).unwrap());
        assert!(!virtual_source_for_request(Some(2), None, 2).unwrap());
        assert!(virtual_source_for_request(Some(1), Some(SourceType::MONITOR), 2).is_err());
        assert!(virtual_source_for_request(Some(2), Some(SourceType::VIRTUAL), 2).is_err());
        assert!(virtual_source_for_request(Some(2), None, 3).is_err());
    }

    #[tokio::test]
    async fn previous_completion_cannot_clear_replacement() {
        let (old_cancel, _) = watch::channel(false);
        let (new_cancel, _) = watch::channel(false);
        let (done_tx, done_rx) = watch::channel(false);
        let active = Mutex::new(Some((new_cancel.clone(), done_rx.clone())));

        clear_current_session(&active, &old_cancel);
        assert!(
            active
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .0
                .same_channel(&new_cancel)
        );

        let waiter = tokio::spawn(wait_for_session(done_rx));
        assert!(!waiter.is_finished());
        done_tx.send(true).unwrap();
        waiter.await.unwrap();
        clear_current_session(&active, &new_cancel);
        assert!(active.lock().unwrap().is_none());
    }

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
